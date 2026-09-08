# LSM-Tree Key-Value Storage Engine

A high-performance, crash-resilient, embedded **Log-Structured Merge-Tree (LSM-Tree)** storage engine written in Rust, engineered from first principles following the design principles of RocksDB and LevelDB.

---

## ⚡ Architectural Highlights

1. **Sequential Append Durability (WAL)**:
   * Write-Ahead Log with **CRC32 checksummed binary framing** to guarantee zero data loss and detect torn writes during sudden system crashes or power failures.
   * Big-endian binary wire format with deterministic operation opcodes (`Put = 0`, `Delete = 1`).
2. **Lock-Free In-Memory MemTable**:
   * Backed by a concurrent, lock-free **SkipList** (`crossbeam-skiplist`) for high-concurrency multi-threaded writes without mutex contention.
   * Real-time atomic memory tracking with soft and hard flush thresholds.
3. **Tombstone Semantics**:
   * Deletions are appended as immutable tombstones (`None`) rather than in-place disk mutations, completely eliminating random disk writes and wear on flash storage.
4. **Sorted String Tables (SSTables)** *(Phase 2)*:
   * Immutable 4KB block-encoded disk files with trailing sparse block indexes for fast binary search.
5. **Probabilistic Bloom Filtering** *(Phase 3)*:
   * Space-efficient bitset filter serialized into SSTable metadata to bypass ~99% of unnecessary disk seeks for non-existent keys.
6. **Leveled Compaction Engine** *(Phase 4)*:
   * Background multi-way merge sort to reclaim tombstones, eliminate stale key versions, and bound read amplification.

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
         │   ├── Data Blocks (Sorted K/V entries)                 │
         │   ├── Sparse Block Index (Offsets for binary search)   │
         │   └── Bloom Filter (Probabilistic skip for ~99% seeks) │
         └────────────────────────────────────────────────────────┘
                  │
                  ▼ (Background Multi-Way Merge Sort)
              Compaction (Reclaims tombstones & deduplicates keys)
```

---

## 🚀 Getting Started

### Using as a Library

```rust
use lsm_tree::LsmEngine;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open storage engine (automatically recovers state from disk if present)
    let db = LsmEngine::open("./data")?;

    // Insert key-value pairs
    db.put(b"user:1001", b"Bradshaw")?;
    db.put(b"user:1002", b"Asher")?;

    // Read keys
    if let Some(val) = db.get(b"user:1001")? {
        println!("Found user: {}", String::from_utf8_lossy(&val));
    }

    // Delete keys (appends a tombstone marker)
    db.delete(b"user:1001")?;
    assert_eq!(db.get(b"user:1001")?, None);

    Ok(())
}
```

---

## 🧪 Testing & Verification

Run the test suite:

```bash
cargo test
```

### Verified Test Cases
* `test_wal_write_and_recover`: Verifies sequential record parsing and opcode integrity.
* `test_wal_crc_corruption_detection`: Verifies that flipped bits or corrupted bytes in the WAL are detected via CRC32 checksum mismatch.
* `test_memtable_crud`: Verifies put, get, delete, and tombstone masking in the concurrent SkipList.
* `test_memtable_sorted_order`: Verifies strict lexicographical iteration order.
* `test_crash_recovery_from_wal`: Simulates abrupt process termination without memory flush, verifying 100% data recovery on reopen.

---

## 📄 License
MIT License.
