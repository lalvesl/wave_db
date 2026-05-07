//! Permissions enforcement layer.
//!
//! Enforces `permission: Option<PermissionRef>` on every read/write.
//! - `None` → only users in the same tenant can access.
//! - `Some(Inline([...]))` → only listed users can access.
//! - `Some(Group(id))` → resolves through a permission group table.
//!
//! Inline lists auto-promote to a B+ tree when they cross a threshold.

use crate::anchor::AnchorKey;
use crate::index::adaptive::AdaptiveIndex;
use crate::index::{IndexBackend, IndexKey};
use std::collections::HashMap;
use wavedb_core::{PermissionGroupId, PermissionRef};

/// Threshold for auto-promoting an inline permission list to a B+ tree.
const INLINE_TO_TREE_THRESHOLD: usize = 50;

/// A permission group stored separately (for large tenants sharing ACLs).
#[derive(Debug, Clone)]
pub struct PermissionGroup {
    /// The group identifier.
    pub id: PermissionGroupId,
    /// Users in this group.
    pub users: Vec<u64>,
}

// TODO(lalvesl): Implement Anchor like to manage group, this use only one disk lookup instead of adaptive index
/// Permission group storage (in-memory for now).
#[derive(Debug, Default)]
pub struct PermissionGroupStore {
    groups: HashMap<u64, PermissionGroup>,
}

impl PermissionGroupStore {
    /// Create a new empty store.
    pub fn new() -> Self {
        Self {
            groups: HashMap::new(),
        }
    }

    /// Register a permission group.
    pub fn register(&mut self, group: PermissionGroup) {
        self.groups.insert(group.id.0, group);
    }

    /// Look up a group by its ID.
    pub fn get(&self, id: PermissionGroupId) -> Option<&PermissionGroup> {
        self.groups.get(&id.0)
    }
}

/// Result of a permission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionResult {
    /// Access is allowed.
    Allowed,
    /// Access is denied.
    Denied,
}

/// Check whether a user has access to a record.
///
/// # Arguments
/// - `permission` — the record's permission field.
/// - `user` — the user attempting the operation.
/// - `record_tenant` — the tenant that owns the record.
/// - `user_tenant` — the tenant the user belongs to.
/// - `groups` — the group store for resolving group references.
pub fn check_access(
    permission: &Option<PermissionRef>,
    user: u64,
    record_tenant: u64,
    user_tenant: u64,
    groups: &PermissionGroupStore,
) -> PermissionResult {
    match permission {
        None => {
            // No permission set → only users in the same tenant can access
            if user_tenant == record_tenant {
                PermissionResult::Allowed
            } else {
                PermissionResult::Denied
            }
        }
        Some(PermissionRef::Inline(users)) => {
            if users.contains(&user) {
                PermissionResult::Allowed
            } else {
                PermissionResult::Denied
            }
        }
        Some(PermissionRef::Group(group_id)) => {
            if let Some(group) = groups.get(*group_id) {
                if group.users.contains(&user) {
                    PermissionResult::Allowed
                } else {
                    PermissionResult::Denied
                }
            } else {
                // Group not found — deny access
                PermissionResult::Denied
            }
        }
    }
}

/// An inline permission list that auto-promotes to a B+ tree index
/// when it crosses the threshold.
#[derive(Debug)]
pub enum PromotablePermissionList {
    /// Small inline list.
    Inline(Vec<u64>),
    /// Promoted to a B+ tree for fast membership checks.
    Tree(AdaptiveIndex),
}

impl PromotablePermissionList {
    /// Create from an existing inline list.
    pub fn from_inline(users: Vec<u64>) -> Self {
        if users.len() > INLINE_TO_TREE_THRESHOLD {
            let mut tree = AdaptiveIndex::with_threshold(0); // force tree immediately
            for &user in &users {
                let _ = tree.insert(IndexKey(user), AnchorKey::from_raw(1));
            }
            Self::Tree(tree)
        } else {
            Self::Inline(users)
        }
    }

    /// Add a user. Returns `true` if a promotion occurred.
    pub fn add_user(&mut self, user: u64) -> bool {
        match self {
            Self::Inline(users) => {
                if !users.contains(&user) {
                    users.push(user);
                }
                if users.len() > INLINE_TO_TREE_THRESHOLD {
                    let old_users = std::mem::take(users);
                    let mut tree = AdaptiveIndex::with_threshold(0);
                    for &u in &old_users {
                        let _ = tree.insert(IndexKey(u), AnchorKey::from_raw(1));
                    }
                    *self = Self::Tree(tree);
                    return true;
                }
                false
            }
            Self::Tree(tree) => {
                let _ = tree.insert(IndexKey(user), AnchorKey::from_raw(1));
                false
            }
        }
    }

    /// Check if a user is in the list.
    pub fn contains(&self, user: u64) -> bool {
        match self {
            Self::Inline(users) => users.contains(&user),
            Self::Tree(tree) => tree.lookup(&IndexKey(user)).is_some(),
        }
    }

    /// Number of users.
    pub fn len(&self) -> usize {
        match self {
            Self::Inline(users) => users.len(),
            Self::Tree(tree) => tree.len(),
        }
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether this list has been promoted.
    pub fn is_promoted(&self) -> bool {
        matches!(self, Self::Tree(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_groups() -> PermissionGroupStore {
        let mut store = PermissionGroupStore::new();
        store.register(PermissionGroup {
            id: PermissionGroupId(1),
            users: vec![10, 20, 30],
        });
        store
    }

    #[test]
    fn none_permission_same_tenant_allowed() {
        let groups = make_groups();
        let result = check_access(&None, 42, 100, 100, &groups);
        assert_eq!(result, PermissionResult::Allowed);
    }

    #[test]
    fn none_permission_different_tenant_denied() {
        let groups = make_groups();
        let result = check_access(&None, 42, 100, 200, &groups);
        assert_eq!(result, PermissionResult::Denied);
    }

    #[test]
    fn inline_permission_allows_listed_user() {
        let groups = make_groups();
        let perm = Some(PermissionRef::Inline(vec![1, 2, 42]));
        assert_eq!(
            check_access(&perm, 42, 100, 100, &groups),
            PermissionResult::Allowed
        );
    }

    #[test]
    fn inline_permission_denies_unlisted_user() {
        let groups = make_groups();
        let perm = Some(PermissionRef::Inline(vec![1, 2, 42]));
        assert_eq!(
            check_access(&perm, 99, 100, 100, &groups),
            PermissionResult::Denied
        );
    }

    #[test]
    fn group_permission_resolves_correctly() {
        let groups = make_groups();
        let perm = Some(PermissionRef::Group(PermissionGroupId(1)));
        assert_eq!(
            check_access(&perm, 20, 100, 100, &groups),
            PermissionResult::Allowed
        );
        assert_eq!(
            check_access(&perm, 99, 100, 100, &groups),
            PermissionResult::Denied
        );
    }

    #[test]
    fn group_not_found_denies() {
        let groups = make_groups();
        let perm = Some(PermissionRef::Group(PermissionGroupId(999)));
        assert_eq!(
            check_access(&perm, 20, 100, 100, &groups),
            PermissionResult::Denied
        );
    }

    #[test]
    fn inline_auto_promotes_at_threshold() {
        let mut list = PromotablePermissionList::from_inline(Vec::new());
        assert!(!list.is_promoted());

        for i in 0..=INLINE_TO_TREE_THRESHOLD as u64 {
            list.add_user(i);
        }
        assert!(
            list.is_promoted(),
            "should auto-promote after crossing threshold"
        );

        // All users should still be findable
        for i in 0..=INLINE_TO_TREE_THRESHOLD as u64 {
            assert!(list.contains(i), "user {i} lost during promotion");
        }
    }

    #[test]
    fn promoted_membership_semantics() {
        let mut list = PromotablePermissionList::from_inline(Vec::new());
        for i in 0..60 {
            list.add_user(i);
        }
        assert!(list.is_promoted());
        assert!(list.contains(0));
        assert!(list.contains(59));
        assert!(!list.contains(60));
    }

    #[test]
    fn from_inline_large_promotes_immediately() {
        let users: Vec<u64> = (0..100).collect();
        let list = PromotablePermissionList::from_inline(users);
        assert!(list.is_promoted());
        assert_eq!(list.len(), 100);
    }
}
