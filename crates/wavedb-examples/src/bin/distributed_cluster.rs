//! Distributed cluster demo: ring-derived ownership, crash failover, restart.
//!
//! Spins up a two-node in-process cluster.  Partition ownership is **not
//! configured** — the consistent-hash ring decides which node owns the
//! tenant.  The demo writes via the ring owner, kills it, waits for the
//! survivor's heartbeat to evict the corpse (3 missed announces), proves
//! ownership re-derived to the survivor, then restarts the dead node.
//!
//! Run with:
//!   cargo run --bin distributed_cluster

use wavedb_test_cluster::{ClusterSpec, TestCluster};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use wavedb_net::request::RequestKind;

    // ── Spawn cluster ────────────────────────────────────────────────────────
    let mut cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        min_replicas: 2,
        owned_tenant: 42,
        ..Default::default()
    })
    .await;

    println!(
        "── Topology ─────────────────────────────────────────────────────"
    );
    for (i, node) in cluster.quick_nodes.iter().enumerate() {
        println!(
            "  node[{i}]  ws={}  alive={}",
            node.ws_url(),
            node.is_alive()
        );
    }
    println!("  owned_tenant={}", cluster.owned_tenant());

    // ── Write via the ring owner ────────────────────────────────────────────
    // Nobody configured who owns tenant 42 — the ring decided at spawn.
    let owner = cluster.owner_idx(42);
    let backup = 1 - owner;
    println!("ring owner of tenant 42: node[{owner}], backup: node[{backup}]");

    let db0 = cluster.open_user_at_owner(1, 42).await;
    let resp = db0
        .send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant: 42,
            payload: b"order_v1".to_vec(),
        })
        .await?;
    assert_ne!(resp, b"not_owner", "ring owner must accept the write");
    println!("Write to node[{owner}] (owner): OK");

    // ── Kill the owner ───────────────────────────────────────────────────────
    cluster.kill_quick_node(owner).await;
    assert!(!cluster.quick_nodes[owner].is_alive(), "owner must be down");
    println!("Killed node[{owner}]");

    // ── Crash failover: survivor evicts the corpse, takes ownership ─────────
    // Heartbeat is 100 ms × 3 strikes in the harness; wait with margin.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(
        cluster.quick_nodes[backup].node.owns(42, 0),
        "survivor must take over the dead owner's partitions"
    );

    let db1 = cluster.open_user_via(1, 42, backup).await;
    let resp2 = db1
        .send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant: 42,
            payload: b"order_v2".to_vec(),
        })
        .await?;
    assert_ne!(resp2, b"not_owner", "survivor must accept the write");
    println!("Write to node[{backup}] (new owner after failover): OK");

    // ── Restart the dead node ───────────────────────────────────────────────
    cluster.restart_quick_node(owner).await;
    assert!(
        cluster.quick_nodes[owner].is_alive(),
        "node must be alive after restart"
    );
    println!("Restarted node[{owner}]");

    // ── Verify both nodes alive ───────────────────────────────────────────────
    assert!(cluster.quick_nodes[0].is_alive());
    assert!(cluster.quick_nodes[1].is_alive());
    println!("Both nodes alive after restart");

    // ── Clean shutdown ────────────────────────────────────────────────────────
    cluster.shutdown().await;
    println!("distributed_cluster example OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_main() {
        super::main().unwrap();
    }
}
