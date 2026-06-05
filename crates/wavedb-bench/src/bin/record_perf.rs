//! Git-tracked performance recorder.
//!
//! Runs a fixed set of storage-engine workloads, prints a summary table, and
//! persists the numbers into version-controlled files so performance can be
//! reviewed in `git log` / `git diff` over time:
//!
//! * `results/history.jsonl` — append-only, one JSON object per run (the time
//!   series). Never rewritten, so history is auditable.
//! * `results/latest.md` — regenerated each run; a human-readable snapshot of
//!   the most recent numbers plus the commit they were measured at.
//!
//! Run it (release, for representative numbers):
//!
//! ```text
//! cargo run -p wavedb-bench --release --bin record-perf
//! git add crates/wavedb-bench/results && git commit -m "perf: record baseline"
//! ```
//!
//! Sizes can be shrunk for a quick smoke run via env vars:
//! `WAVEDB_BENCH_WRITE_N`, `WAVEDB_BENCH_READ_FILL`, `WAVEDB_BENCH_READ_OPS`,
//! `WAVEDB_BENCH_DURABLE_N`, `WAVEDB_BENCH_PAYLOAD`.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use wavedb_bench::{
    PerfSample, measure_durable_write_and_recovery, measure_in_memory_read, measure_in_memory_write,
};

/// One recorder run: metadata + every sample measured.
#[derive(Serialize)]
struct RunRecord {
    /// Seconds since the Unix epoch.
    ts_unix: u64,
    /// Best-effort UTC ISO-8601 timestamp (empty if `date` is unavailable).
    timestamp_utc: String,
    /// Short git commit the binary was built from (`unknown` if not in a repo).
    git_commit: String,
    /// Target architecture the recorder ran on.
    arch: String,
    /// Whether the binary was compiled with optimizations.
    release: bool,
    /// Measured samples.
    samples: Vec<PerfSample>,
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Parse a comma-separated list of record counts, e.g. `"50000,200000"`.
fn env_sizes(key: &str, default: &str) -> Vec<u64> {
    std::env::var(key)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

fn git_commit() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn iso_utc() -> String {
    Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn results_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("results")
}

fn print_table(samples: &[PerfSample]) {
    println!(
        "\n{:<22} {:>11} {:>9} {:>12} {:>14} {:>11} {:>12}",
        "scenario", "records", "payload", "elapsed_s", "records/s", "MiB/s", "bytes/rec"
    );
    println!("{}", "-".repeat(96));
    for s in samples {
        println!(
            "{:<22} {:>11} {:>9} {:>12.4} {:>14.0} {:>11.1} {:>12.1}",
            s.scenario,
            s.records,
            s.payload_len,
            s.elapsed_secs,
            s.throughput_per_sec,
            s.mib_per_sec,
            s.disk_bytes_per_record,
        );
    }
    println!();
}

fn write_markdown(dir: &Path, run: &RunRecord) -> std::io::Result<()> {
    let mut md = String::new();
    md.push_str("# WaveDB performance — latest run\n\n");
    let build = if run.release {
        "release (optimized)"
    } else {
        "debug (NOT optimized — numbers not representative)"
    };
    let when = if run.timestamp_utc.is_empty() {
        "n/a"
    } else {
        &run.timestamp_utc
    };
    let _ = write!(
        md,
        "- Commit: `{}`\n- When: {} (unix {})\n- Arch: `{}`\n- Build: {}\n\n",
        run.git_commit, when, run.ts_unix, run.arch, build,
    );
    md.push_str(
        "| scenario | records | payload (B) | elapsed (s) | records/s | MiB/s | disk B/rec |\n",
    );
    md.push_str("|---|--:|--:|--:|--:|--:|--:|\n");
    for s in &run.samples {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {:.4} | {:.0} | {:.1} | {:.1} |",
            s.scenario,
            s.records,
            s.payload_len,
            s.elapsed_secs,
            s.throughput_per_sec,
            s.mib_per_sec,
            s.disk_bytes_per_record,
        );
    }
    md.push_str("\nThe full time series is in [`history.jsonl`](history.jsonl). ");
    md.push_str("Regenerate with `cargo run -p wavedb-bench --release --bin record-perf`.\n");
    fs::write(dir.join("latest.md"), md)
}

fn append_history(dir: &Path, run: &RunRecord) -> std::io::Result<()> {
    let line = serde_json::to_string(run).expect("serialize run record");
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("history.jsonl"))?;
    writeln!(f, "{line}")
}

fn main() {
    let payload = usize::try_from(env_u64("WAVEDB_BENCH_PAYLOAD", 128)).unwrap();
    // Two write sizes by default so the committed baseline shows whether write
    // throughput holds steady or degrades as the record count grows.
    let write_sizes = env_sizes("WAVEDB_BENCH_WRITE_SIZES", "50000,200000");
    let read_fill = env_u64("WAVEDB_BENCH_READ_FILL", 100_000);
    let read_ops = env_u64("WAVEDB_BENCH_READ_OPS", 200_000);
    let durable_n = env_u64("WAVEDB_BENCH_DURABLE_N", 5_000);

    let release = !cfg!(debug_assertions);
    if !release {
        eprintln!(
            "warning: recording a DEBUG build — numbers are not representative. \
             Use `--release` for a committable baseline."
        );
    }

    eprintln!(
        "running workloads (write_sizes={write_sizes:?}, read_ops={read_ops}, durable_n={durable_n})…"
    );

    let mut samples = Vec::new();
    for &n in &write_sizes {
        samples.push(measure_in_memory_write(n, payload));
    }
    samples.push(measure_in_memory_read(read_fill, read_ops, payload));
    let durable = measure_durable_write_and_recovery(durable_n, payload);
    println!(
        "durable roundtrip recovered {}/{} records after simulated crash",
        durable.recovered, durable_n
    );
    samples.push(durable.write);
    samples.push(durable.recovery);

    print_table(&samples);

    let run = RunRecord {
        ts_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        timestamp_utc: iso_utc(),
        git_commit: git_commit(),
        arch: std::env::consts::ARCH.to_string(),
        release,
        samples,
    };

    let dir = results_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("could not create {}: {e}", dir.display());
        return;
    }
    if let Err(e) = append_history(&dir, &run) {
        eprintln!("could not append history: {e}");
    }
    if let Err(e) = write_markdown(&dir, &run) {
        eprintln!("could not write latest.md: {e}");
    }
    println!("results written to {}", dir.display());
}
