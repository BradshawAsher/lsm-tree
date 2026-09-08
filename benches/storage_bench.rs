use criterion::{black_box, criterion_group, criterion_main, Criterion};
use lsm_tree::LsmEngine;
use tempfile::tempdir;

fn bench_writes(c: &mut Criterion) {
    let mut group = c.benchmark_group("writes");
    group.sample_size(20);

    group.bench_function("sequential_put", |b| {
        let temp = tempdir().unwrap();
        let db = LsmEngine::open(temp.path()).unwrap();
        let mut idx = 0u64;

        b.iter(|| {
            idx += 1;
            let key = format!("seq_key_{:010}", idx);
            let val = format!("seq_val_payload_{:010}", idx);
            db.put(black_box(key.as_bytes()), black_box(val.as_bytes())).unwrap();
        });
    });

    group.bench_function("random_put", |b| {
        let temp = tempdir().unwrap();
        let db = LsmEngine::open(temp.path()).unwrap();
        let mut seed = 123456789u64;

        b.iter(|| {
            // Fast xorshift PRNG
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;

            let key = format!("rnd_key_{:016x}", seed);
            let val = format!("rnd_val_{:016x}", seed);
            db.put(black_box(key.as_bytes()), black_box(val.as_bytes())).unwrap();
        });
    });

    group.finish();
}

fn bench_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("reads");
    group.sample_size(20);

    // Prepare database with flushed SSTable and active MemTable
    let temp = tempdir().unwrap();
    let db = LsmEngine::open_with_options(temp.path(), 1024 * 1024).unwrap();

    // 1. Populate disk SSTable with 1,000 keys
    for i in 0..1000 {
        let key = format!("sst_key_{:06}", i);
        let val = format!("sst_val_{:06}", i);
        db.put(key.as_bytes(), val.as_bytes()).unwrap();
    }
    db.flush_memtable().unwrap();

    // 2. Populate active MemTable with 500 keys
    for i in 0..500 {
        let key = format!("mem_key_{:06}", i);
        let val = format!("mem_val_{:06}", i);
        db.put(key.as_bytes(), val.as_bytes()).unwrap();
    }

    // Benchmark 1: In-memory MemTable point lookup hit
    group.bench_function("memtable_hit", |b| {
        let mut idx = 0;
        b.iter(|| {
            idx = (idx + 1) % 500;
            let key = format!("mem_key_{:06}", idx);
            let res = db.get(black_box(key.as_bytes())).unwrap();
            black_box(res);
        });
    });

    // Benchmark 2: Disk SSTable point lookup hit (Block Index binary search + 4KB block fetch)
    group.bench_function("sstable_hit", |b| {
        let mut idx = 0;
        b.iter(|| {
            idx = (idx + 1) % 1000;
            let key = format!("sst_key_{:06}", idx);
            let res = db.get(black_box(key.as_bytes())).unwrap();
            black_box(res);
        });
    });

    // Benchmark 3: Bloom Filter miss (Instant rejection with zero disk I/O)
    group.bench_function("bloom_filter_miss", |b| {
        let mut idx = 0u64;
        b.iter(|| {
            idx += 1;
            let key = format!("non_existent_key_{:012}", idx);
            let res = db.get(black_box(key.as_bytes())).unwrap();
            black_box(res);
        });
    });

    group.finish();
}

fn bench_scans(c: &mut Criterion) {
    let mut group = c.benchmark_group("scans");
    group.sample_size(15);

    let temp = tempdir().unwrap();
    let db = LsmEngine::open_with_options(temp.path(), 64 * 1024).unwrap();

    // Insert 2,000 keys across multiple SSTables
    for i in 0..2000 {
        let key = format!("record:{:05}", i);
        let val = format!("value_payload_for_record_{:05}", i);
        db.put(key.as_bytes(), val.as_bytes()).unwrap();
    }

    group.bench_function("scan_range_100_keys", |b| {
        b.iter(|| {
            let res = db.scan(
                black_box(Some(b"record:00500")),
                black_box(Some(b"record:00600")),
            ).unwrap();
            black_box(res);
        });
    });

    group.bench_function("scan_all_keys", |b| {
        b.iter(|| {
            let res = db.scan_all().unwrap();
            black_box(res);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_writes, bench_reads, bench_scans);
criterion_main!(benches);
