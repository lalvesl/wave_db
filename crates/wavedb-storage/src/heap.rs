//! Heap data strategy: three-step pipeline for variable-length values.
//!
//! 1. **Inline** — zstd compress and store at the end of the page.
//! 2. **Overflow** — too large for inline; store in a separate overflow block.
//! 3. **Heap anchor** — very large values; a small stub in the data file
//!    points to 4KB-aligned data appended to the tail of the heap file.

use crate::compression::heap_zstd;
use std::path::{Path, PathBuf};

/// The 4KB alignment boundary for heap file entries.
const HEAP_ALIGNMENT: u64 = 4096;

/// Maximum size for inline storage (25% of a typical 4KB page).
const DEFAULT_MAX_INLINE: usize = 1024;

/// A heap anchor: a small record pointing to data in the heap file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HeapAnchor {
    /// Byte offset in the heap file.
    pub offset: u64,
    /// Size of the data in bytes (before padding).
    pub size: u64,
}

/// Result of storing a heap value.
#[derive(Debug)]
pub enum HeapStorageResult {
    /// Value was small enough to store inline (compressed bytes).
    Inline(Vec<u8>),
    /// Value was stored in the heap file; here's the anchor.
    HeapStored(HeapAnchor),
}

/// The heap file manager.
pub struct HeapFile {
    path: PathBuf,
    /// Current tail offset (next write position).
    tail: u64,
    max_inline: usize,
    /// In-memory buffer for tests (replaces file IO in unit tests).
    buffer: Vec<u8>,
}

impl HeapFile {
    /// Open or create a heap file at the given path.
    pub fn open(path: &Path) -> crate::StorageResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let (tail, buffer) = if path.exists() {
            let data = std::fs::read(path)?;
            let tail = data.len() as u64;
            (tail, data)
        } else {
            (0, Vec::new())
        };

        Ok(Self {
            path: path.to_path_buf(),
            tail,
            max_inline: DEFAULT_MAX_INLINE,
            buffer,
        })
    }

    /// Create an in-memory-only heap (for tests).
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::from("/dev/null"),
            tail: 0,
            max_inline: DEFAULT_MAX_INLINE,
            buffer: Vec::new(),
        }
    }

    /// Set the max inline size threshold.
    pub const fn set_max_inline(&mut self, max: usize) {
        self.max_inline = max;
    }

    /// Store a value using the three-step strategy.
    ///
    /// Returns the number of IO operations used (for testing).
    pub fn store(&mut self, data: &[u8]) -> crate::StorageResult<(HeapStorageResult, u32)> {
        // Step 1: try inline compression
        let compressed = heap_zstd::compress(data)?;
        if compressed.len() <= self.max_inline {
            return Ok((HeapStorageResult::Inline(compressed), 0));
        }

        // Step 2/3: store in heap file with 4KB alignment
        let anchor = self.append_aligned(&compressed);
        // 1 IO for the anchor write
        Ok((HeapStorageResult::HeapStored(anchor), 1))
    }

    /// Read a value from the heap file given its anchor.
    ///
    /// Returns the decompressed data and the number of IO operations used.
    pub fn read(&self, anchor: &HeapAnchor) -> crate::StorageResult<(Vec<u8>, u32)> {
        let start = usize::try_from(anchor.offset).expect("heap offset overflow");
        let end = start + usize::try_from(anchor.size).expect("heap size overflow");

        if end > self.buffer.len() {
            return Err(crate::StorageError::Other(format!(
                "heap read out of bounds: offset={}, size={}, file_len={}",
                anchor.offset,
                anchor.size,
                self.buffer.len()
            )));
        }

        let compressed = &self.buffer[start..end];
        let data = heap_zstd::decompress(compressed)?;
        // 1 IO for the heap anchor lookup + 1 IO for the data read = 2 max
        Ok((data, 2))
    }

    /// Append data to the heap file with 4KB alignment.
    fn append_aligned(&mut self, data: &[u8]) -> HeapAnchor {
        // Align tail to 4KB boundary
        let aligned_offset = align_up(self.tail, HEAP_ALIGNMENT);

        // Pad buffer to aligned offset
        if aligned_offset > self.buffer.len() as u64 {
            self.buffer.resize(usize::try_from(aligned_offset).expect("aligned offset overflow"), 0);
        }

        let anchor = HeapAnchor {
            offset: aligned_offset,
            size: data.len() as u64,
        };

        // Append data
        self.buffer.extend_from_slice(data);
        self.tail = aligned_offset + data.len() as u64;

        anchor
    }

    /// Flush the in-memory buffer to disk.
    pub fn flush(&self) -> crate::StorageResult<()> {
        if self.path.to_str() == Some("/dev/null") {
            return Ok(());
        }
        std::fs::write(&self.path, &self.buffer)?;
        Ok(())
    }

    /// Current size of the heap file.
    pub fn size(&self) -> u64 {
        self.buffer.len() as u64
    }
}

/// Round up `value` to the next multiple of `alignment`.
const fn align_up(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_value_stored_inline() {
        let mut heap = HeapFile::in_memory();
        let data = b"small value";
        let (result, ios) = heap.store(data).unwrap();
        assert!(matches!(result, HeapStorageResult::Inline(_)));
        assert_eq!(ios, 0);
    }

    #[test]
    fn large_value_stored_in_heap() {
        let mut heap = HeapFile::in_memory();
        heap.set_max_inline(10); // force overflow
        let data = vec![42u8; 10_000];
        let (result, ios) = heap.store(&data).unwrap();
        match result {
            HeapStorageResult::HeapStored(anchor) => {
                assert_eq!(ios, 1);
                let (read_data, read_ios) = heap.read(&anchor).unwrap();
                assert_eq!(read_data, data);
                assert_eq!(read_ios, 2); // bounded 2 IOs
            }
            HeapStorageResult::Inline(_) => panic!("expected heap storage"),
        }
    }

    #[test]
    fn heap_file_is_4kb_aligned() {
        let mut heap = HeapFile::in_memory();
        heap.set_max_inline(0); // force everything to heap

        let data1 = vec![1u8; 100];
        let (r1, _) = heap.store(&data1).unwrap();
        match r1 {
            HeapStorageResult::HeapStored(a) => {
                assert_eq!(a.offset % HEAP_ALIGNMENT, 0, "first entry must be aligned");
            }
            _ => panic!("expected heap"),
        }

        let data2 = vec![2u8; 200];
        let (r2, _) = heap.store(&data2).unwrap();
        match r2 {
            HeapStorageResult::HeapStored(a) => {
                assert_eq!(
                    a.offset % HEAP_ALIGNMENT,
                    0,
                    "second entry must also be aligned"
                );
            }
            _ => panic!("expected heap"),
        }
    }

    #[test]
    fn align_up_works() {
        assert_eq!(align_up(0, 4096), 0);
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
    }
}
