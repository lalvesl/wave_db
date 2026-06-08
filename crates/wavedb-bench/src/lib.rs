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
    /// I/O operations per second for disk-backed workloads; 0.0 for pure
    /// in-memory workloads where no physical I/O is issued.
    /// For write workloads each fsynced journal commit = 1 IOPS.
    /// For read workloads each on-disk lookup = 1 IOPS.
    pub iops: f64,
}

impl PerfSample {
    fn new(
        scenario: impl Into<String>,
        records: u64,
        payload_len: usize,
        elapsed: Duration,
        disk_bytes: u64,
        iops: f64,
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
        let payload_total_mib =
            (records_f * payload_len as f64) / (1024.0 * 1024.0);
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
            iops,
        }
    }
}

// ── Cloud disk tier specifications ───────────────────────────────────────────
//
// Sources:
//   AWS EBS: https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ebs-volume-types.html
//   GCP PD:  https://cloud.google.com/compute/docs/disks/performance

/// Cloud disk tier specification with sustained IOPS and throughput limits.
#[derive(Debug)]
pub struct DiskTier {
    pub provider: &'static str,
    pub name: &'static str,
    /// Maximum sustained write IOPS (for a representative disk size).
    pub max_write_iops: f64,
    /// Maximum sustained read IOPS (for a representative disk size).
    pub max_read_iops: f64,
    /// Maximum throughput in MiB/s.
    pub max_throughput_mib_s: f64,
}

/// All tracked cloud disk tiers, ordered from cheapest to most capable.
pub static DISK_TIERS: &[DiskTier] = &[
    // ── AWS EBS ───────────────────────────────────────────────────────────────
    // gp2: burst IOPS available for volumes ≤ 1 TiB; baseline = 3 IOPS/GiB.
    DiskTier {
        provider: "AWS",
        name: "EBS gp2 (burst ≤1 TiB)",
        max_write_iops: 3_000.0,
        max_read_iops: 3_000.0,
        max_throughput_mib_s: 250.0,
    },
    // gp3 baseline is always available without extra provisioning cost.
    DiskTier {
        provider: "AWS",
        name: "EBS gp3 (baseline)",
        max_write_iops: 3_000.0,
        max_read_iops: 3_000.0,
        max_throughput_mib_s: 125.0,
    },
    // gp3 fully provisioned.
    DiskTier {
        provider: "AWS",
        name: "EBS gp3 (max provisioned)",
        max_write_iops: 16_000.0,
        max_read_iops: 16_000.0,
        max_throughput_mib_s: 1_000.0,
    },
    // io1/io2: provisioned IOPS SSD, same per-disk IOPS ceiling.
    DiskTier {
        provider: "AWS",
        name: "EBS io1 / io2",
        max_write_iops: 64_000.0,
        max_read_iops: 64_000.0,
        max_throughput_mib_s: 1_000.0,
    },
    // io2 Block Express: highest single-disk tier.
    DiskTier {
        provider: "AWS",
        name: "EBS io2 Block Express",
        max_write_iops: 256_000.0,
        max_read_iops: 256_000.0,
        max_throughput_mib_s: 4_000.0,
    },
    // ── GCP Persistent Disk ───────────────────────────────────────────────────
    // pd-standard (HDD): read and write IOPS differ significantly.
    DiskTier {
        provider: "GCP",
        name: "PD Standard (HDD, 1 TiB)",
        max_write_iops: 1_500.0,
        max_read_iops: 75.0,
        max_throughput_mib_s: 180.0,
    },
    // pd-balanced: SSD, symmetric IOPS.
    DiskTier {
        provider: "GCP",
        name: "PD Balanced",
        max_write_iops: 3_000.0,
        max_read_iops: 3_000.0,
        max_throughput_mib_s: 240.0,
    },
    // pd-ssd: 30 IOPS/GiB r+w; numbers here are for a 500 GiB disk.
    DiskTier {
        provider: "GCP",
        name: "PD SSD (500 GiB)",
        max_write_iops: 15_000.0,
        max_read_iops: 15_000.0,
        max_throughput_mib_s: 240.0,
    },
    // pd-extreme: provisioned IOPS, highest PD tier.
    DiskTier {
        provider: "GCP",
        name: "PD Extreme",
        max_write_iops: 120_000.0,
        max_read_iops: 120_000.0,
        max_throughput_mib_s: 2_400.0,
    },
];

/// Per-tier result of comparing measured workload metrics against published limits.
pub struct TierVerdict<'a> {
    pub tier: &'a DiskTier,
    /// Measured write IOPS ≤ tier's max_write_iops.
    pub write_fits: bool,
    /// Measured read IOPS ≤ tier's max_read_iops.
    pub read_fits: bool,
    /// Measured MiB/s ≤ tier's max_throughput_mib_s.
    pub throughput_fits: bool,
}

impl TierVerdict<'_> {
    /// True when all three dimensions fit within the tier's limits.
    pub fn fits(&self) -> bool {
        self.write_fits && self.read_fits && self.throughput_fits
    }
}

/// Compare measured disk metrics against every entry in [`DISK_TIERS`].
///
/// Pass the IOPS from [`measure_disk_write_iops`] and [`measure_disk_read_iops`]
/// and the peak MiB/s from either workload.
pub fn evaluate_tiers(
    write_iops: f64,
    read_iops: f64,
    mib_per_sec: f64,
) -> Vec<TierVerdict<'static>> {
    DISK_TIERS
        .iter()
        .map(|tier| TierVerdict {
            tier,
            write_fits: write_iops <= tier.max_write_iops,
            read_fits: read_iops <= tier.max_read_iops,
            throughput_fits: mib_per_sec <= tier.max_throughput_mib_s,
        })
        .collect()
}

// ── In-memory workloads ───────────────────────────────────────────────────────

/// Fill a fresh in-memory data file with `n` versioned records.
///
/// No journal, no fsync — this isolates the hash-mapped page-table insert path
/// (routing + page packing + rebalances), which is the engine's hot write loop.
#[must_use]
pub fn fill_in_memory(n: u64, payload_len: usize) -> DataFile {
    let df = DataFile::open_in_memory(DEFAULT_PAGE_SIZE)
        .expect("open in-memory data file");
    for seq in 1..=n {
        df.write_versioned(&make_record(seq, payload_len))
            .expect("in-memory write");
    }
    df
}

/// Measure raw in-memory insert throughput for `n` records.
#[must_use]
pub fn measure_in_memory_write(n: u64, payload_len: usize) -> PerfSample {
    let df = DataFile::open_in_memory(DEFAULT_PAGE_SIZE)
        .expect("open in-memory data file");
    let start = Instant::now();
    for seq in 1..=n {
        df.write_versioned(&make_record(seq, payload_len))
            .expect("in-memory write");
    }
    let elapsed = start.elapsed();
    std::hint::black_box(&df);
    PerfSample::new("in_memory_write", n, payload_len, elapsed, 0, 0.0)
}

/// Measure read throughput: prefill `n` records, then perform `reads` point
/// lookups spread across the whole keyspace (deterministic LCG, no rand dep).
#[must_use]
pub fn measure_in_memory_read(
    n: u64,
    reads: u64,
    payload_len: usize,
) -> PerfSample {
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
    PerfSample::new("in_memory_read", reads, payload_len, elapsed, 0, 0.0)
}

// ── Disk workloads ────────────────────────────────────────────────────────────

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
pub fn measure_durable_write_and_recovery(
    n: u64,
    payload_len: usize,
) -> DurableOutcome {
    let dir = TempDir::new().expect("tempdir");

    let write_elapsed;
    {
        let storage = NodeStorage::open(dir.path()).expect("open storage");
        let start = Instant::now();
        for seq in 1..=n {
            storage
                .commit_write(
                    record_id(seq).raw(),
                    make_record(seq, payload_len).data,
                )
                .expect("durable commit");
        }
        write_elapsed = start.elapsed();
        // Drop WITHOUT flush_snapshot(): the records live only in the journal,
        // exactly as they would after a crash between compactions.
    }

    let disk_bytes = dir_size(dir.path());

    #[allow(clippy::cast_precision_loss)]
    let write_iops = if write_elapsed.as_secs_f64() > 0.0 {
        n as f64 / write_elapsed.as_secs_f64()
    } else {
        0.0
    };

    let start = Instant::now();
    let storage = NodeStorage::open(dir.path()).expect("reopen storage");
    let recovery_elapsed = start.elapsed();

    #[allow(clippy::cast_precision_loss)]
    let recovery_iops = if recovery_elapsed.as_secs_f64() > 0.0 {
        n as f64 / recovery_elapsed.as_secs_f64()
    } else {
        0.0
    };

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
            write_iops,
        ),
        recovery: PerfSample::new(
            "journal_recovery",
            n,
            payload_len,
            recovery_elapsed,
            disk_bytes,
            recovery_iops,
        ),
        recovered,
    }
}

/// Measure sustained write IOPS: `n` sequential fsynced journal commits.
///
/// Each `commit_write` call issues one journal append + fsync, mirroring the
/// write amplification pattern of any WAL-based storage engine.  The resulting
/// IOPS figure is directly comparable to cloud provider write IOPS limits.
#[must_use]
pub fn measure_disk_write_iops(n: u64, payload_len: usize) -> PerfSample {
    let dir = TempDir::new().expect("tempdir");
    let storage = NodeStorage::open(dir.path()).expect("open storage");
    let start = Instant::now();
    for seq in 1..=n {
        storage
            .commit_write(
                record_id(seq).raw(),
                make_record(seq, payload_len).data,
            )
            .expect("disk write");
    }
    let elapsed = start.elapsed();
    std::hint::black_box(&storage);
    let disk_bytes = dir_size(dir.path());
    #[allow(clippy::cast_precision_loss)]
    let iops = if elapsed.as_secs_f64() > 0.0 {
        n as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    PerfSample::new(
        "disk_write_iops",
        n,
        payload_len,
        elapsed,
        disk_bytes,
        iops,
    )
}

/// Measure sustained read IOPS: random point lookups from an on-disk data file.
///
/// Prefills `n` records, flushes a snapshot so all data lands in `data.bin`
/// (no journal involvement during reads), then performs `reads` random lookups.
/// The resulting IOPS figure is comparable to cloud provider read IOPS limits.
#[must_use]
pub fn measure_disk_read_iops(
    n: u64,
    reads: u64,
    payload_len: usize,
) -> PerfSample {
    let dir = TempDir::new().expect("tempdir");
    {
        let storage = NodeStorage::open(dir.path()).expect("open storage");
        for seq in 1..=n {
            storage
                .commit_write(
                    record_id(seq).raw(),
                    make_record(seq, payload_len).data,
                )
                .expect("prefill write");
        }
        // Flush to data.bin so the timed reads come from the data file, not
        // from journal replay — this isolates the read I/O path cleanly.
        storage
            .flush_snapshot()
            .expect("flush snapshot to data.bin");
    }
    let storage = NodeStorage::open(dir.path()).expect("reopen storage");

    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut hits = 0u64;
    let start = Instant::now();
    for _ in 0..reads {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let seq = (state >> 33) % n + 1;
        if storage
            .data_file
            .read_versioned(record_id(seq))
            .expect("disk read")
            .is_some()
        {
            hits += 1;
        }
    }
    let elapsed = start.elapsed();
    assert_eq!(
        hits, reads,
        "every prefilled key must be readable after snapshot flush"
    );
    let disk_bytes = dir_size(dir.path());
    #[allow(clippy::cast_precision_loss)]
    let iops = if elapsed.as_secs_f64() > 0.0 {
        reads as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    PerfSample::new(
        "disk_read_iops",
        reads,
        payload_len,
        elapsed,
        disk_bytes,
        iops,
    )
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
        assert_eq!(s.iops, 0.0, "in-memory workload must not report IOPS");
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
        assert!(out.write.iops > 0.0, "durable write must report IOPS");
        assert!(out.recovery.elapsed_secs >= 0.0);
    }

    #[test]
    fn disk_write_iops_positive() {
        let s = measure_disk_write_iops(200, 64);
        assert_eq!(s.records, 200);
        assert!(s.iops > 0.0);
        assert!(s.disk_bytes > 0);
    }

    #[test]
    fn disk_read_iops_positive() {
        let s = measure_disk_read_iops(200, 500, 64);
        assert_eq!(s.records, 500);
        assert!(s.iops > 0.0);
    }

    #[test]
    fn evaluate_tiers_all_tiers_covered() {
        let verdicts = evaluate_tiers(500.0, 5_000.0, 1.0);
        assert_eq!(verdicts.len(), DISK_TIERS.len());
        // 500 write IOPS fits every tier's write limit
        assert!(verdicts.iter().all(|v| v.write_fits));
    }
}
