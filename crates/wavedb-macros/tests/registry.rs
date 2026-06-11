//! Integration tests for `declare_objects!` + the `DESCRIPTOR` const emitted
//! by `#[wave_db]` — the static object registry every node compiles in.

use wavedb::prelude::*;
use wavedb_core::{FieldKind, Wire};
use wavedb_macros::declare_objects;

#[wave_db(struct_id = 7001, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Order1 {
    pub amount: u64,
    pub status: String,
}

#[wave_db(struct_id = 7001, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Order2 {
    pub amount: u64,
    pub status: String,
    pub note: Option<String>,
}

#[wave_db(struct_id = 7002)]
#[derive(PartialEq, Eq)]
pub struct Profile1 {
    pub display_name: String,
    pub age: u32,
}

declare_objects! {
    pub mod registry {
        orders:   [Order1, Order2],
        profiles: [Profile1],
    }
}

const ID_SIZE: usize = <Id as Wire>::STACK_SIZE;
const META_SIZE: usize = <Metadata as Wire>::STACK_SIZE;

#[test]
fn headers_pack_struct_id_and_version() {
    assert_eq!(Order1::HEADER, (7001 << 8) | 1);
    assert_eq!(Order2::HEADER, (7001 << 8) | 2);
    assert_eq!(Profile1::HEADER, (7002 << 8) | 1);
}

#[test]
fn find_returns_the_right_descriptor() {
    let d = registry::find(Order2::HEADER).expect("declared");
    assert_eq!(d.struct_id, 7001);
    assert_eq!(d.version, 2);
    assert_eq!(d.type_name, "Order2");
    assert_eq!(d.shape, Shape::NonUnique);
    assert_eq!(d.stack_size, <Order2 as Wire>::STACK_SIZE);
    assert!(!d.fixed);

    let p = registry::find(Profile1::HEADER).expect("declared");
    assert_eq!(p.shape, Shape::Unique);
    assert_eq!(p.type_name, "Profile1");

    assert!(registry::find((9999 << 8) | 1).is_none());
    // Right struct_id, undeclared version.
    assert!(registry::find((7001 << 8) | 3).is_none());
}

#[test]
fn contains_and_helpers() {
    assert!(registry::contains(Order1::HEADER));
    assert!(!registry::contains(0));

    assert_eq!(
        registry::stack_size(Order1::HEADER),
        Some(<Order1 as Wire>::STACK_SIZE)
    );
    assert_eq!(registry::stack_size(0), None);

    assert_eq!(
        registry::heap_fields(Order1::HEADER),
        Some(&["status"][..])
    );
    assert_eq!(
        registry::heap_fields(Order2::HEADER),
        Some(&["status", "note"][..])
    );
}

#[test]
fn family_modules_expose_struct_id_and_versions() {
    assert_eq!(registry::orders::STRUCT_ID, 7001);
    assert_eq!(registry::orders::VERSIONS, &[1, 2]);
    assert_eq!(registry::orders::DESCRIPTORS.len(), 2);

    assert_eq!(registry::profiles::STRUCT_ID, 7002);
    assert_eq!(registry::profiles::VERSIONS, &[1]);

    assert_eq!(registry::DESCRIPTORS.len(), 3);
}

#[test]
fn latest_version_picks_the_max() {
    assert_eq!(registry::latest_version(7001), Some(2));
    assert_eq!(registry::latest_version(7002), Some(1));
    assert_eq!(registry::latest_version(7003), None);
}

#[test]
fn descriptor_fields_carry_exact_offsets() {
    let d = Order1::DESCRIPTOR;
    assert_eq!(d.fields.len(), 4); // id, metadata, amount, status

    let id = &d.fields[0];
    assert_eq!(id.name, "id");
    assert_eq!(id.stack_offset, 0);
    assert_eq!(id.stack_size, ID_SIZE);
    assert!(!id.heapable);
    assert_eq!(id.kind, FieldKind::Id);

    let meta = &d.fields[1];
    assert_eq!(meta.name, "metadata");
    assert_eq!(meta.stack_offset, ID_SIZE);
    assert_eq!(meta.stack_size, META_SIZE);
    // Metadata carries an Option<PermissionRef> — not FIXED, so exact
    // heapable is true even though the syntactic heap_fields list skips it.
    assert!(meta.heapable);

    let amount = &d.fields[2];
    assert_eq!(amount.name, "amount");
    assert_eq!(amount.stack_offset, ID_SIZE + META_SIZE);
    assert_eq!(amount.stack_size, 8);
    assert!(!amount.heapable);
    assert_eq!(amount.kind, FieldKind::U64);

    let status = &d.fields[3];
    assert_eq!(status.name, "status");
    assert_eq!(status.stack_offset, ID_SIZE + META_SIZE + 8);
    assert_eq!(status.stack_size, 4); // u32 heap-length slot
    assert!(status.heapable);
    assert_eq!(status.kind, FieldKind::Str);

    assert_eq!(d.stack_size, ID_SIZE + META_SIZE + 8 + 4);

    // Descriptor lookup by name.
    assert_eq!(d.field("status").map(|f| f.stack_offset), Some(ID_SIZE + META_SIZE + 8));
    assert!(d.field("nope").is_none());
}

#[test]
fn descriptor_matches_wire_reality() {
    // Serialize a record; the heap-length slot must sit exactly at the
    // descriptor's offset for the field.
    let rec = Order1 {
        id: Id::default(),
        metadata: Metadata::default(),
        amount: 42,
        status: "shipped".to_string(),
    };
    let bytes = wavedb_core::wire::to_wire(&rec).expect("serialize");

    let d = Order1::DESCRIPTOR;
    let f = d.field("status").expect("declared field");
    let slot: [u8; 4] = bytes[f.stack_offset..f.stack_offset + 4]
        .try_into()
        .expect("4-byte slot");
    assert_eq!(u32::from_le_bytes(slot) as usize, "shipped".len());

    let back: Order1 = wavedb_core::wire::from_wire(&bytes).expect("parse");
    assert_eq!(back, rec);
}
