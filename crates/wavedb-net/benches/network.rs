use bytes::BytesMut;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use serde::{Deserialize, Serialize};
use wavedb_net::{frame, notify::SeenFilter};

/// Payload representative of a typical `WaveDB` network message.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BenchPayload {
    anchor_id: u128,
    version: u64,
    data: Vec<u8>,
}

impl BenchPayload {
    fn new(size: usize) -> Self {
        Self {
            anchor_id: 0xDEAD_BEEF_CAFE_1234_5678_9ABC_DEF0_1234,
            version: 42,
            data: vec![0u8; size],
        }
    }
}

const PAYLOAD_SIZES: &[usize] = &[64, 512, 4_096, 65_536];

fn bench_frame_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("network/frame/encode");

    for &size in PAYLOAD_SIZES {
        let msg = BenchPayload::new(size);
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::from_parameter(size), &msg, |b, msg| {
            b.iter(|| criterion::black_box(frame::encode(msg).unwrap()));
        });
    }

    group.finish();
}

fn bench_frame_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("network/frame/decode");

    for &size in PAYLOAD_SIZES {
        let msg = BenchPayload::new(size);
        let encoded = frame::encode(&msg).unwrap();
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::from_parameter(size), &encoded, |b, data| {
            b.iter(|| {
                let mut buf = BytesMut::from(data.as_ref());
                criterion::black_box(frame::decode::<BenchPayload>(&mut buf).unwrap())
            });
        });
    }

    group.finish();
}

fn bench_bloom_mark(c: &mut Criterion) {
    let mut group = c.benchmark_group("network/bloom/mark");
    group.throughput(Throughput::Elements(1));

    let filter = SeenFilter::new();
    let mut counter = 0u128;

    group.bench_function("single_insert", |b| {
        b.iter(|| {
            filter.mark(counter);
            counter = counter.wrapping_add(1);
        });
    });

    group.finish();
}

fn bench_bloom_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("network/bloom/check");
    group.throughput(Throughput::Elements(1));

    let filter = SeenFilter::new();
    for i in 0..10_000u128 {
        filter.mark(i);
    }

    group.bench_function("probably_seen_hit", |b| {
        b.iter(|| criterion::black_box(filter.probably_seen(5_000)));
    });

    group.bench_function("probably_seen_miss", |b| {
        b.iter(|| criterion::black_box(filter.probably_seen(u128::MAX)));
    });

    group.finish();
}

fn bench_bloom_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("network/bloom/serialize");

    let filter = SeenFilter::new();
    for i in 0..10_000u128 {
        filter.mark(i);
    }

    group.bench_function("to_bytes", |b| {
        b.iter(|| criterion::black_box(filter.to_bytes()));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_frame_encode,
    bench_frame_decode,
    bench_bloom_mark,
    bench_bloom_check,
    bench_bloom_serialize,
);
criterion_main!(benches);
