//! Overflow heap file — append-only, 4 KiB-aligned entries.
//!
//! Used for variable-length payloads that exceed `max_heap_inline`. A 4 KiB
//! alignment guarantee means every read is a single, non-straddling OS page
//! fetch regardless of the actual value size.
//!
//! # File layout
//!
//! ```text
//! ┌───────────── first 4 KiB block (header) ──────────────────┐
//! │ [u32 magic = 0x48455001]  [u64 cursor]  [padding]         │
//! └────────────────────────────────────────────────────────────┘
//! ┌───────────── entry at offset 4096 ────────────────────────┐
//! │ [u32 data_size]  [data bytes]  [padding to 4 KiB boundary] │
//! └────────────────────────────────────────────────────────────┘
//! ┌───────────── entry at next 4 KiB boundary ────────────────┐
//! │ ...                                                        │
//! └────────────────────────────────────────────────────────────┘
//! ```
//!
//! `cursor` in the header is the byte offset where the **next** entry will be
//! written. It is always 4 KiB-aligned.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

const MAGIC: u32 = 0x4845_5001; // "HEP\x01"
const BLOCK: u64 = 4096;

// ─── HeapPtr ─────────────────────────────────────────────────────────────────

/// A stable pointer into the heap file.
///
/// `offset` is always 4 KiB-aligned; `size` is the unpadded data length.
/// A bounded **2 IOs** suffices to read any value: one for this pointer
/// (if stored in the data file) and one for the actual bytes here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapPtr {
    /// Byte offset of the entry (4 KiB-aligned).
    pub offset: u64,
    /// Unpadded data length in bytes.
    pub size: u32,
}

impl HeapPtr {
    /// Encode as 12 bytes: `[u64 offset][u32 size]` (LE).
    pub fn to_bytes(self) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[0..8].copy_from_slice(&self.offset.to_le_bytes());
        out[8..12].copy_from_slice(&self.size.to_le_bytes());
        out
    }

    /// Decode from 12 bytes.
    pub fn from_bytes(b: &[u8; 12]) -> Self {
        Self {
            offset: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            size: u32::from_le_bytes(b[8..12].try_into().unwrap()),
        }
    }
}

// ─── HeapFile ─────────────────────────────────────────────────────────────────

/// Append-only heap file with 4 KiB-aligned entries.
pub struct HeapFile {
    file: File,
    /// Next write position (always 4 KiB-aligned; starts at BLOCK = 4096).
    cursor: u64,
}

impl HeapFile {
    /// Open an existing heap file or create a new one.
    pub fn open_or_create(path: &Path) -> io::Result<Self> {
        if path.exists() {
            let mut file = OpenOptions::new().read(true).write(true).open(path)?;
            let cursor = read_cursor(&mut file)?;
            Ok(Self { file, cursor })
        } else {
            let mut file =
                OpenOptions::new().create(true).read(true).write(true).open(path)?;
            let cursor = BLOCK; // first entry starts at 4096 (after the header block)
            write_cursor(&mut file, cursor)?;
            Ok(Self { file, cursor })
        }
    }

    /// Append data to the heap. Returns a [`HeapPtr`] for later retrieval.
    ///
    /// Each entry occupies at least one 4 KiB block:
    /// `[u32 data_size][data bytes][padding]`.
    pub fn append(&mut self, data: &[u8]) -> io::Result<HeapPtr> {
        let entry_start = self.cursor;

        // Write: [u32 size][data]
        let mut header = [0u8; 4];
        header.copy_from_slice(&(data.len() as u32).to_le_bytes());
        self.file.seek(SeekFrom::Start(entry_start))?;
        self.file.write_all(&header)?;
        self.file.write_all(data)?;

        // Pad to the next 4 KiB boundary
        let written = 4 + data.len() as u64;
        let next_aligned = align_up(entry_start + written, BLOCK);
        let padding = (next_aligned - (entry_start + written)) as usize;
        if padding > 0 {
            self.file.write_all(&vec![0u8; padding])?;
        }

        self.cursor = next_aligned;
        write_cursor(&mut self.file, self.cursor)?;

        Ok(HeapPtr { offset: entry_start, size: data.len() as u32 })
    }

    /// Read data at the given [`HeapPtr`].
    pub fn read(&mut self, ptr: HeapPtr) -> io::Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(ptr.offset))?;

        let mut size_buf = [0u8; 4];
        self.file.read_exact(&mut size_buf)?;
        let stored_size = u32::from_le_bytes(size_buf);

        if stored_size != ptr.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("heap ptr size mismatch: stored={stored_size} ptr={}", ptr.size),
            ));
        }

        let mut data = vec![0u8; ptr.size as usize];
        self.file.read_exact(&mut data)?;
        Ok(data)
    }

    /// `fsync` the heap file.
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// Current append cursor (next write position).
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Round `v` up to the next multiple of `align`.
fn align_up(v: u64, align: u64) -> u64 {
    (v + align - 1) & !(align - 1)
}

fn read_cursor(file: &mut File) -> io::Result<u64> {
    let mut buf = [0u8; 12];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut buf)?;
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad heap magic: {magic:#010x}"),
        ));
    }
    Ok(u64::from_le_bytes(buf[4..12].try_into().unwrap()))
}

fn write_cursor(file: &mut File, cursor: u64) -> io::Result<()> {
    let mut buf = [0u8; 4096]; // write full block to ensure alignment
    buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    buf[4..12].copy_from_slice(&cursor.to_le_bytes());
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&buf)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    #[test]
    fn create_and_reopen() {
        let p = tmp("wave_heap_open.bin");
        let _ = std::fs::remove_file(&p);
        {
            let hf = HeapFile::open_or_create(&p).unwrap();
            assert_eq!(hf.cursor(), BLOCK);
        }
        {
            let hf = HeapFile::open_or_create(&p).unwrap();
            assert_eq!(hf.cursor(), BLOCK); // nothing written yet
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn append_and_read_small() {
        let p = tmp("wave_heap_small.bin");
        let _ = std::fs::remove_file(&p);
        let mut hf = HeapFile::open_or_create(&p).unwrap();
        let data = b"hello heap";
        let ptr = hf.append(data).unwrap();
        assert_eq!(ptr.offset, BLOCK); // first entry at 4096
        assert_eq!(ptr.size, data.len() as u32);
        assert!(hf.cursor() > ptr.offset); // cursor advanced

        let got = hf.read(ptr).unwrap();
        assert_eq!(got, data);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn append_multiple_4k_aligned() {
        let p = tmp("wave_heap_multi.bin");
        let _ = std::fs::remove_file(&p);
        let mut hf = HeapFile::open_or_create(&p).unwrap();
        let ptr1 = hf.append(b"first").unwrap();
        let ptr2 = hf.append(b"second entry").unwrap();
        let ptr3 = hf.append(&vec![0xABu8; 8000]).unwrap();

        // All starts must be 4 KiB-aligned
        assert_eq!(ptr1.offset % BLOCK, 0);
        assert_eq!(ptr2.offset % BLOCK, 0);
        assert_eq!(ptr3.offset % BLOCK, 0);

        assert_eq!(hf.read(ptr1).unwrap(), b"first");
        assert_eq!(hf.read(ptr2).unwrap(), b"second entry");
        assert_eq!(hf.read(ptr3).unwrap(), vec![0xABu8; 8000]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reopen_preserves_cursor_and_data() {
        let p = tmp("wave_heap_reopen.bin");
        let _ = std::fs::remove_file(&p);
        let ptr;
        {
            let mut hf = HeapFile::open_or_create(&p).unwrap();
            ptr = hf.append(b"persistent").unwrap();
        }
        {
            let mut hf = HeapFile::open_or_create(&p).unwrap();
            assert!(hf.cursor() > BLOCK);
            let got = hf.read(ptr).unwrap();
            assert_eq!(got, b"persistent");
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn heap_ptr_roundtrip() {
        let ptr = HeapPtr { offset: 8192, size: 1234 };
        let bytes = ptr.to_bytes();
        let decoded = HeapPtr::from_bytes(&bytes);
        assert_eq!(decoded, ptr);
    }
}
