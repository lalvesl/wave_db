//! Distributed cluster demo: topology, writes, failover, and restart.
//!
//! Spins up a two-node in-process cluster using the Phase 14 test harness,
//! exercises write traffic, kills the primary node, verifies the backup
//! keeps serving requests, then restarts the primary and shuts down cleanly.
//!
//! Run with:
//!   cargo run --bin distributed_cluster

use wavedb_test_cluster::{ClusterSpec, TestCluster};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Spawn cluster ────────────────────────────────────────────────────────
    let mut cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        min_replicas: 2,
        owned_tenant: 42,
        ..Default::default()
    })
    .await;

    println!("── Topology ─────────────────────────────────────────────────────");
    for (i, node) in cluster.quick_nodes.iter().enumerate() {
        println!(
            "  node[{i}]  ws={}  alive={}",
            node.ws_url(),
            node.is_alive()
        );
    }
    println!("  owned_tenant={}", cluster.owned_tenant());

    // ── Write to node 0 ──────────────────────────────────────────────────────
    let db0 = cluster.open_user_via(1, 42, 0).await;
    use wavedb_net::request::RequestKind;
    let resp = db0
        .send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant: 42,
            payload: b"order_v1".to_vec(),
        })
        .await?;
    assert_ne!(resp, b"not_owner", "node 0 must own tenant 42");
    println!("Write to node[0]: OK");

    // ── Kill primary (node 0) ────────────────────────────────────────────────
    cluster.kill_quick_node(0).await;
    assert!(!cluster.quick_nodes[0].is_alive(), "node[0] must be down");
    println!("Killed node[0]");

    // ── Backup (node 1) still serves ─────────────────────────────────────────
    assert!(cluster.quick_nodes[1].is_alive(), "node[1] must be alive");
    let db1 = cluster.open_user_via(1, 42, 1).await;
    let resp2 = db1
        .send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant: 42,
            payload: b"order_v2".to_vec(),
        })
        .await?;
    assert_ne!(resp2, b"not_owner", "node[1] must own tenant 42");
    println!("Write to node[1] (backup): OK");

    // ── Restart node 0 ───────────────────────────────────────────────────────
    cluster.restart_quick_node(0).await;
    assert!(
        cluster.quick_nodes[0].is_alive(),
        "node[0] must be alive after restart"
    );
    println!("Restarted node[0]");

    // ── Verify both nodes alive ───────────────────────────────────────────────
    assert!(cluster.quick_nodes[0].is_alive());
    assert!(cluster.quick_nodes[1].is_alive());
    println!("Both nodes alive after restart");

    // ── Clean shutdown ────────────────────────────────────────────────────────
    cluster.shutdown().await;
    println!("distributed_cluster example OK");
    Ok(())
}
