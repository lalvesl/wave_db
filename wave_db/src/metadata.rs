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

    /// Schema version at write time — used for lazy migration.
    pub struct_version: u32,
}

impl Metadata {
    /// Create metadata for a brand-new object (no prior versions).
    #[inline]
    pub const fn new(struct_version: u32) -> Self {
        Self {
            old_modification_id: Id::ZERO,
            new_modification_id: Id::ZERO,
            struct_version,
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

    /// Build metadata for a **mutation**: takes the old ID and bumps the
    /// schema version to the current compiled value.
    ///
    /// The caller is responsible for:
    /// 1. Writing the new versioned record with this metadata.
    /// 2. Updating the old record's `new_modification_id` to point forward.
    /// 3. Overwriting the anchor slot.
    #[inline]
    pub const fn for_mutation(old_id: Id, new_struct_version: u32) -> Self {
        Self {
            old_modification_id: old_id,
            new_modification_id: Id::ZERO,
            struct_version: new_struct_version,
        }
    }
}

impl Default for Metadata {
    fn default() -> Self {
        Self::new(0)
    }
}

impl fmt::Display for Metadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Metadata {{ old={}, new={}, v={} }}",
            self.old_modification_id, self.new_modification_id, self.struct_version,
        )
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_object_is_live_and_first() {
        let m = Metadata::new(1);
        assert!(m.is_live(), "fresh metadata should be live");
        assert!(m.is_first_version(), "fresh metadata has no predecessor");
        assert_eq!(m.struct_version, 1);
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

        let m = Metadata::for_mutation(old_id, 2);
        assert_eq!(m.old_modification_id, old_id);
        assert!(m.is_live(), "mutation is the new live version");
        assert!(!m.is_first_version(), "mutation has a predecessor");
        assert_eq!(m.struct_version, 2);
    }
}
