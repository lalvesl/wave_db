//! Wire types for the monitor data-browse endpoint (`POST /browse`).
//!
//! The monitor (TUI or GUI) sends a [`BrowseRequest`] to a Slow-Node and
//! receives back a [`BrowseResponse`] describing the tenants and versioned
//! records held in the history store — metadata only, never record payloads.
//! Payload bytes are fetched separately through the existing `/history`
//! endpoint once the operator picks a specific record.
//!
//! Authorization mirrors `/metrics`: when the node has a cluster key, the
//! request must carry a [`NodeToken`] minted with [`TokenPurpose::Monitor`].
//!
//! [`TokenPurpose::Monitor`]: crate::auth::TokenPurpose::Monitor

use wavedb_macros::WaveWire;

use crate::auth::NodeToken;

/// Body of a `POST /browse` request.
#[derive(Debug, Clone, WaveWire)]
pub struct BrowseRequest {
    /// Cluster membership proof (required when the node has a cluster key).
    pub token: Option<NodeToken>,
    /// When `Some`, also return record summaries for this tenant.
    /// When `None`, only the tenant list is returned.
    pub tenant: Option<u64>,
    /// Cap on the number of record summaries returned (`0` = node default).
    pub max_records: u32,
}

/// Per-tenant aggregate of the history store.
#[derive(Debug, Clone, WaveWire)]
pub struct TenantSummary {
    /// Tenant ID.
    pub tenant: u64,
    /// Versioned records held for this tenant.
    pub record_count: u64,
    /// Sum of serialized record bytes for this tenant.
    pub total_bytes: u64,
}

/// Metadata for one versioned record — enough to draw it, not to read it.
#[derive(Debug, Clone, WaveWire)]
pub struct RecordSummary {
    /// The 128-bit record ID (`TENANT | SHARD | STRUCT | CREATED_AT`).
    pub id: u128,
    /// Size of the record's payload bytes.
    pub data_bytes: u32,
    /// Quick-Node write sequence that flushed this record.
    pub write_seq: u64,
    /// Record envelope header (`STRUCT_ID << 8 | STRUCT_VERSION`), or `0`
    /// when the payload is too short to carry one.
    pub header: u32,
}

/// Response to a [`BrowseRequest`].
#[derive(Debug, Clone, WaveWire)]
pub struct BrowseResponse {
    /// All tenants with at least one record, in ascending tenant order.
    pub tenants: Vec<TenantSummary>,
    /// Record summaries for the requested tenant (empty when no tenant was
    /// requested), in ascending record-ID order.
    pub records: Vec<RecordSummary>,
    /// `true` when `records` was truncated at the cap.
    pub truncated: bool,
}

// ── History read (shared with `POST /history`) ────────────────────────────────

/// Request a history read for a specific versioned record ID.
///
/// Served by the Slow-Node's `POST /history`; used by clients and by the
/// monitor's record inspector.
#[derive(Debug, Clone, WaveWire)]
pub struct HistoryReadRequest {
    /// Tenant owning the record.
    pub tenant: u64,
    /// 128-bit versioned record ID.
    pub record_id: u128,
}

/// Response to a history-read request.
#[derive(Debug, Clone, WaveWire)]
pub struct HistoryReadResponse {
    /// Raw serialized bytes of the versioned record, or empty if not found.
    pub data: Vec<u8>,
}
