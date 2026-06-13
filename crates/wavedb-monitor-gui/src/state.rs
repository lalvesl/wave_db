//! Shared state between the poll worker thread and the egui render loop,
//! plus the command channel the GUI uses to drive the worker.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use wavedb_net::browse::{RecordSummary, TenantSummary};
use wavedb_net::metrics::{QuickNodeMetrics, SlowNodeMetrics};
use wavedb_storage::VersionedRecord;

/// Length of the IOps history ring buffers (one sample per poll tick).
pub const HISTORY_LEN: usize = 120;

// ── Node state ────────────────────────────────────────────────────────────────

/// Which tier a monitored node belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Quick,
    Slow,
}

/// Outcome of the most recent poll against one node.
///
/// Distinguishes auth failures from network failures so the GUI can say
/// "wrong key" instead of a generic error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeHealth {
    /// Not polled yet.
    #[default]
    Unknown,
    /// Metrics decoded fine on the last poll.
    Ok,
    /// Node answered HTTP 403 — cluster key missing or wrong.
    Unauthorized,
    /// Connection failed (down, unreachable, refused).
    Unreachable,
    /// Node answered but the payload didn't decode.
    BadResponse,
}

impl NodeHealth {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "…",
            Self::Ok => "OK",
            Self::Unauthorized => "AUTH",
            Self::Unreachable => "DOWN",
            Self::BadResponse => "BAD",
        }
    }
}

/// Live view of one monitored node.
#[derive(Debug, Clone)]
pub struct NodeState {
    pub kind: NodeKind,
    pub url: String,
    pub health: NodeHealth,
    pub quick: Option<QuickNodeMetrics>,
    pub slow: Option<SlowNodeMetrics>,
}

impl NodeState {
    pub const fn new(kind: NodeKind, url: String) -> Self {
        Self {
            kind,
            url,
            health: NodeHealth::Unknown,
            quick: None,
            slow: None,
        }
    }

    /// Short display address: strips the `http://` scheme.
    pub fn short_addr(&self) -> &str {
        self.url
            .strip_prefix("http://")
            .or_else(|| self.url.strip_prefix("https://"))
            .unwrap_or(&self.url)
    }
}

// ── Data explorer state ───────────────────────────────────────────────────────

/// A record payload fetched through `/history` for inspection.
#[derive(Debug, Clone)]
pub struct RecordDetail {
    pub id: u128,
    /// Raw `/history` response bytes (serialized [`VersionedRecord`]).
    pub raw: Vec<u8>,
    /// Decoded form, when the bytes parse.
    pub decoded: Option<VersionedRecord>,
}

/// Browse results from one Slow-Node, displayed in the Data tab.
#[derive(Debug, Clone, Default)]
pub struct DataView {
    /// Index into the slow-node subset of [`Shared::nodes`] this view came from.
    pub source_slow_idx: usize,
    pub tenants: Vec<TenantSummary>,
    pub selected_tenant: Option<u64>,
    pub records: Vec<RecordSummary>,
    pub truncated: bool,
    pub detail: Option<RecordDetail>,
    /// Human-readable error from the last browse/fetch, if any.
    pub last_error: Option<String>,
    /// Monotonic counter bumped on every applied browse response — lets the
    /// GUI know fresh data arrived.
    pub generation: u64,
}

// ── Shared snapshot ───────────────────────────────────────────────────────────

/// Everything the GUI renders, updated in place by the poll worker.
pub struct Shared {
    pub nodes: Vec<NodeState>,
    /// Cluster-wide write-IOps deltas, one per poll tick.
    pub write_history: VecDeque<u64>,
    /// Cluster-wide read-IOps deltas, one per poll tick.
    pub read_history: VecDeque<u64>,
    pub prev_writes: u64,
    pub prev_reads: u64,
    pub data: DataView,
    /// `true` when the worker currently holds a cluster key.
    pub auth_enabled: bool,
    pub poll_count: u64,
}

impl Shared {
    pub fn new(quick_urls: &[String], slow_urls: &[String]) -> Self {
        let nodes = quick_urls
            .iter()
            .map(|u| NodeState::new(NodeKind::Quick, u.clone()))
            .chain(
                slow_urls
                    .iter()
                    .map(|u| NodeState::new(NodeKind::Slow, u.clone())),
            )
            .collect();
        Self {
            nodes,
            write_history: VecDeque::from(vec![0; HISTORY_LEN]),
            read_history: VecDeque::from(vec![0; HISTORY_LEN]),
            prev_writes: 0,
            prev_reads: 0,
            data: DataView::default(),
            auth_enabled: false,
            poll_count: 0,
        }
    }
}

/// Handle shared between the GUI thread and the poll worker.
pub type SharedHandle = Arc<Mutex<Shared>>;

// ── GUI → worker commands ─────────────────────────────────────────────────────

/// Commands sent from the GUI thread to the poll worker.
#[derive(Debug)]
pub enum Command {
    /// Replace the cluster key (`None` clears it — open/dev mode).
    SetClusterKey(Option<wavedb_net::auth::ClusterKey>),
    /// Browse a Slow-Node: tenant list, plus records when `tenant` is `Some`.
    Browse {
        slow_idx: usize,
        tenant: Option<u64>,
    },
    /// Fetch one record's payload from a Slow-Node via `/history`.
    FetchRecord {
        slow_idx: usize,
        tenant: u64,
        id: u128,
    },
    /// Change the metrics poll interval.
    SetRefreshMs(u64),
}
