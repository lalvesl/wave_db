//! Self-locating dictionaries: the dictionary lives in `data.bin` and a page
//! points at it by position.
//!
//! A trained zstd dictionary is written to `data.bin` like any other page
//! (allocated through the [`BlockAllocator`](crate::file::block_alloc)). Its
//! location *is* its identity: a [`DictRef`] — `u48` start block + `u16` block
//! count — packed into one `u64`. Every [`CODEC_DICT`] page header carries the
//! `DictRef` it was compressed with, so a page is entirely self-describing:
//! decode reads the ref, loads that run, decompresses.
//!
//! [`DictStore`] is a per-directory **cache only** — `ref → bytes`. A miss
//! reads the dict page from `data.bin` (the same strategy reads use for record
//! pages); nothing about dictionaries is journaled. The allocation of the dict
//! run is tracked by the ordinary allocation ledger, so recovery keeps the
//! blocks reserved without any dedicated dict directory.
//!
//! [`CODEC_DICT`]: crate::file::directory

use crate::file::block_alloc::{BlockRun, blocks_for_bytes};
use crate::file::block_file::BlockFile;
use crate::{StorageError, StorageResult};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Bits of a [`DictRef`] holding the start block (`u48`); the high `u16` is the
/// block count.
const START_BITS: u32 = 48;
const START_MASK: u64 = (1u64 << START_BITS) - 1;

/// dict page header: `crc32(4) + dict_len(4)`, then the raw dictionary bytes.
const DICT_HEADER: usize = 8;

/// A self-locating dictionary pointer: `u48` start block + `u16` block count.
///
/// The raw `u64` `0` means **no dictionary** (the page is stored raw). A real
/// dictionary always has a non-zero block count, so it never collides with `0`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DictRef(u64);

impl DictRef {
    /// The "no dictionary" ref.
    pub const NONE: Self = Self(0);

    /// Pack a `(start_block, block_count)` into a ref.
    ///
    /// # Panics
    ///
    /// Debug-asserts `start_block` fits `u48`.
    #[must_use]
    pub fn new(start_block: u64, block_count: u16) -> Self {
        debug_assert!(start_block <= START_MASK, "dict start exceeds u48");
        Self(start_block | (u64::from(block_count) << START_BITS))
    }

    /// The packed `u64` (stored in the page header).
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Unpack from the page header's `u64`.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Whether this is the "no dictionary" ref.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Start block of the dictionary's run.
    #[must_use]
    pub const fn start_block(self) -> u64 {
        self.0 & START_MASK
    }

    /// Block count of the dictionary's run.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn block_count(self) -> u16 {
        (self.0 >> START_BITS) as u16
    }

    /// The block run holding the dictionary.
    #[must_use]
    pub const fn run(self) -> BlockRun {
        BlockRun {
            start: self.start_block(),
            len: self.block_count() as u64,
        }
    }
}

/// Encode a dictionary into a CRC-checked dict-page image.
fn encode_dict_page(dict: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; DICT_HEADER + dict.len()];
    buf[4..8].copy_from_slice(
        &u32::try_from(dict.len()).expect("dict exceeds u32").to_le_bytes(),
    );
    buf[DICT_HEADER..].copy_from_slice(dict);
    let crc = crc32fast::hash(&buf[4..]);
    buf[0..4].copy_from_slice(&crc.to_le_bytes());
    buf
}

/// Decode a dict-page image back to the dictionary bytes, verifying its CRC.
fn decode_dict_page(bytes: &[u8]) -> StorageResult<Vec<u8>> {
    if bytes.len() < DICT_HEADER {
        return Err(StorageError::Other("dict page too short".into()));
    }
    let len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let end = DICT_HEADER + len;
    if bytes.len() < end {
        return Err(StorageError::Other("dict page length out of range".into()));
    }
    let expected = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let actual = crc32fast::hash(&bytes[4..end]);
    if expected != actual {
        return Err(StorageError::ChecksumMismatch {
            page_index: 0,
            expected,
            actual,
        });
    }
    Ok(bytes[DICT_HEADER..end].to_vec())
}

/// Per-directory dictionary cache: `ref → bytes`, backed by `data.bin`.
///
/// Pure cache — a miss reads the dict page from the file. The `current` ref is
/// the dictionary new pages compress against ([`NONE`](DictRef::NONE) ⇒ pages
/// stored raw).
pub(crate) struct DictStore {
    current: DictRef,
    cache: RwLock<HashMap<u64, Arc<Vec<u8>>>>,
}

impl DictStore {
    pub(crate) fn new() -> Self {
        Self {
            current: DictRef::NONE,
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// The dictionary new pages compress against.
    pub(crate) const fn current(&self) -> DictRef {
        self.current
    }

    /// Adopt an existing dict (located by ref) as current without writing —
    /// used on recovery so new pages keep compressing against the dict already
    /// in `data.bin`. Bytes load lazily on first use.
    pub(crate) const fn adopt(&mut self, dict_ref: DictRef) {
        self.current = dict_ref;
    }

    /// Load the bytes for `dict_ref` — cache hit, else read the dict page from
    /// `data.bin` and cache it. `NONE` is a programming error here.
    pub(crate) fn load(
        &self,
        file: &BlockFile,
        dict_ref: DictRef,
    ) -> StorageResult<Arc<Vec<u8>>> {
        debug_assert!(!dict_ref.is_none(), "load of the empty dict ref");
        if let Some(bytes) = self.cache.read().get(&dict_ref.raw()) {
            return Ok(Arc::clone(bytes));
        }
        let bytes = Arc::new(decode_dict_page(&file.read_run(dict_ref.run())?)?);
        self.cache.write().insert(dict_ref.raw(), Arc::clone(&bytes));
        Ok(bytes)
    }

    /// Train-then-install: write `dict` to `data.bin`, make it the current
    /// dictionary, and cache its bytes. Returns its [`DictRef`] — the caller
    /// journals the run's allocation through the ordinary ledger.
    pub(crate) fn install(
        &mut self,
        file: &BlockFile,
        dict: &[u8],
    ) -> StorageResult<DictRef> {
        let image = encode_dict_page(dict);
        let blocks = blocks_for_bytes(image.len() as u64);
        let count = u16::try_from(blocks)
            .map_err(|_| StorageError::Other("dictionary too large".into()))?;
        let run = file.allocate(blocks)?;
        file.write_run(run, &image)?;
        let dict_ref = DictRef::new(run.start, count);
        self.cache
            .write()
            .insert(dict_ref.raw(), Arc::new(dict.to_vec()));
        self.current = dict_ref;
        Ok(dict_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dict_ref_packs_and_unpacks() {
        let r = DictRef::new(0x0001_2345_6789, 7);
        assert_eq!(r.start_block(), 0x0001_2345_6789);
        assert_eq!(r.block_count(), 7);
        assert_eq!(r.run(), BlockRun { start: 0x0001_2345_6789, len: 7 });
        assert!(!r.is_none());
        assert_eq!(DictRef::from_raw(r.raw()), r);
        assert!(DictRef::NONE.is_none());
    }

    #[test]
    fn install_then_load_roundtrips_and_caches() {
        let bf = BlockFile::create_in_memory();
        let mut store = DictStore::new();
        let dict = vec![0xABu8; 3000];

        let r = store.install(&bf, &dict).unwrap();
        assert_eq!(store.current(), r);
        // Cache hit.
        assert_eq!(&*store.load(&bf, r).unwrap(), &dict);

        // Fresh store (cold cache) loads from data.bin via the ref alone.
        let cold = DictStore::new();
        assert_eq!(&*cold.load(&bf, r).unwrap(), &dict);
    }

    #[test]
    fn corrupt_dict_page_is_rejected() {
        let bf = BlockFile::create_in_memory();
        let mut store = DictStore::new();
        let r = store.install(&bf, b"dictionary bytes here").unwrap();
        // Corrupt a byte inside the CRC-covered region.
        let mut bytes = bf.read_run(r.run()).unwrap();
        bytes[DICT_HEADER] ^= 0xFF;
        bf.write_run(r.run(), &bytes).unwrap();
        let cold = DictStore::new();
        assert!(matches!(
            cold.load(&bf, r),
            Err(StorageError::ChecksumMismatch { .. })
        ));
    }
}
