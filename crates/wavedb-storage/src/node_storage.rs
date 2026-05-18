//! Per-node on-disk storage bundle: data file + journal + heap.
//!
//! All three handles share one `data_dir`.  Wired into [`QuickNode`] at
//! construction time so every successful write goes through:
//!
//! 1. `journal.append(WriteVersioned { record })`
//! 2. `journal.flush()` — **WAL durability point**
//! 3. `data_file.write_nonunique_element(id, &record)`
//! 4. Only then return `Ok(())` to the caller
//!
//! Step 2 is what makes the write crash-safe.  If the process dies after
//! the flush but before step 3, recovery replays the journal entry to
//! rebuild the data-file state.
//!
//! [`QuickNode`]: ../../../wavedb_quick_node/struct.QuickNode.html

use crate::file::data::{DEFAULT_PAGE_SIZE, DataFile};
use crate::heap::HeapFile;
use crate::pipeline::journal::Journal;
use crate::{StorageError, StorageResult};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The three persistent storage files for one node.
///
/// All three are constructed by [`NodeStorage::open`] and rooted at the
/// same `data_dir`.  Cloning the [`Arc`]-wrapped fields is cheap; the
/// actual files are only opened once per node.
pub struct NodeStorage {
    /// Filesystem root holding `data.bin`, `journal.log`, and `heap.bin`.
    pub data_dir: PathBuf,
    /// Disk-backed hash-mapped page table.
    pub data_file: Arc<DataFile>,
    /// Append-only WAL.  All record mutations are journalled here before
    /// the data file is touched.
    pub journal: Arc<Mutex<Journal>>,
    /// Variable-length heap for blobs that don't fit inline.
    pub heap_file: Arc<Mutex<HeapFile>>,
}

impl std::fmt::Debug for NodeStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeStorage")
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

impl NodeStorage {
    /// Open (or create) the three storage files under `data_dir`.
    ///
    /// Directory is created if missing.  Empty files are materialised so
    /// callers can `assert!(path.exists())` right after open.
    pub fn open(data_dir: &Path) -> StorageResult<Arc<Self>> {
        std::fs::create_dir_all(data_dir)?;
        let data_file = DataFile::open_on_disk(&data_dir.join("data.bin"), DEFAULT_PAGE_SIZE)?;
        let journal = Journal::open(&data_dir.join("journal.log"))?;
        let heap_file = HeapFile::open(&data_dir.join("heap.bin"))?;
        data_file.flush()?;
        journal.flush()?;
        heap_file.flush()?;
        Ok(Arc::new(Self {
            data_dir: data_dir.to_path_buf(),
            data_file: Arc::new(data_file),
            journal: Arc::new(Mutex::new(journal)),
            heap_file: Arc::new(Mutex::new(heap_file)),
        }))
    }

    /// **Write-Ahead Logging commit point.**
    ///
    /// Appends `entry` to the journal, **fsyncs** the journal file, only
    /// then applies the in-memory + data-file state.  This is the only
    /// path a write should take — direct `DataFile::write_*` bypasses
    /// the WAL and is unsafe for production callers.
    ///
    /// # Errors
    ///
    /// Any failure before the journal `flush()` rolls back atomically.
    /// A failure *after* the journal commit returns the error to the
    /// caller; the journal entry is durable so recovery will replay it
    /// into the data file on the next open.
    pub fn commit_versioned_write(
        &self,
        record: &crate::versioned::VersionedRecord,
    ) -> StorageResult<()> {
        use crate::pipeline::journal::JournalEntry;

        // 1+2. Journal append + fsync.  After this returns Ok, the write
        //      is durable; before this returns Ok, nothing happened.
        {
            let mut j = self.journal.lock();
            j.append(JournalEntry::WriteVersioned {
                record: record.clone(),
            })?;
            j.flush()?;
        }

        // 3. Apply to the data file.  If this fails the entry is still
        //    in the journal — recovery will replay it.
        self.data_file.write_versioned(record)?;
        Ok(())
    }

    /// Convenience wrapper: build a `VersionedRecord` from `(id, bytes)`
    /// and commit it.  Used by request handlers that don't already have
    /// the assembled record.
    pub fn commit_write(&self, id: u128, payload: Vec<u8>) -> StorageResult<()> {
        let rec = crate::versioned::VersionedRecord::new(id, payload);
        self.commit_versioned_write(&rec)
    }

    /// Force-flush the data-file snapshot.  No-op for the journal and
    /// heap, which are flushed on every commit.
    pub fn flush_snapshot(&self) -> StorageResult<()> {
        self.data_file.flush()
    }

    /// Validate that the three files exist on disk.  Useful as an
    /// invariant assertion in tests.
    pub fn assert_files_exist(&self) -> StorageResult<()> {
        for name in ["data.bin", "journal.log", "heap.bin"] {
            let p = self.data_dir.join(name);
            if !p.exists() {
                return Err(StorageError::Other(format!(
                    "expected file missing: {}",
                    p.display()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_materialises_files_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let storage = NodeStorage::open(dir.path()).unwrap();
        storage.assert_files_exist().unwrap();
    }

    #[test]
    fn commit_durable_after_journal_flush() {
        let dir = tempfile::tempdir().unwrap();
        let storage = NodeStorage::open(dir.path()).unwrap();

        let id = wavedb_core::Id::new(7, 0, 11, 42);
        storage.commit_write(id.raw(), b"hello".to_vec()).unwrap();

        // Journal file grew (non-empty after fsync).
        let journal_size = std::fs::metadata(dir.path().join("journal.log"))
            .unwrap()
            .len();
        assert!(
            journal_size > 0,
            "journal must be non-empty after a committed write"
        );

        // Record present in the data-file in-memory state.
        assert!(storage.data_file.read_versioned(id).unwrap().is_some());
    }
}
