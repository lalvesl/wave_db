//! Shared metric types exchanged between nodes and the monitor TUI.
//!
//! The monitor sends a [`MetricsRequest`] to each node's `POST /metrics`
//! endpoint and receives back either a [`QuickNodeMetrics`] or a
//! [`SlowNodeMetrics`], both wire-encoded.

use wavedb_macros::WaveWire;

use crate::auth::NodeToken;

/// Body of a `POST /metrics` request.
#[derive(Debug, Clone, WaveWire)]
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
pub const MAX_MAP_PAGES: usize = 512;

/// Snapshot of a Quick-Node's operational state.
#[derive(Debug, Clone, WaveWire)]
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
    /// Per-slot traffic map, scaled relative to the busiest slot at
    /// snapshot time: 0 = no bytes ever, 255 = the hottest slot, and any
    /// slot with traffic reports at least 1.  A fixed byte threshold would
    /// truncate light traffic to all-zeros and render the map blank.
    ///
    /// At most [`MAX_MAP_PAGES`] entries.
    pub page_map: Vec<u8>,
    /// Rough in-memory footprint estimate (write_bytes + structural overhead).
    pub estimated_memory_bytes: u64,
    /// `true` when the node has on-disk storage attached.  `false` means
    /// everything lives in RAM (no journal / heap / data files on disk).
    pub has_storage: bool,
    /// Size of the WAL journal file in bytes (0 when in-memory).
    pub journal_bytes: u64,
    /// Size of the heap blob file in bytes (0 when in-memory).
    pub heap_bytes: u64,
    /// Size of the data page file in bytes (0 when in-memory).
    pub data_file_bytes: u64,
    /// Total number of pages in the DataFile hash table (allocated, not all
    /// necessarily occupied).  0 when in-memory.
    pub data_file_page_count: u64,
    /// Pages in the DataFile hash table that contain at least one entry.
    /// `data_file_used_pages / data_file_page_count` gives the fill ratio.
    /// 0 when in-memory.
    pub data_file_used_pages: u64,
    /// Writes rejected by schema enforcement (unknown header, malformed
    /// payload, `validate` / `preprocess` hook failure).  Always 0 on
    /// schema-blind nodes (no registry attached).
    pub rejected_count: u64,
    /// Records stored on this node as replica copies pushed by partition
    /// owners (`POST /replicate`).
    pub replicated_count: u64,
}

/// Snapshot of a Slow-Node's operational state.
#[derive(Debug, Clone, WaveWire)]
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
