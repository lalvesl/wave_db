//! In-process WaveDB cluster harness for end-to-end testing.
//!
//! Spins up Quick-Nodes and a Slow-Node as tokio tasks within the test
//! process — no subprocesses, no network configuration, no teardown scripts.
//!
//! # Usage
//!
//! ```rust,ignore
//! use wavedb_test_cluster::{TestCluster, ClusterSpec};
//!
//! #[tokio::test]
//! async fn my_e2e_test() {
//!     let cluster = TestCluster::spawn(ClusterSpec::default()).await;
//!     let db = cluster.open_user(1, 42).await;
//!     // ... exercise the cluster ...
//!     cluster.shutdown().await;
//! }
//! ```

#![deny(unsafe_op_in_unsafe_fn)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::AbortHandle;

use wavedb::Db;
use wavedb_net::WsClient;
use wavedb_quick_node::{
    config::{Config as QuickConfig, OwnershipSpec},
    node::QuickNode,
    server as quick_server,
};
use wavedb_slow_node::{server as slow_server, store::HistoryStore};

// Re-export the most commonly needed test types so integration-test files
// can write `use wavedb_test_cluster::*` without importing sub-crates.
pub use wavedb_slow_node::flush::{FlushAck, FlushBatch, ReceiptWatermark};
pub use wavedb_storage::VersionedRecord;

// ── ClusterSpec ──────────────────────────────────────────────────────────────

/// Configuration for a [`TestCluster`].
pub struct ClusterSpec {
    /// Number of Quick-Nodes to spawn (minimum 1).
    pub num_quick_nodes: usize,
    /// Minimum replicas (informational; actual replication is stubbed).
    pub min_replicas: usize,
    /// HTTP idle-tick interval passed through to clients using HTTP transport.
    pub http_poll_interval: Duration,
    /// If `true`, restrict tests to the HTTP transport (no WebSocket).
    pub force_http: bool,
    /// The tenant that every Quick-Node in this cluster will own.
    pub owned_tenant: u64,
}

impl Default for ClusterSpec {
    fn default() -> Self {
        Self {
            num_quick_nodes: 2,
            min_replicas: 2,
            http_poll_interval: Duration::from_secs(1),
            force_http: false,
            owned_tenant: 42,
        }
    }
}

// ── QuickNodeHandle ──────────────────────────────────────────────────────────

/// Handle to a running in-process Quick-Node.
pub struct QuickNodeHandle {
    /// Bound socket address.
    pub addr: SocketAddr,
    /// Shared Quick-Node state.
    ///
    /// Use this to inspect replication watermarks, ownership maps, and other
    /// runtime state without going through the network.
    pub node: Arc<QuickNode>,
    abort: AbortHandle,
}

impl QuickNodeHandle {
    /// HTTP base URL, e.g. `http://127.0.0.1:PORT`.
    pub fn http_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// WebSocket URL, e.g. `ws://127.0.0.1:PORT/ws`.
    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    /// Returns `true` if the server task is still running.
    pub fn is_alive(&self) -> bool {
        !self.abort.is_finished()
    }
}

// ── SlowNodeHandle ───────────────────────────────────────────────────────────

/// Handle to a running in-process Slow-Node.
pub struct SlowNodeHandle {
    /// Bound socket address.
    pub addr: SocketAddr,
    /// Direct access to the in-memory history store.
    ///
    /// Use this to assert on stored records without going through the network.
    pub store: HistoryStore,
    abort: AbortHandle,
}

impl SlowNodeHandle {
    /// HTTP base URL for the Slow-Node.
    pub fn http_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

// ── TestCluster ──────────────────────────────────────────────────────────────

/// An in-process WaveDB cluster for end-to-end testing.
///
/// All nodes run as tokio tasks and communicate over loopback TCP.
/// Call [`TestCluster::spawn`] to create a cluster and [`TestCluster::shutdown`]
/// to stop all nodes when the test is done.
pub struct TestCluster {
    /// All running Quick-Nodes, indexed in spawning order.
    pub quick_nodes: Vec<QuickNodeHandle>,
    /// The Slow-Node receiving history flushes.
    pub slow_node: SlowNodeHandle,
    /// WebSocket URL of `quick_nodes[0]`.
    pub front_door: String,
    /// Resolved configuration used to spawn this cluster.
    owned_tenant: u64,
}

impl TestCluster {
    /// Spawn a cluster configured by `spec`.
    ///
    /// All nodes listen on `127.0.0.1:0` (random ports). This method waits
    /// for all servers to be ready before returning.
    pub async fn spawn(spec: ClusterSpec) -> Self {
        assert!(spec.num_quick_nodes >= 1, "need at least one Quick-Node");

        // ── Slow-Node ──────────────────────────────────────────────────────
        let slow_store = HistoryStore::in_memory();
        let slow_app = slow_server::router(slow_store.clone());
        let slow_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let slow_addr = slow_listener.local_addr().unwrap();
        let slow_task = tokio::spawn(async move {
            axum::serve(slow_listener, slow_app).await.unwrap();
        });

        // Pre-bind all Quick-Node ports so every node knows its peers' addresses
        // before any server starts accepting connections.
        let mut listeners: Vec<TcpListener> = Vec::new();
        let mut addrs: Vec<SocketAddr> = Vec::new();
        for _ in 0..spec.num_quick_nodes {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push(listener.local_addr().unwrap());
            listeners.push(listener);
        }

        // ── Quick-Nodes ────────────────────────────────────────────────────
        let mut quick_nodes: Vec<QuickNodeHandle> = Vec::new();
        for (i, (listener, addr)) in listeners.into_iter().zip(addrs.iter().copied()).enumerate() {
            let peers: Vec<String> = addrs
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, a)| a.to_string())
                .collect();

            let config = QuickConfig {
                listen: addr.to_string(),
                peers: peers.join(","),
                slow_node: Some(slow_addr.to_string()),
                owns: vec![OwnershipSpec {
                    tenant: spec.owned_tenant,
                    shard_start: 0,
                    shard_end: 4095,
                }],
                bloom_interval_secs: 60,
                data_dir: PathBuf::from("/tmp"),
            };

            let node = Arc::new(QuickNode::new(config));
            let app = quick_server::router((*node).clone());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            quick_nodes.push(QuickNodeHandle {
                addr,
                node,
                abort: task.abort_handle(),
            });
        }

        // Brief pause so all server tasks begin accepting connections.
        tokio::time::sleep(Duration::from_millis(30)).await;

        let front_door = quick_nodes[0].ws_url();
        let owned_tenant = spec.owned_tenant;

        Self {
            quick_nodes,
            slow_node: SlowNodeHandle {
                addr: slow_addr,
                store: slow_store,
                abort: slow_task.abort_handle(),
            },
            front_door,
            owned_tenant,
        }
    }

    // ── Client helpers ────────────────────────────────────────────────────

    /// Open a [`Db`] connected to `quick_nodes[0]` via WebSocket.
    pub async fn open_user(&self, user_id: u64, tenant: u64) -> Db {
        self.open_user_via(user_id, tenant, 0).await
    }

    /// Open a [`Db`] connected to `quick_nodes[node_idx]` via WebSocket.
    pub async fn open_user_via(&self, user_id: u64, tenant: u64, node_idx: usize) -> Db {
        let ws_url = self.quick_nodes[node_idx].ws_url();
        let (client, read_loop) = WsClient::connect(ws_url)
            .await
            .expect("WsClient::connect failed");
        tokio::spawn(read_loop);
        Db::open_with_transport(client, user_id, tenant)
            .await
            .expect("Db::open_with_transport failed")
    }

    // ── Node lifecycle ────────────────────────────────────────────────────

    /// Abort the tokio task for `quick_nodes[idx]`.
    ///
    /// The node's TCP listener drops immediately; existing connections will
    /// receive errors on their next send.
    pub async fn kill_quick_node(&mut self, idx: usize) {
        self.quick_nodes[idx].abort.abort();
        // Brief pause so the OS reclaims the port.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    /// Restart the quick-node at `idx` on a fresh random port.
    ///
    /// The new node owns the same tenant but starts with no in-memory state.
    /// Its address in `self.quick_nodes[idx].addr` is updated to the new port.
    pub async fn restart_quick_node(&mut self, idx: usize) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let new_addr = listener.local_addr().unwrap();

        let config = QuickConfig {
            listen: new_addr.to_string(),
            peers: String::new(),
            slow_node: Some(self.slow_node.addr.to_string()),
            owns: vec![OwnershipSpec {
                tenant: self.owned_tenant,
                shard_start: 0,
                shard_end: 4095,
            }],
            bloom_interval_secs: 60,
            data_dir: PathBuf::from("/tmp"),
        };

        let node = Arc::new(QuickNode::new(config));
        let app = quick_server::router((*node).clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        self.quick_nodes[idx] = QuickNodeHandle {
            addr: new_addr,
            node,
            abort: task.abort_handle(),
        };

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // ── Slow-Node helpers ─────────────────────────────────────────────────

    /// POST a [`FlushBatch`] directly to the Slow-Node's `/flush` endpoint.
    ///
    /// Use this to simulate the history-flush that a Quick-Node would
    /// perform after accumulating writes, without requiring the full
    /// replication path to be implemented.
    pub async fn flush_batch(&self, batch: FlushBatch) {
        let body = wavedb_net::frame::encode_payload(&batch).expect("FlushBatch encode failed");
        reqwest::Client::new()
            .post(format!("{}/flush", self.slow_node.http_url()))
            .header("Content-Type", "application/octet-stream")
            .body(body.to_vec())
            .send()
            .await
            .expect("flush HTTP request failed");
    }

    // ── Synchronisation helpers ───────────────────────────────────────────

    /// Wait for replication to propagate across all Quick-Nodes.
    ///
    /// Since peer fan-out is currently a stub (write-sequence tracking only),
    /// this is a short sleep.  Replace with a watermark-polling loop once
    /// real peer fan-out is implemented.
    pub async fn wait_for_replication(&self) {
        tokio::time::sleep(Duration::from_millis(60)).await;
    }

    // ── Accessors ─────────────────────────────────────────────────────────

    /// The tenant that all Quick-Nodes in this cluster own.
    pub fn owned_tenant(&self) -> u64 {
        self.owned_tenant
    }

    // ── Teardown ──────────────────────────────────────────────────────────

    /// Abort all server tasks and wait briefly for TCP to drain.
    pub async fn shutdown(self) {
        for qn in &self.quick_nodes {
            qn.abort.abort();
        }
        self.slow_node.abort.abort();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
