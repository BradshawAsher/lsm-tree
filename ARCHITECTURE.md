# LSM-Tree Architecture & Internal Design

This document describes the architectural principles, on-disk binary formats, memory structures, and concurrency model of the **LSM-Tree Key-Value Storage Engine**.

---

## 1. System Topology & Data Flow

```
                      +---------------------------------------+
                      |           Client Write API            |
                      |        put(k, v) / delete(k)          |
                      +-------------------+-------------------+
                                          |
                        +-----------------+-----------------+
                        |                                   |
                        v                                   v
             +--------------------+               +--------------------+
             |   Append-Only WAL  |               | Lock-Free MemTable |
             | (CRC32 Frame Disk) |               |  (RAM - SkipList)  |
             +--------------------+               +---------+----------+
                                                            |
                                               Threshold Exceeded (4MB)
                                                            |
                                                            v
                                                  +--------------------+
                                                  | Immutable MemTable |
                                                  +---------+----------+
                                                            |
                                                     Background Flush
                                                            |
                                                            v
                                               +-------------------------+
                                               |  Level 0 SSTable (Disk) |
                                               | - 4KB Data Blocks       |
                                               | - Sparse Block Index    |
                                               | - Bloom Filter          |
                                               | - 40B Trailer Footer    |
                                               +------------+------------+
                                                            |
                                                    Multi-Way Merge
                                                            |
                                                            v
                                               +-------------------------+
                                               | Compacted Unified SST   |
                                               | (Purged tombstones)     |
                                               +-------------------------+
```

---

## 2. On-Disk Binary Formats

All integers in binary headers and footers are encoded in **big-endian (network byte order)** to guarantee deterministic cross-platform compatibility.

### 2.1. Write-Ahead Log (WAL) Record Format

Every write operation (`Put` or `Delete`) appends a binary framed record to `active.wal`:

```
+---------------+----------------+----------------+-------------+------------+--------------+
| CRC32 Checksum| Key Length (K) | Val Length (V) | Opcode (Op) | Key Bytes  | Value Bytes  |
|    (4 Bytes)  |   (2 Bytes)    |   (4 Bytes)    |  (1 Byte)   | (K Bytes)  |  (V Bytes)   |
+---------------+----------------+----------------+-------------+------------+--------------+
```

* **CRC32 Checksum (4 Bytes)**: Calculated across `KeyLen + ValLen + Opcode + Key + Value` using IEEE 802.3 polynomial via `crc32fast`. Detects bit rot, disk corruption, and torn writes during power cuts.
* **Key Length (2 Bytes)**: Supports keys up to $65,535$ bytes ($64\text{ KB}$).
* **Value Length (4 Bytes)**: Supports values up to $4\text{ GB}$. For `Delete` operations, `ValLen` is $0$.
* **Opcode (1 Byte)**:
  * `0x00`: `Put` operation.
  * `0x01`: `Delete` operation (Tombstone).
* **Payload**: Raw byte slices of `Key` followed by `Value`.

---

### 2.2. SSTable File Layout

An SSTable is an immutable disk file divided into data blocks, an index block, a probabilistic filter block, and a fixed trailer footer:

```
+-------------------------------------------------------+
| Data Block 0 (Up to 4KB of packed sorted entries)     |
+-------------------------------------------------------+
| Data Block 1                                          |
+-------------------------------------------------------+
| ...                                                   |
+-------------------------------------------------------+
| Data Block N-1                                        |
+-------------------------------------------------------+
| Sparse Block Index                                    |
| - Block 0: [FirstKeyLen: 2B][FirstKey][Offset: 8B][Len: 8B]
| - Block 1: [FirstKeyLen: 2B][FirstKey][Offset: 8B][Len: 8B]
| - ...                                                 |
+-------------------------------------------------------+
| Bloom Filter Bitset                                   |
| - [NumBits: 8B][NumHashes: 4B][Bitset Array]          |
+-------------------------------------------------------+
| Trailer Footer (Fixed 40 Bytes)                       |
| - Index Offset:  8 Bytes                              |
| - Index Length:  8 Bytes                              |
| - Filter Offset: 8 Bytes                              |
| - Filter Length: 8 Bytes                              |
| - Magic Number:  8 Bytes (0x4C534D5452454531)         |
+-------------------------------------------------------+
```

---

### 2.3. Data Block Format (4KB Chunks)

Each data block packs sorted key-value entries with a trailing offset directory to allow $O(\log N)$ intra-block binary search without scanning from block start:

```
+-------------------------------------------------------+
| Entry 0: [KeyLen: 2B][ValLen: 4B][Tombstone: 1B][K][V]|
+-------------------------------------------------------+
| Entry 1: [KeyLen: 2B][ValLen: 4B][Tombstone: 1B][K][V]|
+-------------------------------------------------------+
| ...                                                   |
+-------------------------------------------------------+
| Entry M-1                                             |
+-------------------------------------------------------+
| Offset 0 (2 Bytes, u16)                               |
+-------------------------------------------------------+
| Offset 1 (2 Bytes, u16)                               |
+-------------------------------------------------------+
| ...                                                   |
+-------------------------------------------------------+
| Offset M-1 (2 Bytes, u16)                             |
+-------------------------------------------------------+
| Entry Count M (2 Bytes, u16)                          |
+-------------------------------------------------------+
```

* **Binary Search via Offsets**: To find a key within a decoded 4KB block, the reader binary searches the offset table. It parses only the candidate keys at the indexed offsets, avoiding linear deserialization.
* **Block Size Limit**: Standard target size is $4,096$ bytes ($4\text{ KB}$), matching modern NVMe and OS page cache boundaries.

---

### 2.4. Fixed 40-Byte Trailer Footer

The footer sits at the very end of every SSTable file (`file_len - 40`):

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                      Index Offset (u64)                       |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                      Index Length (u64)                       |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                     Filter Offset (u64)                       |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                     Filter Length (u64)                       |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|               Magic Bytes: 0x4C534D5452454531 ("LSMTREE1")    |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

---

## 3. In-Memory Components & Algorithms

### 3.1. Concurrent Lock-Free MemTable
* **Data Structure**: `crossbeam-skiplist::SkipMap<Bytes, Option<Bytes>>`.
* **Lock-Free Concurrency**: Read and write operations execute without acquiring thread locks, allowing hundreds of concurrent threads to insert and query simultaneously.
* **Memory Tracking**: Real-time atomic byte counter accounts for key length, value length, and estimated node overhead ($64\text{ bytes}$ per tower entry).
* **Freeze & Swap**: When atomic size exceeds `max_memtable_size` (default $4\text{ MB}$), the active buffer is frozen into an immutable `Arc<MemTable>` and queued for flush.

---

### 3.2. Probabilistic Bloom Filtering
* **Kirsch-Mitzenmacher Double Hashing**: Instead of computing $k$ independent cryptographic hash functions, two 64-bit FNV-1a hashes ($h_1, h_2$) generate $k$ hash positions:
  $$g_i(x) = (h_1(x) + i \cdot h_2(x)) \pmod m$$
* **Optimal Parameterization**:
  * Bits per key: $m/n = 10\text{ bits}$.
  * Number of hashes: $k = (m/n) \cdot \ln 2 \approx 7$.
  * False Positive Probability:
    $$p \approx \left(1 - e^{-k n / m}\right)^k \approx (1 - e^{-7 / 10})^7 \approx 0.00819 \approx 0.82\%$$
* **Disk I/O Bypass**: If the Bloom filter returns `false`, the key is **mathematically guaranteed not to exist** in that SSTable. The engine bypasses disk reads entirely (~$99\%$ seek reduction).

---

### 3.3. In-Memory LRU Block Cache
* **Structure**: Thread-safe Least-Recently-Used (LRU) cache (`BlockCache`) protected by `parking_lot::Mutex`.
* **Cache Key**: `BlockKey { sst_id: u64, offset: u64 }`.
* **Value**: `Arc<Block>` (already decoded in RAM).
* **Efficiency**: Uses a doubly-linked list with array indices and a free-slot recycling pool. All insertions, cache hits, and evictions run in strictly $O(1)$ time with zero memory allocations during steady-state execution.
* **Cold vs Warm Reads**:
  * Cold read: Reads 4KB block from disk, decodes, caches, and returns.
  * Warm read: Instantaneous RAM pointer clone with zero disk I/O and zero decoding overhead.

---

### 3.4. K-Way Merge Range Iterator
* **Algorithm**: Priority queue min-heap (`std::collections::BinaryHeap`) coordinating $K$ active iterators across MemTable, immutable buffers, and on-disk SSTables.
* **Monotonic Priority Tagging**:
  * Priority 0: Active mutable MemTable (freshest).
  * Priority 1..$K$: Frozen immutable MemTables.
  * Priority $K+1..N$: SSTables ordered descending by ID (newest to oldest).
* **Live Deduplication**: When multiple streams yield the identical key, the smallest `iterator_idx` (freshest) wins, and older duplicates are popped and discarded on the fly.
* **Tombstone Pruning**: If the winning entry has value `None`, it is suppressed from the user stream.

---

## 4. Concurrency & Synchronization Model

| Component | Synchronization Primitive | Contention Scope |
|---|---|---|
| **Active MemTable** | Lock-Free Concurrent SkipList (`crossbeam-skiplist`) | Zero lock contention across threads |
| **Active WAL Writer** | `parking_lot::Mutex<WalWriter>` | Serialized disk append & `fsync` |
| **Immutable MemTables** | `parking_lot::RwLock<Vec<Arc<MemTable>>>` | Read-locked during queries, write-locked briefly during flush swap |
| **SSTable Readers** | `parking_lot::RwLock<Vec<SsTableReader>>` | Read-locked during point lookups & scan setup; write-locked only on compaction/flush registration |
| **LRU Block Cache** | `parking_lot::Mutex<BlockCache>` | Ultra-short pointer updates ($O(1)$ array manipulation) |
| **SST ID Counter** | `std::sync::atomic::AtomicU64` | Atomic `fetch_add` (Lock-free) |

---

## 5. Amplification Factors (RUM Tradeoffs)

LSM-Trees optimize write throughput by converting random I/O into sequential disk appends, trading off read and space amplification:

* **Write Amplification ($WA$)**:
  * Data is written once to the WAL, once when flushing to Level 0 SSTables, and subsequently during compaction.
  * For $L$ levels with fanout $T$, theoretical write amplification is bounded by $O(T \cdot L)$.
* **Read Amplification ($RA$)**:
  * Mitigated by:
    1. Lock-free in-memory MemTable check ($O(\log N)$).
    2. Bloom filters (eliminates ~$99\%$ of SSTable reads).
    3. Sparse Block Index (reduces disk read to exactly one 4KB block).
    4. LRU Block Cache (eliminates disk reads for hot data).
* **Space Amplification ($SA$)**:
  * Overwritten values and deleted tombstones occupy disk space until purged by multi-way merge compaction.
  * Bottom-level compaction completely reclaims deleted keys, returning disk space to the OS.
