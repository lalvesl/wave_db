//! Slow-node interaction: audit-trail flush and persistence verification.

use std::time::Duration;

use tokio::time::sleep;
use wavedb_monitor::{LogBuffer, push_log};
use wavedb_test_cluster::{FlushBatch, TestCluster, VersionedRecord};

use crate::TENANT;

/// Flush every committed write as a durable [`VersionedRecord`] to the slow-node.
///
/// Records are chunked into batches of [`BATCH_SIZE`].  Returns the number of
/// batches sent.  All progress messages go to `log`.
pub async fn flush_audit_trail(cluster: &TestCluster, committed: u64, log: &LogBuffer) -> u64 {
    const BATCH_SIZE: usize = 50;

    let records: Vec<VersionedRecord> = (0u128..u128::from(committed))
        .map(|i| VersionedRecord::new(i + 1, format!("payment_audit_{i}").into_bytes()))
        .collect();

    push_log(log, format!("  Building {} audit records…", records.len()));

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
    push_log(log, format!("  Sent {batch_count} batches (seq 1–{batch_count})."));

    // Brief pause for the async HTTP flush to settle.
    sleep(Duration::from_millis(60)).await;

    batch_count
}

/// Verify the slow-node's audit store and log its state.
///
/// Panics if the record count does not match `committed` or the high-water
/// mark does not equal `last_batch_seq`.
pub fn verify_and_print(
    cluster: &TestCluster,
    committed: u64,
    last_batch_seq: u64,
    log: &LogBuffer,
) {
    let stored_count = cluster.slow_node.store.len();
    let high_water = cluster.slow_node.store.high_water(TENANT);

    push_log(log, format!("  Records in slow-node : {stored_count}"));
    push_log(log, format!("  High-water mark      : {high_water}  (last acked batch seq)"));

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

    push_log(log, "  ✓ Audit log verified — 0 data loss".to_string());
}
