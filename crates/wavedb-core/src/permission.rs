//! Permission model for `WaveDB` records.
//!
//! Access control is stored inline in [`Metadata`](crate::Metadata).
//! `None` is the common case (only the tenant's own users can touch the
//! record); postcard encodes `None` in a single byte.

/// Reference to a separately-stored permission group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PermissionGroupId(pub u64);

/// Per-record access control.
///
/// | Variant | Semantics | Cost |
/// |---------|-----------|------|
/// | `Inline(list)` | Small ACL — a list of user IDs allowed to act on this record. | 1 byte tag + list bytes |
/// | `Group(id)` | Reference to a separately-stored permission group. | 1 byte tag + group ref |
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PermissionRef {
    /// A small inline ACL — a list of user IDs (u48 values stored as u64)
    /// allowed to act on this record. Auto-promotes to a per-record B+ tree
    /// once the list crosses a threshold.
    Inline(Vec<u64>),
    /// Reference to a separately-stored permission group, suited to large
    /// tenants where many records share an ACL.
    Group(PermissionGroupId),
}

impl PermissionRef {
    /// Check whether a given user is allowed under this permission ref.
    ///
    /// For `Inline`, the user must appear in the list.
    /// For `Group`, the caller must resolve the group externally.
    pub fn allows_user(&self, user: u64) -> bool {
        match self {
            Self::Inline(users) => users.contains(&user),
            Self::Group(_) => {
                // Group resolution requires external lookup — caller must
                // handle this case. We return false here as a safe default.
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_permission_one_byte() {
        let perm: Option<PermissionRef> = None;
        let bytes = postcard::to_allocvec(&perm).unwrap();
        assert_eq!(bytes.len(), 1, "None permission should serialize to 1 byte");
    }

    #[test]
    fn inline_permission_roundtrip() {
        let perm = Some(PermissionRef::Inline(vec![1, 2, 42]));
        let bytes = postcard::to_allocvec(&perm).unwrap();
        let decoded: Option<PermissionRef> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(perm, decoded);
    }

    #[test]
    fn group_permission_roundtrip() {
        let perm = Some(PermissionRef::Group(PermissionGroupId(99)));
        let bytes = postcard::to_allocvec(&perm).unwrap();
        let decoded: Option<PermissionRef> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(perm, decoded);
    }

    #[test]
    fn inline_allows_user() {
        let perm = PermissionRef::Inline(vec![10, 20, 30]);
        assert!(perm.allows_user(20));
        assert!(!perm.allows_user(99));
    }
}
