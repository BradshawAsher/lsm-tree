# LSM-Tree Key-Value Storage Engine

A high-performance, crash-resilient, embedded **Log-Structured Merge-Tree (LSM-Tree)** storage engine written in Rust, engineered from first principles following the design patterns of RocksDB and LevelDB.

> 📚 **Technical Documentation**:
> * 📐 [Architecture, Disk Formats & Concurrency (`ARCHITECTURE.md`)](ARCHITECTURE.md)
> * 🎯 [Staff Systems Engineering Interview Preparation (`INTERVIEW_PREP.md`)](INTERVIEW_PREP.md)

---

## ⚡ Architectural Highlights

1. **Sequential Append Durability (Write-Ahead Log)**:
   * Write-Ahead Log with **CRC32-checksummed binary framing** (`[CRC32: 4B][KeyLen: 2B][ValLen: 4B][Op: 1B][Key][Val]`) to guarantee zero data loss and detect torn writes during power cuts or sudden crashes.
   * Big-endian binary wire format with deterministic operation opcodes (`Put = 0`, `Delete = 1`).
2. **Lock-Free In-Memory MemTable**:
   * Backed by a concurrent, lock-free **SkipList** (`crossbeam-skiplist`) for high-concurrency multi-threaded writes without mutex contention.
   * Real-time atomic memory tracking with soft and hard flush thresholds.
3. **Tombstone Semantics**:
   * Deletions are appended as immutable tombstones (`None`) rather than in-place disk mutations, completely eliminating random disk writes and wear on flash storage.
4. **Sorted String Tables (SSTables)**:
   * Immutable **4KB block-encoded disk files** with trailing sparse block indexes for fast $O(\log N)$ binary search.
5. **Probabilistic Bloom Filtering**:
   * Space-efficient bitset filter with **Kirsch-Mitzenmacher double-hashing** (10 bits/key, optimal $k=7$) serialized directly into SSTable metadata, bypassing ~99% of unnecessary disk seeks for non-existent keys.
6. **In-Memory LRU Block Cache**:
   * Caches decoded 4KB SSTable data blocks in RAM using an $O(1)$ Least-Recently-Used eviction policy to accelerate hot reads and repeated scans.
7. **Range Scans & K-Way Merge Iterator**:
   * Min-heap priority queue iterator across MemTable, frozen write buffers, and SSTables providing $O(\log K)$ unified lexicographical range scans with live deduplication and tombstone pruning.
8. **Multi-Way Merge Compaction**:
   * Multi-way merge sort across overlapping SSTables to reclaim tombstones, purge stale overwritten values, and strictly bound read amplification.

---

## 🏗️ Storage Hierarchy

```
Write Request: put(key, val) / delete(key)
         │
         ├──► 1. Append-Only WAL (Disk) ──► fsync() for crash recovery
         │
         └──► 2. Lock-Free MemTable (RAM) ──► SkipList ordered buffer
                  │
                  ▼ (Threshold Exceeded: ~4MB)
              Immutable MemTable
                  │ (Background Flush)
                  ▼
         ┌────────────────────────────────────────────────────────┐
         │ SSTable Level 0 (Disk)                                 │
         │   ├── LRU Block Cache (RAM buffer for hot 4KB blocks)  │
         │   ├── Data Blocks (4KB Sorted K/V entries)             │
         │   ├── Sparse Block Index (Offsets for binary search)   │
         │   └── Bloom Filter (Probabilistic skip for ~99% seeks) │
         └────────────────────────────────────────────────────────┘
                  │
                  ▼ (Background Multi-Way Merge Sort)
              Compaction (Reclaims tombstones & deduplicates keys)
```

---

## 🚀 Getting Started

### 1. Using as a Library

```rust
use lsm_tree::LsmEngine;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open storage engine (recovers from disk if present, initializes 4MB LRU block cache)
    let db = LsmEngine::open("./data")?;

    // Insert key-value pairs (WAL append + MemTable update)
    db.put(b"user:1001", b"Bradshaw")?;
    db.put(b"user:1002", b"Asher")?;
    db.put(b"user:1003", b"Engineer")?;

    // Point lookup (MemTable -> Immutable MemTable -> Bloom Filter -> Block Cache -> SSTable)
    if let Some(val) = db.get(b"user:1001")? {
        println!("Found user: {}", String::from_utf8_lossy(&val));
    }

    // Range scan: [user:1001, user:1003) -> yields 1001 and 1002
    let users = db.scan(Some(b"user:1001"), Some(b"user:1003"))?;
    for (k, v) in users {
        println!("{} -> {}", String::from_utf8_lossy(&k), String::from_utf8_lossy(&v));
    }

    // Delete key (appends an immutable tombstone marker)
    db.delete(b"user:1001")?;
    assert_eq!(db.get(b"user:1001")?, None);

    // Trigger multi-way merge compaction to reclaim disk space
    db.compact()?;

    Ok(())
}
```

### 2. Interactive CLI

```bash
# Insert a record
cargo run --bin cli -- put user:100 "Staff Software Engineer"

# Read a record
cargo run --bin cli -- get user:100

# Range scan across active records
cargo run --bin cli -- scan user:100 user:200

# Delete a record (writes tombstone)
cargo run --bin cli -- delete user:100

# Check database statistics (MemTable size, active SSTables, block cache residency)
cargo run --bin cli -- status

# Run background compaction
cargo run --bin cli -- compact
```

### 3. Run the End-to-End Demo

```bash
cargo run --example basic_kv
```

### 4. Run the Criterion Microbenchmarks

```bash
cargo bench --bench storage_bench
```

---

## 🧪 Testing & Verification

Run the test suite:

```bash
cargo test
```

### Verified Test Cases (16/16 Passing)
* `test_wal_write_and_recover`: Verifies sequential binary record parsing and opcode integrity.
* `test_wal_crc_corruption_detection`: Verifies that flipped bits or corrupted bytes in the WAL are caught via CRC32 checksum mismatch.
* `test_memtable_crud`: Verifies put, get, delete, and tombstone masking in the concurrent SkipList.
* `test_memtable_sorted_order`: Verifies strict lexicographical iteration order.
* `test_block_build_and_binary_search`: Verifies 4KB block encoding, trailing offset table, and intra-block binary search.
* `test_block_cache_lru_eviction`: Verifies $O(1)$ LRU eviction policy and node promotion under memory pressure.
* `test_bloom_filter_accuracy`: Verifies zero false negatives and ~1% false positive rate using double-hashing.
* `test_sstable_build_and_read_point_lookups`: Verifies writing multi-block SSTables, footer parsing, and reading via block index.
* `test_automatic_flush_to_sstable`: Verifies threshold-triggered automatic memory freeze and SSTable flush.
* `test_persistence_across_full_restart`: Simulates abrupt process termination without memory flush, verifying 100% data recovery on reopen.
* `test_merge_iterator_deduplication_and_tombstones`: Verifies $K$-way min-heap priority queue merge, deduplication, and tombstone pruning across streams.
* `test_compaction_deduplication_and_tombstone_eviction`: Verifies multi-way merge sort, newest-key preservation, and bottom-level tombstone purging.
* `test_engine_crud`: Verifies public engine CRUD API.
* `test_engine_range_scan`: Verifies multi-level range queries over MemTable, frozen buffers, and SSTables.
* `test_engine_block_cache`: Verifies cold vs. warm read caching in the live storage engine.
* `test_engine_compaction`: Verifies engine-level multi-SSTable compaction down to a single unified SSTable.

---

## 📄 License
MIT License.
