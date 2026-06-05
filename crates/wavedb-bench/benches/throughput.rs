//! Statistical throughput micro-benchmarks for the WaveDB storage engine.
//!
//! Run with `cargo bench -p wavedb-bench`.  Criterion writes its detailed
//! reports under `target/criterion/` (git-ignored) and tracks its own
//! run-to-run regression deltas there.  For a *git-committed* performance trail,
//! use the `record-perf` binary instead — both share the workloads in
//! `wavedb_bench`'s library so the numbers are directly comparable.

use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use wavedb_bench::{DEFAULT_PAYLOAD_LEN, fill_in_memory, make_record, record_id};
use wavedb_storage::DataFile;
use wavedb_storage::file::data::DEFAULT_PAGE_SIZE;

/// In-memory insert throughput as the record count grows.  `iter_custom` times
/// the whole fill so page rebalances are amortised into the per-record cost.
fn bench_in_memory_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("in_memory_write");
    for &n in &[10_000u64, 100_000] {
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let df =
                        DataFile::open_in_memory(DEFAULT_PAGE_SIZE).expect("open in-memory file");
                    let start = Instant::now();
                    for seq in 1..=n {
                        df.write_versioned(&make_record(seq, DEFAULT_PAYLOAD_LEN))
                            .expect("write");
                    }
                    total += start.elapsed();
                    std::hint::black_box(&df);
                }
                total
            });
        });
    }
    group.finish();
}

/// Point-read throughput against a table prefilled with `n` records.
fn bench_in_memory_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("in_memory_read");
    for &n in &[10_000u64, 100_000] {
        let df = fill_in_memory(n, DEFAULT_PAYLOAD_LEN);
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let mut state = 0x9E37_79B9_7F4A_7C15u64;
            b.iter(|| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let seq = (state >> 33) % n + 1;
                std::hint::black_box(df.read_versioned(record_id(seq)).expect("read"));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_in_memory_write, bench_in_memory_read);
criterion_main!(benches);
