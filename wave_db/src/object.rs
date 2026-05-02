//! Core traits that every WaveDB object must satisfy.
//!
//! The `#[wave_db]` proc-macro generates the `WaveDbObject` impl, the
//! `UniqueObject` / `NonUniqueObject` marker, and the zero-sized `<Name>Kind`
//! dispatch type automatically.

use crate::{Id, Metadata};

/// Implemented by every struct annotated with `#[wave_db]`.
///
/// Provides access to the mandatory `id` and `metadata` fields, and carries
/// an associated `Kind` type for zero-cost dispatch.
pub trait WaveDbObject: Sized {
    /// Zero-sized marker type for this object kind (e.g. `OrderKind`).
    type Kind: 'static + Copy;

    /// Shared reference to the record's composite ID.
    fn id(&self) -> &Id;

    /// Shared reference to the record's metadata.
    fn metadata(&self) -> &Metadata;

    /// Mutable reference to the record's ID.
    fn id_mut(&mut self) -> &mut Id;

    /// Mutable reference to the record's metadata.
    fn metadata_mut(&mut self) -> &mut Metadata;

    // ── Convenience helpers ──────────────────────────────────────────────────

    /// Returns `true` if this is the live version of the record.
    #[inline]
    fn is_live(&self) -> bool {
        self.metadata().is_live()
    }

    /// Returns `true` if this is the first version of the record (no predecessor).
    #[inline]
    fn is_first_version(&self) -> bool {
        self.metadata().is_first_version()
    }
}

/// Marker trait for objects where only **one** live record exists per tenant
/// (e.g. a user profile, company settings).
///
/// Generated automatically by `#[wave_db]` (without `NonUnique`).
pub trait UniqueObject: WaveDbObject {}

/// Marker trait for objects that allow **multiple** live records per tenant
/// (e.g. orders, messages).
///
/// Generated automatically by `#[wave_db(NonUnique)]`.
///
/// The `BTREE_THRESHOLD` constant controls at what collection size the inline
/// array index upgrades to a per-tenant B+ tree (default **50**).
pub trait NonUniqueObject: WaveDbObject {
    /// Number of items at which the index transitions from a flat array to a
    /// B+ tree. Configurable via `#[wave_db(NonUnique, btree_threshold = N)]`.
    const BTREE_THRESHOLD: u32;
}
