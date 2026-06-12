//! Replication: owner → replica write fan-out and watermark tracking.
//!
//! After the ring-designated **owner** commits a write, it pushes the
//! committed bytes to the next [`crate::node::MIN_REPLICAS`] minus one distinct
//! nodes clockwise on the ring (`POST /replicate`).  Replicas store the
//! owner's canonical bytes for redundancy but never accept client writes
//! for that partition — one writer, n copies.
//!
//! The watermark records — per peer — the highest write sequence number
//! that peer has acknowledged.  This is used to:
//!
//! - Determine which writes are considered durably replicated.
//! - Drive catch-up replication when a peer reconnects after a gap.
//! - Gate the write pipeline under [`ReplicationWatermark::is_fully_replicated`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use wavedb_macros::WaveWire;
use wavedb_net::auth::NodeToken;

use crate::ring::NodeId;

// ── Wire types for the /replicate endpoint ────────────────────────────────────

/// One committed record pushed from the owner to a replica.
#[derive(Debug, Clone, WaveWire)]
pub struct ReplicaRecord {
    /// The record's raw 128-bit ID.
    pub id: u128,
    /// The exact committed payload (`[version u8][wire body]`) — already
    /// validated and preprocessed on the owner, stored verbatim here.
    pub data: Vec<u8>,
}

/// Body of a `POST /replicate` request.
#[derive(Debug, Clone, WaveWire)]
pub struct ReplicateBatch {
    /// Sender (owner) node ID, for ack bookkeeping on the receiver's logs.
    pub origin: NodeId,
    /// The owner's local write sequence covered by this batch.
    pub write_seq: u64,
    /// Records to store.
    pub records: Vec<ReplicaRecord>,
    /// Cluster membership proof (required when a cluster key is configured).
    pub token: Option<NodeToken>,
}

/// Body of a `POST /replicate` response.
#[derive(Debug, Clone, WaveWire)]
pub struct ReplicateAck {
    /// Echo of [`ReplicateBatch::write_seq`] — feeds
    /// [`ReplicationWatermark::record_ack`] on the owner.
    pub write_seq: u64,
}

// ── Per-peer state ────────────────────────────────────────────────────────────

#[derive(Debug)]
struct PeerState {
    /// Highest sequence number the peer has acked.
    acked: AtomicU64,
    /// Highest sequence number sent to the peer.
    sent: AtomicU64,
}

impl PeerState {
    const fn new() -> Self {
        Self {
            acked: AtomicU64::new(0),
            sent: AtomicU64::new(0),
        }
    }

    fn acked(&self) -> u64 {
        self.acked.load(Ordering::Acquire)
    }

    fn sent(&self) -> u64 {
        self.sent.load(Ordering::Acquire)
    }

    fn record_send(&self, seq: u64) {
        self.sent.fetch_max(seq, Ordering::AcqRel);
    }

    fn record_ack(&self, seq: u64) {
        // Monotonic: an older ack must not regress the watermark.
        self.acked.fetch_max(seq, Ordering::AcqRel);
    }

    fn lag(&self) -> u64 {
        self.sent().saturating_sub(self.acked())
    }
}

// ── ReplicationWatermark ──────────────────────────────────────────────────────

/// Thread-safe per-peer replication watermark table.
///
/// Register peers with [`add_peer`](Self::add_peer).  As the write pipeline
/// dispatches and receives acknowledgements, call
/// [`record_send`](Self::record_send) and [`record_ack`](Self::record_ack).
/// The watermark is then queryable via [`acked_by`](Self::acked_by),
/// [`lag_for`](Self::lag_for), and [`is_fully_replicated`](Self::is_fully_replicated).
#[derive(Debug, Clone, Default)]
pub struct ReplicationWatermark {
    peers: Arc<RwLock<HashMap<NodeId, Arc<PeerState>>>>,
    /// Monotonically increasing local write sequence counter.
    local_seq: Arc<AtomicU64>,
}

impl ReplicationWatermark {
    /// Create an empty watermark table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a peer node.  A no-op if the peer is already registered.
    pub fn add_peer(&self, peer: NodeId) {
        self.peers
            .write()
            .entry(peer)
            .or_insert_with(|| Arc::new(PeerState::new()));
    }

    /// Deregister a peer node.
    pub fn remove_peer(&self, peer: NodeId) {
        self.peers.write().remove(&peer);
    }

    /// Mint the next local write sequence number (1-based, monotonically increasing).
    pub fn next_seq(&self) -> u64 {
        self.local_seq.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Current local write sequence (0 when no writes have been issued yet).
    pub fn current_seq(&self) -> u64 {
        self.local_seq.load(Ordering::Acquire)
    }

    /// Record that `seq` has been dispatched to `peer`.
    pub fn record_send(&self, peer: NodeId, seq: u64) {
        let state = self.peers.read().get(&peer).cloned();
        if let Some(state) = state {
            state.record_send(seq);
        }
    }

    /// Record that `peer` has acknowledged writes up to `seq`.
    pub fn record_ack(&self, peer: NodeId, seq: u64) {
        let state = self.peers.read().get(&peer).cloned();
        if let Some(state) = state {
            state.record_ack(seq);
        }
    }

    /// Highest sequence number acknowledged by `peer`, or `None` if unknown.
    pub fn acked_by(&self, peer: NodeId) -> Option<u64> {
        self.peers.read().get(&peer).map(|s| s.acked())
    }

    /// Number of un-acked writes outstanding for `peer`, or `None` if unknown.
    pub fn lag_for(&self, peer: NodeId) -> Option<u64> {
        self.peers.read().get(&peer).map(|s| s.lag())
    }

    /// Returns `true` when all registered peers have acked up to at least `seq`.
    ///
    /// An empty peer table is treated as "fully replicated" (single-node mode).
    pub fn is_fully_replicated(&self, seq: u64) -> bool {
        let peers = self.peers.read();
        if peers.is_empty() {
            return true;
        }
        peers.values().all(|s| s.acked() >= seq)
    }

    /// Returns `true` when all peers have acked the current local sequence.
    pub fn is_caught_up(&self) -> bool {
        self.is_fully_replicated(self.current_seq())
    }

    /// Snapshot of all peers and their acknowledged watermarks.
    pub fn snapshot(&self) -> Vec<(NodeId, u64)> {
        let mut v: Vec<_> = self
            .peers
            .read()
            .iter()
            .map(|(&id, s)| (id, s.acked()))
            .collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_peer_and_basic_ack() {
        let wm = ReplicationWatermark::new();
        wm.add_peer(1);
        assert_eq!(wm.acked_by(1), Some(0));
        wm.record_send(1, 5);
        wm.record_ack(1, 5);
        assert_eq!(wm.acked_by(1), Some(5));
    }

    #[test]
    fn acked_by_unknown_peer_returns_none() {
        let wm = ReplicationWatermark::new();
        assert!(wm.acked_by(999).is_none());
    }

    #[test]
    fn watermark_is_monotonic() {
        let wm = ReplicationWatermark::new();
        wm.add_peer(2);
        wm.record_ack(2, 10);
        wm.record_ack(2, 5); // older ack must not regress
        assert_eq!(wm.acked_by(2), Some(10));
    }

    #[test]
    fn is_fully_replicated_all_peers_acked() {
        let wm = ReplicationWatermark::new();
        wm.add_peer(1);
        wm.add_peer(2);
        wm.record_ack(1, 7);
        wm.record_ack(2, 7);
        assert!(wm.is_fully_replicated(7));
        assert!(!wm.is_fully_replicated(8));
    }

    #[test]
    fn is_fully_replicated_one_peer_behind() {
        let wm = ReplicationWatermark::new();
        wm.add_peer(1);
        wm.add_peer(2);
        wm.record_ack(1, 10);
        wm.record_ack(2, 9);
        assert!(!wm.is_fully_replicated(10));
        assert!(wm.is_fully_replicated(9));
    }

    #[test]
    fn no_peers_is_always_fully_replicated() {
        let wm = ReplicationWatermark::new();
        assert!(wm.is_fully_replicated(u64::MAX));
    }

    #[test]
    fn next_seq_is_monotonically_increasing() {
        let wm = ReplicationWatermark::new();
        let a = wm.next_seq();
        let b = wm.next_seq();
        let c = wm.next_seq();
        assert!(a < b && b < c);
    }

    #[test]
    fn lag_tracking() {
        let wm = ReplicationWatermark::new();
        wm.add_peer(3);
        wm.record_send(3, 10);
        wm.record_ack(3, 6);
        assert_eq!(wm.lag_for(3), Some(4));
    }

    #[test]
    fn remove_peer() {
        let wm = ReplicationWatermark::new();
        wm.add_peer(1);
        wm.add_peer(2);
        wm.remove_peer(1);
        assert!(wm.acked_by(1).is_none());
        assert!(wm.acked_by(2).is_some());
    }

    #[test]
    fn snapshot_all_peers_sorted() {
        let wm = ReplicationWatermark::new();
        wm.add_peer(20);
        wm.add_peer(10);
        wm.record_ack(10, 3);
        wm.record_ack(20, 5);
        assert_eq!(wm.snapshot(), [(10, 3), (20, 5)]);
    }

    #[test]
    fn is_caught_up_no_writes() {
        let wm = ReplicationWatermark::new();
        wm.add_peer(1);
        // No writes issued yet, seq=0; peer acked=0.
        assert!(wm.is_caught_up());
    }

    #[test]
    fn is_caught_up_after_write_and_ack() {
        let wm = ReplicationWatermark::new();
        wm.add_peer(1);
        let seq = wm.next_seq();
        assert!(!wm.is_caught_up()); // peer hasn't acked yet
        wm.record_ack(1, seq);
        assert!(wm.is_caught_up());
    }
}
