//! Failover E2E tests.
//!
//! Verifies that the cluster topology starts correctly, that owned-partition
//! writes succeed, and that a client can reconnect to a surviving node after
//! the primary is killed.

use wavedb_net::request::RequestKind;
use wavedb_test_cluster::{ClusterSpec, TestCluster};

#[tokio::test]
async fn cluster_starts_all_nodes_alive() {
    let cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        ..Default::default()
    })
    .await;

    assert!(cluster.quick_nodes[0].is_alive());
    assert!(cluster.quick_nodes[1].is_alive());
    assert_eq!(cluster.quick_nodes.len(), 2);

    cluster.shutdown().await;
}

#[tokio::test]
async fn connect_returns_owner_url() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;

    let db = cluster.open_user(1, 42).await;
    assert!(
        !db.owner_url().is_empty(),
        "owner URL must be non-empty after connect"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn write_to_owned_partition_succeeds() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;
    // Ring decides the owner; connect there.
    let db = cluster.open_user_at_owner(1, 42).await;

    let payload = db
        .send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant: 42,
            payload: b"hello_world".to_vec(),
        })
        .await
        .unwrap();

    assert_ne!(payload, b"not_owner", "owned write must not be rejected");

    cluster.shutdown().await;
}

#[tokio::test]
async fn write_to_peer_owned_partition_rejected() {
    let cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        ..Default::default()
    })
    .await;

    // Find a tenant the ring assigns to node 1, then write it via node 0.
    let foreign = (0..10_000u64)
        .find(|&t| !cluster.quick_nodes[0].node.owns(t, 0))
        .expect("ring must give node 1 some tenants");
    let db = cluster.open_user_via(1, foreign, 0).await;

    let payload = db
        .send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant: foreign,
            payload: Vec::new(),
        })
        .await
        .unwrap();

    assert_eq!(payload, b"not_owner");

    cluster.shutdown().await;
}

#[tokio::test]
async fn kill_owner_survivor_takes_over_and_serves() {
    let mut cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        ..Default::default()
    })
    .await;
    let tenant = cluster.owned_tenant();
    let owner = cluster.owner_idx(tenant);
    let survivor = 1 - owner;

    cluster.kill_quick_node(owner).await;
    assert!(!cluster.quick_nodes[owner].is_alive());
    assert!(cluster.quick_nodes[survivor].is_alive());

    // The survivor's heartbeat evicts the corpse (100 ms × 3 strikes in
    // the harness) and ownership re-derives from the ring.
    let mut took_over = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if cluster.quick_nodes[survivor].node.owns(tenant, 0) {
            took_over = true;
            break;
        }
    }
    assert!(took_over, "survivor must inherit the dead owner's tenants");

    let db = cluster.open_user_via(1, tenant, survivor).await;
    assert_eq!(db.tenant(), tenant);

    let payload = db
        .send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant,
            payload: b"via_survivor".to_vec(),
        })
        .await
        .unwrap();
    assert_ne!(payload, b"not_owner");

    cluster.shutdown().await;
}

#[tokio::test]
async fn three_node_cluster_exactly_one_writer_per_tenant() {
    let cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 3,
        ..Default::default()
    })
    .await;
    let tenant = cluster.owned_tenant();

    // One writer, n replicas: exactly one of the three accepts the write,
    // the others bounce with a redirect.
    let mut accepted = 0u32;
    for i in 0..3 {
        let db = cluster.open_user_via(1, tenant, i).await;
        let payload = db
            .send(RequestKind::Write {
                struct_id: 1,
                user: 1,
                tenant,
                payload: format!("from_node_{i}").into_bytes(),
            })
            .await
            .unwrap();
        if payload != b"not_owner" {
            accepted += 1;
            assert_eq!(
                i,
                cluster.owner_idx(tenant),
                "the accepting node must be the ring owner"
            );
        }
    }
    assert_eq!(accepted, 1, "exactly one node may accept writes");

    cluster.shutdown().await;
}

#[tokio::test]
async fn search_unique_on_owned_partition_succeeds() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;
    // Ring decides the owner; connect there.
    let db = cluster.open_user_at_owner(1, 42).await;

    let payload = db
        .send(RequestKind::SearchUnique {
            struct_id: 1,
            user: 1,
            tenant: 42,
        })
        .await
        .unwrap();

    assert_ne!(payload, b"not_owner");

    cluster.shutdown().await;
}

#[tokio::test]
async fn disconnect_completes_cleanly() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;

    {
        let db = cluster.open_user(1, 42).await;
        let _ = db
            .send(RequestKind::Write {
                struct_id: 1,
                user: 1,
                tenant: 42,
                payload: vec![1, 2, 3],
            })
            .await
            .unwrap();
        // Db::drop fires disconnect; the server accepts it gracefully.
    }

    // Node is still alive after a client disconnects.
    assert!(cluster.quick_nodes[0].is_alive());

    cluster.shutdown().await;
}
