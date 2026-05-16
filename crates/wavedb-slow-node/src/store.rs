//! Journal-backed history store for the Slow-Node.
//!
//! Records are appended to a [`Journal`] for crash recovery, then indexed
//! in memory by `(tenant, record_id)` for fast history reads.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use wavedb_storage::Journal;
use wavedb_storage::VersionedRecord;
use wavedb_storage::pipeline::journal::JournalEntry;

use crate::flush::{FlushBatch, ReceiptWatermark};

// ── HistoryStore ──────────────────────────────────────────────────────────────

/// In-memory index + journal-backed persistence for versioned history records.
#[derive(Clone)]
pub struct HistoryStore {
    inner: Arc<Mutex<HistoryStoreInner>>,
    flush_count: Arc<AtomicU64>,
}

struct HistoryStoreInner {
    /// `(tenant, record_id) → serialized record bytes`
    index: HashMap<(u64, u128), Vec<u8>>,
    journal: Journal,
    watermark: ReceiptWatermark,
}

impl HistoryStore {
    /// Create an in-memory store (tests / ephemeral use).
    pub fn in_memory() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HistoryStoreInner {
                index: HashMap::new(),
                journal: Journal::in_memory(),
                watermark: ReceiptWatermark::new(),
            })),
            flush_count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Open or create a persistent store rooted at `data_dir`.
    pub fn open(data_dir: &Path) -> wavedb_storage::StorageResult<Self> {
        let journal_path = data_dir.join("history.journal");
        let journal = Journal::open(&journal_path)?;

        let mut index: HashMap<(u64, u128), Vec<u8>> = HashMap::new();
        let mut watermark = ReceiptWatermark::new();
        let mut replay_flush_count: u64 = 0;

        for entry in journal.all_entries() {
            if let JournalEntry::WriteVersioned { record } = entry {
                // These u128 fields hold u64 values packed into u128 at flush
                // time; the upper 64 bits are always zero.
                #[allow(clippy::cast_possible_truncation)]
                let tenant = record.old_modification_id as u64;
                #[allow(clippy::cast_possible_truncation)]
                let write_seq = record.new_modification_id as u64;
                if let Ok(bytes) = record.to_bytes() {
                    index.insert((tenant, record.id), bytes);
                }
                if write_seq > 0 {
                    watermark.advance(tenant, write_seq);
                    replay_flush_count = replay_flush_count.max(write_seq);
                }
            }
        }

        Ok(Self {
            inner: Arc::new(Mutex::new(HistoryStoreInner {
                index,
                journal,
                watermark,
            })),
            flush_count: Arc::new(AtomicU64::new(replay_flush_count)),
        })
    }

    /// Apply a [`FlushBatch`]: journal all records and update the in-memory index.
    ///
    /// Returns the write sequence that was durably stored.
    pub fn apply_flush(&self, batch: FlushBatch) -> wavedb_storage::StorageResult<u64> {
        let write_seq = batch.write_seq;
        let tenant = batch.tenant;
        let mut guard = self.inner.lock();
        for record in batch.records {
            let journaled = VersionedRecord {
                old_modification_id: u128::from(tenant),
                new_modification_id: u128::from(write_seq),
                ..record.clone()
            };
            let bytes = journaled.to_bytes()?;
            guard
                .journal
                .append(JournalEntry::WriteVersioned { record: journaled })?;
            guard.index.insert((tenant, record.id), bytes);
        }
        guard.journal.flush()?;
        guard.watermark.advance(tenant, write_seq);
        drop(guard);
        self.flush_count.fetch_add(1, Ordering::Relaxed);
        Ok(write_seq)
    }

    /// Look up a versioned record by `(tenant, record_id)`.
    ///
    /// Returns the raw serialized bytes, or `None` if not found.
    pub fn get(&self, tenant: u64, record_id: u128) -> Option<Vec<u8>> {
        self.inner.lock().index.get(&(tenant, record_id)).cloned()
    }

    /// Highest durably-acked write sequence for `tenant`.
    pub fn high_water(&self, tenant: u64) -> u64 {
        self.inner.lock().watermark.high_water(tenant)
    }

    /// Total number of indexed records.
    pub fn len(&self) -> usize {
        self.inner.lock().index.len()
    }

    /// `true` when no records are indexed.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().index.is_empty()
    }

    /// Number of distinct tenants with at least one indexed record.
    pub fn tenant_count(&self) -> usize {
        let guard = self.inner.lock();
        let mut tenants = std::collections::HashSet::new();
        for (tenant, _) in guard.index.keys() {
            tenants.insert(*tenant);
        }
        drop(guard);
        tenants.len()
    }

    /// Cumulative number of flush batches successfully applied.
    pub fn flush_count(&self) -> u64 {
        self.flush_count.load(Ordering::Relaxed)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(tenant: u64, write_seq: u64, id: u128, data: &[u8]) -> FlushBatch {
        FlushBatch {
            write_seq,
            tenant,
            records: vec![VersionedRecord::new(id, data.to_vec())],
            token: None,
        }
    }

    #[test]
    fn flush_stores_record_and_advances_watermark() {
        let store = HistoryStore::in_memory();
        let seq = store.apply_flush(batch(42, 1, 100, b"payload")).unwrap();
        assert_eq!(seq, 1);
        assert_eq!(store.high_water(42), 1);
        assert!(store.get(42, 100).is_some());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn multiple_flushes_advance_watermark() {
        let store = HistoryStore::in_memory();
        store.apply_flush(batch(1, 1, 10, b"a")).unwrap();
        store.apply_flush(batch(1, 5, 11, b"b")).unwrap();
        assert_eq!(store.high_water(1), 5);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn unknown_record_returns_none() {
        let store = HistoryStore::in_memory();
        assert!(store.get(42, 999).is_none());
    }

    #[test]
    fn get_returns_correct_record_bytes() {
        let store = HistoryStore::in_memory();
        store.apply_flush(batch(7, 1, 55, b"the data")).unwrap();
        let bytes = store.get(7, 55).unwrap();
        let rec = VersionedRecord::from_bytes(&bytes).unwrap();
        // data is stored in the journaled record which carries tenant/seq in
        // old/new_modification_id — the raw data field is unchanged
        assert_eq!(rec.data, b"the data");
    }

    #[test]
    fn per_tenant_watermarks_independent() {
        let store = HistoryStore::in_memory();
        store.apply_flush(batch(1, 10, 1, b"")).unwrap();
        store.apply_flush(batch(2, 3, 2, b"")).unwrap();
        assert_eq!(store.high_water(1), 10);
        assert_eq!(store.high_water(2), 3);
    }

    #[test]
    fn persistent_store_replays_on_open() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = HistoryStore::open(dir.path()).unwrap();
            store.apply_flush(batch(42, 7, 99, b"durable")).unwrap();
        }
        let store2 = HistoryStore::open(dir.path()).unwrap();
        assert!(store2.get(42, 99).is_some());
        assert_eq!(store2.high_water(42), 7);
    }
}
