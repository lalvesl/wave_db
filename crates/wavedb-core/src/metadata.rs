//! Per-record metadata. Lives next to the object's stack data on every page.

use crate::permission::PermissionRef;

/// Per-record metadata that tracks the version chain, authorship,
/// and access rules for a `WaveDB` object.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct Metadata {
    /// ID of the previous version of this object (`0u128` if this is the first).
    pub old_modification_id: u128,
    /// ID of the next version of this object (`0u128` if this is the live one).
    pub new_modification_id: u128,
    /// Schema version at write time. Used for lazy migration.
    pub struct_version: u8,
    /// User who created or modified this version (u48 in spirit; u64 in storage).
    pub user: u64,
    /// Device of the user that wrote this version. A single user may be online
    /// from multiple devices simultaneously; this field lets the engine
    /// attribute each version to the device that produced it.
    pub device_created: u64,
    /// Optional access-control rule. `None` is the common case (only the
    /// tenant's own users can touch this record).
    pub permission: Option<PermissionRef>,
}

impl PartialEq for Metadata {
    fn eq(&self, other: &Self) -> bool {
        self.old_modification_id == other.old_modification_id
            && self.new_modification_id == other.new_modification_id
            && self.struct_version == other.struct_version
            && self.user == other.user
            && self.device_created == other.device_created
            && self.permission == other.permission
    }
}

impl Eq for Metadata {}

impl Metadata {
    /// Returns `true` if this is the first version of the record
    /// (no previous modification).
    pub const fn is_first_version(&self) -> bool {
        self.old_modification_id == 0
    }

    /// Returns `true` if this is the live (most recent) version of the record.
    pub const fn is_live(&self) -> bool {
        self.new_modification_id == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_first_and_live() {
        let m = Metadata::default();
        assert!(m.is_first_version());
        assert!(m.is_live());
        assert_eq!(m.struct_version, 0);
        assert_eq!(m.user, 0);
        assert!(m.permission.is_none());
    }

    #[test]
    fn postcard_roundtrip() {
        let m = Metadata {
            old_modification_id: 0,
            new_modification_id: 0,
            struct_version: 42,
            user: 999,
            device_created: 1234,
            permission: None,
        };
        let bytes = postcard::to_allocvec(&m).unwrap();
        let decoded: Metadata = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(m, decoded);
    }

    #[test]
    fn none_permission_compact() {
        let perm: Option<crate::PermissionRef> = None;
        let bytes = postcard::to_allocvec(&perm).unwrap();
        assert_eq!(
            bytes.len(),
            1,
            "None permission must be 1 byte under postcard"
        );
    }

    #[test]
    fn version_chain_flags() {
        let mut m = Metadata::default();
        assert!(m.is_first_version());
        assert!(m.is_live());

        m.old_modification_id = 42;
        assert!(!m.is_first_version());
        assert!(m.is_live());

        m.new_modification_id = 43;
        assert!(!m.is_first_version());
        assert!(!m.is_live());
    }
}
