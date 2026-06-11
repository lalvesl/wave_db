//! Permission model for `WaveDB` records.
//!
//! Access control is stored inline in [`Metadata`](crate::Metadata).
//! `None` is the common case (only the tenant's own users can touch the
//! record).

/// Reference to a separately-stored permission group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PermissionGroupId(pub u64);

/// Per-record access control.
///
/// | Variant | Semantics | Cost |
/// |---------|-----------|------|
/// | `Inline(list)` | Small ACL — a list of user IDs allowed to act on this record. | 1 byte tag + list bytes |
/// | `Group(id)` | Reference to a separately-stored permission group. | 1 byte tag + group ref |
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionRef {
    /// A small inline ACL — a list of user IDs (u48 values stored as u64)
    /// allowed to act on this record. Auto-promotes to a per-record B+ tree
    /// once the list crosses a threshold.
    Inline(Vec<u64>),
    /// Reference to a separately-stored permission group, suited to large
    /// tenants where many records share an ACL.
    Group(PermissionGroupId),
}

// Manual `Wire` impls (wavedb-core cannot use the derive in wavedb-macros —
// the macro crate generates code that references this crate). Layout matches
// `#[derive(WaveWire)]` exactly; `wire_derive.rs` tests mirror these types.

impl crate::Wire for PermissionGroupId {
    const STACK_SIZE: usize = 8;
    const FIXED: bool = true;
    fn heap_size(&self) -> usize {
        0
    }
    fn write_stack(
        &self,
        w: &mut crate::WireWriter,
    ) -> crate::WireResult<()> {
        self.0.write_stack(w)
    }
    fn read(r: &mut crate::WireReader<'_>) -> crate::WireResult<Self> {
        Ok(Self(crate::Wire::read(r)?))
    }
}

impl crate::Wire for PermissionRef {
    // Payload enum: [tag u8][payload len u32] in the stack section.
    const STACK_SIZE: usize = 1 + 4;
    const FIXED: bool = false;

    fn heap_size(&self) -> usize {
        use crate::Wire;
        match self {
            // Variant stack (Vec length slot) + element bytes.
            Self::Inline(users) => {
                <Vec<u64> as Wire>::STACK_SIZE + users.heap_size()
            }
            Self::Group(_) => PermissionGroupId::STACK_SIZE,
        }
    }

    fn write_stack(
        &self,
        w: &mut crate::WireWriter,
    ) -> crate::WireResult<()> {
        use crate::Wire;
        match self {
            Self::Inline(users) => {
                0u8.write_stack(w)?;
                w.put_len_slot(
                    <Vec<u64> as Wire>::STACK_SIZE + users.heap_size(),
                )?;
                w.with_unit(<Vec<u64> as Wire>::STACK_SIZE, |w| {
                    users.write_stack(w)
                })
            }
            Self::Group(id) => {
                1u8.write_stack(w)?;
                w.put_len_slot(PermissionGroupId::STACK_SIZE)?;
                w.with_unit(PermissionGroupId::STACK_SIZE, |w| {
                    id.write_stack(w)
                })
            }
        }
    }

    fn read(r: &mut crate::WireReader<'_>) -> crate::WireResult<Self> {
        use crate::Wire;
        let tag = u8::read(r)?;
        let region = r.take_len_slot_region()?;
        match tag {
            0 => {
                let mut sub = crate::WireReader::for_unit(
                    region,
                    <Vec<u64> as Wire>::STACK_SIZE,
                )?;
                let users = Vec::<u64>::read(&mut sub)?;
                sub.finish_unit()?;
                Ok(Self::Inline(users))
            }
            1 => {
                let mut sub = crate::WireReader::for_unit(
                    region,
                    PermissionGroupId::STACK_SIZE,
                )?;
                let id = PermissionGroupId::read(&mut sub)?;
                sub.finish_unit()?;
                Ok(Self::Group(id))
            }
            tag => Err(crate::WireError::InvalidTag {
                type_name: "PermissionRef",
                tag,
            }),
        }
    }
}

impl PermissionRef {
    /// Check whether a given user is allowed under this permission ref.
    ///
    /// For `Inline`, the user must appear in the list.
    /// For `Group`, the caller must resolve the group externally.
    ///
    /// # Examples
    ///
    /// ```
    /// use wavedb_core::PermissionRef;
    /// let perm = PermissionRef::Inline(vec![10, 20, 30]);
    /// assert!(perm.allows_user(20));
    /// assert!(!perm.allows_user(99));
    ///
    /// // Group refs always return false — caller resolves them externally.
    /// use wavedb_core::PermissionGroupId;
    /// let group = PermissionRef::Group(PermissionGroupId(1));
    /// assert!(!group.allows_user(20));
    /// ```
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
    fn none_permission_fixed_slot() {
        let perm: Option<PermissionRef> = None;
        let bytes = crate::wire::to_wire(&perm).unwrap();
        assert_eq!(
            bytes.len(),
            <Option<PermissionRef> as crate::Wire>::STACK_SIZE,
            "None permission reserves the fixed stack slot, no heap"
        );
    }

    #[test]
    fn inline_permission_roundtrip() {
        let perm = Some(PermissionRef::Inline(vec![1, 2, 42]));
        let bytes = crate::wire::to_wire(&perm).unwrap();
        let decoded: Option<PermissionRef> =
            crate::wire::from_wire(&bytes).unwrap();
        assert_eq!(perm, decoded);
    }

    #[test]
    fn group_permission_roundtrip() {
        let perm = Some(PermissionRef::Group(PermissionGroupId(99)));
        let bytes = crate::wire::to_wire(&perm).unwrap();
        let decoded: Option<PermissionRef> =
            crate::wire::from_wire(&bytes).unwrap();
        assert_eq!(perm, decoded);
    }

    #[test]
    fn inline_allows_user() {
        let perm = PermissionRef::Inline(vec![10, 20, 30]);
        assert!(perm.allows_user(20));
        assert!(!perm.allows_user(99));
    }
}
