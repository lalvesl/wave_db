//! Shared metric types exchanged between nodes and the monitor TUI.
//!
//! The monitor sends a [`MetricsRequest`] to each node's `POST /metrics`
//! endpoint and receives back either a [`QuickNodeMetrics`] or a
//! [`SlowNodeMetrics`], both postcard-encoded.

use serde::{Deserialize, Serialize};

use crate::auth::NodeToken;

/// Body of a `POST /metrics` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsRequest {
    /// Cluster membership proof (required when the node has a cluster key).
    pub token: Option<NodeToken>,
}

/// Default page size used by the storage engine (8 KiB).
///
/// Doubled from the historical 4 KiB so the in-page directory + payload
/// fit larger records without immediate overflow.  Storage callers may
/// override per-file via [`wavedb_storage::DataFile::open_with`].
pub const HEAP_PAGE_SIZE: u64 = 4 * 1024;

/// Number of hash-map slots tracked in [`QuickNodeMetrics::page_map`].
///
/// Each byte encodes the occupancy of one slot (0 = empty, 255 = full).
pub const MAX_MAP_PAGES: usize = 512;

/// Byte threshold at which a page-map slot appears visually "full" (255).
///
/// Chosen so pages take tens of seconds to saturate under typical load,
/// keeping the heat-map informative throughout a stress test.
pub const PAGE_MAP_VISUAL_FULL: u64 = 2 * 1024 * 1024; // 2 MiB per slot

/// Snapshot of a Quick-Node's operational state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickNodeMetrics {
    /// FNV-1a hash of this node's listen address.
    pub node_id: u64,
    /// Socket address this node is listening on.
    pub listen_addr: String,
    /// `true` while a graceful drain is in progress.
    pub is_draining: bool,
    /// Nodes known in the consistent-hash ring (including self).
    pub ring_size: usize,
    /// Tenant–shard partitions owned by this node.
    pub owned_partitions: usize,
    /// Cumulative writes handled since startup.
    pub write_count: u64,
    /// Cumulative reads handled since startup.
    pub read_count: u64,
    /// Seconds elapsed since this node started.
    pub uptime_secs: u64,
    /// Total bytes of write payloads processed since startup.
    pub write_bytes: u64,
    /// Theoretical heap page count derived from `write_bytes / HEAP_PAGE_SIZE`.
    pub page_count: u64,
    /// Per-page occupancy map: each byte is 0 (empty) – 255 (full).
    ///
    /// At most [`MAX_MAP_PAGES`] entries.  The last entry may be partially
    /// filled (< 255) when `write_bytes` is not a multiple of the page size.
    pub page_map: Vec<u8>,
    /// Rough in-memory footprint estimate (write_bytes + structural overhead).
    pub estimated_memory_bytes: u64,
}

/// Snapshot of a Slow-Node's operational state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlowNodeMetrics {
    /// Total versioned records in the audit store.
    pub record_count: usize,
    /// Distinct tenants with at least one record.
    pub tenant_count: usize,
    /// Cumulative flush batches applied since startup.
    pub flush_count: u64,
    /// Seconds elapsed since this node started.
    pub uptime_secs: u64,
    /// Actual journal file size in bytes (from `fs::metadata`).
    pub journal_bytes: u64,
    /// Estimated in-memory index size in bytes (key + serialized value per record).
    pub index_bytes: u64,
}
