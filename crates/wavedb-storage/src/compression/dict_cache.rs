//! Per-STRUCT dictionary cache with LRU eviction bounded by `max_dict_memory`.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// A trained zstd dictionary for a specific STRUCT_ID.
#[derive(Debug, Clone)]
pub struct Dictionary {
    /// The STRUCT_ID this dictionary was trained for.
    pub struct_id: u32,
    /// The dictionary version (monotonically increasing).
    pub version: u32,
    /// The raw dictionary bytes.
    pub data: Vec<u8>,
}

/// Entry in the LRU cache.
struct CacheEntry {
    dict: Arc<Dictionary>,
    access_count: u64,
}

/// In-memory LRU cache for per-STRUCT dictionaries.
pub struct DictCache {
    entries: RwLock<HashMap<(u32, u32), CacheEntry>>,
    max_memory: usize,
    current_memory: RwLock<usize>,
}

impl DictCache {
    /// Create a new dict cache with the given memory budget (bytes).
    pub fn new(max_memory: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            max_memory,
            current_memory: RwLock::new(0),
        }
    }

    /// Get a dictionary, loading from the provided loader if not cached.
    pub fn get_or_load<F>(
        &self,
        struct_id: u32,
        version: u32,
        loader: F,
    ) -> crate::StorageResult<Arc<Dictionary>>
    where
        F: FnOnce() -> crate::StorageResult<Dictionary>,
    {
        let key = (struct_id, version);

        // Fast path: check if already cached
        {
            let mut entries = self.entries.write();
            if let Some(entry) = entries.get_mut(&key) {
                entry.access_count += 1;
                return Ok(Arc::clone(&entry.dict));
            }
        }

        // Slow path: load, evict if needed, insert
        let dict = Arc::new(loader()?);
        let dict_size = dict.data.len();

        // Evict until we have room
        self.evict_until(dict_size);

        let mut entries = self.entries.write();
        entries.insert(
            key,
            CacheEntry {
                dict: Arc::clone(&dict),
                access_count: 1,
            },
        );
        *self.current_memory.write() += dict_size;

        Ok(dict)
    }

    /// Insert or update a dictionary directly (e.g. from a journal update).
    pub fn put(&self, dict: Dictionary) {
        let key = (dict.struct_id, dict.version);
        let dict_size = dict.data.len();
        self.evict_until(dict_size);

        let mut entries = self.entries.write();
        // Remove old version if present
        if let Some(old) = entries.remove(&key) {
            *self.current_memory.write() -= old.dict.data.len();
        }
        entries.insert(
            key,
            CacheEntry {
                dict: Arc::new(dict),
                access_count: 1,
            },
        );
        *self.current_memory.write() += dict_size;
    }

    /// Evict LRU entries until there's room for `needed` bytes.
    fn evict_until(&self, needed: usize) {
        let mut current = *self.current_memory.read();
        if current + needed <= self.max_memory {
            return;
        }

        let mut entries = self.entries.write();
        while current + needed > self.max_memory && !entries.is_empty() {
            // Find the entry with the lowest access count
            let victim = entries
                .iter()
                .min_by_key(|(_, e)| e.access_count)
                .map(|(k, _)| *k);

            if let Some(key) = victim {
                if let Some(entry) = entries.remove(&key) {
                    current -= entry.dict.data.len();
                    *self.current_memory.write() = current;
                }
            } else {
                break;
            }
        }
    }

    /// Number of cached dictionaries.
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    /// Current approximate memory usage.
    pub fn memory_usage(&self) -> usize {
        *self.current_memory.read()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_dict(sid: u32, version: u32, size: usize) -> Dictionary {
        Dictionary {
            struct_id: sid,
            version,
            data: vec![0u8; size],
        }
    }

    #[test]
    fn insert_and_retrieve() {
        let cache = DictCache::new(1_000_000);
        cache.put(make_dict(1, 1, 100));
        let got = cache
            .get_or_load(1, 1, || panic!("should not load"))
            .unwrap();
        assert_eq!(got.struct_id, 1);
        assert_eq!(got.version, 1);
    }

    #[test]
    fn cache_miss_triggers_loader() {
        let cache = DictCache::new(1_000_000);
        let got = cache
            .get_or_load(5, 1, || Ok(make_dict(5, 1, 200)))
            .unwrap();
        assert_eq!(got.struct_id, 5);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn eviction_under_memory_pressure() {
        let cache = DictCache::new(500); // very small budget
        cache.put(make_dict(1, 1, 200));
        cache.put(make_dict(2, 1, 200));
        assert_eq!(cache.len(), 2);

        // This should evict one entry
        cache.put(make_dict(3, 1, 200));
        assert!(cache.len() <= 3);
        assert!(cache.memory_usage() <= 500);
    }

    #[test]
    fn frequently_accessed_survives_eviction() {
        let cache = DictCache::new(500);
        cache.put(make_dict(1, 1, 200));
        // Access entry 1 many times
        for _ in 0..10 {
            let _ = cache.get_or_load(1, 1, || panic!("should not load"));
        }
        cache.put(make_dict(2, 1, 200));
        // Adding entry 3 should evict entry 2 (least accessed), not entry 1
        cache.put(make_dict(3, 1, 200));
        // Entry 1 should still be there
        assert!(
            cache
                .get_or_load(1, 1, || panic!("should not load"))
                .is_ok()
        );
    }
}
