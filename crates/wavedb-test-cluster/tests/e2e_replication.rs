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
    let owner = cluster.owner_idx(42);
    let db = cluster.open_user_at_owner(1, 42).await;

    let before = cluster.quick_nodes[owner].node.replication().current_seq();

    db.send(RequestKind::Write {
        struct_id: 1,
        user: 1,
        tenant: 42,
        payload: b"hello".to_vec(),
    })
    .await
    .unwrap();

    let after = cluster.quick_nodes[owner].node.replication().current_seq();
    assert!(
        after > before,
        "write must advance the local replication sequence"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn two_writes_advance_sequence_twice() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;
    let owner = cluster.owner_idx(42);
    let db = cluster.open_user_at_owner(1, 42).await;

    let before = cluster.quick_nodes[owner].node.replication().current_seq();

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

    let after = cluster.quick_nodes[owner].node.replication().current_seq();
    assert!(
        after >= before + 2,
        "two writes must advance sequence by at least 2"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn peer_owned_write_does_not_advance_sequence() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;
    // Pick a tenant node 0 does NOT own — under ring ownership some node
    // always owns every tenant, so "unowned anywhere" no longer exists;
    // the invariant is that a NON-owner rejects without minting a seq.
    let foreign = (0..10_000u64)
        .find(|&t| !cluster.quick_nodes[0].node.owns(t, 0))
        .expect("ring must give node 1 some tenants");
    let db = cluster.open_user_via(1, foreign, 0).await;

    let before = cluster.quick_nodes[0].node.replication().current_seq();

    db.send(RequestKind::Write {
        struct_id: 1,
        user: 1,
        tenant: foreign,
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

    // Write through the ring owner.
    let owner = cluster.owner_idx(42);
    let other = 1 - owner;
    let db0 = cluster.open_user_at_owner(1, 42).await;
    db0.send(RequestKind::Write {
        struct_id: 1,
        user: 1,
        tenant: 42,
        payload: b"from-owner".to_vec(),
    })
    .await
    .unwrap();

    let seq_owner = cluster.quick_nodes[owner].node.replication().current_seq();
    let seq_other = cluster.quick_nodes[other].node.replication().current_seq();

    assert!(seq_owner >= 1, "owner must have advanced its sequence");
    assert_eq!(
        seq_other, 0,
        "the non-owner minted no local writes — sequence stays at zero"
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
