//! Append-only journal for the per-type page directory.
//!
//! Two jobs, both born from the descriptor's `in_journal` flag (see
//! [`page_dir`](crate::file::page_dir)):
//!
//! 1. **Stage pages.** A page in transition (a relocation during balancing, a
//!    fresh write before it settles) is appended here as a *staged page*. The
//!    directory slot points at it with `in_journal = true` and
//!    `start_block = ` the returned journal address. Its bytes live only in
//!    the journal — not in memory — until the drain copies them into
//!    `data.bin` and rewrites the descriptor.
//! 2. **Record directory mutations.** Every change to a `Vec<PageDescriptor>`
//!    — a slot's `u64` value ([`set_descriptor`]) or the directory length
//!    ([`resize_directory`]) — is journaled, so the directories rebuild on
//!    restart by [replay](PageJournal::open).
//!
//! 3. **Track block allocations.** Every [`allocate_block`] / [`free_block`] is
//!    an append-only ledger entry carrying its owner `(struct_id, version,
//!    slot index)`. Replay folds the ledger into the live-run set
//!    ([`ReplayState::allocated_runs`]) so the [`BlockAllocator`] rebuilds
//!    from the journal alone — independent of the page descriptors, and with
//!    no leaked blocks (every run is owned until explicitly freed). This is the
//!    single source of truth for "what space is in use, and whose"; it removes
//!    the need for any magic superblock in `data.bin`.
//!
//! # Record framing
//!
//! Each record is a tag byte then a fixed (or length-prefixed) body:
//!
//! ```text
//! 0  StagePage      [u64 addr][u32 image_len][image bytes]
//! 1  SetDescriptor  [u32 struct_id][u8 version][u32 slot][u64 descriptor]
//! 2  ResizeDirectory[u32 struct_id][u8 version][u32 slot_count]
//! 3  Checkpoint     [u64 sequence]
//! 6  Allocate       [u32 struct_id][u8 version][u32 slot][u64 start][u64 len]
//! 7  Free           [u64 start][u64 len]
//! ```
//!
//! A staged page's address is a monotonic counter, stored *in* its record, so
//! compaction may drop dead records without renumbering the live ones. Only a
//! small `addr → (offset, len)` index is held in memory — never the page
//! bytes — so reading a staged page is a seek, not a cache lookup.
//!
//! [`set_descriptor`]: PageJournal::set_descriptor
//! [`resize_directory`]: PageJournal::resize_directory
//! [`BlockFile::open`]: crate::file::block_file::BlockFile::open

// Every method holds the one inner lock for its whole (short) body, matching
// `file::block_file` / `file::data`.
#![allow(clippy::significant_drop_tightening)]

use crate::file::block_alloc::BlockRun;
use crate::file::page_dir::{MAX_START_BLOCK, PageDescriptor};
use crate::{StorageError, StorageResult};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const TAG_STAGE: u8 = 0;
const TAG_SET_DESC: u8 = 1;
const TAG_RESIZE: u8 = 2;
const TAG_CHECKPOINT: u8 = 3;
const TAG_WRITE_REC: u8 = 4;
const TAG_DELETE_REC: u8 = 5;
const TAG_ALLOC: u8 = 6;
const TAG_FREE: u8 = 7;

/// Bytes before the image in a `StagePage` record: tag + addr + image_len.
const STAGE_HEADER: u64 = 1 + 8 + 4;

/// Records pending drain: `(struct_id, version)` → `id` → `Some(upsert)` /
/// `None(delete)`.
pub type PendingRecords = BTreeMap<(u32, u8), BTreeMap<u128, Option<Vec<u8>>>>;

/// Who owns a journaled block run: `(struct_id, version, slot index)`.
///
/// Recorded on every [`PageJournal::allocate_block`] so a recovered run can be
/// attributed to the directory entry that holds it (used by group rebalancing
/// to recycle a chunk's pages).
pub type BlockOwner = (u32, u8, u32);

fn as_usize(n: u64) -> StorageResult<usize> {
    usize::try_from(n)
        .map_err(|_| StorageError::Other("size exceeds usize".into()))
}

/// State recovered by replaying a journal.
pub struct ReplayState {
    /// `(struct_id, version)` → its `Vec<PageDescriptor>` page directory
    /// (the committed pages in `data.bin`).
    pub directories: BTreeMap<(u32, u8), Vec<PageDescriptor>>,
    /// Records written but **not yet drained** — everything since the last
    /// checkpoint. `Some(bytes)` = upsert, `None` = delete (tombstone). The
    /// store reloads these into its pending buffer and overlays them on reads.
    pub pending: PendingRecords,
    /// Live block runs per the allocation ledger: every [`allocate_block`] not
    /// since [`free_block`]d. Feed these to [`BlockFile::open`] to rebuild the
    /// allocator straight from the journal, without scanning descriptors.
    ///
    /// [`allocate_block`]: PageJournal::allocate_block
    /// [`free_block`]: PageJournal::free_block
    /// [`BlockFile::open`]: crate::file::block_file::BlockFile::open
    pub allocated_runs: Vec<BlockRun>,
    /// `run start block` → its [`BlockOwner`], for the still-live runs in
    /// [`allocated_runs`](Self::allocated_runs).
    pub block_owners: BTreeMap<u64, BlockOwner>,
}

impl ReplayState {
    /// Block runs of every page that lives in `data.bin` (allocated and not
    /// `in_journal`) — feed these to [`BlockFile::open`] to rebuild the
    /// allocator.
    ///
    /// [`BlockFile::open`]: crate::file::block_file::BlockFile::open
    #[must_use]
    pub fn data_runs(&self) -> Vec<BlockRun> {
        self.directories
            .values()
            .flatten()
            .filter(|d| d.is_allocated() && !d.is_in_journal())
            .map(|d| d.run())
            .collect()
    }

    /// `(addr)` of every page still staged in the journal — referenced by a
    /// slot whose descriptor is `in_journal`.
    #[must_use]
    pub fn journal_addrs(&self) -> Vec<u64> {
        self.directories
            .values()
            .flatten()
            .filter(|d| d.is_allocated() && d.is_in_journal())
            .map(|d| d.journal_addr())
            .collect()
    }
}

enum Backing {
    Memory(Vec<u8>),
    Disk(File),
}

struct Inner {
    backing: Backing,
    /// Logical append position / file length in bytes.
    len: u64,
    /// Next staged-page address to hand out.
    next_addr: u64,
    /// Live staged pages: `addr → (byte offset of image, image length)`.
    staged: BTreeMap<u64, (u64, u32)>,
}

impl Inner {
    fn append(&mut self, bytes: &[u8]) -> StorageResult<()> {
        match &mut self.backing {
            Backing::Memory(v) => v.extend_from_slice(bytes),
            Backing::Disk(f) => {
                f.seek(SeekFrom::Start(self.len))?;
                f.write_all(bytes)?;
            }
        }
        self.len += bytes.len() as u64;
        Ok(())
    }

    fn read_at(&mut self, off: u64, n: u64) -> StorageResult<Vec<u8>> {
        let n = as_usize(n)?;
        match &mut self.backing {
            Backing::Memory(v) => {
                let o = as_usize(off)?;
                Ok(v[o..o + n].to_vec())
            }
            Backing::Disk(f) => {
                f.seek(SeekFrom::Start(off))?;
                let mut buf = vec![0u8; n];
                f.read_exact(&mut buf)?;
                Ok(buf)
            }
        }
    }
}

/// The page directory's append-only journal. See the module docs.
pub struct PageJournal {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl PageJournal {
    /// A fresh in-memory journal (tests, ephemeral use).
    #[must_use]
    pub fn create_in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            inner: Mutex::new(Inner {
                backing: Backing::Memory(Vec::new()),
                len: 0,
                next_addr: 0,
                staged: BTreeMap::new(),
            }),
        }
    }

    /// Create (or truncate) a fresh journal file at `path`.
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
                backing: Backing::Disk(file),
                len: 0,
                next_addr: 0,
                staged: BTreeMap::new(),
            }),
        })
    }

    /// Open an existing journal and replay it, returning the journal (ready
    /// for more appends and staged-page reads) plus the recovered directories.
    pub fn open(path: &Path) -> StorageResult<(Self, ReplayState)> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        let mut data = vec![0u8; as_usize(len)?];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut data)?;

        let Replay {
            staged,
            directories,
            pending,
            next_addr,
            allocated,
        } = replay(&data)?;

        let journal = Self {
            path: path.to_path_buf(),
            inner: Mutex::new(Inner {
                backing: Backing::Disk(file),
                len,
                next_addr,
                staged,
            }),
        };
        let allocated_runs = allocated
            .iter()
            .map(|(&start, &(len, _))| BlockRun { start, len })
            .collect();
        let block_owners =
            allocated.into_iter().map(|(start, (_, owner))| (start, owner)).collect();
        Ok((
            journal,
            ReplayState {
                directories,
                pending,
                allocated_runs,
                block_owners,
            },
        ))
    }

    /// Path this journal is bound to (empty for in-memory journals).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current journal length in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.inner.lock().len
    }

    /// Whether the journal has no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of pages currently staged in the journal.
    #[must_use]
    pub fn staged_count(&self) -> usize {
        self.inner.lock().staged.len()
    }

    /// Append a staged page image, returning its journal address. The caller
    /// points a directory slot at it with `in_journal = true` and
    /// `start_block = addr` (then journals that via [`set_descriptor`]).
    ///
    /// [`set_descriptor`]: Self::set_descriptor
    ///
    /// # Panics
    ///
    /// Debug-asserts the address fits the descriptor's `u48` `start_block`.
    pub fn stage_page(&self, image: &[u8]) -> StorageResult<u64> {
        let mut inner = self.inner.lock();
        let addr = inner.next_addr;
        debug_assert!(addr <= MAX_START_BLOCK, "journal addr exceeds u48");
        let image_len = u32::try_from(image.len())
            .map_err(|_| StorageError::Other("staged page exceeds u32".into()))?;

        let image_off = inner.len + STAGE_HEADER;
        let mut record = Vec::with_capacity(as_usize(STAGE_HEADER)? + image.len());
        record.push(TAG_STAGE);
        record.extend_from_slice(&addr.to_le_bytes());
        record.extend_from_slice(&image_len.to_le_bytes());
        record.extend_from_slice(image);
        inner.append(&record)?;

        inner.next_addr += 1;
        inner.staged.insert(addr, (image_off, image_len));
        Ok(addr)
    }

    /// Read a staged page back by its journal address.
    pub fn read_page(&self, addr: u64) -> StorageResult<Vec<u8>> {
        let mut inner = self.inner.lock();
        let (off, len) = *inner
            .staged
            .get(&addr)
            .ok_or_else(|| StorageError::Other(format!("no staged page at {addr}")))?;
        inner.read_at(off, u64::from(len))
    }

    /// Journal a directory slot's new descriptor value (the durable record of
    /// the `Vec<PageDescriptor>` pointer).
    pub fn set_descriptor(
        &self,
        struct_id: u32,
        version: u8,
        slot: u32,
        descriptor: PageDescriptor,
    ) -> StorageResult<()> {
        let mut record = Vec::with_capacity(1 + 4 + 1 + 4 + 8);
        record.push(TAG_SET_DESC);
        record.extend_from_slice(&struct_id.to_le_bytes());
        record.push(version);
        record.extend_from_slice(&slot.to_le_bytes());
        record.extend_from_slice(&descriptor.raw().to_le_bytes());
        self.inner.lock().append(&record)
    }

    /// Journal a directory's new length (a grow).
    pub fn resize_directory(
        &self,
        struct_id: u32,
        version: u8,
        slot_count: u32,
    ) -> StorageResult<()> {
        let mut record = Vec::with_capacity(1 + 4 + 1 + 4);
        record.push(TAG_RESIZE);
        record.extend_from_slice(&struct_id.to_le_bytes());
        record.push(version);
        record.extend_from_slice(&slot_count.to_le_bytes());
        self.inner.lock().append(&record)
    }

    /// Journal a block allocation, tagging the run with its owner
    /// `(struct_id, version, slot)`. Append this **before** the descriptor that
    /// points at the run, so a crash can never leave a block in use yet absent
    /// from the rebuilt allocator. See [`BlockOwner`].
    pub fn allocate_block(
        &self,
        struct_id: u32,
        version: u8,
        slot: u32,
        run: BlockRun,
    ) -> StorageResult<()> {
        let mut record = Vec::with_capacity(1 + 4 + 1 + 4 + 8 + 8);
        record.push(TAG_ALLOC);
        record.extend_from_slice(&struct_id.to_le_bytes());
        record.push(version);
        record.extend_from_slice(&slot.to_le_bytes());
        record.extend_from_slice(&run.start.to_le_bytes());
        record.extend_from_slice(&run.len.to_le_bytes());
        self.inner.lock().append(&record)
    }

    /// Journal a block free — the run returns to the pool. Append this **after**
    /// the descriptor that stopped pointing at the run (copy-on-write commits
    /// the new page first), so the run is reclaimable only once nothing reads
    /// it.
    pub fn free_block(&self, run: BlockRun) -> StorageResult<()> {
        let mut record = Vec::with_capacity(1 + 8 + 8);
        record.push(TAG_FREE);
        record.extend_from_slice(&run.start.to_le_bytes());
        record.extend_from_slice(&run.len.to_le_bytes());
        self.inner.lock().append(&record)
    }

    /// Journal a record write — the durable side of a `put` before it has
    /// been drained into a page. Held in the store's pending buffer until the
    /// drain merges it into `data.bin`.
    pub fn write_record(
        &self,
        struct_id: u32,
        version: u8,
        id: u128,
        payload: &[u8],
    ) -> StorageResult<()> {
        let len = u32::try_from(payload.len())
            .map_err(|_| StorageError::Other("record exceeds u32".into()))?;
        let mut record = Vec::with_capacity(1 + 4 + 1 + 16 + 4 + payload.len());
        record.push(TAG_WRITE_REC);
        record.extend_from_slice(&struct_id.to_le_bytes());
        record.push(version);
        record.extend_from_slice(&id.to_le_bytes());
        record.extend_from_slice(&len.to_le_bytes());
        record.extend_from_slice(payload);
        self.inner.lock().append(&record)
    }

    /// Journal a record delete (a tombstone pending drain).
    pub fn delete_record(
        &self,
        struct_id: u32,
        version: u8,
        id: u128,
    ) -> StorageResult<()> {
        let mut record = Vec::with_capacity(1 + 4 + 1 + 16);
        record.push(TAG_DELETE_REC);
        record.extend_from_slice(&struct_id.to_le_bytes());
        record.push(version);
        record.extend_from_slice(&id.to_le_bytes());
        self.inner.lock().append(&record)
    }

    /// Journal a drain checkpoint: everything before `sequence` is settled in
    /// `data.bin`; pending records up to here are cleared on replay.
    pub fn checkpoint(&self, sequence: u64) -> StorageResult<()> {
        let mut record = Vec::with_capacity(1 + 8);
        record.push(TAG_CHECKPOINT);
        record.extend_from_slice(&sequence.to_le_bytes());
        self.inner.lock().append(&record)
    }

    /// Flush buffered writes to disk (`fsync`). No-op for in-memory journals.
    pub fn sync(&self) -> StorageResult<()> {
        let inner = self.inner.lock();
        if let Backing::Disk(f) = &inner.backing {
            f.sync_all()?;
        }
        Ok(())
    }
}

struct Replay {
    staged: BTreeMap<u64, (u64, u32)>,
    directories: BTreeMap<(u32, u8), Vec<PageDescriptor>>,
    pending: PendingRecords,
    next_addr: u64,
    /// Live runs from the allocation ledger: `start → (len, owner)`.
    allocated: BTreeMap<u64, (u64, BlockOwner)>,
}

/// Read `n` bytes at `*cursor`, advancing it. Errors if the record runs past
/// the end of the journal (a torn tail write).
fn take<'a>(
    data: &'a [u8],
    cursor: &mut usize,
    n: usize,
) -> StorageResult<&'a [u8]> {
    if *cursor + n > data.len() {
        return Err(StorageError::Other("journal record truncated".into()));
    }
    let slice = &data[*cursor..*cursor + n];
    *cursor += n;
    Ok(slice)
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b.try_into().expect("4 bytes"))
}
fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b.try_into().expect("8 bytes"))
}
fn le_u128(b: &[u8]) -> u128 {
    u128::from_le_bytes(b.try_into().expect("16 bytes"))
}

fn replay(data: &[u8]) -> StorageResult<Replay> {
    let mut staged = BTreeMap::new();
    let mut directories: BTreeMap<(u32, u8), Vec<PageDescriptor>> =
        BTreeMap::new();
    let mut pending: PendingRecords =
        BTreeMap::new();
    let mut allocated: BTreeMap<u64, (u64, BlockOwner)> = BTreeMap::new();
    let mut next_addr = 0u64;
    let mut c = 0usize;

    while c < data.len() {
        let tag = take(data, &mut c, 1)?[0];
        match tag {
            TAG_STAGE => {
                let addr = le_u64(take(data, &mut c, 8)?);
                let image_len = le_u32(take(data, &mut c, 4)?);
                let image_off = c as u64;
                take(data, &mut c, image_len as usize)?; // skip the image bytes
                staged.insert(addr, (image_off, image_len));
                next_addr = next_addr.max(addr + 1);
            }
            TAG_SET_DESC => {
                let struct_id = le_u32(take(data, &mut c, 4)?);
                let version = take(data, &mut c, 1)?[0];
                let slot = le_u32(take(data, &mut c, 4)?) as usize;
                let descriptor = le_u64(take(data, &mut c, 8)?);
                let dir = directories.entry((struct_id, version)).or_default();
                if slot >= dir.len() {
                    dir.resize(slot + 1, PageDescriptor::EMPTY);
                }
                dir[slot] = PageDescriptor::from_raw(descriptor);
            }
            TAG_RESIZE => {
                let struct_id = le_u32(take(data, &mut c, 4)?);
                let version = take(data, &mut c, 1)?[0];
                let slot_count = le_u32(take(data, &mut c, 4)?) as usize;
                let dir = directories.entry((struct_id, version)).or_default();
                if slot_count > dir.len() {
                    dir.resize(slot_count, PageDescriptor::EMPTY);
                }
            }
            TAG_WRITE_REC => {
                let struct_id = le_u32(take(data, &mut c, 4)?);
                let version = take(data, &mut c, 1)?[0];
                let id = le_u128(take(data, &mut c, 16)?);
                let len = le_u32(take(data, &mut c, 4)?);
                let payload = take(data, &mut c, len as usize)?.to_vec();
                pending
                    .entry((struct_id, version))
                    .or_default()
                    .insert(id, Some(payload));
            }
            TAG_DELETE_REC => {
                let struct_id = le_u32(take(data, &mut c, 4)?);
                let version = take(data, &mut c, 1)?[0];
                let id = le_u128(take(data, &mut c, 16)?);
                pending
                    .entry((struct_id, version))
                    .or_default()
                    .insert(id, None);
            }
            TAG_CHECKPOINT => {
                take(data, &mut c, 8)?; // sequence
                // Everything before a checkpoint is drained into data.bin.
                pending.clear();
            }
            TAG_ALLOC => {
                let struct_id = le_u32(take(data, &mut c, 4)?);
                let version = take(data, &mut c, 1)?[0];
                let slot = le_u32(take(data, &mut c, 4)?);
                let start = le_u64(take(data, &mut c, 8)?);
                let len = le_u64(take(data, &mut c, 8)?);
                allocated.insert(start, (len, (struct_id, version, slot)));
            }
            TAG_FREE => {
                let start = le_u64(take(data, &mut c, 8)?);
                let _len = le_u64(take(data, &mut c, 8)?);
                allocated.remove(&start);
            }
            other => {
                return Err(StorageError::Other(format!(
                    "unknown journal tag {other}"
                )));
            }
        }
    }

    Ok(Replay {
        staged,
        directories,
        pending,
        next_addr,
        allocated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn stage_and_read_in_memory() {
        let j = PageJournal::create_in_memory();
        let a = j.stage_page(b"page-a").unwrap();
        let b = j.stage_page(b"page-bbbb").unwrap();
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(j.read_page(a).unwrap(), b"page-a");
        assert_eq!(j.read_page(b).unwrap(), b"page-bbbb");
        assert_eq!(j.staged_count(), 2);
        assert!(j.read_page(99).is_err());
    }

    #[test]
    fn replay_rebuilds_directory_and_staged_pages() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("page.journal");

        let addr;
        {
            let j = PageJournal::create(&path).unwrap();
            j.resize_directory(7, 2, 4).unwrap();
            addr = j.stage_page(b"staged-image").unwrap();
            // slot 1 points at the staged page (in journal).
            let staged_desc =
                PageDescriptor::new(addr, 1, 5).with_in_journal(true);
            j.set_descriptor(7, 2, 1, staged_desc).unwrap();
            // slot 3 already settled in data.bin.
            let data_desc = PageDescriptor::new(1000, 2, 30);
            j.set_descriptor(7, 2, 3, data_desc).unwrap();
            j.checkpoint(1).unwrap();
            j.sync().unwrap();
        }

        let (j, state) = PageJournal::open(&path).unwrap();
        let recovered = &state.directories[&(7, 2)];
        assert_eq!(recovered.len(), 4);
        assert!(recovered[1].is_in_journal());
        assert_eq!(recovered[1].journal_addr(), addr);
        assert!(!recovered[3].is_in_journal());
        assert_eq!(recovered[3].start_block(), 1000);

        // Staged image is readable after replay (read from the file, not RAM).
        assert_eq!(j.read_page(addr).unwrap(), b"staged-image");
        // next_addr survived: the next stage continues the sequence.
        assert_eq!(j.stage_page(b"more").unwrap(), addr + 1);
    }

    #[test]
    fn data_runs_excludes_journal_pages() {
        let state = ReplayState {
            directories: BTreeMap::from([(
                (7u32, 2u8),
                vec![
                    PageDescriptor::new(10, 2, 30), // data.bin
                    PageDescriptor::new(5, 1, 5).with_in_journal(true), // journal
                    PageDescriptor::EMPTY,          // unallocated
                ],
            )]),
            pending: BTreeMap::new(),
            allocated_runs: Vec::new(),
            block_owners: BTreeMap::new(),
        };
        assert_eq!(state.data_runs(), vec![BlockRun { start: 10, len: 2 }]);
        assert_eq!(state.journal_addrs(), vec![5]);
    }

    #[test]
    fn last_descriptor_write_wins_on_replay() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("page.journal");
        {
            let j = PageJournal::create(&path).unwrap();
            j.set_descriptor(1, 0, 0, PageDescriptor::new(1, 1, 1)).unwrap();
            // Same slot rewritten (e.g. page grew) — latest must win.
            j.set_descriptor(1, 0, 0, PageDescriptor::new(99, 4, 40)).unwrap();
            j.sync().unwrap();
        }
        let (_j, state) = PageJournal::open(&path).unwrap();
        let d = state.directories[&(1, 0)][0];
        assert_eq!(d.start_block(), 99);
        assert_eq!(d.block_count(), 4);
    }

    #[test]
    fn records_replay_into_pending_until_checkpoint() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("page.journal");
        {
            let j = PageJournal::create(&path).unwrap();
            // Drained batch: cleared by the checkpoint.
            j.write_record(7, 2, 1, b"old").unwrap();
            j.checkpoint(1).unwrap();
            // Un-drained batch: survives as pending.
            j.write_record(7, 2, 2, b"new").unwrap();
            j.write_record(7, 2, 1, b"rewritten").unwrap();
            j.delete_record(7, 2, 9).unwrap();
            j.sync().unwrap();
        }
        let (_j, state) = PageJournal::open(&path).unwrap();
        let pending = &state.pending[&(7, 2)];
        assert_eq!(pending.get(&2), Some(&Some(b"new".to_vec())));
        assert_eq!(pending.get(&1), Some(&Some(b"rewritten".to_vec()))); // post-checkpoint write
        assert_eq!(pending.get(&9), Some(&None)); // tombstone
        assert_eq!(pending.len(), 3);
    }

    #[test]
    fn alloc_ledger_rebuilds_live_runs_and_owners() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("page.journal");
        {
            let j = PageJournal::create(&path).unwrap();
            j.allocate_block(7, 2, 0, BlockRun { start: 0, len: 2 }).unwrap();
            j.allocate_block(7, 2, 1, BlockRun { start: 2, len: 3 }).unwrap();
            j.allocate_block(9, 0, 5, BlockRun { start: 5, len: 1 }).unwrap();
            // Copy-on-write churn: free the first run, re-allocate elsewhere.
            j.free_block(BlockRun { start: 0, len: 2 }).unwrap();
            j.allocate_block(7, 2, 0, BlockRun { start: 6, len: 2 }).unwrap();
            j.sync().unwrap();
        }
        let (_j, state) = PageJournal::open(&path).unwrap();
        // start=0 was freed; the rest stay live (sorted by start).
        assert_eq!(
            state.allocated_runs,
            vec![
                BlockRun { start: 2, len: 3 },
                BlockRun { start: 5, len: 1 },
                BlockRun { start: 6, len: 2 },
            ]
        );
        assert_eq!(state.block_owners.get(&2), Some(&(7, 2, 1)));
        assert_eq!(state.block_owners.get(&5), Some(&(9, 0, 5)));
        assert_eq!(state.block_owners.get(&6), Some(&(7, 2, 0)));
        assert!(!state.block_owners.contains_key(&0)); // freed
    }

    #[test]
    fn truncated_journal_is_rejected() {
        // A torn record (claims a longer image than is present) must error,
        // not silently read garbage.
        let dir = tempdir().unwrap();
        let path = dir.path().join("page.journal");
        {
            let j = PageJournal::create(&path).unwrap();
            j.stage_page(b"complete").unwrap();
            j.sync().unwrap();
        }
        // Corrupt: append a lone StagePage header promising 100 image bytes.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(TAG_STAGE);
        bytes.extend_from_slice(&7u64.to_le_bytes());
        bytes.extend_from_slice(&100u32.to_le_bytes()); // no image follows
        std::fs::write(&path, &bytes).unwrap();

        assert!(PageJournal::open(&path).is_err());
    }
}
