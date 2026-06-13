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
use crate::gossip::{
    GossipClient, GossipKind, GossipMessage, GossipResponse, GossipState,
};
use crate::ownership::{OwnershipMap, TransferTracker};
use crate::replication::{
    ReplicaRecord, ReplicateAck, ReplicateBatch, ReplicationWatermark,
};
use crate::ring::{ConsistentRing, NodeId};
use wavedb_core::query::Expr;
use wavedb_core::{Id, ObjectRegistry, Shape};
use wavedb_net::EventBus;
use wavedb_net::auth::{ClusterKey, TokenPurpose};
use wavedb_net::metrics::{MAX_MAP_PAGES, QuickNodeMetrics};
use wavedb_net::request::{
    ErrorCode, NodeError, RequestKind, TransportRequest, TransportResponse,
};
use wavedb_slow_node::flush::FlushBatch;
use wavedb_storage::{NodeStorage, VersionedRecord, tuple4_page};

/// Target copy count per partition.
///
/// One ring owner + `MIN_REPLICAS - 1` replicas holding the data for
/// redundancy (readme: `MIN_REPLICAS`, default 2).  With fewer physical
/// nodes the replica set is simply every node.
pub const MIN_REPLICAS: usize = 2;

/// Records per `POST /flush` batch when shipping history to the Slow-Node.
const FLUSH_BATCH_MAX: usize = 512;

/// A storage-engine failure mapped to the structured wire error.
fn storage_err(
    seq: u64,
    struct_id: u32,
    e: &wavedb_storage::StorageError,
) -> TransportResponse {
    TransportResponse::err(
        seq,
        NodeError {
            code: ErrorCode::Storage,
            struct_id,
            field: None,
            message: e.to_string(),
        },
    )
}

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
    /// Writes rejected by schema enforcement (unknown header, malformed
    /// payload, `validate` / `preprocess` hook failures).
    rejected_count: AtomicU64,
    /// Records stored on this node as a replica copy (via `/replicate`).
    replicated_count: AtomicU64,
    /// HTTP client for owner → replica fan-out posts.
    replicate_http: reqwest::Client,
    /// Per-page byte totals for hash-map style page occupancy.
    /// Write N goes to slot `N % MAX_MAP_PAGES`.
    page_bytes: Vec<AtomicU64>,
    start_time: Instant,
    /// On-disk storage opened from `config.data_dir`.  `None` when the
    /// config has an empty path (in-memory test usage).
    ///
    /// Every successful write goes through [`NodeStorage::commit_versioned_write`]
    /// — journal append + fsync **before** the handler returns Ok to the
    /// client.  This is the WAL durability contract.
    storage: Option<Arc<NodeStorage>>,
    /// Records committed since the last history flush, per tenant —
    /// drained by [`QuickNode::sync_to_slow`] into `POST /flush` batches.
    /// Only populated when a Slow-Node is configured.
    flush_pending: parking_lot::Mutex<
        std::collections::HashMap<u64, Vec<VersionedRecord>>,
    >,
    /// The application's static object registry (`declare_objects!`'s
    /// `REGISTRY`).  When present, every incoming write is checked against
    /// the schema: unknown `(struct_id, version)` headers are rejected, the
    /// struct's `validate` hook runs, then its `preprocess` hook — all
    /// **before** the WAL commit.  `None` = legacy schema-blind mode
    /// (opaque-byte storage; the shipped generic binary).
    registry: Option<&'static ObjectRegistry>,
}

impl QuickNode {
    /// Create a `QuickNode` from a parsed and validated [`Config`].
    ///
    /// Runs **schema-blind**: payloads are stored as opaque bytes and no
    /// validation/preprocess hooks fire.  Application node binaries should
    /// prefer [`QuickNode::with_registry`].
    pub fn new(config: Config) -> Self {
        Self::build(config, None)
    }

    /// Create a schema-aware `QuickNode`.
    ///
    /// `registry` is the `REGISTRY` static generated by the application's
    /// `declare_objects!` invocation.  With it attached, the node enforces
    /// on every write — before the WAL commit:
    ///
    /// 1. the record header `(struct_id, version)` is declared,
    /// 2. the payload decodes as that type,
    /// 3. the type's `validate` hook passes,
    /// 4. the type's `preprocess` hook is applied (the **transformed**
    ///    bytes are what gets committed).
    ///
    /// This is the seam that makes an application's Quick-Node binary a
    /// real backend instead of a byte bucket:
    ///
    /// ```rust,ignore
    /// declare_objects! { pub mod app_objects { orders: [Order1], … } }
    ///
    /// let node = QuickNode::with_registry(config, app_objects::REGISTRY);
    /// ```
    pub fn with_registry(
        config: Config,
        registry: &'static ObjectRegistry,
    ) -> Self {
        Self::build(config, Some(registry))
    }

    fn build(
        config: Config,
        registry: Option<&'static ObjectRegistry>,
    ) -> Self {
        let node_id = config.node_id();

        let mut ring = ConsistentRing::new();
        ring.add_node(node_id, config.listen.clone());

        // Ownership is ring-derived; the map starts empty and only holds
        // explicit transfer pins (see `QuickNode::owns`).
        let ownership = OwnershipMap::new();

        let replication = ReplicationWatermark::new();
        for peer_addr in config.peer_addrs() {
            let peer_id = addr_to_node_id(&peer_addr);
            ring.add_node(peer_id, peer_addr);
            replication.add_peer(peer_id);
        }

        let listen_addr = config.listen.clone();
        let auth_key = config.cluster_key.clone();

        // Open on-disk storage if a non-empty `data_dir` was supplied.
        // Tests that construct `Config` with `data_dir = PathBuf::from("/tmp")`
        // also get storage opened — they should swap to `PathBuf::new()`
        // for the in-memory path.
        let storage = if config.data_dir.as_os_str().is_empty() {
            None
        } else {
            match NodeStorage::open(&config.data_dir) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::error!(error = %e, dir = ?config.data_dir, "node storage open failed; running without persistence");
                    None
                }
            }
        };

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
                rejected_count: AtomicU64::new(0),
                replicated_count: AtomicU64::new(0),
                replicate_http: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(5))
                    .build()
                    .expect("reqwest client"),
                page_bytes: (0..MAX_MAP_PAGES)
                    .map(|_| AtomicU64::new(0))
                    .collect(),
                start_time: Instant::now(),
                storage,
                flush_pending: parking_lot::Mutex::new(
                    std::collections::HashMap::new(),
                ),
                registry,
            }),
        }
    }

    /// On-disk storage handle, if the node was configured with a
    /// non-empty `data_dir`.
    pub fn storage(&self) -> Option<&Arc<NodeStorage>> {
        self.inner.storage.as_ref()
    }

    /// The attached static object registry, if this node is schema-aware.
    pub fn registry(&self) -> Option<&'static ObjectRegistry> {
        self.inner.registry
    }

    /// Spawn a background tokio task that periodically compacts the journal.
    ///
    /// Every `interval_secs` seconds the task:
    /// 1. Flushes `data.bin` (page-table snapshot to disk).
    /// 2. Writes a checkpoint entry to the journal.
    /// 3. Truncates journal entries before that checkpoint (atomic rename).
    ///
    /// This keeps the journal file and the in-memory entry Vec bounded
    /// regardless of write volume.  The task stops automatically when
    /// the node is draining or when its `Arc<QuickNodeInner>` is dropped
    /// (weak-reference check).
    ///
    /// Returns `None` when `interval_secs == 0` or when the node has no
    /// on-disk storage.
    pub fn start_compaction_loop(
        &self,
        interval_secs: u64,
    ) -> Option<tokio::task::AbortHandle> {
        if interval_secs == 0 {
            return None;
        }
        let storage = self.inner.storage.as_ref()?.clone();
        let inner = Arc::downgrade(&self.inner);
        let interval = std::time::Duration::from_secs(interval_secs);
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(inner) = inner.upgrade() else { break };
                if inner.draining.load(Ordering::Relaxed) {
                    break;
                }
                if let Err(e) = storage.compact_journal() {
                    tracing::warn!(error = %e, "journal compaction failed");
                }
            }
        })
        .abort_handle();
        Some(handle)
    }

    /// Spawn the liveness heartbeat: every `interval`, announce this node to
    /// each known peer.  A peer that fails `strikes` consecutive announces
    /// is **evicted from the ring** — at which point the ring re-derives
    /// ownership and the next node clockwise takes over the dead node's
    /// partitions.  This is the crash-failover half of runtime ownership
    /// negotiation (graceful departures use drain → gossip Withdraw).
    ///
    /// An evicted node that was merely partitioned re-adds itself with its
    /// own next announce — membership converges from the gossip stream.
    pub fn start_heartbeat_loop(
        &self,
        interval: std::time::Duration,
        strikes: u32,
    ) -> tokio::task::AbortHandle {
        let inner = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let mut misses: std::collections::HashMap<NodeId, u32> =
                std::collections::HashMap::new();
            loop {
                tokio::time::sleep(interval).await;
                let Some(inner) = inner.upgrade() else { break };
                if inner.draining.load(Ordering::Relaxed) {
                    break;
                }
                let node = Self { inner };
                node.heartbeat_tick(&mut misses, strikes).await;
            }
        })
        .abort_handle()
    }

    /// One heartbeat round: announce to every peer, evict peers that have
    /// missed `strikes` rounds in a row.
    async fn heartbeat_tick(
        &self,
        misses: &mut std::collections::HashMap<NodeId, u32>,
        strikes: u32,
    ) {
        let peers: Vec<(NodeId, String)> = {
            let ring = self.inner.ring.read();
            ring.nodes()
                .filter(|&id| id != self.inner.node_id)
                .filter_map(|id| ring.addr_of(id).map(|a| (id, a.to_string())))
                .collect()
        };

        for (peer_id, addr) in peers {
            let epoch = self.inner.gossip.next_epoch();
            self.inner.gossip.mark_seen(self.inner.node_id, epoch);
            let msg =
                GossipMessage {
                    epoch,
                    origin: self.inner.node_id,
                    addr: self.inner.listen_addr.clone(),
                    kind: GossipKind::Announce,
                    token: self.inner.auth_key.as_ref().map(|k| {
                        k.mint(self.inner.node_id, TokenPurpose::Gossip)
                    }),
                };
            if self.inner.gossip_client.send(&addr, &msg).await.is_some() {
                misses.remove(&peer_id);
            } else {
                {
                    let n = misses.entry(peer_id).or_insert(0);
                    *n += 1;
                    if *n >= strikes {
                        misses.remove(&peer_id);
                        self.evict_peer(peer_id);
                        tracing::warn!(
                            peer = peer_id,
                            after_misses = strikes,
                            "heartbeat: peer evicted from ring — ownership re-derives",
                        );
                    }
                }
            }
        }
    }

    /// Remove a dead peer from the ring and replication table.  Partitions
    /// it owned re-derive to the surviving nodes on the next `owns()` call —
    /// no records move; replicas already hold the data.
    fn evict_peer(&self, peer: NodeId) {
        self.inner.ring.write().remove_node(peer);
        self.inner.replication.remove_peer(peer);
    }

    /// Test-only: inject a ring member directly (bypasses gossip).
    #[cfg(test)]
    pub(crate) fn ring_write_add_for_tests(&self, id: NodeId, addr: &str) {
        self.inner.ring.write().add_node(id, addr);
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
    ///
    /// Ownership is **derived from the consistent-hash ring**, not from
    /// configuration: every node computes the same owner from the same ring
    /// membership, so agreement needs no extra handshake.  Membership moves
    /// via gossip (announce / withdraw), which is what makes ownership a
    /// runtime-negotiated property — a node joining or leaving reassigns
    /// partitions automatically.  A solo node is the entire ring and
    /// therefore owns everything.
    ///
    /// An explicit entry in the [`OwnershipMap`] (a transfer pin) overrides
    /// the ring while a range move is in flight.
    pub fn owns(&self, tenant: u64, shard: u16) -> bool {
        if self.inner.ownership.owns(tenant, shard) {
            return true;
        }
        self.inner.ring.read().owner_of(tenant, shard)
            == Some(self.inner.node_id)
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
        // Relative scale: the busiest slot is 255 and everything else is
        // proportional, with a floor of 1 for any slot that saw traffic.
        // (A fixed bytes-per-slot threshold truncated light traffic to 0
        // and the heat map rendered blank.)
        let raw_slots: Vec<u64> = self
            .inner
            .page_bytes
            .iter()
            .map(|pb| pb.load(Ordering::Relaxed))
            .collect();
        let busiest = raw_slots.iter().copied().max().unwrap_or(0).max(1);
        let page_map: Vec<u8> = raw_slots
            .iter()
            .map(|&b| {
                if b == 0 {
                    0
                } else {
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        (b.saturating_mul(255) / busiest).clamp(1, 255) as u8
                    }
                }
            })
            .collect();
        // Count only slots that have received at least one write.
        let page_count = page_map.iter().filter(|&&o| o > 0).count() as u64;
        let (
            has_storage,
            journal_bytes,
            heap_bytes,
            data_file_bytes,
            data_file_page_count,
            data_file_used_pages,
        ) = self.inner.storage.as_ref().map_or(
            (false, 0u64, 0u64, 0u64, 0u64, 0u64),
            |storage| {
                let j = std::fs::metadata(storage.data_dir.join("journal.log"))
                    .map_or(0, |m| m.len());
                let h = std::fs::metadata(storage.data_dir.join("heap.bin"))
                    .map_or(0, |m| m.len());
                let d = std::fs::metadata(storage.data_dir.join("data.bin"))
                    .map_or(0, |m| m.len());
                let df_pages = storage.data_file.page_count_now();
                let df_used = storage.data_file.used_page_count();
                (true, j, h, d, df_pages, df_used)
            },
        );
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
            has_storage,
            journal_bytes,
            heap_bytes,
            data_file_bytes,
            data_file_page_count,
            data_file_used_pages,
            rejected_count: self.inner.rejected_count.load(Ordering::Relaxed),
            replicated_count: self
                .inner
                .replicated_count
                .load(Ordering::Relaxed),
            data_dir: self
                .inner
                .storage
                .as_ref()
                .map_or_else(String::new, |s| s.data_dir.display().to_string()),
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
            RequestKind::Connect { user, tenant } => {
                self.handle_connect(req.seq, user, tenant)
            }
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
            } => self.handle_query(req.seq, struct_id, user, tenant, &filter),
            // Write dispatches via spawn_blocking so the journal fsync doesn't
            // pin a Tokio worker thread.  Without this, 3900 concurrent clients
            // block all workers and the /metrics HTTP handler never gets CPU.
            RequestKind::Write {
                struct_id,
                user,
                tenant,
                payload,
            } => {
                let node = self.clone();
                let seq = req.seq;
                tokio::task::spawn_blocking(move || {
                    node.handle_write(seq, struct_id, user, tenant, &payload)
                })
                .await
                .unwrap_or_else(|_| TransportResponse {
                    seq,
                    payload: b"storage_error".to_vec(),
                    owner_url: None,
                    backup_url: None,
                    notifications: Vec::new(),
                    error: None,
                })
            }
            RequestKind::Delete { id, user, tenant } => {
                self.handle_delete(req.seq, id, user, tenant)
            }
            RequestKind::Disconnect { user, tenant } => {
                self.handle_disconnect(req.seq, user, tenant)
            }
        }
    }

    // ── Individual handlers ───────────────────────────────────────────────

    fn handle_connect(
        &self,
        seq: u64,
        _user: u64,
        _tenant: u64,
    ) -> TransportResponse {
        TransportResponse {
            seq,
            payload: Vec::new(),
            owner_url: Some(format!("ws://{}", self.inner.listen_addr)),
            backup_url: self.backup_url(),
            notifications: Vec::new(),
            error: None,
        }
    }

    fn handle_search_unique(
        &self,
        seq: u64,
        struct_id: u32,
        _user: u64,
        tenant: u64,
    ) -> TransportResponse {
        if self.inner.draining.load(Ordering::Acquire) {
            return self.draining_resp(seq);
        }
        if !self.owns(tenant, 0) {
            return self.not_owner_resp(seq, tenant);
        }
        self.inner.read_count.fetch_add(1, Ordering::Relaxed);

        // In-memory nodes (no `data_dir`) have nothing to look up.
        let Some(storage) = self.inner.storage.as_ref() else {
            return TransportResponse::ok(seq, Vec::new());
        };

        // The live tracker's newest entry IS the live Unique record —
        // older entries are prior versions left behind by anchor rotation.
        let live = match storage.data_file.live_ids(struct_id, tenant) {
            Ok(ids) => ids,
            Err(e) => return storage_err(seq, struct_id, &e),
        };
        let Some(&newest) = live.last() else {
            return TransportResponse::ok(seq, Vec::new());
        };

        match storage.data_file.read_versioned(Id::from_raw(newest)) {
            // Stored payload is already the client wire shape:
            // `[STRUCT_VERSION u8][wire body]`.
            Ok(Some(rec)) => TransportResponse::ok(seq, rec.data),
            Ok(None) => {
                tracing::warn!(
                    struct_id,
                    tenant,
                    id = newest,
                    "live tracker points at a missing record"
                );
                TransportResponse::ok(seq, Vec::new())
            }
            Err(e) => storage_err(seq, struct_id, &e),
        }
    }

    fn handle_query(
        &self,
        seq: u64,
        struct_id: u32,
        _user: u64,
        tenant: u64,
        filter: &[u8],
    ) -> TransportResponse {
        if self.inner.draining.load(Ordering::Acquire) {
            return self.draining_resp(seq);
        }
        if !self.owns(tenant, 0) {
            return self.not_owner_resp(seq, tenant);
        }
        self.inner.read_count.fetch_add(1, Ordering::Relaxed);

        let Ok(expr) = Expr::from_bytes(filter) else {
            return TransportResponse::err(
                seq,
                NodeError {
                    code: ErrorCode::MalformedPayload,
                    struct_id,
                    field: None,
                    message: "query filter does not decode as an Expr".into(),
                },
            );
        };

        let Some(storage) = self.inner.storage.as_ref() else {
            return TransportResponse::ok(seq, Vec::new());
        };

        // Field comparisons need the descriptor's offsets; a schema-blind
        // node can only serve unfiltered scans.
        if self.inner.registry.is_none() && expr != Expr::All {
            return TransportResponse::err(
                seq,
                NodeError {
                    code: ErrorCode::Unsupported,
                    struct_id,
                    field: None,
                    message: "filtered queries require a schema registry; \
                              this node is schema-blind"
                        .into(),
                },
            );
        }

        let live = match storage.data_file.live_ids(struct_id, tenant) {
            Ok(ids) => ids,
            Err(e) => return storage_err(seq, struct_id, &e),
        };

        // Response wire shape: Vec<(stored_version, record_body)>.
        let mut entries: Vec<(u8, Vec<u8>)> = Vec::new();
        for id in live {
            let rec = match storage.data_file.read_versioned(Id::from_raw(id)) {
                Ok(Some(rec)) => rec,
                Ok(None) => continue, // raced with a delete — skip
                Err(e) => return storage_err(seq, struct_id, &e),
            };
            let Some((&version, body)) = rec.data.split_first() else {
                continue; // schema-blind era record with no version byte
            };
            let keep = match (&expr, self.inner.registry) {
                (Expr::All, _) => true,
                (_, Some(reg)) => reg
                    .find(wavedb_core::wire::pack_header(struct_id, version))
                    .is_some_and(|desc| {
                        wavedb_core::query::eval(desc, body, &expr)
                    }),
                // Unreachable: schema-blind + non-All rejected above.
                (_, None) => false,
            };
            if keep {
                entries.push((version, body.to_vec()));
            }
        }

        match wavedb_core::wire::to_wire(&entries) {
            Ok(payload) => TransportResponse::ok(seq, payload),
            Err(e) => TransportResponse::err(
                seq,
                NodeError {
                    code: ErrorCode::Storage,
                    struct_id,
                    field: None,
                    message: format!("query response encode: {e}"),
                },
            ),
        }
    }

    fn handle_write(
        &self,
        seq: u64,
        struct_id: u32,
        user: u64,
        tenant: u64,
        payload: &[u8],
    ) -> TransportResponse {
        if self.inner.draining.load(Ordering::Acquire) {
            return self.draining_resp(seq);
        }
        if !self.owns(tenant, 0) {
            return self.not_owner_resp(seq, tenant);
        }

        // ── Schema enforcement (registry-attached nodes only) ────────────
        //
        // Wire shape of `payload`: `[STRUCT_VERSION u8][wire body]`.
        // Order matters: `validate` first (the shared client/server contract
        // over the bytes the client sent), `preprocess` second (the
        // server-authoritative transform of accepted data).  What gets
        // committed below is the **preprocessed** payload.
        let committed: std::borrow::Cow<'_, [u8]> = if let Some(reg) =
            self.inner.registry
        {
            let Some((&version, body)) = payload.split_first() else {
                return TransportResponse::err(
                    seq,
                    NodeError {
                        code: ErrorCode::MalformedPayload,
                        struct_id,
                        field: None,
                        message: "empty write payload (missing version byte)"
                            .into(),
                    },
                );
            };
            let header = wavedb_core::wire::pack_header(struct_id, version);

            if let Err(e) = reg.validate(header, body) {
                self.inner.rejected_count.fetch_add(1, Ordering::Relaxed);
                return TransportResponse::err(
                    seq,
                    to_node_error(e, struct_id, ErrorCode::ValidationFailed),
                );
            }
            match reg.preprocess(header, body) {
                // No hook — commit the client's bytes unchanged.
                Ok(None) => std::borrow::Cow::Borrowed(payload),
                Ok(Some(new_body)) => {
                    let mut owned = Vec::with_capacity(1 + new_body.len());
                    owned.push(version);
                    owned.extend_from_slice(&new_body);
                    std::borrow::Cow::Owned(owned)
                }
                Err(e) => {
                    self.inner.rejected_count.fetch_add(1, Ordering::Relaxed);
                    return TransportResponse::err(
                        seq,
                        to_node_error(
                            e,
                            struct_id,
                            ErrorCode::PreprocessFailed,
                        ),
                    );
                }
            }
        } else {
            std::borrow::Cow::Borrowed(payload)
        };

        let payload_len = committed.len() as u64;
        // Assign the per-record sequence first so the synthetic Id carries
        // distinct entropy in its low bits — that is what makes the heat
        // map spread uniformly instead of collapsing onto one slot per user.
        let write_seq = self.inner.replication.next_seq();
        let id = Id::new(tenant, 0, struct_id, write_seq);

        // Stamp the engine-assigned Id into the record's own `id` field so
        // query/search results carry their real address (clients send it
        // zeroed and use the returned value for `delete`).  Registry mode
        // only — the field offset comes from the descriptor.
        let committed = self.stamp_engine_id(struct_id, id, committed);

        // ── Write-Ahead Logging commit ───────────────────────────────────
        //
        // When the node has on-disk storage attached, the write is only
        // confirmed to the client after `NodeStorage::commit_versioned_write`
        // returns Ok — which means:
        //   1. journal entry appended
        //   2. journal fsynced to disk (durability point)
        //   3. data file updated
        // A failure at any step returns a structured `Storage` error so
        // the client can retry; the counters are NOT bumped on failure.
        if let Some(storage) = self.inner.storage.as_ref() {
            if let Err(e) = storage.commit_write(id.raw(), committed.to_vec()) {
                tracing::error!(error = %e, id = ?id, "WAL commit failed");
                return TransportResponse::err(
                    seq,
                    NodeError {
                        code: ErrorCode::Storage,
                        struct_id,
                        field: None,
                        message: e.to_string(),
                    },
                );
            }
            // Make the committed record discoverable.  An index failure is
            // logged, not returned: the WAL already holds the record, and
            // journal recovery re-derives the tracker entry on restart.
            self.index_committed(storage, id, &committed);

            // Queue for the next history flush to the Slow-Node.
            if self.inner.config.slow_node.is_some() {
                self.inner
                    .flush_pending
                    .lock()
                    .entry(tenant)
                    .or_default()
                    .push(VersionedRecord::new(id.raw(), committed.to_vec()));
            }
        }

        // Only now bump metrics — they reflect *committed* writes, not
        // attempted ones.
        self.inner.write_count.fetch_add(1, Ordering::Relaxed);
        self.inner
            .write_bytes
            .fetch_add(payload_len, Ordering::Relaxed);
        // Heat-map slot mirrors the storage layer's tuple4 routing so the
        // monitor's heat map reflects actual on-disk distribution.
        #[allow(clippy::cast_possible_truncation)]
        let page_idx =
            tuple4_page(struct_id, tenant, 0, write_seq, MAX_MAP_PAGES as u64)
                as usize;
        let _ = user; // user is no longer part of the slot key — preserved in payload.
        self.inner.page_bytes[page_idx]
            .fetch_add(payload_len, Ordering::Relaxed);

        // ── Replica fan-out ──────────────────────────────────────────────
        // Push the committed bytes to the next ring nodes so the data
        // survives this node: one writer (us, per the ring), n copies.
        self.replicate_to_peers(
            tenant,
            write_seq,
            id.raw(),
            committed.into_owned(),
        );

        TransportResponse::ok(seq, Vec::new())
    }

    fn handle_delete(
        &self,
        seq: u64,
        id: u128,
        _user: u64,
        tenant: u64,
    ) -> TransportResponse {
        if self.inner.draining.load(Ordering::Acquire) {
            return self.draining_resp(seq);
        }
        if !self.owns(tenant, 0) {
            return self.not_owner_resp(seq, tenant);
        }

        let parsed = Id::from_raw(id);
        // The tenant rides in the Id's top bits — a session may only
        // tombstone records inside its own partition.
        if parsed.tenant_id() != tenant {
            return TransportResponse::err(
                seq,
                NodeError {
                    code: ErrorCode::MalformedPayload,
                    struct_id: parsed.struct_id(),
                    field: None,
                    message: "record id does not belong to this tenant".into(),
                },
            );
        }

        // Delete = drop from the live tracker.  The versioned record stays
        // stored (history is append-only); it just stops being served.
        // Unknown ids are fine — delete is idempotent.
        if let Some(storage) = self.inner.storage.as_ref() {
            if let Err(e) =
                storage
                    .data_file
                    .live_remove(parsed.struct_id(), tenant, id)
            {
                return storage_err(seq, parsed.struct_id(), &e);
            }
        }
        TransportResponse::ok(seq, Vec::new())
    }

    #[allow(clippy::unused_self)]
    const fn handle_disconnect(
        &self,
        seq: u64,
        _user: u64,
        _tenant: u64,
    ) -> TransportResponse {
        TransportResponse {
            seq,
            payload: Vec::new(),
            owner_url: None,
            backup_url: None,
            notifications: Vec::new(),
            error: None,
        }
    }

    // ── Gossip ────────────────────────────────────────────────────────────

    /// Process an incoming [`GossipMessage`] and return the appropriate
    /// [`GossipResponse`].
    ///
    /// Deduplication ensures each `(origin, epoch)` pair is applied at most
    /// once even if multiple peers relay the same event.
    #[allow(clippy::unused_async)]
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
                    drop(ring);
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

    /// Flush records committed since the last sync to the configured
    /// Slow-Node, one `POST /flush` batch per tenant chunk.
    ///
    /// Failures re-queue the records for the next attempt — history
    /// shipping is at-least-once; the Slow-Node's `(tenant, id)` index
    /// makes re-delivery idempotent.
    pub async fn sync_to_slow(&self) {
        let Some(slow_addr) = self.inner.config.slow_node.as_deref() else {
            return;
        };

        let pending: Vec<(u64, Vec<VersionedRecord>)> = {
            let mut map = self.inner.flush_pending.lock();
            map.drain().collect()
        };
        if pending.is_empty() {
            return;
        }

        let write_seq = self.inner.replication.current_seq();
        let url = format!("http://{slow_addr}/flush");

        for (tenant, records) in pending {
            let mut failed: Vec<VersionedRecord> = Vec::new();
            for chunk in records.chunks(FLUSH_BATCH_MAX) {
                let batch = FlushBatch {
                    write_seq,
                    tenant,
                    records: chunk.to_vec(),
                    token: self.inner.auth_key.as_ref().map(|k| {
                        k.mint(self.inner.node_id, TokenPurpose::Flush)
                    }),
                };
                let Ok(body) = wavedb_net::frame::encode_payload(&batch) else {
                    tracing::error!(tenant, "flush batch encode failed");
                    failed.extend_from_slice(chunk);
                    continue;
                };

                let sent = self
                    .inner
                    .replicate_http
                    .post(&url)
                    .header("Content-Type", "application/octet-stream")
                    .body(body.to_vec())
                    .send()
                    .await;
                match sent {
                    Ok(resp) if resp.status().is_success() => {
                        tracing::debug!(
                            tenant,
                            records = chunk.len(),
                            "history flushed to slow node"
                        );
                    }
                    Ok(resp) => {
                        tracing::warn!(
                            tenant,
                            status = %resp.status(),
                            "slow node rejected flush — re-queueing"
                        );
                        failed.extend_from_slice(chunk);
                    }
                    Err(e) => {
                        tracing::warn!(
                            tenant,
                            error = %e,
                            "slow node unreachable — re-queueing flush"
                        );
                        failed.extend_from_slice(chunk);
                    }
                }
            }
            if !failed.is_empty() {
                let mut map = self.inner.flush_pending.lock();
                let entry = map.entry(tenant).or_default();
                // Failed (older) records go before anything written since
                // the drain above, preserving per-tenant commit order.
                failed.append(entry);
                *entry = failed;
                drop(map);
            }
        }
    }

    /// Spawn the periodic history-flush task: every `interval_secs`,
    /// [`Self::sync_to_slow`].  Returns `None` when no Slow-Node is
    /// configured or `interval_secs == 0`.  The task exits when the node
    /// drops or starts draining (drain runs its own final sync).
    pub fn start_flush_loop(
        &self,
        interval_secs: u64,
    ) -> Option<tokio::task::AbortHandle> {
        if interval_secs == 0 || self.inner.config.slow_node.is_none() {
            return None;
        }
        let inner = Arc::downgrade(&self.inner);
        let interval = std::time::Duration::from_secs(interval_secs);
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(inner) = inner.upgrade() else { break };
                if inner.draining.load(Ordering::Relaxed) {
                    break;
                }
                let node = Self { inner };
                node.sync_to_slow().await;
            }
        })
        .abort_handle();
        Some(handle)
    }

    /// Overwrite the record's embedded `id` field with the engine-assigned
    /// Id (registry mode; no-op for schema-blind nodes or layouts without
    /// a 16-byte `id` field).
    fn stamp_engine_id<'a>(
        &self,
        struct_id: u32,
        id: Id,
        committed: std::borrow::Cow<'a, [u8]>,
    ) -> std::borrow::Cow<'a, [u8]> {
        let Some(reg) = self.inner.registry else {
            return committed;
        };
        let mut owned = committed.into_owned();
        let id_field = owned.split_first().and_then(|(&version, _)| {
            reg.find(wavedb_core::wire::pack_header(struct_id, version))
                .and_then(|desc| desc.field("id"))
        });
        if let Some(fd) = id_field {
            let start = 1 + fd.stack_offset;
            if fd.stack_size == 16 && owned.len() >= start + 16 {
                owned[start..start + 16]
                    .copy_from_slice(&id.raw().to_le_bytes());
            }
        }
        std::borrow::Cow::Owned(owned)
    }

    /// Append (or, for Unique shapes, rotate) the committed record's entry
    /// in its `(struct, tenant)` live tracker.
    ///
    /// `payload` is the stored bytes (`[version][body]`); with a registry
    /// attached the version resolves the shape, so Unique structs keep a
    /// single live entry while everything else appends.  Schema-blind
    /// nodes always append — the newest entry still serves `SearchUnique`
    /// correctly.
    fn index_committed(&self, storage: &NodeStorage, id: Id, payload: &[u8]) {
        let unique = self
            .inner
            .registry
            .and_then(|reg| {
                payload.split_first().and_then(|(&version, _)| {
                    reg.find(wavedb_core::wire::pack_header(
                        id.struct_id(),
                        version,
                    ))
                })
            })
            .is_some_and(|desc| desc.shape == Shape::Unique);

        let result = if unique {
            storage.data_file.live_set_single(
                id.struct_id(),
                id.tenant_id(),
                id.raw(),
            )
        } else {
            storage.data_file.live_append(
                id.struct_id(),
                id.tenant_id(),
                id.raw(),
            )
        };
        if let Err(e) = result {
            tracing::error!(
                error = %e,
                id = ?id,
                "live tracker update failed — record committed but \
                 undiscoverable until journal recovery"
            );
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    fn not_owner_resp(&self, seq: u64, tenant: u64) -> TransportResponse {
        TransportResponse {
            seq,
            payload: b"not_owner".to_vec(),
            // Real owner per the ring — the client should re-dial this URL.
            owner_url: self
                .route_to(tenant, 0)
                .map(|addr| format!("ws://{addr}/ws"))
                .or_else(|| self.route_to_owner_hint()),
            backup_url: None,
            notifications: Vec::new(),
            error: None,
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
            error: None,
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

    // ── Replica fan-out ──────────────────────────────────────────────────

    /// Push a committed write to this partition's replica set (the next
    /// [`MIN_REPLICAS`] minus one distinct ring nodes after us).
    ///
    /// Fire-and-forget per peer: the client's ack never waits on replicas
    /// — durability to the caller is the owner's WAL, redundancy is
    /// asynchronous.  Each successful peer ack advances the
    /// [`ReplicationWatermark`].
    fn replicate_to_peers(
        &self,
        tenant: u64,
        write_seq: u64,
        record_id: u128,
        data: Vec<u8>,
    ) {
        // Snapshot the replica addresses under the lock, then release it
        // before any awaiting happens.
        let peers: Vec<(NodeId, String)> = {
            let ring = self.inner.ring.read();
            ring.replicas_of(tenant, 0, MIN_REPLICAS)
                .into_iter()
                .filter(|&id| id != self.inner.node_id)
                .filter_map(|id| ring.addr_of(id).map(|a| (id, a.to_string())))
                .collect()
        };
        if peers.is_empty() {
            return; // solo node — the ring is just us, nothing to copy to
        }

        let batch =
            ReplicateBatch {
                origin: self.inner.node_id,
                write_seq,
                records: vec![ReplicaRecord {
                    id: record_id,
                    data,
                }],
                token: self.inner.auth_key.as_ref().map(|k| {
                    k.mint(self.inner.node_id, TokenPurpose::Replicate)
                }),
            };
        let Ok(body) = wavedb_net::frame::encode_payload(&batch) else {
            tracing::error!("replicate batch encode failed");
            return;
        };

        for (peer_id, addr) in peers {
            self.inner.replication.add_peer(peer_id);
            self.inner.replication.record_send(peer_id, write_seq);
            let http = self.inner.replicate_http.clone();
            let watermark = self.inner.replication.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let url = format!("http://{addr}/replicate");
                match http.post(&url).body(body).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        if let Ok(bytes) = resp.bytes().await {
                            if let Ok(ack) = wavedb_net::frame::decode_payload::<
                                ReplicateAck,
                            >(
                                &bytes
                            ) {
                                watermark.record_ack(peer_id, ack.write_seq);
                            }
                        }
                    }
                    Ok(resp) => tracing::warn!(
                        peer = peer_id,
                        status = %resp.status(),
                        "replica rejected batch",
                    ),
                    Err(e) => tracing::warn!(
                        peer = peer_id,
                        error = %e,
                        "replica unreachable",
                    ),
                }
            });
        }
    }

    /// Store a batch received from a partition owner (`POST /replicate`).
    ///
    /// No ownership gate and no hooks here on purpose: the bytes are the
    /// owner's canonical, already-validated, already-preprocessed payload
    /// — a replica stores them verbatim.
    pub fn apply_replica(&self, batch: &ReplicateBatch) -> ReplicateAck {
        if let Some(storage) = self.inner.storage.as_ref() {
            for (i, rec) in batch.records.iter().enumerate() {
                if let Err(e) = storage.commit_write(rec.id, rec.data.clone()) {
                    tracing::error!(error = %e, id = rec.id, "replica WAL commit failed");
                    continue;
                }
                // Replicas index too, so the partition stays queryable
                // when the ring re-derives ownership to this node.
                let id = Id::from_raw(rec.id);
                self.index_committed(storage, id, &rec.data);
                // Replicated bytes land on disk like local writes, so feed
                // the heat-map slots too — otherwise a pure-replica node
                // monitors as an empty page map over a multi-MB data.bin.
                #[allow(clippy::cast_possible_truncation)]
                let page_idx = tuple4_page(
                    id.struct_id(),
                    id.tenant_id(),
                    0,
                    batch.write_seq.wrapping_add(i as u64),
                    MAX_MAP_PAGES as u64,
                ) as usize;
                self.inner.page_bytes[page_idx]
                    .fetch_add(rec.data.len() as u64, Ordering::Relaxed);
            }
        }
        self.inner
            .replicated_count
            .fetch_add(batch.records.len() as u64, Ordering::Relaxed);
        ReplicateAck {
            write_seq: batch.write_seq,
        }
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

// ── Schema-rejection mapping ─────────────────────────────────────────────────

/// Convert a schema-enforcement failure into the structured [`NodeError`]
/// sent back to the client.
///
/// `hook_code` distinguishes which stage rejected the record
/// ([`ErrorCode::ValidationFailed`] vs [`ErrorCode::PreprocessFailed`]) —
/// the registry returns the same `Error::Validation` shape for both.
fn to_node_error(
    e: wavedb_core::Error,
    struct_id: u32,
    hook_code: ErrorCode,
) -> NodeError {
    use wavedb_core::Error as E;
    match e {
        // Registry has no entry — `struct_id` carries the FULL header here
        // (documented on `ErrorCode::UnknownHeader`).
        E::UnknownHeader(header) => NodeError {
            code: ErrorCode::UnknownHeader,
            struct_id: header,
            field: None,
            message: format!(
                "record header {header:#010x} not declared in the node's registry"
            ),
        },
        E::Validation { struct_id, source } => NodeError {
            code: hook_code,
            struct_id,
            field: source.field,
            message: source.message,
        },
        E::Wire(w) => NodeError {
            code: ErrorCode::MalformedPayload,
            struct_id,
            field: None,
            message: w.to_string(),
        },
        other => NodeError {
            code: hook_code,
            struct_id,
            field: None,
            message: other.to_string(),
        },
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

    /// In-memory test config — uses an empty `data_dir` so `QuickNode::new`
    /// skips opening real storage files.  Tests that need on-disk storage
    /// should construct their own `Config` with a `tempdir().path()`.
    ///
    /// Ownership is ring-derived now: a solo test node owns **every**
    /// partition.  The legacy `(tenant, start, end)` parameters are
    /// accepted and ignored so older call sites read naturally.
    fn cfg(_tenant: u64, _start: u16, _end: u16) -> Config {
        Config {
            listen: "127.0.0.1:7700".into(),
            peers: String::new(),
            slow_node: None,
            bloom_interval_secs: 1,
            journal_compact_secs: 30,
            // Empty path → in-memory mode (no storage files opened).
            // Tests that need real storage build their own Config with
            // `tempfile::tempdir()`.
            data_dir: std::path::PathBuf::new(),
            cluster_key: None,
        }
    }

    #[test]
    fn solo_node_owns_every_partition() {
        // Ring-derived ownership: a solo node IS the ring, so it owns
        // every tenant and shard without any configuration.
        let node = QuickNode::new(cfg(42, 0, 511));
        assert!(node.owns(42, 0));
        assert!(node.owns(42, 4095));
        assert!(node.owns(99, 0));
        assert!(node.owns(u64::from(u32::MAX), 7));
    }

    /// Find a tenant whose ring owner is `peer` rather than the local node.
    fn tenant_owned_by_peer(node: &QuickNode, peer: NodeId) -> u64 {
        let ring = node.inner.ring.read();
        (0..10_000u64)
            .find(|&t| ring.owner_of(t, 0) == Some(peer))
            .expect("some tenant must hash to the peer")
    }

    #[tokio::test]
    async fn connect_returns_owner_url() {
        let node = QuickNode::new(cfg(1, 0, 4095));
        let req = TransportRequest::new(
            1,
            RequestKind::Connect { user: 7, tenant: 1 },
        );
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
    async fn write_to_peer_owned_partition_returns_not_owner() {
        let node = QuickNode::new(cfg(42, 0, 511));
        // Join a second node so the ring splits ownership.
        let peer = addr_to_node_id("10.9.9.9:7700");
        node.inner.ring.write().add_node(peer, "10.9.9.9:7700");
        let tenant = tenant_owned_by_peer(&node, peer);

        let req = TransportRequest::new(
            1,
            RequestKind::Write {
                struct_id: 1,
                user: 7,
                tenant,
                payload: Vec::new(),
            },
        );
        let resp = node.handle(req).await;
        assert_eq!(resp.payload, b"not_owner");
        // The redirect hint points at the real owner.
        assert_eq!(resp.owner_url.as_deref(), Some("ws://10.9.9.9:7700/ws"),);
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
        let req = TransportRequest::new(
            1,
            RequestKind::Disconnect { user: 1, tenant: 1 },
        );
        let resp = node.handle(req).await;
        assert_eq!(resp.seq, 1);
    }

    /// Regression: 39 writes from 13 distinct users must populate ≥ 30 of
    /// the 512 heat-map slots — the old `fnv1a_page(user, struct_id)` only
    /// hit ~13 slots because the routing key collapsed every write from
    /// one user onto the same slot.
    #[tokio::test]
    async fn writes_spread_across_heat_map_slots() {
        let node = QuickNode::new(cfg(100, 0, 4095));
        // 13 users × 3 writes each, mirroring the real_example layout.
        for user in 0..13u64 {
            for _ in 0..3 {
                let req = TransportRequest::new(
                    1,
                    RequestKind::Write {
                        struct_id: 7,
                        user,
                        tenant: 100,
                        payload: vec![0u8; 128],
                    },
                );
                node.handle(req).await;
            }
        }
        let lit = node
            .inner
            .page_bytes
            .iter()
            .filter(|p| p.load(Ordering::Relaxed) > 0)
            .count();
        // Old routing pinned all 3 writes from one user onto a single slot
        // → ≤ 13 distinct slots ever lit up.  The new routing varies the
        // synthetic Id by write_seq so every write lands somewhere new.
        assert!(lit >= 30, "expected ≥ 30 distinct slots, got {lit}");

        // The published metric must light the same slots: the map scales
        // relative to the busiest slot, so even 128-byte test writes are
        // visible (the old fixed 2 MiB threshold truncated them to 0).
        let metric_lit =
            node.metrics().page_map.iter().filter(|&&o| o > 0).count();
        assert_eq!(metric_lit, lit);
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
        let req = TransportRequest::new(
            1,
            RequestKind::Connect { user: 1, tenant: 1 },
        );
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

    // ── Schema enforcement (registry-attached writes) ────────────────────

    mod schema {
        use super::*;
        use wavedb::prelude::*;
        use wavedb_net::request::ErrorCode;

        fn validate_gadget(
            g: &Gadget1,
        ) -> Result<(), wavedb_core::ValidationError> {
            if g.qty == 0 {
                return Err(wavedb_core::ValidationError::field(
                    "qty",
                    "must be non-zero",
                ));
            }
            Ok(())
        }

        // Signature is the hook contract — infallible here, but the
        // attribute requires the Result shape.
        #[allow(clippy::unnecessary_wraps)]
        fn preprocess_gadget(
            g: &mut Gadget1,
        ) -> Result<(), wavedb_core::ValidationError> {
            g.code = g.code.trim().to_uppercase();
            Ok(())
        }

        #[wave_db(
            struct_id = 7100,
            NonUnique,
            validate = validate_gadget,
            preprocess = preprocess_gadget,
        )]
        pub struct Gadget1 {
            pub qty: u64,
            pub code: String,
        }

        declare_objects! {
            pub mod test_objects {
                gadgets: [Gadget1],
            }
        }

        fn gadget_payload(qty: u64, code: &str) -> Vec<u8> {
            let g = Gadget1 {
                id: wavedb_core::Id::default(),
                metadata: wavedb_core::Metadata::default(),
                qty,
                code: code.to_string(),
            };
            let body = wavedb_core::wire::to_wire(&g).expect("wire");
            let mut payload = Vec::with_capacity(1 + body.len());
            payload.push(Gadget1::STRUCT_VERSION);
            payload.extend_from_slice(&body);
            payload
        }

        fn write_req(
            seq: u64,
            struct_id: u32,
            payload: Vec<u8>,
        ) -> TransportRequest {
            TransportRequest::new(
                seq,
                RequestKind::Write {
                    struct_id,
                    user: 7,
                    tenant: 1,
                    payload,
                },
            )
        }

        fn registry_node() -> QuickNode {
            QuickNode::with_registry(cfg(1, 0, 4095), test_objects::REGISTRY)
        }

        #[tokio::test]
        async fn unknown_header_is_rejected() {
            let node = registry_node();
            let resp = node
                .handle(write_req(1, 9999, gadget_payload(5, "x")))
                .await;
            let err = resp.error.expect("undeclared struct must be rejected");
            assert_eq!(err.code, ErrorCode::UnknownHeader);
            // For UnknownHeader, struct_id carries the full header.
            assert_eq!(err.struct_id, wavedb_core::wire::pack_header(9999, 1));
            assert_eq!(node.metrics().write_count, 0);
            assert_eq!(node.metrics().rejected_count, 1);
        }

        #[tokio::test]
        async fn validate_hook_rejects_before_commit() {
            let node = registry_node();
            let resp = node
                .handle(write_req(1, 7100, gadget_payload(0, "x")))
                .await;
            let err = resp.error.expect("qty=0 must be rejected");
            assert_eq!(err.code, ErrorCode::ValidationFailed);
            assert_eq!(err.struct_id, 7100);
            assert_eq!(err.field.as_deref(), Some("qty"));
            assert_eq!(node.metrics().write_count, 0);
            assert_eq!(node.metrics().rejected_count, 1);
        }

        #[tokio::test]
        async fn malformed_payloads_are_rejected() {
            let node = registry_node();

            // Empty payload — no version byte at all.
            let resp = node.handle(write_req(1, 7100, Vec::new())).await;
            assert_eq!(
                resp.error.expect("empty payload").code,
                ErrorCode::MalformedPayload
            );

            // Declared header, garbage body.
            let resp =
                node.handle(write_req(2, 7100, vec![1, 0xDE, 0xAD])).await;
            assert_eq!(
                resp.error.expect("garbage body").code,
                ErrorCode::MalformedPayload
            );
            assert_eq!(node.metrics().write_count, 0);
        }

        #[tokio::test]
        #[allow(clippy::significant_drop_tightening)]
        async fn preprocess_transforms_the_committed_bytes() {
            // On-disk storage so the WAL journal records the commit.
            let dir = tempfile::tempdir().expect("tempdir");
            let mut config = cfg(1, 0, 4095);
            config.data_dir = dir.path().to_path_buf();
            let node = QuickNode::with_registry(config, test_objects::REGISTRY);

            let resp = node
                .handle(write_req(1, 7100, gadget_payload(3, "  ab12  ")))
                .await;
            assert!(resp.error.is_none(), "valid write must pass");
            assert_eq!(node.metrics().write_count, 1);

            // The journal entry must hold the *preprocessed* payload.
            // Guard scoped so the temporary drops promptly.
            let storage = node.storage().expect("on-disk storage");
            let stored: Gadget1 = {
                let journal = storage.journal.lock();
                let entries = journal.all_entries();
                assert_eq!(entries.len(), 1);
                let wavedb_storage::pipeline::journal::JournalEntry::WriteVersioned {
                    record,
                } = &entries[0]
                else {
                    panic!("expected WriteVersioned entry");
                };
                assert_eq!(record.data[0], Gadget1::STRUCT_VERSION);
                wavedb_core::wire::from_wire(&record.data[1..])
                    .expect("stored bytes decode")
            };
            assert_eq!(stored.code, "AB12", "preprocess result was committed");
            assert_eq!(stored.qty, 3);
        }

        #[tokio::test]
        async fn schema_blind_node_accepts_opaque_bytes() {
            // Legacy mode: no registry — anything goes (current behaviour
            // of the generic binary, unchanged).
            let node = QuickNode::new(cfg(1, 0, 4095));
            let resp = node.handle(write_req(1, 9999, vec![0xFF, 0xEE])).await;
            assert!(resp.error.is_none());
            assert_eq!(node.metrics().write_count, 1);
        }
    }

    // ── Read path (Phase 14: storage-backed search / query / delete) ─────

    mod read_path {
        use super::*;
        use wavedb::prelude::*;
        use wavedb_net::request::ErrorCode;

        #[wave_db(struct_id = 7300, NonUnique)]
        pub struct Widget1 {
            pub qty: u64,
            pub label: String,
        }

        #[wave_db(struct_id = 7301)]
        pub struct Profile1 {
            pub display_name: String,
        }

        declare_objects! {
            pub mod read_objects {
                widgets: [Widget1],
                profiles: [Profile1],
            }
        }

        fn storage_node() -> (QuickNode, tempfile::TempDir) {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut config = cfg(1, 0, 4095);
            config.data_dir = dir.path().to_path_buf();
            (QuickNode::new(config), dir)
        }

        fn registry_storage_node() -> (QuickNode, tempfile::TempDir) {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut config = cfg(1, 0, 4095);
            config.data_dir = dir.path().to_path_buf();
            (
                QuickNode::with_registry(config, read_objects::REGISTRY),
                dir,
            )
        }

        fn widget_payload(qty: u64, label: &str) -> Vec<u8> {
            let w = Widget1 {
                id: wavedb_core::Id::default(),
                metadata: wavedb_core::Metadata::default(),
                qty,
                label: label.to_string(),
            };
            let body = wavedb_core::wire::to_wire(&w).expect("wire");
            let mut payload = Vec::with_capacity(1 + body.len());
            payload.push(Widget1::STRUCT_VERSION);
            payload.extend_from_slice(&body);
            payload
        }

        fn profile_payload(display_name: &str) -> Vec<u8> {
            let p = Profile1 {
                id: wavedb_core::Id::default(),
                metadata: wavedb_core::Metadata::default(),
                display_name: display_name.to_string(),
            };
            let body = wavedb_core::wire::to_wire(&p).expect("wire");
            let mut payload = Vec::with_capacity(1 + body.len());
            payload.push(Profile1::STRUCT_VERSION);
            payload.extend_from_slice(&body);
            payload
        }

        async fn write(
            node: &QuickNode,
            seq: u64,
            struct_id: u32,
            payload: Vec<u8>,
        ) {
            let resp = node
                .handle(TransportRequest::new(
                    seq,
                    RequestKind::Write {
                        struct_id,
                        user: 7,
                        tenant: 1,
                        payload,
                    },
                ))
                .await;
            assert!(resp.error.is_none(), "write {seq} failed");
        }

        async fn query(
            node: &QuickNode,
            struct_id: u32,
            expr: &Expr,
        ) -> TransportResponse {
            node.handle(TransportRequest::new(
                90,
                RequestKind::QueryNonUnique {
                    struct_id,
                    user: 7,
                    tenant: 1,
                    filter: expr.to_bytes().expect("filter"),
                },
            ))
            .await
        }

        fn decode_entries(payload: &[u8]) -> Vec<(u8, Vec<u8>)> {
            wavedb_core::wire::from_wire(payload).expect("entries decode")
        }

        #[tokio::test]
        async fn schema_blind_write_then_query_all() {
            let (node, _dir) = storage_node();
            write(&node, 1, 9000, vec![1, 0xAA]).await;
            write(&node, 2, 9000, vec![1, 0xBB, 0xCC]).await;

            let resp = query(&node, 9000, &Expr::all()).await;
            assert!(resp.error.is_none());
            let entries = decode_entries(&resp.payload);
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0], (1, vec![0xAA]));
            assert_eq!(entries[1], (1, vec![0xBB, 0xCC]));
            assert_eq!(node.metrics().read_count, 1);
        }

        #[tokio::test]
        async fn search_unique_returns_latest_write() {
            let (node, _dir) = storage_node();
            write(&node, 1, 9001, vec![1, 0x01]).await;
            write(&node, 2, 9001, vec![2, 0x02]).await;

            let resp = node
                .handle(TransportRequest::new(
                    3,
                    RequestKind::SearchUnique {
                        struct_id: 9001,
                        user: 7,
                        tenant: 1,
                    },
                ))
                .await;
            assert!(resp.error.is_none());
            // Payload is the stored wire shape: [version][body], newest write.
            assert_eq!(resp.payload, vec![2, 0x02]);
        }

        #[tokio::test]
        async fn search_unique_empty_when_never_written() {
            let (node, _dir) = storage_node();
            let resp = node
                .handle(TransportRequest::new(
                    1,
                    RequestKind::SearchUnique {
                        struct_id: 9002,
                        user: 7,
                        tenant: 1,
                    },
                ))
                .await;
            assert!(resp.error.is_none());
            assert!(resp.payload.is_empty());
        }

        #[tokio::test]
        async fn delete_removes_record_from_query() {
            let (node, _dir) = storage_node();
            write(&node, 1, 9003, vec![1, 0xAA]).await;
            write(&node, 2, 9003, vec![1, 0xBB]).await;

            // Schema-blind clients can't read ids out of payloads; take it
            // from the tracker directly (unit-test access).
            let storage = node.storage().expect("storage").clone();
            let ids = storage.data_file.live_ids(9003, 1).unwrap();
            assert_eq!(ids.len(), 2);

            let resp = node
                .handle(TransportRequest::new(
                    3,
                    RequestKind::Delete {
                        id: ids[0],
                        user: 7,
                        tenant: 1,
                    },
                ))
                .await;
            assert!(resp.error.is_none());

            let resp = query(&node, 9003, &Expr::all()).await;
            let entries = decode_entries(&resp.payload);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].1, vec![0xBB]);
        }

        #[tokio::test]
        async fn delete_rejects_foreign_tenant_id() {
            let (node, _dir) = storage_node();
            // Id whose tenant bits (99) don't match the session tenant (1).
            let foreign = Id::new(99, 0, 9003, 5).raw();
            let resp = node
                .handle(TransportRequest::new(
                    1,
                    RequestKind::Delete {
                        id: foreign,
                        user: 7,
                        tenant: 1,
                    },
                ))
                .await;
            assert_eq!(
                resp.error.expect("must reject").code,
                ErrorCode::MalformedPayload
            );
        }

        #[tokio::test]
        async fn filtered_query_on_schema_blind_node_is_unsupported() {
            let (node, _dir) = storage_node();
            write(&node, 1, 9004, vec![1, 0xAA]).await;

            let resp = query(&node, 9004, &Expr::gt("qty", 10u64)).await;
            assert_eq!(
                resp.error.expect("must reject").code,
                ErrorCode::Unsupported
            );
        }

        #[tokio::test]
        async fn registry_filtered_query_matches_fields() {
            let (node, _dir) = registry_storage_node();
            write(&node, 1, 7300, widget_payload(5, "small")).await;
            write(&node, 2, 7300, widget_payload(50, "big")).await;
            write(&node, 3, 7300, widget_payload(500, "huge")).await;

            let resp = query(&node, 7300, &Expr::gt("qty", 10u64)).await;
            assert!(resp.error.is_none());
            let entries = decode_entries(&resp.payload);
            assert_eq!(entries.len(), 2);

            let decoded: Vec<Widget1> = entries
                .iter()
                .map(|(_, body)| {
                    wavedb_core::wire::from_wire(body).expect("widget")
                })
                .collect();
            assert_eq!(decoded[0].qty, 50);
            assert_eq!(decoded[1].qty, 500);

            // Compound filter over a heap (String) field.
            let expr =
                Expr::and(Expr::gt("qty", 10u64), Expr::eq("label", "huge"));
            let resp = query(&node, 7300, &expr).await;
            let entries = decode_entries(&resp.payload);
            assert_eq!(entries.len(), 1);
        }

        #[tokio::test]
        async fn registry_write_stamps_engine_id_into_body() {
            let (node, _dir) = registry_storage_node();
            write(&node, 1, 7300, widget_payload(5, "x")).await;

            let resp = query(&node, 7300, &Expr::all()).await;
            let entries = decode_entries(&resp.payload);
            let w: Widget1 =
                wavedb_core::wire::from_wire(&entries[0].1).unwrap();
            assert_ne!(w.id, wavedb_core::Id::ZERO, "id must be stamped");
            assert_eq!(w.id.tenant_id(), 1);
            assert_eq!(w.id.struct_id(), 7300);

            // The stamped id round-trips through Delete.
            let resp = node
                .handle(TransportRequest::new(
                    2,
                    RequestKind::Delete {
                        id: w.id.raw(),
                        user: 7,
                        tenant: 1,
                    },
                ))
                .await;
            assert!(resp.error.is_none());
            let resp = query(&node, 7300, &Expr::all()).await;
            assert!(decode_entries(&resp.payload).is_empty());
        }

        #[tokio::test]
        async fn unique_shape_keeps_single_live_entry() {
            let (node, _dir) = registry_storage_node();
            write(&node, 1, 7301, profile_payload("first")).await;
            write(&node, 2, 7301, profile_payload("second")).await;

            // The Unique tracker rotates instead of growing.
            let storage = node.storage().expect("storage").clone();
            assert_eq!(storage.data_file.live_ids(7301, 1).unwrap().len(), 1);

            let resp = node
                .handle(TransportRequest::new(
                    3,
                    RequestKind::SearchUnique {
                        struct_id: 7301,
                        user: 7,
                        tenant: 1,
                    },
                ))
                .await;
            let (&version, body) = resp.payload.split_first().expect("payload");
            assert_eq!(version, Profile1::STRUCT_VERSION);
            let p: Profile1 = wavedb_core::wire::from_wire(body).unwrap();
            assert_eq!(p.display_name, "second");
        }

        #[tokio::test]
        async fn in_memory_node_reads_stay_empty() {
            // No data_dir → no storage → reads answer empty, never error.
            let node = QuickNode::new(cfg(1, 0, 4095));
            write(&node, 1, 9005, vec![1, 0xAA]).await;

            let resp = node
                .handle(TransportRequest::new(
                    2,
                    RequestKind::SearchUnique {
                        struct_id: 9005,
                        user: 7,
                        tenant: 1,
                    },
                ))
                .await;
            assert!(resp.error.is_none());
            assert!(resp.payload.is_empty());

            let resp = query(&node, 9005, &Expr::all()).await;
            assert!(resp.error.is_none());
            assert!(resp.payload.is_empty());
        }

        #[tokio::test]
        #[allow(clippy::significant_drop_tightening)]
        async fn sync_to_slow_flushes_pending_records() {
            use wavedb_net::frame::decode_payload;

            // Stub Slow-Node: accepts /flush, records batches.
            let received = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<
                FlushBatch,
            >::new(
            )));
            let state = received.clone();
            let app = axum::Router::new().route(
                "/flush",
                axum::routing::post(move |body: bytes::Bytes| {
                    let state = state.clone();
                    async move {
                        let batch: FlushBatch =
                            decode_payload(&body).expect("batch");
                        let ack = wavedb_slow_node::flush::FlushAck {
                            write_seq: batch.write_seq,
                        };
                        state.lock().push(batch);
                        wavedb_net::frame::encode_payload(&ack)
                            .unwrap()
                            .to_vec()
                    }
                }),
            );
            let listener =
                tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let slow_addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let dir = tempfile::tempdir().expect("tempdir");
            let mut config = cfg(1, 0, 4095);
            config.data_dir = dir.path().to_path_buf();
            config.slow_node = Some(slow_addr.to_string());
            let node = QuickNode::new(config);

            write(&node, 1, 9006, vec![1, 0xAA]).await;
            write(&node, 2, 9006, vec![1, 0xBB]).await;

            node.sync_to_slow().await;

            {
                let batches = received.lock();
                assert_eq!(batches.len(), 1, "one tenant → one batch");
                assert_eq!(batches[0].tenant, 1);
                assert_eq!(batches[0].records.len(), 2);
                assert_eq!(batches[0].records[0].data, vec![1, 0xAA]);
            }

            // Pending buffer drained — a second sync sends nothing.
            node.sync_to_slow().await;
            assert_eq!(received.lock().len(), 1);
        }
    }
}
