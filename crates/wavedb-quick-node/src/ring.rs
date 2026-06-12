//! Consistent hash ring for Quick-Node partition routing.
//!
//! Each physical node is placed at [`VIRTUAL_NODES`] positions on a
//! `u64`-keyed ring.  A `(tenant_id, shard_id)` pair is mapped to the first
//! node clockwise from its FNV-1a hash.  This gives a roughly uniform
//! distribution and — crucially — minimal reassignment when a node is added
//! or removed.

use std::collections::{BTreeMap, HashMap};

/// A stable node identifier.  Derived from the node's socket address via
/// [`crate::node::addr_to_node_id`].
pub type NodeId = u64;

/// Virtual nodes per physical node.  Higher values improve uniformity at the
/// cost of ring-map memory.
const VIRTUAL_NODES: u32 = 150;

/// A consistent hash ring mapping `(tenant_id, shard_id)` partitions to nodes.
#[derive(Debug, Default)]
pub struct ConsistentRing {
    /// Ring positions → node ID.  One physical node occupies `VIRTUAL_NODES` positions.
    ring: BTreeMap<u64, NodeId>,
    /// Physical node ID → socket-address string.
    addrs: HashMap<NodeId, String>,
}

impl ConsistentRing {
    /// Create an empty ring.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `id` at address `addr` to the ring.
    ///
    /// If the node already exists its address is updated and virtual nodes
    /// are refreshed.
    pub fn add_node(&mut self, id: NodeId, addr: impl Into<String>) {
        self.addrs.insert(id, addr.into());
        for vn in 0..VIRTUAL_NODES {
            let pos = ring_hash(id, vn);
            self.ring.insert(pos, id);
        }
    }

    /// Remove `id` from the ring.
    ///
    /// Returns `true` if the node was present, `false` otherwise.
    pub fn remove_node(&mut self, id: NodeId) -> bool {
        if self.addrs.remove(&id).is_none() {
            return false;
        }
        for vn in 0..VIRTUAL_NODES {
            let pos = ring_hash(id, vn);
            self.ring.remove(&pos);
        }
        true
    }

    /// Return the [`NodeId`] responsible for the given `(tenant, shard)` partition.
    ///
    /// Returns `None` when the ring is empty.
    pub fn owner_of(&self, tenant: u64, shard: u16) -> Option<NodeId> {
        if self.ring.is_empty() {
            return None;
        }
        let key = partition_hash(tenant, shard);
        // Walk clockwise; wrap at the end of the ring.
        self.ring
            .range(key..)
            .next()
            .or_else(|| self.ring.iter().next())
            .map(|(_, &id)| id)
    }

    /// Return the replica set for `(tenant, shard)`: the owner followed by
    /// the next distinct physical nodes clockwise on the ring, up to `n`
    /// nodes total.
    ///
    /// Index 0 is always the owner ([`Self::owner_of`]).  With fewer than
    /// `n` physical nodes the set is simply every node, owner first — a
    /// solo node returns `[self]`.  This is the placement rule behind
    /// `MIN_REPLICAS`: one writer, the rest hold copies for redundancy.
    pub fn replicas_of(
        &self,
        tenant: u64,
        shard: u16,
        n: usize,
    ) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = Vec::with_capacity(n.min(self.addrs.len()));
        if n == 0 || self.ring.is_empty() {
            return out;
        }
        let key = partition_hash(tenant, shard);
        // Clockwise walk starting at the partition key, wrapping once.
        for (_, &id) in self.ring.range(key..).chain(self.ring.iter()) {
            if !out.contains(&id) {
                out.push(id);
                if out.len() == n || out.len() == self.addrs.len() {
                    break;
                }
            }
        }
        out
    }

    /// Return the socket address of `id`, if known.
    pub fn addr_of(&self, id: NodeId) -> Option<&str> {
        self.addrs.get(&id).map(String::as_str)
    }

    /// Number of distinct physical nodes in the ring.
    pub fn node_count(&self) -> usize {
        self.addrs.len()
    }

    /// Returns `true` when the ring contains no nodes.
    pub fn is_empty(&self) -> bool {
        self.addrs.is_empty()
    }

    /// Iterator over all physical node IDs currently in the ring.
    pub fn nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.addrs.keys().copied()
    }
}

/// Deterministic hash that places virtual node `vn` for `node_id` on the ring.
fn ring_hash(node_id: NodeId, vn: u32) -> u64 {
    let mut h = FNV_OFFSET;
    for b in node_id.to_le_bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
    }
    for b in vn.to_le_bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
    }
    h
}

/// Deterministic hash for a `(tenant, shard)` partition lookup key.
pub fn partition_hash(tenant: u64, shard: u16) -> u64 {
    let mut h = FNV_OFFSET;
    for b in tenant.to_le_bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
    }
    for b in shard.to_le_bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
    }
    h
}

// FNV-1a constants (64-bit).
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_ring_returns_none() {
        let ring = ConsistentRing::new();
        assert!(ring.owner_of(1, 0).is_none());
    }

    #[test]
    fn single_node_owns_all_partitions() {
        let mut ring = ConsistentRing::new();
        ring.add_node(1, "127.0.0.1:7700");
        for shard in 0..16u16 {
            assert_eq!(ring.owner_of(42, shard), Some(1));
        }
    }

    #[test]
    fn removing_only_node_empties_ring() {
        let mut ring = ConsistentRing::new();
        ring.add_node(1, "127.0.0.1:7700");
        assert!(ring.remove_node(1));
        assert!(ring.owner_of(42, 0).is_none());
    }

    #[test]
    fn removing_one_of_two_redirects_all_to_survivor() {
        let mut ring = ConsistentRing::new();
        ring.add_node(1, "node1");
        ring.add_node(2, "node2");
        assert!(ring.remove_node(1));
        for shard in 0..32u16 {
            assert_eq!(ring.owner_of(42, shard), Some(2));
        }
    }

    #[test]
    fn remove_unknown_node_returns_false() {
        let mut ring = ConsistentRing::new();
        assert!(!ring.remove_node(999));
    }

    #[test]
    fn two_nodes_both_own_some_partitions() {
        // Use address-derived IDs (same as the production path) so the hash
        // inputs are spread across the u64 range and virtual nodes don't cluster.
        let id1 = fnv_addr("10.0.0.1:7700");
        let id2 = fnv_addr("10.0.0.2:7700");

        let mut ring = ConsistentRing::new();
        ring.add_node(id1, "node1");
        ring.add_node(id2, "node2");

        let mut count1 = 0u32;
        let mut count2 = 0u32;
        for tenant in 0..8u64 {
            for shard in 0..128u16 {
                let owner = ring
                    .owner_of(tenant, shard)
                    .expect("ring must have an owner");
                if owner == id1 {
                    count1 += 1;
                } else if owner == id2 {
                    count2 += 1;
                } else {
                    panic!("unexpected owner: {owner}");
                }
            }
        }
        assert!(count1 > 0, "node 1 owns nothing");
        assert!(count2 > 0, "node 2 owns nothing");
    }

    fn fnv_addr(addr: &str) -> NodeId {
        let mut h: u64 = FNV_OFFSET;
        for b in addr.bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
        }
        h
    }

    #[test]
    fn replicas_solo_node_is_just_the_owner() {
        let mut ring = ConsistentRing::new();
        ring.add_node(1, "a");
        assert_eq!(ring.replicas_of(42, 0, 3), vec![1]);
    }

    #[test]
    fn replicas_owner_first_then_distinct_successor() {
        let id1 = fnv_addr("10.0.0.1:7700");
        let id2 = fnv_addr("10.0.0.2:7700");
        let mut ring = ConsistentRing::new();
        ring.add_node(id1, "node1");
        ring.add_node(id2, "node2");

        for tenant in 0..8u64 {
            let owner = ring.owner_of(tenant, 0).unwrap();
            let replicas = ring.replicas_of(tenant, 0, 2);
            assert_eq!(replicas.len(), 2);
            assert_eq!(replicas[0], owner, "owner must lead the replica set");
            assert_ne!(replicas[0], replicas[1], "replicas must be distinct");
        }
    }

    #[test]
    fn replicas_n_one_equals_owner_of() {
        let id1 = fnv_addr("10.0.0.1:7700");
        let id2 = fnv_addr("10.0.0.2:7700");
        let mut ring = ConsistentRing::new();
        ring.add_node(id1, "node1");
        ring.add_node(id2, "node2");
        assert_eq!(
            ring.replicas_of(7, 0, 1),
            vec![ring.owner_of(7, 0).unwrap()]
        );
        assert!(ring.replicas_of(7, 0, 0).is_empty());
    }

    #[test]
    fn partition_hash_is_deterministic() {
        assert_eq!(partition_hash(42, 7), partition_hash(42, 7));
        assert_ne!(partition_hash(42, 7), partition_hash(42, 8));
        assert_ne!(partition_hash(42, 7), partition_hash(43, 7));
    }

    #[test]
    fn addr_of_returns_correct_string() {
        let mut ring = ConsistentRing::new();
        ring.add_node(10, "10.0.0.1:7700");
        assert_eq!(ring.addr_of(10), Some("10.0.0.1:7700"));
        assert_eq!(ring.addr_of(99), None);
    }

    #[test]
    fn node_count_tracks_adds_and_removes() {
        let mut ring = ConsistentRing::new();
        assert_eq!(ring.node_count(), 0);
        ring.add_node(1, "a");
        ring.add_node(2, "b");
        assert_eq!(ring.node_count(), 2);
        ring.remove_node(1);
        assert_eq!(ring.node_count(), 1);
    }

    #[test]
    fn add_existing_node_updates_addr() {
        let mut ring = ConsistentRing::new();
        ring.add_node(1, "old-addr");
        ring.add_node(1, "new-addr");
        assert_eq!(ring.node_count(), 1);
        assert_eq!(ring.addr_of(1), Some("new-addr"));
    }
}
