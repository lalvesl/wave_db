//! Slow-node interaction: incremental audit-trail flushing.
//!
//! Called periodically from the continuous load loop to push newly committed
//! writes to the Slow-Node's `/flush` endpoint.  Records are chunked into
//! batches of [`BATCH_SIZE`] to match real production behaviour.

use wavedb_monitor::{LogBuffer, push_log};
use wavedb_test_cluster::{FlushBatch, TestCluster, VersionedRecord};

use crate::TENANT;

const BATCH_SIZE: usize = 100;

/// Flush records with IDs `(from_id + 1)..=to_id` to the slow-node.
///
/// Returns the next write-sequence number to use on the following call.
/// Pass the returned value back in as `write_seq` on the next call.
pub async fn flush_incremental(
    cluster: &TestCluster,
    from_id: u64,
    to_id: u64,
    write_seq: u64,
    log: &LogBuffer,
) -> u64 {
    if to_id <= from_id {
        return write_seq;
    }

    let records: Vec<VersionedRecord> = ((from_id + 1)..=to_id)
        .map(|i| VersionedRecord::new(u128::from(i), format!("payment_audit_{i}").into_bytes()))
        .collect();

    let delta = records.len();
    let mut seq = write_seq;

    for chunk in records.chunks(BATCH_SIZE) {
        cluster
            .flush_batch(FlushBatch {
                write_seq: seq,
                tenant: TENANT,
                records: chunk.to_vec(),
                token: None,
            })
            .await;
        seq += 1;
    }

    push_log(
        log,
        format!(
            "  flush: +{delta} records → slow-node  (total {})",
            cluster.slow_node.store.len()
        ),
    );

    seq
}
