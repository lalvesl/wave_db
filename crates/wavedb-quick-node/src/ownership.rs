//! Partition ownership and two-phase transfer protocol.
//!
//! Each Quick-Node claims a set of `(tenant_id, shard_range)` partitions via
//! [`OwnershipMap`].  When the cluster rebalances, partitions move between
//! nodes via the handshake tracked by [`TransferTracker`]:
//!
//! 1. **Pending** — receiver sends a transfer request to the current owner.
//! 2. **Accepted** — owner agrees; data sync is in progress.
//! 3. **Complete** — receiver confirms it's fully caught up; owner yields.
//! 4. **Rejected** — owner refuses (e.g. write backlog is too deep).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;

use crate::ring::NodeId;

// ── ShardRange ────────────────────────────────────────────────────────────────

/// An inclusive shard-ID range `[start, end]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShardRange {
    /// First shard in the range (inclusive).
    pub start: u16,
    /// Last shard in the range (inclusive).
    pub end: u16,
}

impl ShardRange {
    /// Create a range `[start, end]` (both inclusive).
    pub const fn new(start: u16, end: u16) -> Self {
        Self { start, end }
    }

    /// Returns `true` if `shard` falls within `[start, end]`.
    pub const fn contains(self, shard: u16) -> bool {
        self.start <= shard && shard <= self.end
    }

    /// Number of shards in the range (0 when `start > end`).
    pub const fn len(self) -> u32 {
        if self.start > self.end {
            0
        } else {
            (self.end - self.start) as u32 + 1
        }
    }

    /// `true` when no shards are represented (`start > end`).
    pub const fn is_empty(self) -> bool {
        self.start > self.end
    }
}

// ── PartitionOwnership ────────────────────────────────────────────────────────

/// A single ownership entry: this node owns `range` for `tenant`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PartitionOwnership {
    /// The tenant whose data this node is responsible for.
    pub tenant: u64,
    /// The shard sub-range owned for this tenant.
    pub range: ShardRange,
}

// ── OwnershipMap ──────────────────────────────────────────────────────────────

/// Thread-safe registry of partitions currently owned by this Quick-Node.
#[derive(Debug, Clone, Default)]
pub struct OwnershipMap {
    inner: Arc<RwLock<Vec<PartitionOwnership>>>,
}

impl OwnershipMap {
    /// Create an empty ownership map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim ownership of `range` for `tenant`.
    pub fn add(&self, tenant: u64, range: ShardRange) {
        self.inner
            .write()
            .push(PartitionOwnership { tenant, range });
    }

    /// Release ownership of the exact `(tenant, range)` entry.
    ///
    /// Returns `true` if the entry was present and removed.
    pub fn remove(&self, tenant: u64, range: ShardRange) -> bool {
        let target = PartitionOwnership { tenant, range };
        let mut guard = self.inner.write();
        guard.iter().position(|p| p == &target).is_some_and(|pos| {
            guard.swap_remove(pos);
            true
        })
    }

    /// Returns `true` if this node owns `(tenant, shard)`.
    pub fn owns(&self, tenant: u64, shard: u16) -> bool {
        self.inner
            .read()
            .iter()
            .any(|p| p.tenant == tenant && p.range.contains(shard))
    }

    /// Snapshot of all owned partitions.
    pub fn all(&self) -> Vec<PartitionOwnership> {
        self.inner.read().clone()
    }

    /// Number of ownership entries.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// `true` when no partitions are owned.
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

// ── Transfer protocol ─────────────────────────────────────────────────────────

/// State of an in-progress partition ownership transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferState {
    /// Receiver sent a request; owner has not responded yet.
    Pending,
    /// Owner agreed; data sync is in progress.
    Accepted,
    /// Receiver confirmed it is fully caught up; transfer is done.
    Complete,
    /// Owner refused (e.g. it is mid-write on the partition).
    Rejected,
}

/// A record of one in-flight ownership transfer.
#[derive(Debug, Clone)]
pub struct TransferRecord {
    /// The partition being transferred.
    pub ownership: PartitionOwnership,
    /// Node yielding the partition.
    pub from_node: NodeId,
    /// Node acquiring the partition.
    pub to_node: NodeId,
    /// Current handshake state.
    pub state: TransferState,
}

/// Tracks all in-flight ownership transfers for this Quick-Node.
#[derive(Debug, Default)]
pub struct TransferTracker {
    transfers: RwLock<HashMap<u64, TransferRecord>>,
    next_id: AtomicU64,
}

impl TransferTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new transfer and return its monotonic ID.
    pub fn begin(&self, ownership: PartitionOwnership, from_node: NodeId, to_node: NodeId) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.transfers.write().insert(
            id,
            TransferRecord {
                ownership,
                from_node,
                to_node,
                state: TransferState::Pending,
            },
        );
        id
    }

    /// Advance transfer `id` to `state`.
    ///
    /// Returns `false` when `id` is unknown.
    pub fn advance(&self, id: u64, state: TransferState) -> bool {
        let mut guard = self.transfers.write();
        if let Some(rec) = guard.get_mut(&id) {
            rec.state = state;
            true
        } else {
            false
        }
    }

    /// Current state of transfer `id`, or `None` when unknown.
    pub fn state_of(&self, id: u64) -> Option<TransferState> {
        self.transfers.read().get(&id).map(|r| r.state)
    }

    /// Remove a finished (complete or rejected) transfer from the tracker.
    ///
    /// Returns the removed record, or `None` when not found.
    pub fn finish(&self, id: u64) -> Option<TransferRecord> {
        self.transfers.write().remove(&id)
    }

    /// Snapshot of all active (not yet finished) transfers.
    pub fn active(&self) -> Vec<(u64, TransferRecord)> {
        self.transfers
            .read()
            .iter()
            .map(|(&id, r)| (id, r.clone()))
            .collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ShardRange ──────────────────────────────────────────────────────────

    #[test]
    fn shard_range_contains() {
        let r = ShardRange::new(10, 20);
        assert!(r.contains(10));
        assert!(r.contains(15));
        assert!(r.contains(20));
        assert!(!r.contains(9));
        assert!(!r.contains(21));
    }

    #[test]
    fn shard_range_len() {
        assert_eq!(ShardRange::new(0, 0).len(), 1);
        assert_eq!(ShardRange::new(0, 9).len(), 10);
        assert_eq!(ShardRange::new(100, 199).len(), 100);
    }

    #[test]
    fn shard_range_empty() {
        assert!(ShardRange::new(5, 4).is_empty());
        assert_eq!(ShardRange::new(5, 4).len(), 0);
        assert!(!ShardRange::new(0, 0).is_empty());
    }

    // ── OwnershipMap ────────────────────────────────────────────────────────

    #[test]
    fn add_and_owns() {
        let map = OwnershipMap::new();
        map.add(42, ShardRange::new(0, 511));
        assert!(map.owns(42, 0));
        assert!(map.owns(42, 511));
        assert!(!map.owns(42, 512));
        assert!(!map.owns(99, 0));
    }

    #[test]
    fn remove_ownership() {
        let map = OwnershipMap::new();
        map.add(1, ShardRange::new(0, 100));
        assert!(map.owns(1, 50));
        assert!(map.remove(1, ShardRange::new(0, 100)));
        assert!(!map.owns(1, 50));
        assert!(!map.remove(1, ShardRange::new(0, 100)));
    }

    #[test]
    fn multiple_ranges_per_tenant() {
        let map = OwnershipMap::new();
        map.add(5, ShardRange::new(0, 99));
        map.add(5, ShardRange::new(200, 299));
        assert!(map.owns(5, 50));
        assert!(map.owns(5, 250));
        assert!(!map.owns(5, 150));
    }

    #[test]
    fn is_empty_and_len() {
        let map = OwnershipMap::new();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
        map.add(1, ShardRange::new(0, 0));
        assert!(!map.is_empty());
        assert_eq!(map.len(), 1);
    }

    // ── TransferTracker ─────────────────────────────────────────────────────

    #[test]
    fn transfer_lifecycle() {
        let tracker = TransferTracker::new();
        let p = PartitionOwnership {
            tenant: 42,
            range: ShardRange::new(0, 511),
        };

        let id = tracker.begin(p.clone(), 1, 2);
        assert_eq!(tracker.state_of(id), Some(TransferState::Pending));

        assert!(tracker.advance(id, TransferState::Accepted));
        assert_eq!(tracker.state_of(id), Some(TransferState::Accepted));

        assert!(tracker.advance(id, TransferState::Complete));
        let record = tracker.finish(id).unwrap();
        assert_eq!(record.state, TransferState::Complete);
        assert_eq!(record.ownership, p);

        assert!(tracker.state_of(id).is_none());
    }

    #[test]
    fn advance_unknown_transfer_returns_false() {
        let tracker = TransferTracker::new();
        assert!(!tracker.advance(999, TransferState::Accepted));
    }

    #[test]
    fn rejected_transfer() {
        let tracker = TransferTracker::new();
        let id = tracker.begin(
            PartitionOwnership {
                tenant: 1,
                range: ShardRange::new(0, 0),
            },
            10,
            20,
        );
        tracker.advance(id, TransferState::Rejected);
        assert_eq!(tracker.state_of(id), Some(TransferState::Rejected));
    }

    #[test]
    fn multiple_concurrent_transfers() {
        let tracker = TransferTracker::new();
        let range = ShardRange::new(0, 63);
        let ids: Vec<u64> = (0..5u64)
            .map(|i| tracker.begin(PartitionOwnership { tenant: i, range }, i, i + 10))
            .collect();

        assert_eq!(tracker.active().len(), 5);

        for id in ids {
            tracker.advance(id, TransferState::Complete);
            tracker.finish(id);
        }
        assert!(tracker.active().is_empty());
    }
}
