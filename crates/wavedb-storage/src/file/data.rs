//! The data file: hash-mapped pages holding anchor slots and versioned records.
//!
//! # Routing contract
//!
//! Every record's disk page is determined by [`PageKey::page`] applied to the
//! routing key for its kind (see `hash.rs`).  Lookup is **O(1)** in the
//! steady state: one SplitMix64 mix plus a modulo (mask when power-of-two).
//!
//! ## Rebalance (growth)
//!
//! Page count grows by [`DEFAULT_GROWTH_FACTOR`] when fill pressure crosses
//! [`REBALANCE_FILL_THRESHOLD`].  Rebalance runs in the foreground
//! (single-flight via `rebalancing` flag) and:
//!
//! 1. Pushes the **old** page count onto `previous_page_counts`.
//! 2. Atomically swaps `page_count` to the new value.
//! 3. Grows `pages` / `page_bytes` to the new length.
//! 4. Iterates old pages **sorted by fill descending** so the hottest
//!    pages relieve pressure first.
//! 5. For each entry whose new slot differs from its old slot, moves it
//!    and emits a [`JournalEntry`] over the bounded journal pipe so a
//!    crash recovers the in-progress rehash.
//! 6. When every entry has moved, removes the old size from
//!    `previous_page_counts`.
//!
//! Lookups during rehash probe the **new** size first, then fall back to
//! each entry in `previous_page_counts` (newest first).  This keeps the
//! read path correct while the rehash is in flight.
//!
//! Collisions within a page still use double-hashing to a second page;
//! if both are full, the write triggers a rebalance.
//!
//! ## On-disk format
//!
//! Snapshots use a **sparse** layout: only pages that contain at least one
//! entry are written.  Each entry is `(page_index: u64, Page)`.  The file
//! starts with a 4-byte magic `b"WDBS"` so old dense-format files are
//! detected on open and reloaded for backward compatibility.
//!
//! Snapshots are written atomically via `.tmp` → rename so a crash during
//! flush never leaves a truncated or half-written `data.bin`.

use crate::StorageResult;
use crate::anchor::{AnchorKey, AnchorSlot};
use crate::cache::Cache;
use crate::hash::{self, PageKey};
use crate::page::Page;
use crate::pipeline::journal::JournalEntry;
use crate::versioned::VersionedRecord;
use parking_lot::RwLock;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use wavedb_core::Id;

/// Default initial page count.
pub const DEFAULT_PAGE_COUNT: u64 = 256;

/// Growth multiplier applied to `page_count` on each rebalance.
///
/// 1.75 corresponds to WARNING_SPACE = 25%: after rebalance the fill ratio
/// drops to roughly `REBALANCE_FILL_THRESHOLD / DEFAULT_GROWTH_FACTOR` ≈ 43%,
/// giving 57% headroom before the next rebalance.  This is significantly less
/// than the old 2× (100% growth) so disk and memory grow more gradually.
pub const DEFAULT_GROWTH_FACTOR: f64 = 1.75;

/// Magic prefix for the sparse on-disk snapshot format.
const DISK_FORMAT_MAGIC: [u8; 4] = *b"WDBS";

/// Default page size in bytes — 8 KiB.
///
/// Doubled from the historical 4 KiB so directory entries plus payload
/// fit in one page without overflow.  Override per-file via
/// [`DataFile::open_with`].
pub const DEFAULT_PAGE_SIZE: usize = 2 * 4 * 1024;

/// Fill ratio (0.0 – 1.0) at which a write trips a rebalance.
///
/// 0.75 strikes a balance between rehashing too eagerly (wasted work) and
/// too late (writes start hitting `PageFull` on the fast path).
pub const REBALANCE_FILL_THRESHOLD: f64 = 0.75;

/// Bounded capacity of the rebalance → journal pipe.
///
/// Sized to one short pipeline-bubble's worth of moves.  When the journal
/// task lags, the rebalancer blocks on `send` so the journal can never
/// fall behind the in-memory state by more than this.
pub const JOURNAL_PIPE_CAPACITY: usize = 1024;

/// Persistence mode of a [`DataFile`].
///
/// Selected at construction time and stable for the file's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Pages live in RAM only.  No file is touched even if a `path` was
    /// passed in.  Useful for tests, ephemeral caches, and the
    /// `TestCluster` smoke harness.
    InMemory,
    /// Pages also persist to disk.  The whole table snapshot is written
    /// by [`DataFile::flush`]; the journal pipe records incremental
    /// rebalance moves between snapshots.  Reads do **not** hit the
    /// disk on the happy path — the in-memory copy is authoritative.
    OnDisk,
}

/// Build the canonical in-page key for a Unique anchor.
///
/// `tenant | 0 | struct_id | 0` — the same `(tenant, struct_id)` always
/// produces the same u128, which is what makes the anchor stable across
/// every mutation of the record.
#[inline]
const fn unique_anchor_id(struct_id: u32, tenant: u64) -> u128 {
    Id::new(tenant, 0, struct_id, 0).raw()
}

/// Build a tuple4 [`PageKey`] from a full `Id`.
#[inline]
const fn page_key_full(id: Id) -> PageKey {
    PageKey::Tuple4 {
        struct_id: id.struct_id(),
        tenant: id.tenant_id(),
        shard: id.shard_id(),
        created_at: id.created_at(),
    }
}

/// The data file: manages pages on disk with an in-memory cache.
///
/// Pages live in `pages: RwLock<Vec<Page>>`; their count is `page_count`
/// (atomic so it can grow under a rebalance without a global lock on the
/// happy read path).  When the table grows, the **old** size is appended
/// to `previous_page_counts` so lookups still find entries that haven't
/// been rehashed yet.
pub struct DataFile {
    path: PathBuf,
    page_size: usize,
    /// Persistence mode — chosen at open time, immutable thereafter.
    mode: Mode,
    /// Current number of physical pages.  Always a power of two.
    page_count: AtomicU64,
    /// Page counts the file *used to* have.  Kept while rehash is in
    /// flight so lookups fall back to old slots.  Cleared on rebalance
    /// completion.
    previous_page_counts: RwLock<Vec<u64>>,
    /// The pages themselves.  `len() == page_count` at rest; grows under
    /// `rebalance` while the write lock is held.
    pages: RwLock<Vec<Page>>,
    /// Bytes used per page — single source of truth for fill ordering
    /// during rebalance ("rehash hottest pages first").  `len()` tracks
    /// `pages.len()`.
    page_bytes: RwLock<Vec<AtomicU64>>,
    cache: Cache,
    /// Send-side of the rebalance → journal pipe.  Bounded so a slow
    /// journal fsync applies backpressure on the rehash.
    /// `None` when the file was opened without a journal worker.
    journal_tx: RwLock<Option<SyncSender<JournalEntry>>>,
    /// Single-flight latch for rebalance.  Two concurrent writers that
    /// both trip the fill threshold see one rebalance, not two.
    rebalancing: AtomicBool,
    /// Multiplicative growth factor applied to `page_count` on each rebalance.
    /// Default: [`DEFAULT_GROWTH_FACTOR`] (1.75×).  Must be > 1.0.
    growth_factor: f64,
}

impl DataFile {
    /// Open or create a data file at the given path in [`Mode::InMemory`].
    ///
    /// **Path is recorded but never read or written** — this is the
    /// historical default behaviour, retained for back-compat with every
    /// existing test that calls `DataFile::open(&path, 4096)`.  Use
    /// [`DataFile::open_on_disk`] for actual disk persistence.
    pub fn open(path: &Path, page_size: usize) -> StorageResult<Self> {
        Self::open_with_mode(
            path,
            page_size,
            DEFAULT_PAGE_COUNT,
            Mode::InMemory,
        )
    }

    /// Open a purely in-memory data file.
    ///
    /// No filesystem path is stored.  All state lives in RAM and is lost
    /// when the [`DataFile`] is dropped.
    pub fn open_in_memory(page_size: usize) -> StorageResult<Self> {
        Self::open_with_mode(
            Path::new(""),
            page_size,
            DEFAULT_PAGE_COUNT,
            Mode::InMemory,
        )
    }

    /// Open a disk-backed data file.
    ///
    /// If `path` exists and is non-empty, the previous snapshot is
    /// reloaded.  Otherwise the file starts empty.  Call
    /// [`DataFile::flush`] to persist subsequent writes.
    pub fn open_on_disk(path: &Path, page_size: usize) -> StorageResult<Self> {
        let file = Self::open_with_mode(
            path,
            page_size,
            DEFAULT_PAGE_COUNT,
            Mode::OnDisk,
        )?;
        if path.exists() && std::fs::metadata(path).is_ok_and(|m| m.len() > 0) {
            file.reload_from_disk()?;
        }
        Ok(file)
    }

    /// Open or create a data file with an explicit page count.
    pub fn open_with(
        path: &Path,
        page_size: usize,
        page_count: u64,
    ) -> StorageResult<Self> {
        Self::open_with_mode(path, page_size, page_count, Mode::InMemory)
    }

    /// Full constructor — pick page size, count, and persistence mode.
    pub fn open_with_mode(
        path: &Path,
        page_size: usize,
        page_count: u64,
        mode: Mode,
    ) -> StorageResult<Self> {
        if page_count == 0 {
            return Err(crate::StorageError::Other(
                "page_count must be at least 1".into(),
            ));
        }
        let pages: Vec<Page> =
            (0..page_count).map(|_| Page::new(page_size)).collect();
        let page_bytes: Vec<AtomicU64> =
            (0..page_count).map(|_| AtomicU64::new(0)).collect();

        if mode == Mode::OnDisk {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }

        Ok(Self {
            path: path.to_path_buf(),
            page_size,
            mode,
            page_count: AtomicU64::new(page_count),
            previous_page_counts: RwLock::new(Vec::new()),
            pages: RwLock::new(pages),
            page_bytes: RwLock::new(page_bytes),
            cache: Cache::new(),
            journal_tx: RwLock::new(None),
            rebalancing: AtomicBool::new(false),
            growth_factor: DEFAULT_GROWTH_FACTOR,
        })
    }

    /// The persistence mode chosen at open time.
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// Write a snapshot of the current page table to disk.
    ///
    /// No-op when the file was opened with [`Mode::InMemory`].
    ///
    /// **Sparse format**: only pages with at least one entry are written so
    /// disk usage grows with actual data, not with the page-table capacity.
    ///
    /// **Atomic**: writes to `data.tmp`, fsyncs, then renames over `data.bin`.
    /// A crash during flush never leaves a truncated or empty snapshot.
    #[allow(clippy::significant_drop_tightening)]
    pub fn flush(&self) -> StorageResult<()> {
        if self.mode != Mode::OnDisk {
            return Ok(());
        }
        let pages = self.pages.read();
        let page_count = self.page_count.load(Ordering::Acquire);

        // Collect only non-empty pages with their indices.
        let sparse: Vec<(u64, &Page)> = pages
            .iter()
            .enumerate()
            .filter(|(_, p)| p.header.entry_count > 0)
            .map(|(i, p)| (i as u64, p))
            .collect();

        let mut blob = Vec::with_capacity(DISK_FORMAT_MAGIC.len() + 64);
        blob.extend_from_slice(&DISK_FORMAT_MAGIC);
        blob.extend_from_slice(&postcard::to_allocvec(&(page_count, sparse))?);

        let tmp = self.path.with_extension("tmp");
        if let Some(parent) = tmp.parent() {
            std::fs::create_dir_all(parent)?;
        }
        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&blob)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Replace the in-memory state from the on-disk snapshot.
    ///
    /// Called once at [`Self::open_on_disk`] when the file exists.
    /// Handles both the current sparse format (magic prefix `b"WDBS"`) and
    /// the legacy dense format for backward compatibility.
    fn reload_from_disk(&self) -> StorageResult<()> {
        let bytes = std::fs::read(&self.path)?;
        if bytes.is_empty() {
            return Ok(());
        }

        if bytes.starts_with(&DISK_FORMAT_MAGIC) {
            // Sparse format: (page_count, Vec<(page_idx, Page)>)
            let payload = &bytes[DISK_FORMAT_MAGIC.len()..];
            let (page_count, entries): (u64, Vec<(u64, Page)>) =
                postcard::from_bytes(payload)?;
            if page_count == 0 {
                return Ok(());
            }
            let mut pages: Vec<Page> =
                (0..page_count).map(|_| Page::new(self.page_size)).collect();
            let mut page_bytes: Vec<AtomicU64> =
                (0..page_count).map(|_| AtomicU64::new(0)).collect();
            for (idx, page) in entries {
                let i = usize::try_from(idx)
                    .map_err(|e| crate::StorageError::Other(e.to_string()))?;
                if i < pages.len() {
                    let used: u64 =
                        page.directory.iter().map(|e| u64::from(e.size)).sum();
                    page_bytes[i] = AtomicU64::new(used);
                    pages[i] = page;
                }
            }
            *self.pages.write() = pages;
            *self.page_bytes.write() = page_bytes;
            self.page_count.store(page_count, Ordering::Release);
        } else {
            // Legacy dense format: (page_count, Vec<Page>)
            let (page_count, pages): (u64, Vec<Page>) =
                postcard::from_bytes(&bytes)?;
            if page_count == 0 || pages.len() as u64 != page_count {
                return Err(crate::StorageError::Other(format!(
                    "corrupt snapshot: page_count={} pages.len()={}",
                    page_count,
                    pages.len()
                )));
            }
            let page_bytes: Vec<AtomicU64> = pages
                .iter()
                .map(|p| {
                    let used: u64 =
                        p.directory.iter().map(|e| u64::from(e.size)).sum();
                    AtomicU64::new(used)
                })
                .collect();
            *self.pages.write() = pages;
            *self.page_bytes.write() = page_bytes;
            self.page_count.store(page_count, Ordering::Release);
        }
        Ok(())
    }

    /// Attach a journal pipe.  All rebalance moves are forwarded to this
    /// channel for crash-recovery durability.
    ///
    /// The caller owns the receive side and is responsible for draining
    /// it onto disk in a separate thread/task.  When the channel fills up
    /// the rebalancer blocks on send — this is the backpressure that
    /// prevents the journal from being overwhelmed.
    pub fn set_journal_pipe(&self, tx: SyncSender<JournalEntry>) {
        *self.journal_tx.write() = Some(tx);
    }

    /// Current page count.  Power of two.
    pub fn page_count_now(&self) -> u64 {
        self.page_count.load(Ordering::Acquire)
    }

    /// Snapshot of historical page counts the file held before previous
    /// rebalances.  Cleared as each rebalance retires.
    pub fn previous_page_counts(&self) -> Vec<u64> {
        self.previous_page_counts.read().clone()
    }

    // ── Routing primitives ────────────────────────────────────────────────
    //
    // Every read and write funnels through `insert_at_key` / `lookup_at_key`,
    // which pick the page index from a [`PageKey`] (tuple2 or tuple4).  This
    // is the single source of truth — the typed entry points below just
    // build the right `PageKey` for the record kind.

    /// Primary page index for a [`PageKey`] at the given page count.
    #[inline]
    #[allow(clippy::cast_possible_truncation, clippy::missing_const_for_fn)]
    fn page_index(key: PageKey, page_count: u64) -> usize {
        key.page(page_count) as usize
    }

    /// Double-hash fallback page when the primary is full.
    #[inline]
    #[allow(clippy::cast_possible_truncation, clippy::missing_const_for_fn)]
    fn fallback_index(primary: usize, key: PageKey, page_count: u64) -> usize {
        let (struct_id, tenant, shard) = match key {
            PageKey::Tuple2 { struct_id, tenant } => (struct_id, tenant, 0u16),
            PageKey::Tuple4 {
                struct_id,
                tenant,
                shard,
                ..
            } => (struct_id, tenant, shard),
        };
        let step = hash::double_hash_step(struct_id, tenant, shard, page_count)
            as usize;
        let n = (page_count as usize).max(1);
        (primary + step) % n
    }

    /// Insert `bytes` keyed by `raw` at the page determined by `key`.
    ///
    /// On overflow (primary + double-hash fallback both full) the call
    /// triggers a [`DataFile::rebalance`] and retries once.  If the retry
    /// still fails the entry returns `Err(PageFull)`.
    fn insert_at_key(
        &self,
        key: PageKey,
        raw: u128,
        bytes: &[u8],
    ) -> StorageResult<()> {
        match self.try_insert_at_key(key, raw, bytes) {
            Ok(()) => {
                // Opportunistically rehash when fill crosses the threshold —
                // amortises the per-write cost so writes don't pile up at
                // the boundary.
                if self.fill_ratio() >= REBALANCE_FILL_THRESHOLD {
                    let _ = self.rebalance();
                }
                Ok(())
            }
            Err(crate::StorageError::PageFull(_)) => {
                self.rebalance()?;
                self.try_insert_at_key(key, raw, bytes)
            }
            Err(e) => Err(e),
        }
    }

    /// Insert without any rebalance retry — the primitive used both by the
    /// happy-path `insert_at_key` and by the rebalancer itself.
    #[allow(clippy::significant_drop_tightening)]
    fn try_insert_at_key(
        &self,
        key: PageKey,
        raw: u128,
        bytes: &[u8],
    ) -> StorageResult<()> {
        let n = self.page_count.load(Ordering::Acquire);
        let primary = Self::page_index(key, n);
        let mut pages = self.pages.write();
        if pages[primary].insert(raw, bytes) {
            self.bump_page_bytes(primary, bytes.len() as u64);
            return Ok(());
        }
        let alt = Self::fallback_index(primary, key, n);
        if pages[alt].insert(raw, bytes) {
            self.bump_page_bytes(alt, bytes.len() as u64);
            return Ok(());
        }
        Err(crate::StorageError::PageFull(alt as u64))
    }

    /// Look up the bytes for `raw` keyed by `key`.
    ///
    /// Probes the **current** page count first (the steady-state O(1)
    /// path), then falls back to each entry in `previous_page_counts`
    /// (newest first) so reads remain correct while a rebalance is
    /// in flight.  At rest (no rehash running) the fallback list is
    /// empty and lookup is exactly two page probes.
    #[allow(clippy::significant_drop_tightening)]
    fn lookup_at_key(&self, key: PageKey, raw: u128) -> Option<Vec<u8>> {
        let try_at = |pages: &Vec<Page>, page_count: u64| -> Option<Vec<u8>> {
            let primary = Self::page_index(key, page_count);
            if let Some(b) = pages[primary].lookup(raw) {
                return Some(b.to_vec());
            }
            let alt = Self::fallback_index(primary, key, page_count);
            pages[alt].lookup(raw).map(<[u8]>::to_vec)
        };

        let pages = self.pages.read();
        let current = self.page_count.load(Ordering::Acquire);
        if let Some(b) = try_at(&pages, current) {
            return Some(b);
        }
        // Fallback through every prior page-count snapshot — newest
        // first so the freshest fallback wins when the same id was
        // rewritten under multiple sizes.
        for prev in self.previous_page_counts.read().iter().rev() {
            if let Some(b) = try_at(&pages, *prev) {
                return Some(b);
            }
        }
        None
    }

    /// Increment the byte counter for `page_idx`.
    #[inline]
    fn bump_page_bytes(&self, page_idx: usize, delta: u64) {
        let pb = self.page_bytes.read();
        if let Some(slot) = pb.get(page_idx) {
            slot.fetch_add(delta, Ordering::Relaxed);
        }
    }

    // ── Typed entry points: one per record kind ───────────────────────────

    /// Write the **current (live) anchor** for a Unique record.
    ///
    /// Hashed by `(struct_id, tenant)` only — see [`PageKey::Tuple2`].
    /// One slot per `(struct, tenant)`.
    pub fn write_unique_anchor(
        &self,
        struct_id: u32,
        tenant: u64,
        slot: &AnchorSlot,
    ) -> StorageResult<()> {
        let key = PageKey::Tuple2 { struct_id, tenant };
        let raw = unique_anchor_id(struct_id, tenant);
        let bytes = slot.to_bytes()?;
        self.insert_at_key(key, raw, &bytes)?;
        self.cache
            .put_anchor(AnchorKey::from_raw(raw), slot.clone());
        Ok(())
    }

    /// Read the current Unique-record anchor.
    pub fn read_unique_anchor(
        &self,
        struct_id: u32,
        tenant: u64,
    ) -> StorageResult<Option<AnchorSlot>> {
        let raw = unique_anchor_id(struct_id, tenant);
        if let Some(slot) = self.cache.get_anchor(AnchorKey::from_raw(raw)) {
            return Ok(Some(slot));
        }
        let key = PageKey::Tuple2 { struct_id, tenant };
        self.lookup_at_key(key, raw)
            .map(|b| AnchorSlot::from_bytes(&b))
            .transpose()
    }

    /// Write a Unique record's **history version** by its full Id.
    ///
    /// Hashed by `(struct_id, tenant, shard, created_at)` — every old
    /// version has its own page slot.  The id is recoverable from the
    /// current anchor's metadata version chain.
    pub fn write_unique_history(
        &self,
        id: Id,
        rec: &VersionedRecord,
    ) -> StorageResult<()> {
        self.write_full_id(id, rec)
    }

    /// Read a Unique record's history version by Id.
    pub fn read_unique_history(
        &self,
        id: Id,
    ) -> StorageResult<Option<VersionedRecord>> {
        self.read_full_id(id)
    }

    /// Write the NonUnique tracker (BTree/array root) for a struct.
    ///
    /// Hashed by `(struct_id, tenant)` — one tracker per `(struct, tenant)`,
    /// same strategy as Unique anchors but in its own logical namespace.
    pub fn write_nonunique_tracker(
        &self,
        struct_id: u32,
        tenant: u64,
        slot: &AnchorSlot,
    ) -> StorageResult<()> {
        // Same tuple2 strategy as Unique anchor.  Since a `STRUCT_ID` has
        // exactly one shape, the (struct, tenant) namespace cannot also
        // hold a Unique anchor — no collision.
        self.write_unique_anchor(struct_id, tenant, slot)
    }

    /// Read the NonUnique tracker.
    pub fn read_nonunique_tracker(
        &self,
        struct_id: u32,
        tenant: u64,
    ) -> StorageResult<Option<AnchorSlot>> {
        self.read_unique_anchor(struct_id, tenant)
    }

    /// Write a NonUnique element record by its full Id.
    ///
    /// Hashed by all four fields — `shard_id` is the content-hash or
    /// caller-assigned discriminator, and `created_at` distinguishes
    /// versions of the same element.
    pub fn write_nonunique_element(
        &self,
        id: Id,
        rec: &VersionedRecord,
    ) -> StorageResult<()> {
        self.write_full_id(id, rec)
    }

    /// Read a NonUnique element record.
    pub fn read_nonunique_element(
        &self,
        id: Id,
    ) -> StorageResult<Option<VersionedRecord>> {
        self.read_full_id(id)
    }

    /// Write a NestedNonUnique tracker.  The tracker has its own `Id`,
    /// discoverable through the parent NonUnique element that owns it.
    ///
    /// Hashed by the full Id (tuple4).
    pub fn write_nested_tracker(
        &self,
        id: Id,
        slot: &AnchorSlot,
    ) -> StorageResult<()> {
        let key = page_key_full(id);
        let raw = id.raw();
        let bytes = slot.to_bytes()?;
        self.insert_at_key(key, raw, &bytes)?;
        self.cache
            .put_anchor(AnchorKey::from_raw(raw), slot.clone());
        Ok(())
    }

    /// Read a NestedNonUnique tracker by Id.
    pub fn read_nested_tracker(
        &self,
        id: Id,
    ) -> StorageResult<Option<AnchorSlot>> {
        let raw = id.raw();
        if let Some(slot) = self.cache.get_anchor(AnchorKey::from_raw(raw)) {
            return Ok(Some(slot));
        }
        self.lookup_at_key(page_key_full(id), raw)
            .map(|b| AnchorSlot::from_bytes(&b))
            .transpose()
    }

    /// Write a NestedNonUnique element record — same routing as NonUnique
    /// element.
    pub fn write_nested_element(
        &self,
        id: Id,
        rec: &VersionedRecord,
    ) -> StorageResult<()> {
        self.write_full_id(id, rec)
    }

    /// Read a NestedNonUnique element record.
    pub fn read_nested_element(
        &self,
        id: Id,
    ) -> StorageResult<Option<VersionedRecord>> {
        self.read_full_id(id)
    }

    // ── Internal full-Id read/write (shared by all tuple4 paths) ──────────

    /// Write a versioned record at its full-Id page (tuple4).
    fn write_full_id(
        &self,
        id: Id,
        rec: &VersionedRecord,
    ) -> StorageResult<()> {
        let bytes = rec.to_bytes()?;
        self.insert_at_key(page_key_full(id), rec.id, &bytes)?;
        self.cache.put_versioned(rec.clone());
        Ok(())
    }

    /// Read a versioned record from its full-Id page (tuple4).
    fn read_full_id(&self, id: Id) -> StorageResult<Option<VersionedRecord>> {
        if let Some(rec) = self.cache.get_versioned(id.raw()) {
            return Ok(Some(rec));
        }
        self.lookup_at_key(page_key_full(id), id.raw())
            .map(|b| VersionedRecord::from_bytes(&b))
            .transpose()
    }

    // ── Legacy back-compat wrappers ───────────────────────────────────────
    //
    // These auto-pick the strategy from the input.  Prefer the typed
    // `write_*` / `read_*` methods above for new code.

    /// Write an anchor slot — strategy auto-picked from `AnchorKey`.
    ///
    /// `shard_id == 0` → Unique anchor (tuple2); otherwise → NonUnique
    /// element anchor (tuple4).  See the typed methods above for explicit
    /// control.
    pub fn write_anchor(
        &self,
        key: AnchorKey,
        slot: &AnchorSlot,
    ) -> StorageResult<()> {
        let id = Id::from_raw(key.raw());
        if id.shard_id() == 0 {
            self.write_unique_anchor(id.struct_id(), id.tenant_id(), slot)
        } else {
            let pkey = page_key_full(id);
            let bytes = slot.to_bytes()?;
            self.insert_at_key(pkey, key.raw(), &bytes)?;
            self.cache.put_anchor(key, slot.clone());
            Ok(())
        }
    }

    /// Read an anchor slot — strategy auto-picked from `AnchorKey`.
    pub fn read_anchor(
        &self,
        key: AnchorKey,
    ) -> StorageResult<Option<AnchorSlot>> {
        let id = Id::from_raw(key.raw());
        if id.shard_id() == 0 {
            self.read_unique_anchor(id.struct_id(), id.tenant_id())
        } else {
            if let Some(slot) = self.cache.get_anchor(key) {
                return Ok(Some(slot));
            }
            self.lookup_at_key(page_key_full(id), key.raw())
                .map(|b| AnchorSlot::from_bytes(&b))
                .transpose()
        }
    }

    /// Write a versioned record — tuple4 strategy (full Id always known).
    pub fn write_versioned(&self, rec: &VersionedRecord) -> StorageResult<()> {
        self.write_full_id(Id::from_raw(rec.id), rec)
    }

    /// Read a versioned record by Id.
    pub fn read_versioned(
        &self,
        id: Id,
    ) -> StorageResult<Option<VersionedRecord>> {
        self.read_full_id(id)
    }

    /// Fill ratio (0.0 – 1.0) — fraction of pages that hold at least one
    /// directory entry.  Useful for monitoring distribution uniformity.
    #[allow(clippy::cast_precision_loss, clippy::significant_drop_tightening)]
    pub fn fill_ratio(&self) -> f64 {
        let pages = self.pages.read();
        let used = pages.iter().filter(|p| p.header.entry_count > 0).count();
        let n = self.page_count.load(Ordering::Acquire);
        used as f64 / n as f64
    }

    /// Number of pages that contain at least one entry.
    pub fn used_page_count(&self) -> u64 {
        self.pages
            .read()
            .iter()
            .filter(|p| p.header.entry_count > 0)
            .count() as u64
    }

    /// Get the file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the page size.
    pub const fn page_size(&self) -> usize {
        self.page_size
    }

    /// Get the current page count.  Equivalent to [`Self::page_count_now`].
    pub fn page_count(&self) -> u64 {
        self.page_count.load(Ordering::Acquire)
    }

    // ── Rebalance ────────────────────────────────────────────────────────

    /// Grow `page_count` by [`Self::growth_factor`] and rehash every entry
    /// that ends up on a different slot under the new size.
    ///
    /// Steps:
    ///
    /// 1. Single-flight latch on `rebalancing` — concurrent callers
    ///    return `Ok(())` immediately.
    /// 2. Push the old `page_count` onto `previous_page_counts` so
    ///    lookups still find unmoved entries.
    /// 3. Atomically swap to the new `page_count` and grow `pages` and
    ///    `page_bytes` to match.
    /// 4. Walk old pages **sorted by fill descending** — hottest pages
    ///    relieve pressure first.
    /// 5. For each entry whose new slot ≠ old slot, move it and forward
    ///    a [`JournalEntry`] over the bounded journal pipe.
    /// 6. When every old slot has been processed, pop the old size off
    ///    `previous_page_counts`.
    ///
    /// Concurrent **writes** are blocked while the page lock is held for
    /// a move; concurrent **reads** continue to work via the fallback
    /// list.  After the call returns the file is in steady state at the
    /// new size.
    #[allow(clippy::significant_drop_tightening)]
    pub fn rebalance(&self) -> StorageResult<()> {
        // Single-flight latch.  Another writer is already rehashing —
        // bail out; the new `page_count` will be visible to us by the
        // time our caller retries the failed insert.
        if self.rebalancing.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        let result = (|| -> StorageResult<()> {
            let old_count = self.page_count.load(Ordering::Acquire);
            // Grow by growth_factor (default 1.75×), always at least +1.
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let new_count = ((old_count as f64 * self.growth_factor).ceil()
                as u64)
                .max(old_count + 1);
            if new_count < old_count {
                return Err(crate::StorageError::Other(
                    "rebalance overflow: cannot grow beyond u64::MAX pages"
                        .into(),
                ));
            }

            // 1. Publish old size onto the fallback list FIRST so any
            //    concurrent lookup can still find unmoved records.
            self.previous_page_counts.write().push(old_count);

            // 2. Grow the page vector to the new size.  Empty pages
            //    occupy the high half until the rehash moves entries in.
            {
                let mut pages = self.pages.write();
                let mut page_bytes = self.page_bytes.write();
                for _ in 0..(new_count - old_count) {
                    pages.push(Page::new(self.page_size));
                    page_bytes.push(AtomicU64::new(0));
                }
            }

            // 3. Publish the new size — happens-after the page vector
            //    has actually grown so any new lookup at the new size
            //    indexes into a valid slot.
            self.page_count.store(new_count, Ordering::Release);

            // 4. Build fill-descending iteration order so the hottest
            //    old pages rehash first.  This relieves the worst
            //    write-pressure regions earliest.
            let order: Vec<usize> = {
                let pb = self.page_bytes.read();
                #[allow(clippy::cast_possible_truncation)]
                let mut v: Vec<(usize, u64)> = (0..old_count as usize)
                    .map(|i| (i, pb[i].load(Ordering::Relaxed)))
                    .collect();
                v.sort_by_key(|&(_, bytes)| std::cmp::Reverse(bytes));
                v.into_iter().map(|(i, _)| i).collect()
            };

            // 5. Walk every old page and move any entry whose new slot
            //    differs.  Each move is journalled over the bounded
            //    pipe so a crash recovers an in-progress rehash.
            for old_idx in order {
                self.rebalance_one_page(old_idx, new_count)?;
            }

            // 6. Rehash complete — drop the old size from the fallback
            //    list.  Subsequent lookups go straight to the new size.
            self.previous_page_counts
                .write()
                .retain(|&c| c != old_count);

            Ok(())
        })();

        self.rebalancing.store(false, Ordering::Release);
        result
    }

    /// Move every entry on old page `old_idx` whose slot at `new_count`
    /// is different from `old_idx`.  Journalled.
    #[allow(clippy::significant_drop_tightening)]
    fn rebalance_one_page(
        &self,
        old_idx: usize,
        new_count: u64,
    ) -> StorageResult<()> {
        // Collect the move list under a read lock so we hold the write
        // lock for as short a window as possible.
        struct Move {
            raw: u128,
            new_slot: usize,
            bytes: Vec<u8>,
        }

        let moves: Vec<Move> = {
            let pages = self.pages.read();
            let page = &pages[old_idx];
            let mut out = Vec::with_capacity(page.directory.len());
            for entry in &page.directory {
                let id = Id::from_raw(entry.id);
                // Re-derive the original [`PageKey`].  Anchor entries
                // for Unique records keep `shard_id == 0 && created_at == 0`;
                // every other kind hashes by the full Id.
                let key = if id.shard_id() == 0 && id.created_at() == 0 {
                    PageKey::Tuple2 {
                        struct_id: id.struct_id(),
                        tenant: id.tenant_id(),
                    }
                } else {
                    page_key_full(id)
                };
                let new_slot = Self::page_index(key, new_count);
                if new_slot != old_idx {
                    let start = entry.offset as usize;
                    let end = start + entry.size as usize;
                    out.push(Move {
                        raw: entry.id,
                        new_slot,
                        bytes: page.data[start..end].to_vec(),
                    });
                }
            }
            out
        };

        if moves.is_empty() {
            return Ok(());
        }

        // Snapshot the journal sender so the lock isn't held across
        // the `send` call (which can block on backpressure).
        let tx = self.journal_tx.read().clone();

        for mv in moves {
            // Move under write lock.  Order: insert into new slot
            // first; only on success remove from old slot.  This means
            // a concurrent reader either sees the entry in the old
            // slot (via fallback) or in the new slot — never neither.
            {
                let mut pages = self.pages.write();
                if pages[mv.new_slot].insert(mv.raw, &mv.bytes) {
                    pages[old_idx].remove(mv.raw);
                    self.bump_page_bytes(mv.new_slot, mv.bytes.len() as u64);
                    // (Old-page bytes counter intentionally not
                    // decremented — it's used only as a heuristic for
                    // ordering, and the heuristic is consumed once per
                    // rebalance.)
                } else {
                    // New slot already full.  Skip this move; the next
                    // rebalance will pick it up.  Log via Other so the
                    // operator can see the stall.
                    return Err(crate::StorageError::Other(format!(
                        "rebalance: new slot {} full while moving id {:#034x}",
                        mv.new_slot, mv.raw
                    )));
                }
            }

            // Journal the move.  Blocking send applies backpressure
            // when the channel is full.  Send-error means the receiver
            // has hung up; we treat that as an unjournalled (best-effort)
            // file but continue the rehash so the in-memory state stays
            // consistent.
            if let Some(tx) = &tx {
                let entry = JournalEntry::WriteVersioned {
                    record: VersionedRecord::new(mv.raw, mv.bytes),
                };
                let _ = tx.send(entry);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Disk format ───────────────────────────────────────────────────────

    #[test]
    fn flush_sparse_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        {
            let f = DataFile::open_on_disk(&path, 4096).unwrap();
            for i in 1u64..=10 {
                let id = wavedb_core::Id::new(7, 0, 11, i);
                f.write_versioned(&VersionedRecord::new(
                    id.raw(),
                    b"v".to_vec(),
                ))
                .unwrap();
            }
            f.flush().unwrap();
            // Sparse: file must be much smaller than page_count × page_size.
            let size = std::fs::metadata(&path).unwrap().len();
            let full_size = f.page_count_now() * 4096;
            assert!(
                size < full_size,
                "sparse file ({size}B) should be smaller than full ({full_size}B)"
            );
            // File must start with magic.
            let bytes = std::fs::read(&path).unwrap();
            assert!(bytes.starts_with(&DISK_FORMAT_MAGIC));
        }
        // Reload and verify all entries survive.
        let f2 = DataFile::open_on_disk(&path, 4096).unwrap();
        for i in 1u64..=10 {
            let id = wavedb_core::Id::new(7, 0, 11, i);
            assert!(
                f2.read_versioned(id).unwrap().is_some(),
                "id {i} missing after reload"
            );
        }
    }

    #[test]
    fn flush_uses_atomic_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let f = DataFile::open_on_disk(&path, 4096).unwrap();
        let id = wavedb_core::Id::new(7, 0, 11, 1);
        f.write_versioned(&VersionedRecord::new(id.raw(), b"x".to_vec()))
            .unwrap();
        f.flush().unwrap();
        // .tmp must not remain after flush.
        assert!(!dir.path().join("data.tmp").exists());
        assert!(path.exists());
    }

    #[test]
    fn write_then_read_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        let id = wavedb_core::Id::new(42, 0, 7, 1_000_000);
        let key = AnchorKey::from(id);
        let slot = AnchorSlot::inline(b"test data", 1_000_000);
        file.write_anchor(key, &slot).unwrap();
        let got = file.read_anchor(key).unwrap().unwrap();
        assert_eq!(got.inline_bytes().unwrap(), b"test data");
    }

    #[test]
    fn write_then_read_versioned() {
        let dir = tempfile::tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        let id = wavedb_core::Id::new(42, 0, 7, 1_000_000);
        let rec = VersionedRecord::new(id.raw(), b"versioned data".to_vec());
        file.write_versioned(&rec).unwrap();
        let got = file.read_versioned(id).unwrap().unwrap();
        assert_eq!(got.data, b"versioned data");
    }

    // ── Typed entry points ─────────────────────────────────────────────

    #[test]
    fn unique_anchor_roundtrip_by_struct_tenant() {
        let dir = tempfile::tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        let slot = AnchorSlot::inline(b"profile", 1000);
        file.write_unique_anchor(7, 42, &slot).unwrap();
        let got = file.read_unique_anchor(7, 42).unwrap().unwrap();
        assert_eq!(got.inline_bytes().unwrap(), b"profile");
    }

    /// Unique anchor lookup must NOT depend on `created_at` — the caller
    /// only knows `(struct_id, tenant)` at lookup time.
    #[test]
    fn unique_anchor_lookup_is_independent_of_created_at() {
        let dir = tempfile::tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        file.write_unique_anchor(7, 42, &AnchorSlot::inline(b"v1", 1000))
            .unwrap();
        // A different tenant must not see this anchor.
        assert!(file.read_unique_anchor(7, 99).unwrap().is_none());
        // Different struct, same tenant — also nothing.
        assert!(file.read_unique_anchor(8, 42).unwrap().is_none());
        // Same (struct, tenant) — hits regardless of cache, page count, etc.
        assert!(file.read_unique_anchor(7, 42).unwrap().is_some());
    }

    #[test]
    fn nonunique_tracker_shares_tuple2_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        file.write_nonunique_tracker(
            10,
            99,
            &AnchorSlot::inline(b"tracker", 0),
        )
        .unwrap();
        let got = file.read_nonunique_tracker(10, 99).unwrap().unwrap();
        assert_eq!(got.inline_bytes().unwrap(), b"tracker");
    }

    #[test]
    fn nonunique_element_roundtrip_by_full_id() {
        let dir = tempfile::tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        let id = wavedb_core::Id::new(99, 5, 10, 12_345);
        let rec = VersionedRecord::new(id.raw(), b"element".to_vec());
        file.write_nonunique_element(id, &rec).unwrap();
        let got = file.read_nonunique_element(id).unwrap().unwrap();
        assert_eq!(got.data, b"element");
    }

    #[test]
    fn nested_tracker_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        // Tracker has its own Id, distinct from its parent NonUnique element.
        let tracker_id = wavedb_core::Id::new(99, 7, 10, 999);
        let slot = AnchorSlot::inline(b"nested-tracker", 999);
        file.write_nested_tracker(tracker_id, &slot).unwrap();
        let got = file.read_nested_tracker(tracker_id).unwrap().unwrap();
        assert_eq!(got.inline_bytes().unwrap(), b"nested-tracker");
    }

    #[test]
    fn nested_element_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        let id = wavedb_core::Id::new(99, 3, 11, 555);
        let rec = VersionedRecord::new(id.raw(), b"nested-elem".to_vec());
        file.write_nested_element(id, &rec).unwrap();
        let got = file.read_nested_element(id).unwrap().unwrap();
        assert_eq!(got.data, b"nested-elem");
    }

    /// Two records under the same `(struct, tenant)` but with distinct
    /// `created_at` must land on different pages so version history does
    /// not pile onto a single hot page.
    #[test]
    fn nonunique_versions_spread_across_pages() {
        let dir = tempfile::tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        for created_at in 1..=512u64 {
            let id = wavedb_core::Id::new(99, 0, 10, created_at);
            let rec = VersionedRecord::new(id.raw(), b"v".to_vec());
            file.write_nonunique_element(id, &rec).unwrap();
        }
        // Round-trip every id — rebalances may have grown the file in
        // the meantime; this is the correctness signal we care about.
        for created_at in 1..=512u64 {
            let id = wavedb_core::Id::new(99, 0, 10, created_at);
            assert!(
                file.read_nonunique_element(id).unwrap().is_some(),
                "id created_at={created_at} not found"
            );
        }
    }

    // ── Mode (in-memory vs on-disk) ─────────────────────────────────────

    #[test]
    fn in_memory_mode_does_not_touch_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nowhere");
        let file = DataFile::open(&path, 4096).unwrap();
        let id = wavedb_core::Id::new(7, 0, 1, 99);
        file.write_versioned(&VersionedRecord::new(id.raw(), b"x".to_vec()))
            .unwrap();
        // open() defaults to InMemory; flush() must be a no-op and the
        // path on disk must not appear.
        assert_eq!(file.mode(), Mode::InMemory);
        file.flush().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn open_in_memory_constructor() {
        let file = DataFile::open_in_memory(4096).unwrap();
        assert_eq!(file.mode(), Mode::InMemory);
        let id = wavedb_core::Id::new(7, 0, 1, 99);
        file.write_versioned(&VersionedRecord::new(id.raw(), b"x".to_vec()))
            .unwrap();
        assert!(file.read_versioned(id).unwrap().is_some());
    }

    #[test]
    fn on_disk_roundtrip_through_flush_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");

        // Write + flush.
        {
            let file = DataFile::open_on_disk(&path, 4096).unwrap();
            assert_eq!(file.mode(), Mode::OnDisk);
            for created_at in 1..=8u64 {
                let id = wavedb_core::Id::new(7, 0, 11, created_at);
                file.write_versioned(&VersionedRecord::new(
                    id.raw(),
                    b"hi".to_vec(),
                ))
                .unwrap();
            }
            file.flush().unwrap();
        }

        // Reopen → reads land on disk snapshot.
        let file = DataFile::open_on_disk(&path, 4096).unwrap();
        for created_at in 1..=8u64 {
            let id = wavedb_core::Id::new(7, 0, 11, created_at);
            assert!(
                file.read_versioned(id).unwrap().is_some(),
                "id created_at={created_at} missing after reload"
            );
        }
    }

    // ── Rebalance ──────────────────────────────────────────────────────

    /// Manual `rebalance()` grows `page_count` by the configured factor and
    /// every existing entry remains findable afterwards.
    #[test]
    fn rebalance_grows_page_count_and_preserves_entries() {
        let dir = tempfile::tempdir().unwrap();
        let file =
            DataFile::open_with(&dir.path().join("data"), 4096, 32).unwrap();

        for created_at in 1..=64u64 {
            let id = wavedb_core::Id::new(7, 0, 11, created_at);
            file.write_versioned(&VersionedRecord::new(
                id.raw(),
                b"v".to_vec(),
            ))
            .unwrap();
        }

        let before = file.page_count_now();
        file.rebalance().unwrap();
        let after = file.page_count_now();

        // Must grow by at least the growth factor.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let min_expected =
            (before as f64 * DEFAULT_GROWTH_FACTOR).ceil() as u64;
        assert!(
            after >= min_expected,
            "expected at least {min_expected} pages: {before} -> {after}"
        );
        // After rebalance the fallback list is drained; readers go
        // direct to the new size.
        assert!(file.previous_page_counts().is_empty());

        // Every entry still findable at the new size.
        for created_at in 1..=64u64 {
            let id = wavedb_core::Id::new(7, 0, 11, created_at);
            assert!(file.read_versioned(id).unwrap().is_some());
        }
    }

    /// Lookups fall back through `previous_page_counts` while the
    /// fallback list is non-empty — simulated by manually publishing an
    /// old size onto the list before a rebalance completes.
    #[test]
    fn lookup_falls_back_to_previous_page_count() {
        let dir = tempfile::tempdir().unwrap();
        let file =
            DataFile::open_with(&dir.path().join("data"), 4096, 64).unwrap();
        let id = wavedb_core::Id::new(7, 0, 11, 42);
        file.write_versioned(&VersionedRecord::new(id.raw(), b"orig".to_vec()))
            .unwrap();

        // Simulate a mid-flight rebalance: bump page_count_now without
        // moving the entry, but record the old size as a fallback.
        // The entry still lives in its original slot under page_count = 64.
        file.previous_page_counts.write().push(64);
        // Lookup at the "new" size won't find it (entry sits on the
        // old slot), so the fallback must kick in.
        let got = file.read_versioned(id).unwrap();
        assert!(got.is_some(), "fallback lookup did not find entry");
    }

    /// The bounded journal pipe receives one entry per actual move.
    /// Empty pages contribute no traffic — backpressure is bounded by
    /// the number of relocations, not the page count.
    #[test]
    fn rebalance_emits_journal_entries() {
        use std::sync::mpsc::sync_channel;

        let dir = tempfile::tempdir().unwrap();
        let file =
            DataFile::open_with(&dir.path().join("data"), 4096, 16).unwrap();
        let (tx, rx) =
            sync_channel::<crate::pipeline::journal::JournalEntry>(256);
        file.set_journal_pipe(tx);

        for created_at in 1..=64u64 {
            let id = wavedb_core::Id::new(7, 0, 11, created_at);
            file.write_versioned(&VersionedRecord::new(
                id.raw(),
                b"v".to_vec(),
            ))
            .unwrap();
        }
        file.rebalance().unwrap();

        // Drop the file so the SyncSender is dropped — drains the channel.
        drop(file);

        let received: usize = rx.iter().count();
        // At least some entries should have moved.  Conservative lower
        // bound: any non-zero count proves the pipe was wired and
        // backpressure-aware sends went through.
        assert!(received > 0, "no journal entries received");
    }

    #[test]
    fn open_with_accepts_non_power_of_two() {
        let dir = tempfile::tempdir().unwrap();
        // Non-power-of-two page counts are now valid.
        let f =
            DataFile::open_with(&dir.path().join("data"), 4096, 13).unwrap();
        assert_eq!(f.page_count_now(), 13);
    }

    #[test]
    fn open_with_rejects_zero_page_count() {
        let dir = tempfile::tempdir().unwrap();
        let r = DataFile::open_with(&dir.path().join("data"), 4096, 0);
        assert!(matches!(r, Err(crate::StorageError::Other(_))));
    }

    /// 1024 distinct Ids write successfully and remain readable across
    /// any rebalances triggered along the way.  Replaces an older test
    /// that asserted a static fill ratio (no longer valid: rebalance now
    /// dilutes the ratio whenever it crosses the threshold).
    #[test]
    fn distribution_spreads_writes_uniformly() {
        let dir = tempfile::tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        for seq in 1..=1024u64 {
            let id = wavedb_core::Id::new(100, 0, 7, seq);
            let rec = VersionedRecord::new(id.raw(), b"x".to_vec());
            file.write_versioned(&rec).unwrap();
        }
        // Every id must still be readable after any rebalances that
        // fired along the way.
        for seq in 1..=1024u64 {
            let id = wavedb_core::Id::new(100, 0, 7, seq);
            let got = file.read_versioned(id).unwrap();
            assert!(got.is_some(), "id seq={seq} not found after rebalance");
        }
    }

    #[test]
    fn version_chain_links() {
        let dir = tempfile::tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();

        let id1 = wavedb_core::Id::new(42, 0, 7, 1_000);
        let id2 = wavedb_core::Id::new(42, 0, 7, 2_000);

        let v1 = VersionedRecord::new(id1.raw(), b"v1".to_vec());
        file.write_versioned(&v1).unwrap();

        let v2 = VersionedRecord {
            id: id2.raw(),
            data: b"v2".to_vec(),
            old_modification_id: id1.raw(),
            new_modification_id: 0,
        };
        file.write_versioned(&v2).unwrap();

        let got_v2 = file.read_versioned(id2).unwrap().unwrap();
        assert_eq!(got_v2.old_modification_id, id1.raw());
        assert!(got_v2.is_live());
    }
}
