//! Anchor slot types for stable cross-pointer addresses.

use serde::{Deserialize, Serialize};

/// 128-bit anchor address.
#[repr(transparent)]
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct AnchorKey(pub u128);

impl AnchorKey {
    /// Create from raw u128.
    pub const fn from_raw(raw: u128) -> Self {
        Self(raw)
    }
    /// Return raw u128.
    pub const fn raw(self) -> u128 {
        self.0
    }
}

impl From<wavedb_core::Id> for AnchorKey {
    fn from(id: wavedb_core::Id) -> Self {
        Self(id.anchor_key().raw())
    }
}

/// How the primary anchor stores its data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AnchorMode {
    /// Full live record bytes in the anchor slot.
    Inline { bytes: Vec<u8> },
    /// Pointer to the versioned record.
    Pointer { versioned_id: u128 },
}

/// The kind of data stored in an anchor slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AnchorKind {
    /// Primary anchor for a record.
    Primary {
        current_version_at: u64,
        mode: AnchorMode,
        secondaries: Vec<AnchorKey>,
    },
    /// Secondary anchor — redirect to a primary.
    Secondary { primary: AnchorKey, marker: u64 },
    /// Tombstone for a deleted primary.
    PrimaryTombstone { final_version_at: u64 },
    /// Tombstone for a deleted secondary.
    SecondaryTombstone,
}

/// A complete anchor slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnchorSlot {
    /// What kind of anchor this is.
    pub kind: AnchorKind,
    /// All inbound references.
    pub references: Vec<AnchorKey>,
}

impl AnchorSlot {
    /// Create a new inline primary anchor.
    pub fn inline(bytes: &[u8], current_version_at: u64) -> Self {
        Self {
            kind: AnchorKind::Primary {
                current_version_at,
                mode: AnchorMode::Inline {
                    bytes: bytes.to_vec(),
                },
                secondaries: Vec::new(),
            },
            references: Vec::new(),
        }
    }

    /// Create a new pointer-only primary anchor.
    pub const fn pointer(versioned_id: u128, current_version_at: u64) -> Self {
        Self {
            kind: AnchorKind::Primary {
                current_version_at,
                mode: AnchorMode::Pointer { versioned_id },
                secondaries: Vec::new(),
            },
            references: Vec::new(),
        }
    }

    /// Create a secondary anchor.
    pub const fn secondary(primary: AnchorKey, marker: u64) -> Self {
        Self {
            kind: AnchorKind::Secondary { primary, marker },
            references: Vec::new(),
        }
    }

    /// Create a primary tombstone.
    pub const fn primary_tombstone(final_version_at: u64) -> Self {
        Self {
            kind: AnchorKind::PrimaryTombstone { final_version_at },
            references: Vec::new(),
        }
    }

    /// Create a secondary tombstone.
    pub const fn secondary_tombstone() -> Self {
        Self {
            kind: AnchorKind::SecondaryTombstone,
            references: Vec::new(),
        }
    }

    /// Is this a live primary?
    pub const fn is_live_primary(&self) -> bool {
        matches!(self.kind, AnchorKind::Primary { .. })
    }

    /// Is this a tombstone?
    pub const fn is_tombstone(&self) -> bool {
        matches!(
            self.kind,
            AnchorKind::PrimaryTombstone { .. } | AnchorKind::SecondaryTombstone
        )
    }

    /// Get inline bytes if available.
    pub fn inline_bytes(&self) -> Option<&[u8]> {
        match &self.kind {
            AnchorKind::Primary {
                mode: AnchorMode::Inline { bytes },
                ..
            } => Some(bytes),
            _ => None,
        }
    }

    /// Serialize to bytes.
    pub fn to_bytes(&self) -> crate::StorageResult<Vec<u8>> {
        Ok(postcard::to_allocvec(self)?)
    }

    /// Deserialize from bytes.
    pub fn from_bytes(bytes: &[u8]) -> crate::StorageResult<Self> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_roundtrip() {
        let slot = AnchorSlot::inline(b"hello", 1000);
        let bytes = slot.to_bytes().unwrap();
        let decoded = AnchorSlot::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.inline_bytes().unwrap(), b"hello");
        assert!(decoded.is_live_primary());
    }

    #[test]
    fn secondary_roundtrip() {
        let slot = AnchorSlot::secondary(AnchorKey::from_raw(999), 42);
        let bytes = slot.to_bytes().unwrap();
        let decoded = AnchorSlot::from_bytes(&bytes).unwrap();
        match &decoded.kind {
            AnchorKind::Secondary { primary, marker } => {
                assert_eq!(primary.raw(), 999);
                assert_eq!(*marker, 42);
            }
            _ => panic!("expected Secondary"),
        }
    }

    #[test]
    fn tombstones() {
        assert!(AnchorSlot::primary_tombstone(500).is_tombstone());
        assert!(AnchorSlot::secondary_tombstone().is_tombstone());
    }
}
