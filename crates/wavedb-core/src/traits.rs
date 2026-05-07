//! The trait that every `#[wave_db]`-annotated struct implements.

use crate::Shape;

/// Trait implemented by the `#[wave_db]` proc-macro on every annotated struct.
///
/// Provides compile-time constants for struct identification and classification.
pub trait WaveDbStruct {
    /// The permanent struct family identifier (u20, shared across versions).
    const STRUCT_ID: u32;
    /// The schema version, parsed from the trailing integer of the type name.
    const STRUCT_VERSION: u8;
    /// The data shape: `Unique`, `NonUnique`, or `NestedNonUnique`.
    const SHAPE: Shape;
}
