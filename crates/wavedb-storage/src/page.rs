//! Page layout for WaveDB.
//!
//! Each page is internally organised as:
//! ```text
//! ┌──────────────────────────────────────────────────┐
//! │  PageHeader (checksum, entry_count, free_offset) │
//! │  ──────────────────────────────────────────────── │
//! │  Vec<(ID, offset, size)>   ← object directory    │
//! │  ──────────────────────────────────────────────── │
//! │  [object A bytes][object B bytes][object C ...]   │  ← stacked forward
//! │                                                   │
//! │                     [heap value C][heap value B]  │  ← growing from end
//! └──────────────────────────────────────────────────┘
//! ```

use wavedb_macros::WaveWire;

/// Fixed page header at the start of every page.
#[derive(Debug, Clone, WaveWire)]
pub struct PageHeader {
    /// CRC32 checksum of the page body (everything after the header).
    pub checksum: u32,
    /// Number of directory entries in this page.
    pub entry_count: u32,
    /// Byte offset where free stack space begins (grows forward).
    pub stack_offset: u32,
    /// Byte offset where free heap space begins (grows backward from end).
    pub heap_offset: u32,
    /// Dictionary version used to compress stack data on this page.
    pub dict_version: u32,
}

impl PageHeader {
    /// Size of a serialized page header in bytes.
    pub const SIZE: usize = 20; // 5 × u32

    /// Create a new empty page header for a given page size.
    pub const fn new(page_size: u32) -> Self {
        Self {
            checksum: 0,
            entry_count: 0,
            #[allow(clippy::cast_possible_truncation)]
            stack_offset: Self::SIZE as u32,
            heap_offset: page_size,
            dict_version: 0,
        }
    }

    /// How many free bytes remain between stack and heap regions.
    pub const fn free_space(&self) -> u32 {
        self.heap_offset.saturating_sub(self.stack_offset)
    }
}

/// A directory entry mapping an object ID to its byte range within a page.
#[derive(Debug, Clone, WaveWire)]
pub struct DirEntry {
    /// The 128-bit ID of the stored object.
    pub id: u128,
    /// Byte offset within the page where the object's stack data starts.
    pub offset: u32,
    /// Size of the object's stack data in bytes.
    pub size: u32,
}

impl DirEntry {
    /// Size of a serialized directory entry.
    pub const SIZE: usize = size_of::<Self>();
}

/// An in-memory representation of a page.
#[derive(Debug, Clone, WaveWire)]
pub struct Page {
    /// The page's header.
    pub header: PageHeader,
    /// Object directory.
    pub directory: Vec<DirEntry>,
    /// Raw page bytes (full page_size).
    pub data: Vec<u8>,
    /// The configured page size.
    pub page_size: usize,
}

impl Page {
    /// Create a new empty page.
    pub fn new(page_size: usize) -> Self {
        let header = PageHeader::new(
            u32::try_from(page_size).expect("page size overflow"),
        );
        Self {
            header,
            directory: Vec::new(),
            data: vec![0u8; page_size],
            page_size,
        }
    }

    /// Look up an object by its 128-bit ID. Returns the raw bytes if found.
    pub fn lookup(&self, id: u128) -> Option<&[u8]> {
        self.directory.iter().find(|e| e.id == id).map(|e| {
            let start = e.offset as usize;
            let end = start + e.size as usize;
            &self.data[start..end]
        })
    }

    /// Insert an object into the page. Returns `false` if there isn't enough space.
    pub fn insert(&mut self, id: u128, bytes: &[u8]) -> bool {
        let dir_entry_cost =
            u32::try_from(DirEntry::SIZE).expect("dir entry size overflow");
        let needed = dir_entry_cost
            + u32::try_from(bytes.len()).expect("object too large");

        if self.header.free_space() < needed {
            return false;
        }

        let offset = self.header.stack_offset;
        let end = offset as usize + bytes.len();
        self.data[offset as usize..end].copy_from_slice(bytes);

        self.directory.push(DirEntry {
            id,
            offset,
            size: u32::try_from(bytes.len()).expect("object too large for u32"),
        });

        self.header.stack_offset =
            u32::try_from(end).expect("stack offset overflow");
        self.header.entry_count += 1;
        true
    }

    /// Insert-or-replace an object by its ID.
    ///
    /// The page is a hash-map bucket, so a second write of the same ID must
    /// **replace** the first — `insert` alone would append a second
    /// directory entry and `lookup` (first match) would keep returning the
    /// stale bytes forever.
    ///
    /// Replacement leaves the old bytes as dead space; when the append
    /// region is exhausted but dead space exists, the page is compacted in
    /// place and the insert retried.  Returns `false` only when the live
    /// bytes genuinely don't fit.
    pub fn upsert(&mut self, id: u128, bytes: &[u8]) -> bool {
        self.remove(id);
        if self.insert(id, bytes) {
            return true;
        }
        // Append region exhausted — reclaim dead bytes if that would help.
        let dir_entry_cost = DirEntry::SIZE;
        let live: usize = self.directory.iter().map(|e| e.size as usize).sum();
        let capacity = self.header.heap_offset as usize
            - PageHeader::SIZE
            - (self.directory.len() + 1) * dir_entry_cost;
        if live + bytes.len() > capacity {
            return false;
        }
        self.compact();
        self.insert(id, bytes)
    }

    /// Rewrite the stack region so live entries are contiguous, reclaiming
    /// the dead space left behind by [`Self::remove`] / [`Self::upsert`].
    fn compact(&mut self) {
        let mut packed: Vec<u8> = Vec::with_capacity(
            self.directory.iter().map(|e| e.size as usize).sum(),
        );
        for entry in &mut self.directory {
            let start = entry.offset as usize;
            let end = start + entry.size as usize;
            #[allow(clippy::cast_possible_truncation)]
            {
                entry.offset = (PageHeader::SIZE + packed.len()) as u32;
            }
            packed.extend_from_slice(&self.data[start..end]);
        }
        let start = PageHeader::SIZE;
        let end = start + packed.len();
        self.data[start..end].copy_from_slice(&packed);
        // Zero the reclaimed tail so checksums don't depend on dead bytes.
        let heap_start = self.header.heap_offset as usize;
        self.data[end..heap_start].fill(0);
        self.header.stack_offset =
            u32::try_from(end).expect("stack offset overflow");
    }

    /// Remove an object by its ID. Returns `true` if it was found and removed.
    ///
    /// Note: This only removes the directory entry; it does not reclaim the
    /// space within the page. Compaction handles that.
    pub fn remove(&mut self, id: u128) -> bool {
        let before = self.directory.len();
        self.directory.retain(|e| e.id != id);
        let removed = self.directory.len() < before;
        if removed {
            self.header.entry_count = u32::try_from(self.directory.len())
                .expect("entry count overflow");
        }
        removed
    }

    /// Compute the CRC32 checksum for the current page data.
    pub fn compute_checksum(&self) -> u32 {
        crc32fast::hash(&self.data[PageHeader::SIZE..])
    }

    /// Update the header's checksum to match the current data.
    pub fn update_checksum(&mut self) {
        self.header.checksum = self.compute_checksum();
    }

    /// Verify the page's checksum. Returns `Ok(())` if valid.
    pub fn verify_checksum(&self) -> crate::StorageResult<()> {
        let actual = self.compute_checksum();
        if actual != self.header.checksum {
            return Err(crate::StorageError::ChecksumMismatch {
                page_index: 0, // caller should fill in
                expected: self.header.checksum,
                actual,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_page_has_correct_size() {
        let page = Page::new(4096);
        assert_eq!(page.data.len(), 4096);
        assert_eq!(page.header.entry_count, 0);
        assert_eq!(
            page.header.stack_offset,
            u32::try_from(PageHeader::SIZE).unwrap()
        );
        assert_eq!(page.header.heap_offset, 4096);
    }

    #[test]
    fn insert_and_lookup() {
        let mut page = Page::new(4096);
        let id = 42u128;
        let data = b"hello world";
        assert!(page.insert(id, data));
        let found = page.lookup(id).unwrap();
        assert_eq!(found, data);
    }

    #[test]
    fn insert_returns_false_when_full() {
        let mut page = Page::new(64); // tiny page
        let big_data = vec![0u8; 100];
        assert!(!page.insert(1, &big_data));
    }

    #[test]
    fn remove_entry() {
        let mut page = Page::new(4096);
        page.insert(1, b"aaa");
        page.insert(2, b"bbb");
        assert_eq!(page.header.entry_count, 2);

        assert!(page.remove(1));
        assert_eq!(page.header.entry_count, 1);
        assert!(page.lookup(1).is_none());
        assert!(page.lookup(2).is_some());
    }

    #[test]
    fn upsert_replaces_existing_entry() {
        let mut page = Page::new(4096);
        assert!(page.upsert(1, b"first"));
        assert!(page.upsert(1, b"second, longer payload"));
        assert_eq!(page.header.entry_count, 1);
        assert_eq!(page.lookup(1).unwrap(), b"second, longer payload");
    }

    #[test]
    fn upsert_compacts_dead_space_when_append_region_full() {
        // Page sized so repeated rewrites of one entry exhaust the append
        // region long before the live bytes do — upsert must keep working
        // by compacting in place.
        let mut page = Page::new(512);
        let payload = [0xAB; 100];
        for round in 0..50u8 {
            let body: Vec<u8> = payload.iter().map(|b| b ^ round).collect();
            assert!(page.upsert(7, &body), "round {round} failed");
            assert_eq!(page.lookup(7).unwrap(), body.as_slice());
        }
        assert_eq!(page.header.entry_count, 1);
    }

    #[test]
    fn upsert_still_fails_when_live_bytes_do_not_fit() {
        let mut page = Page::new(128);
        assert!(!page.upsert(1, &vec![0u8; 256]));
    }

    #[test]
    fn compact_preserves_all_live_entries() {
        let mut page = Page::new(1024);
        for id in 0..5u128 {
            assert!(page.insert(id, format!("payload-{id}").as_bytes()));
        }
        page.remove(2);
        page.compact();
        for id in [0u128, 1, 3, 4] {
            assert_eq!(
                page.lookup(id).unwrap(),
                format!("payload-{id}").as_bytes()
            );
        }
        assert!(page.lookup(2).is_none());
    }

    #[test]
    fn checksum_roundtrip() {
        let mut page = Page::new(4096);
        page.insert(1, b"test data");
        page.update_checksum();
        assert!(page.verify_checksum().is_ok());

        // Corrupt data
        page.data[PageHeader::SIZE + 1] ^= 0xFF;
        assert!(page.verify_checksum().is_err());
    }
}
