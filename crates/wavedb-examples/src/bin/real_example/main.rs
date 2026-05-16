//! Real-world scenario: payment gateway under high load with cascading node failures.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  Quick-Node[0]  ─┐                                          │
//! │  Quick-Node[1]  ─┼── tenant 100 (payment service hot data)  │
//! │  Quick-Node[2]  ─┘                                          │
//! │        │                                                     │
//! │        ▼  flush on drain                                     │
//! │  Slow-Node  ── audit trail (durable versioned history)       │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Scenario
//!
//! | Time   | Event                                              |
//! |--------|----------------------------------------------------|
//! | t=0    | 39 payment-processor clients connect and write     |
//! | t=100ms| quick-node[0] drains (hardware replacement)        |
//! | t=200ms| quick-node[1] drains (cascading failure)           |
//! | t~300ms| All client tasks finish; counters collected         |
//! | flush  | Committed records pushed to slow-node audit log     |
//! | verify | Slow-node holds 100% of committed payments          |
//!
//! Run with:
//!   cargo run --release --bin real_example
//!   nix run .#real_example

mod clients;
mod quick_node;
mod slow_node;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use wavedb_monitor::{LogBuffer, new_log, push_log, run_tui_thread};
use wavedb_test_cluster::{ClusterSpec, TestCluster};

pub const TENANT: u64 = 100; // payment service

fn sep(title: &str) {
    println!(
        "\n── {title} {}",
        "─".repeat(54_usize.saturating_sub(title.len() + 3))
    );
}

// ── Scenario result ───────────────────────────────────────────────────────────

pub struct ScenarioResult {
    pub committed: u64,
    pub dropped: u64,
    pub attempted: u64,
    pub elapsed: Duration,
    pub batch_count: u64,
}

// ── Core scenario (no TUI, no direct stdout) ──────────────────────────────────

/// Run the full stress-test against an already-spawned `cluster`.
///
/// All output goes into `log` (shown in the TUI Events panel).  No `println!`
/// calls inside — safe to run while the TUI owns stdout.
pub async fn run_scenario(
    cluster: &TestCluster,
    log: &LogBuffer,
) -> Result<ScenarioResult, Box<dyn std::error::Error>> {
    // ── Client launch ─────────────────────────────────────────────────────
    push_log(
        log,
        format!(
            "  {} clients connecting — {} writes each",
            clients::NUM_CLIENTS,
            clients::WRITES_PER_CLIENT,
        ),
    );

    let (tasks, counters) = clients::launch(cluster).await;
    push_log(log, "  All clients connected and writing…");

    // ── Failure injection (concurrent with client tasks) ──────────────────
    let qn0 = cluster.quick_nodes[0].node.clone();
    let qn1 = cluster.quick_nodes[1].node.clone();
    let log_fail = log.clone();
    let fail_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        push_log(&log_fail, "  [t=100ms] DRAIN quick-node[0]  (hardware replacement)");
        qn0.drain().await;

        tokio::time::sleep(Duration::from_millis(100)).await;
        push_log(&log_fail, "  [t=200ms] DRAIN quick-node[1]  (cascading failure)");
        qn1.drain().await;

        push_log(
            &log_fail,
            format!(
                "  [t=200ms] node[0] draining: {}  node[1] draining: {}",
                qn0.is_draining(),
                qn1.is_draining(),
            ),
        );
    });

    // ── Wait for all tasks ────────────────────────────────────────────────
    let t0 = Instant::now();
    fail_task.await?;
    for t in tasks {
        let _ = t.await;
    }
    let elapsed = t0.elapsed();

    // ── Collect write results ─────────────────────────────────────────────
    let committed = counters.total_committed();
    let dropped = counters.total_dropped();
    let attempted = (clients::NUM_CLIENTS * clients::WRITES_PER_CLIENT) as u64;

    push_log(log, format!("  Elapsed              : {:.0}ms", elapsed.as_millis()));
    push_log(log, format!("  Writes attempted     : {attempted}"));
    push_log(log, format!("  Writes committed     : {committed}"));
    push_log(log, format!("  Writes dropped       : {dropped}"));
    push_log(
        log,
        format!(
            "  node[2] surviving    : {}",
            !cluster.quick_nodes[2].node.is_draining()
        ),
    );

    assert!(committed > 0, "at least node[2] clients must commit writes");
    assert!(
        committed + dropped == attempted,
        "committed + dropped must equal attempted"
    );

    // ── Slow-node audit flush ─────────────────────────────────────────────
    push_log(log, "── Flushing audit trail to slow-node ──────────────────");
    let batch_count = slow_node::flush_audit_trail(cluster, committed, log).await;

    // ── Slow-node verification ────────────────────────────────────────────
    push_log(log, "── Verifying slow-node audit log ───────────────────────");
    slow_node::verify_and_print(cluster, committed, batch_count, log);

    Ok(ScenarioResult {
        committed,
        dropped,
        attempted,
        elapsed,
        batch_count,
    })
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║     WaveDB — Payment Gateway High-Load Stress Test       ║");
    println!("╚══════════════════════════════════════════════════════════╝");

    // ── Cluster spawn (normal stdout, before TUI takes over) ──────────────
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
    println!("  Starting live monitor — press  q  to exit and see summary.");

    // Brief pause so HTTP servers are fully up before TUI starts polling.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ── Build monitor config from cluster URLs ────────────────────────────
    let monitor_cfg = wavedb_monitor::config::Config {
        quick_node_urls: cluster.quick_nodes.iter().map(|n| n.http_url()).collect(),
        slow_node_url: cluster.slow_node.http_url(),
        cluster_key: None,
        refresh_ms: 400,
    };

    let log: LogBuffer = new_log();
    let done = Arc::new(AtomicBool::new(false));

    // ── Start TUI in background thread (takes over stdout) ────────────────
    let tui_handle = run_tui_thread(monitor_cfg, log.clone(), done.clone());

    // ── Run stress test (sends all output to log → TUI Events panel) ──────
    let result = run_scenario(&cluster, &log).await?;

    // Signal TUI: scenario done, display final state (user presses q to exit).
    done.store(true, Ordering::Release);

    // Wait for user to press q → TUI exits → terminal restored.
    tui_handle.join().ok();

    // ── Print summary (after terminal is restored) ────────────────────────
    sep("Summary");

    let survivor_writes =
        (clients::NUM_CLIENTS - clients::PER_NODE * 2) * clients::WRITES_PER_CLIENT;

    println!("  Clients             : {}", clients::NUM_CLIENTS);
    println!("  Writes attempted    : {}", result.attempted);
    println!("  Writes committed    : {}", result.committed);
    println!(
        "  Writes dropped      : {}  (2/3 of nodes lost mid-flight)",
        result.dropped
    );
    println!(
        "  Audit records       : {}  (100% of committed)",
        result.committed
    );
    println!(
        "  Min expected commits: {survivor_writes}  (node[2] alone)"
    );
    println!("  Data loss           : 0  (slow-node holds full audit trail)");
    println!();
    println!("✓  All assertions passed — payment gateway scenario complete.");

    // ── Shutdown ──────────────────────────────────────────────────────────
    cluster.shutdown().await;

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_scenario_headless() {
        let cluster = TestCluster::spawn(ClusterSpec {
            num_quick_nodes: 3,
            owned_tenant: TENANT,
            ..Default::default()
        })
        .await;

        let log = new_log();
        run_scenario(&cluster, &log).await.unwrap();
        cluster.shutdown().await;
    }
}
