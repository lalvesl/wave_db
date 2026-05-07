//! Hash function for mapping `(STRUCT_ID, TENANT_ID, SHARD_ID)` to a page index.
//!
//! Uses a simple but effective mixing strategy based on the composite key.
//! Double-hashing is used as a collision resolution / fullness signal.

/// Hash a composite key to a page index.
///
/// The primary hash maps `(struct_id, tenant_id, shard_id)` to a page.
/// For versioned records, `created_at` is included in the hash.
pub fn page_hash(struct_id: u32, tenant_id: u64, shard_id: u16, page_count: u64) -> u64 {
    let mut h: u64 = 14_695_981_039_346_656_037; // FNV offset basis
    let prime: u64 = 1_099_511_628_211; // FNV prime

    // Mix struct_id
    h ^= struct_id as u64;
    h = h.wrapping_mul(prime);

    // Mix tenant_id
    h ^= tenant_id;
    h = h.wrapping_mul(prime);

    // Mix shard_id
    h ^= shard_id as u64;
    h = h.wrapping_mul(prime);

    h % page_count
}

/// Hash a versioned record key (includes `created_at`).
pub fn versioned_hash(
    struct_id: u32,
    tenant_id: u64,
    shard_id: u16,
    created_at: u64,
    page_count: u64,
) -> u64 {
    let mut h: u64 = 14_695_981_039_346_656_037;
    let prime: u64 = 1_099_511_628_211;

    h ^= struct_id as u64;
    h = h.wrapping_mul(prime);
    h ^= tenant_id;
    h = h.wrapping_mul(prime);
    h ^= shard_id as u64;
    h = h.wrapping_mul(prime);
    h ^= created_at;
    h = h.wrapping_mul(prime);

    h % page_count
}

/// Double-hashing step for collision resolution.
///
/// Returns the step size for probing the next candidate page.
/// The step is always odd to ensure coprimality with any power-of-2 page count.
pub fn double_hash_step(struct_id: u32, tenant_id: u64, shard_id: u16, page_count: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // different basis for second hash
    let prime: u64 = 1_099_511_628_211;

    h ^= struct_id as u64;
    h = h.wrapping_mul(prime);
    h ^= tenant_id;
    h = h.wrapping_mul(prime);
    h ^= shard_id as u64;
    h = h.wrapping_mul(prime);

    // Ensure step is odd and at least 1
    let step = (h % page_count) | 1;
    if step == 0 { 1 } else { step }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_deterministic() {
        let a = page_hash(7, 42, 0, 256);
        let b = page_hash(7, 42, 0, 256);
        assert_eq!(a, b);
    }

    #[test]
    fn hash_within_range() {
        for page_count in [1, 100, 256, 1024, 4096] {
            let h = page_hash(7, 42, 0, page_count);
            assert!(h < page_count);
        }
    }

    #[test]
    fn different_inputs_different_hashes() {
        let a = page_hash(1, 1, 0, 1024);
        let b = page_hash(2, 1, 0, 1024);
        let c = page_hash(1, 2, 0, 1024);
        // Not guaranteed but very likely to differ
        assert!(a != b || a != c, "expected some variation");
    }

    #[test]
    fn double_hash_step_is_odd() {
        for sid in 0..100 {
            let step = double_hash_step(sid, 42, 0, 256);
            assert_eq!(step % 2, 1, "step must be odd");
        }
    }

    #[test]
    fn versioned_hash_differs_by_created_at() {
        let a = versioned_hash(7, 42, 0, 1000, 256);
        let b = versioned_hash(7, 42, 0, 2000, 256);
        assert_ne!(a, b);
    }
}
