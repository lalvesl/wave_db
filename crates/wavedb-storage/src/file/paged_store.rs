//! `PagedStore` — the assembled per-node store for the new model.
//!
//! Ties the pieces together: a [`BlockFile`] (the `data` file), a
//! [`PageJournal`] (durable write log + directory deltas), and one
//! [`PageDirectory`] per `(struct_id, version)`.
//!
//! # Write path (journal-first)
//!
//! A `put`/`remove` does **no** `data.bin` work on the hot path:
//!
//! 1. The record is appended to the **journal** (durable).
//! 2. It is held in an in-memory **pending** buffer.
//! 3. The caller is acked "saved".
//!
//! Reads overlay the pending buffer on top of the committed pages, so a record
//! is visible the instant it is written.
//!
//! # Drain (journal → data.bin), copy-on-write
//!
//! [`drain`](PagedStore::drain) settles pending records into pages. For each
//! page that has pending records it:
//!
//! 1. reads the current committed page,
//! 2. merges the pending upserts/deletes,
//! 3. **allocates a fresh page**, writes it, and **commits** by swapping the
//!    directory descriptor — then frees the old page.
//!
//! Nothing is mutated in place, so a crash mid-drain leaves every page intact;
//! recovery replays the still-pending records. The drain syncs `data.bin`
//! before journaling the descriptor commits, then writes a checkpoint so the
//! drained records are reclaimable.
//!
//! # Recovery
//!
//! [`open`](PagedStore::open) replays the journal: committed directories come
//! from the descriptor/resize deltas, the pending buffer from the records
//! since the last checkpoint, and the [`BlockFile`] allocator from the live
//! descriptors.

use crate::file::block_file::BlockFile;
use crate::file::directory::PageDirectory;
use crate::file::page_dir::PageDescriptor;
use crate::file::page_journal::PageJournal;
use crate::StorageResult;
use std::collections::BTreeMap;
use std::path::Path;

/// Slots a freshly-seen `(struct_id, version)` directory starts with.
pub const DEFAULT_DIRECTORY_SLOTS: usize = 8;

const DATA_FILE: &str = "data.bin";
const JOURNAL_FILE: &str = "page.journal";

type Mutations = BTreeMap<u128, Option<Vec<u8>>>;

fn u32_of(n: usize) -> u32 {
    u32::try_from(n).expect("directory index exceeds u32")
}

/// The per-node paged store. See the module docs.
pub struct PagedStore {
    data: BlockFile,
    journal: PageJournal,
    /// Committed directories (pages settled in `data.bin`).
    dirs: BTreeMap<(u32, u8), PageDirectory>,
    /// Records written but not yet drained. `Some` = upsert, `None` = delete.
    pending: BTreeMap<(u32, u8), Mutations>,
    default_slots: usize,
    drain_seq: u64,
}

impl PagedStore {
    /// An entirely in-memory store (tests, ephemeral caches).
    #[must_use]
    pub fn create_in_memory() -> Self {
        Self {
            data: BlockFile::create_in_memory(),
            journal: PageJournal::create_in_memory(),
            dirs: BTreeMap::new(),
            pending: BTreeMap::new(),
            default_slots: DEFAULT_DIRECTORY_SLOTS,
            drain_seq: 0,
        }
    }

    /// Create a fresh store under `dir` (`data.bin` + `page.journal`).
    pub fn create(dir: &Path) -> StorageResult<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            data: BlockFile::create(&dir.join(DATA_FILE))?,
            journal: PageJournal::create(&dir.join(JOURNAL_FILE))?,
            dirs: BTreeMap::new(),
            pending: BTreeMap::new(),
            default_slots: DEFAULT_DIRECTORY_SLOTS,
            drain_seq: 0,
        })
    }

    /// Open an existing store under `dir`, replaying the journal to rebuild
    /// the committed directories, the pending buffer, and the allocator.
    pub fn open(dir: &Path) -> StorageResult<Self> {
        let (journal, state) = PageJournal::open(&dir.join(JOURNAL_FILE))?;
        let data = BlockFile::open(&dir.join(DATA_FILE), state.data_runs())?;
        let dirs = state
            .directories
            .into_iter()
            .map(|(key, slots)| (key, PageDirectory::from_slots(slots)))
            .collect();
        Ok(Self {
            data,
            journal,
            dirs,
            pending: state.pending,
            default_slots: DEFAULT_DIRECTORY_SLOTS,
            drain_seq: 0,
        })
    }

    /// Override the slot count new directories are created with. Mainly for
    /// tuning / tests.
    pub fn set_default_slots(&mut self, slots: usize) {
        assert!(slots > 0, "default slots must be positive");
        self.default_slots = slots;
    }

    /// Total records held in the pending buffer (not yet drained).
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.values().map(BTreeMap::len).sum()
    }

    /// Look up a record — pending buffer first, then the committed page.
    pub fn get(
        &self,
        struct_id: u32,
        version: u8,
        id: u128,
    ) -> StorageResult<Option<Vec<u8>>> {
        if let Some(mutation) =
            self.pending.get(&(struct_id, version)).and_then(|t| t.get(&id))
        {
            // `Some(bytes)` upsert or `None` tombstone — both authoritative.
            return Ok(mutation.clone());
        }
        self.dirs
            .get(&(struct_id, version))
            .map_or(Ok(None), |dir| dir.get(&self.data, id))
    }

    /// Write a record: journal it, buffer it, and return (acked "saved").
    /// `data.bin` is untouched until [`drain`](Self::drain).
    pub fn put(
        &mut self,
        struct_id: u32,
        version: u8,
        id: u128,
        payload: &[u8],
    ) -> StorageResult<()> {
        self.journal.write_record(struct_id, version, id, payload)?;
        self.pending
            .entry((struct_id, version))
            .or_default()
            .insert(id, Some(payload.to_vec()));
        Ok(())
    }

    /// Delete a record (a tombstone, drained like any write).
    pub fn remove(
        &mut self,
        struct_id: u32,
        version: u8,
        id: u128,
    ) -> StorageResult<()> {
        self.journal.delete_record(struct_id, version, id)?;
        self.pending
            .entry((struct_id, version))
            .or_default()
            .insert(id, None);
        Ok(())
    }

    /// Settle the pending buffer into `data.bin` pages, copy-on-write.
    pub fn drain(&mut self) -> StorageResult<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let default_slots = self.default_slots;
        let pending = std::mem::take(&mut self.pending);

        let mut resizes: Vec<(u32, u8, usize)> = Vec::new();
        let mut commits: Vec<(u32, u8, usize, PageDescriptor)> = Vec::new();

        for ((struct_id, version), muts) in pending {
            let key = (struct_id, version);
            let is_new = !self.dirs.contains_key(&key);
            let dir = self
                .dirs
                .entry(key)
                .or_insert_with(|| PageDirectory::new(default_slots));
            if is_new {
                resizes.push((struct_id, version, dir.slot_count()));
            }
            // Group this type's pending records by their target page.
            let mut by_slot: BTreeMap<usize, Mutations> = BTreeMap::new();
            for (id, mutation) in muts {
                by_slot.entry(dir.slot_of(id)).or_default().insert(id, mutation);
            }
            for (slot, slot_muts) in by_slot {
                let descriptor =
                    dir.commit_slot(&self.data, slot, &slot_muts)?;
                commits.push((struct_id, version, slot, descriptor));
            }
        }

        // Durability order: the new pages must be on disk before the
        // descriptors that point at them, and the checkpoint last.
        self.data.sync()?;
        for (struct_id, version, slot_count) in resizes {
            self.journal
                .resize_directory(struct_id, version, u32_of(slot_count))?;
        }
        for (struct_id, version, slot, descriptor) in commits {
            self.journal
                .set_descriptor(struct_id, version, u32_of(slot), descriptor)?;
        }
        self.drain_seq += 1;
        self.journal.checkpoint(self.drain_seq)?;
        self.journal.sync()?;
        Ok(())
    }

    /// Lengthen a type's (committed) directory to `new_slot_count`, rehashing
    /// its records copy-on-write, and journal the new layout. Returns `false`
    /// if the type has no committed directory yet.
    pub fn grow_directory(
        &mut self,
        struct_id: u32,
        version: u8,
        new_slot_count: usize,
    ) -> StorageResult<bool> {
        let Some(dir) = self.dirs.get_mut(&(struct_id, version)) else {
            return Ok(false);
        };
        dir.grow_to(&self.data, new_slot_count)?;
        let slot_count = dir.slot_count();
        let descriptors: Vec<_> = dir.descriptors().to_vec();

        self.data.sync()?;
        self.journal
            .resize_directory(struct_id, version, u32_of(slot_count))?;
        for (i, descriptor) in descriptors.into_iter().enumerate() {
            self.journal
                .set_descriptor(struct_id, version, u32_of(i), descriptor)?;
        }
        self.journal.sync()
            .map(|()| true)
    }

    /// Grow a type's directory (doubling it) if [`PageDirectory::needs_grow`]
    /// says it should. Returns whether a grow happened.
    pub fn maybe_grow(
        &mut self,
        struct_id: u32,
        version: u8,
    ) -> StorageResult<bool> {
        let key = (struct_id, version);
        let should = self.dirs.get(&key).is_some_and(PageDirectory::needs_grow);
        if !should {
            return Ok(false);
        }
        let target = self.dirs[&key].slot_count().saturating_mul(2);
        self.grow_directory(struct_id, version, target)
    }

    /// Flush `data.bin` and the journal to disk.
    pub fn sync(&self) -> StorageResult<()> {
        self.data.sync()?;
        self.journal.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn pending_put_get_remove() {
        let mut store = PagedStore::create_in_memory();
        store.put(7, 2, 1, b"hello").unwrap();
        store.put(8, 1, 1, b"other-type").unwrap();
        assert_eq!(store.get(7, 2, 1).unwrap().unwrap(), b"hello");
        assert_eq!(store.get(8, 1, 1).unwrap().unwrap(), b"other-type");
        assert_eq!(store.get(7, 2, 2).unwrap(), None);

        store.put(7, 2, 1, b"hello-v2").unwrap();
        assert_eq!(store.get(7, 2, 1).unwrap().unwrap(), b"hello-v2");
        store.remove(7, 2, 1).unwrap();
        assert_eq!(store.get(7, 2, 1).unwrap(), None);
        assert!(store.pending_len() >= 1); // tombstone still pending
    }

    #[test]
    fn drain_commits_and_clears_pending() {
        let mut store = PagedStore::create_in_memory();
        for id in 0..20u128 {
            store.put(7, 2, id, format!("v{id}").as_bytes()).unwrap();
        }
        assert_eq!(store.pending_len(), 20);
        store.drain().unwrap();
        assert_eq!(store.pending_len(), 0);
        // Reads now come from committed pages.
        for id in 0..20u128 {
            assert_eq!(
                store.get(7, 2, id).unwrap().unwrap(),
                format!("v{id}").as_bytes()
            );
        }
    }

    #[test]
    fn pending_overlays_committed() {
        let mut store = PagedStore::create_in_memory();
        store.put(7, 2, 1, b"committed").unwrap();
        store.drain().unwrap();
        // New write sits in pending and shadows the committed page.
        store.put(7, 2, 1, b"shadow").unwrap();
        assert_eq!(store.get(7, 2, 1).unwrap().unwrap(), b"shadow");
        store.drain().unwrap();
        assert_eq!(store.get(7, 2, 1).unwrap().unwrap(), b"shadow");
    }

    #[test]
    fn reopen_recovers_undrained_pending() {
        let dir = tempdir().unwrap();
        {
            let mut store = PagedStore::create(dir.path()).unwrap();
            store.put(7, 2, 1, b"unsettled").unwrap();
            store.put(7, 2, 2, b"also").unwrap();
            store.sync().unwrap(); // journal durable, never drained
        }
        let store = PagedStore::open(dir.path()).unwrap();
        // Recovered as pending records.
        assert_eq!(store.pending_len(), 2);
        assert_eq!(store.get(7, 2, 1).unwrap().unwrap(), b"unsettled");
        assert_eq!(store.get(7, 2, 2).unwrap().unwrap(), b"also");
    }

    #[test]
    fn reopen_recovers_drained_committed() {
        let dir = tempdir().unwrap();
        {
            let mut store = PagedStore::create(dir.path()).unwrap();
            for id in 0..30u128 {
                store.put(7, 2, id, format!("rec{id}").as_bytes()).unwrap();
            }
            store.put(3, 1, 999, b"unique").unwrap();
            store.drain().unwrap(); // settle into data.bin
            store.remove(7, 2, 0).unwrap();
            store.drain().unwrap(); // settle the delete
            store.sync().unwrap();
        }
        let store = PagedStore::open(dir.path()).unwrap();
        assert_eq!(store.pending_len(), 0); // all checkpointed
        assert_eq!(store.get(7, 2, 0).unwrap(), None); // stayed deleted
        for id in 1..30u128 {
            assert_eq!(
                store.get(7, 2, id).unwrap().unwrap(),
                format!("rec{id}").as_bytes()
            );
        }
        assert_eq!(store.get(3, 1, 999).unwrap().unwrap(), b"unique");
    }

    #[test]
    fn grow_after_drain_persists() {
        let dir = tempdir().unwrap();
        {
            let mut store = PagedStore::create(dir.path()).unwrap();
            store.set_default_slots(1); // one bucket → big page
            let payload = vec![0x7Eu8; 200];
            for id in 0..30u128 {
                store.put(7, 2, id, &payload).unwrap();
            }
            store.drain().unwrap();
            assert!(store.grow_directory(7, 2, 16).unwrap());
            for id in 0..30u128 {
                assert_eq!(store.get(7, 2, id).unwrap().unwrap(), payload);
            }
            store.sync().unwrap();
        }
        let store = PagedStore::open(dir.path()).unwrap();
        let payload = vec![0x7Eu8; 200];
        for id in 0..30u128 {
            assert_eq!(store.get(7, 2, id).unwrap().unwrap(), payload);
        }
    }

    #[test]
    fn maybe_grow_noop_when_small() {
        let mut store = PagedStore::create_in_memory();
        store.put(7, 2, 1, b"x").unwrap();
        store.drain().unwrap();
        assert!(!store.maybe_grow(7, 2).unwrap());
        assert!(!store.maybe_grow(9, 9).unwrap());
    }
}
