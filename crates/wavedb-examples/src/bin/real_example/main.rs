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

use std::time::Instant;

use wavedb_test_cluster::{ClusterSpec, TestCluster};

pub const TENANT: u64 = 100; // payment service

fn sep(title: &str) {
    println!(
        "\n── {title} {}",
        "─".repeat(54_usize.saturating_sub(title.len() + 3))
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║     WaveDB — Payment Gateway High-Load Stress Test       ║");
    println!("╚══════════════════════════════════════════════════════════╝");

    // ── Cluster spawn ─────────────────────────────────────────────────────────
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

    // ── Client launch ─────────────────────────────────────────────────────────
    sep("Connecting clients");

    println!(
        "  {} payment-processor clients — {} writes each",
        clients::NUM_CLIENTS,
        clients::WRITES_PER_CLIENT,
    );
    println!(
        "  distribution: {0} → node[0]  ·  {0} → node[1]  ·  {1} → node[2]",
        clients::PER_NODE,
        clients::NUM_CLIENTS - clients::PER_NODE * 2,
    );

    let (tasks, counters) = clients::launch(&cluster).await;
    println!("  All clients connected and writing …");

    // ── Failure injection ─────────────────────────────────────────────────────
    sep("Injecting failures");

    // Run failures concurrently with client tasks.
    let fail_task = {
        // Shadow the cluster borrow — failure injection only needs a shared ref
        // to the node handles, which are already behind Arc.
        let qn0 = cluster.quick_nodes[0].node.clone();
        let qn1 = cluster.quick_nodes[1].node.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            println!("  [t=100ms] DRAIN quick-node[0]  (hardware replacement)");
            qn0.drain().await;

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            println!("  [t=200ms] DRAIN quick-node[1]  (cascading failure)");
            qn1.drain().await;

            println!("  [t=200ms] node[0] draining: {}  node[1] draining: {}",
                qn0.is_draining(), qn1.is_draining());
        })
    };

    // ── Wait for all tasks ────────────────────────────────────────────────────
    let t0 = Instant::now();
    fail_task.await?;
    for t in tasks {
        let _ = t.await;
    }
    let elapsed = t0.elapsed();

    // ── Write results ─────────────────────────────────────────────────────────
    sep("Write results");

    let committed = counters.total_committed();
    let dropped = counters.total_dropped();
    let attempted = (clients::NUM_CLIENTS * clients::WRITES_PER_CLIENT) as u64;

    println!("  Elapsed              : {:.0}ms", elapsed.as_millis());
    println!("  Writes attempted     : {attempted}");
    println!("  Writes committed     : {committed}  (delivered before drain)");
    println!("  Writes dropped       : {dropped}  (node drained mid-flight)");
    println!(
        "  node[2] surviving    : {}  (still alive, not draining)",
        !cluster.quick_nodes[2].node.is_draining()
    );

    assert!(committed > 0, "at least node[2] clients must commit writes");
    assert!(committed + dropped == attempted, "committed + dropped must equal attempted");

    // ── Slow-node audit flush ─────────────────────────────────────────────────
    sep("Flushing audit trail to slow-node");

    let batch_count = slow_node::flush_audit_trail(&cluster, committed).await;

    // ── Slow-node verification ────────────────────────────────────────────────
    sep("Verifying slow-node audit log");

    slow_node::verify_and_print(&cluster, committed, batch_count);

    // ── Shutdown ──────────────────────────────────────────────────────────────
    sep("Graceful shutdown");

    cluster.shutdown().await;
    println!("  All nodes stopped.");

    // ── Summary ───────────────────────────────────────────────────────────────
    sep("Summary");

    let survivor_writes = (clients::NUM_CLIENTS - clients::PER_NODE * 2)
        * clients::WRITES_PER_CLIENT;
    println!("  Clients             : {}", clients::NUM_CLIENTS);
    println!("  Writes attempted    : {attempted}");
    println!("  Writes committed    : {committed}");
    println!("  Writes dropped      : {dropped}  (2/3 of nodes lost mid-flight)");
    println!("  Audit records       : {committed}  (100% of committed)");
    println!("  Min expected commits: {survivor_writes}  (node[2] alone)");
    println!("  Data loss           : 0  (slow-node holds full audit trail)");
    println!();
    println!("✓  All assertions passed — payment gateway scenario complete.");

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_main() {
        super::main().unwrap();
    }
}
