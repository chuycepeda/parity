// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::cmp::{Ord, PartialOrd, Ordering};
use std::sync::Arc;
use parking_lot::{Mutex, Condvar};
use ethkey::{Secret, Signature};
use key_server_cluster::{Error, AclStorage, DocumentKeyShare, NodeId, SessionId, EncryptedDocumentKeyShadow, SessionMeta};
use key_server_cluster::cluster::Cluster;
use key_server_cluster::cluster_sessions::ClusterSession;
use key_server_cluster::message::{Message, DecryptionMessage, DecryptionConsensusMessage, RequestPartialDecryption,
	PartialDecryption, DecryptionSessionError, DecryptionSessionCompleted, ConsensusMessage, InitializeConsensusSession,
	ConfirmConsensusInitialization};
use key_server_cluster::jobs::job_session::JobTransport;
use key_server_cluster::jobs::decryption_job::{PartialDecryptionRequest, PartialDecryptionResponse, DecryptionJob};
use key_server_cluster::jobs::consensus_session::{ConsensusSessionParams, ConsensusSessionState, ConsensusSession};

/// Decryption session API.
pub trait Session: Send + Sync + 'static {
	/// Wait until session is completed. Returns distributely restored secret key.
	fn wait(&self) -> Result<EncryptedDocumentKeyShadow, Error>;
}

/// Distributed decryption session.
/// Based on "ECDKG: A Distributed Key Generation Protocol Based on Elliptic Curve Discrete Logarithm" paper:
/// http://citeseerx.ist.psu.edu/viewdoc/download?doi=10.1.1.124.4128&rep=rep1&type=pdf
/// Brief overview:
/// 1) initialization: master node (which has received request for decrypting the secret) requests all other nodes to decrypt the secret
/// 2) ACL check: all nodes which have received the request are querying ACL-contract to check if requestor has access to the document
/// 3) partial decryption: every node which has succussfully checked access for the requestor do a partial decryption
/// 4) decryption: master node receives all partial decryptions of the secret and restores the secret
pub struct SessionImpl {
	/// Session core.
	core: SessionCore,
	/// Session data.
	data: Mutex<SessionData>,
}

/// Immutable session data.
struct SessionCore {
	/// Session metadata.
	pub meta: SessionMeta,
	/// Decryption session access key.
	pub access_key: Secret,
	/// Key share.
	pub key_share: DocumentKeyShare,
	/// Cluster which allows this node to send messages to other nodes in the cluster.
	pub cluster: Arc<Cluster>,
	/// SessionImpl completion condvar.
	pub completed: Condvar,
}

/// Decryption consensus session type.
type DecryptionConsensusSession = ConsensusSession<DecryptionConsensusTransport, DecryptionJob, DecryptionJobTransport>;

/// Mutable session data.
struct SessionData {
	/// Consensus-based decryption session.
	pub consensus_session: DecryptionConsensusSession,
	/// Is shadow decryption requested?
	pub is_shadow_decryption: Option<bool>,
	/// Decryption result.
	pub result: Option<Result<EncryptedDocumentKeyShadow, Error>>,
}

/// Decryption session Id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecryptionSessionId {
	/// Encryption session id.
	pub id: SessionId,
	/// Decryption session access key.
	pub access_key: Secret,
}

/// SessionImpl creation parameters
pub struct SessionParams {
	/// Session metadata.
	pub meta: SessionMeta,
	/// Session access key.
	pub access_key: Secret,
	/// Key share.
	pub key_share: DocumentKeyShare,
	/// ACL storage.
	pub acl_storage: Arc<AclStorage>,
	/// Cluster
	pub cluster: Arc<Cluster>,
}

/// Decryption consensus transport.
struct DecryptionConsensusTransport {
	/// Session id.
	id: SessionId,
	/// Session access key.
	access_key: Secret,
	/// Cluster.
	cluster: Arc<Cluster>,
}

/// Decryption job transport
struct DecryptionJobTransport {
	/// Session id.
	id: SessionId,
	//// Session access key.
	access_key: Secret,
	/// Cluster.
	cluster: Arc<Cluster>,
}

impl SessionImpl {
	/// Create new decryption session.
	pub fn new(params: SessionParams, requester_signature: Option<Signature>) -> Result<Self, Error> {
		debug_assert_eq!(params.meta.threshold, params.key_share.threshold);
		debug_assert_eq!(params.meta.self_node_id == params.meta.master_node_id, requester_signature.is_some());

		use key_server_cluster::generation_session::{check_cluster_nodes, check_threshold};

		// check that common_point and encrypted_point are already set
		if params.key_share.common_point.is_none() || params.key_share.encrypted_point.is_none() {
			return Err(Error::NotStartedSessionId);
		}

		// check nodes and threshold
		let nodes = params.key_share.id_numbers.keys().cloned().collect();
		check_cluster_nodes(&params.meta.self_node_id, &nodes)?;
		check_threshold(params.key_share.threshold, &nodes)?;

		let consensus_transport = DecryptionConsensusTransport {
			id: params.meta.id.clone(),
			access_key: params.access_key.clone(),
			cluster: params.cluster.clone(),
		};

		Ok(SessionImpl {
			core: SessionCore {
				meta: params.meta.clone(),
				access_key: params.access_key,
				key_share: params.key_share,
				cluster: params.cluster,
				completed: Condvar::new(),
			},
			data: Mutex::new(SessionData {
				consensus_session: match requester_signature {
					Some(requester_signature) => ConsensusSession::new_on_master(ConsensusSessionParams {
						meta: params.meta,
						acl_storage: params.acl_storage.clone(),
						consensus_transport: consensus_transport,
					}, requester_signature)?,
					None => ConsensusSession::new_on_slave(ConsensusSessionParams {
						meta: params.meta,
						acl_storage: params.acl_storage.clone(),
						consensus_transport: consensus_transport,
					})?,
				},
				is_shadow_decryption: None,
				result: None,
			}),
		})
	}

	#[cfg(test)]
	/// Get this node id.
	pub fn node(&self) -> &NodeId {
		&self.core.meta.self_node_id
	}

	#[cfg(test)]
	/// Get this session access key.
	pub fn access_key(&self) -> &Secret {
		&self.core.access_key
	}

	#[cfg(test)]
	/// Get session state.
	pub fn state(&self) -> ConsensusSessionState {
		self.data.lock().consensus_session.state()
	}

	#[cfg(test)]
	/// Get decrypted secret
	pub fn decrypted_secret(&self) -> Option<Result<EncryptedDocumentKeyShadow, Error>> {
		self.data.lock().result.clone()
	}

	/// Initialize decryption session on master node.
	pub fn initialize(&self, is_shadow_decryption: bool) -> Result<(), Error> {
		let mut data = self.data.lock();
		data.is_shadow_decryption = Some(is_shadow_decryption);
		data.consensus_session.initialize(self.core.key_share.id_numbers.keys().cloned().collect())?;

		if data.consensus_session.state() == ConsensusSessionState::ConsensusEstablished {
			self.core.disseminate_jobs(&mut data.consensus_session, is_shadow_decryption)?;

			debug_assert!(data.consensus_session.state() == ConsensusSessionState::Finished);
			data.result = Some(Ok(data.consensus_session.result()?));
			self.core.completed.notify_all();
		}

		Ok(())
	}

	/// Process decryption message.
	pub fn process_message(&self, sender: &NodeId, message: &DecryptionMessage) -> Result<(), Error> {
		match message {
			&DecryptionMessage::DecryptionConsensusMessage(ref message) =>
				self.on_consensus_message(sender, message),
			&DecryptionMessage::RequestPartialDecryption(ref message) =>
				self.on_partial_decryption_requested(sender, message),
			&DecryptionMessage::PartialDecryption(ref message) =>
				self.on_partial_decryption(sender, message),
			&DecryptionMessage::DecryptionSessionError(ref message) =>
				self.on_session_error(sender, message),
			&DecryptionMessage::DecryptionSessionCompleted(ref message) =>
				self.on_session_completed(sender, message),
		}
	}

	/// When consensus-related message is received.
	pub fn on_consensus_message(&self, sender: &NodeId, message: &DecryptionConsensusMessage) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);

		let mut data = self.data.lock();
		let is_establishing_consensus = data.consensus_session.state() == ConsensusSessionState::EstablishingConsensus;
		data.consensus_session.on_consensus_message(&sender, &message.message)?;

		let is_consensus_established = data.consensus_session.state() == ConsensusSessionState::ConsensusEstablished;
		if self.core.meta.self_node_id != self.core.meta.master_node_id || !is_establishing_consensus || !is_consensus_established {
			return Ok(());
		}

		let is_shadow_decryption = data.is_shadow_decryption
			.expect("we are on master node; on master node is_shadow_decryption is filled in initialize(); on_consensus_message follows initialize (state check in consensus_session); qed");
		self.core.disseminate_jobs(&mut data.consensus_session, is_shadow_decryption)
	}

	/// When partial decryption is requested.
	pub fn on_partial_decryption_requested(&self, sender: &NodeId, message: &RequestPartialDecryption) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		let mut data = self.data.lock();
		let requester = data.consensus_session.requester()?.clone();
		let decryption_job = DecryptionJob::new_on_slave(self.core.meta.self_node_id.clone(), self.core.access_key.clone(), requester, self.core.key_share.clone())?;
		let decryption_transport = self.core.decryption_transport();

		data.consensus_session.on_job_request(&sender, PartialDecryptionRequest {
			id: message.request_id.clone().into(),
			is_shadow_decryption: message.is_shadow_decryption,
			other_nodes_ids: message.nodes.iter().cloned().map(Into::into).collect(),
		}, decryption_job, decryption_transport)
	}

	/// When partial decryption is received.
	pub fn on_partial_decryption(&self, sender: &NodeId, message: &PartialDecryption) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		let mut data = self.data.lock();
		data.consensus_session.on_job_response(sender, PartialDecryptionResponse {
			request_id: message.request_id.clone().into(),
			shadow_point: message.shadow_point.clone().into(),
			decrypt_shadow: message.decrypt_shadow.clone(),
		})?;

		if data.consensus_session.state() != ConsensusSessionState::Finished {
			return Ok(());
		}

		self.core.cluster.broadcast(Message::Decryption(DecryptionMessage::DecryptionSessionCompleted(DecryptionSessionCompleted {
			session: self.core.meta.id.clone().into(),
			sub_session: self.core.access_key.clone().into(),
		})))?;

		data.result = Some(Ok(data.consensus_session.result()?));
		self.core.completed.notify_all();

		Ok(())
	}

	/// When session is completed.
	pub fn on_session_completed(&self, sender: &NodeId, message: &DecryptionSessionCompleted) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		self.data.lock().consensus_session.on_session_completed(sender)
	}

	/// When error has occured on another node.
	pub fn on_session_error(&self, sender: &NodeId, message: &DecryptionSessionError) -> Result<(), Error> {
		self.process_node_error(Some(&sender), &message.error)
	}

	/// Process error from the other node.
	fn process_node_error(&self, node: Option<&NodeId>, error: &String) -> Result<(), Error> {
		let mut data = self.data.lock();
		match {
			match node {
				Some(node) => data.consensus_session.on_node_error(node),
				None => data.consensus_session.on_session_timeout(),
			}
		} {
			Ok(false) => Ok(()),
			Ok(true) => {
				let is_shadow_decryption = data.is_shadow_decryption.expect("on_node_error returned true; this means that jobs must be REsent; this means that jobs already have been sent; jobs are sent when is_shadow_decryption.is_some(); qed");
				let disseminate_result = self.core.disseminate_jobs(&mut data.consensus_session, is_shadow_decryption);
				match disseminate_result {
					Ok(()) => Ok(()),
					Err(err) => {
						warn!("{}: decryption session failed with error: {:?} from {:?}", &self.core.meta.self_node_id, error, node);

						data.result = Some(Err(err.clone()));
						self.core.completed.notify_all();
						Err(err)
					}
				}
			},
			Err(err) => {
				warn!("{}: decryption session failed with error: {:?} from {:?}", &self.core.meta.self_node_id, error, node);

				data.result = Some(Err(err.clone()));
				self.core.completed.notify_all();
				Err(err)
			},
		}
	}
}

impl ClusterSession for SessionImpl {
	fn is_finished(&self) -> bool {
		let data = self.data.lock();
		data.consensus_session.state() == ConsensusSessionState::Failed
			|| data.consensus_session.state() == ConsensusSessionState::Finished
	}

	fn on_node_timeout(&self, node: &NodeId) {
		// ignore error, only state matters
		let _ = self.process_node_error(Some(node), &Error::NodeDisconnected.into());
	}

	fn on_session_timeout(&self) {
		// ignore error, only state matters
		let _ = self.process_node_error(None, &Error::NodeDisconnected.into());
	}
}

impl Session for SessionImpl {
	fn wait(&self) -> Result<EncryptedDocumentKeyShadow, Error> {
		let mut data = self.data.lock();
		if !data.result.is_some() {
			self.core.completed.wait(&mut data);
		}

		data.result.as_ref()
			.expect("checked above or waited for completed; completed is only signaled when result.is_some(); qed")
			.clone()
	}
}

impl SessionCore {
	pub fn decryption_transport(&self) -> DecryptionJobTransport {
		DecryptionJobTransport {
			id: self.meta.id.clone(),
			access_key: self.access_key.clone(),
			cluster: self.cluster.clone()
		}
	}

	pub fn disseminate_jobs(&self, consensus_session: &mut DecryptionConsensusSession, is_shadow_decryption: bool) -> Result<(), Error> {
		let requester = consensus_session.requester()?.clone();
		let decryption_job = DecryptionJob::new_on_master(self.meta.self_node_id.clone(), self.access_key.clone(), requester, self.key_share.clone(), is_shadow_decryption)?;
		consensus_session.disseminate_jobs(decryption_job, self.decryption_transport())
	}
}

impl JobTransport for DecryptionConsensusTransport {
	type PartialJobRequest=Signature;
	type PartialJobResponse=bool;

	fn send_partial_request(&self, node: &NodeId, request: Signature) -> Result<(), Error> {
		self.cluster.send(node, Message::Decryption(DecryptionMessage::DecryptionConsensusMessage(DecryptionConsensusMessage {
			session: self.id.clone().into(),
			sub_session: self.access_key.clone().into(),
			message: ConsensusMessage::InitializeConsensusSession(InitializeConsensusSession {
				requestor_signature: request.into(),
			})
		})))
	}

	fn send_partial_response(&self, node: &NodeId, response: bool) -> Result<(), Error> {
		self.cluster.send(node, Message::Decryption(DecryptionMessage::DecryptionConsensusMessage(DecryptionConsensusMessage {
			session: self.id.clone().into(),
			sub_session: self.access_key.clone().into(),
			message: ConsensusMessage::ConfirmConsensusInitialization(ConfirmConsensusInitialization {
				is_confirmed: response,
			})
		})))
	}
}

impl JobTransport for DecryptionJobTransport {
	type PartialJobRequest=PartialDecryptionRequest;
	type PartialJobResponse=PartialDecryptionResponse;

	fn send_partial_request(&self, node: &NodeId, request: PartialDecryptionRequest) -> Result<(), Error> {
		self.cluster.send(node, Message::Decryption(DecryptionMessage::RequestPartialDecryption(RequestPartialDecryption {
			session: self.id.clone().into(),
			sub_session: self.access_key.clone().into(),
			request_id: request.id.into(),
			is_shadow_decryption: request.is_shadow_decryption,
			nodes: request.other_nodes_ids.into_iter().map(Into::into).collect(),
		})))
	}

	fn send_partial_response(&self, node: &NodeId, response: PartialDecryptionResponse) -> Result<(), Error> {
		self.cluster.send(node, Message::Decryption(DecryptionMessage::PartialDecryption(PartialDecryption {
			session: self.id.clone().into(),
			sub_session: self.access_key.clone().into(),
			request_id: response.request_id.into(),
			shadow_point: response.shadow_point.into(),
			decrypt_shadow: response.decrypt_shadow,
		})))
	}
}

impl DecryptionSessionId {
	/// Create new decryption session Id.
	pub fn new(session_id: SessionId, sub_session_id: Secret) -> Self {
		DecryptionSessionId {
			id: session_id,
			access_key: sub_session_id,
		}
	}
}

impl PartialOrd for DecryptionSessionId {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for DecryptionSessionId {
	fn cmp(&self, other: &Self) -> Ordering {
		match self.id.cmp(&other.id) {
			Ordering::Equal => self.access_key.cmp(&other.access_key),
			r @ _ => r,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use std::collections::BTreeMap;
	use acl_storage::DummyAclStorage;
	use ethkey::{self, KeyPair, Random, Generator, Public, Secret};
	use key_server_cluster::{NodeId, DocumentKeyShare, SessionId, Error, EncryptedDocumentKeyShadow, SessionMeta};
	use key_server_cluster::cluster::tests::DummyCluster;
	use key_server_cluster::cluster_sessions::ClusterSession;
	use key_server_cluster::decryption_session::{SessionImpl, SessionParams};
	use key_server_cluster::message::{self, Message, DecryptionMessage};
	use key_server_cluster::math;
	use key_server_cluster::jobs::consensus_session::ConsensusSessionState;

	const SECRET_PLAIN: &'static str = "d2b57ae7619e070af0af6bc8c703c0cd27814c54d5d6a999cacac0da34ede279ca0d9216e85991029e54e2f0c92ee0bd30237725fa765cbdbfc4529489864c5f";

	fn prepare_decryption_sessions() -> (KeyPair, Vec<Arc<DummyCluster>>, Vec<Arc<DummyAclStorage>>, Vec<SessionImpl>) {
		// prepare encrypted data + cluster configuration for scheme 4-of-5
		let session_id = SessionId::default();
		let access_key = Random.generate().unwrap().secret().clone();
		let secret_shares: Vec<Secret> = vec![
			"834cb736f02d9c968dfaf0c37658a1d86ff140554fc8b59c9fdad5a8cf810eec".parse().unwrap(),
			"5a3c1d90fafafa66bb808bcc464354a98b05e6b2c95b5f609d4511cdd1b17a0b".parse().unwrap(),
			"71bf61e7848e08e3a8486c308ce521bdacfebcf9116a0151447eb301f3a2d0e9".parse().unwrap(),
			"80c0e5e2bea66fa9b2e07f7ce09630a9563e8242446d5ee63221feb09c4338f4".parse().unwrap(),
			"c06546b5669877ba579ca437a5602e89425c53808c708d44ccd6afcaa4610fad".parse().unwrap(),
		];
		let id_numbers: Vec<(NodeId, Secret)> = vec![
			("b486d3840218837b035c66196ecb15e6b067ca20101e11bd5e626288ab6806ecc70b8307012626bd512bad1559112d11d21025cef48cc7a1d2f3976da08f36c8".into(),
				"281b6bf43cb86d0dc7b98e1b7def4a80f3ce16d28d2308f934f116767306f06c".parse().unwrap()),
			("1395568277679f7f583ab7c0992da35f26cde57149ee70e524e49bdae62db3e18eb96122501e7cbb798b784395d7bb5a499edead0706638ad056d886e56cf8fb".into(),
				"00125d85a05e5e63e214cb60fe63f132eec8a103aa29266b7e6e6c5b7597230b".parse().unwrap()),
			("99e82b163b062d55a64085bacfd407bb55f194ba5fb7a1af9c34b84435455520f1372e0e650a4f91aed0058cb823f62146ccb5599c8d13372c300dea866b69fc".into(),
				"f43ac0fba42a5b6ed95707d2244659e89ba877b1c9b82c0d0a9dcf834e80fc62".parse().unwrap()),
			("7e05df9dd077ec21ed4bc45c9fe9e0a43d65fa4be540630de615ced5e95cf5c3003035eb713317237d7667feeeb64335525158f5f7411f67aca9645169ea554c".into(),
				"5a324938dfb2516800487d25ab7289ba8ec38811f77c3df602e4e65e3c9acd9f".parse().unwrap()),
			("321977760d1d8e15b047a309e4c7fe6f355c10bb5a06c68472b676926427f69f229024fa2692c10da167d14cdc77eb95d0fce68af0a0f704f0d3db36baa83bb2".into(),
				"12cf422d50002d04e52bd4906fd7f5f235f051ca36abfe37e061f8da248008d8".parse().unwrap()),
		];
		let common_point: Public = "6962be696e1bcbba8e64cc7fddf140f854835354b5804f3bb95ae5a2799130371b589a131bd39699ac7174ccb35fc4342dab05331202209582fc8f3a40916ab0".into();
		let encrypted_point: Public = "b07031982bde9890e12eff154765f03c56c3ab646ad47431db5dd2d742a9297679c4c65b998557f8008469afd0c43d40b6c5f6c6a1c7354875da4115237ed87a".into();
		let encrypted_datas: Vec<_> = (0..5).map(|i| DocumentKeyShare {
			author: Public::default(),
			threshold: 3,
			id_numbers: id_numbers.clone().into_iter().collect(),
			secret_share: secret_shares[i].clone(),
			common_point: Some(common_point.clone()),
			encrypted_point: Some(encrypted_point.clone()),
		}).collect();
		let acl_storages: Vec<_> = (0..5).map(|_| Arc::new(DummyAclStorage::default())).collect();
		let clusters: Vec<_> = (0..5).map(|i| {
			let cluster = Arc::new(DummyCluster::new(id_numbers.iter().nth(i).clone().unwrap().0));
			for id_number in &id_numbers {
				cluster.add_node(id_number.0.clone());
			}
			cluster
		}).collect();
		let requester = Random.generate().unwrap();
		let signature = Some(ethkey::sign(requester.secret(), &SessionId::default()).unwrap());
		let sessions: Vec<_> = (0..5).map(|i| SessionImpl::new(SessionParams {
			meta: SessionMeta {
				id: session_id.clone(),
				self_node_id: id_numbers.iter().nth(i).clone().unwrap().0,
				master_node_id: id_numbers.iter().nth(0).clone().unwrap().0,
				threshold: encrypted_datas[i].threshold,
			},
			access_key: access_key.clone(),
			key_share: encrypted_datas[i].clone(),
			acl_storage: acl_storages[i].clone(),
			cluster: clusters[i].clone()
		}, if i == 0 { signature.clone() } else { None }).unwrap()).collect();

		(requester, clusters, acl_storages, sessions)
	}

	fn do_messages_exchange(clusters: &[Arc<DummyCluster>], sessions: &[SessionImpl]) -> Result<(), Error> {
		do_messages_exchange_until(clusters, sessions, |_, _, _| false)
	}

	fn do_messages_exchange_until<F>(clusters: &[Arc<DummyCluster>], sessions: &[SessionImpl], mut cond: F) -> Result<(), Error> where F: FnMut(&NodeId, &NodeId, &Message) -> bool {
		while let Some((from, to, message)) = clusters.iter().filter_map(|c| c.take_message().map(|(to, msg)| (c.node(), to, msg))).next() {
			let session = &sessions[sessions.iter().position(|s| s.node() == &to).unwrap()];
			if cond(&from, &to, &message) {
				break;
			}

			match message {
				Message::Decryption(message) => session.process_message(&from, &message)?,
				_ => unreachable!(),
			}
		}

		Ok(())
	}

	#[test]
	fn constructs_in_cluster_of_single_node() {
		let mut nodes = BTreeMap::new();
		let self_node_id = Random.generate().unwrap().public().clone();
		nodes.insert(self_node_id, Random.generate().unwrap().secret().clone());
		match SessionImpl::new(SessionParams {
			meta: SessionMeta {
				id: SessionId::default(),
				self_node_id: self_node_id.clone(),
				master_node_id: self_node_id.clone(),
				threshold: 0,
			},
			access_key: Random.generate().unwrap().secret().clone(),
			key_share: DocumentKeyShare {
				author: Public::default(),
				threshold: 0,
				id_numbers: nodes,
				secret_share: Random.generate().unwrap().secret().clone(),
				common_point: Some(Random.generate().unwrap().public().clone()),
				encrypted_point: Some(Random.generate().unwrap().public().clone()),
			},
			acl_storage: Arc::new(DummyAclStorage::default()),
			cluster: Arc::new(DummyCluster::new(self_node_id.clone())),
		}, Some(ethkey::sign(Random.generate().unwrap().secret(), &SessionId::default()).unwrap())) {
			Ok(_) => (),
			_ => panic!("unexpected"),
		}
	}

	#[test]
	fn fails_to_construct_if_not_a_part_of_cluster() {
		let mut nodes = BTreeMap::new();
		let self_node_id = Random.generate().unwrap().public().clone();
		nodes.insert(Random.generate().unwrap().public().clone(), Random.generate().unwrap().secret().clone());
		nodes.insert(Random.generate().unwrap().public().clone(), Random.generate().unwrap().secret().clone());
		match SessionImpl::new(SessionParams {
			meta: SessionMeta {
				id: SessionId::default(),
				self_node_id: self_node_id.clone(),
				master_node_id: self_node_id.clone(),
				threshold: 0,
			},
			access_key: Random.generate().unwrap().secret().clone(),
			key_share: DocumentKeyShare {
				author: Public::default(),
				threshold: 0,
				id_numbers: nodes,
				secret_share: Random.generate().unwrap().secret().clone(),
				common_point: Some(Random.generate().unwrap().public().clone()),
				encrypted_point: Some(Random.generate().unwrap().public().clone()),
			},
			acl_storage: Arc::new(DummyAclStorage::default()),
			cluster: Arc::new(DummyCluster::new(self_node_id.clone())),
		}, Some(ethkey::sign(Random.generate().unwrap().secret(), &SessionId::default()).unwrap())) {
			Err(Error::InvalidNodesConfiguration) => (),
			_ => panic!("unexpected"),
		}
	}

	#[test]
	fn fails_to_construct_if_threshold_is_wrong() {
		let mut nodes = BTreeMap::new();
		let self_node_id = Random.generate().unwrap().public().clone();
		nodes.insert(self_node_id.clone(), Random.generate().unwrap().secret().clone());
		nodes.insert(Random.generate().unwrap().public().clone(), Random.generate().unwrap().secret().clone());
		match SessionImpl::new(SessionParams {
			meta: SessionMeta {
				id: SessionId::default(),
				self_node_id: self_node_id.clone(),
				master_node_id: self_node_id.clone(),
				threshold: 2,
			},
			access_key: Random.generate().unwrap().secret().clone(),
			key_share: DocumentKeyShare {
				author: Public::default(),
				threshold: 2,
				id_numbers: nodes,
				secret_share: Random.generate().unwrap().secret().clone(),
				common_point: Some(Random.generate().unwrap().public().clone()),
				encrypted_point: Some(Random.generate().unwrap().public().clone()),
			},
			acl_storage: Arc::new(DummyAclStorage::default()),
			cluster: Arc::new(DummyCluster::new(self_node_id.clone())),
		}, Some(ethkey::sign(Random.generate().unwrap().secret(), &SessionId::default()).unwrap())) {
			Err(Error::InvalidThreshold) => (),
			_ => panic!("unexpected"),
		}
	}

	#[test]
	fn fails_to_initialize_when_already_initialized() {
		let (_, _, _, sessions) = prepare_decryption_sessions();
		assert_eq!(sessions[0].initialize(false).unwrap(), ());
		assert_eq!(sessions[0].initialize(false).unwrap_err(), Error::InvalidStateForRequest);
	}

	#[test]
	fn fails_to_accept_initialization_when_already_initialized() {
		let (_, _, _, sessions) = prepare_decryption_sessions();
		assert_eq!(sessions[0].initialize(false).unwrap(), ());
		assert_eq!(sessions[0].on_consensus_message(sessions[1].node(), &message::DecryptionConsensusMessage {
				session: SessionId::default().into(),
				sub_session: sessions[0].access_key().clone().into(),
				message: message::ConsensusMessage::InitializeConsensusSession(message::InitializeConsensusSession {
					requestor_signature: ethkey::sign(Random.generate().unwrap().secret(), &SessionId::default()).unwrap().into(),
				}),
			}).unwrap_err(), Error::InvalidMessage);
	}

	#[test]
	fn fails_to_partial_decrypt_if_requested_by_slave() {
		let (_, _, _, sessions) = prepare_decryption_sessions();
		assert_eq!(sessions[1].on_consensus_message(sessions[0].node(), &message::DecryptionConsensusMessage {
				session: SessionId::default().into(),
				sub_session: sessions[0].access_key().clone().into(),
				message: message::ConsensusMessage::InitializeConsensusSession(message::InitializeConsensusSession {
					requestor_signature: ethkey::sign(Random.generate().unwrap().secret(), &SessionId::default()).unwrap().into(),
				}),
		}).unwrap(), ());
		assert_eq!(sessions[1].on_partial_decryption_requested(sessions[2].node(), &message::RequestPartialDecryption {
			session: SessionId::default().into(),
			sub_session: sessions[0].access_key().clone().into(),
			request_id: Random.generate().unwrap().secret().clone().into(),
			is_shadow_decryption: false,
			nodes: sessions.iter().map(|s| s.node().clone().into()).take(4).collect(),
		}).unwrap_err(), Error::InvalidMessage);
	}

	#[test]
	fn fails_to_partial_decrypt_if_wrong_number_of_nodes_participating() {
		let (_, _, _, sessions) = prepare_decryption_sessions();
		assert_eq!(sessions[1].on_consensus_message(sessions[0].node(), &message::DecryptionConsensusMessage {
				session: SessionId::default().into(),
				sub_session: sessions[0].access_key().clone().into(),
				message: message::ConsensusMessage::InitializeConsensusSession(message::InitializeConsensusSession {
					requestor_signature: ethkey::sign(Random.generate().unwrap().secret(), &SessionId::default()).unwrap().into(),
				}),
		}).unwrap(), ());
		assert_eq!(sessions[1].on_partial_decryption_requested(sessions[0].node(), &message::RequestPartialDecryption {
			session: SessionId::default().into(),
			sub_session: sessions[0].access_key().clone().into(),
			request_id: Random.generate().unwrap().secret().clone().into(),
			is_shadow_decryption: false,
			nodes: sessions.iter().map(|s| s.node().clone().into()).take(2).collect(),
		}).unwrap_err(), Error::InvalidMessage);
	}

	#[test]
	fn fails_to_accept_partial_decrypt_if_not_waiting() {
		let (_, _, _, sessions) = prepare_decryption_sessions();
		assert_eq!(sessions[0].on_partial_decryption(sessions[1].node(), &message::PartialDecryption {
			session: SessionId::default().into(),
			sub_session: sessions[0].access_key().clone().into(),
			request_id: Random.generate().unwrap().secret().clone().into(),
			shadow_point: Random.generate().unwrap().public().clone().into(),
			decrypt_shadow: None,
		}).unwrap_err(), Error::InvalidStateForRequest);
	}

	#[test]
	fn fails_to_accept_partial_decrypt_twice() {
		let (_, clusters, _, sessions) = prepare_decryption_sessions();
		sessions[0].initialize(false).unwrap();

		let mut pd_from = None;
		let mut pd_msg = None;
		do_messages_exchange_until(&clusters, &sessions, |from, _, msg| match msg {
			&Message::Decryption(DecryptionMessage::PartialDecryption(ref msg)) => {
				pd_from = Some(from.clone());
				pd_msg = Some(msg.clone());
				true
			},
			_ => false,
		}).unwrap();

		assert_eq!(sessions[0].on_partial_decryption(pd_from.as_ref().unwrap(), &pd_msg.clone().unwrap()).unwrap(), ());
		assert_eq!(sessions[0].on_partial_decryption(pd_from.as_ref().unwrap(), &pd_msg.unwrap()).unwrap_err(), Error::InvalidNodeForRequest);
	}

	#[test]
	fn decryption_fails_on_session_timeout() {
		let (_, _, _, sessions) = prepare_decryption_sessions();
		assert!(sessions[0].decrypted_secret().is_none());
		sessions[0].on_session_timeout();
		assert_eq!(sessions[0].decrypted_secret().unwrap().unwrap_err(), Error::ConsensusUnreachable);
	}

	#[test]
	fn node_is_marked_rejected_when_timed_out_during_initialization_confirmation() {
		let (_, _, _, sessions) = prepare_decryption_sessions();
		sessions[0].initialize(false).unwrap();

		// 1 node disconnects => we still can recover secret
		sessions[0].on_node_timeout(sessions[1].node());
		assert!(sessions[0].data.lock().consensus_session.consensus_job().rejects().contains(sessions[1].node()));
		assert!(sessions[0].state() == ConsensusSessionState::EstablishingConsensus);

		// 2 node are disconnected => we can not recover secret
		sessions[0].on_node_timeout(sessions[2].node());
		assert!(sessions[0].state() == ConsensusSessionState::Failed);
	}

	#[test]
	fn session_does_not_fail_if_rejected_node_disconnects() {
		let (_, clusters, acl_storages, sessions) = prepare_decryption_sessions();
		let key_pair = Random.generate().unwrap();

		acl_storages[1].prohibit(key_pair.public().clone(), SessionId::default());
		sessions[0].initialize(false).unwrap();

		do_messages_exchange_until(&clusters, &sessions, |_, _, _| sessions[0].state() == ConsensusSessionState::WaitingForPartialResults).unwrap();

		// 1st node disconnects => ignore this
		sessions[0].on_node_timeout(sessions[1].node());
		assert_eq!(sessions[0].state(), ConsensusSessionState::EstablishingConsensus);
	}

	#[test]
	fn session_does_not_fail_if_requested_node_disconnects() {
		let (_, clusters, _, sessions) = prepare_decryption_sessions();
		sessions[0].initialize(false).unwrap();

		do_messages_exchange_until(&clusters, &sessions, |_, _, _| sessions[0].state() == ConsensusSessionState::WaitingForPartialResults).unwrap();

		// 1 node disconnects => we still can recover secret
		sessions[0].on_node_timeout(sessions[1].node());
		assert!(sessions[0].state() == ConsensusSessionState::EstablishingConsensus);

		// 2 node are disconnected => we can not recover secret
		sessions[0].on_node_timeout(sessions[2].node());
		assert!(sessions[0].state() == ConsensusSessionState::Failed);
	}

	#[test]
	fn session_does_not_fail_if_node_with_shadow_point_disconnects() {
		let (_, clusters, _, sessions) = prepare_decryption_sessions();
		sessions[0].initialize(false).unwrap();

		do_messages_exchange_until(&clusters, &sessions, |_, _, _| sessions[0].state() == ConsensusSessionState::WaitingForPartialResults
			&& sessions[0].data.lock().consensus_session.computation_job().responses().len() == 2).unwrap();

		// disconnects from the node which has already sent us its own shadow point
		let disconnected = sessions[0].data.lock().
			consensus_session.computation_job().responses().keys()
			.filter(|n| *n != sessions[0].node())
			.cloned().nth(0).unwrap();
		sessions[0].on_node_timeout(&disconnected);
		assert_eq!(sessions[0].state(), ConsensusSessionState::EstablishingConsensus);
	}

	#[test]
	fn session_restarts_if_confirmed_node_disconnects() {
		let (_, clusters, _, sessions) = prepare_decryption_sessions();
		sessions[0].initialize(false).unwrap();

		do_messages_exchange_until(&clusters, &sessions, |_, _, _| sessions[0].state() == ConsensusSessionState::WaitingForPartialResults).unwrap();

		// disconnects from the node which has already confirmed its participation
		let disconnected = sessions[0].data.lock().consensus_session.computation_job().requests().iter().cloned().nth(0).unwrap();
		sessions[0].on_node_timeout(&disconnected);
		assert_eq!(sessions[0].state(), ConsensusSessionState::EstablishingConsensus);
		assert!(sessions[0].data.lock().consensus_session.computation_job().rejects().contains(&disconnected));
		assert!(!sessions[0].data.lock().consensus_session.computation_job().requests().contains(&disconnected));
	}

	#[test]
	fn session_does_not_fail_if_non_master_node_disconnects_from_non_master_node() {
		let (_, clusters, _, sessions) = prepare_decryption_sessions();
		sessions[0].initialize(false).unwrap();

		do_messages_exchange_until(&clusters, &sessions, |_, _, _| sessions[0].state() == ConsensusSessionState::WaitingForPartialResults).unwrap();

		// disconnects from the node which has already confirmed its participation
		sessions[1].on_node_timeout(sessions[2].node());
		assert!(sessions[0].state() == ConsensusSessionState::WaitingForPartialResults);
		assert!(sessions[1].state() == ConsensusSessionState::ConsensusEstablished);
	}

	#[test]
	fn complete_dec_session() {
		let (_, clusters, _, sessions) = prepare_decryption_sessions();

		// now let's try to do a decryption
		sessions[0].initialize(false).unwrap();

		do_messages_exchange(&clusters, &sessions).unwrap();

		// now check that:
		// 1) 5 of 5 sessions are in Finished state
		assert_eq!(sessions.iter().filter(|s| s.state() == ConsensusSessionState::Finished).count(), 5);
		// 2) 1 session has decrypted key value
		assert!(sessions.iter().skip(1).all(|s| s.decrypted_secret().is_none()));

		assert_eq!(sessions[0].decrypted_secret().unwrap().unwrap(), EncryptedDocumentKeyShadow {
			decrypted_secret: SECRET_PLAIN.into(),
			common_point: None,
			decrypt_shadows: None,
		});
	}

	#[test]
	fn complete_shadow_dec_session() {
		let (key_pair, clusters, _, sessions) = prepare_decryption_sessions();

		// now let's try to do a decryption
		sessions[0].initialize(true).unwrap();

		do_messages_exchange(&clusters, &sessions).unwrap();

		// now check that:
		// 1) 5 of 5 sessions are in Finished state
		assert_eq!(sessions.iter().filter(|s| s.state() == ConsensusSessionState::Finished).count(), 5);
		// 2) 1 session has decrypted key value
		assert!(sessions.iter().skip(1).all(|s| s.decrypted_secret().is_none()));

		let decrypted_secret = sessions[0].decrypted_secret().unwrap().unwrap();
		// check that decrypted_secret != SECRET_PLAIN
		assert!(decrypted_secret.decrypted_secret != SECRET_PLAIN.into());
		// check that common point && shadow coefficients are returned
		assert!(decrypted_secret.common_point.is_some());
		assert!(decrypted_secret.decrypt_shadows.is_some());
		// check that KS client is able to restore original secret
		use ethcrypto::DEFAULT_MAC;
		use ethcrypto::ecies::decrypt;
		let decrypt_shadows: Vec<_> = decrypted_secret.decrypt_shadows.unwrap().into_iter()
			.map(|c| Secret::from_slice(&decrypt(key_pair.secret(), &DEFAULT_MAC, &c).unwrap()))
			.collect();
		let decrypted_secret = math::decrypt_with_shadow_coefficients(decrypted_secret.decrypted_secret, decrypted_secret.common_point.unwrap(), decrypt_shadows).unwrap();
		assert_eq!(decrypted_secret, SECRET_PLAIN.into());
	}

	#[test]
	fn failed_dec_session() {
		let (key_pair, clusters, acl_storages, sessions) = prepare_decryption_sessions();

		// now let's try to do a decryption
		sessions[0].initialize(false).unwrap();

		// we need 4 out of 5 nodes to agree to do a decryption
		// let's say that 2 of these nodes are disagree
		acl_storages[1].prohibit(key_pair.public().clone(), SessionId::default());
		acl_storages[2].prohibit(key_pair.public().clone(), SessionId::default());

		assert_eq!(do_messages_exchange(&clusters, &sessions).unwrap_err(), Error::ConsensusUnreachable);

		// check that 3 nodes have failed state
		assert_eq!(sessions[0].state(), ConsensusSessionState::Failed);
		assert_eq!(sessions.iter().filter(|s| s.state() == ConsensusSessionState::Failed).count(), 3);
	}

	#[test]
	fn complete_dec_session_with_acl_check_failed_on_master() {
		let (key_pair, clusters, acl_storages, sessions) = prepare_decryption_sessions();

		// we need 4 out of 5 nodes to agree to do a decryption
		// let's say that 1 of these nodes (master) is disagree
		acl_storages[0].prohibit(key_pair.public().clone(), SessionId::default());

		// now let's try to do a decryption
		sessions[0].initialize(false).unwrap();

		do_messages_exchange(&clusters, &sessions).unwrap();

		// now check that:
		// 1) 4 of 5 sessions are in Finished state
		assert_eq!(sessions.iter().filter(|s| s.state() == ConsensusSessionState::Finished).count(), 5);
		// 2) 1 session has decrypted key value
		assert!(sessions.iter().skip(1).all(|s| s.decrypted_secret().is_none()));
		assert_eq!(sessions[0].decrypted_secret().unwrap().unwrap(), EncryptedDocumentKeyShadow {
			decrypted_secret: SECRET_PLAIN.into(),
			common_point: None,
			decrypt_shadows: None,
		});
	}

	#[test]
	fn decryption_session_works_over_network() {
		// TODO
	}
}
