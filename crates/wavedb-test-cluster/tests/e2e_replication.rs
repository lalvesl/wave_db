//! Replication watermark E2E tests.
//!
//! Verifies that the local write-sequence counter advances with each write and
//! that the per-peer send watermark is updated on the owning node.

use wavedb_net::request::RequestKind;
use wavedb_test_cluster::{ClusterSpec, TestCluster};

#[tokio::test]
async fn replication_seq_starts_at_zero() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;

    let seq = cluster.quick_nodes[0].node.replication().current_seq();
    assert_eq!(seq, 0, "no writes yet — sequence must start at zero");

    cluster.shutdown().await;
}

#[tokio::test]
async fn write_advances_replication_sequence() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;
    let db = cluster.open_user(1, 42).await;

    let before = cluster.quick_nodes[0].node.replication().current_seq();

    db.send(RequestKind::Write {
        struct_id: 1,
        user: 1,
        tenant: 42,
        payload: b"hello".to_vec(),
    })
    .await
    .unwrap();

    let after = cluster.quick_nodes[0].node.replication().current_seq();
    assert!(
        after > before,
        "write must advance the local replication sequence"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn two_writes_advance_sequence_twice() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;
    let db = cluster.open_user(1, 42).await;

    let before = cluster.quick_nodes[0].node.replication().current_seq();

    for i in 0u8..2 {
        db.send(RequestKind::Write {
            struct_id: 1,
            user: 1,
            tenant: 42,
            payload: vec![i],
        })
        .await
        .unwrap();
    }

    let after = cluster.quick_nodes[0].node.replication().current_seq();
    assert!(
        after >= before + 2,
        "two writes must advance sequence by at least 2"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn unowned_write_does_not_advance_sequence() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;
    let db = cluster.open_user(1, 42).await;

    let before = cluster.quick_nodes[0].node.replication().current_seq();

    db.send(RequestKind::Write {
        struct_id: 1,
        user: 1,
        tenant: 999, // not owned by any node in the default cluster
        payload: b"ignored".to_vec(),
    })
    .await
    .unwrap();

    let after = cluster.quick_nodes[0].node.replication().current_seq();
    assert_eq!(
        after, before,
        "rejected writes must not advance the replication sequence"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn replication_seq_is_per_node() {
    let cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        ..Default::default()
    })
    .await;

    // Write through node 0.
    let db0 = cluster.open_user_via(1, 42, 0).await;
    db0.send(RequestKind::Write {
        struct_id: 1,
        user: 1,
        tenant: 42,
        payload: b"from0".to_vec(),
    })
    .await
    .unwrap();

    let seq0 = cluster.quick_nodes[0].node.replication().current_seq();
    let seq1 = cluster.quick_nodes[1].node.replication().current_seq();

    assert!(seq0 >= 1, "node 0 must have advanced its sequence");
    assert_eq!(
        seq1, 0,
        "node 1 received no writes — sequence stays at zero"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn snapshot_includes_all_peers() {
    let cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 3,
        ..Default::default()
    })
    .await;

    let snap = cluster.quick_nodes[0].node.replication().snapshot();
    // Node 0 was configured with 2 peers (nodes 1 and 2).
    assert_eq!(snap.len(), 2, "node 0 must track exactly 2 peers");

    cluster.shutdown().await;
}
