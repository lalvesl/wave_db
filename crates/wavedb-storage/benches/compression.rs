use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use wavedb_storage::compression::{heap_zstd, page_codec};

fn make_payload(size: usize) -> Vec<u8> {
    // Repetitive pattern — realistic for serialized structs.
    (0..size).map(|i| (i % 251) as u8).collect()
}

const SIZES_BYTES: &[usize] = &[256, 1_024, 16_384, 65_536];

fn bench_heap_compress(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression/heap_zstd/compress");

    for &size in SIZES_BYTES {
        let payload = make_payload(size);
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::from_parameter(size), &payload, |b, data| {
            b.iter(|| criterion::black_box(heap_zstd::compress(data).unwrap()));
        });
    }

    group.finish();
}

fn bench_heap_decompress(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression/heap_zstd/decompress");

    for &size in SIZES_BYTES {
        let payload = make_payload(size);
        let compressed = heap_zstd::compress(&payload).unwrap();
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::from_parameter(size), &compressed, |b, data| {
            b.iter(|| criterion::black_box(heap_zstd::decompress(data).unwrap()));
        });
    }

    group.finish();
}

fn bench_page_codec(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression/page_codec");

    for &size in SIZES_BYTES {
        let payload = make_payload(size);
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::new("encode", size), &payload, |b, data| {
            b.iter(|| criterion::black_box(page_codec::encode_stack(data, 0).unwrap()));
        });

        let encoded = page_codec::encode_stack(&payload, 0).unwrap();
        group.bench_with_input(BenchmarkId::new("decode", size), &encoded, |b, data| {
            b.iter(|| criterion::black_box(page_codec::decode_stack(data, 0).unwrap()));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_heap_compress,
    bench_heap_decompress,
    bench_page_codec,
);
criterion_main!(benches);
