use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tempfile::tempdir;
use wavedb_core::Id;
use wavedb_storage::{AnchorKey, AnchorSlot, DataFile, VersionedRecord, hash};

fn bench_anchor_write(c: &mut Criterion) {
    let slot = AnchorSlot::inline(b"bench payload 32 bytes of data!!", 0);

    let mut group = c.benchmark_group("anchor_write");
    group.throughput(Throughput::Elements(1));

    let mut counter = 0u64;
    group.bench_function("inline_primary", |b| {
        b.iter_batched(
            || {
                // Fresh DataFile per batch so pages never fill up.
                let dir = tempdir().unwrap();
                let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
                (dir, file)
            },
            |(_dir, file)| {
                // Vary tenant_id so writes distribute across pages
                // (page_hash only uses struct_id, tenant_id, shard_id).
                counter = counter.wrapping_add(1);
                let tenant = counter % 256;
                let id = Id::new(tenant, 0, 0, counter);
                let key = AnchorKey::from(id);
                file.write_anchor(key, &slot).unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_anchor_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("anchor_read");
    group.throughput(Throughput::Elements(1));

    // Pre-populate N anchors, then benchmark reads (warm cache).
    // Vary tenant_id so writes distribute across pages.
    for &n in &[100u64, 500, 2_000] {
        let dir = tempdir().unwrap();
        let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        let slot = AnchorSlot::inline(b"bench read payload", 0);
        for i in 0..n {
            let tenant = i % 256;
            let key = AnchorKey::from(Id::new(tenant, 0, 0, i));
            file.write_anchor(key, &slot).unwrap();
        }

        // Read a key that was definitely written.
        let target_i = n / 2;
        let target_tenant = target_i % 256;
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            let target = AnchorKey::from(Id::new(target_tenant, 0, 0, target_i));
            b.iter(|| {
                criterion::black_box(file.read_anchor(target).unwrap());
            });
        });
    }

    group.finish();
}

fn bench_versioned_write(c: &mut Criterion) {
    let data = vec![0u8; 128];

    let mut group = c.benchmark_group("versioned_write");
    group.throughput(Throughput::Elements(1));

    let mut counter = 0u64;
    group.bench_function("single_128b", |b| {
        b.iter_batched(
            || {
                let dir = tempdir().unwrap();
                let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
                (dir, file)
            },
            |(_dir, file)| {
                counter = counter.wrapping_add(1);
                let id = Id::new(3, 0, 0, counter);
                let rec = VersionedRecord::new(id.raw(), data.clone());
                file.write_versioned(&rec).unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_hash_functions(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash");
    group.throughput(Throughput::Elements(1));

    group.bench_function("page_hash", |b| {
        b.iter(|| criterion::black_box(hash::page_hash(7, 42, 3, 256)));
    });

    group.bench_function("double_hash_step", |b| {
        b.iter(|| criterion::black_box(hash::double_hash_step(7, 42, 3, 256)));
    });

    // Combined: primary hash + double-hash step (collision resolution cost).
    group.bench_function("full_double_hash_probe", |b| {
        b.iter(|| {
            let page_count = 256u64;
            let primary = hash::page_hash(7, 42, 3, page_count);
            let step = hash::double_hash_step(7, 42, 3, page_count);
            criterion::black_box((primary + step) % page_count)
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_anchor_write,
    bench_anchor_read,
    bench_versioned_write,
    bench_hash_functions,
);
criterion_main!(benches);
