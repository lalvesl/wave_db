//! Block allocator for the `data` file.
//!
//! The `data` file is an **array of fixed 4 KiB blocks** (see the _Block
//! Allocator_ section of the project readme).  A page lives in a run of
//! `block_count` contiguous blocks; this module owns that block space — it
//! hands out runs, takes them back, coalesces neighbours, and truncates the
//! file tail when the trailing blocks are all free.
//!
//! # What this module is (and isn't)
//!
//! [`BlockAllocator`] is a **pure in-memory data structure**.  It tracks
//! which blocks are free and which are in use; it does **not** touch the
//! filesystem and it does **not** journal.  Durability is the caller's job:
//! the data-file layer journals each [`BlockAllocator::allocate`] /
//! [`BlockAllocator::free`] as a free-space delta and replays those deltas
//! over the last snapshot on startup (the journal is the single source of
//! truth for "what space can be reclaimed where").  Keeping the allocator
//! pure makes it trivial to unit-test and lets the journal own crash safety.
//!
//! # Free-space model
//!
//! Free blocks are tracked as **extents** indexed two ways:
//!
//! - `by_start: start_block → len` — ordered by position, so a [`free`] can
//!   find and merge its left/right neighbours in `O(log n)`.
//! - `by_len: len → {start_block, …}` — ordered by size, so an [`allocate`]
//!   can best-fit the smallest extent that satisfies the request in
//!   `O(log n)`.
//!
//! Both maps describe the same set of **maximal, non-overlapping** extents;
//! every mutation goes through [`BlockAllocator::insert_free`] /
//! [`BlockAllocator::remove_free`] so they never drift apart.
//!
//! [`free`]: BlockAllocator::free
//! [`allocate`]: BlockAllocator::allocate

use std::collections::{BTreeMap, BTreeSet};

/// Size of one block in the `data` file, in bytes.
///
/// Pages are runs of these; `block_count` (a `u8` in the page descriptor)
/// caps a single page at `255 * DATA_BLOCK_SIZE ≈ 1 MiB`.
pub const DATA_BLOCK_SIZE: u64 = 4096;

/// A contiguous run of blocks: `len` blocks starting at block index `start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockRun {
    /// Index of the first block in the run.
    pub start: u64,
    /// Number of blocks in the run.
    pub len: u64,
}

impl BlockRun {
    /// One past the last block in the run (`start + len`).
    #[inline]
    pub const fn end(self) -> u64 {
        self.start + self.len
    }

    /// Byte offset of the run's first block in the `data` file.
    #[inline]
    pub const fn byte_offset(self) -> u64 {
        self.start * DATA_BLOCK_SIZE
    }

    /// Total byte length of the run.
    #[inline]
    pub const fn byte_len(self) -> u64 {
        self.len * DATA_BLOCK_SIZE
    }
}

/// Number of whole blocks needed to hold `bytes` bytes (rounding up).
#[inline]
pub const fn blocks_for_bytes(bytes: u64) -> u64 {
    bytes.div_ceil(DATA_BLOCK_SIZE)
}

/// Manager of allocation and deallocation for the `data` file's block space.
///
/// See the module docs for the free-space model and the durability contract.
pub struct BlockAllocator {
    /// High-water mark: number of blocks the file currently spans.  Blocks
    /// `[0, capacity)` are either allocated or in a free extent.
    capacity: u64,
    /// Free extents keyed by start block → length (blocks).
    by_start: BTreeMap<u64, u64>,
    /// Free extents keyed by length → the set of start blocks of that
    /// length.  The best-fit index for [`allocate`](Self::allocate).
    by_len: BTreeMap<u64, BTreeSet<u64>>,
}

impl Default for BlockAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockAllocator {
    /// Create an empty allocator over a zero-length file.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            capacity: 0,
            by_start: BTreeMap::new(),
            by_len: BTreeMap::new(),
        }
    }

    /// Rebuild an allocator from a recovered `capacity` and a set of free
    /// runs — the shape produced by replaying journal free-space deltas.
    ///
    /// The runs are coalesced defensively, so callers may pass adjacent or
    /// unordered extents.
    #[must_use]
    pub fn from_state(
        capacity: u64,
        free: impl IntoIterator<Item = BlockRun>,
    ) -> Self {
        let mut alloc = Self {
            capacity,
            by_start: BTreeMap::new(),
            by_len: BTreeMap::new(),
        };
        for run in free {
            alloc.free(run.start, run.len);
        }
        alloc
    }

    // ── Queries ──────────────────────────────────────────────────────────

    /// Number of blocks the file currently spans (the high-water mark).
    #[inline]
    #[must_use]
    pub const fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Total free blocks across every extent.
    #[must_use]
    pub fn free_blocks(&self) -> u64 {
        self.by_start.values().sum()
    }

    /// Blocks currently handed out (`capacity - free`).
    #[must_use]
    pub fn used_blocks(&self) -> u64 {
        self.capacity - self.free_blocks()
    }

    /// Number of distinct free extents (a fragmentation gauge).
    #[must_use]
    pub fn free_extent_count(&self) -> usize {
        self.by_start.len()
    }

    /// Iterate the free extents in ascending block-position order.
    pub fn free_runs(&self) -> impl Iterator<Item = BlockRun> + '_ {
        self.by_start
            .iter()
            .map(|(&start, &len)| BlockRun { start, len })
    }

    // ── Allocation ───────────────────────────────────────────────────────

    /// Allocate `blocks` contiguous blocks, returning their [`BlockRun`].
    ///
    /// Best-fit: the smallest free extent that fits is used and any leftover
    /// is returned to the pool.  When no extent fits, the file grows at the
    /// tail — reusing a tail-adjacent free extent first so a partial tail
    /// hole is never stranded.
    ///
    /// # Panics
    ///
    /// Panics if `blocks == 0` — a zero-length allocation is a caller bug.
    pub fn allocate(&mut self, blocks: u64) -> BlockRun {
        assert!(blocks > 0, "cannot allocate zero blocks");

        // Best-fit: smallest free extent with len >= blocks.
        if let Some((&len, starts)) = self.by_len.range(blocks..).next() {
            let start = *starts
                .iter()
                .next()
                .expect("by_len never holds an empty start set");
            self.remove_free(start, len);
            if len > blocks {
                self.insert_free(start + blocks, len - blocks);
            }
            return BlockRun { start, len: blocks };
        }

        // No free extent fits.  If the trailing free extent abuts the tail,
        // extend it instead of stranding it; otherwise grow at the very end.
        if let Some((start, len)) = self.tail_free() {
            self.remove_free(start, len);
            self.capacity = start + blocks; // start + len == old capacity
            return BlockRun { start, len: blocks };
        }

        let start = self.capacity;
        self.capacity += blocks;
        BlockRun { start, len: blocks }
    }

    // ── Deallocation ─────────────────────────────────────────────────────

    /// Return a run of `blocks` blocks starting at `start` to the free pool,
    /// coalescing with adjacent free extents on both sides.
    ///
    /// Freeing zero blocks is a no-op.  Double-freeing or freeing past the
    /// current capacity trips a debug assertion.
    pub fn free(&mut self, start: u64, blocks: u64) {
        if blocks == 0 {
            return;
        }
        let end = start + blocks;
        debug_assert!(end <= self.capacity, "free past capacity");
        debug_assert!(
            self.by_start
                .range(start..)
                .next()
                .is_none_or(|(&s, _)| s >= end),
            "double free / overlapping free at block {start}"
        );

        let mut run_start = start;
        let mut run_len = blocks;

        // Merge the left neighbour if it ends exactly where we begin.
        if let Some((&ls, &ll)) = self.by_start.range(..start).next_back() {
            debug_assert!(ls + ll <= start, "overlapping free (left side)");
            if ls + ll == start {
                self.remove_free(ls, ll);
                run_start = ls;
                run_len += ll;
            }
        }

        // Merge the right neighbour if one starts exactly where we end.
        if let Some(&rl) = self.by_start.get(&end) {
            self.remove_free(end, rl);
            run_len += rl;
        }

        self.insert_free(run_start, run_len);
    }

    /// Drop a free extent sitting at the file tail and lower `capacity` to
    /// its start, shrinking the file.  Returns the number of blocks
    /// reclaimed (`0` if the tail block is in use).
    ///
    /// Extents are kept maximal, so at most one can abut the tail — one call
    /// reclaims all trailing free space.
    pub fn truncate(&mut self) -> u64 {
        if let Some((start, len)) = self.tail_free() {
            self.remove_free(start, len);
            self.capacity = start;
            return len;
        }
        0
    }

    // ── Internals ────────────────────────────────────────────────────────

    /// The free extent that abuts the file tail, if any.
    fn tail_free(&self) -> Option<(u64, u64)> {
        self.by_start.iter().next_back().and_then(|(&start, &len)| {
            (start + len == self.capacity).then_some((start, len))
        })
    }

    /// Record a free extent in both indexes.
    fn insert_free(&mut self, start: u64, len: u64) {
        debug_assert!(len > 0, "zero-length free extent");
        let prev = self.by_start.insert(start, len);
        debug_assert!(prev.is_none(), "free extent already present at {start}");
        self.by_len.entry(len).or_default().insert(start);
    }

    /// Drop a free extent from both indexes.
    fn remove_free(&mut self, start: u64, len: u64) {
        self.by_start.remove(&start);
        if let Some(set) = self.by_len.get_mut(&len) {
            set.remove(&start);
            if set.is_empty() {
                self.by_len.remove(&len);
            }
        }
    }

    /// Assert the two indexes describe the same extents — test-only.
    #[cfg(test)]
    fn check_invariants(&self) {
        // by_len is the transpose of by_start.
        let mut from_len: BTreeMap<u64, u64> = BTreeMap::new();
        for (&len, starts) in &self.by_len {
            for &start in starts {
                assert!(
                    from_len.insert(start, len).is_none(),
                    "duplicate start {start} in by_len"
                );
            }
        }
        assert_eq!(from_len, self.by_start, "by_start / by_len disagree");

        // Extents are within capacity, non-overlapping, and maximal
        // (no two are adjacent — adjacency would mean a missed coalesce).
        let mut prev_end: Option<u64> = None;
        for (&start, &len) in &self.by_start {
            assert!(len > 0, "zero-length extent at {start}");
            assert!(start + len <= self.capacity, "extent past capacity");
            if let Some(pe) = prev_end {
                assert!(start > pe, "adjacent/overlapping extents not merged");
            }
            prev_end = Some(start + len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_run_geometry() {
        let run = BlockRun { start: 3, len: 4 };
        assert_eq!(run.end(), 7);
        assert_eq!(run.byte_offset(), 3 * 4096);
        assert_eq!(run.byte_len(), 4 * 4096);
    }

    #[test]
    fn blocks_for_bytes_rounds_up() {
        assert_eq!(blocks_for_bytes(0), 0);
        assert_eq!(blocks_for_bytes(1), 1);
        assert_eq!(blocks_for_bytes(4096), 1);
        assert_eq!(blocks_for_bytes(4097), 2);
        assert_eq!(blocks_for_bytes(16 * 1024), 4);
    }

    #[test]
    fn empty_allocator_grows_at_tail() {
        let mut a = BlockAllocator::new();
        assert_eq!(a.capacity(), 0);
        assert_eq!(a.allocate(4), BlockRun { start: 0, len: 4 });
        assert_eq!(a.allocate(2), BlockRun { start: 4, len: 2 });
        assert_eq!(a.capacity(), 6);
        assert_eq!(a.free_blocks(), 0);
        assert_eq!(a.used_blocks(), 6);
        a.check_invariants();
    }

    #[test]
    fn free_returns_run_to_pool_and_reuses_it() {
        let mut a = BlockAllocator::new();
        let r0 = a.allocate(4); // [0,4)
        let _r1 = a.allocate(4); // [4,8)
        a.free(r0.start, r0.len);
        assert_eq!(a.free_blocks(), 4);
        // Best-fit reuses the freed hole, not the tail.
        let r2 = a.allocate(4);
        assert_eq!(r2, BlockRun { start: 0, len: 4 });
        assert_eq!(a.capacity(), 8);
        a.check_invariants();
    }

    #[test]
    fn best_fit_picks_smallest_sufficient_extent() {
        let mut a = BlockAllocator::new();
        let big = a.allocate(8); // [0,8)
        let _mid = a.allocate(2); // [8,10)
        let small = a.allocate(4); // [10,14)
        a.free(big.start, big.len); // free len-8 hole at 0
        a.free(small.start, small.len); // free len-4 hole at 10
        // Requesting 3 must take the len-4 hole (best fit), not the len-8.
        let r = a.allocate(3);
        assert_eq!(r.start, 10);
        // Leftover 1 block returned to pool.
        assert_eq!(a.free_blocks(), 8 + 1);
        a.check_invariants();
    }

    #[test]
    fn allocate_splits_remainder_back_to_free() {
        let mut a = BlockAllocator::new();
        let r = a.allocate(10);
        a.free(r.start, r.len);
        let part = a.allocate(3);
        assert_eq!(part, BlockRun { start: 0, len: 3 });
        // Remainder [3,10) is one 7-block extent.
        assert_eq!(a.free_extent_count(), 1);
        assert_eq!(a.free_blocks(), 7);
        assert_eq!(
            a.free_runs().next(),
            Some(BlockRun { start: 3, len: 7 })
        );
        a.check_invariants();
    }

    #[test]
    fn free_coalesces_left_neighbour() {
        let mut a = BlockAllocator::new();
        let r0 = a.allocate(2); // [0,2)
        let r1 = a.allocate(2); // [2,4)
        let _tail = a.allocate(2); // [4,6) keeps the holes off the tail
        a.free(r0.start, r0.len);
        a.free(r1.start, r1.len);
        // [0,2)+[2,4) coalesce into one [0,4) extent.
        assert_eq!(a.free_extent_count(), 1);
        assert_eq!(a.free_runs().next(), Some(BlockRun { start: 0, len: 4 }));
        a.check_invariants();
    }

    #[test]
    fn free_coalesces_both_neighbours() {
        let mut a = BlockAllocator::new();
        let r0 = a.allocate(2); // [0,2)
        let r1 = a.allocate(2); // [2,4)
        let r2 = a.allocate(2); // [4,6)
        let _tail = a.allocate(1); // [6,7)
        a.free(r0.start, r0.len); // [0,2)
        a.free(r2.start, r2.len); // [4,6)
        a.free(r1.start, r1.len); // [2,4) bridges the two
        assert_eq!(a.free_extent_count(), 1);
        assert_eq!(a.free_runs().next(), Some(BlockRun { start: 0, len: 6 }));
        a.check_invariants();
    }

    #[test]
    fn truncate_reclaims_tail_free_space() {
        let mut a = BlockAllocator::new();
        let _r0 = a.allocate(4); // [0,4)
        let r1 = a.allocate(4); // [4,8)
        a.free(r1.start, r1.len); // tail hole [4,8)
        assert_eq!(a.capacity(), 8);
        let reclaimed = a.truncate();
        assert_eq!(reclaimed, 4);
        assert_eq!(a.capacity(), 4);
        assert_eq!(a.free_blocks(), 0);
        a.check_invariants();
    }

    #[test]
    fn truncate_noop_when_tail_in_use() {
        let mut a = BlockAllocator::new();
        let r0 = a.allocate(4); // [0,4)
        let _r1 = a.allocate(4); // [4,8) — tail in use
        a.free(r0.start, r0.len); // interior hole, not at tail
        assert_eq!(a.truncate(), 0);
        assert_eq!(a.capacity(), 8);
        a.check_invariants();
    }

    #[test]
    fn allocate_reuses_partial_tail_hole_before_growing() {
        let mut a = BlockAllocator::new();
        let _r0 = a.allocate(2); // [0,2)
        let r1 = a.allocate(2); // [2,4)
        a.free(r1.start, r1.len); // tail hole [2,4), len 2
        // Ask for 5 (> tail hole): reuse the hole and grow over it, so the
        // run starts at 2 and capacity becomes 7 — no stranded gap.
        let r = a.allocate(5);
        assert_eq!(r, BlockRun { start: 2, len: 5 });
        assert_eq!(a.capacity(), 7);
        assert_eq!(a.free_blocks(), 0);
        a.check_invariants();
    }

    #[test]
    fn from_state_coalesces_recovered_extents() {
        // Two adjacent recovered runs collapse into one.
        let a = BlockAllocator::from_state(
            10,
            [BlockRun { start: 2, len: 2 }, BlockRun { start: 4, len: 3 }],
        );
        assert_eq!(a.capacity(), 10);
        assert_eq!(a.free_extent_count(), 1);
        assert_eq!(a.free_runs().next(), Some(BlockRun { start: 2, len: 5 }));
        a.check_invariants();
    }

    #[test]
    #[should_panic(expected = "cannot allocate zero blocks")]
    fn allocate_zero_panics() {
        BlockAllocator::new().allocate(0);
    }

    #[test]
    fn free_zero_is_noop() {
        let mut a = BlockAllocator::new();
        a.allocate(4);
        a.free(2, 0);
        assert_eq!(a.free_blocks(), 0);
        a.check_invariants();
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    /// Model-based check: drive random allocate/free/truncate sequences and
    /// assert the allocator never overlaps live runs, never loses or
    /// invents blocks, and keeps its two indexes consistent.
    #[derive(Debug, Clone)]
    enum Op {
        Alloc(u64),
        FreeNth(usize),
        Truncate,
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (1u64..=8).prop_map(Op::Alloc),
            (0usize..16).prop_map(Op::FreeNth),
            Just(Op::Truncate),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]
        #[test]
        fn allocator_never_double_allocates(ops in prop::collection::vec(op_strategy(), 0..200)) {
            let mut alloc = BlockAllocator::new();
            // Live runs we've handed out and not yet freed.
            let mut live: Vec<BlockRun> = Vec::new();

            for op in ops {
                match op {
                    Op::Alloc(n) => {
                        let run = alloc.allocate(n);
                        // The new run must not overlap any live run.
                        for other in &live {
                            let disjoint =
                                run.end() <= other.start || other.end() <= run.start;
                            prop_assert!(disjoint, "alloc {run:?} overlaps live {other:?}");
                        }
                        prop_assert!(run.end() <= alloc.capacity());
                        live.push(run);
                    }
                    Op::FreeNth(i) => {
                        if !live.is_empty() {
                            let run = live.swap_remove(i % live.len());
                            alloc.free(run.start, run.len);
                        }
                    }
                    Op::Truncate => {
                        let before = alloc.capacity();
                        let reclaimed = alloc.truncate();
                        prop_assert_eq!(alloc.capacity(), before - reclaimed);
                        // Truncate must never cut into a live run.
                        for r in &live {
                            prop_assert!(r.end() <= alloc.capacity());
                        }
                    }
                }
                alloc.check_invariants();

                // Conservation: every block in [0,capacity) is either inside
                // exactly one live run or inside the free set.
                let mut covered: BTreeSet<u64> = BTreeSet::new();
                for r in &live {
                    for b in r.start..r.end() {
                        prop_assert!(covered.insert(b), "block {b} double-covered");
                    }
                }
                for r in alloc.free_runs() {
                    for b in r.start..r.end() {
                        prop_assert!(covered.insert(b), "free block {b} overlaps live");
                    }
                }
                prop_assert_eq!(covered.len() as u64, alloc.capacity());
            }
        }
    }
}
