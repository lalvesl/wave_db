//! Roundtrip tests for Metadata wire serialization.

use wavedb_core::{Metadata, PermissionRef, Wire, wire};

#[test]
fn metadata_default_roundtrip() {
    let m = Metadata::default();
    let bytes = wire::to_wire(&m).unwrap();
    let decoded: Metadata = wire::from_wire(&bytes).unwrap();
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
    let bytes = wire::to_wire(&m).unwrap();
    let decoded: Metadata = wire::from_wire(&bytes).unwrap();
    assert_eq!(m, decoded);
}

#[test]
fn none_permission_stays_on_stack() {
    let perm: Option<PermissionRef> = None;
    let bytes = wire::to_wire(&perm).unwrap();
    assert_eq!(
        bytes.len(),
        <Option<PermissionRef> as Wire>::STACK_SIZE,
        "None permission occupies its fixed stack slot and no heap bytes"
    );
}
