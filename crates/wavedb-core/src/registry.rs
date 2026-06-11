//! Static object registry primitives.
//!
//! Every `#[wave_db]` struct gets a generated `&'static ObjectDescriptor`
//! describing its complete wire layout: stack size, per-field stack offsets,
//! and which fields own heap regions. The `declare_objects!` macro collects
//! the descriptors of **all** object versions an application knows about into
//! one module — quick-nodes, slow-nodes, and clients all build the same
//! registry at compile time, searchable by the `u32` record header
//! (`struct_id` u24 `<< 8 |` version u8).
//!
//! Because the registry is plain `'static` data plus monomorphised lookup
//! fns, the storage engine can locate any field of any declared object
//! without deserialising it — and without a single `dyn` dispatch.

use crate::Shape;

/// Coarse wire-type classification of a field, for engines that organise
/// anchors and indexes from the descriptor alone.
///
/// `stack_offset` / `stack_size` on [`FieldDescriptor`] are always exact
/// regardless of the kind; `Other` only means "no specialised index/anchor
/// handling is implied".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FieldKind {
    /// `u8`
    U8,
    /// `u16`
    U16,
    /// `u32`
    U32,
    /// `u64`
    U64,
    /// `u128`
    U128,
    /// `i8`
    I8,
    /// `i16`
    I16,
    /// `i32`
    I32,
    /// `i64`
    I64,
    /// `i128`
    I128,
    /// `f32`
    F32,
    /// `f64`
    F64,
    /// `bool`
    Bool,
    /// `char`
    Char,
    /// [`crate::Id`] or a macro-generated typed `FooId` / `FooAnchor` wrapper.
    Id,
    /// `String` — heap field, u32 length slot.
    Str,
    /// `Vec<u8>` — raw blob, u32 length slot.
    Bytes,
    /// `Vec<T>` for any other `T` — u32 region-length slot.
    List,
    /// `Option<T>` — 1 flag byte + `T`'s stack slots.
    Option,
    /// Anything else implementing `Wire` (nested structs, enums, tuples).
    Other,
}

/// Wire layout of one field: where its stack slot lives and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldDescriptor {
    /// Field name as declared in source.
    pub name: &'static str,
    /// Byte offset of this field's slot inside the stack section.
    pub stack_offset: usize,
    /// Byte size of this field's slot inside the stack section.
    pub stack_size: usize,
    /// `true` when the field can own bytes in the heap section
    /// (`!<T as Wire>::FIXED` — exact, not a heuristic).
    pub heapable: bool,
    /// Coarse type classification.
    pub kind: FieldKind,
}

/// Complete compile-time description of one `(struct_id, version)` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectDescriptor {
    /// Registry search key: `(struct_id as u24) << 8 | version`.
    pub header: u32,
    /// The struct family ID (u20).
    pub struct_id: u32,
    /// The schema version (trailing integer of the type name).
    pub version: u8,
    /// `Unique` / `NonUnique` / `NestedNonUnique`.
    pub shape: Shape,
    /// The Rust type name (`"Message42"`).
    pub type_name: &'static str,
    /// `<T as Wire>::STACK_SIZE` — the compile-time stack-section size.
    pub stack_size: usize,
    /// `<T as Wire>::FIXED` — `true` when records never carry heap bytes.
    pub fixed: bool,
    /// Every field in wire order (including the injected `id` / `metadata`).
    pub fields: &'static [FieldDescriptor],
    /// Names of the heap-owning fields, in wire order — the "current list of
    /// heap properties" the engine consults for eviction and heap anchors.
    pub heap_fields: &'static [&'static str],
}

impl ObjectDescriptor {
    /// Find a field by name.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&'static FieldDescriptor> {
        self.fields.iter().find(|f| f.name == name)
    }
}
