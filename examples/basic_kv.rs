use lsm_tree::LsmEngine;
use tempfile::tempdir;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== LSM-Tree Key-Value Engine Demo ===");

    let temp_dir = tempdir()?;
    let path = temp_dir.path();
    println!("Initializing engine at: {:?}", path);

    // Open engine with small 512-byte MemTable to demonstrate automatic SSTable flushes
    let db = LsmEngine::open_with_options(path, 512)?;

    println!("\n1. Writing initial key-value pairs (Active MemTable)...");
    db.put(b"user:001", b"{\"name\": \"Bradshaw\", \"role\": \"Staff Engineer\"}")?;
    db.put(b"user:002", b"{\"name\": \"Alice\", \"role\": \"Core Database Architect\"}")?;
    db.put(b"user:003", b"{\"name\": \"Bob\", \"role\": \"Distributed Systems Engineer\"}")?;

    println!("   MemTable memory usage: {} bytes", db.memtable_size());
    println!("   Active SSTable count on disk: {}", db.sstable_count());

    println!("\n2. Querying stored keys...");
    if let Some(val) = db.get(b"user:001")? {
        println!("   user:001 => {}", String::from_utf8_lossy(&val));
    }

    println!("\n3. Adding more data to trigger automatic background SSTable flush...");
    for i in 10..40 {
        let k = format!("metric:cluster_node_{:03}", i);
        let v = format!("{{\"cpu\": 42.5, \"mem_mb\": 16384, \"seq\": {}}}", i);
        db.put(k.as_bytes(), v.as_bytes())?;
    }

    println!("   Active SSTable count on disk: {} file(s)", db.sstable_count());

    println!("\n4. Deleting a key (Writing tombstone)...");
    db.delete(b"user:002")?;
    println!("   get(user:002) => {:?}", db.get(b"user:002")?);

    println!("\n5. Running Compaction (Multi-way merge sort)...");
    println!("   SSTable count before compaction: {}", db.sstable_count());
    db.compact()?;
    println!("   SSTable count after compaction: {} (Merged & deduplicated!)", db.sstable_count());

    println!("\n6. Verifying keys after compaction...");
    assert_eq!(db.get(b"user:001")?.is_some(), true);
    assert_eq!(db.get(b"user:002")?, None); // Confirmed purged
    println!("   user:001 successfully retrieved!");
    println!("   user:002 confirmed purged via bottom-level tombstone eviction!");

    println!("\n✅ LSM-Tree Demo completed successfully!");

    Ok(())
}
