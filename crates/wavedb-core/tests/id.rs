//! Property tests for the 128-bit composite `Id`.

use proptest::prelude::*;
use wavedb_core::Id;

proptest! {
    #[test]
    fn id_roundtrip(
        t in 0u64..((1u64 << 48)),
        s in 0u16..((1u16 << 12)),
        sid in 0u32..((1u32 << 20)),
        c in 0u64..((1u64 << 48))
    ) {
        let id = Id::new(t, s, sid, c);
        prop_assert_eq!(id.tenant_id(), t);
        prop_assert_eq!(id.shard_id(), s);
        prop_assert_eq!(id.struct_id(), sid);
        prop_assert_eq!(id.created_at(), c);
    }

    #[test]
    fn id_truncates_overflow(
        t in any::<u64>(),
        s in any::<u16>(),
        sid in any::<u32>(),
        c in any::<u64>()
    ) {
        let id = Id::new(t, s, sid, c);
        prop_assert!(id.tenant_id()  < (1 << 48));
        prop_assert!(id.shard_id()   < (1 << 12));
        prop_assert!(id.struct_id()  < (1 << 20));
        prop_assert!(id.created_at() < (1 << 48));
    }

    #[test]
    fn id_postcard_roundtrip(
        t in 0u64..((1u64 << 48)),
        s in 0u16..((1u16 << 12)),
        sid in 0u32..((1u32 << 20)),
        c in 0u64..((1u64 << 48))
    ) {
        let id = Id::new(t, s, sid, c);
        let bytes = postcard::to_allocvec(&id).unwrap();
        let decoded: Id = postcard::from_bytes(&bytes).unwrap();
        prop_assert_eq!(id, decoded);
    }

    #[test]
    fn anchor_key_preserves_fields_except_created_at(
        t in 0u64..((1u64 << 48)),
        s in 0u16..((1u16 << 12)),
        sid in 0u32..((1u32 << 20)),
        c in 0u64..((1u64 << 48))
    ) {
        let id = Id::new(t, s, sid, c);
        let anchor = id.anchor_key();
        prop_assert_eq!(anchor.tenant_id(), t);
        prop_assert_eq!(anchor.shard_id(), s);
        prop_assert_eq!(anchor.struct_id(), sid);
        prop_assert_eq!(anchor.created_at(), 0);
    }
}
