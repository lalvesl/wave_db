//! Hash-mapped page file — the primary on-disk key-value store.
//!
//! # File layout
//!
//! ```text
//! [page 0: header][page 1][page 2]...[page N]
//! ```
//!
//! Every page is exactly `page_size` bytes. Page 0 is the header page.
//!
//! ## Header page (page 0)
//! ```text
//! [u32 magic = 0x57415645 ("WAVE")][u32 page_size][u64 page_count][padding]
//! ```
//!
//! ## Data pages (pages 1..N)
//! Each is a [`PageFormat`] slotted page. The page index for a record is
//! computed by hashing its 128-bit ID modulo the number of data pages.
//!
//! ## Double-hashing collision resolution
//!
//! If the primary page is full, a secondary page is tried. If that is also
//! full, the file is grown by [`GROW_PAGES`] additional pages and the insert
//! is retried. The secondary hash uses a different mixing constant so that
//! both hashes are independent.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

use super::page_format::PageFormat;

const MAGIC: u32 = 0x5741_5645; // "WAVE"
const HEADER_MAGIC_OFF: u64 = 0;
const HEADER_PS_OFF: u64 = 4;
const HEADER_PC_OFF: u64 = 8;

const INITIAL_DATA_PAGES: u64 = 8;
const GROW_PAGES: u64 = 8;
const MAX_PROBE_STEPS: usize = 16;

// ─── DataFile ─────────────────────────────────────────────────────────────────

/// Hash-mapped paged file storing records by 128-bit ID.
pub struct DataFile {
    file: File,
    pub page_size: usize,
    pub page_count: u64, // includes header page (page 0)
}

impl DataFile {
    /// Open an existing data file or create a new one with `INITIAL_DATA_PAGES`.
    pub fn open_or_create(path: &Path, page_size: usize) -> io::Result<Self> {
        assert!(page_size >= 64, "page_size must be at least 64 bytes");

        if path.exists() {
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            let mut df = Self { file, page_size, page_count: 0 };
            df.read_header()?;
            if df.page_size != page_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "data file page_size mismatch: file={} requested={}",
                        df.page_size, page_size
                    ),
                ));
            }
            Ok(df)
        } else {
            let file = OpenOptions::new().create(true).read(true).write(true).open(path)?;
            let mut df = Self { file, page_size, page_count: 0 };
            // Write header page
            df.page_count = 1;
            df.write_header()?;
            // Write INITIAL_DATA_PAGES blank data pages
            df.grow_by(INITIAL_DATA_PAGES)?;
            Ok(df)
        }
    }

    // ── Header management ─────────────────────────────────────────────────────

    fn write_header(&mut self) -> io::Result<()> {
        let mut buf = vec![0u8; self.page_size];
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        buf[4..8].copy_from_slice(&(self.page_size as u32).to_le_bytes());
        buf[8..16].copy_from_slice(&self.page_count.to_le_bytes());
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&buf)
    }

    fn read_header(&mut self) -> io::Result<()> {
        let mut buf = vec![0u8; 16];
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut buf)?;
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad magic: {magic:#010x}"),
            ));
        }
        self.page_size = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        self.page_count = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        Ok(())
    }

    // ── Low-level page I/O ────────────────────────────────────────────────────

    fn read_page(&mut self, page_idx: u64) -> io::Result<PageFormat> {
        let offset = page_idx * self.page_size as u64;
        let mut buf = vec![0u8; self.page_size];
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut buf)?;
        PageFormat::from_bytes(buf, self.page_size)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    fn write_page(&mut self, page_idx: u64, page: &PageFormat) -> io::Result<()> {
        let offset = page_idx * self.page_size as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(page.as_bytes())
    }

    // ── Page hashing ─────────────────────────────────────────────────────────

    /// Number of usable data pages (total pages minus the header page).
    fn data_page_count(&self) -> u64 {
        self.page_count.saturating_sub(1).max(1)
    }

    /// Primary page index for a given ID (1-based, skipping header page 0).
    fn primary_page(&self, id: u128) -> u64 {
        let h = (id as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        (h % self.data_page_count()) + 1
    }

    /// Incremental probe step (always > 0 to guarantee full cycle).
    fn probe_step(&self, id: u128, step: usize) -> u64 {
        let h = ((id >> 64) as u64)
            .wrapping_mul(0x6c62_272e_07bb_0142)
            .wrapping_add(step as u64);
        (h % self.data_page_count()) + 1
    }

    /// Probe sequence: primary, then linear steps via `probe_step`.
    fn probe_pages(&self, id: u128) -> impl Iterator<Item = u64> + '_ {
        let primary = self.primary_page(id);
        let dpc = self.data_page_count();
        (0..MAX_PROBE_STEPS).scan(primary, move |page, step| {
            let current = *page;
            if step > 0 {
                *page = (*page + self.probe_step(id, step)) % dpc;
                if *page == 0 {
                    *page = 1;
                }
            }
            Some(current)
        })
    }

    // ── Public CRUD ───────────────────────────────────────────────────────────

    /// Insert a record. Probes pages until one has room; grows the file if needed.
    pub fn insert(&mut self, id: u128, data: &[u8]) -> io::Result<()> {
        loop {
            // Try probe sequence
            let pages: Vec<u64> = self.probe_pages(id).collect();
            for page_idx in pages {
                let mut page = self.read_page(page_idx)?;
                if page.can_fit(data.len()) {
                    page.insert(id, data)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                    self.write_page(page_idx, &page)?;
                    return Ok(());
                }
            }
            // All probed pages full — grow and retry
            self.grow_by(GROW_PAGES)?;
        }
    }

    /// Look up a record by ID. Probes the same sequence used by `insert`.
    pub fn get(&mut self, id: u128) -> io::Result<Option<Vec<u8>>> {
        for page_idx in self.probe_pages(id).collect::<Vec<_>>() {
            let page = self.read_page(page_idx)?;
            if let Some(bytes) = page.get(id) {
                return Ok(Some(bytes.to_vec()));
            }
            // If the page is completely empty, the record can't be on later probes
            if page.slot_count() == 0 {
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// Update a record in-place (same size) or via remove+insert (size changed).
    ///
    /// Returns `false` if the record was not found.
    pub fn update(&mut self, id: u128, new_data: &[u8]) -> io::Result<bool> {
        for page_idx in self.probe_pages(id).collect::<Vec<_>>() {
            let mut page = self.read_page(page_idx)?;
            if page.contains(id) {
                if page.update(id, new_data) {
                    self.write_page(page_idx, &page)?;
                    return Ok(true);
                }
                // Size changed: remove old, reinsert
                page.remove(id);
                self.write_page(page_idx, &page)?;
                self.insert(id, new_data)?;
                return Ok(true);
            }
            if page.slot_count() == 0 {
                return Ok(false);
            }
        }
        Ok(false)
    }

    /// Remove a record by ID (marks its slot as freed).
    ///
    /// Returns `false` if the record was not found.
    pub fn remove(&mut self, id: u128) -> io::Result<bool> {
        for page_idx in self.probe_pages(id).collect::<Vec<_>>() {
            let mut page = self.read_page(page_idx)?;
            if page.remove(id) {
                self.write_page(page_idx, &page)?;
                return Ok(true);
            }
            if page.slot_count() == 0 {
                return Ok(false);
            }
        }
        Ok(false)
    }

    /// Scan every data page and collect all live `(id, bytes)` pairs.
    ///
    /// Used by [`crate::disk_store::DiskStore`] on open to rebuild the in-memory cache.
    pub fn scan_all(&mut self) -> io::Result<Vec<(u128, Vec<u8>)>> {
        let mut out = Vec::new();
        for page_idx in 1..self.page_count {
            let page = self.read_page(page_idx)?;
            for (id, bytes) in page.iter_records() {
                out.push((id, bytes.to_vec()));
            }
        }
        Ok(out)
    }

    /// Append `n` new blank data pages to the file and update the header.
    pub fn grow_by(&mut self, n: u64) -> io::Result<()> {
        let blank = vec![0u8; self.page_size];
        for _ in 0..n {
            let offset = self.page_count * self.page_size as u64;
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(&blank)?;
            self.page_count += 1;
        }
        self.write_header()
    }

    /// `fsync` the data file.
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn create_and_reopen() {
        let p = tmp_path("wave_test_data_create.bin");
        cleanup(&p);
        {
            let df = DataFile::open_or_create(&p, 4096).unwrap();
            assert_eq!(df.page_size, 4096);
            assert_eq!(df.page_count, 1 + INITIAL_DATA_PAGES);
        }
        {
            let df = DataFile::open_or_create(&p, 4096).unwrap();
            assert_eq!(df.page_count, 1 + INITIAL_DATA_PAGES);
        }
        cleanup(&p);
    }

    #[test]
    fn insert_and_get() {
        let p = tmp_path("wave_test_data_ig.bin");
        cleanup(&p);
        let mut df = DataFile::open_or_create(&p, 4096).unwrap();
        let id = 0xDEAD_BEEF_CAFE_1234_u128;
        df.insert(id, b"hello disk").unwrap();
        let got = df.get(id).unwrap();
        assert_eq!(got.as_deref(), Some(b"hello disk".as_ref()));
        cleanup(&p);
    }

    #[test]
    fn insert_update_get() {
        let p = tmp_path("wave_test_data_upd.bin");
        cleanup(&p);
        let mut df = DataFile::open_or_create(&p, 4096).unwrap();
        let id = 0xABCD_u128;
        df.insert(id, b"v1-data").unwrap();
        df.update(id, b"v2-xxx").unwrap(); // same length (7 bytes each -> wait, "v1-data" is 7, "v2-xxx" is 6)
        // Use same-length strings
        df.update(id, b"updated").unwrap(); // "updated" = 7 bytes same as "v1-data"
        let got = df.get(id).unwrap();
        assert_eq!(got.as_deref(), Some(b"updated".as_ref()));
        cleanup(&p);
    }

    #[test]
    fn insert_update_different_size() {
        let p = tmp_path("wave_test_data_upd2.bin");
        cleanup(&p);
        let mut df = DataFile::open_or_create(&p, 4096).unwrap();
        let id = 0x1234_u128;
        df.insert(id, b"short").unwrap();
        df.update(id, b"a much longer string value").unwrap();
        let got = df.get(id).unwrap();
        assert_eq!(got.as_deref(), Some(b"a much longer string value".as_ref()));
        cleanup(&p);
    }

    #[test]
    fn remove_record() {
        let p = tmp_path("wave_test_data_rm.bin");
        cleanup(&p);
        let mut df = DataFile::open_or_create(&p, 4096).unwrap();
        let id = 0x5678_u128;
        df.insert(id, b"to delete").unwrap();
        assert!(df.remove(id).unwrap());
        assert_eq!(df.get(id).unwrap(), None);
        cleanup(&p);
    }

    #[test]
    fn scan_all_returns_inserted_records() {
        let p = tmp_path("wave_test_data_scan.bin");
        cleanup(&p);
        let mut df = DataFile::open_or_create(&p, 4096).unwrap();
        df.insert(1u128, b"one").unwrap();
        df.insert(2u128, b"two").unwrap();
        df.insert(3u128, b"three").unwrap();
        let all = df.scan_all().unwrap();
        let ids: Vec<u128> = all.iter().map(|(id, _)| *id).collect();
        for expected in [1u128, 2, 3] {
            assert!(ids.contains(&expected), "missing id {expected}");
        }
        cleanup(&p);
    }

    #[test]
    fn many_inserts_trigger_grow() {
        let p = tmp_path("wave_test_data_grow.bin");
        cleanup(&p);
        // Small pages to force grow quickly
        let mut df = DataFile::open_or_create(&p, 256).unwrap();
        let initial_pages = df.page_count;
        // Insert enough records to fill all initial pages
        for i in 0u128..100 {
            df.insert(i + 1000, &vec![0u8; 64]).unwrap();
        }
        assert!(df.page_count > initial_pages, "file should have grown");
        cleanup(&p);
    }
}
