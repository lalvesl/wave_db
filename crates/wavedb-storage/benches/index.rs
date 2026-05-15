use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use wavedb_storage::{
    AnchorKey,
    index::{ArrayIndex, BTreeIndex, IndexBackend, IndexKey},
};

const SIZES: &[usize] = &[1, 10, 50, 100, 1_000, 10_000];

fn bench_array_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_lookup/array");

    for &n in SIZES {
        let mut idx = ArrayIndex::new();
        for i in 0..n {
            idx.insert(IndexKey(i as u64), AnchorKey::from_raw(i as u128))
                .unwrap();
        }
        let target = IndexKey((n / 2) as u64);

        group.bench_with_input(BenchmarkId::from_parameter(n), &target, |b, &key| {
            b.iter(|| std::hint::black_box(idx.lookup(&key)));
        });
    }

    group.finish();
}

fn bench_btree_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_lookup/btree");

    for &n in SIZES {
        let entries: Vec<_> = (0..n)
            .map(|i| (IndexKey(i as u64), AnchorKey::from_raw(i as u128)))
            .collect();
        let idx = BTreeIndex::from_sorted(&entries);
        let target = IndexKey((n / 2) as u64);

        group.bench_with_input(BenchmarkId::from_parameter(n), &target, |b, &key| {
            b.iter(|| std::hint::black_box(idx.lookup(&key)));
        });
    }

    group.finish();
}

fn bench_array_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_insert/array");

    for &n in SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let mut idx = ArrayIndex::new();
                    for i in 0..n {
                        idx.insert(IndexKey(i as u64 * 2), AnchorKey::from_raw(i as u128))
                            .unwrap();
                    }
                    idx
                },
                |mut idx| {
                    // Insert one new key into an already-populated index.
                    idx.insert(IndexKey(n as u64 * 2 + 1), AnchorKey::from_raw(0))
                        .unwrap();
                    std::hint::black_box(idx);
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn bench_btree_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_insert/btree");

    for &n in SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let entries: Vec<_> = (0..n)
                        .map(|i| (IndexKey(i as u64 * 2), AnchorKey::from_raw(i as u128)))
                        .collect();
                    BTreeIndex::from_sorted(&entries)
                },
                |mut idx| {
                    idx.insert(IndexKey(n as u64 * 2 + 1), AnchorKey::from_raw(0))
                        .unwrap();
                    std::hint::black_box(idx);
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_array_lookup,
    bench_btree_lookup,
    bench_array_insert,
    bench_btree_insert,
);
criterion_main!(benches);
