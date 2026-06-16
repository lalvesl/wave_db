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
//! # Copy-on-write
//!
//! A live page is **never mutated in place**. Every change — a record write,
//! a bucket growing, a directory rehash — **allocates a fresh page, writes it
//! in full, then commits by swapping the slot's descriptor, and only then
//! frees the old page**. The old page stays the truth until the one-`u64`
//! commit, so a crash can never tear a page; recovery simply sees the old or
//! the new pointer, never a half-written page. No record changes slots —
//! `hash % len` is invariant. Lengthening the directory itself (the rare
//! per-type rehash) follows the same discipline.
//!
//! # Page image
//!
//! A page is stored as a small, self-describing, CRC-checked image so it can
//! sit in a fixed run with trailing padding:
//!
//! ```text
//! [ crc32: u32 ][ byte_len: u32 ]                            ┐
//! [ struct_id: u32 ][ version: u8 ][ codec: u8 ]             │ 30-byte header
//! [ dict_ref: u64 ][ raw_len: u32 ][ entry_count: u32 ]      ┘
//! [ payload bytes ]   ← records region, possibly dictionary-compressed
//! ```
//!
//! The records region is the entries `(id: u128)(len: u32)(payload) ×
//! entry_count`, id-sorted. With [`CODEC_RAW`] it is stored verbatim; with
//! [`CODEC_DICT`] it is zstd-compressed against the dictionary at
//! [`dict_ref`](crate::file::dict::DictRef) (a `u48` block + `u16` count
//! locating the dict page in `data.bin`), and `raw_len` is its uncompressed
//! length (decode's capacity). `byte_len` bounds the CRC so padding is ignored.
//!
//! The `struct_id`/`version` stamp makes a page **self-describing** (a
//! `data.bin` scan knows each page's owner, cross-checking the journal's
//! allocation ledger); `dict_ref` makes it self-locating — decode reads the ref
//! and loads that dict run, no journal dict directory needed (see
//! [`dict`](crate::file::dict)).

use crate::compression::page_codec::{decode_with_dict, encode_with_dict};
use crate::file::block_alloc::{BlockRun, blocks_for_bytes};
use crate::file::block_file::BlockFile;
use crate::file::dict::{DictRef, DictStore};
use crate::file::heap_store::{HeapRef, HeapStore};
use crate::file::page_dir::{MAX_BLOCK_COUNT, PageDescriptor, occupation_for};
use crate::{StorageError, StorageResult};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Blocks handed to a brand-new page before it has grown.
pub const INITIAL_PAGE_BLOCKS: u64 = 1;

/// `occupation` gauge (0..63) at or above which a page is relocated to a
/// roomier run on its next write. `48/64 = 75%`.
pub const PAGE_GROW_OCCUPATION: u8 = 48;

/// Values larger than this spill out of the data page into a heap extent (the
/// "infinity" tier), with the page keeping only a 16-byte [`HeapRef`].
pub const HEAP_SPILL_THRESHOLD: usize = 64 * 1024;

// crc(4) + byte_len(4) + struct_id(4) + version(1) + codec(1)
//   + dict_ref(8) + raw_len(4) + entry_count(4)
const HEADER_LEN: usize = 30;
const ENTRY_HEADER_LEN: usize = 21; // id(16) + kind(1) + len(4)

/// Page payload codec: uncompressed, id-sorted records.
pub(crate) const CODEC_RAW: u8 = 0;
/// Page payload codec: records zstd-compressed against the type's dictionary.
pub(crate) const CODEC_DICT: u8 = 1;

/// Entry kind: the payload is the record's bytes, stored inline.
const KIND_INLINE: u8 = 0;
/// Entry kind: the payload is a 16-byte [`HeapRef`] to a heap extent.
const KIND_HEAP: u8 = 1;

/// One record's value as a page holds it: inline bytes, or a pointer to a heap
/// extent for values past [`HEAP_SPILL_THRESHOLD`].
#[derive(Debug, Clone)]
pub(crate) enum Entry {
    Inline(Vec<u8>),
    Heap(HeapRef),
}

impl Entry {
    /// The inline bytes, or `None` for a heap entry.
    pub(crate) fn inline_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Inline(bytes) => Some(bytes),
            Self::Heap(_) => None,
        }
    }

    /// The heap ref, or `None` for an inline entry.
    pub(crate) const fn heap_ref(&self) -> Option<HeapRef> {
        match self {
            Self::Heap(r) => Some(*r),
            Self::Inline(_) => None,
        }
    }
}

/// Route a record `id` to a directory slot.
#[allow(clippy::cast_possible_truncation)]
const fn slot_for(id: u128, slot_count: usize) -> usize {
    let lo = id as u64;
    let hi = (id >> 64) as u64;
    let mixed = crate::hash::mix64(lo ^ hi.rotate_left(32));
    (mixed % slot_count as u64) as usize
}

/// In-memory view of one page: the records of a single bucket, id-sorted, plus
/// the `(struct_id, version)` stamp the image carries.
///
/// Shared by [`PageDirectory`] and `file::linear` — both store a bucket as
/// this CRC-checked record image.
#[derive(Debug)]
pub(crate) struct SlotPage {
    pub(crate) struct_id: u32,
    pub(crate) version: u8,
    pub(crate) entries: Vec<(u128, Entry)>,
}

impl SlotPage {
    pub(crate) const fn new(struct_id: u32, version: u8) -> Self {
        Self {
            struct_id,
            version,
            entries: Vec::new(),
        }
    }

    pub(crate) fn get(&self, id: u128) -> Option<&Entry> {
        self.entries
            .binary_search_by_key(&id, |(k, _)| *k)
            .ok()
            .map(|i| &self.entries[i].1)
    }

    /// Upsert an inline value (the small-value / `file::linear` path).
    pub(crate) fn upsert(&mut self, id: u128, payload: &[u8]) {
        self.upsert_entry(id, Entry::Inline(payload.to_vec()));
    }

    /// Upsert a fully-formed [`Entry`] — the spill path stores `Heap`, and the
    /// rehash moves an entry between pages without touching its heap extent.
    pub(crate) fn upsert_entry(&mut self, id: u128, entry: Entry) {
        match self.entries.binary_search_by_key(&id, |(k, _)| *k) {
            Ok(i) => self.entries[i].1 = entry,
            Err(i) => self.entries.insert(i, (id, entry)),
        }
    }

    pub(crate) fn remove(&mut self, id: u128) -> bool {
        match self.entries.binary_search_by_key(&id, |(k, _)| *k) {
            Ok(i) => {
                self.entries.remove(i);
                true
            }
            Err(_) => false,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialized byte length of one entry's payload.
    fn payload_len(entry: &Entry) -> usize {
        match entry {
            Entry::Inline(bytes) => bytes.len(),
            Entry::Heap(_) => 16, // a HeapRef raw u128
        }
    }

    /// Uncompressed length of the records region (every entry serialized).
    fn records_len(&self) -> usize {
        self.entries
            .iter()
            .map(|(_, e)| ENTRY_HEADER_LEN + Self::payload_len(e))
            .sum()
    }

    /// Serialize the records region:
    /// `(id u128)(kind u8)(len u32)(payload) ×`, id-sorted.
    fn records_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.records_len());
        for (id, entry) in &self.entries {
            buf.extend_from_slice(&id.to_le_bytes());
            match entry {
                Entry::Inline(bytes) => {
                    buf.push(KIND_INLINE);
                    buf.extend_from_slice(
                        &u32::try_from(bytes.len())
                            .expect("payload exceeds u32")
                            .to_le_bytes(),
                    );
                    buf.extend_from_slice(bytes);
                }
                Entry::Heap(r) => {
                    buf.push(KIND_HEAP);
                    buf.extend_from_slice(&16u32.to_le_bytes());
                    buf.extend_from_slice(&r.raw().to_le_bytes());
                }
            }
        }
        buf
    }

    /// Read the [`DictRef`] from a page image header without fully decoding —
    /// lets the directory resolve the dict bytes before [`decode`](Self::decode).
    pub(crate) fn dict_ref_of(bytes: &[u8]) -> StorageResult<DictRef> {
        if bytes.len() < HEADER_LEN {
            return Err(StorageError::Other("page image too short".into()));
        }
        Ok(DictRef::from_raw(u64::from_le_bytes(
            bytes[14..22].try_into().unwrap(),
        )))
    }

    /// Encode the page to a CRC-checked image. `dict` is the bytes + [`DictRef`]
    /// to compress against ([`CODEC_DICT`]); `None` stores [`CODEC_RAW`].
    fn encode(&self, dict: Option<(&[u8], DictRef)>) -> StorageResult<Vec<u8>> {
        let records = self.records_bytes();
        let raw_len = records.len();
        let (codec, dict_ref, payload) = match dict {
            Some((bytes, dict_ref)) if !bytes.is_empty() => {
                (CODEC_DICT, dict_ref, encode_with_dict(&records, bytes)?)
            }
            _ => (CODEC_RAW, DictRef::NONE, records),
        };

        let total = HEADER_LEN + payload.len();
        let mut buf = vec![0u8; total];
        buf[4..8].copy_from_slice(
            &u32::try_from(total).expect("page exceeds u32 bytes").to_le_bytes(),
        );
        buf[8..12].copy_from_slice(&self.struct_id.to_le_bytes());
        buf[12] = self.version;
        buf[13] = codec;
        buf[14..22].copy_from_slice(&dict_ref.raw().to_le_bytes());
        buf[22..26].copy_from_slice(
            &u32::try_from(raw_len).expect("records exceed u32").to_le_bytes(),
        );
        buf[26..30].copy_from_slice(
            &u32::try_from(self.entries.len())
                .expect("too many entries")
                .to_le_bytes(),
        );
        buf[HEADER_LEN..].copy_from_slice(&payload);
        let crc = crc32fast::hash(&buf[4..total]);
        buf[0..4].copy_from_slice(&crc.to_le_bytes());
        Ok(buf)
    }

    /// Decode a page image, verifying its CRC and ignoring trailing padding. A
    /// [`CODEC_DICT`] page needs `dict` — the bytes for the header's `dict_ref`,
    /// which the caller resolves via [`dict_ref_of`](Self::dict_ref_of).
    pub(crate) fn decode(
        bytes: &[u8],
        dict: Option<&[u8]>,
    ) -> StorageResult<Self> {
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
        let struct_id = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let version = bytes[12];
        let codec = bytes[13];
        let raw_len =
            u32::from_le_bytes(bytes[22..26].try_into().unwrap()) as usize;
        let count =
            u32::from_le_bytes(bytes[26..30].try_into().unwrap()) as usize;

        let payload = &bytes[HEADER_LEN..byte_len];
        let records = match codec {
            CODEC_RAW => payload.to_vec(),
            CODEC_DICT => {
                let d = dict.ok_or_else(|| {
                    StorageError::Other("dict page needs a dictionary".into())
                })?;
                decode_with_dict(payload, d, raw_len)?
            }
            other => {
                return Err(StorageError::Other(format!(
                    "unknown page codec {other}"
                )));
            }
        };

        let mut entries = Vec::with_capacity(count);
        let mut off = 0usize;
        for _ in 0..count {
            if off + ENTRY_HEADER_LEN > records.len() {
                return Err(StorageError::Other("truncated entry header".into()));
            }
            let id =
                u128::from_le_bytes(records[off..off + 16].try_into().unwrap());
            off += 16;
            let kind = records[off];
            off += 1;
            let len = u32::from_le_bytes(
                records[off..off + 4].try_into().unwrap(),
            ) as usize;
            off += 4;
            if off + len > records.len() {
                return Err(StorageError::Other("truncated entry payload".into()));
            }
            let body = &records[off..off + len];
            let entry = match kind {
                KIND_INLINE => Entry::Inline(body.to_vec()),
                KIND_HEAP => {
                    if len != 16 {
                        return Err(StorageError::Other(
                            "heap entry payload is not a 16-byte ref".into(),
                        ));
                    }
                    Entry::Heap(HeapRef::from_raw(u128::from_le_bytes(
                        body.try_into().unwrap(),
                    )))
                }
                other => {
                    return Err(StorageError::Other(format!(
                        "unknown entry kind {other}"
                    )));
                }
            };
            entries.push((id, entry));
            off += len;
        }
        Ok(Self {
            struct_id,
            version,
            entries,
        })
    }
}

/// Outcome of a copy-on-write slot commit: the slot's new descriptor plus the
/// block runs the commit allocated and/or freed.
///
/// The store feeds `allocated` / `freed` into the journal's allocation ledger
/// (see [`PageJournal::allocate_block`]) so the [`BlockAllocator`] rebuilds from
/// the journal alone.
///
/// [`PageJournal::allocate_block`]: crate::file::page_journal::PageJournal::allocate_block
/// [`BlockAllocator`]: crate::file::block_alloc::BlockAllocator
#[derive(Debug, Clone)]
pub struct SlotCommit {
    /// The slot's new descriptor (`EMPTY` if the page is now empty).
    pub descriptor: PageDescriptor,
    /// The run a fresh page was written to, if any.
    pub allocated: Option<BlockRun>,
    /// The old run that was released, if the slot previously held a page.
    pub freed: Option<BlockRun>,
    /// Heap extents written for values that spilled this commit.
    pub heap_allocated: Vec<BlockRun>,
    /// Heap extents orphaned this commit (overwritten/deleted heap values).
    pub heap_freed: Vec<BlockRun>,
}

/// A per-`(STRUCT_ID, version)` page directory over a shared [`BlockFile`].
///
/// Holds the `Vec<PageDescriptor>` plus the `(struct_id, version)` it stamps
/// onto every page it writes; every operation takes the `BlockFile` so one file
/// backs many directories (one per type).
pub struct PageDirectory {
    struct_id: u32,
    version: u8,
    /// Dictionary cache + current ref. `current() == NONE` ⇒ pages stored
    /// [`CODEC_RAW`]; otherwise new pages compress against it ([`CODEC_DICT`]).
    dicts: DictStore,
    slots: Vec<PageDescriptor>,
}

impl PageDirectory {
    /// A directory of `slot_count` empty slots for type `(struct_id, version)`.
    ///
    /// # Panics
    ///
    /// Panics if `slot_count == 0`.
    #[must_use]
    pub fn new(struct_id: u32, version: u8, slot_count: usize) -> Self {
        assert!(slot_count > 0, "page directory needs at least one slot");
        Self {
            struct_id,
            version,
            dicts: DictStore::new(),
            slots: vec![PageDescriptor::EMPTY; slot_count],
        }
    }

    /// Rebuild a directory from recovered descriptors (e.g. journal replay).
    ///
    /// # Panics
    ///
    /// Panics if `slots` is empty.
    #[must_use]
    pub fn from_slots(
        struct_id: u32,
        version: u8,
        slots: Vec<PageDescriptor>,
    ) -> Self {
        assert!(!slots.is_empty(), "page directory needs at least one slot");
        Self {
            struct_id,
            version,
            dicts: DictStore::new(),
            slots,
        }
    }

    /// Train-and-install a dictionary: write it to `data.bin` and make it the
    /// one new pages compress against. Returns its [`DictRef`] (its run); the
    /// store journals that allocation through the ordinary ledger. Existing
    /// pages keep decoding via their own header `dict_ref`.
    pub fn install_dictionary(
        &mut self,
        file: &BlockFile,
        dict: &[u8],
    ) -> StorageResult<DictRef> {
        self.dicts.install(file, dict)
    }

    /// Whether this type has a current dictionary (new pages compress).
    #[must_use]
    pub const fn has_dictionary(&self) -> bool {
        !self.dicts.current().is_none()
    }

    /// Adopt an already-installed dictionary as current (recovery): set it from
    /// any [`CODEC_DICT`] page's `dict_ref`, so post-restart writes keep
    /// compressing without re-training. No-op if no dict page exists.
    pub fn adopt_dictionary_from_pages(
        &mut self,
        file: &BlockFile,
    ) -> StorageResult<()> {
        if let Some(dict_ref) = self.page_dict_refs(file)?.into_iter().next() {
            self.dicts.adopt(dict_ref);
        }
        Ok(())
    }

    /// Distinct non-`NONE` dictionary refs across all allocated page headers,
    /// plus the current one. The store re-emits these as live allocations when
    /// it rewrites the journal (compaction), so dict runs aren't dropped.
    pub fn dict_runs_in_use(
        &self,
        file: &BlockFile,
    ) -> StorageResult<Vec<DictRef>> {
        let mut refs = self.page_dict_refs(file)?;
        let current = self.dicts.current();
        if !current.is_none() && !refs.contains(&current) {
            refs.push(current);
        }
        Ok(refs)
    }

    /// Distinct non-`NONE` dict refs read from each allocated page's header.
    fn page_dict_refs(&self, file: &BlockFile) -> StorageResult<Vec<DictRef>> {
        let mut refs: Vec<DictRef> = Vec::new();
        for desc in self.slots.iter().filter(|d| d.is_allocated()) {
            let bytes = file.read_run(desc.run())?;
            let dict_ref = SlotPage::dict_ref_of(&bytes)?;
            if !dict_ref.is_none() && !refs.contains(&dict_ref) {
                refs.push(dict_ref);
            }
        }
        Ok(refs)
    }

    /// Collect up to `max` record payloads from the committed pages — the
    /// training corpus for a dictionary.
    pub fn sample_payloads(
        &self,
        file: &BlockFile,
        max: usize,
    ) -> StorageResult<Vec<Vec<u8>>> {
        let mut samples = Vec::new();
        for desc in self.slots.iter().filter(|d| d.is_allocated()) {
            let page = self.read_run_page(file, desc.run())?;
            for (_, entry) in page.entries {
                // Spilled (heap) values aren't part of the page corpus.
                if let Entry::Inline(payload) = entry {
                    samples.push(payload);
                    if samples.len() >= max {
                        return Ok(samples);
                    }
                }
            }
        }
        Ok(samples)
    }

    /// Heap extents referenced by this directory's pages — the store re-emits
    /// these as live allocations during compaction (mirrors
    /// [`dict_runs_in_use`](Self::dict_runs_in_use)).
    pub fn heap_runs_in_use(
        &self,
        file: &BlockFile,
    ) -> StorageResult<Vec<BlockRun>> {
        let mut runs = Vec::new();
        for desc in self.slots.iter().filter(|d| d.is_allocated()) {
            let page = self.read_run_page(file, desc.run())?;
            for (_, entry) in &page.entries {
                if let Some(heap_ref) = entry.heap_ref() {
                    runs.push(heap_ref.run());
                }
            }
        }
        Ok(runs)
    }

    /// Resolve the current dictionary's bytes + ref for encoding (`None` ⇒ raw).
    fn current_dict(
        &self,
        file: &BlockFile,
    ) -> StorageResult<Option<(Arc<Vec<u8>>, DictRef)>> {
        let dict_ref = self.dicts.current();
        if dict_ref.is_none() {
            return Ok(None);
        }
        Ok(Some((self.dicts.load(file, dict_ref)?, dict_ref)))
    }

    /// A fresh, empty [`SlotPage`] stamped with this directory's type.
    const fn empty_page(&self) -> SlotPage {
        SlotPage::new(self.struct_id, self.version)
    }

    /// Decode the page image at `run`, resolving its header `dict_ref` through
    /// this directory's dictionary cache.
    fn read_run_page(
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

    /// Read the page at `desc`, or a fresh empty page if the slot is empty.
    fn read_slot(
        &self,
        file: &BlockFile,
        desc: PageDescriptor,
    ) -> StorageResult<SlotPage> {
        if desc.is_allocated() {
            self.read_run_page(file, desc.run())
        } else {
            Ok(self.empty_page())
        }
    }

    /// Encode and place `page`, compressing against the current dictionary.
    fn place(
        &self,
        file: &BlockFile,
        page: &SlotPage,
    ) -> StorageResult<PageDescriptor> {
        let current = self.current_dict(file)?;
        let dict = current.as_ref().map(|(bytes, r)| (bytes.as_slice(), *r));
        place_page(file, page, dict)
    }

    /// Number of slots (the `hash % len` modulus).
    #[must_use]
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// The directory slot a record `id` routes to — useful for journaling the
    /// one slot a `put`/`remove` touched.
    #[must_use]
    pub fn slot_of(&self, id: u128) -> usize {
        slot_for(id, self.slots.len())
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

    /// Look up a record by id. `Ok(None)` if it isn't stored. A spilled value
    /// is resolved from its heap extent.
    pub fn get(
        &self,
        file: &BlockFile,
        id: u128,
    ) -> StorageResult<Option<Vec<u8>>> {
        let desc = self.slots[slot_for(id, self.slots.len())];
        if !desc.is_allocated() {
            return Ok(None);
        }
        let page = self.read_run_page(file, desc.run())?;
        match page.get(id) {
            None => Ok(None),
            Some(Entry::Inline(bytes)) => Ok(Some(bytes.clone())),
            Some(Entry::Heap(heap_ref)) => {
                Ok(Some(HeapStore::get(file, *heap_ref)?))
            }
        }
    }

    /// Apply one mutation to `page`, spilling values past
    /// [`HEAP_SPILL_THRESHOLD`] to a heap extent. Records the heap run allocated
    /// (a new spill) and the heap run orphaned (the id's previous heap value).
    fn apply_mutation(
        file: &BlockFile,
        page: &mut SlotPage,
        id: u128,
        value: Option<&[u8]>,
        heap_allocated: &mut Vec<BlockRun>,
        heap_freed: &mut Vec<BlockRun>,
    ) -> StorageResult<()> {
        // The id's previous heap extent (if any) is orphaned by this write.
        if let Some(old) = page.get(id).and_then(Entry::heap_ref) {
            heap_freed.push(old.run());
        }
        match value {
            None => {
                page.remove(id);
            }
            Some(bytes) if bytes.len() > HEAP_SPILL_THRESHOLD => {
                let heap_ref = HeapStore::put(file, bytes)?;
                heap_allocated.push(heap_ref.run());
                page.upsert_entry(id, Entry::Heap(heap_ref));
            }
            Some(bytes) => page.upsert_entry(id, Entry::Inline(bytes.to_vec())),
        }
        Ok(())
    }

    /// Insert or replace a single record (copy-on-write — see
    /// [`commit_slot`](Self::commit_slot)).
    pub fn put(
        &mut self,
        file: &BlockFile,
        id: u128,
        payload: &[u8],
    ) -> StorageResult<()> {
        let idx = slot_for(id, self.slots.len());
        let desc = self.slots[idx];
        let mut page = self.read_slot(file, desc)?;
        let (mut heap_allocated, mut heap_freed) = (Vec::new(), Vec::new());
        Self::apply_mutation(
            file,
            &mut page,
            id,
            Some(payload),
            &mut heap_allocated,
            &mut heap_freed,
        )?;
        self.commit_page(file, idx, desc, &page, heap_allocated, heap_freed)?;
        Ok(())
    }

    /// Remove a single record. Returns whether it was present.
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
        let mut page = self.read_slot(file, desc)?;
        if page.get(id).is_none() {
            return Ok(false);
        }
        let (mut heap_allocated, mut heap_freed) = (Vec::new(), Vec::new());
        Self::apply_mutation(
            file,
            &mut page,
            id,
            None,
            &mut heap_allocated,
            &mut heap_freed,
        )?;
        self.commit_page(file, idx, desc, &page, heap_allocated, heap_freed)?;
        Ok(true)
    }

    /// Apply a **batch** of record mutations to one slot copy-on-write,
    /// returning the slot's new descriptor. This is what the drain uses: all
    /// pending records for a page are merged in one page rebuild.
    ///
    /// `Some(bytes)` upserts a record (spilling large values to the heap);
    /// `None` deletes it. The merged page is written to a fresh run (or the slot
    /// is emptied if no records remain); the old page run and any orphaned heap
    /// extents are freed only after the new page is committed.
    pub fn commit_slot(
        &mut self,
        file: &BlockFile,
        slot: usize,
        mutations: &BTreeMap<u128, Option<Vec<u8>>>,
    ) -> StorageResult<SlotCommit> {
        let desc = self.slots[slot];
        let mut page = self.read_slot(file, desc)?;
        let (mut heap_allocated, mut heap_freed) = (Vec::new(), Vec::new());
        for (id, mutation) in mutations {
            Self::apply_mutation(
                file,
                &mut page,
                *id,
                mutation.as_deref(),
                &mut heap_allocated,
                &mut heap_freed,
            )?;
        }
        self.commit_page(file, slot, desc, &page, heap_allocated, heap_freed)
    }

    /// Commit a rebuilt page to a slot copy-on-write: write the new page (or
    /// empty the slot), swap the descriptor, then free the old page run and any
    /// orphaned heap extents. Reports the allocated/freed runs (page + heap) so
    /// the caller can journal the allocation ledger.
    fn commit_page(
        &mut self,
        file: &BlockFile,
        slot: usize,
        old: PageDescriptor,
        page: &SlotPage,
        heap_allocated: Vec<BlockRun>,
        heap_freed: Vec<BlockRun>,
    ) -> StorageResult<SlotCommit> {
        let new_desc = if page.is_empty() {
            PageDescriptor::EMPTY
        } else {
            self.place(file, page)?
        };
        let allocated = new_desc.is_allocated().then(|| new_desc.run());
        let freed = old.is_allocated().then(|| old.run());
        if let Some(run) = freed {
            file.free(run);
        }
        // Orphaned heap extents are reclaimable now the new page (which no
        // longer points at them) is committed.
        for run in &heap_freed {
            file.free(*run);
        }
        self.slots[slot] = new_desc;
        Ok(SlotCommit {
            descriptor: new_desc,
            allocated,
            freed,
            heap_allocated,
            heap_freed,
        })
    }

    /// Whether the directory should be lengthened: most slots already hold
    /// pages that are both **large** (≥ half the 255-block ceiling) and
    /// **full** (`occupation ≥ PAGE_GROW_OCCUPATION`). When a type's buckets
    /// can no longer absorb growth in place, adding slots is the only relief.
    /// This is the rare, heavy path — the engine runs it in the background.
    #[must_use]
    pub fn needs_grow(&self) -> bool {
        let hot = self
            .slots
            .iter()
            .filter(|d| {
                d.is_allocated()
                    && d.occupation() >= PAGE_GROW_OCCUPATION
                    && u64::from(d.block_count()) * 2 >= u64::from(MAX_BLOCK_COUNT)
            })
            .count();
        // ≥ 75% of all slots are hot.
        hot * 4 >= self.slots.len() * 3
    }

    /// Double the directory length and rehash every record. Convenience over
    /// [`grow_to`](Self::grow_to). Returns the old runs it freed.
    pub fn grow(&mut self, file: &BlockFile) -> StorageResult<Vec<BlockRun>> {
        self.grow_to(file, self.slots.len().saturating_mul(2))
    }

    /// Lengthen the directory to `new_slot_count` and rehash every record
    /// into the longer directory.
    ///
    /// New pages are materialized **before** the old runs are freed, so a
    /// transient I/O error mid-rehash leaves the existing directory and data
    /// fully intact (the only cost is leaked blocks a later `truncate` or
    /// journal replay reclaims). Production journals this as one atomic step.
    ///
    /// # Panics
    ///
    /// Panics if `new_slot_count < slot_count` — the directory only grows.
    pub fn grow_to(
        &mut self,
        file: &BlockFile,
        new_slot_count: usize,
    ) -> StorageResult<Vec<BlockRun>> {
        assert!(
            new_slot_count >= self.slots.len(),
            "directory only grows ({} -> {new_slot_count})",
            self.slots.len()
        );

        // Drain every record into the new bucket layout. Old runs stay live.
        let old_runs: Vec<BlockRun> = self.used_runs().collect();
        let mut buckets: Vec<SlotPage> =
            (0..new_slot_count).map(|_| self.empty_page()).collect();
        for &run in &old_runs {
            let page = self.read_run_page(file, run)?;
            // Move whole entries — a heap entry keeps its extent, only its ref
            // lands in the new bucket. No heap I/O here.
            for (id, entry) in page.entries {
                buckets[slot_for(id, new_slot_count)].upsert_entry(id, entry);
            }
        }

        // Materialize the new pages with fresh allocations (old data intact
        // until we commit, so any error here is safely recoverable).
        let mut new_slots = vec![PageDescriptor::EMPTY; new_slot_count];
        for (i, page) in buckets.iter().enumerate() {
            if !page.is_empty() {
                new_slots[i] = self.place(file, page)?;
            }
        }

        // Commit: swap in the new directory, then release the old runs.
        self.slots = new_slots;
        for &run in &old_runs {
            file.free(run);
        }
        Ok(old_runs)
    }
}

/// Encode `page` (compressing against `dict` when present), allocate a roomier
/// run for the image, write it, and return its descriptor.
///
/// Shared by [`PageDirectory`] and `file::linear`. Sizes the run from the
/// *encoded* image (so compression shrinks the allocation), with ~2× headroom
/// so the page doesn't relocate on its very next write. `dict` is the bytes +
/// [`DictRef`] to compress against.
pub(crate) fn place_page(
    file: &BlockFile,
    page: &SlotPage,
    dict: Option<(&[u8], DictRef)>,
) -> StorageResult<PageDescriptor> {
    let image = page.encode(dict)?;
    let need = image.len() as u64;
    let min_blocks = blocks_for_bytes(need);
    if min_blocks > u64::from(MAX_BLOCK_COUNT) {
        return Err(StorageError::Other(format!(
            "page needs {min_blocks} blocks, exceeds max {MAX_BLOCK_COUNT}"
        )));
    }
    let target = blocks_for_bytes(need)
        .saturating_mul(2)
        .clamp(INITIAL_PAGE_BLOCKS, u64::from(MAX_BLOCK_COUNT))
        .max(min_blocks);
    let run = file.allocate(target)?;
    file.write_run(run, &image)?;
    Ok(PageDescriptor::from_run(run, occupation_for(need, run.byte_len())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn put_get_basic() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(7, 2,8);
        dir.put(&bf, 1, b"hello").unwrap();
        assert_eq!(dir.get(&bf, 1).unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(dir.get(&bf, 2).unwrap(), None);
    }

    #[test]
    fn collisions_share_one_bucket() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(7, 2,1); // everything lands in slot 0
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
        let mut dir = PageDirectory::new(7, 2,4);
        dir.put(&bf, 5, b"v1").unwrap();
        dir.put(&bf, 5, b"v2-longer").unwrap();
        assert_eq!(dir.get(&bf, 5).unwrap().unwrap(), b"v2-longer");
    }

    #[test]
    fn remove_frees_empty_page() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(7, 2,1);
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
        let mut dir = PageDirectory::new(7, 2,1); // force one bucket
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
        let mut dir = PageDirectory::new(7, 2,1);
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
    fn page_image_carries_type_stamp_and_codec() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(9, 4, 1);
        dir.put(&bf, 1, b"hi").unwrap();
        let run = dir.descriptors()[0].run();
        let bytes = bf.read_run(run).unwrap();
        // struct_id [8..12], version [12], codec [13].
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 9);
        assert_eq!(bytes[12], 4);
        assert_eq!(bytes[13], CODEC_RAW);
        // Decode round-trips the stamp.
        let page = SlotPage::decode(&bytes, None).unwrap();
        assert_eq!((page.struct_id, page.version), (9, 4));
    }

    #[test]
    fn unknown_codec_is_rejected() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(1, 0, 1);
        dir.put(&bf, 1, b"x").unwrap();
        let run = dir.descriptors()[0].run();
        let mut bytes = bf.read_run(run).unwrap();
        let byte_len =
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        bytes[13] = 0xFF; // bogus codec
        // Re-stamp the CRC so the codec check (not the checksum) is what trips.
        let crc = crc32fast::hash(&bytes[4..byte_len]);
        bytes[0..4].copy_from_slice(&crc.to_le_bytes());
        let err = SlotPage::decode(&bytes, None).unwrap_err();
        assert!(matches!(err, StorageError::Other(_)));
    }

    #[test]
    fn dict_directory_roundtrips_and_compresses() {
        use crate::compression::page_codec::train_dictionary;

        // Train a dictionary on homogeneous one-type payloads.
        let samples: Vec<Vec<u8>> = (0..2000)
            .map(|i| {
                format!(
                    "{{\"id\":{i},\"role\":\"member\",\"status\":\"active\"}}"
                )
                .into_bytes()
            })
            .collect();
        let dict_bytes = train_dictionary(&samples, 8 * 1024).unwrap();

        // RAW baseline: one bucket → one growing page.
        let raw_bf = BlockFile::create_in_memory();
        let mut raw = PageDirectory::new(7, 2, 1);
        for (i, s) in samples.iter().enumerate().take(200) {
            raw.put(&raw_bf, i as u128, s).unwrap();
        }
        let raw_blocks = raw.descriptors()[0].block_count();

        // DICT directory: install the dict (into data.bin) then write the data.
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(7, 2, 1);
        let dict_ref = dir.install_dictionary(&bf, &dict_bytes).unwrap();
        for (i, s) in samples.iter().enumerate().take(200) {
            dir.put(&bf, i as u128, s).unwrap();
        }
        // Every record round-trips through the DICT codec.
        for (i, s) in samples.iter().enumerate().take(200) {
            assert_eq!(dir.get(&bf, i as u128).unwrap().unwrap(), *s);
        }

        // The page is marked CODEC_DICT, points at the dict, and occupies fewer
        // blocks than RAW.
        let run = dir.descriptors()[0].run();
        let image = bf.read_run(run).unwrap();
        assert_eq!(image[13], CODEC_DICT);
        assert_eq!(SlotPage::dict_ref_of(&image).unwrap(), dict_ref);
        let dict_blocks = dir.descriptors()[0].block_count();
        assert!(
            dict_blocks < raw_blocks,
            "dict {dict_blocks} !< raw {raw_blocks} blocks"
        );

        // A fresh directory (cold dict cache) still decodes: it loads the dict
        // from data.bin via the page's own ref.
        let cold = PageDirectory::from_slots(7, 2, dir.descriptors().to_vec());
        for (i, s) in samples.iter().enumerate().take(200) {
            assert_eq!(cold.get(&bf, i as u128).unwrap().unwrap(), *s);
        }
    }

    #[test]
    fn large_value_spills_to_heap_and_roundtrips() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(7, 2, 1);
        let big = vec![0xCDu8; HEAP_SPILL_THRESHOLD + 1];
        dir.put(&bf, 1, &big).unwrap();
        dir.put(&bf, 2, b"small inline").unwrap();

        // The big value spilled: the page holds a 16-byte ref (stays ~1 block),
        // and there is exactly one heap extent.
        assert!(dir.descriptors()[0].block_count() <= 2);
        assert_eq!(dir.heap_runs_in_use(&bf).unwrap().len(), 1);
        // Both values round-trip (the big one resolved from its heap extent).
        assert_eq!(dir.get(&bf, 1).unwrap().unwrap(), big);
        assert_eq!(dir.get(&bf, 2).unwrap().unwrap(), b"small inline");

        // Overwrite the big value with another big value → old extent freed,
        // new one written; still exactly one heap extent.
        let big2 = vec![0xEEu8; HEAP_SPILL_THRESHOLD + 9999];
        dir.put(&bf, 1, &big2).unwrap();
        assert_eq!(dir.get(&bf, 1).unwrap().unwrap(), big2);
        assert_eq!(dir.heap_runs_in_use(&bf).unwrap().len(), 1);

        // Overwrite with a small value → entry goes inline, extent orphaned.
        dir.put(&bf, 1, b"now small").unwrap();
        assert_eq!(dir.get(&bf, 1).unwrap().unwrap(), b"now small");
        assert_eq!(dir.heap_runs_in_use(&bf).unwrap().len(), 0);

        // A cold directory resolves the remaining records straight from disk.
        let cold = PageDirectory::from_slots(7, 2, dir.descriptors().to_vec());
        assert_eq!(cold.get(&bf, 2).unwrap().unwrap(), b"small inline");
    }

    #[test]
    fn grow_to_preserves_records_and_shrinks_pages() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(7, 2,2);
        let payload = vec![0x5Au8; 500];
        for id in 0..40u128 {
            dir.put(&bf, id, &payload).unwrap();
        }
        let max_before = dir
            .descriptors()
            .iter()
            .map(|d| d.block_count())
            .max()
            .unwrap();

        dir.grow_to(&bf, 16).unwrap();
        assert_eq!(dir.slot_count(), 16);

        for id in 0..40u128 {
            assert_eq!(dir.get(&bf, id).unwrap().unwrap(), payload);
        }
        // Spreading 40 records over 8× the slots makes each page smaller.
        let max_after = dir
            .descriptors()
            .iter()
            .map(|d| d.block_count())
            .max()
            .unwrap();
        assert!(max_after <= max_before, "{max_after} !<= {max_before}");
        // Old runs were released back to the allocator.
        assert!(bf.free_blocks() > 0);
    }

    #[test]
    fn grow_doubles_slot_count() {
        let bf = BlockFile::create_in_memory();
        let mut dir = PageDirectory::new(7, 2,3);
        dir.put(&bf, 1, b"a").unwrap();
        dir.grow(&bf).unwrap();
        assert_eq!(dir.slot_count(), 6);
        assert_eq!(dir.get(&bf, 1).unwrap().unwrap(), b"a");
    }

    #[test]
    fn needs_grow_heuristic() {
        // Both pages large (≥128 blocks) and full (occ ≥ 48) → grow.
        let hot = PageDirectory::from_slots(7, 2,vec![
            PageDescriptor::from_run(BlockRun { start: 0, len: 200 }, 50),
            PageDescriptor::from_run(BlockRun { start: 200, len: 130 }, 48),
        ]);
        assert!(hot.needs_grow());

        // Small pages → no grow.
        let cool = PageDirectory::from_slots(7, 2,vec![
            PageDescriptor::from_run(BlockRun { start: 0, len: 2 }, 10),
            PageDescriptor::EMPTY,
        ]);
        assert!(!cool.needs_grow());

        // Only half the slots are hot (< 75%) → no grow.
        let mixed = PageDirectory::from_slots(7, 2,vec![
            PageDescriptor::from_run(BlockRun { start: 0, len: 200 }, 50),
            PageDescriptor::from_run(BlockRun { start: 200, len: 2 }, 10),
        ]);
        assert!(!mixed.needs_grow());
    }

    #[test]
    fn survives_block_file_reopen() {
        let dir_tmp = tempdir().unwrap();
        let path = dir_tmp.path().join("data.bin");
        let mut dir = PageDirectory::new(7, 2,4);
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
        Grow,
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            5 => (0u128..8, prop::collection::vec(any::<u8>(), 0..40))
                .prop_map(|(id, p)| Op::Put(id, p)),
            3 => (0u128..8).prop_map(Op::Remove),
            1 => Just(Op::Grow),
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
            let mut dir = PageDirectory::new(7, 2,2);
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
                    Op::Grow => {
                        // Cap the doubling so a long op sequence can't blow up
                        // the slot count.
                        if dir.slot_count() < 64 {
                            dir.grow(&bf).unwrap();
                        }
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
