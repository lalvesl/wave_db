//! Real-world scenario: payment gateway under sustained, randomised high load
//! with periodic cascading node failures.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Quick-Node[0]  ─┐                                          │
//! │  Quick-Node[1]  ─┼── tenant 100 (payment service hot data)  │
//! │  Quick-Node[2]  ─┘  (never drained — guaranteed survivor)   │
//! │        │                                                     │
//! │        ▼  flush every 10 s                                   │
//! │  Slow-Node  ── audit trail (durable versioned history)       │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Continuous scenario
//!
//! - 39 clients write indefinitely with per-client jitter (3–25 ms) and
//!   pseudo-random payload sizes (64–512 B).
//! - Every ~12–18 s a chaos step drains node[0] or node[1] (alternating),
//!   simulating a hardware replacement event.  Clients on the drained node
//!   stop; the load continues on the surviving nodes.
//! - Every 10 s committed writes are flushed to the Slow-Node audit log.
//! - The TUI monitor shows live IOps sparklines, ring-size changes, slow-node
//!   record counts, and process memory / CPU.
//!
//! Run with:
//!   cargo run --release --bin real_example
//!   nix run .#real_example

mod clients;
mod quick_node;
mod schema;
mod slow_node;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use wavedb_monitor::{LogBuffer, new_log, push_log, run_tui_thread};
use wavedb_test_cluster::{ClusterSpec, QuickNodeHandle, TestCluster};

pub const TENANT: u64 = 100;

fn sep(title: &str) {
    println!(
        "\n── {title} {}",
        "─".repeat(54_usize.saturating_sub(title.len() + 3))
    );
}

// ── Continuous scenario ───────────────────────────────────────────────────────

/// Run continuous load against `cluster` until `quit` is set to `true`.
///
/// Uses a single `tokio::select!` loop so all background tasks (checkpoint
/// logging, chaos injection, slow-node flush) share `&cluster` without
/// needing a separate Arc or Mutex.
///
/// Returns `(total_committed, total_dropped)`.
pub async fn run_continuous(
    cluster: &TestCluster,
    log: &LogBuffer,
    quit: Arc<AtomicBool>,
) -> (u64, u64) {
    // ── Connect clients ───────────────────────────────────────────────────
    push_log(
        log,
        format!(
            "  {} clients starting — {}/{}/{} on node[0/1/2] — random 64–512 B payloads",
            clients::NUM_CLIENTS,
            clients::PER_NODE,
            clients::PER_NODE,
            clients::NUM_CLIENTS - clients::PER_NODE * 2,
        ),
    );
    let (client_tasks, counters) = clients::launch_continuous(cluster).await;
    push_log(
        log,
        "  All clients connected — watch the Write IOps sparkline ↑",
    );

    // ── Loop state ────────────────────────────────────────────────────────
    let t_start = Instant::now();
    let mut flushed_count: u64 = 0;
    let mut flush_write_seq: u64 = 1;
    let mut chaos_step: usize = 0;

    let mut checkpoint = tokio::time::interval(Duration::from_secs(4));
    let mut flush_tick = tokio::time::interval(Duration::from_secs(10));
    checkpoint.tick().await; // discard immediate first tick
    flush_tick.tick().await;

    // First chaos event fires after 12 s.
    let mut chaos_at = tokio::time::Instant::now() + Duration::from_secs(12);

    // ── Main event loop ───────────────────────────────────────────────────
    loop {
        tokio::select! {
            // Quit-check: 50 ms polling so we don't spin but also don't lag.
            () = tokio::time::sleep(Duration::from_millis(50)) => {
                if quit.load(Ordering::Relaxed) { break; }
            }

            // Checkpoint: log cumulative counters every 4 s.
            _ = checkpoint.tick() => {
                if quit.load(Ordering::Relaxed) { break; }
                push_log(
                    log,
                    format!(
                        "  [{:>4}s] committed: {}  dropped: {}",
                        t_start.elapsed().as_secs(),
                        counters.total_committed(),
                        counters.total_dropped(),
                    ),
                );
            }

            // Flush: push newly committed writes to the slow-node every 10 s.
            _ = flush_tick.tick() => {
                if quit.load(Ordering::Relaxed) { break; }
                let total = counters.total_committed();
                flush_write_seq = slow_node::flush_incremental(
                    cluster,
                    flushed_count,
                    total,
                    flush_write_seq,
                    log,
                )
                .await;
                flushed_count = total;
            }

            // Chaos: drain node[0] or node[1] on a staggered schedule.
            () = tokio::time::sleep_until(chaos_at) => {
                if quit.load(Ordering::Relaxed) { break; }
                let node_idx = chaos_step % 2; // alternates 0 → 1 → 0 → …
                let node = &cluster.quick_nodes[node_idx].node;
                if !node.is_draining() {
                    push_log(
                        log,
                        format!("  ── CHAOS: DRAIN quick-node[{node_idx}]  (hardware replacement)"),
                    );
                    node.drain().await;
                    push_log(
                        log,
                        format!(
                            "  ── node[{node_idx}] draining: {}  ring shrinking…",
                            node.is_draining()
                        ),
                    );
                }
                chaos_step += 1;
                // Next chaos event: 14 s + small step-based variation.
                let next_secs = 14 + (chaos_step as u64 % 5);
                chaos_at = tokio::time::Instant::now() + Duration::from_secs(next_secs);
            }
        }
    }

    // ── Cleanup ───────────────────────────────────────────────────────────
    for t in &client_tasks {
        t.abort();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    let committed = counters.total_committed();
    let dropped = counters.total_dropped();
    push_log(
        log,
        format!("  Final — committed: {committed}  dropped: {dropped}"),
    );

    (committed, dropped)
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║     WaveDB — Payment Gateway Continuous Load Test        ║");
    println!("╚══════════════════════════════════════════════════════════╝");

    // ── Cluster spawn ─────────────────────────────────────────────────────
    sep("Spawning cluster");

    let cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 3,
        owned_tenant: TENANT,
        ..Default::default()
    })
    .await;

    quick_node::print_topology(&cluster);
    println!("  slow-node       {}", cluster.slow_node.http_url());
    println!("  tenant          {TENANT}  (payment service)");
    println!();
    println!("  Starting live monitor TUI — press  q  to stop and see summary.");

    // Brief pause so HTTP servers are up before the TUI starts polling.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ── Monitor config from live cluster ──────────────────────────────────
    let monitor_cfg = wavedb_monitor::config::Config {
        quick_node_urls: cluster
            .quick_nodes
            .iter()
            .map(QuickNodeHandle::http_url)
            .collect(),
        slow_node_urls: vec![cluster.slow_node.http_url()],
        cluster_key: None,
        refresh_ms: 400,
    };

    let log: LogBuffer = new_log();
    let done = Arc::new(AtomicBool::new(false)); // never set — TUI lives until 'q'
    let on_quit = Arc::new(AtomicBool::new(false)); // set by TUI on 'q' → stops scenario

    // ── TUI takes over stdout ─────────────────────────────────────────────
    let tui_handle = run_tui_thread(monitor_cfg, log.clone(), done, on_quit.clone());

    // ── Continuous load (returns when on_quit fires) ───────────────────────
    let t_start = Instant::now();
    let (committed, dropped) = run_continuous(&cluster, &log, on_quit).await;
    let elapsed = t_start.elapsed();

    // User pressed q → TUI already exiting → join for terminal restore.
    tui_handle.join().ok();

    // ── Summary ───────────────────────────────────────────────────────────
    sep("Summary");

    println!("  Duration            : {:.1}s", elapsed.as_secs_f64());
    println!("  Writes committed    : {committed}");
    println!("  Writes dropped      : {dropped}  (clients on drained nodes)");
    #[allow(clippy::cast_precision_loss)]
    let throughput = committed as f64 / elapsed.as_secs_f64().max(0.001);
    println!("  Throughput (avg)    : {throughput:.0} writes/s");
    println!();
    println!("✓  Continuous load test complete.");

    cluster.shutdown().await;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Headless smoke-test: spin up 12 clients (4 per Quick-Node), drive a
    /// few hundred typed writes, then shut down.
    ///
    /// Uses `launch_continuous_with(.., 12)` instead of the production
    /// `NUM_CLIENTS = 3900` so the 600 ms budget actually finishes the
    /// sequential WS-open phase.
    #[tokio::test]
    async fn test_continuous_headless() {
        let cluster = TestCluster::spawn(ClusterSpec {
            num_quick_nodes: 3,
            owned_tenant: TENANT,
            ..Default::default()
        })
        .await;

        // Per-node tempdir wiring: every Quick-Node opens its own
        // journal.log + heap.bin + data.bin under the cluster's TempDir.
        // The harness creates the three files at spawn time; assert
        // they're present before any writes.
        let data_root = cluster.data_root().to_path_buf();
        assert!(data_root.exists(), "cluster data root missing: {data_root:?}");
        for (i, qn) in cluster.quick_nodes.iter().enumerate() {
            let node_dir = data_root.join(format!("quick-{i}"));
            assert!(node_dir.exists(), "node-{i} dir missing: {node_dir:?}");
            assert!(node_dir.join("journal.log").exists(), "node-{i} journal missing");
            assert!(node_dir.join("heap.bin").exists(), "node-{i} heap missing");
            // The data file is opened lazily on first flush, so we only
            // assert the directory + journal + heap exist.
            assert_eq!(qn.storage.data_dir, node_dir);
        }

        let (client_tasks, counters) = clients::launch_continuous_with(&cluster, 12).await;
        tokio::time::sleep(Duration::from_millis(600)).await;

        for t in &client_tasks {
            t.abort();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            counters.total_committed() > 0,
            "must have at least one committed write"
        );

        // Flush each node's on-disk DataFile to materialise the snapshot.
        for qn in &cluster.quick_nodes {
            qn.storage
                .data_file
                .flush()
                .expect("flush quick-node data file");
        }

        cluster.shutdown().await;

        // Cluster drop removes the TempDir — data_root no longer exists.
        // (Verify after shutdown so the cluster handle is the last live ref.)
        assert!(
            !data_root.exists(),
            "tempdir leaked after cluster drop: {data_root:?}"
        );
    }
}
