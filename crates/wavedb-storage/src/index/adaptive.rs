//! Adaptive index: starts as an array and atomically converts to a B+ tree.
//!
//! The conversion happens at `threshold + 1` items and is one-way —
//! there is no fallback path from tree back to array.

use super::array::ArrayIndex;
use super::btree::BTreeIndex;
use super::{DEFAULT_MAX_NON_UNIQUE_ELEMENTS, IndexBackend, IndexKey};
use crate::anchor::AnchorKey;
use std::ops::Range;

/// Internal state of the adaptive index.
#[derive(Debug, Clone)]
enum IndexState {
    /// Small collection — linear scan.
    Array(ArrayIndex),
    /// Large collection — B+ tree.
    BTree(BTreeIndex),
}

/// An index that starts as a sorted array and promotes to a B+ tree
/// once the entry count exceeds the configured threshold.
#[derive(Debug, Clone)]
pub struct AdaptiveIndex {
    state: IndexState,
    threshold: u32,
    /// Set to `true` once the array→tree conversion has occurred.
    converted: bool,
}

impl AdaptiveIndex {
    /// Create a new adaptive index with the default threshold.
    pub const fn new() -> Self {
        Self {
            state: IndexState::Array(ArrayIndex::new()),
            threshold: DEFAULT_MAX_NON_UNIQUE_ELEMENTS,
            converted: false,
        }
    }

    /// Create with a custom threshold (e.g. from `btree_threshold = K`).
    pub const fn with_threshold(threshold: u32) -> Self {
        Self {
            state: IndexState::Array(ArrayIndex::new()),
            threshold,
            converted: false,
        }
    }

    /// Whether the index has been promoted to a B+ tree.
    pub const fn is_tree(&self) -> bool {
        self.converted
    }

    /// The configured array→tree conversion threshold.
    pub const fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Promote from array to B+ tree (one-way, journaled in production).
    fn promote(&mut self) {
        if let IndexState::Array(array) = std::mem::replace(
            &mut self.state,
            IndexState::Array(ArrayIndex::new()), // placeholder
        ) {
            let entries = array.into_entries();
            self.state = IndexState::BTree(BTreeIndex::from_sorted(&entries));
            self.converted = true;
        }
    }
}

impl Default for AdaptiveIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexBackend for AdaptiveIndex {
    fn insert(
        &mut self,
        key: IndexKey,
        anchor: AnchorKey,
    ) -> crate::StorageResult<()> {
        // Check if we need to promote
        if !self.converted && self.len() >= self.threshold as usize {
            self.promote();
        }

        match &mut self.state {
            IndexState::Array(arr) => arr.insert(key, anchor),
            IndexState::BTree(tree) => tree.insert(key, anchor),
        }
    }

    fn lookup(&self, key: &IndexKey) -> Option<AnchorKey> {
        match &self.state {
            IndexState::Array(arr) => arr.lookup(key),
            IndexState::BTree(tree) => tree.lookup(key),
        }
    }

    fn remove(&mut self, key: &IndexKey) -> bool {
        match &mut self.state {
            IndexState::Array(arr) => arr.remove(key),
            IndexState::BTree(tree) => tree.remove(key),
        }
    }

    fn range(&self, range: Range<IndexKey>) -> Vec<(IndexKey, AnchorKey)> {
        match &self.state {
            IndexState::Array(arr) => arr.range(range),
            IndexState::BTree(tree) => tree.range(range),
        }
    }

    fn len(&self) -> usize {
        match &self.state {
            IndexState::Array(arr) => arr.len(),
            IndexState::BTree(tree) => tree.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_as_array() {
        let idx = AdaptiveIndex::new();
        assert!(!idx.is_tree());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn converts_at_threshold_plus_one() {
        let mut idx = AdaptiveIndex::with_threshold(5);
        for i in 0..5 {
            idx.insert(IndexKey(i), AnchorKey::from_raw(u128::from(i)))
                .unwrap();
        }
        assert!(!idx.is_tree(), "should still be array at threshold");
        assert_eq!(idx.len(), 5);

        // The 6th insert triggers promotion
        idx.insert(IndexKey(5), AnchorKey::from_raw(5)).unwrap();
        assert!(idx.is_tree(), "should be tree after threshold+1");
        assert_eq!(idx.len(), 6);
    }

    #[test]
    fn all_entries_survive_conversion() {
        let threshold = 10u32;
        let mut idx = AdaptiveIndex::with_threshold(threshold);

        for i in 0..(threshold + 5) {
            idx.insert(
                IndexKey(u64::from(i) * 7),
                AnchorKey::from_raw(u128::from(i)),
            )
            .unwrap();
        }

        assert!(idx.is_tree());
        for i in 0..(threshold + 5) {
            assert!(
                idx.lookup(&IndexKey(u64::from(i) * 7)).is_some(),
                "entry {i} lost during conversion"
            );
        }
    }

    #[test]
    fn conversion_is_one_way() {
        let mut idx = AdaptiveIndex::with_threshold(3);
        for i in 0..5 {
            idx.insert(IndexKey(i), AnchorKey::from_raw(u128::from(i)))
                .unwrap();
        }
        assert!(idx.is_tree());

        // Remove entries below threshold — still a tree
        idx.remove(&IndexKey(0));
        idx.remove(&IndexKey(1));
        idx.remove(&IndexKey(2));
        assert!(idx.is_tree(), "conversion must be one-way");
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn sorted_iteration_both_modes() {
        for &threshold in &[5u32, 100] {
            let mut idx = AdaptiveIndex::with_threshold(threshold);
            let n = 20u64;
            for i in (0..n).rev() {
                idx.insert(IndexKey(i), AnchorKey::from_raw(u128::from(i)))
                    .unwrap();
            }
            let all = idx.all_sorted();
            assert_eq!(all.len(), usize::try_from(n).unwrap());
            for w in all.windows(2) {
                assert!(w[0].0 < w[1].0, "must be sorted");
            }
        }
    }

    #[test]
    fn range_query_both_modes() {
        for &threshold in &[5u32, 100] {
            let mut idx = AdaptiveIndex::with_threshold(threshold);
            for i in 0..20 {
                idx.insert(
                    IndexKey(i * 10),
                    AnchorKey::from_raw(u128::from(i)),
                )
                .unwrap();
            }
            let result = idx.range(IndexKey(50)..IndexKey(120));
            let keys: Vec<u64> = result.iter().map(|(k, _)| k.0).collect();
            assert_eq!(keys, vec![50, 60, 70, 80, 90, 100, 110]);
        }
    }
}
