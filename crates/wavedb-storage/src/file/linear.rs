//! Linear-hashing page directory.
//!
//! Keeps the predictable, O(1) bit-mask routing of power-of-two hashing, but
//! grows **one bucket at a time** — pick any increment (25%, 10%), never
//! double. The directory is a plain append-only `Vec<PageDescriptor>`; a split
//! `push`es one slot.
//!
//! # Routing — two masks + a split pointer
//!
//! Everything derives from the bucket count `M = dir.len()`:
//!
//! ```text
//! L    = floor(log2 M)        // level
//! base = 1 << L               // 2^L
//! s    = M - base             // split pointer: buckets [0, s) already split
//!
//! b = hash & (base - 1)       // low L bits
//! if b < s { b = hash & (2*base - 1) }   // already-split bucket → one more bit
//! ```
//!
//! Two masks and a compare — no disk, no `local_depths` array. `s` and `L` are
//! computed from `M`, not stored.
//!
//! # Split (the only growth)
//!
//! [`split_next`](LinearDirectory::split_next) splits bucket `s`: partition its
//! records by bit `L` — bit 0 stays in `s`, bit 1 moves to the appended bucket
//! `s + base` (= `M`) — copy-on-write, free the old page, `push` the new
//! descriptor. `M` grows by one; `s`/`L` advance automatically (when `s`
//! reaches `base`, `M == 2*base` so the level bumps).
//!
//! Only bucket `s`'s keys move (~1/M of all keys); a key's low `L` bits are a
//! stable address until *its* bucket is split. Splits go round-robin by `s`
//! (not most-full-first); a hot bucket rides a bigger page (copy-on-write
//! block growth) until its turn.
//!
//! Persistence (journaling the descriptors; `M`/`s`/`L` follow from the array)
//! is the integration step; this module is the in-memory + [`BlockFile`]
//! mechanism.

// Routing is bit manipulation: u64 hash ↔ usize index, masks, shifts.
#![allow(clippy::cast_possible_truncation)]

use crate::StorageResult;
use crate::file::block_alloc::BlockRun;
use crate::file::block_file::BlockFile;
use crate::file::dict::{DictRef, DictStore};
use crate::file::directory::{SlotPage, place_page};
use crate::file::page_dir::PageDescriptor;

/// Largest starting level (`new(level)` allocates `2^level` buckets).
pub const MAX_LEVEL: u8 = 32;

/// Hash a record id to the 64-bit space the directory indexes.
const fn hash_of(id: u128) -> u64 {
    let lo = id as u64;
    let hi = (id >> 64) as u64;
    crate::hash::mix64(lo ^ hi.rotate_left(32))
}

/// A linear-hashing directory over a shared [`BlockFile`].
pub struct LinearDirectory {
    struct_id: u32,
    version: u8,
    /// Dictionary cache + current ref (`current() == NONE` ⇒ pages stored raw).
    dicts: DictStore,
    /// Length `M` is the live bucket count; `s` and `L` are derived from it.
    dir: Vec<PageDescriptor>,
}

impl LinearDirectory {
    /// A directory of `2^level` empty buckets for type `(struct_id, version)`
    /// (a power-of-two base, so the split pointer starts at 0).
    ///
    /// # Panics
    ///
    /// Panics if `level > MAX_LEVEL`.
    #[must_use]
    pub fn new(struct_id: u32, version: u8, level: u8) -> Self {
        assert!(level <= MAX_LEVEL, "level too large");
        Self {
            struct_id,
            version,
            dicts: DictStore::new(),
            dir: vec![PageDescriptor::EMPTY; 1usize << level],
        }
    }

    /// Rebuild from recovered descriptors (e.g. journal replay). `M`/`s`/`L`
    /// follow from the length.
    ///
    /// # Panics
    ///
    /// Panics if `dir` is empty.
    #[must_use]
    pub fn from_buckets(
        struct_id: u32,
        version: u8,
        dir: Vec<PageDescriptor>,
    ) -> Self {
        assert!(!dir.is_empty(), "directory needs at least one bucket");
        Self {
            struct_id,
            version,
            dicts: DictStore::new(),
            dir,
        }
    }

    /// Train-and-install a dictionary (written to `data.bin`). Returns its
    /// [`DictRef`]; the store journals the run's allocation through the ledger.
    pub fn install_dictionary(
        &mut self,
        file: &BlockFile,
        dict: &[u8],
    ) -> StorageResult<DictRef> {
        self.dicts.install(file, dict)
    }

    /// Decode the page at `run`, resolving its header `dict_ref` via the cache.
    fn read_run_bucket(
        &self,
        file: &BlockFile,
        run: BlockRun,
    ) -> StorageResult<SlotPage> {
        let bytes = file.read_run(run)?;
        let dict_ref = SlotPage::dict_ref_of(&bytes)?;
        if dict_ref.is_none() {
            SlotPage::decode(&bytes, None)
        } else {
            let dict = self.dicts.load(file, dict_ref)?;
            SlotPage::decode(&bytes, Some(&dict))
        }
    }

    /// Read bucket `desc`, or a fresh empty page if unallocated.
    fn read_bucket(
        &self,
        file: &BlockFile,
        desc: PageDescriptor,
    ) -> StorageResult<SlotPage> {
        if desc.is_allocated() {
            self.read_run_bucket(file, desc.run())
        } else {
            Ok(SlotPage::new(self.struct_id, self.version))
        }
    }

    /// Place `page` (compressing against the current dict), or `EMPTY` if empty.
    fn place_if_nonempty(
        &self,
        file: &BlockFile,
        page: &SlotPage,
    ) -> StorageResult<PageDescriptor> {
        if page.is_empty() {
            return Ok(PageDescriptor::EMPTY);
        }
        let dict_ref = self.dicts.current();
        if dict_ref.is_none() {
            place_page(file, page, None)
        } else {
            let bytes = self.dicts.load(file, dict_ref)?;
            place_page(file, page, Some((&bytes, dict_ref)))
        }
    }

    /// Number of buckets `M`.
    #[must_use]
    pub fn bucket_count(&self) -> usize {
        self.dir.len()
    }

    /// Current level `L = floor(log2 M)`.
    #[must_use]
    pub fn level(&self) -> u32 {
        (self.dir.len() as u64).ilog2()
    }

    /// Current split pointer `s = M - 2^L` (next bucket to split).
    #[must_use]
    pub fn split_pointer(&self) -> usize {
        let m = self.dir.len() as u64;
        (m - (1u64 << m.ilog2())) as usize
    }

    /// The descriptor at bucket `i`.
    #[must_use]
    pub fn descriptor(&self, i: usize) -> PageDescriptor {
        self.dir[i]
    }

    /// Bucket a record routes to — two masks + the split pointer.
    #[must_use]
    pub fn index(&self, id: u128) -> usize {
        let m = self.dir.len() as u64;
        let level = m.ilog2();
        let base = 1u64 << level;
        let s = m - base;
        let h = hash_of(id);
        let mut b = h & (base - 1);
        if b < s {
            b = h & ((base << 1) - 1);
        }
        b as usize
    }

    /// Block runs of every allocated bucket — feed to [`BlockFile::open`] to
    /// rebuild the allocator. (No sharing in linear hashing, so no dedup.)
    #[must_use]
    pub fn used_runs(&self) -> Vec<BlockRun> {
        self.dir
            .iter()
            .filter(|d| d.is_allocated())
            .map(|d| d.run())
            .collect()
    }

    /// Look up a record. `Ok(None)` if absent.
    pub fn get(
        &self,
        file: &BlockFile,
        id: u128,
    ) -> StorageResult<Option<Vec<u8>>> {
        let desc = self.dir[self.index(id)];
        if !desc.is_allocated() {
            return Ok(None);
        }
        let bucket = self.read_bucket(file, desc)?;
        Ok(bucket.get(id).map(<[u8]>::to_vec))
    }

    /// Insert or replace a single record (copy-on-write).
    pub fn put(
        &mut self,
        file: &BlockFile,
        id: u128,
        payload: &[u8],
    ) -> StorageResult<()> {
        let i = self.index(id);
        self.rewrite(file, i, |bucket| bucket.upsert(id, payload))
    }

    /// Remove a single record. Returns whether it was present.
    pub fn remove(
        &mut self,
        file: &BlockFile,
        id: u128,
    ) -> StorageResult<bool> {
        let i = self.index(id);
        let desc = self.dir[i];
        if !desc.is_allocated() {
            return Ok(false);
        }
        let mut bucket = self.read_bucket(file, desc)?;
        if !bucket.remove(id) {
            return Ok(false);
        }
        self.commit(file, i, desc, &bucket)?;
        Ok(true)
    }

    /// Apply a batch of mutations to one bucket copy-on-write (the drain's
    /// entry point). `Some` upserts, `None` deletes.
    pub fn commit_slot(
        &mut self,
        file: &BlockFile,
        i: usize,
        mutations: &std::collections::BTreeMap<u128, Option<Vec<u8>>>,
    ) -> StorageResult<()> {
        let desc = self.dir[i];
        let mut bucket = self.read_bucket(file, desc)?;
        for (id, mutation) in mutations {
            match mutation {
                Some(payload) => bucket.upsert(*id, payload),
                None => {
                    bucket.remove(*id);
                }
            }
        }
        self.commit(file, i, desc, &bucket)
    }

    /// Split bucket `s` — the one growth step. Grows `M` by one.
    pub fn split_next(&mut self, file: &BlockFile) -> StorageResult<()> {
        let m = self.dir.len() as u64;
        let level = m.ilog2();
        let s = (m - (1u64 << level)) as usize; // bucket to split

        let desc = self.dir[s];
        let bucket = self.read_bucket(file, desc)?;
        // Partition by bit `level`: 0 stays in s, 1 moves to the new bucket.
        let mut keep = SlotPage::new(self.struct_id, self.version);
        let mut moved = SlotPage::new(self.struct_id, self.version);
        for (id, payload) in bucket.entries {
            if (hash_of(id) >> level) & 1 == 0 {
                keep.upsert_owned(id, payload);
            } else {
                moved.upsert_owned(id, payload);
            }
        }
        let keep_desc = self.place_if_nonempty(file, &keep)?;
        let moved_desc = self.place_if_nonempty(file, &moved)?;
        if desc.is_allocated() {
            file.free(desc.run());
        }
        self.dir[s] = keep_desc;
        self.dir.push(moved_desc); // new bucket at index M (= s + base)
        Ok(())
    }

    // ── Internals ────────────────────────────────────────────────────────

    /// Read bucket `i`, apply `mutate`, commit copy-on-write.
    fn rewrite(
        &mut self,
        file: &BlockFile,
        i: usize,
        mutate: impl FnOnce(&mut SlotPage),
    ) -> StorageResult<()> {
        let desc = self.dir[i];
        let mut bucket = self.read_bucket(file, desc)?;
        mutate(&mut bucket);
        self.commit(file, i, desc, &bucket)
    }

    /// Commit a rebuilt bucket page copy-on-write: write the new page (or
    /// empty the slot), swap the descriptor, free the old page.
    fn commit(
        &mut self,
        file: &BlockFile,
        i: usize,
        old: PageDescriptor,
        bucket: &SlotPage,
    ) -> StorageResult<()> {
        let new_desc = self.place_if_nonempty(file, bucket)?;
        if old.is_allocated() {
            file.free(old.run());
        }
        self.dir[i] = new_desc;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_remove() {
        let bf = BlockFile::create_in_memory();
        let mut dir = LinearDirectory::new(7, 2,2); // 4 buckets
        dir.put(&bf, 1, b"a").unwrap();
        dir.put(&bf, 2, b"bb").unwrap();
        assert_eq!(dir.get(&bf, 1).unwrap().unwrap(), b"a");
        assert_eq!(dir.get(&bf, 2).unwrap().unwrap(), b"bb");
        assert_eq!(dir.get(&bf, 3).unwrap(), None);
        assert!(dir.remove(&bf, 1).unwrap());
        assert_eq!(dir.get(&bf, 1).unwrap(), None);
        assert!(!dir.remove(&bf, 1).unwrap());
    }

    #[test]
    fn split_grows_one_bucket_and_preserves_records() {
        let bf = BlockFile::create_in_memory();
        let mut dir = LinearDirectory::new(7, 2,2); // M=4, s=0, L=2
        for id in 0..60u128 {
            dir.put(&bf, id, format!("v{id}").as_bytes()).unwrap();
        }
        // Grow by 3 — to M=7, a NON-power-of-two size (the whole point).
        dir.split_next(&bf).unwrap();
        dir.split_next(&bf).unwrap();
        dir.split_next(&bf).unwrap();
        assert_eq!(dir.bucket_count(), 7);
        assert_eq!(dir.level(), 2); // 4 <= 7 < 8
        assert_eq!(dir.split_pointer(), 3); // 7 - 4
        for id in 0..60u128 {
            assert_eq!(
                dir.get(&bf, id).unwrap().unwrap(),
                format!("v{id}").as_bytes()
            );
        }
    }

    #[test]
    fn level_bumps_when_split_pointer_wraps() {
        let bf = BlockFile::create_in_memory();
        let mut dir = LinearDirectory::new(7, 2,1); // M=2, L=1
        for id in 0..40u128 {
            dir.put(&bf, id, b"x").unwrap();
        }
        // Split both buckets → M=4, level 1→2, s back to 0.
        dir.split_next(&bf).unwrap(); // M=3, s=1
        assert_eq!(dir.level(), 1);
        assert_eq!(dir.split_pointer(), 1);
        dir.split_next(&bf).unwrap(); // M=4, s=0, L=2
        assert_eq!(dir.bucket_count(), 4);
        assert_eq!(dir.level(), 2);
        assert_eq!(dir.split_pointer(), 0);
        for id in 0..40u128 {
            assert_eq!(dir.get(&bf, id).unwrap().unwrap(), b"x");
        }
    }

    #[test]
    fn only_split_bucket_relocates() {
        // Splitting bucket s must leave every other bucket's page untouched.
        let bf = BlockFile::create_in_memory();
        let mut dir = LinearDirectory::new(7, 2,2);
        for id in 0..40u128 {
            dir.put(&bf, id, b"y").unwrap();
        }
        let before: Vec<_> =
            (0..dir.bucket_count()).map(|i| dir.descriptor(i)).collect();
        dir.split_next(&bf).unwrap(); // splits bucket 0
        // Buckets 1..M kept their descriptors (only 0 changed + one appended).
        for (i, &old) in before.iter().enumerate().skip(1) {
            assert_eq!(dir.descriptor(i), old, "bucket {i} moved");
        }
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    #[derive(Debug, Clone)]
    enum Op {
        Put(u128, Vec<u8>),
        Remove(u128),
        Split,
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            5 => (0u128..16, prop::collection::vec(any::<u8>(), 0..24))
                .prop_map(|(id, p)| Op::Put(id, p)),
            3 => (0u128..16).prop_map(Op::Remove),
            2 => Just(Op::Split),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(250))]
        #[test]
        fn matches_reference(ops in prop::collection::vec(op_strategy(), 0..80)) {
            let bf = BlockFile::create_in_memory();
            let mut dir = LinearDirectory::new(7, 2,1);
            let mut model: BTreeMap<u128, Vec<u8>> = BTreeMap::new();

            for op in ops {
                match op {
                    Op::Put(id, payload) => {
                        dir.put(&bf, id, &payload).unwrap();
                        model.insert(id, payload);
                    }
                    Op::Remove(id) => {
                        let removed = dir.remove(&bf, id).unwrap();
                        prop_assert_eq!(removed, model.remove(&id).is_some());
                    }
                    Op::Split => {
                        if dir.bucket_count() < 64 {
                            dir.split_next(&bf).unwrap();
                        }
                    }
                }
                for id in 0u128..16 {
                    let got = dir.get(&bf, id).unwrap();
                    prop_assert_eq!(got.as_deref(), model.get(&id).map(Vec::as_slice));
                }
            }
        }
    }
}
