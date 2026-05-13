//! Cold-handoff / Slow-Node flush E2E tests.
//!
//! Verifies that records flushed from a Quick-Node to the Slow-Node are
//! durably indexed and queryable through the in-process store handle.

use wavedb_storage::VersionedRecord;
use wavedb_test_cluster::{ClusterSpec, FlushBatch, TestCluster};

#[tokio::test]
async fn slow_node_store_empty_before_flush() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;

    assert!(cluster.slow_node.store.is_empty());
    assert_eq!(cluster.slow_node.store.high_water(42), 0);

    cluster.shutdown().await;
}

#[tokio::test]
async fn flush_batch_appears_in_store() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;

    let batch = FlushBatch {
        write_seq: 1,
        tenant: 42,
        records: vec![VersionedRecord::new(100, b"record_a".to_vec())],
    };
    cluster.flush_batch(batch).await;

    // Allow the HTTP request to complete and the store to update.
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    assert_eq!(
        cluster.slow_node.store.len(),
        1,
        "one record must be indexed after flush"
    );
    assert_eq!(cluster.slow_node.store.high_water(42), 1);

    cluster.shutdown().await;
}

#[tokio::test]
async fn flushed_record_is_retrievable_by_id() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;

    let record_id: u128 = 0xDEAD_BEEF;
    let batch = FlushBatch {
        write_seq: 5,
        tenant: 42,
        records: vec![VersionedRecord::new(record_id, b"payload".to_vec())],
    };
    cluster.flush_batch(batch).await;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    let stored = cluster.slow_node.store.get(42, record_id);
    assert!(
        stored.is_some(),
        "flushed record must be retrievable by (tenant, id)"
    );

    cluster.shutdown().await;
}

#[tokio::test]
async fn multiple_flushes_accumulate_records() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;

    for i in 0u128..3 {
        let batch = FlushBatch {
            write_seq: i as u64 + 1,
            tenant: 42,
            records: vec![VersionedRecord::new(i, format!("data_{i}").into_bytes())],
        };
        cluster.flush_batch(batch).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert_eq!(cluster.slow_node.store.len(), 3);
    assert_eq!(cluster.slow_node.store.high_water(42), 3);

    cluster.shutdown().await;
}

#[tokio::test]
async fn high_water_advances_monotonically() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;

    let seqs = [1u64, 5, 3, 10, 7]; // intentionally out-of-order
    for (i, &seq) in seqs.iter().enumerate() {
        let batch = FlushBatch {
            write_seq: seq,
            tenant: 42,
            records: vec![VersionedRecord::new(i as u128, vec![])],
        };
        cluster.flush_batch(batch).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // High water must equal the maximum seen, not the last received.
    assert_eq!(cluster.slow_node.store.high_water(42), 10);

    cluster.shutdown().await;
}

#[tokio::test]
async fn watermarks_are_independent_per_tenant() {
    let cluster = TestCluster::spawn(ClusterSpec {
        owned_tenant: 42,
        ..Default::default()
    })
    .await;

    cluster
        .flush_batch(FlushBatch {
            write_seq: 8,
            tenant: 42,
            records: vec![VersionedRecord::new(1, b"t42".to_vec())],
        })
        .await;
    cluster
        .flush_batch(FlushBatch {
            write_seq: 3,
            tenant: 99,
            records: vec![VersionedRecord::new(2, b"t99".to_vec())],
        })
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert_eq!(cluster.slow_node.store.high_water(42), 8);
    assert_eq!(cluster.slow_node.store.high_water(99), 3);
    assert_eq!(cluster.slow_node.store.len(), 2);

    cluster.shutdown().await;
}

#[tokio::test]
async fn unknown_record_returns_none() {
    let cluster = TestCluster::spawn(ClusterSpec::default()).await;

    // No flushes — any lookup must return None.
    assert!(cluster.slow_node.store.get(42, 0xFFFF_FFFF).is_none());

    cluster.shutdown().await;
}
