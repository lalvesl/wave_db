//! [`QuickNode`] — the central state object for a running Quick-Node process.
//!
//! Combines partition ownership, cluster routing, replication bookkeeping, and
//! event broadcasting into a single cheaply-cloneable handle.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::config::Config;
use crate::ownership::{OwnershipMap, ShardRange, TransferTracker};
use crate::replication::ReplicationWatermark;
use crate::ring::{ConsistentRing, NodeId};
use wavedb_net::EventBus;
use wavedb_net::request::{RequestKind, TransportRequest, TransportResponse};

// ── QuickNode ─────────────────────────────────────────────────────────────────

/// All shared state for a running Quick-Node.
///
/// [`QuickNode`] is cheaply cloneable: every clone holds a reference to the
/// same `Arc`-backed inner state.
#[derive(Debug, Clone)]
pub struct QuickNode {
    inner: Arc<QuickNodeInner>,
}

#[derive(Debug)]
struct QuickNodeInner {
    node_id: NodeId,
    listen_addr: String,
    ownership: OwnershipMap,
    transfers: TransferTracker,
    ring: RwLock<ConsistentRing>,
    replication: ReplicationWatermark,
    events: EventBus,
    config: Config,
}

impl QuickNode {
    /// Create a `QuickNode` from a parsed and validated [`Config`].
    pub fn new(config: Config) -> Self {
        let node_id = config.node_id();

        let mut ring = ConsistentRing::new();
        ring.add_node(node_id, config.listen.clone());

        let ownership = OwnershipMap::new();
        for spec in &config.owns {
            ownership.add(
                spec.tenant,
                ShardRange::new(spec.shard_start, spec.shard_end),
            );
        }

        let replication = ReplicationWatermark::new();
        for peer_addr in config.peer_addrs() {
            let peer_id = addr_to_node_id(&peer_addr);
            ring.add_node(peer_id, peer_addr);
            replication.add_peer(peer_id);
        }

        let listen_addr = config.listen.clone();
        Self {
            inner: Arc::new(QuickNodeInner {
                node_id,
                listen_addr,
                ownership,
                transfers: TransferTracker::new(),
                ring: RwLock::new(ring),
                replication,
                events: EventBus::new(),
                config,
            }),
        }
    }

    // ── Accessors ─────────────────────────────────────────────────────────

    /// This node's stable identifier.
    pub fn node_id(&self) -> NodeId {
        self.inner.node_id
    }

    /// Socket address this node is listening on.
    pub fn listen_addr(&self) -> &str {
        &self.inner.listen_addr
    }

    /// Returns `true` if this node owns the given `(tenant, shard)` partition.
    pub fn owns(&self, tenant: u64, shard: u16) -> bool {
        self.inner.ownership.owns(tenant, shard)
    }

    /// Reference to the partition ownership map.
    pub fn ownership(&self) -> &OwnershipMap {
        &self.inner.ownership
    }

    /// Reference to the in-flight transfer tracker.
    pub fn transfers(&self) -> &TransferTracker {
        &self.inner.transfers
    }

    /// Reference to the replication watermark table.
    pub fn replication(&self) -> &ReplicationWatermark {
        &self.inner.replication
    }

    /// Reference to the object-changed event bus.
    pub fn events(&self) -> &EventBus {
        &self.inner.events
    }

    /// Runtime configuration.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Address of the node responsible for `(tenant, shard)`, if known.
    pub fn route_to(&self, tenant: u64, shard: u16) -> Option<String> {
        let ring = self.inner.ring.read();
        let owner = ring.owner_of(tenant, shard)?;
        ring.addr_of(owner).map(ToString::to_string)
    }

    // ── Request dispatch ──────────────────────────────────────────────────

    /// Process an incoming [`TransportRequest`] and return a [`TransportResponse`].
    ///
    /// This is the single dispatch point used by both the HTTP and WebSocket
    /// server handlers.  Phase 14 will wire in the real storage engine;
    /// for now the data-layer operations are stubbed.
    #[allow(clippy::unused_async)]
    pub async fn handle(&self, req: TransportRequest) -> TransportResponse {
        match req.kind {
            RequestKind::Connect { user, tenant } => self.handle_connect(req.seq, user, tenant),
            RequestKind::SearchUnique {
                struct_id,
                user,
                tenant,
            } => self.handle_search_unique(req.seq, struct_id, user, tenant),
            RequestKind::QueryNonUnique {
                struct_id,
                user,
                tenant,
                filter,
            } => self.handle_query(req.seq, struct_id, user, tenant, filter),
            RequestKind::Write {
                struct_id,
                user,
                tenant,
                payload,
            } => self.handle_write(req.seq, struct_id, user, tenant, payload),
            RequestKind::Delete { id, user, tenant } => {
                self.handle_delete(req.seq, id, user, tenant)
            }
            RequestKind::Disconnect { user, tenant } => {
                self.handle_disconnect(req.seq, user, tenant)
            }
        }
    }

    // ── Individual handlers ───────────────────────────────────────────────

    fn handle_connect(&self, seq: u64, _user: u64, _tenant: u64) -> TransportResponse {
        TransportResponse {
            seq,
            payload: Vec::new(),
            owner_url: Some(format!("ws://{}", self.inner.listen_addr)),
            backup_url: self.backup_url(),
            notifications: Vec::new(),
        }
    }

    fn handle_search_unique(
        &self,
        seq: u64,
        _struct_id: u32,
        _user: u64,
        tenant: u64,
    ) -> TransportResponse {
        if !self.inner.ownership.owns(tenant, 0) {
            return self.not_owner_resp(seq);
        }
        // Phase 14 wires the storage engine lookup here.
        TransportResponse {
            seq,
            payload: Vec::new(),
            owner_url: None,
            backup_url: None,
            notifications: Vec::new(),
        }
    }

    fn handle_query(
        &self,
        seq: u64,
        _struct_id: u32,
        _user: u64,
        tenant: u64,
        _filter: Vec<u8>,
    ) -> TransportResponse {
        if !self.inner.ownership.owns(tenant, 0) {
            return self.not_owner_resp(seq);
        }
        TransportResponse {
            seq,
            payload: Vec::new(),
            owner_url: None,
            backup_url: None,
            notifications: Vec::new(),
        }
    }

    fn handle_write(
        &self,
        seq: u64,
        _struct_id: u32,
        _user: u64,
        tenant: u64,
        _payload: Vec<u8>,
    ) -> TransportResponse {
        if !self.inner.ownership.owns(tenant, 0) {
            return self.not_owner_resp(seq);
        }
        // Advance replication sequence so peers know a write occurred.
        let write_seq = self.inner.replication.next_seq();
        for peer_id in self.peer_ids() {
            self.inner.replication.record_send(peer_id, write_seq);
        }
        // Phase 14 persists the write and fans it out to peers here.
        TransportResponse {
            seq,
            payload: Vec::new(),
            owner_url: None,
            backup_url: None,
            notifications: Vec::new(),
        }
    }

    fn handle_delete(&self, seq: u64, _id: u128, _user: u64, tenant: u64) -> TransportResponse {
        if !self.inner.ownership.owns(tenant, 0) {
            return self.not_owner_resp(seq);
        }
        TransportResponse {
            seq,
            payload: Vec::new(),
            owner_url: None,
            backup_url: None,
            notifications: Vec::new(),
        }
    }

    #[allow(clippy::unused_self)]
    const fn handle_disconnect(&self, seq: u64, _user: u64, _tenant: u64) -> TransportResponse {
        TransportResponse {
            seq,
            payload: Vec::new(),
            owner_url: None,
            backup_url: None,
            notifications: Vec::new(),
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    fn not_owner_resp(&self, seq: u64) -> TransportResponse {
        TransportResponse {
            seq,
            payload: b"not_owner".to_vec(),
            owner_url: self.route_to_owner_hint(),
            backup_url: None,
            notifications: Vec::new(),
        }
    }

    /// Address of the first peer node, used as a redirect hint.
    fn route_to_owner_hint(&self) -> Option<String> {
        let ring = self.inner.ring.read();
        ring.nodes()
            .find(|&id| id != self.inner.node_id)
            .and_then(|id| ring.addr_of(id).map(|a| format!("ws://{a}")))
    }

    fn backup_url(&self) -> Option<String> {
        let ring = self.inner.ring.read();
        ring.nodes()
            .find(|&id| id != self.inner.node_id)
            .and_then(|id| ring.addr_of(id).map(|a| format!("ws://{a}")))
    }

    fn peer_ids(&self) -> Vec<NodeId> {
        let ring = self.inner.ring.read();
        ring.nodes()
            .filter(|&id| id != self.inner.node_id)
            .collect()
    }
}

// ── addr_to_node_id ───────────────────────────────────────────────────────────

/// Derive a stable [`NodeId`] from a peer socket-address string using FNV-1a.
pub fn addr_to_node_id(addr: &str) -> NodeId {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in addr.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OwnershipSpec;

    fn cfg(tenant: u64, start: u16, end: u16) -> Config {
        Config {
            listen: "127.0.0.1:7700".into(),
            peers: String::new(),
            slow_node: None,
            owns: vec![OwnershipSpec {
                tenant,
                shard_start: start,
                shard_end: end,
            }],
            bloom_interval_secs: 1,
            data_dir: std::path::PathBuf::from("/tmp"),
        }
    }

    #[test]
    fn node_owns_configured_partition() {
        let node = QuickNode::new(cfg(42, 0, 511));
        assert!(node.owns(42, 0));
        assert!(node.owns(42, 511));
        assert!(!node.owns(42, 512));
        assert!(!node.owns(99, 0));
    }

    #[tokio::test]
    async fn connect_returns_owner_url() {
        let node = QuickNode::new(cfg(1, 0, 4095));
        let req = TransportRequest::new(1, RequestKind::Connect { user: 7, tenant: 1 });
        let resp = node.handle(req).await;
        assert!(resp.owner_url.is_some());
        assert_eq!(resp.seq, 1);
    }

    #[tokio::test]
    async fn write_owned_partition_advances_seq() {
        let node = QuickNode::new(cfg(42, 0, 4095));
        let before = node.replication().current_seq();
        let req = TransportRequest::new(
            1,
            RequestKind::Write {
                struct_id: 1,
                user: 7,
                tenant: 42,
                payload: vec![1, 2, 3],
            },
        );
        node.handle(req).await;
        assert!(node.replication().current_seq() > before);
    }

    #[tokio::test]
    async fn write_unowned_partition_returns_not_owner() {
        let node = QuickNode::new(cfg(42, 0, 511));
        let req = TransportRequest::new(
            1,
            RequestKind::Write {
                struct_id: 1,
                user: 7,
                tenant: 99,
                payload: Vec::new(),
            },
        );
        let resp = node.handle(req).await;
        assert_eq!(resp.payload, b"not_owner");
    }

    #[tokio::test]
    async fn search_owned_partition_succeeds() {
        let node = QuickNode::new(cfg(5, 0, 4095));
        let req = TransportRequest::new(
            1,
            RequestKind::SearchUnique {
                struct_id: 1,
                user: 1,
                tenant: 5,
            },
        );
        let resp = node.handle(req).await;
        assert_ne!(resp.payload, b"not_owner" as &[u8]);
    }

    #[tokio::test]
    async fn disconnect_always_succeeds() {
        let node = QuickNode::new(cfg(1, 0, 4095));
        let req = TransportRequest::new(1, RequestKind::Disconnect { user: 1, tenant: 1 });
        let resp = node.handle(req).await;
        assert_eq!(resp.seq, 1);
    }

    #[test]
    fn addr_to_node_id_is_deterministic() {
        assert_eq!(
            addr_to_node_id("10.0.0.1:7700"),
            addr_to_node_id("10.0.0.1:7700")
        );
        assert_ne!(
            addr_to_node_id("10.0.0.1:7700"),
            addr_to_node_id("10.0.0.2:7700")
        );
    }

    #[test]
    fn route_to_returns_addr_for_known_partition() {
        let node = QuickNode::new(cfg(42, 0, 4095));
        // This node owns tenant 42, so route_to should return this node's address.
        let addr = node.route_to(42, 0);
        assert!(addr.is_some());
    }
}
