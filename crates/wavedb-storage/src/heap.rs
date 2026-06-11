//! Heap data strategy — content-addressed, deduplicating heap file.
//!
//! Write path is **inline-first**: every heapable value (string, `Vec`,
//! blob) is zstd-compressed and lives inline in its record's page until
//! either it exceeds `max_inline` or page pressure evicts heapables (see
//! the readme section _Heap Data Strategy_).  Values that leave the page
//! land here.
//!
//! # Block format
//!
//! The heap file is an array of **4KB blocks**.  An entry occupies whole
//! blocks:
//!
//! ```text
//! [ size: u64 LE ][ value bytes ][ owner_count: u64 LE ][ owner u128 LE … ]
//! ```
//!
//! padded with zeros to the next block boundary.  The **owner-ID list** at
//! the end names every record that uses this heap data.
//!
//! # Content addressing & dedup
//!
//! An entry's address in the data file is a **heap anchor** whose u128 key
//! is [`content_hash128`] of the stored bytes and whose payload is the
//! single `u64` block position ([`crate::AnchorKind::HeapRef`]).  When a
//! second record stores the same value, no new entry is written — its ID
//! is appended to the owner list ([`HeapFile::add_owner`]).
//!
//! # Growth & relocation
//!
//! Appending an owner uses the entry's tail padding when possible.  When
//! the list outgrows the allocated blocks, the entry either extends in
//! place (if it sits at the file tail) or relocates to the tail — in that
//! case `add_owner` returns the new block index and the caller updates the
//! one `u64` in the heap anchor.  Freed ranges accumulate in
//! [`HeapFile::take_freed`] for the cleanup pass / journal free-space
//! deltas.

use crate::StorageResult;
use crate::compression::heap_zstd;
use crate::file::data::DataFile;
use std::path::{Path, PathBuf};

/// Size of one heap block in bytes.
pub const HEAP_BLOCK_SIZE: u64 = 4096;

/// Maximum size for inline storage (25% of a typical 4KB page).
const DEFAULT_MAX_INLINE: usize = 1024;

/// Byte length of the `size` header field.
const SIZE_HEADER: u64 = 8;
/// Byte length of the `owner_count` field.
const COUNT_FIELD: u64 = 8;
/// Byte length of one owner ID.
const OWNER_BYTES: u64 = 16;

/// 128-bit FNV-1a content hash — the address of a heap entry.
///
/// Not cryptographic: collisions are detected by comparing the stored
/// bytes on every dedup hit ([`dedup_store`]), so a collision degrades to
/// an explicit error instead of silent corruption.
#[must_use]
pub fn content_hash128(bytes: &[u8]) -> u128 {
    const OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013B;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= u128::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Where a heapable value ended up after [`HeapFile::store`] /
/// [`dedup_store`].
#[derive(Debug)]
pub enum HeapPlacement {
    /// Small enough to stay inline in the page (compressed bytes).
    Inline(Vec<u8>),
    /// Stored in the heap file behind a content-hashed heap anchor.
    Heap {
        /// u128 content hash of the stored bytes — the heap anchor's key.
        content_hash: u128,
        /// Block position of the entry — the heap anchor's payload.
        block: u64,
    },
}

/// Parsed offsets of one heap entry. Internal.
struct EntryView {
    /// Byte offset of the entry's first block.
    start: usize,
    /// Length of the value bytes.
    size: usize,
    /// Owner count.
    count: usize,
}

impl EntryView {
    /// Byte offset of the `owner_count` field.
    const fn count_at(&self) -> usize {
        self.start + 8 + self.size
    }
    /// Byte offset of owner `i`.
    const fn owner_at(&self, i: usize) -> usize {
        self.count_at() + 8 + i * 16
    }
    /// Bytes in use (header + value + count + owners).
    const fn used(&self) -> u64 {
        SIZE_HEADER
            + self.size as u64
            + COUNT_FIELD
            + OWNER_BYTES * self.count as u64
    }
}

/// Round `bytes` up to a whole number of heap blocks.
const fn blocks_for(bytes: u64) -> u64 {
    bytes.div_ceil(HEAP_BLOCK_SIZE)
}

/// The heap file manager.
pub struct HeapFile {
    path: PathBuf,
    /// Block array. Length is always a multiple of [`HEAP_BLOCK_SIZE`].
    buffer: Vec<u8>,
    max_inline: usize,
    /// `(start_block, n_blocks)` ranges freed by relocation or death.
    /// Drained by [`Self::take_freed`] for the cleanup pass.
    freed: Vec<(u64, u64)>,
}

impl HeapFile {
    /// Open or create a heap file at the given path.
    pub fn open(path: &Path) -> StorageResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let buffer = if path.exists() {
            std::fs::read(path)?
        } else {
            Vec::new()
        };
        Ok(Self {
            path: path.to_path_buf(),
            buffer,
            max_inline: DEFAULT_MAX_INLINE,
            freed: Vec::new(),
        })
    }

    /// Create an in-memory-only heap (for tests).
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::from("/dev/null"),
            buffer: Vec::new(),
            max_inline: DEFAULT_MAX_INLINE,
            freed: Vec::new(),
        }
    }

    /// Set the max inline size threshold.
    pub const fn set_max_inline(&mut self, max: usize) {
        self.max_inline = max;
    }

    /// Store a value: inline if the compressed bytes fit `max_inline`,
    /// otherwise append a heap entry owned by `owner`.
    ///
    /// This is the **non-deduplicating** primitive — production callers
    /// that hold the [`DataFile`] should use [`dedup_store`], which
    /// collapses identical values onto one entry.
    pub fn store(
        &mut self,
        value: &[u8],
        owner: u128,
    ) -> StorageResult<HeapPlacement> {
        let compressed = heap_zstd::compress(value)?;
        if compressed.len() <= self.max_inline {
            return Ok(HeapPlacement::Inline(compressed));
        }
        let content_hash = content_hash128(&compressed);
        let block = self.append_entry(&compressed, owner);
        Ok(HeapPlacement::Heap {
            content_hash,
            block,
        })
    }

    /// Append a new entry holding `stored` bytes with a single owner.
    /// Returns the entry's block position.
    pub fn append_entry(&mut self, stored: &[u8], owner: u128) -> u64 {
        let start_block = blocks_for(self.buffer.len() as u64);
        let start = usize::try_from(start_block * HEAP_BLOCK_SIZE)
            .expect("heap offset overflow");

        let used =
            SIZE_HEADER + stored.len() as u64 + COUNT_FIELD + OWNER_BYTES;
        let alloc = usize::try_from(blocks_for(used) * HEAP_BLOCK_SIZE)
            .expect("heap alloc overflow");
        self.buffer.resize(start + alloc, 0);

        self.buffer[start..start + 8]
            .copy_from_slice(&(stored.len() as u64).to_le_bytes());
        self.buffer[start + 8..start + 8 + stored.len()]
            .copy_from_slice(stored);
        let count_at = start + 8 + stored.len();
        self.buffer[count_at..count_at + 8]
            .copy_from_slice(&1u64.to_le_bytes());
        self.buffer[count_at + 8..count_at + 24]
            .copy_from_slice(&owner.to_le_bytes());

        start_block
    }

    /// Read the stored (compressed) bytes of the entry at `block`.
    pub fn read_value(&self, block: u64) -> StorageResult<Vec<u8>> {
        let e = self.entry(block)?;
        Ok(self.buffer[e.start + 8..e.start + 8 + e.size].to_vec())
    }

    /// Read and decompress the value of the entry at `block`.
    ///
    /// Bounded 2-IO on disk: the heap anchor (block position) was one read,
    /// the entry block(s) are the second — the `size` header lives in the
    /// block itself, so no third hop exists.
    pub fn read_decompressed(&self, block: u64) -> StorageResult<Vec<u8>> {
        let stored = self.read_value(block)?;
        heap_zstd::decompress(&stored)
    }

    /// The owner-ID list of the entry at `block`.
    pub fn owners(&self, block: u64) -> StorageResult<Vec<u128>> {
        let e = self.entry(block)?;
        let mut out = Vec::with_capacity(e.count);
        for i in 0..e.count {
            let at = e.owner_at(i);
            let mut b = [0u8; 16];
            b.copy_from_slice(&self.buffer[at..at + 16]);
            out.push(u128::from_le_bytes(b));
        }
        Ok(out)
    }

    /// Append `owner` to the entry's owner list (no-op if already listed).
    ///
    /// Returns `Some(new_block)` when the list outgrew the entry's blocks
    /// and the entry **relocated** to the file tail — the caller must
    /// update the heap anchor's block position.  Returns `None` when the
    /// entry stayed put.
    pub fn add_owner(
        &mut self,
        block: u64,
        owner: u128,
    ) -> StorageResult<Option<u64>> {
        let e = self.entry(block)?;
        for i in 0..e.count {
            let at = e.owner_at(i);
            if self.buffer[at..at + 16] == owner.to_le_bytes() {
                return Ok(None);
            }
        }

        let old_alloc = blocks_for(e.used());
        let new_alloc = blocks_for(e.used() + OWNER_BYTES);

        if new_alloc > old_alloc {
            let entry_end = e.start as u64 + old_alloc * HEAP_BLOCK_SIZE;
            if entry_end == self.buffer.len() as u64 {
                // At the tail — extend in place by one block.
                self.buffer.resize(
                    self.buffer.len()
                        + usize::try_from(HEAP_BLOCK_SIZE)
                            .expect("block size fits usize"),
                    0,
                );
            } else {
                // Mid-file — relocate to the tail and free the old range.
                let value =
                    self.buffer[e.start + 8..e.start + 8 + e.size].to_vec();
                let mut owners = self.owners(block)?;
                owners.push(owner);
                let new_block = self.append_relocated(&value, &owners);
                self.freed.push((block, old_alloc));
                return Ok(Some(new_block));
            }
        }

        let count_at = e.count_at();
        let new_count = e.count as u64 + 1;
        self.buffer[count_at..count_at + 8]
            .copy_from_slice(&new_count.to_le_bytes());
        let at = e.owner_at(e.count);
        self.buffer[at..at + 16].copy_from_slice(&owner.to_le_bytes());
        Ok(None)
    }

    /// Remove `owner` from the entry's list (no-op if absent).
    ///
    /// Returns `true` when the list became empty — the entry is **dead**:
    /// its blocks are recorded as freed and the caller should clear the
    /// heap anchor.
    pub fn remove_owner(
        &mut self,
        block: u64,
        owner: u128,
    ) -> StorageResult<bool> {
        let e = self.entry(block)?;
        let Some(found) = (0..e.count).find(|&i| {
            let at = e.owner_at(i);
            self.buffer[at..at + 16] == owner.to_le_bytes()
        }) else {
            return Ok(e.count == 0);
        };

        // Swap-remove: move the last owner into the freed slot.
        let last = e.count - 1;
        if found != last {
            let (from, to) = (e.owner_at(last), e.owner_at(found));
            let bytes: [u8; 16] =
                self.buffer[from..from + 16].try_into().expect("16 bytes");
            self.buffer[to..to + 16].copy_from_slice(&bytes);
        }
        let count_at = e.count_at();
        self.buffer[count_at..count_at + 8]
            .copy_from_slice(&(last as u64).to_le_bytes());

        if last == 0 {
            self.freed.push((block, blocks_for(e.used())));
            return Ok(true);
        }
        Ok(false)
    }

    /// Drain the `(start_block, n_blocks)` ranges freed since the last
    /// call — input for journal free-space deltas / the cleanup pass.
    pub fn take_freed(&mut self) -> Vec<(u64, u64)> {
        std::mem::take(&mut self.freed)
    }

    /// Flush the in-memory buffer to disk.
    pub fn flush(&self) -> StorageResult<()> {
        if self.path.to_str() == Some("/dev/null") {
            return Ok(());
        }
        std::fs::write(&self.path, &self.buffer)?;
        Ok(())
    }

    /// Current size of the heap file in bytes (multiple of the block size).
    #[must_use]
    pub fn size(&self) -> u64 {
        self.buffer.len() as u64
    }

    // ── Internal ─────────────────────────────────────────────────────────

    /// Append a relocated entry with a full owner list. Returns its block.
    fn append_relocated(&mut self, value: &[u8], owners: &[u128]) -> u64 {
        let start_block = blocks_for(self.buffer.len() as u64);
        let start = usize::try_from(start_block * HEAP_BLOCK_SIZE)
            .expect("heap offset overflow");

        let used = SIZE_HEADER
            + value.len() as u64
            + COUNT_FIELD
            + OWNER_BYTES * owners.len() as u64;
        let alloc = usize::try_from(blocks_for(used) * HEAP_BLOCK_SIZE)
            .expect("heap alloc overflow");
        self.buffer.resize(start + alloc, 0);

        self.buffer[start..start + 8]
            .copy_from_slice(&(value.len() as u64).to_le_bytes());
        self.buffer[start + 8..start + 8 + value.len()].copy_from_slice(value);
        let count_at = start + 8 + value.len();
        self.buffer[count_at..count_at + 8]
            .copy_from_slice(&(owners.len() as u64).to_le_bytes());
        for (i, owner) in owners.iter().enumerate() {
            let at = count_at + 8 + i * 16;
            self.buffer[at..at + 16].copy_from_slice(&owner.to_le_bytes());
        }

        start_block
    }

    /// Parse and bounds-check the entry at `block`.
    fn entry(&self, block: u64) -> StorageResult<EntryView> {
        let start = usize::try_from(block * HEAP_BLOCK_SIZE)
            .map_err(|e| crate::StorageError::Other(e.to_string()))?;
        if start + 16 > self.buffer.len() {
            return Err(crate::StorageError::Other(format!(
                "heap block {block} out of bounds (file len {})",
                self.buffer.len()
            )));
        }
        let size = u64::from_le_bytes(
            self.buffer[start..start + 8].try_into().expect("8 bytes"),
        );
        let size = usize::try_from(size)
            .map_err(|e| crate::StorageError::Other(e.to_string()))?;
        let count_at = start + 8 + size;
        if count_at + 8 > self.buffer.len() {
            return Err(crate::StorageError::Other(format!(
                "heap entry at block {block} truncated: size={size}"
            )));
        }
        let count = u64::from_le_bytes(
            self.buffer[count_at..count_at + 8]
                .try_into()
                .expect("8 bytes"),
        );
        let count = usize::try_from(count)
            .map_err(|e| crate::StorageError::Other(e.to_string()))?;
        if count_at + 8 + count * 16 > self.buffer.len() {
            return Err(crate::StorageError::Other(format!(
                "heap entry at block {block} truncated: owners={count}"
            )));
        }
        Ok(EntryView { start, size, count })
    }
}

// ── Dedup orchestration: heap file + heap anchors in the data file ──────────

/// Store a heapable value with **content-addressed dedup**.
///
/// 1. Compress; if it fits `max_inline`, return [`HeapPlacement::Inline`].
/// 2. Hash the compressed bytes and look up the heap anchor.
/// 3. **Hit** — verify the stored bytes match (collision guard), append
///    `owner` to the entry's owner list, and update the anchor if the
///    entry relocated.  One copy on disk, one more ID in the list.
/// 4. **Miss** — append a new entry and write the heap anchor.
pub fn dedup_store(
    data: &DataFile,
    heap: &mut HeapFile,
    value: &[u8],
    owner: u128,
) -> StorageResult<HeapPlacement> {
    let compressed = heap_zstd::compress(value)?;
    if compressed.len() <= heap.max_inline {
        return Ok(HeapPlacement::Inline(compressed));
    }

    let content_hash = content_hash128(&compressed);
    if let Some(block) = data.read_heap_anchor(content_hash)? {
        if heap.read_value(block)? != compressed {
            return Err(crate::StorageError::Other(format!(
                "content hash collision at {content_hash:#034x}"
            )));
        }
        if let Some(new_block) = heap.add_owner(block, owner)? {
            data.write_heap_anchor(content_hash, new_block)?;
            return Ok(HeapPlacement::Heap {
                content_hash,
                block: new_block,
            });
        }
        return Ok(HeapPlacement::Heap {
            content_hash,
            block,
        });
    }

    let block = heap.append_entry(&compressed, owner);
    data.write_heap_anchor(content_hash, block)?;
    Ok(HeapPlacement::Heap {
        content_hash,
        block,
    })
}

/// Read a heap-stored value back through its content hash: anchor →
/// block → decompress.  The bounded 2-IO read path.
pub fn dedup_read(
    data: &DataFile,
    heap: &HeapFile,
    content_hash: u128,
) -> StorageResult<Option<Vec<u8>>> {
    let Some(block) = data.read_heap_anchor(content_hash)? else {
        return Ok(None);
    };
    heap.read_decompressed(block).map(Some)
}

/// Release one owner's claim on a heap entry.
///
/// Removes `owner` from the owner list.  When the list empties the entry
/// is dead: its blocks are recorded as freed and the heap anchor is
/// tombstoned.  Returns `true` when the entry died.
pub fn release_owner(
    data: &DataFile,
    heap: &mut HeapFile,
    content_hash: u128,
    owner: u128,
) -> StorageResult<bool> {
    let Some(block) = data.read_heap_anchor(content_hash)? else {
        return Ok(false);
    };
    let dead = heap.remove_owner(block, owner)?;
    if dead {
        data.clear_heap_anchor(content_hash)?;
    }
    Ok(dead)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_value_stored_inline() {
        let mut heap = HeapFile::in_memory();
        let placement = heap.store(b"small value", 1).unwrap();
        assert!(matches!(placement, HeapPlacement::Inline(_)));
        assert_eq!(heap.size(), 0, "inline stores never touch the file");
    }

    #[test]
    fn large_value_roundtrips_through_heap() {
        let mut heap = HeapFile::in_memory();
        heap.set_max_inline(10);
        let value = vec![42u8; 10_000];
        match heap.store(&value, 7).unwrap() {
            HeapPlacement::Heap {
                content_hash,
                block,
            } => {
                assert_ne!(content_hash, 0);
                assert_eq!(heap.read_decompressed(block).unwrap(), value);
                assert_eq!(heap.owners(block).unwrap(), vec![7]);
            }
            HeapPlacement::Inline(_) => panic!("expected heap placement"),
        }
    }

    #[test]
    fn entries_are_block_aligned() {
        let mut heap = HeapFile::in_memory();
        let a = heap.append_entry(&[1u8; 100], 1);
        let b = heap.append_entry(&[2u8; 5000], 2);
        let c = heap.append_entry(&[3u8; 100], 3);
        assert_eq!(a, 0);
        assert_eq!(b, 1, "second entry starts on the next block");
        assert_eq!(c, 3, "5000B entry spans 2 blocks");
        assert_eq!(heap.size() % HEAP_BLOCK_SIZE, 0);
    }

    #[test]
    fn add_owner_appends_and_dedupes() {
        let mut heap = HeapFile::in_memory();
        let block = heap.append_entry(b"shared", 1);
        assert_eq!(heap.add_owner(block, 2).unwrap(), None);
        assert_eq!(heap.add_owner(block, 2).unwrap(), None, "idempotent");
        assert_eq!(heap.owners(block).unwrap(), vec![1, 2]);
    }

    #[test]
    fn owner_list_grows_in_place_at_tail() {
        let mut heap = HeapFile::in_memory();
        // 4060B value: 8 + 4060 + 8 + 16 = 4092 used of 4096 — the next
        // owner crosses the block boundary while the entry is at the tail.
        let block = heap.append_entry(&[9u8; 4060], 1);
        assert_eq!(heap.size(), HEAP_BLOCK_SIZE);
        assert_eq!(heap.add_owner(block, 2).unwrap(), None);
        assert_eq!(heap.size(), 2 * HEAP_BLOCK_SIZE, "extended in place");
        assert_eq!(heap.owners(block).unwrap(), vec![1, 2]);
    }

    #[test]
    fn owner_overflow_mid_file_relocates_entry() {
        let mut heap = HeapFile::in_memory();
        let first = heap.append_entry(&[9u8; 4060], 1);
        // A second entry pins the first away from the tail.
        let _pin = heap.append_entry(b"pin", 50);
        let new_block = heap
            .add_owner(first, 2)
            .unwrap()
            .expect("must relocate — not at tail");
        assert_ne!(new_block, first);
        assert_eq!(heap.owners(new_block).unwrap(), vec![1, 2]);
        assert_eq!(heap.read_value(new_block).unwrap(), vec![9u8; 4060]);
        assert_eq!(heap.take_freed(), vec![(first, 1)]);
    }

    #[test]
    fn remove_owner_until_dead() {
        let mut heap = HeapFile::in_memory();
        let block = heap.append_entry(b"refcounted", 1);
        heap.add_owner(block, 2).unwrap();
        assert!(!heap.remove_owner(block, 1).unwrap());
        assert_eq!(heap.owners(block).unwrap(), vec![2]);
        assert!(!heap.remove_owner(block, 99).unwrap());
        assert!(heap.remove_owner(block, 2).unwrap(), "last owner → dead");
        assert_eq!(heap.take_freed(), vec![(block, 1)]);
    }

    #[test]
    fn content_hash_is_deterministic_and_input_sensitive() {
        assert_eq!(content_hash128(b"abc"), content_hash128(b"abc"));
        assert_ne!(content_hash128(b"abc"), content_hash128(b"abd"));
        assert_ne!(content_hash128(b""), 0);
    }

    #[test]
    fn flush_and_reopen_preserves_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heap.bin");
        let block = {
            let mut heap = HeapFile::open(&path).unwrap();
            let block = heap.append_entry(b"durable", 11);
            heap.add_owner(block, 22).unwrap();
            heap.flush().unwrap();
            block
        };
        let heap = HeapFile::open(&path).unwrap();
        assert_eq!(heap.read_value(block).unwrap(), b"durable");
        assert_eq!(heap.owners(block).unwrap(), vec![11, 22]);
    }

    // ── Dedup orchestration through the data file ────────────────────────

    fn data_file() -> DataFile {
        DataFile::open_in_memory(8192).unwrap()
    }

    #[test]
    fn dedup_store_collapses_identical_values() {
        let data = data_file();
        let mut heap = HeapFile::in_memory();
        heap.set_max_inline(10);
        let value: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();

        let first = dedup_store(&data, &mut heap, &value, 1).unwrap();
        let HeapPlacement::Heap {
            content_hash,
            block,
        } = first
        else {
            panic!("expected heap placement");
        };
        let size_after_first = heap.size();

        // Same value, different owner: no new bytes, one more owner.
        let second = dedup_store(&data, &mut heap, &value, 2).unwrap();
        let HeapPlacement::Heap {
            content_hash: h2,
            block: b2,
        } = second
        else {
            panic!("expected heap placement");
        };
        assert_eq!(h2, content_hash);
        assert_eq!(b2, block);
        assert_eq!(heap.size(), size_after_first, "no duplicate bytes");
        assert_eq!(heap.owners(block).unwrap(), vec![1, 2]);

        // 2-IO read path: anchor → block → value.
        let read = dedup_read(&data, &heap, content_hash).unwrap().unwrap();
        assert_eq!(read, value);
    }

    #[test]
    fn release_owner_tombstones_anchor_when_dead() {
        let data = data_file();
        let mut heap = HeapFile::in_memory();
        heap.set_max_inline(0);
        let value = vec![7u8; 9000];

        let HeapPlacement::Heap { content_hash, .. } =
            dedup_store(&data, &mut heap, &value, 1).unwrap()
        else {
            panic!("expected heap placement");
        };
        dedup_store(&data, &mut heap, &value, 2).unwrap();

        assert!(!release_owner(&data, &mut heap, content_hash, 1).unwrap());
        assert!(
            dedup_read(&data, &heap, content_hash).unwrap().is_some(),
            "still alive for owner 2"
        );
        assert!(release_owner(&data, &mut heap, content_hash, 2).unwrap());
        assert!(
            dedup_read(&data, &heap, content_hash).unwrap().is_none(),
            "anchor tombstoned after last owner left"
        );
        assert_eq!(heap.take_freed().len(), 1);
    }

    #[test]
    fn dedup_store_after_relocation_updates_anchor() {
        let data = data_file();
        let mut heap = HeapFile::in_memory();
        heap.set_max_inline(0);

        // Entry whose first block is nearly full, then pin it off the tail.
        // Compressed size is unpredictable, so fill the padding by adding
        // owners until the next add must relocate.
        let value = vec![3u8; 6000];
        let HeapPlacement::Heap {
            content_hash,
            mut block,
        } = dedup_store(&data, &mut heap, &value, 0).unwrap()
        else {
            panic!("expected heap placement");
        };
        let _pin = heap.append_entry(b"pin", 999);

        let mut owner = 1u128;
        let relocated_to = loop {
            match heap.add_owner(block, owner).unwrap() {
                Some(new_block) => break new_block,
                None => owner += 1,
            }
        };
        data.write_heap_anchor(content_hash, relocated_to).unwrap();
        block = relocated_to;

        assert_eq!(data.read_heap_anchor(content_hash).unwrap(), Some(block));
        assert_eq!(
            dedup_read(&data, &heap, content_hash).unwrap().unwrap(),
            value
        );
    }
}
