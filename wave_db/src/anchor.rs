//! Anchor slots — stable cross-reference targets for WaveDB records.
//!
//! Every live record has two storage slots:
//!
//! | Slot           | Keyed at                              | Contents                            |
//! |----------------|---------------------------------------|-------------------------------------|
//! | **Anchor**     | `(STRUCT_ID, TENANT_ID)` — no ts      | Live data (inline) **or** pointer   |
//! | **Versioned**  | `(STRUCT_ID, TENANT_ID, CREATED_AT)`  | Full data + modification chain      |
//!
//! All cross-references (indexes, M2M links, sync handles) point to the anchor
//! address, never to a versioned record. When the underlying data mutates,
//! none of those pointers need rewriting.

use crate::id::Id;

// ─── Anchor content ───────────────────────────────────────────────────────────

/// The content stored inside an [`AnchorSlot`].
///
/// Supports two operating modes and a terminal tombstone state.
#[derive(Debug, Clone)]
pub enum AnchorContent {
    /// Full live record bytes stored inline — **Quick-Node mode**.
    ///
    /// Trades extra storage (~2× live data) for one fewer IO on the read path.
    Inline {
        /// Serialised bytes of the current live record.
        data: Vec<u8>,
        /// Full ID of the current live versioned record (used for direct O(1)
        /// versioned-record lookup and for the modification-chain walk).
        current_version_id: Id,
    },

    /// Only a pointer to the versioned record — **pointer-only mode**.
    ///
    /// Storage-optimal; costs one extra IO per read.
    PointerOnly {
        /// Full ID of the current live versioned record.
        current_version_id: Id,
    },

    /// The record was deleted.
    ///
    /// References can distinguish "deleted" from "never existed" with a single
    /// read. History remains traversable via `final_version_id`.
    Tombstone {
        /// Full ID of the last live version — history chain entry point.
        final_version_id: Id,
    },
}

impl AnchorContent {
    /// Returns the live version ID, or `None` if this is a tombstone.
    #[inline]
    pub fn current_version_id(&self) -> Option<Id> {
        match self {
            Self::Inline { current_version_id, .. }
            | Self::PointerOnly { current_version_id } => Some(*current_version_id),
            Self::Tombstone { .. } => None,
        }
    }

    /// Returns the most-recent version ID regardless of tombstone state.
    ///
    /// Useful for starting a backward history walk even after deletion.
    #[inline]
    pub fn most_recent_version_id(&self) -> Option<Id> {
        match self {
            Self::Inline { current_version_id, .. }
            | Self::PointerOnly { current_version_id } => Some(*current_version_id),
            Self::Tombstone { final_version_id } => Some(*final_version_id),
        }
    }

    /// Returns `true` if this slot has been tombstoned.
    #[inline]
    pub fn is_tombstone(&self) -> bool {
        matches!(self, Self::Tombstone { .. })
    }
}

// ─── AnchorSlot ───────────────────────────────────────────────────────────────

/// A stable slot for a `(STRUCT_ID, TENANT_ID)` pair.
///
/// Holds either live data or a pointer to the current versioned record, plus
/// the full list of inbound references so cross-pointer rewrites are never
/// needed on mutation.
#[derive(Debug, Clone)]
pub struct AnchorSlot {
    /// The current state of this anchor.
    pub content: AnchorContent,

    /// All inbound references that route through this anchor
    /// (index entries, M2M links, sync handles, etc.).
    ///
    /// These never need to be rewritten when the underlying record mutates
    /// because they target the anchor address, not a versioned record.
    pub inbound_refs: Vec<Id>,
}

impl AnchorSlot {
    /// Create a new inline-mode anchor for a freshly inserted record.
    pub fn new_inline(data: Vec<u8>, current_version_id: Id) -> Self {
        Self {
            content: AnchorContent::Inline { data, current_version_id },
            inbound_refs: Vec::new(),
        }
    }

    /// Create a new pointer-only anchor.
    pub fn new_pointer(current_version_id: Id) -> Self {
        Self {
            content: AnchorContent::PointerOnly { current_version_id },
            inbound_refs: Vec::new(),
        }
    }

    /// Replace this anchor's content with a tombstone, preserving `inbound_refs`.
    ///
    /// References to deleted objects can distinguish "deleted" from "never
    /// existed" with a single read; history remains accessible via
    /// `final_version_id`.
    pub fn tombstone(&mut self, final_version_id: Id) {
        self.content = AnchorContent::Tombstone { final_version_id };
    }

    /// Returns `true` if this anchor holds live data (not a tombstone).
    #[inline]
    pub fn is_live(&self) -> bool {
        !self.content.is_tombstone()
    }

    /// Returns the live version ID, or `None` if tombstoned.
    #[inline]
    pub fn current_version_id(&self) -> Option<Id> {
        self.content.current_version_id()
    }

    /// Returns the most-recent version ID (works for tombstones too).
    #[inline]
    pub fn most_recent_version_id(&self) -> Option<Id> {
        self.content.most_recent_version_id()
    }

    /// Returns the inline data bytes if operating in inline mode.
    #[inline]
    pub fn inline_data(&self) -> Option<&[u8]> {
        match &self.content {
            AnchorContent::Inline { data, .. } => Some(data),
            _ => None,
        }
    }

    /// Register a new inbound reference pointing through this anchor.
    pub fn add_inbound_ref(&mut self, ref_id: Id) {
        if !self.inbound_refs.contains(&ref_id) {
            self.inbound_refs.push(ref_id);
        }
    }

    /// Remove an inbound reference (e.g. when an index entry is deleted).
    pub fn remove_inbound_ref(&mut self, ref_id: Id) {
        self.inbound_refs.retain(|&r| r != ref_id);
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{Id, ShardId, Slider, StructId, TenantId, Timestamp};

    fn make_id(ts: u64) -> Id {
        Id::new(
            TenantId::new(1),
            ShardId::new(0),
            StructId::new(2),
            Timestamp::from_ticks(ts),
            Slider::new(0),
        )
    }

    #[test]
    fn inline_anchor_is_live() {
        let id = make_id(100);
        let slot = AnchorSlot::new_inline(b"data".to_vec(), id);
        assert!(slot.is_live());
        assert_eq!(slot.current_version_id(), Some(id));
        assert_eq!(slot.inline_data(), Some(b"data".as_ref()));
    }

    #[test]
    fn pointer_anchor_is_live() {
        let id = make_id(200);
        let slot = AnchorSlot::new_pointer(id);
        assert!(slot.is_live());
        assert_eq!(slot.current_version_id(), Some(id));
        assert!(slot.inline_data().is_none());
    }

    #[test]
    fn tombstone_is_not_live() {
        let id = make_id(300);
        let mut slot = AnchorSlot::new_inline(b"data".to_vec(), id);
        assert!(slot.is_live());
        slot.tombstone(id);
        assert!(!slot.is_live());
        assert_eq!(slot.current_version_id(), None);
        assert_eq!(slot.most_recent_version_id(), Some(id));
    }

    #[test]
    fn tombstone_preserves_inbound_refs() {
        let id = make_id(400);
        let ref1 = make_id(401);
        let mut slot = AnchorSlot::new_inline(b"data".to_vec(), id);
        slot.add_inbound_ref(ref1);
        slot.tombstone(id);
        assert_eq!(slot.inbound_refs, vec![ref1]);
    }

    #[test]
    fn inbound_ref_dedup() {
        let id = make_id(500);
        let ref1 = make_id(501);
        let mut slot = AnchorSlot::new_pointer(id);
        slot.add_inbound_ref(ref1);
        slot.add_inbound_ref(ref1); // duplicate
        assert_eq!(slot.inbound_refs.len(), 1);
    }

    #[test]
    fn remove_inbound_ref() {
        let id = make_id(600);
        let ref1 = make_id(601);
        let ref2 = make_id(602);
        let mut slot = AnchorSlot::new_pointer(id);
        slot.add_inbound_ref(ref1);
        slot.add_inbound_ref(ref2);
        slot.remove_inbound_ref(ref1);
        assert_eq!(slot.inbound_refs, vec![ref2]);
    }
}
