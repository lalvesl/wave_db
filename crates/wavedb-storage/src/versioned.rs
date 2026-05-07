//! Versioned record storage — the immutable historical data.

use serde::{Deserialize, Serialize};

/// A versioned record: the full object data at a specific point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedRecord {
    /// The 128-bit ID of this record version.
    pub id: u128,
    /// The serialized object bytes.
    pub data: Vec<u8>,
    /// ID of the previous version (0 if first).
    pub old_modification_id: u128,
    /// ID of the next version (0 if live).
    pub new_modification_id: u128,
}

impl VersionedRecord {
    /// Create a new versioned record (first version).
    pub fn new(id: u128, data: Vec<u8>) -> Self {
        Self { id, data, old_modification_id: 0, new_modification_id: 0 }
    }

    /// Is this the live (most recent) version?
    pub fn is_live(&self) -> bool { self.new_modification_id == 0 }

    /// Serialize to bytes.
    pub fn to_bytes(&self) -> crate::StorageResult<Vec<u8>> { Ok(postcard::to_allocvec(self)?) }

    /// Deserialize from bytes.
    pub fn from_bytes(bytes: &[u8]) -> crate::StorageResult<Self> { Ok(postcard::from_bytes(bytes)?) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let rec = VersionedRecord::new(42, b"test data".to_vec());
        assert!(rec.is_live());
        let bytes = rec.to_bytes().unwrap();
        let decoded = VersionedRecord::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.id, 42);
        assert_eq!(decoded.data, b"test data");
        assert!(decoded.is_live());
    }

    #[test]
    fn version_chain() {
        let mut v1 = VersionedRecord::new(1, b"v1".to_vec());
        let v2 = VersionedRecord { id: 2, data: b"v2".to_vec(), old_modification_id: 1, new_modification_id: 0 };
        v1.new_modification_id = 2;
        assert!(!v1.is_live());
        assert!(v2.is_live());
    }
}
