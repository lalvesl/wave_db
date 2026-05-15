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
    let db = cluster.open_user(1, 42).await;

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
async fn write_to_unowned_partition_rejected() {
    let cluster = TestCluster::spawn(ClusterSpec {
        owned_tenant: 42,
        ..Default::default()
    })
    .await;
    let db = cluster.open_user(1, 42).await;

    let payload = db
        .send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant: 999, // not owned
            payload: Vec::new(),
        })
        .await
        .unwrap();

    assert_eq!(payload, b"not_owner");

    cluster.shutdown().await;
}

#[tokio::test]
async fn kill_primary_backup_still_serves_requests() {
    let mut cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        ..Default::default()
    })
    .await;

    // Verify node 0 is alive and serving.
    assert!(cluster.quick_nodes[0].is_alive());

    // Kill node 0.
    cluster.kill_quick_node(0).await;
    assert!(!cluster.quick_nodes[0].is_alive());

    // Node 1 (backup) should still be alive.
    assert!(cluster.quick_nodes[1].is_alive());

    // A new client connected directly to node 1 should still work.
    let db = cluster.open_user_via(1, 42, 1).await;
    assert_eq!(db.tenant(), 42);
    assert_eq!(db.user(), 1);

    let payload = db
        .send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant: 42,
            payload: b"via_backup".to_vec(),
        })
        .await
        .unwrap();
    assert_ne!(payload, b"not_owner");

    cluster.shutdown().await;
}

#[tokio::test]
async fn three_node_cluster_all_accept_writes() {
    let cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 3,
        ..Default::default()
    })
    .await;

    for i in 0..3 {
        let db = cluster.open_user_via(1, 42, i).await;
        let payload = db
            .send(RequestKind::Write {
                struct_id: 1,
                user: 1,
                tenant: 42,
                payload: format!("from_node_{i}").into_bytes(),
            })
            .await
            .unwrap();
        assert_ne!(payload, b"not_owner", "node {i} must own the partition");
    }

    cluster.shutdown().await;
}

#[tokio::test]
async fn search_unique_on_owned_partition_succeeds() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;
    let db = cluster.open_user(1, 42).await;

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
