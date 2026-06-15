//! The physical block device behind the `data` file.
//!
//! [`BlockFile`] is the substrate the per-type page directory sits on: it
//! pairs a [`BlockAllocator`] with the actual file (or an in-memory buffer for
//! tests) and exposes run-granular I/O — allocate a run, write/read its bytes,
//! free it, truncate the tail. The allocator decides *where* blocks live; this
//! type makes those decisions physical.
//!
//! ## What it does / doesn't do
//!
//! - It keeps the file's physical length in lock-step with the allocator's
//!   `capacity` (every `allocate`/`reserve` grows the file, `truncate` shrinks
//!   it).
//! - It does **not** journal and does **not** know about pages or descriptors
//!   — durability and the key→run map are the directory layer's job. On reopen
//!   the caller supplies the set of live runs (gathered from descriptors) so
//!   the allocator reconstructs via [`BlockAllocator::from_used_runs`].
//! - Concurrency is a single coarse lock for now. The per-block mutex the
//!   readme describes is an actor-layer refinement; correctness first.

// Every method holds the one inner lock for its whole (short) body — the
// guard is meant to live to the end of the call, matching the convention in
// `file::data`.
#![allow(clippy::significant_drop_tightening)]

use crate::file::block_alloc::{BlockAllocator, BlockRun, DATA_BLOCK_SIZE};
use crate::{StorageError, StorageResult};
use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Where a [`BlockFile`]'s bytes actually live.
enum Backing {
    /// In-memory buffer — tests and ephemeral caches; never touches disk.
    Memory(Vec<u8>),
    /// A real file handle opened read+write.
    Disk(File),
}

/// Allocator + backing, kept consistent under one lock.
struct Inner {
    alloc: BlockAllocator,
    backing: Backing,
}

/// The `data` file as a block device: a [`BlockAllocator`] over physical
/// storage, with run-granular read/write.
pub struct BlockFile {
    path: PathBuf,
    inner: Mutex<Inner>,
}

fn as_usize(n: u64) -> StorageResult<usize> {
    usize::try_from(n)
        .map_err(|_| StorageError::Other("size exceeds usize".into()))
}

impl Inner {
    /// Set the backing's physical length to exactly `bytes` (grow or shrink).
    fn set_len(&mut self, bytes: u64) -> StorageResult<()> {
        match &mut self.backing {
            Backing::Memory(v) => {
                v.resize(as_usize(bytes)?, 0);
                Ok(())
            }
            Backing::Disk(f) => {
                f.set_len(bytes)?;
                Ok(())
            }
        }
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> StorageResult<()> {
        match &mut self.backing {
            Backing::Memory(v) => {
                let o = as_usize(offset)?;
                v[o..o + data.len()].copy_from_slice(data);
                Ok(())
            }
            Backing::Disk(f) => {
                f.seek(SeekFrom::Start(offset))?;
                f.write_all(data)?;
                Ok(())
            }
        }
    }

    fn read_at(&mut self, offset: u64, len: u64) -> StorageResult<Vec<u8>> {
        let len = as_usize(len)?;
        match &mut self.backing {
            Backing::Memory(v) => {
                let o = as_usize(offset)?;
                Ok(v[o..o + len].to_vec())
            }
            Backing::Disk(f) => {
                f.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; len];
                f.read_exact(&mut buf)?;
                Ok(buf)
            }
        }
    }
}

impl BlockFile {
    /// Create an empty in-memory block file (no filesystem access).
    #[must_use]
    pub fn create_in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            inner: Mutex::new(Inner {
                alloc: BlockAllocator::new(),
                backing: Backing::Memory(Vec::new()),
            }),
        }
    }

    /// Create (or truncate) a fresh, empty block file at `path`.
    pub fn create(path: &Path) -> StorageResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            inner: Mutex::new(Inner {
                alloc: BlockAllocator::new(),
                backing: Backing::Disk(file),
            }),
        })
    }

    /// Reopen an existing block file, reconstructing the allocator from the
    /// set of currently-live (used) runs.
    ///
    /// Capacity is derived from the file's byte length. `used_runs` are the
    /// runs the directory layer knows are in use; everything else becomes
    /// free. For a freshly recovered DB with nothing written, pass an empty
    /// iterator and the whole file reads back as free space.
    pub fn open(
        path: &Path,
        used_runs: impl IntoIterator<Item = BlockRun>,
    ) -> StorageResult<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let capacity = file.metadata()?.len() / DATA_BLOCK_SIZE;
        Ok(Self {
            path: path.to_path_buf(),
            inner: Mutex::new(Inner {
                alloc: BlockAllocator::from_used_runs(capacity, used_runs),
                backing: Backing::Disk(file),
            }),
        })
    }

    /// Path this block file is bound to (empty for in-memory files).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current capacity in blocks (the allocator's high-water mark).
    #[must_use]
    pub fn capacity_blocks(&self) -> u64 {
        let inner = self.inner.lock();
        inner.alloc.capacity()
    }

    /// Current capacity in bytes (`capacity_blocks * 4 KiB`).
    #[must_use]
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_blocks() * DATA_BLOCK_SIZE
    }

    /// Total free blocks across the file.
    #[must_use]
    pub fn free_blocks(&self) -> u64 {
        let inner = self.inner.lock();
        inner.alloc.free_blocks()
    }

    /// Allocate a run of `blocks` contiguous blocks and grow the file to
    /// cover it. Returns the run's location.
    ///
    /// # Panics
    ///
    /// Panics if `blocks == 0` (via the allocator).
    pub fn allocate(&self, blocks: u64) -> StorageResult<BlockRun> {
        let mut inner = self.inner.lock();
        let run = inner.alloc.allocate(blocks);
        let cap = inner.alloc.capacity() * DATA_BLOCK_SIZE;
        inner.set_len(cap)?;
        Ok(run)
    }

    /// Mark a specific run allocated (crash-recovery replay), growing the file
    /// if the run extends past the current tail.
    pub fn reserve(&self, run: BlockRun) -> StorageResult<()> {
        let mut inner = self.inner.lock();
        inner.alloc.reserve(run);
        let cap = inner.alloc.capacity() * DATA_BLOCK_SIZE;
        inner.set_len(cap)
    }

    /// Return a run to the free pool. Physical space is reclaimed lazily by
    /// [`truncate`](Self::truncate); the bytes are left as-is until then.
    pub fn free(&self, run: BlockRun) {
        let mut inner = self.inner.lock();
        inner.alloc.free(run.start, run.len);
    }

    /// Write `data` into the start of `run`. `data` may be shorter than the
    /// run (the tail keeps its previous bytes); it may not be longer.
    pub fn write_run(&self, run: BlockRun, data: &[u8]) -> StorageResult<()> {
        if data.len() as u64 > run.byte_len() {
            return Err(StorageError::Other(format!(
                "write of {} bytes exceeds run capacity {}",
                data.len(),
                run.byte_len()
            )));
        }
        let mut inner = self.inner.lock();
        inner.write_at(run.byte_offset(), data)
    }

    /// Read the full `run.byte_len()` bytes of a run.
    pub fn read_run(&self, run: BlockRun) -> StorageResult<Vec<u8>> {
        let mut inner = self.inner.lock();
        inner.read_at(run.byte_offset(), run.byte_len())
    }

    /// Reclaim trailing free blocks, shrinking the file. Returns the number of
    /// blocks reclaimed (`0` if the tail is in use).
    pub fn truncate(&self) -> StorageResult<u64> {
        let mut inner = self.inner.lock();
        let reclaimed = inner.alloc.truncate();
        if reclaimed > 0 {
            let cap = inner.alloc.capacity() * DATA_BLOCK_SIZE;
            inner.set_len(cap)?;
        }
        Ok(reclaimed)
    }

    /// Flush buffered writes to disk (`fsync`). No-op for in-memory files.
    pub fn sync(&self) -> StorageResult<()> {
        let inner = self.inner.lock();
        if let Backing::Disk(f) = &inner.backing {
            f.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn run_roundtrip(bf: &BlockFile) {
        let r = bf.allocate(2).unwrap();
        assert_eq!(r.len, 2);
        assert_eq!(bf.capacity_blocks(), 2);
        assert_eq!(bf.capacity_bytes(), 2 * DATA_BLOCK_SIZE);

        let payload = b"wavedb block payload";
        bf.write_run(r, payload).unwrap();
        let read = bf.read_run(r).unwrap();
        assert_eq!(read.len() as u64, r.byte_len());
        assert_eq!(&read[..payload.len()], payload);
        // Untouched tail of the run is zero on a fresh allocation.
        assert!(read[payload.len()..].iter().all(|&b| b == 0));
    }

    #[test]
    fn in_memory_roundtrip() {
        run_roundtrip(&BlockFile::create_in_memory());
    }

    #[test]
    fn on_disk_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data.bin");
        run_roundtrip(&BlockFile::create(&path).unwrap());
    }

    #[test]
    fn allocate_grows_physical_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let bf = BlockFile::create(&path).unwrap();
        let _r0 = bf.allocate(3).unwrap();
        let _r1 = bf.allocate(1).unwrap();
        bf.sync().unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, 4 * DATA_BLOCK_SIZE);
        assert_eq!(bf.capacity_bytes(), len);
    }

    #[test]
    fn free_then_truncate_shrinks_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let bf = BlockFile::create(&path).unwrap();
        let _r0 = bf.allocate(2).unwrap();
        let r1 = bf.allocate(2).unwrap(); // tail run [2,4)
        bf.free(r1);
        let reclaimed = bf.truncate().unwrap();
        bf.sync().unwrap();
        assert_eq!(reclaimed, 2);
        assert_eq!(bf.capacity_blocks(), 2);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 2 * DATA_BLOCK_SIZE);
    }

    #[test]
    fn reopen_reconstructs_allocator_from_used_runs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data.bin");

        let (r0, r1);
        {
            let bf = BlockFile::create(&path).unwrap();
            r0 = bf.allocate(2).unwrap(); // [0,2)
            r1 = bf.allocate(3).unwrap(); // [2,5)
            bf.write_run(r0, b"first").unwrap();
            bf.write_run(r1, b"second").unwrap();
            bf.sync().unwrap();
        }

        // Reopen telling it both runs are still live.
        let bf = BlockFile::open(&path, [r0, r1]).unwrap();
        assert_eq!(bf.capacity_blocks(), 5);
        assert_eq!(bf.free_blocks(), 0);
        // Data survived the reopen.
        assert_eq!(&bf.read_run(r0).unwrap()[..5], b"first");
        assert_eq!(&bf.read_run(r1).unwrap()[..6], b"second");
        // A new allocation must not collide with the live runs.
        let r2 = bf.allocate(1).unwrap();
        assert_eq!(r2, BlockRun { start: 5, len: 1 });
    }

    #[test]
    fn write_run_rejects_oversize_payload() {
        let bf = BlockFile::create_in_memory();
        let r = bf.allocate(1).unwrap(); // 4 KiB
        let too_big = vec![0u8; usize::try_from(DATA_BLOCK_SIZE).unwrap() + 1];
        let err = bf.write_run(r, &too_big).unwrap_err();
        assert!(matches!(err, StorageError::Other(_)));
    }

    #[test]
    fn partial_write_preserves_rest_then_overwrites() {
        let bf = BlockFile::create_in_memory();
        let r = bf.allocate(1).unwrap();
        bf.write_run(r, b"abc").unwrap();
        let read = bf.read_run(r).unwrap();
        assert_eq!(&read[..3], b"abc");
        // Rewriting at the start overwrites only the front.
        bf.write_run(r, b"XY").unwrap();
        let read = bf.read_run(r).unwrap();
        assert_eq!(&read[..3], b"XYc");
    }
}
