//! Sorted index file — per-`(STRUCT_ID, TENANT_ID)` creation-order index.
//!
//! In the research phase this is a `BTreeMap` in memory, persisted as a flat
//! sorted binary file. Production would use proper B+ tree nodes laid out
//! contiguously in the index file (one tree per `(STRUCT_ID, TENANT_ID)` pair,
//! with the adaptive array→tree conversion described in the README).
//!
//! # File layout
//!
//! ```text
//! [u32 magic = 0x49444258][u32 entry_count]
//! [entry_count × 34-byte entries, sorted by key]
//! ```
//!
//! Each 34-byte entry: `[u16 struct_id][u64 tenant_id][u64 created_at][u128 anchor_id]`

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

const MAGIC: u32 = 0x4944_4258; // "IDBX"
const ENTRY_SIZE: usize = 34; // u16 + u64 + u64 + u128

// ─── IndexKey ─────────────────────────────────────────────────────────────────

/// Composite sort key for one index entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct IndexKey {
    pub struct_id: u16,
    pub tenant_id: u64,
    /// `created_at` ticks of the current live version — used for ordering
    /// within a tenant's collection.
    pub created_at: u64,
}

impl IndexKey {
    pub fn new(struct_id: u16, tenant_id: u64, created_at: u64) -> Self {
        Self { struct_id, tenant_id, created_at }
    }

    pub fn tenant_range_start(struct_id: u16, tenant_id: u64) -> Self {
        Self { struct_id, tenant_id, created_at: 0 }
    }

    pub fn tenant_range_end(struct_id: u16, tenant_id: u64) -> Self {
        Self { struct_id, tenant_id, created_at: u64::MAX }
    }
}

// ─── IndexFile ────────────────────────────────────────────────────────────────

/// In-memory sorted index backed by a binary file.
pub struct IndexFile {
    /// Maps `(struct_id, tenant_id, created_at)` → `anchor_id` (u128).
    map: BTreeMap<IndexKey, u128>,
    /// `true` if the in-memory map has unsaved changes.
    dirty: bool,
}

impl IndexFile {
    /// Load an existing index file or return an empty index.
    pub fn open_or_create(path: &Path) -> io::Result<Self> {
        if path.exists() {
            let map = load_from_file(path)?;
            Ok(Self { map, dirty: false })
        } else {
            Ok(Self { map: BTreeMap::new(), dirty: false })
        }
    }

    // ── Write ─────────────────────────────────────────────────────────────────

    /// Insert or replace an index entry.
    pub fn insert(&mut self, struct_id: u16, tenant_id: u64, created_at: u64, anchor_id: u128) {
        self.map.insert(IndexKey::new(struct_id, tenant_id, created_at), anchor_id);
        self.dirty = true;
    }

    /// Remove an index entry. Returns `true` if it existed.
    pub fn remove(&mut self, struct_id: u16, tenant_id: u64, created_at: u64) -> bool {
        let removed = self.map.remove(&IndexKey::new(struct_id, tenant_id, created_at)).is_some();
        if removed {
            self.dirty = true;
        }
        removed
    }

    // ── Read ──────────────────────────────────────────────────────────────────

    /// Return all anchor IDs for `(struct_id, tenant_id)`, sorted by `created_at`.
    pub fn range_for_tenant(&self, struct_id: u16, tenant_id: u64) -> Vec<u128> {
        let start = IndexKey::tenant_range_start(struct_id, tenant_id);
        let end = IndexKey::tenant_range_end(struct_id, tenant_id);
        self.map.range(start..=end).map(|(_, &v)| v).collect()
    }

    /// Look up a specific entry.
    pub fn get(&self, struct_id: u16, tenant_id: u64, created_at: u64) -> Option<u128> {
        self.map.get(&IndexKey::new(struct_id, tenant_id, created_at)).copied()
    }

    /// Total number of index entries.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    // ── Persistence ───────────────────────────────────────────────────────────

    /// Flush the in-memory index to `path` (full file rewrite).
    ///
    /// A B+ tree implementation would do incremental node writes instead.
    pub fn flush(&mut self, path: &Path) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        write_to_file(path, &self.map)?;
        self.dirty = false;
        Ok(())
    }
}

// ─── File I/O ─────────────────────────────────────────────────────────────────

fn load_from_file(path: &Path) -> io::Result<BTreeMap<IndexKey, u128>> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 8];
    file.read_exact(&mut header)?;

    let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad index magic: {magic:#010x}"),
        ));
    }
    let count = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;

    let mut map = BTreeMap::new();
    let mut entry_buf = [0u8; ENTRY_SIZE];
    for _ in 0..count {
        file.read_exact(&mut entry_buf)?;
        let struct_id = u16::from_le_bytes(entry_buf[0..2].try_into().unwrap());
        let tenant_id = u64::from_le_bytes(entry_buf[2..10].try_into().unwrap());
        let created_at = u64::from_le_bytes(entry_buf[10..18].try_into().unwrap());
        let anchor_id = u128::from_le_bytes(entry_buf[18..34].try_into().unwrap());
        map.insert(IndexKey::new(struct_id, tenant_id, created_at), anchor_id);
    }
    Ok(map)
}

fn write_to_file(path: &Path, map: &BTreeMap<IndexKey, u128>) -> io::Result<()> {
    let mut file =
        std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(path)?;

    let count = map.len() as u32;
    let mut header = [0u8; 8];
    header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    header[4..8].copy_from_slice(&count.to_le_bytes());
    file.write_all(&header)?;

    for (key, &anchor_id) in map.iter() {
        let mut entry = [0u8; ENTRY_SIZE];
        entry[0..2].copy_from_slice(&key.struct_id.to_le_bytes());
        entry[2..10].copy_from_slice(&key.tenant_id.to_le_bytes());
        entry[10..18].copy_from_slice(&key.created_at.to_le_bytes());
        entry[18..34].copy_from_slice(&anchor_id.to_le_bytes());
        file.write_all(&entry)?;
    }
    file.sync_data()
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
    fn insert_and_range() {
        let mut idx = IndexFile { map: BTreeMap::new(), dirty: false };
        idx.insert(2, 1, 100, 0xAAAA);
        idx.insert(2, 1, 200, 0xBBBB);
        idx.insert(2, 1, 300, 0xCCCC);
        idx.insert(2, 99, 100, 0xDDDD); // different tenant

        let range = idx.range_for_tenant(2, 1);
        assert_eq!(range, vec![0xAAAA, 0xBBBB, 0xCCCC]);
        // Different tenant not included
        assert!(!range.contains(&0xDDDD));
    }

    #[test]
    fn remove_entry() {
        let mut idx = IndexFile { map: BTreeMap::new(), dirty: false };
        idx.insert(2, 1, 100, 0xAAAA);
        idx.insert(2, 1, 200, 0xBBBB);
        assert!(idx.remove(2, 1, 100));
        assert_eq!(idx.range_for_tenant(2, 1), vec![0xBBBB]);
    }

    #[test]
    fn flush_and_reload() {
        let p = tmp("wave_idx_test.bin");
        let _ = std::fs::remove_file(&p);
        {
            let mut idx = IndexFile::open_or_create(&p).unwrap();
            idx.insert(2, 1, 100, 0xAAAA);
            idx.insert(2, 1, 200, 0xBBBB);
            idx.insert(3, 42, 500, 0xCCCC);
            idx.flush(&p).unwrap();
        }
        {
            let idx = IndexFile::open_or_create(&p).unwrap();
            assert_eq!(idx.len(), 3);
            assert_eq!(idx.range_for_tenant(2, 1), vec![0xAAAA, 0xBBBB]);
            assert_eq!(idx.get(3, 42, 500), Some(0xCCCC));
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn empty_file_creates_new() {
        let p = tmp("wave_idx_empty.bin");
        let _ = std::fs::remove_file(&p);
        let idx = IndexFile::open_or_create(&p).unwrap();
        assert!(idx.is_empty());
        // No file created yet (dirty=false, no flush)
        assert!(!p.exists());
    }
}
