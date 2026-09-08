# LSM-Tree Key-Value Storage Engine: Technical Interview Guide

This guide is designed for **Staff, Principal, and Senior Systems / Database Infrastructure** interviews. It covers the core design decisions, algorithmic trade-offs, low-level binary framing, and deep-dive technical questions about this LSM-Tree engine.

---

## 1. The 60-Second Elevator Pitch

> *"I engineered a crash-resilient, embedded Log-Structured Merge-Tree (LSM-Tree) storage engine in Rust from first principles, modeled on the core architecture of RocksDB and LevelDB.*
> 
> *The engine addresses the classic RUM (Read-Update-Memory) trade-off by turning random disk mutations into sequential appends. It features an append-only WAL with CRC32 framing and fsync durability, a concurrent lock-free SkipList MemTable with atomic memory tracking, immutable 4KB block-encoded SSTables with binary-searchable block offsets, space-efficient Bloom filters with Kirsch-Mitzenmacher double hashing that eliminate ~99% of non-existent disk seeks, an in-memory LRU block cache for hot reads, and a min-heap K-way MergeIterator for live deduplication during range scans and compaction.*
> 
> *The entire codebase is verified with unit and crash-recovery tests and comprehensive Criterion microbenchmarks."*

---

## 2. Core Architectural Questions & Deep Dives

### Q1: Why an LSM-Tree instead of a B+Tree?
* **Trade-off Analysis**:
  * **B+Tree**: Optimized for **read-heavy** workloads with low read amplification ($O(\log_B N)$). However, in-place updates require random disk writes across pages, causing severe write amplification and premature flash drive wear on SSDs/NVMe.
  * **LSM-Tree**: Optimized for **write-heavy** workloads. Writes are sequentially appended to a Write-Ahead Log (WAL) and buffered in an in-memory SkipList. Sequential I/O achieves near-hardware maximum disk write throughput on flash and spinning disks.
* **Cost**: LSM-Trees trade off write speed for increased Read Amplification (must check MemTable $\to$ Immutable MemTables $\to$ SSTables) and Space Amplification (stale versions exist until compacted). We mitigate read amplification with **Bloom filters**, **sparse block indexes**, and **LRU block caching**.

---

### Q2: How is durability and crash resilience guaranteed? What prevents torn writes?
* **Append-Only WAL**: Every `put` and `delete` is written to `active.wal` before touching the in-memory MemTable.
* **CRC32 Binary Framing**:
  * Each record is framed: `[CRC32: 4B][KeyLen: 2B][ValLen: 4B][Op: 1B][Key][Val]`.
  * The CRC32 checksum (IEEE 802.3 via hardware-accelerated polynomial) covers all bytes after the checksum.
* **Crash Recovery Mechanics**:
  * During replay on startup, if a server crashes mid-write, the trailing torn write will fail the CRC32 verification or return an unexpected EOF.
  * The parser detects the torn boundary, halts playback safely at the last valid transaction, and recovers the exact uncorrupted state.
* **OS Buffer Flushes**: We call `file.sync_data()` / `sync_all()` (`fsync`), ensuring records are flushed from the OS page cache to physical non-volatile storage.

---

### Q3: Why a SkipList for the MemTable instead of a Red-Black Tree or B-Tree?
* **Lock-Free Concurrency**:
  * Self-balancing binary trees (like Red-Black or AVL trees) require rotations during insertions and rebalancing. Rotations mutate multiple parent/child pointers across tree levels, requiring global mutex locks or heavy lock coupling.
  * A **SkipList** (`crossbeam-skiplist`) only modifies local forward pointers with atomic Compare-And-Swap (CAS) operations. This enables completely **lock-free concurrent writes and lock-free reads** with zero thread contention.
* **Iterative Slicing**: SkipLists maintain sorted order at Level 0, making sequential range scans and flushing to disk a simple pointer walk ($O(N)$).

---

### Q4: Explain the Bloom filter mathematics and why Kirsch-Mitzenmacher double-hashing was used.
* **Kirsch-Mitzenmacher Optimization**:
  * Computing $k$ distinct cryptographic hashes (like SHA-256 or Murmur3) is CPU-intensive.
  * Kirsch and Mitzenmacher proved that two hash functions $h_1(x)$ and $h_2(x)$ can simulate $k$ independent hash functions without asymptotic loss in error rate:
    $$g_i(x) = (h_1(x) + i \cdot h_2(x)) \pmod m$$
  * We use two 64-bit FNV-1a variants for $h_1$ and $h_2$.
* **Parameter Sizing**:
  * Target False Positive Rate ($p$): $\approx 1\%$ ($0.0082$).
  * Bits per key ($m/n$): $10\text{ bits}$.
  * Optimal number of hash functions ($k$):
    $$k = \frac{m}{n} \ln 2 \approx 10 \cdot 0.693 \approx 7$$
* **Impact**: 99% of queries for non-existent keys are rejected in RAM before issuing a single disk read seek.

---

### Q5: What is the anatomy of a 4KB SSTable Data Block? Why use trailing offsets?
* **Variable-Length Challenge**: Keys and values have variable lengths. If records were packed sequentially without an index, finding a key inside a block would require linear scanning from the beginning ($O(N)$).
* **Trailing Offset Array**:
  * Entries are packed consecutively at the front of the block.
  * A 2-byte offset array (`u16`) is appended to the tail of the block, followed by the entry count.
* **Binary Search**:
  * The reader reads the count, binary searches the offset table in $O(\log N)$ steps, and seeks directly to the target entry within the block.
  * Aligns perfectly with standard $4\text{ KB}$ OS memory pages and hardware sector sizes.

---

### Q6: How does Deletion work? What are Tombstones and when can they be purged?
* **Tombstone Semantics**:
  * In an LSM-Tree, you cannot perform an in-place delete on disk because previous SSTables are strictly immutable.
  * Instead, a delete is an insert of a special marker: a **Tombstone** (`Opcode::Delete` in WAL, `None` value in SSTable).
* **Masking**:
  * When reading, if the freshest entry encountered across the hierarchy is a tombstone, the engine returns `None` (key does not exist).
* **When is it safe to purge a tombstone?**:
  * **CRITICAL TRAP QUESTION**: If you discard a tombstone while an older SSTable still holds an earlier version of that key, the old version will "resurrect"!
  * **Answer**: A tombstone can **only be purged during bottom-level compaction**, when there are no older SSTables below it that could contain an obsolete version of the key.

---

### Q7: How does Range Scan work across multiple SSTables and MemTables?
* **Algorithm**: $K$-way Merge Sort using a Priority Queue (`std::collections::BinaryHeap`).
* **Source Ordering & Priority**:
  * Each source iterator is tagged with a priority based on freshness:
    1. Active MemTable (Priority 0 - Freshest)
    2. Immutable MemTables (Priority 1..$K$)
    3. SSTables (Priority $K+1..N$ - Newest to Oldest)
* **Live Deduplication**:
  * The min-heap always pops the lexicographically smallest key.
  * If multiple iterators contain the same key, the entry with the lowest priority number (freshest) wins. All older versions in the heap are popped and discarded immediately.
* **Tombstone Filtering**:
  * If the freshest entry's value is a tombstone (`None`), it is dropped from the emitted stream.

---

### Q8: How does the In-Memory LRU Block Cache work? How do you prevent cache pollution?
* **Implementation**:
  * Thread-safe LRU cache (`BlockCache`) protected by a fine-grained mutex.
  * Key: `(sst_id, block_offset)`.
  * Value: `Arc<Block>` (pre-decoded block structure).
  * Storage: Intrusive doubly-linked list with array indices + free slot reuse to eliminate memory allocation overhead on the hot path.
* **Performance Impact**:
  * Cold read: 1 disk seek + decode + cache insertion.
  * Warm read: Direct RAM pointer clone (zero disk I/O, zero decoding).
* **Cache Pollution Prevention**:
  * Large sequential scans can evict hot point-lookup blocks. In production engines, scans either use a small ring buffer or bypass the block cache (`fill_cache = false`) to keep the primary working set resident in RAM.

---

### Q9: What happens if the system crashes in the middle of compaction?
* **Atomicity Protocol**:
  1. Compaction reads old SSTables and writes the unified output to a brand new file with a temporary/new ID (`000003.sst`).
  2. The new file is fully written and `fsync`'d.
  3. The engine atomically updates its in-memory SSTable directory manifest.
  4. The old SSTable files (`000001.sst`, `000002.sst`) are unlinked from disk.
* **Crash Resilience**:
  * If the node crashes at any point during steps 1–2, the new file is either partially written or unreferenced. On restart, the engine inspects the manifest/directory, ignores incomplete files, and continues running off the intact original SSTables. Zero data loss or corruption.

---

### Q10: Why not just use `mmap` (Memory-Mapped Files)?
* **The `mmap` Anti-Pattern in Storage Engines**:
  1. **Unhandled I/O Errors**: If an I/O error or physical disk failure occurs during an `mmap` read, the OS triggers a `SIGBUS` signal, crashing the entire process unless complex signal handlers are configured.
  2. **Zero Control over Eviction**: The OS page cache decides when to flush dirty pages and when to evict blocks, ignoring LSM-specific semantics (e.g., evicting index blocks before data blocks).
  3. **Page Fault Latency**: Thread pools can experience random 10–50ms page-fault stalls during reads that cannot be scheduled asynchronously.
  4. `File::seek` + `File::read` with an explicit in-memory LRU block cache provides deterministic latency and error handling.

---

## 3. Systems Engineering "Trap" Questions

| Interviewer Question | The Trap | The Winning Answer |
|---|---|---|
| *"Can we use SHA-256 instead of CRC32 for WAL records?"* | Assuming stronger cryptographic hashes are always better. | No. WAL writing is on the latency-critical path. Hardware-accelerated CRC32 (SSE4.2/ARMv8) runs at **>10 GB/s** per core with near-zero CPU footprint. SHA-256 adds microsecond latency per write for zero added durability benefit against non-adversarial disk corruption. |
| *"Why not lock the entire database with an RwLock?"* | Coarse-grained locking kills multi-core scaling. | A global RwLock serializes concurrent readers and blocks all reads during flushes. We decouple active writes (lock-free SkipList), WAL serialization (separate mutex), and SSTable reads (immutable file pointers). |
| *"What causes Write Stalls in an LSM-tree?"* | Blaming disk speed alone. | Write stalls occur when the **compaction scheduler lags behind ingestion**. If Level 0 SSTables accumulate too fast, read amplification spikes; the engine must throttle client writes to give background compaction time to merge levels. |
| *"How do you size a Bloom Filter?"* | Picking arbitrary bit counts. | Use the formula $m = -n \ln(p) / (\ln 2)^2$. For $1\%$ FPR, $m/n \approx 9.6$ (10 bits/key) with $k = 7$ hashes. Sizing below 8 bits causes FPR to jump to 5-10%, doubling read amplification. |

---

## 4. Architectural Sizing & Numbers Cheat Sheet

* **Default MemTable Size**: $4\text{ MB}$ (Flushes when size exceeds threshold).
* **Data Block Target**: $4\text{ KB}$ (Matches NVMe hardware blocks & OS page boundaries).
* **Bloom Filter**: $10\text{ bits/key}$, $k=7$ hashes, $\approx 0.82\%$ false positive rate.
* **SSTable Trailer**: Fixed $40\text{ bytes}$ at end of file (`magic: 0x4C534D5452454531`).
* **LRU Block Cache**: Default $1,024\text{ blocks}$ ($\approx 4\text{ MB}$ decoded in RAM).
* **WAL Record Overhead**: $11\text{ bytes}$ fixed binary header per entry.
