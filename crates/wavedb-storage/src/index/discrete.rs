//! Discrete value index: hash-bucket → array-or-tree model.
//!
//! Each bucket starts as an array and promotes to a per-bucket B+ tree
//! if it grows past the threshold. Used for discrete (non-ordered) lookups.

use super::adaptive::AdaptiveIndex;
use super::{DEFAULT_MAX_NON_UNIQUE_ELEMENTS, IndexBackend, IndexKey};
use crate::anchor::AnchorKey;
use std::collections::HashMap;

/// A discrete index that buckets entries by a hash of the indexed property value.
///
/// Each bucket is an `AdaptiveIndex`, so it starts as an array and promotes
/// to a B+ tree if it grows past the threshold.
#[derive(Debug, Clone)]
pub struct DiscreteIndex {
    buckets: HashMap<u64, AdaptiveIndex>,
    threshold: u32,
    total_count: usize,
}

impl DiscreteIndex {
    /// Create a new empty discrete index.
    pub fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            threshold: DEFAULT_MAX_NON_UNIQUE_ELEMENTS,
            total_count: 0,
        }
    }

    /// Create with a custom threshold.
    pub fn with_threshold(threshold: u32) -> Self {
        Self {
            buckets: HashMap::new(),
            threshold,
            total_count: 0,
        }
    }

    /// Insert an entry into the bucket identified by `bucket_key`.
    pub fn insert(
        &mut self,
        bucket_key: u64,
        sort_key: IndexKey,
        anchor: AnchorKey,
    ) -> crate::StorageResult<()> {
        let bucket = self
            .buckets
            .entry(bucket_key)
            .or_insert_with(|| AdaptiveIndex::with_threshold(self.threshold));
        bucket.insert(sort_key, anchor)?;
        self.total_count += 1;
        Ok(())
    }

    /// Lookup all entries in a given bucket.
    pub fn lookup_bucket(&self, bucket_key: u64) -> Vec<(IndexKey, AnchorKey)> {
        self.buckets
            .get(&bucket_key)
            .map(IndexBackend::all_sorted)
            .unwrap_or_default()
    }

    /// Point lookup: find a specific sort key within a bucket.
    pub fn lookup(&self, bucket_key: u64, sort_key: &IndexKey) -> Option<AnchorKey> {
        self.buckets
            .get(&bucket_key)
            .and_then(|b| b.lookup(sort_key))
    }

    /// Remove an entry from a bucket.
    pub fn remove(&mut self, bucket_key: u64, sort_key: &IndexKey) -> bool {
        if let Some(bucket) = self.buckets.get_mut(&bucket_key) {
            if bucket.remove(sort_key) {
                self.total_count -= 1;
                return true;
            }
        }
        false
    }

    /// Total number of entries across all buckets.
    pub fn total_count(&self) -> usize {
        self.total_count
    }

    /// Number of distinct buckets.
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }
}

impl Default for DiscreteIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_into_buckets() {
        let mut idx = DiscreteIndex::new();
        idx.insert(1, IndexKey(10), AnchorKey::from_raw(100))
            .unwrap();
        idx.insert(1, IndexKey(20), AnchorKey::from_raw(200))
            .unwrap();
        idx.insert(2, IndexKey(30), AnchorKey::from_raw(300))
            .unwrap();

        assert_eq!(idx.total_count(), 3);
        assert_eq!(idx.bucket_count(), 2);
    }

    #[test]
    fn lookup_by_bucket() {
        let mut idx = DiscreteIndex::new();
        idx.insert(42, IndexKey(1), AnchorKey::from_raw(10))
            .unwrap();
        idx.insert(42, IndexKey(2), AnchorKey::from_raw(20))
            .unwrap();
        idx.insert(99, IndexKey(3), AnchorKey::from_raw(30))
            .unwrap();

        let bucket_42 = idx.lookup_bucket(42);
        assert_eq!(bucket_42.len(), 2);

        let bucket_99 = idx.lookup_bucket(99);
        assert_eq!(bucket_99.len(), 1);

        let empty = idx.lookup_bucket(0);
        assert!(empty.is_empty());
    }

    #[test]
    fn point_lookup() {
        let mut idx = DiscreteIndex::new();
        idx.insert(42, IndexKey(1), AnchorKey::from_raw(10))
            .unwrap();
        assert_eq!(idx.lookup(42, &IndexKey(1)).unwrap().raw(), 10);
        assert!(idx.lookup(42, &IndexKey(999)).is_none());
    }

    #[test]
    fn remove_from_bucket() {
        let mut idx = DiscreteIndex::new();
        idx.insert(1, IndexKey(10), AnchorKey::from_raw(100))
            .unwrap();
        idx.insert(1, IndexKey(20), AnchorKey::from_raw(200))
            .unwrap();
        assert!(idx.remove(1, &IndexKey(10)));
        assert_eq!(idx.total_count(), 1);
        assert!(!idx.remove(1, &IndexKey(10)));
    }

    #[test]
    fn bucket_promotes_to_tree() {
        let mut idx = DiscreteIndex::with_threshold(3);
        for i in 0..5 {
            idx.insert(1, IndexKey(i), AnchorKey::from_raw(i as u128))
                .unwrap();
        }
        // All entries should survive the promotion
        for i in 0..5 {
            assert!(idx.lookup(1, &IndexKey(i)).is_some());
        }
    }
}
