//! Slotted-page layout — in-memory representation of one fixed-size page.
//!
//! # Binary layout
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │ [u32 data_used]  (4 bytes at offset 0)                   │
//! │ ──────────────────────────────────────────────────────── │
//! │ data bytes   →  growing upward from offset 4             │
//! │                                                          │
//! │                         free space                       │
//! │                                                          │
//! │  ←  slot directory growing downward (24 bytes / slot)    │
//! │ [u32 slot_count] (4 bytes at end of page)                │
//! └──────────────────────────────────────────────────────────┘
//! ```
//!
//! Each slot: `[u128 id (16 B)][u32 data_offset (4 B)][u32 data_size (4 B)]` = 24 bytes.
//! `data_offset` is the byte offset from the start of the data region (byte 4 of the page).
//! A slot with `id == 0` is a freed/empty slot and is skipped during iteration.

use std::io;

const HEADER: usize = 4; // data_used: u32
const FOOTER: usize = 4; // slot_count: u32
pub const SLOT_SIZE: usize = 24; // id(16) + offset(4) + size(4)

// ─── PageFormat ───────────────────────────────────────────────────────────────

/// In-memory owner of one page's bytes with slotted-directory management.
#[derive(Debug, Clone)]
pub struct PageFormat {
    buf: Vec<u8>,
    page_size: usize,
}

/// Errors from page operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageFormatError {
    /// The bytes slice length does not match the expected page size.
    SizeMismatch { got: usize, want: usize },
    /// Not enough contiguous free bytes to store the record.
    PageFull,
}

impl std::fmt::Display for PageFormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SizeMismatch { got, want } => {
                write!(f, "page size mismatch: got {got}, want {want}")
            }
            Self::PageFull => write!(f, "page is full"),
        }
    }
}

impl std::error::Error for PageFormatError {}

impl From<PageFormatError> for io::Error {
    fn from(e: PageFormatError) -> Self {
        io::Error::new(io::ErrorKind::Other, e.to_string())
    }
}

impl PageFormat {
    /// Create a new, empty page of `page_size` bytes (all zeros).
    pub fn new(page_size: usize) -> Self {
        assert!(page_size >= HEADER + FOOTER, "page_size too small");
        Self { buf: vec![0u8; page_size], page_size }
    }

    /// Wrap existing raw bytes as a page.
    /// Returns an error if `buf.len() != page_size`.
    pub fn from_bytes(buf: Vec<u8>, page_size: usize) -> Result<Self, PageFormatError> {
        if buf.len() != page_size {
            return Err(PageFormatError::SizeMismatch { got: buf.len(), want: page_size });
        }
        Ok(Self { buf, page_size })
    }

    // ── Header / footer accessors ─────────────────────────────────────────────

    pub fn data_used(&self) -> u32 {
        u32::from_le_bytes(self.buf[0..4].try_into().unwrap())
    }

    pub fn slot_count(&self) -> u32 {
        let end = self.page_size;
        u32::from_le_bytes(self.buf[end - 4..end].try_into().unwrap())
    }

    fn set_data_used(&mut self, v: u32) {
        self.buf[0..4].copy_from_slice(&v.to_le_bytes());
    }

    fn set_slot_count(&mut self, v: u32) {
        let end = self.page_size;
        self.buf[end - 4..end].copy_from_slice(&v.to_le_bytes());
    }

    // ── Space accounting ─────────────────────────────────────────────────────

    /// Bytes available for a new record (data bytes + one new slot).
    pub fn free_bytes(&self) -> usize {
        let used = HEADER + self.data_used() as usize + self.slot_count() as usize * SLOT_SIZE + FOOTER;
        self.page_size.saturating_sub(used)
    }

    /// Returns `true` if a record of `data_len` bytes can be inserted.
    pub fn can_fit(&self, data_len: usize) -> bool {
        data_len + SLOT_SIZE <= self.free_bytes()
    }

    // ── Slot directory helpers ────────────────────────────────────────────────

    /// Byte offset of the start of slot `slot_idx` (0 = first/oldest slot).
    /// Slots are packed from the end of the page (before the footer) downward.
    fn slot_start(&self, slot_idx: u32) -> usize {
        self.page_size - FOOTER - (slot_idx as usize + 1) * SLOT_SIZE
    }

    fn read_slot(&self, slot_idx: u32) -> (u128, u32, u32) {
        let off = self.slot_start(slot_idx);
        let id = u128::from_le_bytes(self.buf[off..off + 16].try_into().unwrap());
        let offset = u32::from_le_bytes(self.buf[off + 16..off + 20].try_into().unwrap());
        let size = u32::from_le_bytes(self.buf[off + 20..off + 24].try_into().unwrap());
        (id, offset, size)
    }

    fn write_slot(&mut self, slot_idx: u32, id: u128, offset: u32, size: u32) {
        let off = self.slot_start(slot_idx);
        self.buf[off..off + 16].copy_from_slice(&id.to_le_bytes());
        self.buf[off + 16..off + 20].copy_from_slice(&offset.to_le_bytes());
        self.buf[off + 20..off + 24].copy_from_slice(&size.to_le_bytes());
    }

    // ── CRUD ─────────────────────────────────────────────────────────────────

    /// Insert a record. Returns `Err(PageFull)` if the page has insufficient space.
    pub fn insert(&mut self, id: u128, record: &[u8]) -> Result<(), PageFormatError> {
        if !self.can_fit(record.len()) {
            return Err(PageFormatError::PageFull);
        }
        let data_offset = self.data_used(); // relative to HEADER
        let abs_start = HEADER + data_offset as usize;
        self.buf[abs_start..abs_start + record.len()].copy_from_slice(record);

        let slot_count = self.slot_count();
        self.write_slot(slot_count, id, data_offset, record.len() as u32);

        self.set_data_used(data_offset + record.len() as u32);
        self.set_slot_count(slot_count + 1);
        Ok(())
    }

    /// Look up a record by its 128-bit ID.
    pub fn get(&self, id: u128) -> Option<&[u8]> {
        let slot_count = self.slot_count();
        for i in 0..slot_count {
            let (slot_id, offset, size) = self.read_slot(i);
            if slot_id == id {
                let start = HEADER + offset as usize;
                return Some(&self.buf[start..start + size as usize]);
            }
        }
        None
    }

    /// Overwrite a record in-place. Only succeeds if `new_record.len() == old_size`.
    /// Returns `true` on success, `false` if the ID was not found or size differs.
    pub fn update(&mut self, id: u128, new_record: &[u8]) -> bool {
        let slot_count = self.slot_count();
        for i in 0..slot_count {
            let (slot_id, offset, size) = self.read_slot(i);
            if slot_id == id {
                if size as usize != new_record.len() {
                    return false;
                }
                let start = HEADER + offset as usize;
                self.buf[start..start + size as usize].copy_from_slice(new_record);
                return true;
            }
        }
        false
    }

    /// Mark a record as removed by zeroing its slot's ID field.
    /// The data bytes remain (fragmentation reclaimed during compaction).
    /// Returns `true` if the record was found.
    pub fn remove(&mut self, id: u128) -> bool {
        let slot_count = self.slot_count();
        for i in 0..slot_count {
            let (slot_id, _, _) = self.read_slot(i);
            if slot_id == id {
                // Zero only the id field of the slot (first 16 bytes)
                let off = self.slot_start(i);
                self.buf[off..off + 16].fill(0);
                return true;
            }
        }
        false
    }

    /// Returns `true` if the page contains a record with the given ID.
    pub fn contains(&self, id: u128) -> bool {
        self.get(id).is_some()
    }

    /// Iterate over all live (non-zero-id) slot entries as `(id, data_bytes)`.
    pub fn iter_records(&self) -> impl Iterator<Item = (u128, &[u8])> {
        let slot_count = self.slot_count();
        (0..slot_count).filter_map(move |i| {
            let (id, offset, size) = self.read_slot(i);
            if id == 0 {
                return None;
            }
            let start = HEADER + offset as usize;
            Some((id, &self.buf[start..start + size as usize]))
        })
    }

    /// Raw page bytes (exactly `page_size`).
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const PS: usize = 4096;

    #[test]
    fn empty_page_has_max_free() {
        let p = PageFormat::new(PS);
        // free = page_size - HEADER - FOOTER = 4096 - 8 = 4088
        assert_eq!(p.free_bytes(), PS - HEADER - FOOTER);
        assert_eq!(p.data_used(), 0);
        assert_eq!(p.slot_count(), 0);
    }

    #[test]
    fn insert_and_get() {
        let mut p = PageFormat::new(PS);
        let id = 0xDEAD_BEEF_u128;
        p.insert(id, b"hello").unwrap();
        assert_eq!(p.get(id), Some(b"hello".as_ref()));
        assert_eq!(p.slot_count(), 1);
        assert_eq!(p.data_used(), 5);
    }

    #[test]
    fn free_bytes_decreases() {
        let mut p = PageFormat::new(PS);
        let before = p.free_bytes();
        p.insert(1u128, &[0u8; 100]).unwrap();
        let after = p.free_bytes();
        assert_eq!(before - after, 100 + SLOT_SIZE);
    }

    #[test]
    fn page_full_error() {
        let mut p = PageFormat::new(PS);
        let result = p.insert(1, &vec![0u8; PS]); // definitely too big
        assert_eq!(result, Err(PageFormatError::PageFull));
    }

    #[test]
    fn update_same_size() {
        let mut p = PageFormat::new(PS);
        p.insert(42u128, b"hello").unwrap();
        assert!(p.update(42, b"world"));
        assert_eq!(p.get(42), Some(b"world".as_ref()));
    }

    #[test]
    fn update_size_mismatch_returns_false() {
        let mut p = PageFormat::new(PS);
        p.insert(42u128, b"hello").unwrap();
        assert!(!p.update(42, b"hi")); // shorter
    }

    #[test]
    fn remove_hides_record() {
        let mut p = PageFormat::new(PS);
        p.insert(99u128, b"data").unwrap();
        assert!(p.contains(99));
        assert!(p.remove(99));
        assert!(!p.contains(99));
        assert!(p.get(99).is_none());
    }

    #[test]
    fn iter_records_skips_removed() {
        let mut p = PageFormat::new(PS);
        p.insert(1u128, b"a").unwrap();
        p.insert(2u128, b"b").unwrap();
        p.insert(3u128, b"c").unwrap();
        p.remove(2);
        let ids: Vec<u128> = p.iter_records().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn multiple_inserts() {
        let mut p = PageFormat::new(PS);
        for i in 0u128..10 {
            p.insert(i, &i.to_le_bytes()).unwrap();
        }
        for i in 0u128..10 {
            let bytes = p.get(i).unwrap();
            assert_eq!(u128::from_le_bytes(bytes.try_into().unwrap()), i);
        }
    }

    #[test]
    fn from_bytes_size_mismatch() {
        let buf = vec![0u8; 100];
        assert!(PageFormat::from_bytes(buf, PS).is_err());
    }

    #[test]
    fn roundtrip_via_as_bytes() {
        let mut p = PageFormat::new(PS);
        p.insert(123u128, b"test data").unwrap();
        let raw = p.as_bytes().to_vec();
        let p2 = PageFormat::from_bytes(raw, PS).unwrap();
        assert_eq!(p2.get(123), Some(b"test data".as_ref()));
    }
}
