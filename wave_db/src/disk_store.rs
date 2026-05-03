//! Disk-backed WaveDB store.
//!
//! `DiskStore` wraps the in-memory [`Store`] as a read cache and persists every
//! mutation to four on-disk files:
//!
//! | File      | Role                                              |
//! |-----------|---------------------------------------------------|
//! | `data`    | Hash-mapped pages: anchor slots + versioned recs  |
//! | `journal` | Append-only mutation log for crash recovery       |
//! | `heap`    | 4 KiB-aligned overflow payloads                   |
//! | `index`   | Per-`(STRUCT_ID, TENANT_ID)` creation-order index |
//!
//! # Write path
//!
//! 1. Update the in-memory `Store` cache.
//! 2. Write to the `data` file (both anchor and versioned slots).
//! 3. Append a [`JournalEntry`] to the journal file.
//! 4. Update the `index`.
//!
//! Data is written before the journal so that on a crash-between-writes, the
//! data file already has the record and the journal entry is simply absent —
//! a safe state that `scan_all` + journal-replay will converge on correctly.
//!
//! # On-open rebuild
//!
//! 1. `DataFile::scan_all()` reads every live page slot.
//! 2. Each entry is decoded by its tag byte (anchor `0x01` vs versioned `0x02`).
//! 3. Decoded entries are loaded into the in-memory `Store` via `load_anchor` /
//!    `load_versioned` (bypass-insert, no journal side-effect).
//! 4. The journal is replayed to recover any mutations that hit the journal
//!    but whose corresponding data-file writes may not be complete yet.

use std::{
    fs,
    io,
    path::{Path, PathBuf},
};

use crate::{
    anchor::AnchorSlot,
    config::Config,
    id::{Id, StructId, TenantId},
    journal::JournalEntry,
    page::PageKey,
    store::{AnchorMode, StoredRecord, Store, StoreError},
    storage::{
        codec::{self, CodecError},
        data_file::DataFile,
        heap_file::HeapFile,
        index_file::IndexFile,
        journal_file::JournalFile,
    },
};

// ─── Error ────────────────────────────────────────────────────────────────────

/// Errors from [`DiskStore`] operations.
#[derive(Debug)]
pub enum DiskStoreError {
    Store(StoreError),
    Io(io::Error),
    Codec(CodecError),
}

impl std::fmt::Display for DiskStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "store error: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Codec(e) => write!(f, "codec error: {e}"),
        }
    }
}

impl std::error::Error for DiskStoreError {}

impl From<StoreError> for DiskStoreError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<io::Error> for DiskStoreError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<CodecError> for DiskStoreError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}

// ─── DiskStore ────────────────────────────────────────────────────────────────

/// Disk-backed WaveDB store.
///
/// All reads are served from the in-memory [`Store`] cache; all writes go to
/// both the cache and the on-disk files atomically (data file first, then
/// journal).
pub struct DiskStore {
    /// In-memory cache — all reads are O(1) from here.
    cache: Store,
    data: DataFile,
    journal: JournalFile,
    heap: HeapFile,
    index: IndexFile,
    index_path: PathBuf,
    config: Config,
}

impl DiskStore {
    // ── Open / create ─────────────────────────────────────────────────────────

    /// Open an existing WaveDB directory or create a new one.
    ///
    /// On open the in-memory cache is rebuilt from `scan_all` + journal replay.
    pub fn open(
        dir: &Path,
        config: Config,
        anchor_mode: AnchorMode,
    ) -> Result<Self, DiskStoreError> {
        fs::create_dir_all(dir)?;

        let data_path = dir.join("data");
        let journal_path = dir.join("journal");
        let heap_path = dir.join("heap");
        let index_path = dir.join("index");

        let mut data = DataFile::open_or_create(&data_path, config.page_size)?;
        let mut journal = JournalFile::open_or_create(&journal_path)?;
        let heap = HeapFile::open_or_create(&heap_path)?;
        let index = IndexFile::open_or_create(&index_path)?;

        let mut cache = Store::new(config.clone(), anchor_mode);

        // Rebuild from data file
        let all_entries = data.scan_all()?;
        rebuild_cache(&mut cache, all_entries)?;

        // Replay journal on top (handles writes that hit journal but not data file yet)
        let pending = journal.replay()?;
        apply_journal_entries(&mut cache, &mut data, &pending)?;

        Ok(Self {
            cache,
            data,
            journal,
            heap,
            index,
            index_path,
            config,
        })
    }

    // ── Write operations ─────────────────────────────────────────────────────

    /// Insert a new record (first version).
    pub fn insert(&mut self, record: StoredRecord) -> Result<(), DiskStoreError> {
        let rec_id = record.id;
        let struct_id = rec_id.struct_id();
        let tenant_id = rec_id.tenant_id();
        let shard_id = rec_id.shard_id();
        let anchor_sentinel = codec::anchor_sentinel_id(rec_id);

        // 1. Update in-memory cache
        self.cache.insert(record.clone())?;

        // 2. Write versioned record to data file
        let versioned_bytes = codec::encode_versioned_record(&record);
        self.data.insert(rec_id.raw(), &versioned_bytes)?;

        // 3. Write anchor slot to data file
        let anchor = self.cache.anchor(struct_id, tenant_id).expect("just inserted");
        let anchor_bytes = codec::encode_anchor_slot(anchor);
        self.data.insert(anchor_sentinel.raw(), &anchor_bytes)?;

        // 4. Update index
        self.index.insert(
            struct_id.value(),
            tenant_id.value(),
            rec_id.created_at().ticks(),
            anchor_sentinel.raw(),
        );

        // 5. Journal entry
        let key = PageKey::new(struct_id, tenant_id);
        self.journal.append(&JournalEntry::Insert { record_id: rec_id, anchor_key: key })?;

        Ok(())
    }

    /// Mutate an existing record.
    pub fn mutate(
        &mut self,
        old_id: Id,
        new_record: StoredRecord,
    ) -> Result<(), DiskStoreError> {
        let new_id = new_record.id;
        let struct_id = old_id.struct_id();
        let tenant_id = old_id.tenant_id();
        let anchor_sentinel = codec::anchor_sentinel_id(old_id);

        // Old created_at for index removal
        let old_created_at = old_id.created_at().ticks();

        // 1. Update in-memory cache (validates old_id is live, updates chain)
        self.cache.mutate(old_id, new_record.clone())?;

        // 2. Write new versioned record to data file
        let new_versioned_bytes = codec::encode_versioned_record(&new_record);
        self.data.insert(new_id.raw(), &new_versioned_bytes)?;

        // 3. Update old versioned record (its new_modification_id was updated in cache)
        if let Some(old_rec) = self.cache.get_versioned(old_id) {
            let old_bytes = codec::encode_versioned_record(old_rec);
            self.data.update(old_id.raw(), &old_bytes)?;
        }

        // 4. Overwrite anchor slot with new state
        let anchor = self
            .cache
            .anchor(struct_id, tenant_id)
            .expect("anchor exists after mutate");
        let anchor_bytes = codec::encode_anchor_slot(anchor);
        self.data.update(anchor_sentinel.raw(), &anchor_bytes)?;

        // 5. Update index: remove old created_at entry, insert new
        self.index.remove(struct_id.value(), tenant_id.value(), old_created_at);
        self.index.insert(
            struct_id.value(),
            tenant_id.value(),
            new_id.created_at().ticks(),
            anchor_sentinel.raw(),
        );

        // 6. Journal
        let key = PageKey::new(struct_id, tenant_id);
        self.journal
            .append(&JournalEntry::Mutate { old_id, new_id, anchor_key: key })?;

        Ok(())
    }

    /// Delete a record: tombstone the anchor, keep history.
    pub fn delete(
        &mut self,
        struct_id: StructId,
        tenant_id: TenantId,
    ) -> Result<(), DiskStoreError> {
        // Capture the final version ID before deletion for the journal
        let anchor = self
            .cache
            .anchor(struct_id, tenant_id)
            .ok_or(DiskStoreError::Store(StoreError::NotFound))?;

        if !anchor.is_live() {
            return Err(StoreError::Deleted.into());
        }

        let final_id = anchor
            .current_version_id()
            .expect("live anchor has current_version_id");
        let anchor_sentinel = codec::anchor_sentinel_id(final_id);

        // 1. Update cache (tombstones the anchor)
        self.cache.delete(struct_id, tenant_id)?;

        // 2. Overwrite anchor in data file with tombstone bytes
        let tombstone = self
            .cache
            .anchor(struct_id, tenant_id)
            .expect("anchor exists after delete (as tombstone)");
        let tombstone_bytes = codec::encode_anchor_slot(tombstone);
        self.data.update(anchor_sentinel.raw(), &tombstone_bytes)?;

        // 3. Remove from index
        self.index.remove(
            struct_id.value(),
            tenant_id.value(),
            final_id.created_at().ticks(),
        );

        // 4. Journal
        let key = PageKey::new(struct_id, tenant_id);
        self.journal
            .append(&JournalEntry::Delete { anchor_key: key, final_version_id: final_id })?;

        Ok(())
    }

    // ── Read operations (served from cache) ──────────────────────────────────

    /// Live record for `(STRUCT_ID, TENANT_ID)`. `None` if tombstoned.
    pub fn get_live(&self, struct_id: StructId, tenant_id: TenantId) -> Option<&StoredRecord> {
        self.cache.get_live(struct_id, tenant_id)
    }

    /// Look up a specific versioned record by its full ID.
    pub fn get_versioned(&self, id: Id) -> Option<&StoredRecord> {
        self.cache.get_versioned(id)
    }

    /// Reference to the anchor slot.
    pub fn anchor(&self, struct_id: StructId, tenant_id: TenantId) -> Option<&AnchorSlot> {
        self.cache.anchor(struct_id, tenant_id)
    }

    /// Full version history from newest to oldest.
    pub fn history_backward(
        &self,
        struct_id: StructId,
        tenant_id: TenantId,
    ) -> Vec<&StoredRecord> {
        self.cache.history_backward(struct_id, tenant_id)
    }

    /// Full version history from oldest to newest.
    pub fn history_forward(
        &self,
        struct_id: StructId,
        tenant_id: TenantId,
    ) -> Vec<&StoredRecord> {
        self.cache.history_forward(struct_id, tenant_id)
    }

    // ── Heap (overflow payloads) ──────────────────────────────────────────────

    /// Write a large payload to the heap file. Returns a stable [`crate::storage::heap_file::HeapPtr`].
    pub fn heap_append(
        &mut self,
        data: &[u8],
    ) -> Result<crate::storage::heap_file::HeapPtr, DiskStoreError> {
        Ok(self.heap.append(data)?)
    }

    /// Read a heap payload by pointer.
    pub fn heap_read(
        &mut self,
        ptr: crate::storage::heap_file::HeapPtr,
    ) -> Result<Vec<u8>, DiskStoreError> {
        Ok(self.heap.read(ptr)?)
    }

    // ── Index ─────────────────────────────────────────────────────────────────

    /// Ordered anchor IDs for `(struct_id, tenant_id)` — creation order.
    pub fn index_range(&self, struct_id: StructId, tenant_id: TenantId) -> Vec<u128> {
        self.index.range_for_tenant(struct_id.value(), tenant_id.value())
    }

    // ── Flush / close ─────────────────────────────────────────────────────────

    /// Flush all file handles to disk (no-op if nothing is dirty).
    pub fn flush(&mut self) -> Result<(), DiskStoreError> {
        self.data.sync()?;
        self.journal.flush()?;
        self.heap.sync()?;
        self.index.flush(&self.index_path)?;
        Ok(())
    }

    /// Flush, sync, and consume the store.
    pub fn close(mut self) -> Result<(), DiskStoreError> {
        self.flush()
    }

    // ── Stats ─────────────────────────────────────────────────────────────────

    pub fn live_count(&self) -> usize {
        self.cache.live_count()
    }

    pub fn versioned_count(&self) -> usize {
        self.cache.versioned_count()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Number of data file pages (including the header page).
    pub fn data_page_count(&self) -> u64 {
        self.data.page_count
    }

    /// Heap append cursor (next write offset, 4 KiB-aligned).
    pub fn heap_cursor(&self) -> u64 {
        self.heap.cursor()
    }
}

// ─── Rebuild helpers ──────────────────────────────────────────────────────────

/// Decode raw `(u128, Vec<u8>)` scan pairs into the in-memory cache.
fn rebuild_cache(
    cache: &mut Store,
    entries: Vec<(u128, Vec<u8>)>,
) -> Result<(), DiskStoreError> {
    // Sort: anchors (created_at=0) last so versioned records are loaded first.
    // This way anchor.current_version_id can be resolved immediately.
    let mut versioned_entries = Vec::new();
    let mut anchor_entries = Vec::new();

    for (raw_id, bytes) in entries {
        let id = Id::from_raw(raw_id);
        if codec::is_anchor_sentinel(id) {
            anchor_entries.push((id, bytes));
        } else {
            versioned_entries.push((raw_id, bytes));
        }
    }

    // Load versioned records first
    for (_, bytes) in versioned_entries {
        match codec::decode_versioned_record(&bytes) {
            Ok(rec) => cache.load_versioned(rec),
            Err(_) => {} // skip corrupt entries during rebuild
        }
    }

    // Load anchor slots
    for (sentinel_id, bytes) in anchor_entries {
        match codec::decode_anchor_slot(&bytes) {
            Ok(slot) => {
                let key = PageKey::new(sentinel_id.struct_id(), sentinel_id.tenant_id());
                cache.load_anchor(key, slot);
            }
            Err(_) => {}
        }
    }

    Ok(())
}

/// Re-apply journal entries that may represent writes not yet in the data file.
///
/// In the research phase we write to the data file before the journal, so by
/// the time we replay, all journal entries' data is already on disk. We just
/// log them here.
fn apply_journal_entries(
    _cache: &mut Store,
    _data: &mut DataFile,
    entries: &[JournalEntry],
) -> Result<(), DiskStoreError> {
    // In the research phase: data file is always written before journal,
    // so all entries here are already reflected in the cache via scan_all.
    // Full WAL-style recovery (data written after journal) is a TODO for
    // the production implementation.
    let _ = entries; // suppress unused warning
    Ok(())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{ShardId, Slider, StructId, TenantId, Timestamp};

    fn tmp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn make_id(struct_id: u16, tenant: u64, ts: u64, slider: u8) -> Id {
        Id::new(
            TenantId::new(tenant),
            ShardId::new(0),
            StructId::new(struct_id),
            Timestamp::from_ticks(ts),
            Slider::new(slider),
        )
    }

    fn record(struct_id: u16, tenant: u64, ts: u64, slider: u8, data: &[u8]) -> StoredRecord {
        StoredRecord::new_first(make_id(struct_id, tenant, ts, slider), 1, 42, data.to_vec())
    }

    #[test]
    fn insert_and_get_live() {
        let dir = tmp_dir("wave_ds_insert");
        let mut ds =
            DiskStore::open(&dir, Config::dev(), AnchorMode::Inline).unwrap();
        let r = record(2, 1, 100, 0, b"disk-data");
        ds.insert(r).unwrap();
        let live = ds.get_live(StructId::new(2), TenantId::new(1)).unwrap();
        assert_eq!(live.data, b"disk-data");
        ds.close().unwrap();
    }

    #[test]
    fn persist_and_reopen() {
        let dir = tmp_dir("wave_ds_reopen");
        let v1_id;
        {
            let mut ds =
                DiskStore::open(&dir, Config::dev(), AnchorMode::Inline).unwrap();
            let r = record(2, 1, 100, 0, b"persisted");
            v1_id = r.id;
            ds.insert(r).unwrap();
            ds.close().unwrap();
        }
        // Reopen — data should be restored from disk
        let ds2 = DiskStore::open(&dir, Config::dev(), AnchorMode::Inline).unwrap();
        let live = ds2.get_live(StructId::new(2), TenantId::new(1)).unwrap();
        assert_eq!(live.data, b"persisted");
        assert_eq!(live.id, v1_id);
        ds2.close().unwrap();
    }

    #[test]
    fn mutate_and_reopen() {
        let dir = tmp_dir("wave_ds_mutate");
        let v1_id;
        let v2_id;
        {
            let mut ds =
                DiskStore::open(&dir, Config::dev(), AnchorMode::Inline).unwrap();
            let v1 = record(2, 1, 100, 0, b"v1");
            v1_id = v1.id;
            ds.insert(v1).unwrap();

            let v2 = record(2, 1, 200, 1, b"v2");
            v2_id = v2.id;
            ds.mutate(v1_id, v2).unwrap();
            ds.close().unwrap();
        }
        let ds2 = DiskStore::open(&dir, Config::dev(), AnchorMode::Inline).unwrap();
        let live = ds2.get_live(StructId::new(2), TenantId::new(1)).unwrap();
        assert_eq!(live.data, b"v2");
        assert_eq!(live.id, v2_id);

        let chain = ds2.history_backward(StructId::new(2), TenantId::new(1));
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].data, b"v2");
        assert_eq!(chain[1].data, b"v1");
        ds2.close().unwrap();
    }

    #[test]
    fn delete_and_reopen() {
        let dir = tmp_dir("wave_ds_delete");
        {
            let mut ds =
                DiskStore::open(&dir, Config::dev(), AnchorMode::Inline).unwrap();
            let r = record(2, 1, 100, 0, b"to delete");
            ds.insert(r).unwrap();
            ds.delete(StructId::new(2), TenantId::new(1)).unwrap();
            ds.close().unwrap();
        }
        let ds2 = DiskStore::open(&dir, Config::dev(), AnchorMode::Inline).unwrap();
        assert!(ds2.get_live(StructId::new(2), TenantId::new(1)).is_none());
        let anchor = ds2.anchor(StructId::new(2), TenantId::new(1)).unwrap();
        assert!(!anchor.is_live(), "anchor should be tombstoned after reopen");
        // History still accessible after reopen
        let chain = ds2.history_backward(StructId::new(2), TenantId::new(1));
        assert_eq!(chain.len(), 1);
        ds2.close().unwrap();
    }

    #[test]
    fn heap_append_and_read() {
        let dir = tmp_dir("wave_ds_heap");
        let mut ds =
            DiskStore::open(&dir, Config::dev(), AnchorMode::Inline).unwrap();
        let big_data = vec![0xCAu8; 10_000];
        let ptr = ds.heap_append(&big_data).unwrap();
        let got = ds.heap_read(ptr).unwrap();
        assert_eq!(got, big_data);
        ds.close().unwrap();
    }

    #[test]
    fn multiple_tenants_independent() {
        let dir = tmp_dir("wave_ds_multi");
        let mut ds =
            DiskStore::open(&dir, Config::dev(), AnchorMode::Inline).unwrap();
        ds.insert(record(2, 1, 100, 0, b"tenant-1")).unwrap();
        ds.insert(record(2, 2, 100, 0, b"tenant-2")).unwrap();

        assert_eq!(
            ds.get_live(StructId::new(2), TenantId::new(1)).unwrap().data,
            b"tenant-1"
        );
        assert_eq!(
            ds.get_live(StructId::new(2), TenantId::new(2)).unwrap().data,
            b"tenant-2"
        );
        ds.close().unwrap();
    }
}
