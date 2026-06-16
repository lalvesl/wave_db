//! The "infinity" tier: large heap values stored in `data.bin`.
//!
//! Data pages cap at `u8` blocks (~1 MB) and dictionary pages at `u16` blocks
//! (~256 MB). A record value bigger than a data page spills here, into a **heap
//! extent**: a CRC-checked, zstd-compressed run located by a [`HeapRef`] — `u48`
//! start block + `u32` block count, up to **16 TB** per value.
//!
//! v1 is **owned, no dedup**: each spilled value gets its own extent, and the
//! data page that owns it stores the `HeapRef` in place of the inline bytes
//! (copy-on-write, like every other page — write the new extent, repoint, free
//! the old). The extent's allocation rides the ordinary journal ledger, so
//! recovery reserves its blocks with no heap directory.
//!
//! # Heap page format
//!
//! ```text
//! [ crc32 u32 ][ codec u8 ][ raw_len u64 ][ stored_len u64 ][ stored bytes ]
//! ```
//!
//! padded to the block boundary. `codec` is raw or zstd (compress, falling back
//! to raw if it doesn't shrink); `raw_len` is the uncompressed length (decode's
//! capacity), `stored_len` the on-disk bytes. The `crc32` covers everything from
//! `codec` through the stored bytes — one checksum over the whole value.

use crate::compression::heap_zstd;
use crate::file::block_alloc::{BlockRun, blocks_for_bytes};
use crate::file::block_file::BlockFile;
use crate::{StorageError, StorageResult};

/// Bits of a [`HeapRef`] holding the start block (`u48`); the next `u32` is the
/// block count.
const START_BITS: u32 = 48;
const START_MASK: u128 = (1u128 << START_BITS) - 1;
const COUNT_MASK: u128 = (1u128 << 32) - 1;

/// heap page header: `crc32(4) + codec(1) + raw_len(8) + stored_len(8)`.
const HEAP_HEADER: usize = 21;

/// Stored bytes are the verbatim value.
const CODEC_RAW: u8 = 0;
/// Stored bytes are zstd-compressed.
const CODEC_ZSTD: u8 = 1;

/// A self-locating pointer to a heap extent: `u48` start block + `u32` block
/// count, packed into a `u128`. `raw() == 0` ⇒ no extent.
///
/// The high 48 bits are reserved (future `in_journal`/`in_memory` staging flags
/// and a kind tag), kept zero in v1.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HeapRef(u128);

impl HeapRef {
    /// The "no extent" ref.
    pub const NONE: Self = Self(0);

    /// Pack a `(start_block, block_count)` into a ref.
    ///
    /// # Panics
    ///
    /// Debug-asserts `start_block` fits `u48`.
    #[must_use]
    pub fn new(start_block: u64, block_count: u32) -> Self {
        debug_assert!(
            u128::from(start_block) <= START_MASK,
            "heap start exceeds u48"
        );
        Self(u128::from(start_block) | (u128::from(block_count) << START_BITS))
    }

    /// The packed `u128` (stored in the owning page's entry).
    #[must_use]
    pub const fn raw(self) -> u128 {
        self.0
    }

    /// Unpack from a stored `u128`.
    #[must_use]
    pub const fn from_raw(raw: u128) -> Self {
        Self(raw)
    }

    /// Whether this is the "no extent" ref.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Start block of the extent.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn start_block(self) -> u64 {
        (self.0 & START_MASK) as u64
    }

    /// Block count of the extent.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn block_count(self) -> u32 {
        ((self.0 >> START_BITS) & COUNT_MASK) as u32
    }

    /// The block run holding the extent.
    #[must_use]
    pub const fn run(self) -> BlockRun {
        BlockRun {
            start: self.start_block(),
            len: self.block_count() as u64,
        }
    }
}

/// Encode a value into a CRC-checked heap-page image (zstd unless it doesn't
/// shrink the bytes).
fn encode_heap_page(value: &[u8]) -> StorageResult<Vec<u8>> {
    let compressed = heap_zstd::compress(value)?;
    let (codec, stored) = if compressed.len() < value.len() {
        (CODEC_ZSTD, compressed)
    } else {
        (CODEC_RAW, value.to_vec())
    };

    let total = HEAP_HEADER + stored.len();
    let mut buf = vec![0u8; total];
    buf[4] = codec;
    buf[5..13].copy_from_slice(&(value.len() as u64).to_le_bytes());
    buf[13..21].copy_from_slice(&(stored.len() as u64).to_le_bytes());
    buf[HEAP_HEADER..].copy_from_slice(&stored);
    let crc = crc32fast::hash(&buf[4..total]);
    buf[0..4].copy_from_slice(&crc.to_le_bytes());
    Ok(buf)
}

/// Decode a heap-page image back to the value, verifying its CRC.
fn decode_heap_page(bytes: &[u8]) -> StorageResult<Vec<u8>> {
    if bytes.len() < HEAP_HEADER {
        return Err(StorageError::Other("heap page too short".into()));
    }
    let codec = bytes[4];
    let raw_len =
        usize::try_from(u64::from_le_bytes(bytes[5..13].try_into().unwrap()))
            .map_err(|_| StorageError::Other("heap raw_len exceeds usize".into()))?;
    let stored_len =
        usize::try_from(u64::from_le_bytes(bytes[13..21].try_into().unwrap()))
            .map_err(|_| {
                StorageError::Other("heap stored_len exceeds usize".into())
            })?;
    let end = HEAP_HEADER + stored_len;
    if bytes.len() < end {
        return Err(StorageError::Other("heap page length out of range".into()));
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
    let stored = &bytes[HEAP_HEADER..end];
    match codec {
        CODEC_RAW => Ok(stored.to_vec()),
        CODEC_ZSTD => {
            let value = heap_zstd::decompress(stored)?;
            if value.len() != raw_len {
                return Err(StorageError::Other(
                    "heap value length mismatch after decompression".into(),
                ));
            }
            Ok(value)
        }
        other => Err(StorageError::Other(format!(
            "unknown heap codec {other}"
        ))),
    }
}

/// Read/write large values as heap extents over a shared [`BlockFile`].
///
/// Stateless: a value resolves straight from `data.bin` via its [`HeapRef`], so
/// (unlike dictionaries) no cache is needed — the owning page already holds the
/// ref.
pub struct HeapStore;

impl HeapStore {
    /// Write `value` to a fresh heap extent and return its [`HeapRef`]. The
    /// caller journals the run's allocation through the ledger and stores the
    /// ref in the owning page.
    pub fn put(file: &BlockFile, value: &[u8]) -> StorageResult<HeapRef> {
        let image = encode_heap_page(value)?;
        let blocks = blocks_for_bytes(image.len() as u64);
        let count = u32::try_from(blocks).map_err(|_| {
            StorageError::Other("heap value exceeds the 16 TB extent limit".into())
        })?;
        let run = file.allocate(blocks)?;
        file.write_run(run, &image)?;
        Ok(HeapRef::new(run.start, count))
    }

    /// Read the value behind `heap_ref`.
    pub fn get(file: &BlockFile, heap_ref: HeapRef) -> StorageResult<Vec<u8>> {
        decode_heap_page(&file.read_run(heap_ref.run())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::page_dir::MAX_BLOCK_COUNT;
    use tempfile::tempdir;

    /// Incompressible bytes (xorshift) so a large value really spans many
    /// blocks instead of zstd-collapsing to one.
    fn incompressible(len: usize) -> Vec<u8> {
        let mut s: u64 = 0x1234_5678_9abc_def1;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s.to_le_bytes()[0]
            })
            .collect()
    }

    #[test]
    fn heap_ref_packs_and_unpacks() {
        let r = HeapRef::new(0x0001_2345_6789, 1_000_000);
        assert_eq!(r.start_block(), 0x0001_2345_6789);
        assert_eq!(r.block_count(), 1_000_000);
        assert_eq!(
            r.run(),
            BlockRun { start: 0x0001_2345_6789, len: 1_000_000 }
        );
        assert!(!r.is_none());
        assert_eq!(HeapRef::from_raw(r.raw()), r);
        assert!(HeapRef::NONE.is_none());
    }

    #[test]
    fn put_get_roundtrip_small() {
        let bf = BlockFile::create_in_memory();
        let value = b"a modest heap value that still spills".to_vec();
        let r = HeapStore::put(&bf, &value).unwrap();
        assert_eq!(HeapStore::get(&bf, r).unwrap(), value);
    }

    #[test]
    fn put_get_large_multiblock_exceeds_data_page_ceiling() {
        let bf = BlockFile::create_in_memory();
        // ~2 MB incompressible → far beyond the 255-block data-page ceiling.
        let value = incompressible(2_000_000);
        let r = HeapStore::put(&bf, &value).unwrap();
        assert!(
            r.block_count() > u32::from(MAX_BLOCK_COUNT),
            "{} should exceed the data-page ceiling {MAX_BLOCK_COUNT}",
            r.block_count()
        );
        assert_eq!(HeapStore::get(&bf, r).unwrap(), value);
    }

    #[test]
    fn empty_value_roundtrips() {
        let bf = BlockFile::create_in_memory();
        let r = HeapStore::put(&bf, b"").unwrap();
        assert!(HeapStore::get(&bf, r).unwrap().is_empty());
    }

    #[test]
    fn corrupt_heap_page_is_rejected() {
        let bf = BlockFile::create_in_memory();
        let r = HeapStore::put(&bf, b"value worth checksumming").unwrap();
        let mut bytes = bf.read_run(r.run()).unwrap();
        bytes[HEAP_HEADER] ^= 0xFF; // flip a byte inside the CRC-covered region
        bf.write_run(r.run(), &bytes).unwrap();
        assert!(matches!(
            HeapStore::get(&bf, r),
            Err(StorageError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn survives_block_file_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let value = incompressible(500_000);
        let r;
        {
            let bf = BlockFile::create(&path).unwrap();
            r = HeapStore::put(&bf, &value).unwrap();
            bf.sync().unwrap();
        }
        // Reopen telling the allocator the extent is live, then read it back.
        let bf = BlockFile::open(&path, [r.run()]).unwrap();
        assert_eq!(HeapStore::get(&bf, r).unwrap(), value);
    }
}
