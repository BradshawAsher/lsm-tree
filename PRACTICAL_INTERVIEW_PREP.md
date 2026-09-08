# LSM-Tree Key-Value Engine: Conversational & Practical Interview Prep

> **Goal**: Simple, conversational English answers for real human interviewers. No textbook jargon, no academic flexing—just clear, authentic engineering stories that make you sound like a thoughtful, pragmatic colleague.

---

## 1. "Explain this project like I'm 5 (or a non-technical manager)"

> *"Imagine you run a busy restaurant kitchen. If every time a customer ordered a dish, the chef stopped everything, walked down to the basement, and put one carrot into the pantry, the restaurant would grind to a halt.*
> 
> *A traditional database (like a B-Tree) does that: every time you write data, it jumps around doing slow, random disk writes.*
> 
> *An **LSM-Tree** does the opposite: when an order comes in, the chef quickly scribbles it on a notepad on the counter (in-memory MemTable) and onto an order ticket tape (the Write-Ahead Log). Once the notepad fills up, they take the entire batch downstairs and file it neatly onto a shelf all in one go (an SSTable).*
> 
> *It turns slow, random disk updates into super-fast sequential writes, which is why engines like RocksDB power systems at Google, Meta, and Netflix."*

---

## 2. "Tell me about a difficult bug you ran into and how you fixed it"

Have **two distinct bugs** ready so you can pick the one that fits the conversation:

### Bug #1: The "Ghost" Key / Cursor Length Calculation Bug (Storage Internals)
* **Situation**: "When I was testing our multi-way merge compaction, I deleted a key in memory, flushed it to disk, and ran compaction. To my shock, after compaction finished, the deleted key was **still showing up** as active data! The deletion completely vanished."
* **Task**: "I needed to figure out why our compactor was silently ignoring tombstones (deletion markers) and letting stale data resurrect from older disk files."
* **Action**: "I stepped through the binary decoding of our 4KB data blocks. In our block parser, we read a 7-byte header for the key length and value length. Reading those numbers automatically advanced our slice cursor forward by 7 bytes. But right after, I had a safety check that tested: `if cursor.len() < 7 + key_len`. Because the cursor had already moved 7 bytes ahead, it was checking remaining bytes against an inflated requirement! When a tombstone came through with a 0-byte value, the check failed, and `get_entry()` returned `None`. The compactor thought the block was empty, dropped the tombstone, and when it read the older SSTable, it assumed the old key was still alive."
* **Result**: "I fixed the slice boundary check so it evaluated the true remaining payload length, wrote an automated test with overlapping SSTables containing overwrites and tombstones, and verified that deleted keys stayed permanently purged."

---

### Bug #2: The MemTable "Flush Flapping" Bug (Concurrency & Memory Accounting)
* **Situation**: "During our integration tests, the engine was supposed to flush memory to disk once every 4MB. But during a test run, the engine suddenly created 15 tiny SSTables within 2 milliseconds, thrashing the disk."
* **Task**: "I had to investigate why the automatic flush trigger was misfiring on tiny write volumes."
* **Action**: "I inspected our atomic memory accounting logic. In our SkipList MemTable, we don't just count the raw key and value bytes—we also account for pointer overhead, which is roughly 64 bytes per node. In our test harness, someone had configured the threshold to 64 bytes to simulate memory pressure. Because the node overhead alone was 64 bytes, literally *every single key* inserted immediately crossed the threshold, triggering a premature freeze and disk flush loop!"
* **Result**: "I decoupled the test configurations, set realistic baseline thresholds with a safety floor, and added automated assertions verifying that SSTables only flush when actual data payload boundaries are reached."

---

## 3. "What part of this project are you most proud of?"

### Proud Story #1: The Min-Heap K-Way Merge Iterator (Range Scans)
> *"I'm really proud of how we solved range scans across memory and disk.*
> 
> *In an LSM tree, data is scattered across multiple places: the active MemTable in RAM, frozen tables being flushed, and multiple SSTable files on disk. If a user asks for all keys between `user:100` and `user:200`, you can't just query one file.*
> 
> *I built a `MergeIterator` that uses a priority queue (min-heap). It streams from all active storage sources simultaneously. The min-heap always pops the smallest key, but here's the cool part: if three different files have the key `user:150`, it automatically detects the duplicates, picks the newest one based on priority, throws away the stale ones, and if it's a deleted tombstone, skips it entirely without the user ever knowing.*
> 
> *Seeing it seamlessly stream sorted, deduplicated records in real-time across disk and RAM was really rewarding."*

---

### Proud Story #2: Bypassing 99% of Disk Seeks with Bloom Filters
> *"I'm most proud of our Bloom filter implementation.*
> 
> *In database engineering, disk I/O is by far the slowest operation. If a user queries a key that doesn't exist, a naive database has to open every single file on disk to verify it's not there.*
> 
> *I implemented a space-efficient Bloom filter using Kirsch-Mitzenmacher double hashing. It takes just 10 bits of memory per key. Before the engine even thinks about touching the hard drive, it checks the in-memory filter. If the filter says 'No', we return `None` in less than a microsecond—zero disk reads.*
> 
> *When we ran Criterion microbenchmarks and saw non-existent key lookups take 200 nanoseconds instead of milliseconds because the drive head never moved, that was a huge win."*

---

## 4. Common "Easy English" Questions & Natural Answers

### "Why did you build this in Rust instead of Go, C++, or Python?"
* **Conversational Answer**:
  > *"Python is too slow for storage engines because of garbage collection pauses and interpreter overhead. Go is great for networking, but its runtime garbage collector causes unpredictable latency spikes when you're managing gigabytes of memory buffers.*
  > 
  > *C++ is traditional for databases (like RocksDB), but manual memory management makes buffer overflows and use-after-free bugs a constant danger.*
  > 
  > *Rust gave us the best of both worlds: C++ speed and low-level memory control (we control exact byte layouts in our 4KB blocks), but the compiler guarantees zero data races and zero memory corruption."*

---

### "What is the biggest tradeoff you made in this project?"
* **Conversational Answer**:
  > *"The classic trade-off in an LSM tree is **Write Speed vs. Read Complexity**.*
  > 
  > *We optimized heavily for writes: writes are blazingly fast because they just append to a log and insert into memory. But the trade-off is reads have to check multiple layers (MemTable, then Level 0 files). To pay down that read debt, we had to add complexity: Bloom filters to skip files, sparse block indexes to do binary searches inside 4KB blocks, and an LRU cache in RAM.*
  > 
  > *If our workload was 99% reads and almost zero writes, a traditional B-Tree would have been simpler."*

---

### "If you had another 3 weeks to work on this, what would you add?"
* **Conversational Answer**:
  > *"Two big things:*
  > 1. *Right now, all our SSTables sit in 'Level 0'. In production (like RocksDB), you have **Leveled Compaction (L0, L1, L2, etc.)**, where each higher level is 10x larger and keys never overlap within a level. That bounds read amplification even further.*
  > 2. *I would add **block compression** (like LZ4 or ZSTD) so our 4KB blocks are compressed before hitting disk, saving ~50% disk space with almost zero CPU penalty."*

---

### "How did you test this to know it actually worked?"
* **Conversational Answer**:
  > *"We didn't just write unit tests for the happy path. We wrote tests specifically to try to break it:*
  > 1. *We simulated sudden power outages: wrote data, terminated the process without flushing memory, reopened the database, and verified 100% of the data recovered from the WAL.*
  > 2. *We intentionally flipped random bits in the WAL file to test that our CRC32 checksums immediately caught the corruption rather than silently loading bad data.*
  > 3. *We benchmarked it with Criterion to ensure lookups and writes didn't regress across commits."*
