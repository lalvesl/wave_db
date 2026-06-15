//! The page descriptor — one `u64` per page in a type's page directory.
//!
//! Each live `(STRUCT_ID, struct_version)` owns a `Vec<u64>` page directory;
//! every entry is a [`PageDescriptor`] locating one homogeneous page in the
//! `data` file (see the _Per-Type Page Directory_ section of the project
//! readme).  This module is the bit codec for that `u64`; the directory
//! itself (the `Vec<u64>` + `hash % len` routing) lands here next.
//!
//! # Layout
//!
//! ```text
//! bits 63..16 │ start_block : u48   first 4 KiB block of the page
//! bits 15..8  │ block_count : u8    contiguous blocks (≤ 255 ⇒ ≤ ~1 MiB)
//! bits  7..2  │ occupation  : u6    fill gauge in 1/64ths (0 empty … 63 full)
//! bit      1  │ in_journal         page lives in the journal, not data.bin
//! bit      0  │ in_memory          page resident in memory (reserved/deferred)
//! ```
//!
//! `start_block` + `block_count` are exactly a [`BlockRun`]; `occupation` is a
//! cached summary the allocator reads from the directory **without touching
//! the page**, so it can pick a page to grow or a relocation victim without a
//! disk read.  An all-zero descriptor ([`PageDescriptor::EMPTY`]) is an
//! unallocated slot — `block_count == 0` means "no page here yet".
//!
//! # The `in_journal` flag
//!
//! When `in_journal` is set, the page's bytes live in the **journal**, not the
//! `data` file: [`start_block`](PageDescriptor::start_block) is reinterpreted
//! as a journal address. Journal metadata records when the page is migrated
//! into `data`; once the drain copies it down, the descriptor is rewritten in
//! place (flag cleared, address = the new `data` block). This lets pages be
//! **staged in the journal during balancing / transitions** without forcing
//! the whole working set into memory.
//!
//! `in_memory` (bit 0) is reserved for a future resident-page path and is not
//! acted on yet.

use crate::file::block_alloc::BlockRun;

/// Bit width of the `start_block` field.
pub const START_BLOCK_BITS: u32 = 48;
/// Bit width of the `block_count` field.
pub const BLOCK_COUNT_BITS: u32 = 8;
/// Bit width of the `occupation` field.
pub const OCCUPATION_BITS: u32 = 6;

const OCC_SHIFT: u32 = 2;
const COUNT_SHIFT: u32 = OCC_SHIFT + OCCUPATION_BITS; // 8
const START_SHIFT: u32 = COUNT_SHIFT + BLOCK_COUNT_BITS; // 16

const OCC_MASK: u64 = (1 << OCCUPATION_BITS) - 1; // 0x3F
const COUNT_MASK: u64 = (1 << BLOCK_COUNT_BITS) - 1; // 0xFF
const START_MASK: u64 = (1 << START_BLOCK_BITS) - 1; // 0xFFFF_FFFF_FFFF

/// Both low flag bits (`in_journal` | `in_memory`).
const FLAGS_MASK: u64 = (1 << OCC_SHIFT) - 1; // 0x3
/// `in_journal` — page lives in the journal, address is a journal location.
const IN_JOURNAL_MASK: u64 = 1 << 1; // bit 1
/// `in_memory` — page resident in memory (reserved / deferred).
const IN_MEMORY_MASK: u64 = 1 << 0; // bit 0

/// Largest representable `start_block` (2⁴⁸ − 1 ⇒ 1 EiB of 4 KiB blocks).
pub const MAX_START_BLOCK: u64 = START_MASK;
/// Largest representable `block_count` (a full `u8`).
pub const MAX_BLOCK_COUNT: u8 = u8::MAX;
/// `occupation` denominator — the gauge is in `1/OCCUPATION_SCALE` units.
///
/// Note this is 64 while the max *value* is 63: a full page saturates at 63,
/// and the config default `page_grow_occupation = 48` is `48/64 = 75%`.
pub const OCCUPATION_SCALE: u8 = 1 << OCCUPATION_BITS; // 64
/// Largest representable `occupation` value (saturated / "full").
pub const MAX_OCCUPATION: u8 = (1u8 << OCCUPATION_BITS) - 1; // 63

/// A packed page-directory entry. See the module docs for the bit layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageDescriptor(u64);

impl PageDescriptor {
    /// The empty slot — no page allocated (`block_count == 0`).
    pub const EMPTY: Self = Self(0);

    /// Pack a descriptor from its three fields. Reserved bits are zeroed.
    ///
    /// # Panics
    ///
    /// Panics if `start_block > MAX_START_BLOCK` or `occupation >
    /// MAX_OCCUPATION` — both are caller bugs (the field can't hold the value).
    #[must_use]
    pub const fn new(start_block: u64, block_count: u8, occupation: u8) -> Self {
        assert!(start_block <= MAX_START_BLOCK, "start_block exceeds u48");
        assert!(occupation <= MAX_OCCUPATION, "occupation exceeds u6");
        Self(
            (start_block << START_SHIFT)
                | ((block_count as u64) << COUNT_SHIFT)
                | ((occupation as u64) << OCC_SHIFT),
        )
    }

    /// Build a descriptor for a [`BlockRun`] at a given occupation.
    ///
    /// # Panics
    ///
    /// Panics if `run.len > MAX_BLOCK_COUNT` (a page can't exceed 255 blocks)
    /// or the field-range checks in [`new`](Self::new) fail.
    #[must_use]
    pub const fn from_run(run: BlockRun, occupation: u8) -> Self {
        assert!(run.len <= MAX_BLOCK_COUNT as u64, "run exceeds 255 blocks");
        #[allow(clippy::cast_possible_truncation)]
        Self::new(run.start, run.len as u8, occupation)
    }

    /// Reinterpret a raw `u64` as a descriptor (reserved bits preserved).
    #[inline]
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw `u64`, e.g. for journaling or writing the directory.
    #[inline]
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// First block of the page in the `data` file.
    #[inline]
    #[must_use]
    pub const fn start_block(self) -> u64 {
        (self.0 >> START_SHIFT) & START_MASK
    }

    /// Number of contiguous blocks the page occupies.
    #[inline]
    #[must_use]
    pub const fn block_count(self) -> u8 {
        #[allow(clippy::cast_possible_truncation)]
        (((self.0 >> COUNT_SHIFT) & COUNT_MASK) as u8)
    }

    /// Fill gauge in `1/OCCUPATION_SCALE` units (0 empty … 63 full).
    #[inline]
    #[must_use]
    pub const fn occupation(self) -> u8 {
        #[allow(clippy::cast_possible_truncation)]
        (((self.0 >> OCC_SHIFT) & OCC_MASK) as u8)
    }

    /// Whether this page lives in the **journal** rather than the `data` file.
    ///
    /// When set, [`start_block`](Self::start_block) /
    /// [`journal_addr`](Self::journal_addr) address the journal; the drain
    /// later migrates the page into `data` and rewrites this descriptor (flag
    /// cleared, address = the new `data` block). See the module docs.
    #[inline]
    #[must_use]
    pub const fn is_in_journal(self) -> bool {
        self.0 & IN_JOURNAL_MASK != 0
    }

    /// Whether this page is resident in memory. **Reserved / deferred** — the
    /// in-memory page path is not implemented yet.
    #[inline]
    #[must_use]
    pub const fn is_in_memory(self) -> bool {
        self.0 & IN_MEMORY_MASK != 0
    }

    /// Set or clear the [`is_in_journal`](Self::is_in_journal) flag.
    #[inline]
    #[must_use]
    pub const fn with_in_journal(self, on: bool) -> Self {
        if on {
            Self(self.0 | IN_JOURNAL_MASK)
        } else {
            Self(self.0 & !IN_JOURNAL_MASK)
        }
    }

    /// Set or clear the [`is_in_memory`](Self::is_in_memory) flag (reserved).
    #[inline]
    #[must_use]
    pub const fn with_in_memory(self, on: bool) -> Self {
        if on {
            Self(self.0 | IN_MEMORY_MASK)
        } else {
            Self(self.0 & !IN_MEMORY_MASK)
        }
    }

    /// The page's journal address — the [`start_block`](Self::start_block)
    /// bits reinterpreted as a journal location. Meaningful only when
    /// [`is_in_journal`](Self::is_in_journal) is set.
    #[inline]
    #[must_use]
    pub const fn journal_addr(self) -> u64 {
        self.start_block()
    }

    /// Whether a page is allocated for this slot (`block_count != 0`).
    #[inline]
    #[must_use]
    pub const fn is_allocated(self) -> bool {
        self.block_count() != 0
    }

    /// The page's block run (`start_block`, `block_count`).
    #[inline]
    #[must_use]
    pub const fn run(self) -> BlockRun {
        BlockRun {
            start: self.start_block(),
            len: self.block_count() as u64,
        }
    }

    /// `occupation` as a 0.0–~0.984 fraction (`occ / OCCUPATION_SCALE`).
    #[inline]
    #[must_use]
    pub fn occupation_fraction(self) -> f64 {
        f64::from(self.occupation()) / f64::from(OCCUPATION_SCALE)
    }

    /// Same descriptor with a new occupation gauge (run + flag bits preserved).
    ///
    /// # Panics
    ///
    /// Panics if `occupation > MAX_OCCUPATION`.
    #[must_use]
    pub const fn with_occupation(self, occupation: u8) -> Self {
        assert!(occupation <= MAX_OCCUPATION, "occupation exceeds u6");
        Self(
            (self.0 & !(OCC_MASK << OCC_SHIFT))
                | ((occupation as u64) << OCC_SHIFT),
        )
    }

    /// Same descriptor pointing at a new block run (occupation + flags kept).
    ///
    /// Used after a page is relocated to a larger run: the routing index in
    /// the directory is unchanged, only the bytes' location moves.
    ///
    /// # Panics
    ///
    /// Panics if `run.len > MAX_BLOCK_COUNT` or `run.start > MAX_START_BLOCK`.
    #[must_use]
    pub const fn with_run(self, run: BlockRun) -> Self {
        assert!(run.len <= MAX_BLOCK_COUNT as u64, "run exceeds 255 blocks");
        assert!(run.start <= MAX_START_BLOCK, "start_block exceeds u48");
        // Replace start_block (63..16) and block_count (15..8); keep the
        // occupation (7..2) and flag (1..0) bits untouched.
        let keep = self.0 & ((OCC_MASK << OCC_SHIFT) | FLAGS_MASK);
        Self((run.start << START_SHIFT) | (run.len << COUNT_SHIFT) | keep)
    }
}

impl Default for PageDescriptor {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Compute an `occupation` gauge (0..=63) from used vs. total page bytes.
///
/// Floors `used / total * OCCUPATION_SCALE` and saturates at
/// [`MAX_OCCUPATION`]. A page that is byte-for-byte full reports 63, the
/// largest the `u6` field can hold.
#[must_use]
pub fn occupation_for(used_bytes: u64, total_bytes: u64) -> u8 {
    if total_bytes == 0 {
        return 0;
    }
    let q = used_bytes.saturating_mul(u64::from(OCCUPATION_SCALE)) / total_bytes;
    #[allow(clippy::cast_possible_truncation)]
    (q.min(u64::from(MAX_OCCUPATION)) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_fields() {
        let d = PageDescriptor::new(0x1234_5678_9ABC, 200, 48);
        assert_eq!(d.start_block(), 0x1234_5678_9ABC);
        assert_eq!(d.block_count(), 200);
        assert_eq!(d.occupation(), 48);
        assert!(!d.is_in_journal());
        assert!(!d.is_in_memory());
        assert!(d.is_allocated());
        assert_eq!(PageDescriptor::from_raw(d.raw()), d);
    }

    #[test]
    fn fields_are_independent() {
        // Each field at its max with the others zero — no bleed between them.
        assert_eq!(
            PageDescriptor::new(MAX_START_BLOCK, 0, 0).start_block(),
            MAX_START_BLOCK
        );
        assert_eq!(
            PageDescriptor::new(0, MAX_BLOCK_COUNT, 0).block_count(),
            MAX_BLOCK_COUNT
        );
        assert_eq!(
            PageDescriptor::new(0, 0, MAX_OCCUPATION).occupation(),
            MAX_OCCUPATION
        );
        // A fully-saturated descriptor: every non-flag bit set.
        let full =
            PageDescriptor::new(MAX_START_BLOCK, MAX_BLOCK_COUNT, MAX_OCCUPATION);
        assert_eq!(full.raw(), !0x3u64); // every bit but the two flag bits
    }

    #[test]
    fn empty_is_unallocated() {
        let e = PageDescriptor::EMPTY;
        assert_eq!(e.raw(), 0);
        assert_eq!(e.block_count(), 0);
        assert!(!e.is_allocated());
        assert_eq!(PageDescriptor::default(), e);
    }

    #[test]
    fn run_bridges_block_run() {
        let run = BlockRun { start: 4096, len: 4 };
        let d = PageDescriptor::from_run(run, 10);
        assert_eq!(d.run(), run);
        assert_eq!(d.occupation(), 10);
        assert_eq!(d.start_block(), 4096);
        assert_eq!(d.block_count(), 4);
    }

    #[test]
    fn with_occupation_keeps_run() {
        let d = PageDescriptor::new(99, 4, 10).with_occupation(60);
        assert_eq!(d.occupation(), 60);
        assert_eq!(d.start_block(), 99);
        assert_eq!(d.block_count(), 4);
    }

    #[test]
    fn journal_and_memory_flags() {
        let d = PageDescriptor::new(123, 4, 30);
        assert!(!d.is_in_journal());
        assert!(!d.is_in_memory());

        let j = d.with_in_journal(true);
        assert!(j.is_in_journal());
        assert!(!j.is_in_memory());
        // The flag must not disturb the packed fields.
        assert_eq!(j.start_block(), 123);
        assert_eq!(j.block_count(), 4);
        assert_eq!(j.occupation(), 30);
        assert_eq!(j.journal_addr(), 123);
        // Clearing returns the original descriptor exactly.
        assert_eq!(j.with_in_journal(false), d);

        let m = d.with_in_memory(true);
        assert!(m.is_in_memory());
        assert!(!m.is_in_journal());
    }

    #[test]
    fn flags_survive_with_run_and_with_occupation() {
        let d = PageDescriptor::new(10, 4, 20).with_in_journal(true);
        // Migrating the page to a data block keeps the (caller-managed) flag
        // until it is explicitly cleared.
        let moved = d.with_run(BlockRun { start: 99, len: 8 });
        assert!(moved.is_in_journal());
        assert_eq!(moved.start_block(), 99);
        assert_eq!(moved.block_count(), 8);
        let reoccupied = d.with_occupation(40);
        assert!(reoccupied.is_in_journal());
        assert_eq!(reoccupied.occupation(), 40);
    }

    #[test]
    fn with_run_keeps_occupation() {
        // Relocate a page to a bigger run; occupation must survive unchanged.
        let d = PageDescriptor::new(10, 4, 33);
        let moved = d.with_run(BlockRun { start: 500, len: 8 });
        assert_eq!(moved.start_block(), 500);
        assert_eq!(moved.block_count(), 8);
        assert_eq!(moved.occupation(), 33);
    }

    #[test]
    fn occupation_fraction_matches_config_default() {
        // page_grow_occupation = 48 ⇒ 75%.
        let d = PageDescriptor::new(0, 4, 48);
        assert!((d.occupation_fraction() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn occupation_for_floors_and_saturates() {
        assert_eq!(occupation_for(0, 16384), 0);
        assert_eq!(occupation_for(16384, 16384), MAX_OCCUPATION); // full → 63
        assert_eq!(occupation_for(12288, 16384), 48); // 75% → 48
        assert_eq!(occupation_for(8192, 16384), 32); // 50% → 32
        assert_eq!(occupation_for(10, 0), 0); // guard divide-by-zero
    }

    #[test]
    #[should_panic(expected = "occupation exceeds u6")]
    fn new_rejects_oversize_occupation() {
        let _ = PageDescriptor::new(0, 1, 64);
    }

    #[test]
    #[should_panic(expected = "run exceeds 255 blocks")]
    fn from_run_rejects_oversize_run() {
        let _ = PageDescriptor::from_run(BlockRun { start: 0, len: 256 }, 0);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn pack_unpack_roundtrips(
            start in 0u64..=MAX_START_BLOCK,
            count in 0u8..=u8::MAX,
            occ in 0u8..=MAX_OCCUPATION,
        ) {
            let d = PageDescriptor::new(start, count, occ);
            prop_assert_eq!(d.start_block(), start);
            prop_assert_eq!(d.block_count(), count);
            prop_assert_eq!(d.occupation(), occ);
            prop_assert!(!d.is_in_journal());
            prop_assert!(!d.is_in_memory());
            prop_assert_eq!(PageDescriptor::from_raw(d.raw()), d);
        }

        #[test]
        fn with_run_then_with_occupation_are_orthogonal(
            s0 in 0u64..=MAX_START_BLOCK, c0 in 0u8..=u8::MAX, o0 in 0u8..=MAX_OCCUPATION,
            s1 in 0u64..=MAX_START_BLOCK, c1 in 0u8..=u8::MAX, o1 in 0u8..=MAX_OCCUPATION,
        ) {
            let d = PageDescriptor::new(s0, c0, o0)
                .with_run(BlockRun { start: s1, len: u64::from(c1) })
                .with_occupation(o1);
            prop_assert_eq!(d.start_block(), s1);
            prop_assert_eq!(d.block_count(), c1);
            prop_assert_eq!(d.occupation(), o1);
        }
    }
}
