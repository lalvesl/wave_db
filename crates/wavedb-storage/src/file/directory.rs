//! The per-`(STRUCT_ID, struct_version)` page directory.
//!
//! This is where the three leaf pieces combine:
//! [`BlockAllocator`](crate::file::block_alloc::BlockAllocator) (free space) +
//! [`PageDescriptor`](crate::file::page_dir::PageDescriptor) (the `u64` slot
//! codec) + [`BlockFile`] (physical I/O).
//!
//! Each live `(STRUCT_ID, version)` owns a [`PageDirectory`]: a
//! `Vec<PageDescriptor>` where a record's slot is `hash(id) % slots.len()`.
//! The descriptor at that slot locates a homogeneous page (a run of blocks in
//! the shared [`BlockFile`]) holding that bucket's records.
//!
//! # Growth
//!
//! A full bucket **grows in place** rather than spilling to a neighbour: when
//! a page outgrows its run (or crosses [`PAGE_GROW_OCCUPATION`]), the
//! allocator hands it a roomier run, the page is copied over, the old run is
//! freed, and the single descriptor is rewritten with the new location. No
//! record changes slots — `hash % len` is invariant. Lengthening the
//! directory itself (the rare per-type rehash) is a separate, future step.
//!
//! # Page image
//!
//! A page is stored as a small, self-describing, CRC-checked image so it can
//! sit in a fixed run with trailing padding:
//!
//! ```text
//! [ crc32: u32 ][ byte_len: u32 ][ entry_count: u32 ]   ← 12-byte header
//! [ (id: u128)(len: u32)(payload bytes) ] × entry_count  ← records, id-sorted
//! ```
//!
//! `byte_len` bounds the CRC so padding past the page is ignored on read.
//! This is a deliberately minimal record page — no heap region, no
//! dictionary compression yet; those fold in when this layer merges with the
//! richer [`Page`](crate::page::Page) on the legacy path.

use crate::file::block_alloc::{BlockRun, blocks_for_bytes};
use crate::file::block_file::BlockFile;
use crate::file::page_dir::{MAX_BLOCK_COUNT, PageDescriptor, occupation_for};
use crate::{StorageError, StorageResult};

/// Blocks handed to a brand-new page before it has grown.
pub const INITIAL_PAGE_BLOCKS: u64 = 1;

/// `occupation` gauge (0..63) at or above which a page is relocated to a
/// roomier run on its next write. `48/64 = 75%`.
pub const PAGE_GROW_OCCUPATION: u8 = 48;

const HEADER_LEN: usize = 12; // crc(4) + byte_len(4) + entry_count(4)
const ENTRY_HEADER_LEN: usize = 20; // id(16) + len(4)

/// Route a record `id` to a directory slot.
#[allow(clippy::cast_possible_truncation)]
const fn slot_for(id: u128, slot_count: usize) -> usize {
    let lo = id as u64;
    let hi = (id >> 64) as u64;
    let mixed = crate::hash::mix64(lo ^ hi.rotate_left(32));
    (mixed % slot_count as u64) as usize
}

/// In-memory view of one page: the records of a single bucket, id-sorted.
struct SlotPage {
    entries: Vec<(u128, Vec<u8>)>,
}

impl SlotPage {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn get(&self, id: u128) -> Option<&[u8]> {
        self.entries
            .binary_search_by_key(&id, |(k, _)| *k)
            .ok()
            .map(|i| self.entries[i].1.as_slice())
    }

    fn upsert(&mut self, id: u128, payload: &[u8]) {
        match self.entries.binary_search_by_key(&id, |(k, _)| *k) {
            Ok(i) => self.entries[i].1 = payload.to_vec(),
            Err(i) => self.entries.insert(i, (id, payload.to_vec())),
        }
    }

    fn remove(&mut self, id: u128) -> bool {
        match self.entries.binary_search_by_key(&id, |(k, _)| *k) {
            Ok(i) => {
                self.entries.remove(i);
                true
            }
            Err(_) => false,
        }
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialized byte length of this page.
    fn encoded_len(&self) -> usize {
        HEADER_LEN
            + self
                .entries
                .iter()
                .map(|(_, v)| ENTRY_HEADER_LEN + v.len())
                .sum::<usize>()
    }

    /// Encode the page to a CRC-checked image.
    fn encode(&self) -> Vec<u8> {
        let total = self.encoded_len();
        let mut buf = vec![0u8; total];
        buf[4..8].copy_from_slice(
            &u32::try_from(total).expect("page exceeds u32 bytes").to_le_bytes(),
        );
        buf[8..12].copy_from_slice(
            &u32::try_from(self.entries.len())
                .expect("too many entries")
                .to_le_bytes(),
        );
        let mut off = HEADER_LEN;
        for (id, v) in &self.entries {
            buf[off..off + 16].copy_from_slice(&id.to_le_bytes());
            off += 16;
            buf[off..off + 4].copy_from_slice(
                &u32::try_from(v.len())
                    .expect("payload exceeds u32")
                    .to_le_bytes(),
            );
            off += 4;
            buf[off..off + v.len()].copy_from_slice(v);
            off += v.len();
        }
        let crc = crc32fast::hash(&buf[4..total]);
        buf[0..4].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Decode a page image, verifying its CRC and ignoring trailing padding.
    fn decode(bytes: &[u8]) -> StorageResult<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(StorageError::Other("page image too short".into()));
        }
        let byte_len =
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        if byte_len < HEADER_LEN || bytes.len() < byte_len {
            return Err(StorageError::Other(
                "page image length out of range".into(),
            ));
        }
        let expected = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let actual = crc32fast::hash(&bytes[4..byte_len]);
        if expected != actual {
            return Err(StorageError::ChecksumMismatch {
                page_index: 0,
                expected,
                actual,
            });
        }
        let count =
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let mut entries = Vec::with_capacity(count);
        let mut off = HEADER_LEN;
        for _ in 0..count {
            if off + ENTRY_HEADER_LEN > byte_len {
                return Err(StorageError::Other("truncated entry header".into()));
            }
            let id = u128::from_le_bytes(bytes[off..off + 16].try_into().unwrap());
            off += 16;
            let len =
                u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
            off += 4;
            if off + len > byte_len {
                return Err(StorageError::Other("truncated entry payload".into()));
            }
            entries.push((id, bytes[off..off + len].to_vec()));
            off += len;
        }
        Ok(Self { entries })
    }
}

/// A per-`(STRUCT_ID, version)` page directory over a shared [`BlockFile`].
///
/// Holds the `Vec<PageDescriptor>`; every operation takes the `BlockFile` so
/// one file backs many directories (one per type).
pub struct PageDirectory {
    slots: Vec<PageDescriptor>,
}

impl PageDirectory {
    /// A directory of `slot_count` empty slots.
    ///
    /// # Panics
    ///
    /// Panics if `slot_count == 0`.
    #[must_use]
    pub fn new(slot_count: usize) -> Self {
        assert!(slot_count > 0, "page directory needs at least one slot");
        Self {
            slots: vec![PageDescriptor::EMPTY; slot_count],
        }
    }

    /// Rebuild a directory from recovered descriptors (e.g. journal replay).
    ///
    /// # Panics
    ///
    /// Panics if `slots` is empty.
    #[must_use]
    pub fn from_slots(slots: Vec<PageDescriptor>) -> Self {
        assert!(!slots.is_empty(), "page directory needs at least one slot");
        Self { slots }
    }

    /// Number of slots (the `hash % len` modulus).
    #[must_use]
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// The raw descriptors, e.g. for journaling the directory.
    #[must_use]
    pub fn descriptors(&self) -> &[PageDescriptor] {
        &self.slots
    }

    /// The block runs of every allocated page — feed these to
    /// [`BlockFile::open`] to reconstruct the allocator on restart.
    pub fn used_runs(&self) -> impl Iterator<Item = BlockRun> + '_ {
        self.slots
            .iter()
            .filter(|d| d.is_allocated())
            .map(|d| d.run())
    }

    /// Look up a record by id. `Ok(None)` if it isn't stored.
    pub fn get(
        &self,
        file: &BlockFile,
        id: u128,
    ) -> StorageResult<Option<Vec<u8>>> {
        let desc = self.slots[slot_for(id, self.slots.len())];
        if !desc.is_allocated() {
            return Ok(None);
        }
        let page = SlotPage::decode(&file.read_run(desc.run())?)?;
        Ok(page.get(id).map(<[u8]>::to_vec))
    }

    /// Insert or replace a record, growing/relocating the page as needed.
    pub fn put(
        &mut self,
        file: &BlockFile,
        id: u128,
        payload: &[u8],
    ) -> StorageResult<()> {
        let idx = slot_for(id, self.slots.len());
        let desc = self.slots[idx];
        let mut page = if desc.is_allocated() {
            SlotPage::decode(&file.read_run(desc.run())?)?
        } else {
            SlotPage::new()
        };
        page.upsert(id, payload);

        let need = page.encoded_len() as u64;
        let min_blocks = blocks_for_bytes(need);
        if min_blocks > u64::from(MAX_BLOCK_COUNT) {
            return Err(StorageError::Other(format!(
                "page needs {min_blocks} blocks, exceeds max {MAX_BLOCK_COUNT}"
            )));
        }
        let image = page.encode();

        let current = desc.run();
        let fits_in_place = desc.is_allocated()
            && need <= current.byte_len()
            && occupation_for(need, current.byte_len()) <= PAGE_GROW_OCCUPATION;

        let run = if fits_in_place {
            file.write_run(current, &image)?;
            current
        } else {
            // Roomier run (≈2× headroom) so we don't relocate on every write.
            let target = blocks_for_bytes(need)
                .saturating_mul(2)
                .clamp(INITIAL_PAGE_BLOCKS, u64::from(MAX_BLOCK_COUNT))
                .max(min_blocks);
            let new_run = file.allocate(target)?;
            file.write_run(new_run, &image)?;
            if desc.is_allocated() {
                file.free(current);
            }
            new_run
        };

        let occ = occupation_for(need, run.byte_len());
        self.slots[idx] = PageDescriptor::from_run(run, occ);
        Ok(())
    }

    /// Remove a record. Returns whether it was present. Frees the page's run
    /// once its last record is gone.
    pub fn remove(
        &mut self,
        file: &BlockFile,
        id: u128,
    ) -> StorageResult<bool> {
        let idx = slot_for(id, self.slots.len());
        let desc = self.slots[idx];
        if !desc.is_allocated() {
            return Ok(false);
        }
        let mut page = SlotPage::decode(&file.read_run(desc.run())?)?;
        if !page.remove(id) {
            return Ok(false);
        }
        if page.is_empty() {
            file.free(desc.run());
            self.slots[idx] = PageDescriptor::EMPTY;
        } else {
            let image = page.encode();
            file.write_run(desc.run(), &image)?;
            let occ = occupation_for(image.len() as u64, desc.run().byte_len());
            self.slots[idx] = desc.with_occupation(occ);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn put_get_basic() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(8);
        dir.put(&bf, 1, b"hello").unwrap();
        assert_eq!(dir.get(&bf, 1).unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(dir.get(&bf, 2).unwrap(), None);
    }

    #[test]
    fn collisions_share_one_bucket() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(1); // everything lands in slot 0
        dir.put(&bf, 10, b"a").unwrap();
        dir.put(&bf, 20, b"bb").unwrap();
        dir.put(&bf, 30, b"ccc").unwrap();
        assert_eq!(dir.get(&bf, 10).unwrap().unwrap(), b"a");
        assert_eq!(dir.get(&bf, 20).unwrap().unwrap(), b"bb");
        assert_eq!(dir.get(&bf, 30).unwrap().unwrap(), b"ccc");
        assert_eq!(dir.used_runs().count(), 1); // one shared page
    }

    #[test]
    fn upsert_replaces_value() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(4);
        dir.put(&bf, 5, b"v1").unwrap();
        dir.put(&bf, 5, b"v2-longer").unwrap();
        assert_eq!(dir.get(&bf, 5).unwrap().unwrap(), b"v2-longer");
    }

    #[test]
    fn remove_frees_empty_page() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(1);
        dir.put(&bf, 7, b"x").unwrap();
        assert_eq!(dir.used_runs().count(), 1);

        assert!(dir.remove(&bf, 7).unwrap());
        assert_eq!(dir.get(&bf, 7).unwrap(), None);
        assert_eq!(dir.used_runs().count(), 0);
        assert!(dir.descriptors()[0] == PageDescriptor::EMPTY);
        // Already gone — second remove reports false.
        assert!(!dir.remove(&bf, 7).unwrap());
    }

    #[test]
    fn page_grows_and_relocates_when_outgrowing_run() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(1); // force one bucket
        let big = vec![0xABu8; 1000];
        for id in 0..20u128 {
            dir.put(&bf, id, &big).unwrap();
        }
        // 20 × (20 + 1000) ≫ 4 KiB ⇒ the page must have grown past 1 block.
        assert!(dir.descriptors()[0].block_count() > 1);
        // Every record survived the relocations.
        for id in 0..20u128 {
            assert_eq!(dir.get(&bf, id).unwrap().unwrap(), big);
        }
    }

    #[test]
    fn checksum_detects_corruption() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(1);
        dir.put(&bf, 1, b"data").unwrap();
        let run = dir.descriptors()[0].run();
        let mut bytes = bf.read_run(run).unwrap();
        bytes[HEADER_LEN] ^= 0xFF; // flip a byte inside the CRC-covered region
        bf.write_run(run, &bytes).unwrap();
        assert!(matches!(
            dir.get(&bf, 1),
            Err(StorageError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn survives_block_file_reopen() {
        let dir_tmp = tempdir().unwrap();
        let path = dir_tmp.path().join("data.bin");
        let mut dir = PageDirectory::new(4);
        {
            let bf = BlockFile::create(&path).unwrap();
            dir.put(&bf, 100, b"persisted").unwrap();
            dir.put(&bf, 200, b"another").unwrap();
            bf.sync().unwrap();
        }
        // Reopen the block file, telling it which runs are live.
        let used: Vec<_> = dir.used_runs().collect();
        let bf = BlockFile::open(&path, used).unwrap();
        assert_eq!(dir.get(&bf, 100).unwrap().unwrap(), b"persisted");
        assert_eq!(dir.get(&bf, 200).unwrap().unwrap(), b"another");
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
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0u128..8, prop::collection::vec(any::<u8>(), 0..40))
                .prop_map(|(id, p)| Op::Put(id, p)),
            (0u128..8).prop_map(Op::Remove),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(250))]
        #[test]
        fn directory_matches_reference(
            ops in prop::collection::vec(op_strategy(), 0..80),
        ) {
            let bf = BlockFile::create_in_memory();
            // Two slots → ids collide and pages grow under churn.
            let mut dir = PageDirectory::new(2);
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
                }
                for id in 0u128..8 {
                    let got = dir.get(&bf, id).unwrap();
                    prop_assert_eq!(
                        got.as_deref(),
                        model.get(&id).map(Vec::as_slice)
                    );
                }
            }
        }
    }
}
