//! The data file: hash-mapped pages holding anchor slots and versioned records.
//!
//! # Routing contract
//!
//! Every record's disk page is determined by [`crate::hash::id_to_page`] applied
//! to its raw [`wavedb_core::Id`].  Lookup is **O(1)**: one SplitMix64 mix
//! plus a mask (when `page_count` is a power of two).
//!
//! Collisions within a page are resolved by **double-hashing** to a second
//! page; if that one is also full, the write fails with [`StorageError::PageFull`].
//! Double-hashing is preferable to linear probing because the step is coprime
//! with any power-of-two `page_count`, so the probe sequence is a permutation
//! of every page index — no clustering.
//!
//! # Page count invariant
//!
//! `page_count` is **always a power of two** so the modulo in `id_to_page`
//! lowers to a single mask instruction.  The default is 256.

use crate::StorageResult;
use crate::anchor::{AnchorKey, AnchorSlot};
use crate::cache::Cache;
use crate::hash::{self, PageKey};
use crate::page::Page;
use crate::versioned::VersionedRecord;
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use wavedb_core::Id;

/// Default initial page count.  Must remain a power of two so [`PageKey::page`]
/// reduces via mask instead of divide.
pub const DEFAULT_PAGE_COUNT: u64 = 256;

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
pub struct DataFile {
    path: PathBuf,
    page_size: usize,
    page_count: u64,
    pages: RwLock<Vec<Page>>,
    cache: Cache,
}

impl DataFile {
    /// Open or create a data file at the given path.
    ///
    /// Uses [`DEFAULT_PAGE_COUNT`] (256).  For a custom page count call
    /// [`DataFile::open_with`].
    pub fn open(path: &Path, page_size: usize) -> StorageResult<Self> {
        Self::open_with(path, page_size, DEFAULT_PAGE_COUNT)
    }

    /// Open or create a data file with an explicit page count.
    ///
    /// `page_count` **must be a power of two** — otherwise the routing
    /// modulo becomes a division and we lose the O(1) cache-friendly mask.
    /// Returns [`StorageError::Other`] when this invariant is violated.
    pub fn open_with(path: &Path, page_size: usize, page_count: u64) -> StorageResult<Self> {
        if !page_count.is_power_of_two() {
            return Err(crate::StorageError::Other(format!(
                "page_count must be a power of two; got {page_count}"
            )));
        }
        let pages: Vec<Page> = (0..page_count).map(|_| Page::new(page_size)).collect();

        // Create directory if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        Ok(Self {
            path: path.to_path_buf(),
            page_size,
            page_count,
            pages: RwLock::new(pages),
            cache: Cache::new(),
        })
    }

    // ── Routing primitives ────────────────────────────────────────────────
    //
    // Every read and write funnels through `insert_at_key` / `lookup_at_key`,
    // which pick the page index from a [`PageKey`] (tuple2 or tuple4).  This
    // is the single source of truth — the typed entry points below just
    // build the right `PageKey` for the record kind.

    /// Primary page index for a [`PageKey`].
    #[inline]
    #[allow(clippy::cast_possible_truncation, clippy::missing_const_for_fn)]
    fn primary_for(&self, key: PageKey) -> usize {
        // page_count is bounded to a power of two at open() time and the
        // hash output is < page_count, so the cast cannot truncate.
        key.page(self.page_count) as usize
    }

    /// Double-hash fallback page when the primary is full.
    #[inline]
    #[allow(clippy::cast_possible_truncation, clippy::missing_const_for_fn)]
    fn fallback_for(&self, primary: usize, key: PageKey) -> usize {
        let (struct_id, tenant, shard) = match key {
            PageKey::Tuple2 { struct_id, tenant } => (struct_id, tenant, 0u16),
            PageKey::Tuple4 {
                struct_id,
                tenant,
                shard,
                ..
            } => (struct_id, tenant, shard),
        };
        let step = hash::double_hash_step(struct_id, tenant, shard, self.page_count) as usize;
        let mask = (self.page_count as usize).saturating_sub(1);
        (primary + step) & mask
    }

    /// Insert `bytes` keyed by `raw` at the page determined by `key`.
    #[allow(clippy::significant_drop_tightening)]
    fn insert_at_key(&self, key: PageKey, raw: u128, bytes: &[u8]) -> StorageResult<()> {
        let primary = self.primary_for(key);
        let mut pages = self.pages.write();
        if pages[primary].insert(raw, bytes) {
            return Ok(());
        }
        let alt = self.fallback_for(primary, key);
        if pages[alt].insert(raw, bytes) {
            return Ok(());
        }
        Err(crate::StorageError::PageFull(alt as u64))
    }

    /// Look up the bytes for `raw` at the page determined by `key`.
    #[allow(clippy::significant_drop_tightening)]
    fn lookup_at_key(&self, key: PageKey, raw: u128) -> Option<Vec<u8>> {
        let primary = self.primary_for(key);
        let pages = self.pages.read();
        if let Some(b) = pages[primary].lookup(raw) {
            return Some(b.to_vec());
        }
        let alt = self.fallback_for(primary, key);
        pages[alt].lookup(raw).map(<[u8]>::to_vec)
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
        self.cache.put_anchor(AnchorKey::from_raw(raw), slot.clone());
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
    pub fn write_unique_history(&self, id: Id, rec: &VersionedRecord) -> StorageResult<()> {
        self.write_full_id(id, rec)
    }

    /// Read a Unique record's history version by Id.
    pub fn read_unique_history(&self, id: Id) -> StorageResult<Option<VersionedRecord>> {
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
    pub fn write_nonunique_element(&self, id: Id, rec: &VersionedRecord) -> StorageResult<()> {
        self.write_full_id(id, rec)
    }

    /// Read a NonUnique element record.
    pub fn read_nonunique_element(&self, id: Id) -> StorageResult<Option<VersionedRecord>> {
        self.read_full_id(id)
    }

    /// Write a NestedNonUnique tracker.  The tracker has its own `Id`,
    /// discoverable through the parent NonUnique element that owns it.
    ///
    /// Hashed by the full Id (tuple4).
    pub fn write_nested_tracker(&self, id: Id, slot: &AnchorSlot) -> StorageResult<()> {
        let key = page_key_full(id);
        let raw = id.raw();
        let bytes = slot.to_bytes()?;
        self.insert_at_key(key, raw, &bytes)?;
        self.cache.put_anchor(AnchorKey::from_raw(raw), slot.clone());
        Ok(())
    }

    /// Read a NestedNonUnique tracker by Id.
    pub fn read_nested_tracker(&self, id: Id) -> StorageResult<Option<AnchorSlot>> {
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
    pub fn write_nested_element(&self, id: Id, rec: &VersionedRecord) -> StorageResult<()> {
        self.write_full_id(id, rec)
    }

    /// Read a NestedNonUnique element record.
    pub fn read_nested_element(&self, id: Id) -> StorageResult<Option<VersionedRecord>> {
        self.read_full_id(id)
    }

    // ── Internal full-Id read/write (shared by all tuple4 paths) ──────────

    /// Write a versioned record at its full-Id page (tuple4).
    fn write_full_id(&self, id: Id, rec: &VersionedRecord) -> StorageResult<()> {
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
    pub fn write_anchor(&self, key: AnchorKey, slot: &AnchorSlot) -> StorageResult<()> {
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
    pub fn read_anchor(&self, key: AnchorKey) -> StorageResult<Option<AnchorSlot>> {
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
    pub fn read_versioned(&self, id: Id) -> StorageResult<Option<VersionedRecord>> {
        self.read_full_id(id)
    }

    /// Fill ratio (0.0 – 1.0) — fraction of pages that hold at least one
    /// directory entry.  Useful for monitoring distribution uniformity.
    #[allow(clippy::cast_precision_loss, clippy::significant_drop_tightening)]
    pub fn fill_ratio(&self) -> f64 {
        let pages = self.pages.read();
        let used = pages.iter().filter(|p| p.header.entry_count > 0).count();
        used as f64 / self.page_count as f64
    }

    /// Get the file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the page size.
    pub const fn page_size(&self) -> usize {
        self.page_size
    }

    /// Get the page count.
    pub const fn page_count(&self) -> u64 {
        self.page_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        file.write_nonunique_tracker(10, 99, &AnchorSlot::inline(b"tracker", 0))
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
        // Mean = 2 entries/page (512 ÷ 256).  Expected coverage at this
        // density ≈ 1 − e^-2 ≈ 86.5 %; threshold 85 % leaves variance
        // headroom.
        assert!(
            file.fill_ratio() >= 0.85,
            "fill ratio {} too low",
            file.fill_ratio()
        );
    }

    #[test]
    fn open_with_rejects_non_power_of_two() {
        let dir = tempfile::tempdir().unwrap();
        let r = DataFile::open_with(&dir.path().join("data"), 4096, 13);
        assert!(matches!(r, Err(crate::StorageError::Other(_))));
    }

    /// 1024 distinct Ids hash to ≥ 95% of pages — the real-example
    /// 13-page-out-of-512 distribution bug doesn't recur.
    #[test]
    fn distribution_spreads_writes_uniformly() {
        let dir = tempfile::tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap(); // 256 pages
        for seq in 1..=1024u64 {
            let id = wavedb_core::Id::new(100, 0, 7, seq);
            let rec = VersionedRecord::new(id.raw(), b"x".to_vec());
            file.write_versioned(&rec).unwrap();
        }
        // With 1024 writes into 256 pages (mean = 4/page) the fill ratio
        // should be essentially 100 % under a strong mixer.
        assert!(
            file.fill_ratio() >= 0.95,
            "fill ratio too low: {}",
            file.fill_ratio()
        );
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
