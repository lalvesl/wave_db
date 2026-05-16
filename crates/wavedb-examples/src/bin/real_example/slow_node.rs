//! Slow-node interaction: audit-trail flush and persistence verification.
//!
//! The slow-node is the durable cold-tier store that persists every committed
//! write as a versioned record.  In Phase 14, `QuickNode::sync_to_slow()` will
//! batch-send these automatically on drain.  Here we do it manually via
//! `TestCluster::flush_batch` to exercise the full `/flush` HTTP path and verify
//! the slow-node audit trail.

use std::time::Duration;

use tokio::time::sleep;

use wavedb_test_cluster::{FlushBatch, TestCluster, VersionedRecord};

use crate::TENANT;

/// Flush every committed write as a durable [`VersionedRecord`] to the slow-node.
///
/// Records are chunked into batches of [`BATCH_SIZE`] — consistent with the
/// batch sizes a real production flush would use.  Returns the number of batches
/// sent.
pub async fn flush_audit_trail(cluster: &TestCluster, committed: u64) -> u64 {
    const BATCH_SIZE: usize = 50;

    // Synthetic audit records — in production these carry serialised payloads.
    let records: Vec<VersionedRecord> = (0u128..u128::from(committed))
        .map(|i| VersionedRecord::new(i + 1, format!("payment_audit_{i}").into_bytes()))
        .collect();

    println!("  Building {} audit records …", records.len());

    let mut write_seq = 1u64;
    for chunk in records.chunks(BATCH_SIZE) {
        cluster
            .flush_batch(FlushBatch {
                write_seq,
                tenant: TENANT,
                records: chunk.to_vec(),
                token: None,
            })
            .await;
        write_seq += 1;
    }

    let batch_count = write_seq - 1;
    println!("  Sent {batch_count} batches (seq 1 – {batch_count}).");

    // Brief pause for the async HTTP flush to settle in the store.
    sleep(Duration::from_millis(60)).await;

    batch_count
}

/// Verify the slow-node's audit store and print its state.
///
/// Panics if:
/// - no records were stored at all, or
/// - record count does not match committed writes, or
/// - high-water mark doesn't equal the last batch sequence.
pub fn verify_and_print(cluster: &TestCluster, committed: u64, last_batch_seq: u64) {
    let stored_count = cluster.slow_node.store.len();
    let high_water = cluster.slow_node.store.high_water(TENANT);

    println!("  Records in slow-node : {stored_count}");
    println!("  High-water mark      : {high_water}  (last acked batch seq)");

    assert!(
        stored_count > 0,
        "slow-node must have at least one audit record after flush"
    );
    assert_eq!(
        stored_count,
        committed as usize,
        "every committed write must appear in the audit log"
    );
    assert_eq!(
        high_water, last_batch_seq,
        "high-water mark must equal the last flushed batch sequence"
    );
}
