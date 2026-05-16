//! [`QuickNode`] — the central state object for a running Quick-Node process.
//!
//! Combines partition ownership, cluster routing, replication bookkeeping,
//! event broadcasting, gossip-based peer discovery, and graceful drain into a
//! single cheaply-cloneable handle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::RwLock;

use crate::config::Config;
use crate::gossip::{GossipClient, GossipKind, GossipMessage, GossipResponse, GossipState};
use crate::ownership::{OwnershipMap, ShardRange, TransferTracker};
use crate::replication::ReplicationWatermark;
use crate::ring::{ConsistentRing, NodeId};
use wavedb_net::EventBus;
use wavedb_net::auth::{ClusterKey, TokenPurpose};
use wavedb_net::metrics::{HEAP_PAGE_SIZE, MAX_MAP_PAGES, QuickNodeMetrics};
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
    // ── Gossip + drain ────────────────────────────────────────────────────
    gossip: GossipState,
    gossip_client: GossipClient,
    /// Set to `true` when the operator triggers a graceful drain.
    /// All data handlers redirect clients to another node while draining.
    draining: AtomicBool,
    // ── Auth ──────────────────────────────────────────────────────────────
    /// Shared cluster secret for HMAC-SHA256 node-to-node auth.
    /// `None` in open/dev mode — gossip is accepted without a token.
    auth_key: Option<ClusterKey>,
    // ── Metrics ───────────────────────────────────────────────────────────
    write_count: AtomicU64,
    read_count: AtomicU64,
    write_bytes: AtomicU64,
    /// Per-page byte totals for hash-map style page occupancy.
    /// Write N goes to slot `N % MAX_MAP_PAGES`.
    page_bytes: Vec<AtomicU64>,
    start_time: Instant,
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
        let auth_key = config.cluster_key.clone();
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
                gossip: GossipState::new(),
                gossip_client: GossipClient::new(),
                draining: AtomicBool::new(false),
                auth_key,
                write_count: AtomicU64::new(0),
                read_count: AtomicU64::new(0),
                write_bytes: AtomicU64::new(0),
                page_bytes: (0..MAX_MAP_PAGES).map(|_| AtomicU64::new(0)).collect(),
                start_time: Instant::now(),
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

    /// `true` while a graceful drain is in progress.
    pub fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::Acquire)
    }

    /// Cluster key used to verify incoming gossip tokens, or `None` in open mode.
    pub fn auth_key(&self) -> Option<&ClusterKey> {
        self.inner.auth_key.as_ref()
    }

    /// Snapshot of this node's current metrics.
    pub fn metrics(&self) -> QuickNodeMetrics {
        let write_bytes = self.inner.write_bytes.load(Ordering::Relaxed);
        let page_count = (write_bytes + HEAP_PAGE_SIZE - 1) / HEAP_PAGE_SIZE;
        let raw: Vec<u64> = self
            .inner
            .page_bytes
            .iter()
            .map(|pb| pb.load(Ordering::Relaxed))
            .collect();
        let max_pb = raw.iter().copied().max().unwrap_or(0);
        let page_map: Vec<u8> = raw
            .iter()
            .map(|&b| {
                if max_pb == 0 {
                    0
                } else {
                    (b * 255 / max_pb) as u8
                }
            })
            .collect();
        QuickNodeMetrics {
            node_id: self.inner.node_id,
            listen_addr: self.inner.listen_addr.clone(),
            is_draining: self.inner.draining.load(Ordering::Relaxed),
            ring_size: self.inner.ring.read().node_count(),
            owned_partitions: self.inner.ownership.len(),
            write_count: self.inner.write_count.load(Ordering::Relaxed),
            read_count: self.inner.read_count.load(Ordering::Relaxed),
            uptime_secs: self.inner.start_time.elapsed().as_secs(),
            write_bytes,
            page_count,
            page_map,
            estimated_memory_bytes: write_bytes + 65536,
        }
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
    /// server handlers.
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
        if self.inner.draining.load(Ordering::Acquire) {
            return self.draining_resp(seq);
        }
        if !self.inner.ownership.owns(tenant, 0) {
            return self.not_owner_resp(seq);
        }
        self.inner.read_count.fetch_add(1, Ordering::Relaxed);
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
        if self.inner.draining.load(Ordering::Acquire) {
            return self.draining_resp(seq);
        }
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
        struct_id: u32,
        user: u64,
        tenant: u64,
        payload: Vec<u8>,
    ) -> TransportResponse {
        if self.inner.draining.load(Ordering::Acquire) {
            return self.draining_resp(seq);
        }
        if !self.inner.ownership.owns(tenant, 0) {
            return self.not_owner_resp(seq);
        }
        let payload_len = payload.len() as u64;
        self.inner.write_count.fetch_add(1, Ordering::Relaxed);
        self.inner.write_bytes.fetch_add(payload_len, Ordering::Relaxed);
        // Route to page by FNV-1a hash of (user, struct_id) so each user/struct
        // pair consistently hits the same page slot, creating realistic hotspots.
        let page_idx = fnv1a_page(user, struct_id, MAX_MAP_PAGES);
        self.inner.page_bytes[page_idx].fetch_add(payload_len, Ordering::Relaxed);
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
        if self.inner.draining.load(Ordering::Acquire) {
            return self.draining_resp(seq);
        }
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

    // ── Gossip ────────────────────────────────────────────────────────────

    /// Process an incoming [`GossipMessage`] and return the appropriate
    /// [`GossipResponse`].
    ///
    /// Deduplication ensures each `(origin, epoch)` pair is applied at most
    /// once even if multiple peers relay the same event.
    pub async fn handle_gossip(&self, msg: GossipMessage) -> GossipResponse {
        // Duplicate — already processed this event.
        if !self.inner.gossip.mark_seen(msg.origin, msg.epoch) {
            return match msg.kind {
                GossipKind::Announce => GossipResponse::NodeList {
                    nodes: self.known_nodes(),
                },
                GossipKind::Withdraw => GossipResponse::Ack,
            };
        }

        match &msg.kind {
            GossipKind::Announce => {
                let peer_id = addr_to_node_id(&msg.addr);
                {
                    let mut ring = self.inner.ring.write();
                    ring.add_node(peer_id, msg.addr.clone());
                }
                self.inner.replication.add_peer(peer_id);
                tracing::info!(
                    peer = %msg.addr,
                    peer_id,
                    "gossip: node announced, added to ring"
                );

                // Fan out to all known peers except the originator.
                self.fanout(msg, peer_id);

                GossipResponse::NodeList {
                    nodes: self.known_nodes(),
                }
            }
            GossipKind::Withdraw => {
                let peer_id = addr_to_node_id(&msg.addr);

                // Fan out before removing so the peer list is still intact.
                self.fanout(msg.clone(), peer_id);

                {
                    let mut ring = self.inner.ring.write();
                    ring.remove_node(peer_id);
                }
                self.inner.replication.remove_peer(peer_id);
                tracing::info!(
                    peer = %msg.addr,
                    peer_id,
                    "gossip: node withdrew, removed from ring"
                );

                GossipResponse::Ack
            }
        }
    }

    /// Announce this node's existence to all configured peers and merge their
    /// [`GossipResponse::NodeList`] replies into the local ring.
    ///
    /// Called once at startup, after the listening socket is bound.
    pub async fn announce_self(&self) {
        let epoch = self.inner.gossip.next_epoch();
        let token = self
            .inner
            .auth_key
            .as_ref()
            .map(|k| k.mint(self.inner.node_id, TokenPurpose::Gossip));
        let msg = GossipMessage {
            epoch,
            origin: self.inner.node_id,
            addr: self.inner.listen_addr.clone(),
            kind: GossipKind::Announce,
            token,
        };

        // Mark our own announce as seen so we don't process it if relayed back.
        self.inner.gossip.mark_seen(self.inner.node_id, epoch);

        for peer_addr in self.peer_addresses() {
            match self.inner.gossip_client.send(&peer_addr, &msg).await {
                Some(GossipResponse::NodeList { nodes }) => {
                    tracing::info!(
                        peer = %peer_addr,
                        discovered = nodes.len(),
                        "gossip: announce accepted, merging node list"
                    );
                    let mut ring = self.inner.ring.write();
                    for (id, addr) in nodes {
                        if id != self.inner.node_id {
                            ring.add_node(id, addr);
                            self.inner.replication.add_peer(id);
                        }
                    }
                }
                Some(_) | None => {
                    tracing::warn!(peer = %peer_addr, "gossip: announce to peer failed or got unexpected response");
                }
            }
        }
    }

    // ── Drain ─────────────────────────────────────────────────────────────

    /// Begin a graceful drain:
    ///
    /// 1. Set the `draining` flag so all data handlers redirect clients.
    /// 2. Flush in-memory writes to the Slow-Node (Phase 14 hook).
    /// 3. Send [`GossipKind::Withdraw`] to all peers so they remove this node
    ///    from their rings before it stops accepting connections.
    pub async fn drain(&self) {
        self.inner.draining.store(true, Ordering::Release);
        tracing::info!(
            node_id = self.inner.node_id,
            listen = %self.inner.listen_addr,
            "drain: started — redirecting clients"
        );

        self.sync_to_slow().await;

        let epoch = self.inner.gossip.next_epoch();
        let token = self
            .inner
            .auth_key
            .as_ref()
            .map(|k| k.mint(self.inner.node_id, TokenPurpose::Gossip));
        let msg = GossipMessage {
            epoch,
            origin: self.inner.node_id,
            addr: self.inner.listen_addr.clone(),
            kind: GossipKind::Withdraw,
            token,
        };
        self.inner.gossip.mark_seen(self.inner.node_id, epoch);

        for peer_addr in self.peer_addresses() {
            let _ = self.inner.gossip_client.send(&peer_addr, &msg).await;
        }

        tracing::info!(
            node_id = self.inner.node_id,
            "drain: withdraw gossip sent to all peers"
        );
    }

    /// Flush in-memory writes to the configured Slow-Node.
    ///
    /// Phase 14: replace with actual record serialisation and batch POST.
    async fn sync_to_slow(&self) {
        let Some(slow_addr) = self.inner.config.slow_node.as_deref() else {
            return;
        };
        // Phase 14: collect pending in-memory records and send them in a
        // FlushBatch to `http://{slow_addr}/flush`.  For now we just log the
        // intent so the operator knows the hook is in place.
        let write_seq = self.inner.replication.current_seq();
        tracing::info!(
            slow_node = slow_addr,
            write_seq,
            "drain: Phase-14 sync_to_slow (no-op until storage engine wired)"
        );
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

    /// Response returned while draining: same redirect hint as `not_owner` but
    /// with a distinct payload so clients can distinguish the reason.
    fn draining_resp(&self, seq: u64) -> TransportResponse {
        TransportResponse {
            seq,
            payload: b"draining".to_vec(),
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

    /// Listen addresses of all known peers (excludes self).
    fn peer_addresses(&self) -> Vec<String> {
        let ring = self.inner.ring.read();
        ring.nodes()
            .filter(|&id| id != self.inner.node_id)
            .filter_map(|id| ring.addr_of(id).map(ToString::to_string))
            .collect()
    }

    /// Listen addresses of all known peers, excluding `skip_id` and self.
    fn peer_addresses_except(&self, skip_id: NodeId) -> Vec<String> {
        let ring = self.inner.ring.read();
        ring.nodes()
            .filter(|&id| id != self.inner.node_id && id != skip_id)
            .filter_map(|id| ring.addr_of(id).map(ToString::to_string))
            .collect()
    }

    /// Snapshot of all nodes in the ring as `(node_id, addr)` pairs.
    fn known_nodes(&self) -> Vec<(NodeId, String)> {
        let ring = self.inner.ring.read();
        ring.nodes()
            .filter_map(|id| ring.addr_of(id).map(|a| (id, a.to_string())))
            .collect()
    }

    /// Fire-and-forget gossip fanout: spawn a task that relays `msg` to all
    /// peers except `skip_id` (the originator).
    fn fanout(&self, msg: GossipMessage, skip_id: NodeId) {
        let peers = self.peer_addresses_except(skip_id);
        if peers.is_empty() {
            return;
        }
        let node = self.clone();
        tokio::spawn(async move {
            for peer_addr in peers {
                if node
                    .inner
                    .gossip_client
                    .send(&peer_addr, &msg)
                    .await
                    .is_none()
                {
                    tracing::warn!(
                        peer = %peer_addr,
                        "gossip: fanout to peer failed"
                    );
                }
            }
        });
    }
}

// ── fnv1a_page ────────────────────────────────────────────────────────────────

/// Map `(user, struct_id)` to a page-map slot via FNV-1a so the same
/// caller always hits the same slot, creating realistic per-user hotspots.
fn fnv1a_page(user: u64, struct_id: u32, n_pages: usize) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in user.to_le_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for b in struct_id.to_le_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h % n_pages as u64) as usize
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
            cluster_key: None,
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
        let addr = node.route_to(42, 0);
        assert!(addr.is_some());
    }

    #[tokio::test]
    async fn draining_node_redirects_writes() {
        let node = QuickNode::new(cfg(42, 0, 4095));
        // Drain without network (no peers configured).
        node.drain().await;
        assert!(node.is_draining());

        let req = TransportRequest::new(
            1,
            RequestKind::Write {
                struct_id: 1,
                user: 1,
                tenant: 42,
                payload: Vec::new(),
            },
        );
        let resp = node.handle(req).await;
        assert_eq!(resp.payload, b"draining");
    }

    #[tokio::test]
    async fn draining_node_still_connects() {
        let node = QuickNode::new(cfg(1, 0, 4095));
        node.drain().await;

        // Connect is not affected by draining — clients use it to discover
        // the redirect target.
        let req = TransportRequest::new(1, RequestKind::Connect { user: 1, tenant: 1 });
        let resp = node.handle(req).await;
        assert_eq!(resp.seq, 1);
        assert_ne!(resp.payload, b"draining" as &[u8]);
    }

    #[tokio::test]
    async fn gossip_announce_adds_peer_to_ring() {
        let node = QuickNode::new(cfg(1, 0, 4095));
        let initial_peers = node.peer_addresses();
        assert!(initial_peers.is_empty());

        let msg = crate::gossip::GossipMessage {
            epoch: 1,
            origin: addr_to_node_id("10.0.0.2:7700"),
            addr: "10.0.0.2:7700".into(),
            kind: crate::gossip::GossipKind::Announce,
            token: None,
        };
        let resp = node.handle_gossip(msg).await;
        assert!(matches!(resp, GossipResponse::NodeList { .. }));

        let peers = node.peer_addresses();
        assert!(peers.contains(&"10.0.0.2:7700".to_string()));
    }

    #[tokio::test]
    async fn gossip_withdraw_removes_peer_from_ring() {
        let node = QuickNode::new(cfg(1, 0, 4095));

        // Add a peer via Announce.
        node.handle_gossip(crate::gossip::GossipMessage {
            epoch: 1,
            origin: addr_to_node_id("10.0.0.3:7700"),
            addr: "10.0.0.3:7700".into(),
            kind: crate::gossip::GossipKind::Announce,
            token: None,
        })
        .await;
        assert!(node.peer_addresses().contains(&"10.0.0.3:7700".to_string()));

        // Withdraw removes it.
        node.handle_gossip(crate::gossip::GossipMessage {
            epoch: 2,
            origin: addr_to_node_id("10.0.0.3:7700"),
            addr: "10.0.0.3:7700".into(),
            kind: crate::gossip::GossipKind::Withdraw,
            token: None,
        })
        .await;
        assert!(!node.peer_addresses().contains(&"10.0.0.3:7700".to_string()));
    }

    #[tokio::test]
    async fn gossip_dedup_prevents_double_add() {
        let node = QuickNode::new(cfg(1, 0, 4095));

        let msg = crate::gossip::GossipMessage {
            epoch: 5,
            origin: addr_to_node_id("10.0.0.4:7700"),
            addr: "10.0.0.4:7700".into(),
            kind: crate::gossip::GossipKind::Announce,
            token: None,
        };
        node.handle_gossip(msg.clone()).await;
        node.handle_gossip(msg).await; // duplicate — should be no-op

        // Still just one entry for this peer.
        let peers = node.peer_addresses();
        assert_eq!(
            peers
                .iter()
                .filter(|a| a.as_str() == "10.0.0.4:7700")
                .count(),
            1
        );
    }
}
