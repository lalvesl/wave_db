//! In-memory page layout.
//!
//! A page is the fundamental unit of IO. Its binary layout mirrors the
//! README specification:
//!
//! ```text
//! ┌──────────────────────────────────────────────────┐
//! │  Vec<(ID, offset, size)>   ← object directory   │
//! │  ──────────────────────────────────────────────  │
//! │  [object A bytes][object B bytes][object C ...]  │  ← stacked forward
//! │                                                  │
//! │              [heap value C][heap value B]        │  ← growing from end
//! └──────────────────────────────────────────────────┘
//! ```
//!
//! Multiple `(STRUCT_ID, TENANT_ID)` pairs coexist in the same page. The
//! directory at the front allows O(1) lookup of any object's byte range.

use crate::id::{Id, StructId, TenantId};
use std::collections::HashMap;

// ─── Directory entry ─────────────────────────────────────────────────────────

/// One entry in the page directory: locates an object's serialised bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectoryEntry {
    /// Composite record ID.
    pub id: Id,
    /// Byte offset of the object data from the start of the data region.
    pub offset: u32,
    /// Byte length of the object data.
    pub size: u32,
}

// ─── Page address ─────────────────────────────────────────────────────────────

/// The hash key used to locate a page for a given `(STRUCT_ID, TENANT_ID)` pair.
///
/// Anchor slots omit the timestamp; versioned records include it as part of
/// their full `Id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageKey {
    pub struct_id: StructId,
    pub tenant_id: TenantId,
}

impl PageKey {
    pub const fn new(struct_id: StructId, tenant_id: TenantId) -> Self {
        Self {
            struct_id,
            tenant_id,
        }
    }
}

// ─── Page ────────────────────────────────────────────────────────────────────

/// An in-memory page holding object data and a forward heap region.
///
/// This implementation models the logical layout described in the README.
/// A real storage engine would map this to a fixed-size on-disk block; here
/// we use a `Vec<u8>` for flexibility during the research phase.
#[derive(Debug)]
pub struct Page {
    /// Maximum byte capacity of this page.
    capacity: usize,
    /// Object directory: maps record `Id` → `DirectoryEntry`.
    pub(crate) directory: HashMap<Id, DirectoryEntry>,
    /// Forward-growing object data region (bytes from offset 0 upward).
    data: Vec<u8>,
    /// Reverse-growing heap region (bytes from `capacity` downward).
    heap: Vec<u8>,
    /// Fill percentage threshold at which a warning is emitted.
    pub warning_fill_pct: u8,
}

impl Page {
    /// Create a new empty page.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            directory: HashMap::new(),
            data: Vec::new(),
            heap: Vec::new(),
            warning_fill_pct: 70,
        }
    }

    // ── Metrics ──────────────────────────────────────────────────────────────

    /// Total bytes currently occupied (data + heap regions).
    #[inline]
    pub fn occupied(&self) -> usize {
        self.data.len() + self.heap.len()
    }

    /// Remaining free bytes.
    #[inline]
    pub fn free(&self) -> usize {
        self.capacity.saturating_sub(self.occupied())
    }

    /// Current fill as a percentage (0–100).
    #[inline]
    pub fn fill_pct(&self) -> u8 {
        ((self.occupied() * 100) / self.capacity.max(1)).min(100) as u8
    }

    /// Returns `true` if the page has exceeded the warning fill threshold.
    #[inline]
    pub fn is_warn_full(&self) -> bool {
        self.fill_pct() >= self.warning_fill_pct
    }

    // ── Object insertion ─────────────────────────────────────────────────────

    /// Insert an object's serialised bytes into the page data region.
    ///
    /// Returns `Err(PageFull)` if there isn't enough space.
    pub fn insert(&mut self, id: Id, bytes: &[u8]) -> Result<(), PageError> {
        if bytes.len() > self.free() {
            return Err(PageError::PageFull);
        }
        let offset = self.data.len() as u32;
        let size = bytes.len() as u32;
        self.data.extend_from_slice(bytes);
        self.directory
            .insert(id, DirectoryEntry { id, offset, size });
        Ok(())
    }

    /// Insert a heap value (variable-length, grown from the end).
    ///
    /// Returns the byte offset within the heap region, or `Err(PageFull)`.
    pub fn insert_heap(&mut self, bytes: &[u8]) -> Result<u32, PageError> {
        if bytes.len() > self.free() {
            return Err(PageError::PageFull);
        }
        let offset_from_end = self.heap.len() as u32;
        self.heap.extend_from_slice(bytes);
        Ok(offset_from_end)
    }

    /// Look up the raw bytes for a given record ID.
    pub fn get(&self, id: Id) -> Option<&[u8]> {
        let entry = self.directory.get(&id)?;
        let start = entry.offset as usize;
        let end = start + entry.size as usize;
        self.data.get(start..end)
    }

    /// Remove a record from this page, marking its space as free.
    ///
    /// In the current in-memory model this simply removes the directory entry;
    /// a real on-disk implementation would track free-space deltas in the
    /// journal for later compaction.
    pub fn remove(&mut self, id: Id) -> bool {
        self.directory.remove(&id).is_some()
    }

    /// Returns `true` if this page contains a record with the given ID.
    pub fn contains(&self, id: Id) -> bool {
        self.directory.contains_key(&id)
    }

    /// Iterator over all directory entries in the page.
    pub fn entries(&self) -> impl Iterator<Item = &DirectoryEntry> {
        self.directory.values()
    }
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageError {
    /// The page has no remaining capacity.
    PageFull,
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PageFull => write!(f, "page is full"),
        }
    }
}

impl std::error::Error for PageError {}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{ShardId, Slider, StructId, TenantId, Timestamp};

    fn make_id(tick: u64) -> Id {
        Id::new(
            TenantId::new(1),
            ShardId::new(0),
            StructId::new(2),
            Timestamp::from_ticks(tick),
            Slider::new(0),
        )
    }

    #[test]
    fn insert_and_get() {
        let mut page = Page::new(4096);
        let id = make_id(1);
        page.insert(id, b"hello world").unwrap();
        assert_eq!(page.get(id), Some(b"hello world".as_ref()));
    }

    #[test]
    fn page_full_error() {
        let mut page = Page::new(10);
        let id = make_id(1);
        let result = page.insert(id, &[0u8; 11]);
        assert_eq!(result, Err(PageError::PageFull));
    }

    #[test]
    fn remove_entry() {
        let mut page = Page::new(4096);
        let id = make_id(2);
        page.insert(id, b"data").unwrap();
        assert!(page.contains(id));
        assert!(page.remove(id));
        assert!(!page.contains(id));
    }

    #[test]
    fn fill_pct() {
        let mut page = Page::new(100);
        page.insert(make_id(1), &[0u8; 50]).unwrap();
        assert_eq!(page.fill_pct(), 50);
    }

    #[test]
    fn warn_full() {
        let mut page = Page::new(100);
        page.warning_fill_pct = 70;
        page.insert(make_id(1), &[0u8; 75]).unwrap();
        assert!(page.is_warn_full());
    }
}
