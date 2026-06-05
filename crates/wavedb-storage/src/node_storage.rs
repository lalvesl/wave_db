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
    ///
    /// # Crash recovery
    ///
    /// `data.bin` holds the last *snapshot* (written by [`Self::compact_journal`]
    /// or [`Self::flush_snapshot`]); `journal.log` holds every committed write
    /// **after** that snapshot.  On open the journal is replayed into the data
    /// file so writes made since the last snapshot are visible again — this is
    /// what makes a write durable across a process crash, not just across a
    /// clean shutdown.  After replay the recovered state is snapshotted and the
    /// journal is truncated so a subsequent restart never re-applies (and
    /// duplicates) the same entries.
    pub fn open(data_dir: &Path) -> StorageResult<Arc<Self>> {
        std::fs::create_dir_all(data_dir)?;
        let data_file = DataFile::open_on_disk(&data_dir.join("data.bin"), DEFAULT_PAGE_SIZE)?;
        let journal = Journal::open(&data_dir.join("journal.log"))?;
        let heap_file = HeapFile::open(&data_dir.join("heap.bin"))?;

        let storage = Arc::new(Self {
            data_dir: data_dir.to_path_buf(),
            data_file: Arc::new(data_file),
            journal: Arc::new(Mutex::new(journal)),
            heap_file: Arc::new(Mutex::new(heap_file)),
        });

        // Replay journalled writes that never made it into `data.bin` before
        // the previous process exited.  No-op for a fresh or already-compacted
        // journal.
        storage.recover_from_journal()?;

        storage.data_file.flush()?;
        storage.journal.lock().flush()?;
        storage.heap_file.lock().flush()?;
        Ok(storage)
    }

    /// Replay journalled mutations into the in-memory data file, then snapshot
    /// and truncate so the recovered state is durable in `data.bin` and the
    /// journal is reset.
    ///
    /// Only [`JournalEntry::WriteVersioned`] is produced by any live write path
    /// (request commit, rebalance relocation, drain), so that is the only
    /// variant that must be re-applied to reconstruct queryable state.  The
    /// remaining variants describe free-space / dictionary bookkeeping and need
    /// no data-file mutation on recovery.
    fn recover_from_journal(&self) -> StorageResult<()> {
        use crate::pipeline::journal::JournalEntry;

        // Snapshot the entries under the lock, then release it before touching
        // the data file (whose writes may rebalance and take their own locks).
        let entries = {
            let j = self.journal.lock();
            if j.is_empty() {
                return Ok(());
            }
            j.all_entries().to_vec()
        };

        let mut replayed = 0usize;
        for entry in &entries {
            if let JournalEntry::WriteVersioned { record } = entry {
                self.data_file.write_versioned(record)?;
                replayed += 1;
            }
        }

        if replayed > 0 {
            // Persist the reconstructed state, then checkpoint + truncate so the
            // next open starts from a clean snapshot with an empty journal.
            self.data_file.flush()?;
            let mut j = self.journal.lock();
            let seq = j.checkpoint()?;
            j.truncate_through(seq)?;
        }
        Ok(())
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

    /// Compact the journal: flush data.bin, checkpoint, then truncate.
    ///
    /// Order matters for crash safety:
    /// 1. `data_file.flush()` — write the current page-table snapshot to
    ///    `data.bin`.  After this, every committed write is recoverable from
    ///    `data.bin` alone.
    /// 2. Checkpoint the journal — mark the current head as durable.
    /// 3. Truncate the journal — drop in-memory entries and rewrite the
    ///    on-disk file via atomic rename so the file is never empty.
    ///
    /// Skipping step 1 would be unsafe: if we removed journal entries whose
    /// corresponding `data.bin` pages were not yet flushed, a crash would
    /// lose those writes.
    pub fn compact_journal(&self) -> StorageResult<()> {
        self.data_file.flush()?;
        let mut j = self.journal.lock();
        let seq = j.checkpoint()?;
        j.truncate_through(seq)
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

    #[test]
    fn journal_replays_unsnapshotted_writes_on_reopen() {
        // The WAL durability contract: a write that returned Ok survives a
        // crash even when no snapshot ran between the write and the crash.
        let dir = tempfile::tempdir().unwrap();

        let ids: Vec<wavedb_core::Id> = (1..=128)
            .map(|seq| wavedb_core::Id::new(7, 0, 11, seq))
            .collect();

        {
            let storage = NodeStorage::open(dir.path()).unwrap();
            for id in &ids {
                storage
                    .commit_write(
                        id.raw(),
                        format!("payload-{}", id.created_at()).into_bytes(),
                    )
                    .unwrap();
            }
            // NB: deliberately NO flush_snapshot()/compact_journal() — this is
            // the "crash before the next snapshot" window.  Drop the handle to
            // simulate the process exiting.
        }

        // Reopen the same directory — recovery must rebuild every committed
        // record from the journal.
        let reopened = NodeStorage::open(dir.path()).unwrap();
        for id in &ids {
            let rec = reopened.data_file.read_versioned(*id).unwrap();
            assert!(
                rec.is_some(),
                "id created_at={} lost after reopen",
                id.created_at()
            );
            assert_eq!(
                rec.unwrap().data,
                format!("payload-{}", id.created_at()).into_bytes()
            );
        }
    }

    #[test]
    fn double_reopen_does_not_duplicate_or_lose_records() {
        // Recovery truncates the journal after replay, so a second restart
        // must neither replay stale entries nor drop the recovered state.
        let dir = tempfile::tempdir().unwrap();
        let id = wavedb_core::Id::new(3, 0, 9, 99);

        {
            let storage = NodeStorage::open(dir.path()).unwrap();
            storage.commit_write(id.raw(), b"once".to_vec()).unwrap();
        }
        // First reopen recovers from the journal and re-snapshots.
        let _ = NodeStorage::open(dir.path()).unwrap();
        // Second reopen reads the snapshot with an empty journal.
        let again = NodeStorage::open(dir.path()).unwrap();
        let rec = again.data_file.read_versioned(id).unwrap();
        assert_eq!(rec.unwrap().data, b"once");
    }
}
