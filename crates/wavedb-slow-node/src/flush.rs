//! Wire types for Quick-Node → Slow-Node flush and history-read requests.

use serde::{Deserialize, Serialize};
use wavedb_storage::VersionedRecord;

/// A batch of versioned records pushed from a Quick-Node during a history flush.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlushBatch {
    /// Monotonic write-sequence number assigned by the Quick-Node.
    pub write_seq: u64,
    /// The tenant that produced these records.
    pub tenant: u64,
    /// Records to persist.
    pub records: Vec<VersionedRecord>,
}

/// Acknowledgement returned to the Quick-Node after a successful flush.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlushAck {
    /// The write sequence that was durably stored.
    pub write_seq: u64,
}

/// Request a history read for a specific versioned record ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryReadRequest {
    /// Tenant owning the record.
    pub tenant: u64,
    /// 128-bit versioned record ID.
    pub record_id: u128,
}

/// Response to a history-read request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryReadResponse {
    /// Raw serialized bytes of the [`VersionedRecord`], or empty if not found.
    pub data: Vec<u8>,
}

/// Watermark tracking the highest durably-acked write sequence per tenant.
#[derive(Debug, Default)]
pub struct ReceiptWatermark {
    /// `tenant → highest acked write_seq`
    marks: std::collections::HashMap<u64, u64>,
}

impl ReceiptWatermark {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance the watermark for `tenant` to `seq` (monotonic, ignores regressions).
    pub fn advance(&mut self, tenant: u64, seq: u64) {
        let entry = self.marks.entry(tenant).or_insert(0);
        if seq > *entry {
            *entry = seq;
        }
    }

    /// Highest acked write sequence for `tenant`, or 0 if none.
    pub fn high_water(&self, tenant: u64) -> u64 {
        self.marks.get(&tenant).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watermark_advances_monotonically() {
        let mut wm = ReceiptWatermark::new();
        wm.advance(1, 5);
        assert_eq!(wm.high_water(1), 5);
        wm.advance(1, 3); // regression — ignored
        assert_eq!(wm.high_water(1), 5);
        wm.advance(1, 10);
        assert_eq!(wm.high_water(1), 10);
    }

    #[test]
    fn watermark_per_tenant() {
        let mut wm = ReceiptWatermark::new();
        wm.advance(1, 100);
        wm.advance(2, 50);
        assert_eq!(wm.high_water(1), 100);
        assert_eq!(wm.high_water(2), 50);
        assert_eq!(wm.high_water(3), 0);
    }

    #[test]
    fn flush_batch_serde_roundtrip() {
        let batch = FlushBatch {
            write_seq: 7,
            tenant: 42,
            records: vec![VersionedRecord::new(1, b"hello".to_vec())],
        };
        let bytes = postcard::to_allocvec(&batch).unwrap();
        let decoded: FlushBatch = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.write_seq, 7);
        assert_eq!(decoded.tenant, 42);
        assert_eq!(decoded.records.len(), 1);
    }
}
