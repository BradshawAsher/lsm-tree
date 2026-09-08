# LSM-Tree Key-Value Storage Engine: Implementation Phases

This document details the progressive, 8-phase engineering roadmap followed to design, implement, and empirically verify the **Log-Structured Merge-Tree (LSM-Tree)** storage engine from first principles.

---

## Roadmap Overview

```
Phase 1: WAL & Durability Framing
   │
   ▼
Phase 2: Concurrent Lock-Free MemTable
   │
   ▼
Phase 3: 4KB Data Block SSTable Layout
   │
   ▼
Phase 4: Kirsch-Mitzenmacher Bloom Filtering
   │
   ▼
Phase 5: Multi-Way Merge Compaction
   │
   ▼
Phase 6: K-Way Merge Range Iterator (`scan`)
   │
   ▼
Phase 7: In-Memory $O(1)$ LRU Block Cache
   │
   ▼
Phase 8: Criterion Benchmarks & Verification
```

---

## Phase 1: Write-Ahead Log (WAL) & Crash Durability

* **Objective**: Guarantee sequential write durability and prevent data corruption from torn writes during unexpected power loss.
* **Key Technical Implementations**:
  * Append-only binary log (`active.wal`) using big-endian serialization.
  * 11-byte binary frame header: `[CRC32: 4B][KeyLen: 2B][ValLen: 4B][Op: 1B]`.
  * Hardware-accelerated IEEE 802.3 CRC32 checksums via `crc32fast` (>10 GB/s throughput).
  * Explicit OS page cache flushing using `fsync` (`sync_data`).
  * Sequential crash-recovery parser that verifies CRC32 checksums on startup and truncates corrupted trailing writes.
* **Verified By**:
  * `test_wal_write_and_recover`
  * `test_wal_crc_corruption_detection` (simulated flipped bits in byte stream).

---

## Phase 2: Concurrent Lock-Free MemTable

* **Objective**: Deliver high-throughput in-memory writes with zero thread lock contention.
* **Key Technical Implementations**:
  * In-memory sorted write buffer backed by `crossbeam-skiplist::SkipMap`.
  * Lock-free atomic Compare-And-Swap (CAS) pointer updates with epoch-based memory reclamation.
  * Real-time atomic memory accounting (`size_bytes`) tracking keys, values, and 64-byte node overheads.
  * Configurable soft and hard flush thresholds (default $4\text{ MB}$).
  * Immutable tombstone insertion semantics (`None` value) for deletions.
* **Verified By**:
  * `test_memtable_crud`
  * `test_memtable_sorted_order`

---

## Phase 3: SSTable 4KB Data Block Engine & Sparse Index

* **Objective**: Transition memory buffers into immutable, sector-aligned disk structures with fast intra-block search.
* **Key Technical Implementations**:
  * 4KB data blocks (`BlockBuilder`) packing sorted key-value pairs.
  * Trailing 2-byte offset index (`Vec<u16>`) allowing $O(\log N)$ intra-block binary search without linear block scans.
  * Sparse Block Index writing `[FirstKeyLen][FirstKey][Offset: 8B][Len: 8B]` metadata at the end of each SSTable.
  * Fixed 40-byte trailer footer containing index/filter offsets, lengths, and magic bytes (`0x4C534D5452454531`).
* **Verified By**:
  * `test_block_build_and_binary_search`
  * `test_sstable_build_and_read_point_lookups`

---

## Phase 4: Probabilistic Bloom Filtering

* **Objective**: Eliminate unnecessary disk I/O for non-existent key lookups.
* **Key Technical Implementations**:
  * Space-efficient bitset serialized into the SSTable metadata section.
  * Kirsch-Mitzenmacher double-hashing algorithm:
    $$g_i(x) = (h_1(x) + i \cdot h_2(x)) \pmod m$$
  * Sized at 10 bits/key ($m/n = 10$) with optimal $k=7$ hash functions using 64-bit FNV-1a hashes.
  * Achieved theoretical false positive rate of $\approx 0.82\%$, bypassing $\sim 99.2\%$ of non-existent disk seeks.
* **Verified By**:
  * `test_bloom_filter_accuracy` (zero false negatives, $<1\%$ false positive rate).

---

## Phase 5: Multi-Way Merge Compaction & Tombstone Eviction

* **Objective**: Reclaim disk space from superseded versions and bound read amplification across overlapping SSTables.
* **Key Technical Implementations**:
  * Multi-way merge sort across overlapping SSTables from newest to oldest.
  * In-memory key deduplication: freshest version supersedes stale historical writes.
  * Safe bottom-level tombstone purging: evicts deletion markers only when no older SSTables exist below them, preventing key resurrection.
  * Atomic file replacement: writes the unified SSTable to a new ID and unlinks obsolete SSTable files.
* **Verified By**:
  * `test_compaction_deduplication_and_tombstone_eviction`
  * `test_engine_compaction`

---

## Phase 6: K-Way Merge Range Iterator (`db.scan`)

* **Objective**: Enable ordered range queries across multiple tiers of volatile and non-volatile storage.
* **Key Technical Implementations**:
  * `SsTableIterator`: Sequentially streams only the 4KB blocks intersecting query range `[start, end)`. Blocks outside the range are never read from disk.
  * `MergeIterator`: Min-heap priority queue (`std::collections::BinaryHeap`) merging $K$ sorted streams:
    * Priority 0: Active MemTable.
    * Priority 1..$K$: Frozen immutable MemTables.
    * Priority $K+1..N$: SSTables ordered descending by ID (newest to oldest).
  * On-the-fly deduplication and tombstone masking.
  * Interactive CLI command: `cargo run --bin cli -- scan [start] [end]`.
* **Verified By**:
  * `test_merge_iterator_deduplication_and_tombstones`
  * `test_engine_range_scan`

---

## Phase 7: In-Memory $O(1)$ LRU Block Cache

* **Objective**: Accelerate warm point lookups and repeated scans by buffering decoded 4KB data blocks in RAM.
* **Key Technical Implementations**:
  * Thread-safe LRU cache (`BlockCache`) protected by `parking_lot::Mutex`.
  * Cache key: `BlockKey { sst_id: u64, offset: u64 }`.
  * Value: `Arc<Block>` (already decoded in-memory block structure).
  * Custom intrusive doubly-linked list with array indices and free-slot recycling for zero heap allocation overhead during cache churn.
  * Default capacity: 1,024 blocks ($\approx 4\text{ MB}$ of hot data in RAM).
  * Cache hits bypass both file seek/read I/O and block deserialization.
* **Verified By**:
  * `test_block_cache_lru_eviction`
  * `test_engine_block_cache`

---

## Phase 8: Statistical Microbenchmarks & Performance Profiling

* **Objective**: Empirically quantify engine throughput and latency distributions.
* **Key Technical Implementations**:
  * Integrated Criterion 0.5 benchmarking suite (`benches/storage_bench.rs`).
  * Microbenchmark groups:
    * `writes/sequential_put`: Measures WAL append + SkipList insertion throughput.
    * `writes/random_put`: Evaluates PRNG write distribution.
    * `reads/memtable_hit`: In-memory lock-free lookup latency.
    * `reads/sstable_hit`: Block Index binary search + 4KB block fetch.
    * `reads/bloom_filter_miss`: Instant RAM rejection latency ($\sim 0$ disk seeks).
    * `scans/scan_range_100_keys`: Multi-way merge iterator streaming latency.
    * `scans/scan_all_keys`: Full database traversal throughput.
* **Verified By**:
  * `cargo bench --bench storage_bench -- --test`
  * Complete test suite passing: **16/16 unit and integration tests**.
