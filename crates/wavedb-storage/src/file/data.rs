//! The data file: hash-mapped pages holding anchor slots and versioned records.

use crate::StorageResult;
use crate::anchor::{AnchorKey, AnchorSlot};
use crate::cache::Cache;
use crate::hash;
use crate::page::Page;
use crate::versioned::VersionedRecord;
use parking_lot::RwLock;
use std::path::{Path, PathBuf};

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
    pub fn open(path: &Path, page_size: usize) -> StorageResult<Self> {
        let page_count = 256u64; // default initial page count
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

    /// Write an anchor slot at the given key.
    pub fn write_anchor(&self, key: AnchorKey, slot: &AnchorSlot) -> StorageResult<()> {
        let id = wavedb_core::Id::from_raw(key.raw());
        let page_idx = usize::try_from(hash::page_hash(
            id.struct_id(),
            id.tenant_id(),
            id.shard_id(),
            self.page_count,
        ))
        .expect("page index overflow");

        let bytes = slot.to_bytes()?;
        let mut pages = self.pages.write();

        if !pages[page_idx].insert(key.raw(), &bytes) {
            // Try double-hashing
            let step = usize::try_from(hash::double_hash_step(
                id.struct_id(),
                id.tenant_id(),
                id.shard_id(),
                self.page_count,
            ))
            .expect("step overflow");
            let alt_idx =
                (page_idx + step) % usize::try_from(self.page_count).expect("page count overflow");
            if !pages[alt_idx].insert(key.raw(), &bytes) {
                return Err(crate::StorageError::PageFull(alt_idx as u64));
            }
        }

        // Also cache it
        self.cache.put_anchor(key, slot.clone());
        Ok(())
    }

    /// Read an anchor slot at the given key.
    pub fn read_anchor(&self, key: AnchorKey) -> StorageResult<Option<AnchorSlot>> {
        // Check cache first
        if let Some(slot) = self.cache.get_anchor(key) {
            return Ok(Some(slot));
        }

        let id = wavedb_core::Id::from_raw(key.raw());
        let page_idx = usize::try_from(hash::page_hash(
            id.struct_id(),
            id.tenant_id(),
            id.shard_id(),
            self.page_count,
        ))
        .expect("page index overflow");

        {
            let pages = self.pages.read();

            // Check primary page
            if let Some(bytes) = pages[page_idx].lookup(key.raw()) {
                let slot = AnchorSlot::from_bytes(bytes)?;
                return Ok(Some(slot));
            }

            // Check double-hash page
            let step = usize::try_from(hash::double_hash_step(
                id.struct_id(),
                id.tenant_id(),
                id.shard_id(),
                self.page_count,
            ))
            .expect("step overflow");
            let alt_idx =
                (page_idx + step) % usize::try_from(self.page_count).expect("page count overflow");
            if let Some(bytes) = pages[alt_idx].lookup(key.raw()) {
                let slot = AnchorSlot::from_bytes(bytes)?;
                return Ok(Some(slot));
            }
        }

        Ok(None)
    }

    /// Write a versioned record.
    pub fn write_versioned(&self, rec: &VersionedRecord) -> StorageResult<()> {
        let id = wavedb_core::Id::from_raw(rec.id);
        let page_idx = usize::try_from(hash::versioned_hash(
            id.struct_id(),
            id.tenant_id(),
            id.shard_id(),
            id.created_at(),
            self.page_count,
        ))
        .expect("page index overflow");

        let bytes = rec.to_bytes()?;
        {
            let mut pages = self.pages.write();
            if !pages[page_idx].insert(rec.id, &bytes) {
                return Err(crate::StorageError::PageFull(
                    u64::try_from(page_idx).unwrap(),
                ));
            }
        }

        self.cache.put_versioned(rec.clone());
        Ok(())
    }

    /// Read a versioned record by ID.
    pub fn read_versioned(&self, id: wavedb_core::Id) -> StorageResult<Option<VersionedRecord>> {
        // Check cache first
        if let Some(rec) = self.cache.get_versioned(id.raw()) {
            return Ok(Some(rec));
        }

        let page_idx = usize::try_from(hash::versioned_hash(
            id.struct_id(),
            id.tenant_id(),
            id.shard_id(),
            id.created_at(),
            self.page_count,
        ))
        .expect("page index overflow");

        {
            let pages = self.pages.read();
            if let Some(bytes) = pages[page_idx].lookup(id.raw()) {
                let rec = VersionedRecord::from_bytes(bytes)?;
                return Ok(Some(rec));
            }
        }

        Ok(None)
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
