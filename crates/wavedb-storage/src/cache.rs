//! In-memory write/read cache shaped like the on-disk hash-map.

use crate::anchor::{AnchorKey, AnchorSlot};
use crate::versioned::VersionedRecord;
use parking_lot::RwLock;
use std::collections::HashMap;

/// In-memory cache mirroring on-disk layout.
pub struct Cache {
    anchors: RwLock<HashMap<u128, AnchorSlot>>,
    versioned: RwLock<HashMap<u128, VersionedRecord>>,
}

impl Cache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self {
            anchors: RwLock::new(HashMap::new()),
            versioned: RwLock::new(HashMap::new()),
        }
    }

    /// Insert or update an anchor.
    pub fn put_anchor(&self, key: AnchorKey, slot: AnchorSlot) {
        self.anchors.write().insert(key.raw(), slot);
    }

    /// Look up an anchor.
    pub fn get_anchor(&self, key: AnchorKey) -> Option<AnchorSlot> {
        self.anchors.read().get(&key.raw()).cloned()
    }

    /// Remove an anchor.
    pub fn remove_anchor(&self, key: AnchorKey) -> Option<AnchorSlot> {
        self.anchors.write().remove(&key.raw())
    }

    /// Insert or update a versioned record.
    pub fn put_versioned(&self, record: VersionedRecord) {
        self.versioned.write().insert(record.id, record);
    }

    /// Look up a versioned record by its ID.
    pub fn get_versioned(&self, id: u128) -> Option<VersionedRecord> {
        self.versioned.read().get(&id).cloned()
    }

    /// Number of cached anchors.
    pub fn anchor_count(&self) -> usize { self.anchors.read().len() }

    /// Number of cached versioned records.
    pub fn versioned_count(&self) -> usize { self.versioned.read().len() }

    /// Approximate memory usage in bytes.
    pub fn estimated_bytes(&self) -> usize {
        // Rough estimate: each entry ~256 bytes on average
        (self.anchor_count() + self.versioned_count()) * 256
    }
}

impl Default for Cache {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_crud() {
        let cache = Cache::new();
        let key = AnchorKey::from_raw(42);
        let slot = AnchorSlot::inline(b"data", 1000);

        assert!(cache.get_anchor(key).is_none());
        cache.put_anchor(key, slot.clone());
        assert!(cache.get_anchor(key).is_some());
        assert_eq!(cache.anchor_count(), 1);

        cache.remove_anchor(key);
        assert!(cache.get_anchor(key).is_none());
    }

    #[test]
    fn versioned_crud() {
        let cache = Cache::new();
        let rec = VersionedRecord::new(99, b"hello".to_vec());

        cache.put_versioned(rec);
        let got = cache.get_versioned(99).unwrap();
        assert_eq!(got.data, b"hello");
    }
}
