//! Roundtrip and layout tests for `#[derive(WaveWire)]`.
#![allow(clippy::assertions_on_constants)]

use wavedb_core::wire::{from_wire, to_wire};
use wavedb_core::{Id, Wire, WireError};
use wavedb_macros::WaveWire;

fn roundtrip<T: Wire + PartialEq + std::fmt::Debug>(value: &T) {
    let bytes = to_wire(value).unwrap();
    assert_eq!(bytes.len(), value.wire_size());
    let back: T = from_wire(&bytes).unwrap();
    assert_eq!(&back, value);
}

// ── Structs ──────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, WaveWire)]
struct AllFixed {
    a: u8,
    b: u64,
    c: bool,
    d: i32,
}

#[derive(Debug, PartialEq, WaveWire)]
struct WithHeap {
    id: u128,
    name: String,
    flags: u16,
    tags: Vec<u32>,
    note: Option<String>,
}

#[derive(Debug, PartialEq, WaveWire)]
struct Nested {
    fixed: AllFixed,
    heapy: WithHeap,
    trail: u8,
}

#[derive(Debug, PartialEq, WaveWire)]
struct TupleStruct(u32, String);

#[derive(Debug, PartialEq, WaveWire)]
struct UnitStruct;

#[test]
fn fixed_struct_is_packed() {
    assert_eq!(AllFixed::STACK_SIZE, 1 + 8 + 1 + 4);
    assert!(AllFixed::FIXED);
    let v = AllFixed { a: 1, b: u64::MAX, c: true, d: -7 };
    assert_eq!(v.heap_size(), 0);
    roundtrip(&v);
}

#[test]
fn heap_struct_layout() {
    // stack: u128(16) + String slot(4) + u16(2) + Vec slot(4) + Option(1+4)
    assert_eq!(WithHeap::STACK_SIZE, 16 + 4 + 2 + 4 + 5);
    assert!(!WithHeap::FIXED);
    let v = WithHeap {
        id: 42,
        name: "wave".into(),
        flags: 7,
        tags: vec![1, 2, 3],
        note: Some("n".into()),
    };
    assert_eq!(v.heap_size(), 4 + 12 + 1);
    roundtrip(&v);
    roundtrip(&WithHeap {
        id: 0,
        name: String::new(),
        flags: 0,
        tags: vec![],
        note: None,
    });
}

#[test]
fn nested_struct_flattens() {
    assert_eq!(
        Nested::STACK_SIZE,
        AllFixed::STACK_SIZE + WithHeap::STACK_SIZE + 1
    );
    let v = Nested {
        fixed: AllFixed { a: 9, b: 8, c: false, d: 7 },
        heapy: WithHeap {
            id: 1,
            name: "abc".into(),
            flags: 2,
            tags: vec![10, 20],
            note: None,
        },
        trail: 0xEE,
    };
    roundtrip(&v);
}

#[test]
fn tuple_and_unit_structs() {
    assert_eq!(TupleStruct::STACK_SIZE, 4 + 4);
    roundtrip(&TupleStruct(5, "five".into()));
    assert_eq!(UnitStruct::STACK_SIZE, 0);
    roundtrip(&UnitStruct);
}

// ── Enums ────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, WaveWire)]
enum Plain {
    A,
    B,
    C,
}

#[derive(Debug, PartialEq, WaveWire)]
enum Mixed {
    Empty,
    Scalar(u64),
    Pair { x: u32, y: String },
    List(Vec<u64>),
}

#[test]
fn unit_enum_is_one_byte() {
    assert_eq!(Plain::STACK_SIZE, 1);
    assert!(Plain::FIXED);
    assert_eq!(to_wire(&Plain::B).unwrap(), [1]);
    roundtrip(&Plain::A);
    roundtrip(&Plain::C);
    assert!(matches!(
        from_wire::<Plain>(&[3]),
        Err(WireError::InvalidTag { type_name: "Plain", tag: 3 })
    ));
}

#[test]
fn payload_enum_layout() {
    assert_eq!(Mixed::STACK_SIZE, 5);
    assert!(!Mixed::FIXED);

    // Empty: tag 0, payload length 0, no heap beyond the unit itself.
    assert_eq!(to_wire(&Mixed::Empty).unwrap(), [0, 0, 0, 0, 0]);

    // Scalar(7): tag 1, payload len 8, then the u64 unit.
    let bytes = to_wire(&Mixed::Scalar(7)).unwrap();
    assert_eq!(bytes[0], 1);
    assert_eq!(u32::from_le_bytes(bytes[1..5].try_into().unwrap()), 8);
    assert_eq!(bytes.len(), 5 + 8);

    roundtrip(&Mixed::Empty);
    roundtrip(&Mixed::Scalar(u64::MAX));
    roundtrip(&Mixed::Pair { x: 1, y: "pair".into() });
    roundtrip(&Mixed::List(vec![1, 2, 3, 4]));
}

#[test]
fn enum_in_struct_and_vec() {
    #[derive(Debug, PartialEq, WaveWire)]
    struct Holder {
        before: u8,
        m: Mixed,
        after: u8,
    }
    // Enum occupies a fixed 5-byte stack slot inside the struct.
    assert_eq!(Holder::STACK_SIZE, 1 + 5 + 1);
    roundtrip(&Holder {
        before: 1,
        m: Mixed::Pair { x: 9, y: "inner".into() },
        after: 2,
    });

    roundtrip(&vec![
        Mixed::Empty,
        Mixed::List(vec![5, 6]),
        Mixed::Scalar(1),
    ]);
}

// ── Mirrors of the real core types ──────────────────────────────────────────

#[derive(Debug, PartialEq, WaveWire)]
enum PermissionRefMirror {
    Inline(Vec<u64>),
    Group(u64),
}

#[derive(Debug, PartialEq, WaveWire)]
struct MetadataMirror {
    old_modification_id: u128,
    new_modification_id: u128,
    struct_version: u8,
    user: u64,
    device_created: u64,
    permission: Option<PermissionRefMirror>,
}

#[test]
fn metadata_shaped_struct_roundtrips() {
    // 16+16+1+8+8 + Option(1 + enum 5)
    assert_eq!(MetadataMirror::STACK_SIZE, 49 + 6);
    roundtrip(&MetadataMirror {
        old_modification_id: 1,
        new_modification_id: 2,
        struct_version: 42,
        user: 99,
        device_created: 1234,
        permission: Some(PermissionRefMirror::Inline(vec![10, 20])),
    });
    roundtrip(&MetadataMirror {
        old_modification_id: 0,
        new_modification_id: 0,
        struct_version: 0,
        user: 0,
        device_created: 0,
        permission: None,
    });
}

#[test]
fn id_field_roundtrips() {
    #[derive(Debug, PartialEq, WaveWire)]
    struct HasId {
        id: Id,
        n: u32,
    }
    assert_eq!(HasId::STACK_SIZE, 20);
    roundtrip(&HasId { id: Id::new(42, 7, 1000, 123_456), n: 5 });
}

// ── Generics ─────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, WaveWire)]
struct Wrapper<T> {
    inner: T,
    count: u32,
}

#[test]
fn generic_struct_roundtrips() {
    roundtrip(&Wrapper { inner: "generic".to_owned(), count: 3 });
    roundtrip(&Wrapper { inner: 17u64, count: 0 });
    assert!(Wrapper::<u64>::FIXED);
    assert!(!Wrapper::<String>::FIXED);
}
