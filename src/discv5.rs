//! The Discovery v5 protocol. See `lib.rs` for further details.
//!

use crate::error::{QueryError, RequestError};
use crate::kbucket::{self, ip_limiter, KBucketsTable, NodeStatus};
use crate::node_info::NodeContact;
use crate::service::{QueryKind, Service, ServiceRequest};
use crate::Discv5Config;
use crate::Enr;
use enr::{CombinedKey, EnrError, EnrKey, NodeId};
use log::{error, warn};
use parking_lot::RwLock;
use std::{
    convert::TryFrom,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

use libp2p_core::Multiaddr;

/// The main Discv5 Service struct. This provides the user-level API for performing queries and
/// interacting with the underlying service.
// TODO: Add blacklist and whitelist
pub struct Discv5 {
    config: Discv5Config,
    /// The channel to make requests from the main service.
    service_channel: Option<mpsc::Sender<ServiceRequest>>,
    service_exit: Option<oneshot::Sender<()>>,
    kbuckets: Arc<RwLock<KBucketsTable<NodeId, Enr>>>,
    local_enr: Arc<RwLock<Enr>>,
    enr_key: Arc<RwLock<CombinedKey>>,
    active_sessions: Arc<AtomicUsize>,
    /// Stores a default runtime if none is given in the configuration.
    _runtime: Option<tokio::runtime::Runtime>,
}

impl Discv5 {
    pub fn new(
        local_enr: Enr,
        enr_key: CombinedKey,
        mut config: Discv5Config,
    ) -> Result<Self, &'static str> {
        // ensure the keypair matches the one that signed the enr.
        if local_enr.public_key() != enr_key.public() {
            return Err("Provided keypair does not match the provided ENR");
        }

        // if an executor is not provided create one and store locally
        let _runtime = {
            if config.executor.is_none() {
                let (executor, runtime) = crate::executor::TokioExecutor::new();
                config.executor = Some(Box::new(executor));
                Some(runtime)
            } else {
                None
            }
        };

        let local_enr = Arc::new(RwLock::new(local_enr));
        let enr_key = Arc::new(RwLock::new(enr_key));
        let kbuckets = Arc::new(RwLock::new(KBucketsTable::new(
            local_enr.read().node_id().into(),
            Duration::from_secs(60),
        )));

        Ok(Discv5 {
            config,
            service_channel: None,
            service_exit: None,
            kbuckets,
            local_enr,
            enr_key,
            active_sessions: Arc::new(AtomicUsize::new(0)),
            _runtime,
        })
    }

    /// Starts the required tasks and begins listening on a given UDP SocketAddr.
    pub fn start(&mut self, listen_socket: SocketAddr) {
        if self.service_channel.is_some() {
            warn!("Service is already started");
            return;
        }

        // create the main service
        let (service_exit, service_channel) = Service::spawn(
            self.local_enr.clone(),
            self.enr_key.clone(),
            self.kbuckets.clone(),
            self.config.clone(),
            self.active_sessions.clone(),
            listen_socket,
        );
        self.service_exit = Some(service_exit);
        self.service_channel = Some(service_channel);
    }

    /// Terminates the service.
    pub fn shutdown(&mut self) {
        if let Some(exit) = self.service_exit.take() {
            if let Err(_) = exit.send(()) {
                error!("Could not send exit request to Discv5 service");
            }
            self.service_channel = None;
        } else {
            warn!("Service is already shutdown");
        }
    }

    /// Adds a known ENR of a peer participating in Service to the
    /// routing table.
    ///
    /// This allows pre-populating the Kademlia routing table with known
    /// addresses, so that they can be used immediately in following DHT
    /// operations involving one of these peers, without having to dial
    /// them upfront.
    pub fn add_enr(&mut self, enr: Enr) -> Result<(), &'static str> {
        // only add ENR's that have a valid udp socket.
        if enr.udp_socket().is_none() {
            warn!("ENR attempted to be added without a UDP socket has been ignored");
            return Err("ENR has no UDP socket to connect to");
        }

        if !(self.config.table_filter)(&enr) {
            warn!("ENR attempted to be added which is banned by the configuration table filter.");
            return Err("ENR banned by table filter");
        }

        let key = kbucket::Key::from(enr.node_id());

        // should the ENR be inserted or updated to a value that would exceed the IP limit ban
        let ip_limit_ban = self.config.ip_limit
            && !self
                .kbuckets
                .read()
                .check(&key, &enr, { |v, o, l| ip_limiter(v, &o, l) });

        match self.kbuckets.write().entry(&key) {
            kbucket::Entry::Present(mut entry, _) => {
                // still update an ENR, regardless of the IP limit ban
                *entry.value() = enr;
            }
            kbucket::Entry::Pending(mut entry, _) => {
                *entry.value() = enr;
            }
            kbucket::Entry::Absent(entry) => {
                if !ip_limit_ban {
                    match entry.insert(enr.clone(), NodeStatus::Disconnected) {
                        kbucket::InsertResult::Inserted => {}
                        kbucket::InsertResult::Full => {
                            return Err("Table full");
                        }
                        kbucket::InsertResult::Pending { .. } => {}
                    }
                }
            }
            kbucket::Entry::SelfEntry => {}
        };
        Ok(())
    }

    /// Removes a `node_id` from the routing table.
    ///
    /// This allows applications, for whatever reason, to remove nodes from the local routing
    /// table. Returns `true` if the node was in the table and `false` otherwise.
    pub fn remove_node(&mut self, node_id: &NodeId) -> bool {
        let key = &kbucket::Key::from(*node_id);
        self.kbuckets.write().remove(key)
    }

    /// Returns the number of connected peers that exist in the routing table.
    pub fn connected_peers(&self) -> usize {
        self.kbuckets
            .write()
            .iter()
            .filter(|entry| entry.status == NodeStatus::Connected)
            .count()
    }

    /// The number of active Discv5 session handshakes stored in the cache.
    pub fn active_sessions(&self) -> usize {
        self.active_sessions.load(Ordering::Relaxed)
    }

    /// Returns the local ENR of the node.
    pub fn local_enr(&self) -> Enr {
        self.local_enr.read().clone()
    }

    /// Returns an ENR if one is known for the given NodeId.
    pub fn find_enr(&mut self, node_id: &NodeId) -> Option<Enr> {
        // check if we know this node id in our routing table
        let key = kbucket::Key::from(*node_id);
        if let kbucket::Entry::Present(mut entry, _) = self.kbuckets.write().entry(&key) {
            return Some(entry.value().clone());
        }
        None
    }

    /// Updates the local ENR TCP/UDP socket.
    pub fn update_local_enr_socket(&mut self, socket_addr: SocketAddr, is_tcp: bool) -> bool {
        let local_socket = self.local_enr.read().udp_socket();
        if local_socket != Some(socket_addr) {
            if is_tcp {
                self.local_enr
                    .write()
                    .set_tcp_socket(socket_addr, &self.enr_key.read())
                    .is_ok()
            } else {
                self.local_enr
                    .write()
                    .set_udp_socket(socket_addr, &self.enr_key.read())
                    .is_ok()
            }
        } else {
            false
        }
    }

    /// Allows application layer to insert an arbitrary field into the local ENR.
    pub fn enr_insert(&mut self, key: &str, value: Vec<u8>) -> Result<Option<Vec<u8>>, EnrError> {
        self.local_enr
            .write()
            .insert(key, value, &self.enr_key.read())
    }

    /// Returns an iterator over all ENR node IDs of nodes currently contained in the routing table.
    pub fn table_entries_id(&mut self) -> Vec<NodeId> {
        self.kbuckets
            .write()
            .iter()
            .map(|entry| entry.node.key.preimage().clone())
            .collect()
    }

    /// Returns an iterator over all the ENR's of nodes currently contained in the routing table.
    pub fn table_entries_enr(&mut self) -> Vec<Enr> {
        self.kbuckets
            .write()
            .iter()
            .map(|entry| entry.node.value.clone())
            .collect()
    }

    /// Requests the ENR of a node corresponding to multiaddr or multi-addr string.
    ///
    /// Only `ed25519` and `secp256k1` key types are currently supported.
    pub async fn request_enr(
        &mut self,
        multiaddr: impl std::convert::TryInto<Multiaddr>,
    ) -> Result<Option<Enr>, RequestError> {
        // Sanitize the multiaddr

        // The multiaddr must support the udp protocol and be of an appropriate key type.
        // The conversion logic is contained in the `TryFrom<MultiAddr>` implementation of a
        // `NodeContact`.
        let multiaddr: Multiaddr = multiaddr
            .try_into()
            .map_err(|_| RequestError::InvalidMultiaddr("Could not convert to multiaddr".into()))?;
        let node_contact: NodeContact = NodeContact::try_from(multiaddr)
            .map_err(|e| RequestError::InvalidMultiaddr(e.into()))?;

        let (callback_send, callback_recv) = oneshot::channel();

        let event = ServiceRequest::FindEnr(node_contact, callback_send);
        if let Err(_) = self.send_event(event).await {
            return Err(RequestError::ServiceNotStarted);
        }
        Ok(callback_recv
            .await
            .map_err(|e| RequestError::ChannelFailed(e.to_string()))?)
    }

    /// Runs an iterative `FIND_NODE` request.
    ///
    /// This will return peers containing contactable nodes of the DHT closest to the
    /// requested `NodeId`.
    pub async fn find_node(&mut self, target_node: NodeId) -> Result<Vec<Enr>, QueryError> {
        let (callback_send, callback_recv) = oneshot::channel();

        let query_kind = QueryKind::FindNode { target_node };

        let event = ServiceRequest::StartQuery(query_kind, callback_send);

        if let Err(_) = self.send_event(event).await {
            return Err(QueryError::ServiceNotStarted);
        }

        Ok(callback_recv
            .await
            .map_err(|e| QueryError::ChannelFailed(e.to_string()))?)
    }

    /// Starts a `FIND_NODE` request.
    ///
    /// This will return less than or equal to `num_nodes` ENRs which satisfy the
    /// `predicate`.
    pub async fn find_node_predicate<F>(
        &mut self,
        target_node: NodeId,
        predicate: F,
        target_peer_no: usize,
    ) -> Result<Vec<Enr>, QueryError>
    where
        F: Fn(&Enr) -> bool + Send + Clone + 'static,
    {
        let (callback_send, callback_recv) = oneshot::channel();

        let query_kind = QueryKind::Predicate {
            target_node,
            predicate: Box::new(predicate),
            target_peer_no,
        };

        let event = ServiceRequest::StartQuery(query_kind, callback_send);

        if let Err(_) = self.send_event(event).await {
            return Err(QueryError::ServiceNotStarted);
        }

        Ok(callback_recv
            .await
            .map_err(|e| QueryError::ChannelFailed(e.to_string()))?)
    }

    /// Creates an event stream channel which can be polled to receive Discv5 events.
    pub async fn event_stream(&mut self) -> Result<mpsc::Receiver<Discv5Event>, String> {
        let (callback_send, callback_recv) = oneshot::channel();

        let event = ServiceRequest::RequestEventStream(callback_send);

        self.send_event(event).await?;

        Ok(callback_recv
            .await
            .map_err(|_| String::from("Service channel closed"))?)
    }

    async fn send_event(&mut self, event: ServiceRequest) -> Result<(), String> {
        if let Some(channel) = self.service_channel.as_mut() {
            channel.send(event).await.map_err(|e| e.to_string())?;
            Ok(())
        } else {
            return Err(String::from("Service has not started"));
        }
    }
}

impl Drop for Discv5 {
    fn drop(&mut self) {
        self.shutdown();
        // wait for the runtime to exit if one exists
        if let Some(runtime) = self._runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(1))
        }
    }
}

/// Events that can be produced by the `Discv5` event stream.
#[derive(Debug)]
pub enum Discv5Event {
    /// A node has been discovered from a FINDNODES request.
    ///
    /// The ENR of the node is returned. Various properties can be derived from the ENR.
    /// - `NodeId`: enr.node_id()
    /// - `SeqNo`: enr.seq_no()
    /// - `Ip`: enr.ip()
    Discovered(Enr),
    /// A new ENR was added to the routing table.
    EnrAdded { enr: Enr, replaced: Option<Enr> },
    /// A new node has been added to the routing table.
    NodeInserted {
        node_id: NodeId,
        replaced: Option<NodeId>,
    },
    /// Our local ENR IP address has been updated.
    SocketUpdated(SocketAddr),
}
