//! Object metadata — modification chain and schema versioning.
//!
//! Every WaveDB object carries a `Metadata` field that tracks:
//! - the ID of the **previous** version (`old_modification_id`)
//! - the ID of the **next** version (`new_modification_id`) — `0` means this
//!   is the live object
//! - the **schema version** at write time (`struct_version`), used for lazy
//!   migration when the compiled version has advanced

use crate::id::Id;
use std::fmt;

/// A packed 64-bit value containing the `struct_version` (16 bits) and `creator_id` (48 bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CreatorAndVersion(u64);

impl CreatorAndVersion {
    /// Creates a new packed `CreatorAndVersion`.
    #[inline]
    pub const fn new(struct_version: u16, creator_id: u64) -> Self {
        Self(((struct_version as u64) << 48) | (creator_id & 0x0000_FFFF_FFFF_FFFF))
    }

    /// Schema version at write time.
    #[inline]
    pub const fn struct_version(self) -> u16 {
        (self.0 >> 48) as u16
    }

    /// User/creator ID that created or modified this object (48 bits).
    #[inline]
    pub const fn creator_id(self) -> u64 {
        self.0 & 0x0000_FFFF_FFFF_FFFF
    }

    /// The raw packed `u64` value.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Reconstruct from a raw packed `u64`.
    #[inline]
    pub const fn from_raw(v: u64) -> Self {
        Self(v)
    }
}

/// Metadata stored in every WaveDB object.
///
/// # Versioning & Lazy Migration
///
/// `struct_version` is compared against the compiled version constant on every
/// read. If behind, the migration transform runs in memory and the updated
/// record is written back in the background — no global lock, no downtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    /// ID of the previous version of this object.
    /// `Id::ZERO` if this is the first version.
    pub old_modification_id: Id,

    /// ID of the next version of this object.
    /// `Id::ZERO` if this is the live object (anchor state).
    pub new_modification_id: Id,

    /// Packed schema version and creator ID.
    pub creator_and_version: CreatorAndVersion,
}

impl Metadata {
    /// Create metadata for a brand-new object (no prior versions).
    #[inline]
    pub const fn new(struct_version: u16, creator_id: u64) -> Self {
        Self {
            old_modification_id: Id::ZERO,
            new_modification_id: Id::ZERO,
            creator_and_version: CreatorAndVersion::new(struct_version, creator_id),
        }
    }

    /// Returns `true` if this is the **live** (anchor) version of the object —
    /// i.e., no newer version exists yet.
    #[inline]
    pub fn is_live(&self) -> bool {
        self.new_modification_id.is_zero()
    }

    /// Returns `true` if this is the **first** version — no predecessor.
    #[inline]
    pub fn is_first_version(&self) -> bool {
        self.old_modification_id.is_zero()
    }

    /// Schema version at write time.
    #[inline]
    pub const fn struct_version(&self) -> u16 {
        self.creator_and_version.struct_version()
    }

    /// User/creator ID that created or modified this object (48 bits).
    #[inline]
    pub const fn creator_id(&self) -> u64 {
        self.creator_and_version.creator_id()
    }

    /// Build metadata for a **mutation**: takes the old ID and bumps the
    /// schema version to the current compiled value.
    ///
    /// The caller is responsible for:
    /// 1. Writing the new versioned record with this metadata.
    /// 2. Updating the old record's `new_modification_id` to point forward.
    /// 3. Overwriting the anchor slot.
    #[inline]
    pub const fn for_mutation(old_id: Id, new_struct_version: u16, creator_id: u64) -> Self {
        Self {
            old_modification_id: old_id,
            new_modification_id: Id::ZERO,
            creator_and_version: CreatorAndVersion::new(new_struct_version, creator_id),
        }
    }
}

impl Default for Metadata {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

impl fmt::Display for Metadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Metadata {{ old={}, new={}, v={}, creator={} }}",
            self.old_modification_id,
            self.new_modification_id,
            self.struct_version(),
            self.creator_id(),
        )
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_object_is_live_and_first() {
        let m = Metadata::new(1, 42);
        assert!(m.is_live(), "fresh metadata should be live");
        assert!(m.is_first_version(), "fresh metadata has no predecessor");
        assert_eq!(m.struct_version(), 1);
        assert_eq!(m.creator_id(), 42);

        let cav = CreatorAndVersion::new(1, 42);
        assert_eq!(cav.struct_version(), 1);
        assert_eq!(cav.creator_id(), 42);
    }

    #[test]
    fn mutation_metadata() {
        use crate::id::{Id, ShardId, Slider, StructId, TenantId, Timestamp};

        let old_id = Id::new(
            TenantId::new(1),
            ShardId::new(0),
            StructId::new(5),
            Timestamp::from_ticks(100),
            Slider::new(0),
        );

        let m = Metadata::for_mutation(old_id, 2, 1001);
        assert_eq!(m.old_modification_id, old_id);
        assert!(m.is_live(), "mutation is the new live version");
        assert!(!m.is_first_version(), "mutation has a predecessor");
        assert_eq!(m.struct_version(), 2);
        assert_eq!(m.creator_id(), 1001);
    }
}
