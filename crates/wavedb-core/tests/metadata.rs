//! Property tests for Metadata serialization.

use wavedb_core::{Metadata, PermissionRef};

#[test]
fn metadata_default_roundtrip() {
    let m = Metadata::default();
    let bytes = postcard::to_allocvec(&m).unwrap();
    let decoded: Metadata = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(m, decoded);
}

#[test]
fn metadata_with_permission_roundtrip() {
    let m = Metadata {
        old_modification_id: 100,
        new_modification_id: 200,
        struct_version: 42,
        user: 999,
        device_created: 5555,
        permission: Some(PermissionRef::Inline(vec![1, 2, 3])),
    };
    let bytes = postcard::to_allocvec(&m).unwrap();
    let decoded: Metadata = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(m, decoded);
}

#[test]
fn none_permission_serialises_to_one_byte() {
    let perm: Option<PermissionRef> = None;
    let bytes = postcard::to_allocvec(&perm).unwrap();
    assert_eq!(
        bytes.len(),
        1,
        "None permission must serialize to exactly 1 byte under postcard"
    );
}
