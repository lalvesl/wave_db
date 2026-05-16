//! Continuous payment-processor client load generation.
//!
//! Each client is a tokio task that loops indefinitely, sending write requests
//! with randomised payload sizes and per-client jitter delays.  When a node
//! drains or errors the client exits its loop (its task ends naturally).
//!
//! All 39 clients are evenly distributed across the three quick-nodes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::sleep;

use wavedb_net::request::RequestKind;
use wavedb_test_cluster::TestCluster;

use crate::TENANT;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Total concurrent clients (divisible by 3 for even distribution).
pub const NUM_CLIENTS: usize = 39;
/// Clients per node (39 / 3 = 13).
pub const PER_NODE: usize = NUM_CLIENTS / 3;

// ── Counters ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Counters {
    pub committed: Arc<AtomicU64>,
    pub dropped:   Arc<AtomicU64>,
}

impl Counters {
    pub fn new() -> Self {
        Self {
            committed: Arc::new(AtomicU64::new(0)),
            dropped:   Arc::new(AtomicU64::new(0)),
        }
    }
    pub fn total_committed(&self) -> u64 { self.committed.load(Ordering::Relaxed) }
    pub fn total_dropped(&self)   -> u64 { self.dropped.load(Ordering::Relaxed) }
}

// ── Launch ────────────────────────────────────────────────────────────────────

/// Connect all [`NUM_CLIENTS`] clients and start continuous writes.
///
/// Each client task loops until:
/// - its node returns `draining` or an error (task exits), or
/// - the task is externally aborted (call `.abort()` on the handle).
///
/// Payload sizes vary between 64–512 bytes per client+write, driven by a
/// simple deterministic hash of `(user_id, seq)` — no external RNG needed.
///
/// Per-client write delay: `base_ms + jitter_ms` where both factors depend
/// on `user_id` so all 39 clients have distinct duty cycles.
pub async fn launch_continuous(cluster: &TestCluster) -> (Vec<JoinHandle<()>>, Counters) {
    let counters = Counters::new();
    let mut tasks = Vec::with_capacity(NUM_CLIENTS);

    for user_id in 0u64..NUM_CLIENTS as u64 {
        let node_idx = (user_id as usize / PER_NODE).min(2);
        let db = cluster.open_user_via(user_id + 1, TENANT, node_idx).await;
        let ctr = counters.clone();

        tasks.push(tokio::spawn(async move {
            let mut seq: u64 = 0;

            loop {
                // Deterministic pseudo-random payload size 64–512 bytes.
                let payload_len =
                    64 + ((user_id.wrapping_mul(31).wrapping_add(seq.wrapping_mul(17))) % 449)
                        as usize;

                let mut payload = format!(
                    r#"{{"payment_id":{},"user":{},"seq":{},"amount_cents":{}}}"#,
                    user_id * 100_000 + seq,
                    user_id + 1,
                    seq,
                    (seq + 1) * 100,
                )
                .into_bytes();

                // Pad with ASCII digits to reach the target size.
                let base = b'0';
                while payload.len() < payload_len {
                    payload.push(base + ((payload.len() as u8) % 10));
                }

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
                    // Node draining or transport error — this client's node is gone.
                    _ => {
                        ctr.dropped.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }

                // Per-client jitter: 3–25 ms depending on user_id and write burst phase.
                let burst = if (seq % 20) < 5 { 1u64 } else { 3 };
                let delay_ms = burst + (user_id % 7) * 2 + (seq % 5);
                sleep(Duration::from_millis(delay_ms)).await;

                seq = seq.wrapping_add(1);
            }
        }));
    }

    (tasks, counters)
}
