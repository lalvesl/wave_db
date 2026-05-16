//! Concurrent payment-processor client load generation.
//!
//! Each client is a tokio task that sends a fixed number of write requests to
//! one quick-node via WebSocket.  Writes use a per-write jitter sleep so bursts
//! stagger naturally.  When a node drains (returns `b"draining"`) or errors,
//! the client counts every undelivered write as dropped and exits.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::sleep;

use wavedb_net::request::RequestKind;
use wavedb_test_cluster::TestCluster;

use crate::TENANT;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Total concurrent clients (must be divisible by 3 for even distribution).
pub const NUM_CLIENTS: usize = 39;
/// Writes each client attempts.
pub const WRITES_PER_CLIENT: usize = 20;
/// Base sleep between writes; jitter adds up to +12 ms.
const BASE_WRITE_DELAY_MS: u64 = 13;
/// Clients per node (39 / 3 = 13).
pub const PER_NODE: usize = NUM_CLIENTS / 3;

// ── Counters ──────────────────────────────────────────────────────────────────

/// Shared write-outcome counters updated by every client task.
#[derive(Clone)]
pub struct Counters {
    /// Writes that returned a successful (non-redirect, non-drain) response.
    pub committed: Arc<AtomicU64>,
    /// Writes that were lost because the node drained or the connection failed.
    pub dropped: Arc<AtomicU64>,
}

impl Counters {
    pub fn new() -> Self {
        Self {
            committed: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn total_committed(&self) -> u64 {
        self.committed.load(Ordering::Relaxed)
    }

    pub fn total_dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

// ── Launch ────────────────────────────────────────────────────────────────────

/// Connect all [`NUM_CLIENTS`] payment-processor clients and begin sending writes.
///
/// Returns the task handles and the shared counters.  Clients are distributed
/// evenly across the three quick-nodes:
///
/// | Node   | Users      |
/// |--------|------------|
/// | node[0]| 1 – 13     |
/// | node[1]| 14 – 26    |
/// | node[2]| 27 – 39    |
pub async fn launch(cluster: &TestCluster) -> (Vec<JoinHandle<()>>, Counters) {
    let counters = Counters::new();
    let mut tasks = Vec::with_capacity(NUM_CLIENTS);

    for user_id in 0u64..NUM_CLIENTS as u64 {
        let node_idx = (user_id as usize / PER_NODE).min(2);
        let db = cluster.open_user_via(user_id + 1, TENANT, node_idx).await;

        let ctr = counters.clone();

        tasks.push(tokio::spawn(async move {
            for seq in 0u32..WRITES_PER_CLIENT as u32 {
                let payload = format!(
                    r#"{{"payment_id":{},"user":{},"seq":{},"amount_cents":{}}}"#,
                    user_id * 1000 + u64::from(seq),
                    user_id + 1,
                    seq,
                    u64::from(seq + 1) * 100,
                )
                .into_bytes();

                match db
                    .send(RequestKind::Write {
                        struct_id: 1,
                        user: user_id + 1,
                        tenant: TENANT,
                        payload,
                    })
                    .await
                {
                    Ok(resp) if resp != b"not_owner" && resp != b"draining" => {
                        ctr.committed.fetch_add(1, Ordering::Relaxed);
                    }
                    // Node draining or transport error: count this write and
                    // every remaining write for this client as dropped.
                    _ => {
                        let remaining =
                            u64::from(WRITES_PER_CLIENT as u32 - seq);
                        ctr.dropped.fetch_add(remaining, Ordering::Relaxed);
                        break;
                    }
                }

                // Jitter keeps traffic natural — avoids uniform thundering herds.
                sleep(Duration::from_millis(
                    BASE_WRITE_DELAY_MS + (user_id % 7) * 2,
                ))
                .await;
            }
        }));
    }

    (tasks, counters)
}
