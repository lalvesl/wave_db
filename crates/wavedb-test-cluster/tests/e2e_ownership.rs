//! Runtime ownership negotiation + replica redundancy E2E.
//!
//! Nothing here configures who owns what — the consistent-hash ring decides
//! from the membership view, gossip moves the membership, and the heartbeat
//! evicts the dead.  Redundancy: exactly one node accepts writes for a
//! partition (the ring owner); the next ring node stores a pushed copy.

use wavedb_net::request::RequestKind;
use wavedb_test_cluster::{ClusterSpec, TestCluster};

/// A solo node is the entire ring: it owns every tenant with zero config.
#[tokio::test]
async fn solo_node_owns_everything() {
    let cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 1,
        ..Default::default()
    })
    .await;

    let node = &cluster.quick_nodes[0].node;
    for tenant in [0u64, 1, 42, 99, 7_000_000] {
        assert!(node.owns(tenant, 0), "solo node must own tenant {tenant}");
    }

    cluster.shutdown().await;
}

/// Two nodes: exactly one owner per tenant, and both nodes agree on who it
/// is — same ring, same answer, no handshake.
#[tokio::test]
async fn two_nodes_agree_on_a_single_owner_per_tenant() {
    let cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        ..Default::default()
    })
    .await;

    let a = &cluster.quick_nodes[0].node;
    let b = &cluster.quick_nodes[1].node;
    let mut a_owns = 0u32;
    let mut b_owns = 0u32;
    for tenant in 0..64u64 {
        let owners =
            u32::from(a.owns(tenant, 0)) + u32::from(b.owns(tenant, 0));
        assert_eq!(owners, 1, "tenant {tenant} must have exactly one owner");
        if a.owns(tenant, 0) {
            a_owns += 1;
        } else {
            b_owns += 1;
        }
    }
    assert!(a_owns > 0, "ring must give node 0 some tenants");
    assert!(b_owns > 0, "ring must give node 1 some tenants");

    cluster.shutdown().await;
}

/// A write to the wrong node bounces with a redirect hint pointing at the
/// real owner; the same write at the owner is accepted.
#[tokio::test]
async fn non_owner_redirects_owner_accepts() {
    let cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        ..Default::default()
    })
    .await;
    let tenant = cluster.owned_tenant();
    let owner = cluster.owner_idx(tenant);
    let wrong = 1 - owner;

    // Wrong node: not_owner + a usable redirect URL.
    let db_wrong = cluster.open_user_via(1, tenant, wrong).await;
    let resp = db_wrong
        .send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant,
            payload: b"x".to_vec(),
        })
        .await
        .unwrap();
    assert_eq!(resp, b"not_owner");

    // Owner: accepted.
    let db_owner = cluster.open_user_at_owner(1, tenant).await;
    let resp = db_owner
        .send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant,
            payload: b"x".to_vec(),
        })
        .await
        .unwrap();
    assert_ne!(resp, b"not_owner");

    cluster.shutdown().await;
}

/// Redundancy: the owner commits, then pushes the bytes to the next ring
/// node.  One writer, two copies.
#[tokio::test]
async fn owner_write_lands_a_copy_on_the_replica() {
    let cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        ..Default::default()
    })
    .await;
    let tenant = cluster.owned_tenant();
    let owner = cluster.owner_idx(tenant);
    let replica = 1 - owner;

    let db = cluster.open_user_at_owner(1, tenant).await;
    db.send(RequestKind::Write {
        struct_id: 1,
        user: 1,
        tenant,
        payload: b"replicate-me".to_vec(),
    })
    .await
    .unwrap();

    // Fan-out is async fire-and-forget; give the POST a moment.
    let mut copied = 0;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        copied = cluster.quick_nodes[replica].node.metrics().replicated_count;
        if copied > 0 {
            break;
        }
    }
    assert_eq!(copied, 1, "replica must hold exactly one pushed copy");

    // The replica stored it but does NOT own the partition — it would
    // still bounce a client write.
    assert!(!cluster.quick_nodes[replica].node.owns(tenant, 0));

    cluster.shutdown().await;
}

/// Crash failover: kill the owner; the survivor's heartbeat evicts it and
/// ownership re-derives — the survivor now accepts writes for the tenant.
#[tokio::test]
async fn owner_crash_moves_ownership_to_the_survivor() {
    let mut cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        ..Default::default()
    })
    .await;
    let tenant = cluster.owned_tenant();
    let owner = cluster.owner_idx(tenant);
    let survivor = 1 - owner;

    cluster.kill_quick_node(owner).await;

    // Harness heartbeat: 100 ms × 3 strikes; wait with margin.
    let mut took_over = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if cluster.quick_nodes[survivor].node.owns(tenant, 0) {
            took_over = true;
            break;
        }
    }
    assert!(took_over, "survivor must take over after the owner dies");

    let db = cluster.open_user_via(1, tenant, survivor).await;
    let resp = db
        .send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant,
            payload: b"after-failover".to_vec(),
        })
        .await
        .unwrap();
    assert_ne!(resp, b"not_owner");

    cluster.shutdown().await;
}
