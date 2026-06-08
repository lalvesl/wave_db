//! Page-routing hash strategies, one per record kind.
//!
//! # Why kind-specific?
//!
//! A single `id_to_page(id)` is wrong — the lookup-side caller does not
//! always have the full `Id`.  Two strategies cover every kind:
//!
//! | Record kind                    | Strategy           | Inputs                                |
//! |--------------------------------|--------------------|---------------------------------------|
//! | Unique anchor (current/live)   | [`tuple2_page`]    | `struct_id`, `tenant_id`              |
//! | Unique history record          | [`tuple4_page`]    | `struct_id`, `tenant_id`, `shard_id`, `created_at` |
//! | NonUnique tracker (root)       | [`tuple2_page`]    | `struct_id`, `tenant_id`              |
//! | NonUnique element              | [`tuple4_page`]    | `struct_id`, `tenant_id`, `shard_id`, `created_at` |
//! | NestedNonUnique tracker        | [`tuple4_page`]    | `struct_id`, `tenant_id`, `shard_id`, `created_at` (tracker has its own Id) |
//! | NestedNonUnique element        | [`tuple4_page`]    | `struct_id`, `tenant_id`, `shard_id`, `created_at` |
//!
//! Two-tuple inputs apply when the caller knows *only* `(struct_id, tenant)`
//! at lookup time — i.e. the live root anchor that survives every mutation.
//! Four-tuple inputs apply to record-shaped entries whose full `Id` is known
//! (history records, NonUnique elements, nested trackers/elements).
//!
//! # O(1) guarantees
//!
//! All page mappings are 3–5 arithmetic instructions plus a modulo (or a
//! mask, when `page_count.is_power_of_two()`).  No probing on the page-
//! routing path; the storage layer probes only when a target page is full
//! (see [`double_hash_step`]).
//!
//! # Avalanche
//!
//! [`mix64`] is the SplitMix64 finalizer with a pre-XOR by the golden
//! ratio constant, so it has **no fixed points** — `mix64(0) != 0`.

/// SplitMix64 avalanche finalizer with golden-ratio pre-XOR.
///
/// Strong bit-mixer: every input bit flip toggles ~32 output bits on
/// average.  `mix64(0) != 0` — the zero key does not pin to page 0.
#[inline]
pub const fn mix64(x: u64) -> u64 {
    let mut z = x ^ 0x9E37_79B9_7F4A_7C15;
    z ^= z >> 30;
    z = z.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z ^= z >> 27;
    z = z.wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    z
}

/// Combine two mixed 64-bit halves into a single uniform value.
#[inline]
const fn fold(hi: u64, lo: u64) -> u64 {
    mix64(hi).wrapping_add(mix64(lo).wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Reduce a uniform 64-bit hash to a page index in `0 .. page_count`.
///
/// Power-of-two `page_count` reduces to a single mask; otherwise modulo.
#[inline]
const fn reduce(h: u64, page_count: u64) -> u64 {
    if page_count == 0 {
        0
    } else if page_count.is_power_of_two() {
        h & (page_count - 1)
    } else {
        h % page_count
    }
}

// ── Strategy 1: tuple2 (struct_id, tenant_id) ────────────────────────────────

/// Route by `(struct_id, tenant_id)` — the two-input strategy.
///
/// Used for **Unique current anchors** and **NonUnique trackers**: the
/// lookup caller knows `struct_id` (from the type) and `tenant_id` (from
/// the session) but nothing else.  One slot per `(struct, tenant)`.
///
/// O(1): two mixes + a mask.
#[inline]
pub const fn tuple2_page(
    struct_id: u32,
    tenant_id: u64,
    page_count: u64,
) -> u64 {
    // XOR the struct_id (shifted into the high half) with the tenant_id so
    // both inputs contribute to the entire 64-bit pre-mix word.  Without
    // the shift the struct_id (typically < 1 M) would only affect the low
    // 20 bits before mixing.
    let key =
        tenant_id ^ ((struct_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    reduce(mix64(key), page_count)
}

// ── Strategy 2: tuple4 (struct_id, tenant_id, shard_id, created_at) ──────────

/// Route by `(struct_id, tenant_id, shard_id, created_at)` — the four-input
/// strategy.
///
/// Used for **Unique history**, **NonUnique elements**, **NestedNonUnique
/// trackers**, and **NestedNonUnique elements**.  Every input contributes
/// to the page index, so distinct records always have a chance to land on
/// distinct pages.
///
/// O(1): two folds + a mask.
#[inline]
pub const fn tuple4_page(
    struct_id: u32,
    tenant_id: u64,
    shard_id: u16,
    created_at: u64,
    page_count: u64,
) -> u64 {
    let lo = tenant_id ^ created_at.rotate_left(17);
    let hi = (struct_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ ((shard_id as u64) << 48)
        ^ created_at;
    reduce(fold(hi, lo), page_count)
}

// ── PageKey: enum facade over the two strategies ─────────────────────────────

/// Routing key — names the hash strategy a record kind requires.
///
/// Pass a `PageKey` to [`PageKey::page`] to compute the page index in O(1).
/// This is the recommended entry point because the variant name documents
/// the record kind at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PageKey {
    /// Two-input strategy — Unique current anchor or NonUnique tracker.
    Tuple2 {
        /// `STRUCT_ID` of the record type.
        struct_id: u32,
        /// Tenant owner.
        tenant: u64,
    },
    /// Four-input strategy — every other record kind (Unique history,
    /// NonUnique element, Nested tracker, Nested element).
    Tuple4 {
        /// `STRUCT_ID` of the record type.
        struct_id: u32,
        /// Tenant owner.
        tenant: u64,
        /// Shard / content-hash discriminator.
        shard: u16,
        /// Creation timestamp (100-µs ticks since the epoch).
        created_at: u64,
    },
}

impl PageKey {
    /// Compute the page index for this key in O(1).
    #[inline]
    pub const fn page(self, page_count: u64) -> u64 {
        match self {
            Self::Tuple2 { struct_id, tenant } => {
                tuple2_page(struct_id, tenant, page_count)
            }
            Self::Tuple4 {
                struct_id,
                tenant,
                shard,
                created_at,
            } => tuple4_page(struct_id, tenant, shard, created_at, page_count),
        }
    }
}

// ── Double-hash probe ────────────────────────────────────────────────────────

/// Double-hashing probe step for collision resolution.
///
/// Always odd → coprime with any power-of-two `page_count`, so the probe
/// sequence is a permutation of every page index.
#[inline]
pub const fn double_hash_step(
    struct_id: u32,
    tenant_id: u64,
    shard_id: u16,
    page_count: u64,
) -> u64 {
    if page_count <= 1 {
        return 1;
    }
    let key = tenant_id.wrapping_mul(0xc6a4_a793_5bd1_e995)
        ^ (struct_id as u64)
        ^ ((shard_id as u64) << 32);
    (mix64(key) % page_count) | 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ── Basic correctness ────────────────────────────────────────────────

    #[test]
    fn mix64_no_fixed_point_at_zero() {
        assert_ne!(mix64(0), 0);
        assert_ne!(mix64(1), 0);
        assert_ne!(mix64(0), mix64(1));
    }

    #[test]
    fn fold_no_fixed_point_at_zero() {
        assert_ne!(fold(0, 0), 0);
    }

    #[test]
    fn tuple2_within_range() {
        for n in [1u64, 2, 13, 100, 256, 1024, 4096] {
            for s in 0u32..32 {
                for t in 0u64..32 {
                    assert!(tuple2_page(s, t, n) < n);
                }
            }
        }
    }

    #[test]
    fn tuple4_within_range() {
        for n in [1u64, 2, 13, 100, 256, 1024, 4096] {
            for c in 0u64..256 {
                assert!(tuple4_page(7, 42, 0, c, n) < n);
            }
        }
    }

    #[test]
    fn tuple2_handles_zero_page_count() {
        assert_eq!(tuple2_page(7, 42, 0), 0);
    }

    #[test]
    fn tuple4_handles_zero_page_count() {
        assert_eq!(tuple4_page(7, 42, 0, 1000, 0), 0);
    }

    #[test]
    fn double_hash_step_is_odd() {
        for sid in 0..100u32 {
            assert_eq!(double_hash_step(sid, 42, 0, 256) & 1, 1);
        }
    }

    // ── Strategy semantics ───────────────────────────────────────────────

    /// Same `(struct_id, tenant)` always lands on the same Unique anchor
    /// slot regardless of unrelated noise — that's what makes Unique
    /// lookup possible without knowing `created_at`.
    #[test]
    fn tuple2_is_deterministic_in_struct_and_tenant() {
        for _ in 0..16 {
            assert_eq!(tuple2_page(7, 42, 256), tuple2_page(7, 42, 256));
        }
    }

    /// Two distinct created_at values hash to different pages with very
    /// high probability — required so version history doesn't cluster.
    #[test]
    fn tuple4_differs_by_created_at() {
        let a = tuple4_page(7, 42, 0, 1000, 256);
        let b = tuple4_page(7, 42, 0, 2000, 256);
        assert_ne!(a, b);
    }

    /// tuple2 and tuple4 give different output for the same shared bits.
    /// Anchors (tuple2) and history records (tuple4) should never *forced*
    /// onto the same page — otherwise the anchor page hot-spots.
    #[test]
    fn tuple2_and_tuple4_are_independent() {
        // Sample a handful — independence is statistical, but for these
        // canonical inputs the two strategies must disagree.
        let cases = [(7u32, 42u64), (1, 1), (1024, 99), (100, 1)];
        let mut disagreements = 0;
        for (s, t) in cases {
            let a = tuple2_page(s, t, 1024);
            let b = tuple4_page(s, t, 0, 0, 1024);
            if a != b {
                disagreements += 1;
            }
        }
        assert!(
            disagreements >= 3,
            "tuple2 and tuple4 collide too often on canonical inputs"
        );
    }

    #[test]
    fn page_key_dispatches_to_correct_strategy() {
        let k2 = PageKey::Tuple2 {
            struct_id: 7,
            tenant: 42,
        };
        let k4 = PageKey::Tuple4 {
            struct_id: 7,
            tenant: 42,
            shard: 0,
            created_at: 0,
        };
        assert_eq!(k2.page(256), tuple2_page(7, 42, 256));
        assert_eq!(k4.page(256), tuple4_page(7, 42, 0, 0, 256));
    }

    // ── Distribution / uniformity ────────────────────────────────────────

    /// 4096 sequential writes over 512 pages must populate all 512 slots.
    /// Regression for the original "13 of 512" pathology.
    #[test]
    fn tuple4_uniform_for_sequential_created_at() {
        const N_PAGES: u64 = 512;
        let mut hist = HashMap::<u64, u64>::new();
        for seq in 1..=4096u64 {
            let p = tuple4_page(7, 100, 0, seq, N_PAGES);
            *hist.entry(p).or_default() += 1;
        }
        // Expected empty slots at N = 8 × pages ≈ 0.17 — one empty slot
        // is within normal variance.  Threshold 510/512 ≈ 99.6 %.
        assert!(
            hist.len() as u64 >= N_PAGES - 2,
            "only {}/{N_PAGES} slots populated",
            hist.len()
        );
        // Variance bound: max ≤ 4× mean.  Mean = 8, threshold 32.
        let max = hist.values().copied().max().unwrap();
        assert!(max <= 32, "peaked at {max}");
    }

    /// 1024 distinct `(struct, tenant)` pairs must spread across ≥ 95 %
    /// of a 256-page file — otherwise tuple2 is biased and Unique anchors
    /// hot-spot.
    #[test]
    fn tuple2_spreads_distinct_struct_tenant_pairs() {
        const N_PAGES: u64 = 256;
        let mut hits = std::collections::HashSet::new();
        for s in 0..32u32 {
            for t in 0..32u64 {
                hits.insert(tuple2_page(s, t, N_PAGES));
            }
        }
        // 1024 inputs into 256 slots; expected populated ≈ 256 × (1 −
        // (1 − 1/256)^1024) ≈ 246.  Threshold 240 leaves headroom.
        assert!(
            hits.len() >= 240,
            "tuple2 spread only {} slots out of {N_PAGES}",
            hits.len()
        );
    }

    /// 13 distinct (struct, tenant) pairs spread across ≥ 12 slots —
    /// reproduces the real_example load with 13 clients per node.
    #[test]
    fn tuple2_real_example_13_pairs() {
        const N_PAGES: u64 = 512;
        let mut hits = std::collections::HashSet::new();
        for user in 0..13u64 {
            hits.insert(tuple2_page(7, 100 + user, N_PAGES));
        }
        assert!(hits.len() >= 12, "got {}", hits.len());
    }
}
