//! Shared performance workloads for WaveDB's storage engine.
//!
//! These functions are the single source of truth for *what* is measured, used
//! by both the statistical criterion benches (`benches/throughput.rs`) and the
//! git-tracked recorder (`src/bin/record_perf.rs`).  Keeping them here means the
//! number you watch over time and the number criterion reports come from the
//! exact same code path.
//!
//! Everything targets the storage layer directly (no network, no async) so the
//! measurements are deterministic and reflect engine performance rather than
//! transport or scheduler noise.

#![deny(unsafe_op_in_unsafe_fn)]

use std::time::{Duration, Instant};

use serde::Serialize;
use tempfile::TempDir;
use wavedb_core::Id;
use wavedb_storage::file::data::DEFAULT_PAGE_SIZE;
use wavedb_storage::{DataFile, NodeStorage, VersionedRecord};

/// Fixed tenant used by every workload so record Ids are reconstructable.
pub const TENANT: u64 = 42;
/// Fixed struct family used by every workload.
pub const STRUCT_ID: u32 = 7;
/// Default record payload size in bytes.
pub const DEFAULT_PAYLOAD_LEN: usize = 128;

/// Build the canonical Id for the `seq`-th record of a workload (1-based).
#[must_use]
pub const fn record_id(seq: u64) -> Id {
    Id::new(TENANT, 0, STRUCT_ID, seq)
}

/// Build the `seq`-th versioned record with a `payload_len`-byte body.
#[must_use]
pub fn make_record(seq: u64, payload_len: usize) -> VersionedRecord {
    let fill = u8::try_from(seq % 251).unwrap_or(0);
    VersionedRecord::new(record_id(seq).raw(), vec![fill; payload_len])
}

/// One measured performance data point.
///
/// Serialized as one JSON object per line into the git-tracked history file.
#[derive(Debug, Clone, Serialize)]
pub struct PerfSample {
    /// Workload name, e.g. `"in_memory_write"`.
    pub scenario: String,
    /// Number of records involved.
    pub records: u64,
    /// Per-record payload size in bytes.
    pub payload_len: usize,
    /// Wall-clock time for the measured operation.
    pub elapsed_secs: f64,
    /// Records processed per second.
    pub throughput_per_sec: f64,
    /// Payload megabytes processed per second (MiB = 1024 × 1024 B).
    pub mib_per_sec: f64,
    /// On-disk bytes after the workload (data.bin + journal.log + heap.bin),
    /// or 0 for in-memory workloads.
    pub disk_bytes: u64,
    /// On-disk bytes per record, or 0 for in-memory workloads.
    pub disk_bytes_per_record: f64,
}

impl PerfSample {
    fn new(
        scenario: impl Into<String>,
        records: u64,
        payload_len: usize,
        elapsed: Duration,
        disk_bytes: u64,
    ) -> Self {
        let elapsed_secs = elapsed.as_secs_f64();
        #[allow(clippy::cast_precision_loss)]
        let records_f = records as f64;
        let throughput_per_sec = if elapsed_secs > 0.0 {
            records_f / elapsed_secs
        } else {
            0.0
        };
        #[allow(clippy::cast_precision_loss)]
        let payload_total_mib = (records_f * payload_len as f64) / (1024.0 * 1024.0);
        let mib_per_sec = if elapsed_secs > 0.0 {
            payload_total_mib / elapsed_secs
        } else {
            0.0
        };
        #[allow(clippy::cast_precision_loss)]
        let disk_bytes_per_record = if records > 0 {
            disk_bytes as f64 / records_f
        } else {
            0.0
        };
        Self {
            scenario: scenario.into(),
            records,
            payload_len,
            elapsed_secs,
            throughput_per_sec,
            mib_per_sec,
            disk_bytes,
            disk_bytes_per_record,
        }
    }
}

/// Fill a fresh in-memory data file with `n` versioned records.
///
/// No journal, no fsync — this isolates the hash-mapped page-table insert path
/// (routing + page packing + rebalances), which is the engine's hot write loop.
#[must_use]
pub fn fill_in_memory(n: u64, payload_len: usize) -> DataFile {
    let df = DataFile::open_in_memory(DEFAULT_PAGE_SIZE).expect("open in-memory data file");
    for seq in 1..=n {
        df.write_versioned(&make_record(seq, payload_len))
            .expect("in-memory write");
    }
    df
}

/// Measure raw in-memory insert throughput for `n` records.
#[must_use]
pub fn measure_in_memory_write(n: u64, payload_len: usize) -> PerfSample {
    let df = DataFile::open_in_memory(DEFAULT_PAGE_SIZE).expect("open in-memory data file");
    let start = Instant::now();
    for seq in 1..=n {
        df.write_versioned(&make_record(seq, payload_len))
            .expect("in-memory write");
    }
    let elapsed = start.elapsed();
    std::hint::black_box(&df);
    PerfSample::new("in_memory_write", n, payload_len, elapsed, 0)
}

/// Measure read throughput: prefill `n` records, then perform `reads` point
/// lookups spread across the whole keyspace (deterministic LCG, no rand dep).
#[must_use]
pub fn measure_in_memory_read(n: u64, reads: u64, payload_len: usize) -> PerfSample {
    let df = fill_in_memory(n, payload_len);
    // Deterministic pseudo-random walk over 1..=n.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut hits = 0u64;
    let start = Instant::now();
    for _ in 0..reads {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let seq = (state >> 33) % n + 1;
        if df.read_versioned(record_id(seq)).expect("read").is_some() {
            hits += 1;
        }
    }
    let elapsed = start.elapsed();
    assert_eq!(hits, reads, "every prefilled key must be readable");
    PerfSample::new("in_memory_read", reads, payload_len, elapsed, 0)
}

/// Result of the durable-write + recovery workload.
pub struct DurableOutcome {
    /// Throughput of the full WAL commit path (journal append + fsync + apply).
    pub write: PerfSample,
    /// Time to re-open the node and replay the journal back into the data file.
    pub recovery: PerfSample,
    /// Records confirmed readable after recovery (must equal `n`).
    pub recovered: u64,
}

/// Measure the durable write path *and* crash recovery in one pass.
///
/// 1. Open a fresh on-disk node and `commit_write` `n` records — every write is
///    journalled and **fsynced** before it counts, so this is the real durable
///    write rate, not the in-memory rate.
/// 2. Drop the node with no snapshot flush (simulating a crash mid-stream).
/// 3. Re-open the same directory and time how long recovery takes to replay the
///    journal, then verify every record came back.
#[must_use]
pub fn measure_durable_write_and_recovery(n: u64, payload_len: usize) -> DurableOutcome {
    let dir = TempDir::new().expect("tempdir");

    let write_elapsed;
    {
        let storage = NodeStorage::open(dir.path()).expect("open storage");
        let start = Instant::now();
        for seq in 1..=n {
            storage
                .commit_write(record_id(seq).raw(), make_record(seq, payload_len).data)
                .expect("durable commit");
        }
        write_elapsed = start.elapsed();
        // Drop WITHOUT flush_snapshot(): the records live only in the journal,
        // exactly as they would after a crash between compactions.
    }

    let disk_bytes = dir_size(dir.path());

    let start = Instant::now();
    let storage = NodeStorage::open(dir.path()).expect("reopen storage");
    let recovery_elapsed = start.elapsed();

    let mut recovered = 0u64;
    for seq in 1..=n {
        if storage
            .data_file
            .read_versioned(record_id(seq))
            .expect("read after recovery")
            .is_some()
        {
            recovered += 1;
        }
    }
    assert_eq!(
        recovered,
        n,
        "recovery lost {} of {n} records",
        n - recovered
    );

    DurableOutcome {
        write: PerfSample::new(
            "durable_write_wal",
            n,
            payload_len,
            write_elapsed,
            disk_bytes,
        ),
        recovery: PerfSample::new(
            "journal_recovery",
            n,
            payload_len,
            recovery_elapsed,
            disk_bytes,
        ),
        recovered,
    }
}

/// Total size in bytes of the three storage files under `dir`.
fn dir_size(dir: &std::path::Path) -> u64 {
    ["data.bin", "journal.log", "heap.bin"]
        .iter()
        .map(|name| std::fs::metadata(dir.join(name)).map_or(0, |m| m.len()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_write_reports_positive_throughput() {
        let s = measure_in_memory_write(2_000, 64);
        assert_eq!(s.records, 2_000);
        assert!(s.throughput_per_sec > 0.0);
    }

    #[test]
    fn read_workload_hits_every_key() {
        // Asserts internally that all reads hit; just ensure it runs.
        let s = measure_in_memory_read(1_000, 4_000, 64);
        assert_eq!(s.records, 4_000);
    }

    #[test]
    fn durable_roundtrip_recovers_everything() {
        let out = measure_durable_write_and_recovery(500, 64);
        assert_eq!(out.recovered, 500);
        assert!(out.write.disk_bytes > 0);
        assert!(out.recovery.elapsed_secs >= 0.0);
    }
}
