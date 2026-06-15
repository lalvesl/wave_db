//! `PagedStore` — the assembled per-node store for the new model.
//!
//! Ties the pieces together: a [`BlockFile`] (the `data` file), a
//! [`PageJournal`] (durable directory deltas + page staging), and one
//! [`PageDirectory`] per `(struct_id, version)`.
//!
//! ```text
//!   put/get/remove ──► PageDirectory[(sid,ver)] ──► BlockFile (data.bin)
//!         │                                            ▲
//!         └── descriptor / resize deltas ──► PageJournal ──┘ replay on open
//! ```
//!
//! # Durability & recovery
//!
//! Pages are written into `data.bin` by the directory; the **journal** records
//! every directory mutation (a slot's `u64` descriptor, a directory resize).
//! On [`open`](PagedStore::open) the journal replays to rebuild the
//! directories, and the [`BlockFile`] allocator is reconstructed from the live
//! descriptors ([`ReplayState::data_runs`]). Call [`sync`](PagedStore::sync)
//! to make a batch durable.
//!
//! This is the **direct-to-data** path: a page lives in `data.bin` as soon as
//! it is written. Staging pages in the journal (the descriptor's `in_journal`
//! flag) for transition-time relocation is a forthcoming layer on top — the
//! flag and [`PageJournal::stage_page`] already exist for it.
//!
//! [`ReplayState::data_runs`]: crate::file::page_journal::ReplayState::data_runs
//! [`PageJournal::stage_page`]: crate::file::page_journal::PageJournal::stage_page

use crate::file::block_file::BlockFile;
use crate::file::directory::PageDirectory;
use crate::file::page_journal::PageJournal;
use crate::StorageResult;
use std::collections::BTreeMap;
use std::path::Path;

/// Slots a freshly-seen `(struct_id, version)` directory starts with.
pub const DEFAULT_DIRECTORY_SLOTS: usize = 8;

const DATA_FILE: &str = "data.bin";
const JOURNAL_FILE: &str = "page.journal";

fn u32_of(n: usize) -> u32 {
    u32::try_from(n).expect("directory index exceeds u32")
}

/// The per-node paged store. See the module docs.
pub struct PagedStore {
    data: BlockFile,
    journal: PageJournal,
    dirs: BTreeMap<(u32, u8), PageDirectory>,
    default_slots: usize,
}

impl PagedStore {
    /// An entirely in-memory store (tests, ephemeral caches).
    #[must_use]
    pub fn create_in_memory() -> Self {
        Self {
            data: BlockFile::create_in_memory(),
            journal: PageJournal::create_in_memory(),
            dirs: BTreeMap::new(),
            default_slots: DEFAULT_DIRECTORY_SLOTS,
        }
    }

    /// Create a fresh store under `dir` (`data.bin` + `page.journal`).
    pub fn create(dir: &Path) -> StorageResult<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            data: BlockFile::create(&dir.join(DATA_FILE))?,
            journal: PageJournal::create(&dir.join(JOURNAL_FILE))?,
            dirs: BTreeMap::new(),
            default_slots: DEFAULT_DIRECTORY_SLOTS,
        })
    }

    /// Open an existing store under `dir`, replaying the journal to rebuild
    /// the directories and the block allocator.
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
            default_slots: DEFAULT_DIRECTORY_SLOTS,
        })
    }

    /// Override the slot count new directories are created with (before any
    /// data is written for that type). Mainly for tuning / tests.
    pub fn set_default_slots(&mut self, slots: usize) {
        assert!(slots > 0, "default slots must be positive");
        self.default_slots = slots;
    }

    /// Look up a record. `Ok(None)` if its type or the record is absent.
    pub fn get(
        &self,
        struct_id: u32,
        version: u8,
        id: u128,
    ) -> StorageResult<Option<Vec<u8>>> {
        self.dirs
            .get(&(struct_id, version))
            .map_or(Ok(None), |dir| dir.get(&self.data, id))
    }

    /// Insert or replace a record, creating the type's directory on first use.
    pub fn put(
        &mut self,
        struct_id: u32,
        version: u8,
        id: u128,
        payload: &[u8],
    ) -> StorageResult<()> {
        let key = (struct_id, version);
        let is_new = !self.dirs.contains_key(&key);
        let default_slots = self.default_slots;
        let dir = self
            .dirs
            .entry(key)
            .or_insert_with(|| PageDirectory::new(default_slots));

        let slot = dir.slot_of(id);
        dir.put(&self.data, id, payload)?;
        let descriptor = dir.descriptors()[slot];
        let slot_count = dir.slot_count();

        if is_new {
            self.journal
                .resize_directory(struct_id, version, u32_of(slot_count))?;
        }
        self.journal
            .set_descriptor(struct_id, version, u32_of(slot), descriptor)?;
        Ok(())
    }

    /// Remove a record. Returns whether it was present.
    pub fn remove(
        &mut self,
        struct_id: u32,
        version: u8,
        id: u128,
    ) -> StorageResult<bool> {
        let Some(dir) = self.dirs.get_mut(&(struct_id, version)) else {
            return Ok(false);
        };
        let slot = dir.slot_of(id);
        let removed = dir.remove(&self.data, id)?;
        if removed {
            let descriptor = dir.descriptors()[slot];
            self.journal
                .set_descriptor(struct_id, version, u32_of(slot), descriptor)?;
        }
        Ok(removed)
    }

    /// Lengthen a type's directory to `new_slot_count`, rehashing its records,
    /// and journal the new layout. Returns `false` if the type is unknown.
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
        // Snapshot the new layout, then journal it (drops the `dir` borrow).
        let slot_count = dir.slot_count();
        let descriptors: Vec<_> = dir.descriptors().to_vec();

        self.journal
            .resize_directory(struct_id, version, u32_of(slot_count))?;
        for (i, descriptor) in descriptors.into_iter().enumerate() {
            self.journal
                .set_descriptor(struct_id, version, u32_of(i), descriptor)?;
        }
        Ok(true)
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
    fn put_get_remove_in_memory() {
        let mut store = PagedStore::create_in_memory();
        store.put(7, 2, 1, b"hello").unwrap();
        store.put(7, 2, 2, b"world").unwrap();
        // Different type, same id — isolated.
        store.put(8, 1, 1, b"other-type").unwrap();

        assert_eq!(store.get(7, 2, 1).unwrap().unwrap(), b"hello");
        assert_eq!(store.get(7, 2, 2).unwrap().unwrap(), b"world");
        assert_eq!(store.get(8, 1, 1).unwrap().unwrap(), b"other-type");
        assert_eq!(store.get(7, 2, 3).unwrap(), None);
        assert_eq!(store.get(9, 9, 1).unwrap(), None); // unknown type

        // Update then delete.
        store.put(7, 2, 1, b"hello-v2").unwrap();
        assert_eq!(store.get(7, 2, 1).unwrap().unwrap(), b"hello-v2");
        assert!(store.remove(7, 2, 1).unwrap());
        assert_eq!(store.get(7, 2, 1).unwrap(), None);
        assert!(!store.remove(7, 2, 1).unwrap());
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempdir().unwrap();
        {
            let mut store = PagedStore::create(dir.path()).unwrap();
            for id in 0..50u128 {
                store.put(7, 2, id, format!("record-{id}").as_bytes()).unwrap();
            }
            store.put(3, 1, 999, b"unique-ish").unwrap();
            store.remove(7, 2, 0).unwrap(); // delete one
            store.sync().unwrap();
        }

        let store = PagedStore::open(dir.path()).unwrap();
        assert_eq!(store.get(7, 2, 0).unwrap(), None); // stayed deleted
        for id in 1..50u128 {
            assert_eq!(
                store.get(7, 2, id).unwrap().unwrap(),
                format!("record-{id}").as_bytes()
            );
        }
        assert_eq!(store.get(3, 1, 999).unwrap().unwrap(), b"unique-ish");
    }

    #[test]
    fn grow_directory_persists() {
        let dir = tempdir().unwrap();
        {
            let mut store = PagedStore::create(dir.path()).unwrap();
            store.set_default_slots(1); // one bucket → big page, forces churn
            let payload = vec![0x7Eu8; 300];
            for id in 0..30u128 {
                store.put(7, 2, id, &payload).unwrap();
            }
            assert!(store.grow_directory(7, 2, 16).unwrap());
            // Data intact immediately after grow.
            for id in 0..30u128 {
                assert_eq!(store.get(7, 2, id).unwrap().unwrap(), payload);
            }
            store.sync().unwrap();
        }
        // And after replaying the resize + new descriptors.
        let store = PagedStore::open(dir.path()).unwrap();
        let payload = vec![0x7Eu8; 300];
        for id in 0..30u128 {
            assert_eq!(store.get(7, 2, id).unwrap().unwrap(), payload);
        }
    }

    #[test]
    fn maybe_grow_is_noop_when_small() {
        let mut store = PagedStore::create_in_memory();
        store.put(7, 2, 1, b"x").unwrap();
        assert!(!store.maybe_grow(7, 2).unwrap());
        assert!(!store.maybe_grow(9, 9).unwrap()); // unknown type
    }
}
