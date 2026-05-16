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
}
