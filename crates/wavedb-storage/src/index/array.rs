//! Linear array index — cache-friendly contiguous storage for small collections.
//!
//! For collections under `MAX_NON_UNIQUE_ELEMENTS`, lookups are O(N) but
//! beat a tree on real hardware due to branch prediction and cache locality.

use super::{IndexBackend, IndexKey};
use crate::anchor::AnchorKey;
use std::ops::Range;

/// A sorted array of `(key, anchor)` pairs.
#[derive(Debug, Clone)]
pub struct ArrayIndex {
    entries: Vec<(IndexKey, AnchorKey)>,
}

impl ArrayIndex {
    /// Create an empty array index.
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Create from existing entries (will be sorted).
    pub fn from_entries(mut entries: Vec<(IndexKey, AnchorKey)>) -> Self {
        entries.sort_by_key(|(k, _)| *k);
        Self { entries }
    }

    /// Consume this array and return the sorted entries (for bulk-loading into B+ tree).
    pub fn into_entries(self) -> Vec<(IndexKey, AnchorKey)> {
        self.entries
    }
}

impl Default for ArrayIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexBackend for ArrayIndex {
    fn insert(
        &mut self,
        key: IndexKey,
        anchor: AnchorKey,
    ) -> crate::StorageResult<()> {
        // Binary search for insertion point to keep sorted order.
        let pos = self
            .entries
            .binary_search_by_key(&key, |(k, _)| *k)
            .unwrap_or_else(|e| e);
        self.entries.insert(pos, (key, anchor));
        Ok(())
    }

    fn lookup(&self, key: &IndexKey) -> Option<AnchorKey> {
        self.entries
            .binary_search_by_key(key, |(k, _)| *k)
            .ok()
            .map(|idx| self.entries[idx].1)
    }

    fn remove(&mut self, key: &IndexKey) -> bool {
        if let Ok(idx) = self.entries.binary_search_by_key(key, |(k, _)| *k) {
            self.entries.remove(idx);
            true
        } else {
            false
        }
    }

    fn range(&self, range: Range<IndexKey>) -> Vec<(IndexKey, AnchorKey)> {
        self.entries
            .iter()
            .filter(|(k, _)| *k >= range.start && *k < range.end)
            .copied()
            .collect()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_maintains_sorted_order() {
        let mut idx = ArrayIndex::new();
        idx.insert(IndexKey(50), AnchorKey::from_raw(5)).unwrap();
        idx.insert(IndexKey(10), AnchorKey::from_raw(1)).unwrap();
        idx.insert(IndexKey(30), AnchorKey::from_raw(3)).unwrap();
        idx.insert(IndexKey(20), AnchorKey::from_raw(2)).unwrap();
        idx.insert(IndexKey(40), AnchorKey::from_raw(4)).unwrap();

        let all = idx.all_sorted();
        let keys: Vec<u64> = all.iter().map(|(k, _)| k.0).collect();
        assert_eq!(keys, vec![10, 20, 30, 40, 50]);
    }

    #[test]
    fn lookup_existing() {
        let mut idx = ArrayIndex::new();
        idx.insert(IndexKey(42), AnchorKey::from_raw(99)).unwrap();
        assert_eq!(idx.lookup(&IndexKey(42)).unwrap().raw(), 99);
        assert!(idx.lookup(&IndexKey(43)).is_none());
    }

    #[test]
    fn remove_entry() {
        let mut idx = ArrayIndex::new();
        idx.insert(IndexKey(10), AnchorKey::from_raw(1)).unwrap();
        idx.insert(IndexKey(20), AnchorKey::from_raw(2)).unwrap();
        assert!(idx.remove(&IndexKey(10)));
        assert!(!idx.remove(&IndexKey(10)));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn range_query() {
        let mut idx = ArrayIndex::new();
        for i in 0..10 {
            idx.insert(IndexKey(i * 10), AnchorKey::from_raw(u128::from(i)))
                .unwrap();
        }
        let result = idx.range(IndexKey(25)..IndexKey(65));
        let keys: Vec<u64> = result.iter().map(|(k, _)| k.0).collect();
        assert_eq!(keys, vec![30, 40, 50, 60]);
    }
}
