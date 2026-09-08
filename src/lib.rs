pub mod compaction;
pub mod error;
pub mod filter;
pub mod memtable;
pub mod sstable;
pub mod wal;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::{Mutex, RwLock};

use crate::error::Result;
use crate::memtable::MemTable;
use crate::sstable::{SsTableBuilder, SsTableReader};
use crate::wal::{WalOp, WalReader, WalWriter};

pub const DEFAULT_MAX_MEMTABLE_SIZE: usize = 4 * 1024 * 1024; // 4MB

pub struct LsmEngine {
    dir: PathBuf,
    memtable: RwLock<Arc<MemTable>>,
    imm_memtables: RwLock<Vec<Arc<MemTable>>>,
    sstables: RwLock<Vec<SsTableReader>>, // Ordered from newest (idx 0) to oldest
    wal_writer: Mutex<WalWriter>,
    next_sst_id: AtomicU64,
    max_memtable_size: usize,
}

impl LsmEngine {
    /// Opens an LSM storage engine at the specified directory with default 4MB memtable threshold.
    pub fn open<P: AsRef<Path>>(dir: P) -> Result<Self> {
        Self::open_with_options(dir, DEFAULT_MAX_MEMTABLE_SIZE)
    }

    /// Opens an LSM storage engine with custom memtable size threshold.
    pub fn open_with_options<P: AsRef<Path>>(dir: P, max_memtable_size: usize) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;

        // 1. Discover and load existing SSTables (.sst files)
        let mut sst_files = Vec::new();
        let mut max_id = 0u64;

        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("sst") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Ok(id) = stem.parse::<u64>() {
                        max_id = max_id.max(id);
                        sst_files.push((id, path));
                    }
                }
            }
        }

        // Sort descending so index 0 is the newest SSTable
        sst_files.sort_by(|a, b| b.0.cmp(&a.0));
        let mut sstables = Vec::with_capacity(sst_files.len());
        for (_, path) in sst_files {
            sstables.push(SsTableReader::open(&path)?);
        }

        // 2. Recover active MemTable from WAL
        let wal_path = dir.join("active.wal");
        let memtable = Arc::new(MemTable::new());

        if wal_path.exists() {
            let entries = WalReader::read_all(&wal_path)?;
            for entry in entries {
                match entry.op {
                    WalOp::Put => {
                        if let Some(val) = entry.value {
                            memtable.put(entry.key, val);
                        }
                    }
                    WalOp::Delete => {
                        memtable.delete(entry.key);
                    }
                }
            }
        }

        let wal_writer = Mutex::new(WalWriter::open(&wal_path)?);

        Ok(Self {
            dir,
            memtable: RwLock::new(memtable),
            imm_memtables: RwLock::new(Vec::new()),
            sstables: RwLock::new(sstables),
            wal_writer,
            next_sst_id: AtomicU64::new(max_id + 1),
            max_memtable_size,
        })
    }

    /// Stores a key-value pair.
    /// Writes to the append-only WAL first, then memory. Flushes to SSTable if threshold is reached.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        {
            let mut writer = self.wal_writer.lock();
            writer.append_put(key, value)?;
            writer.sync()?;
        }

        let memtable = self.memtable.read().clone();
        memtable.put(Bytes::copy_from_slice(key), Bytes::copy_from_slice(value));

        if memtable.size_bytes() >= self.max_memtable_size {
            self.flush_memtable()?;
        }

        Ok(())
    }

    /// Deletes a key by appending a tombstone.
    pub fn delete(&self, key: &[u8]) -> Result<()> {
        {
            let mut writer = self.wal_writer.lock();
            writer.append_delete(key)?;
            writer.sync()?;
        }

        let memtable = self.memtable.read().clone();
        memtable.delete(Bytes::copy_from_slice(key));

        if memtable.size_bytes() >= self.max_memtable_size {
            self.flush_memtable()?;
        }

        Ok(())
    }

    /// Retrieves a value by key.
    /// Traverses the storage hierarchy: MemTable -> Immutable MemTables -> SSTables (Newest to Oldest).
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        // Tier 1: Check active mutable MemTable
        {
            let memtable = self.memtable.read();
            match memtable.get(key) {
                Some(Some(val)) => return Ok(Some(val)),
                Some(None) => return Ok(None), // Tombstone means deleted!
                None => {}
            }
        }

        // Tier 2: Check immutable MemTables being flushed
        {
            let imm_tables = self.imm_memtables.read();
            for imm in imm_tables.iter().rev() {
                match imm.get(key) {
                    Some(Some(val)) => return Ok(Some(val)),
                    Some(None) => return Ok(None),
                    None => {}
                }
            }
        }

        // Tier 3: Search SSTables from newest to oldest
        {
            let mut sstables = self.sstables.write();
            for sstable in sstables.iter_mut() {
                match sstable.get(key)? {
                    Some(Some(val)) => return Ok(Some(val)),
                    Some(None) => return Ok(None), // Masked by disk tombstone
                    None => {}                    // Bypassed via Bloom Filter or Block Index
                }
            }
        }

        Ok(None)
    }

    /// Flushes the active MemTable to a new immutable SSTable file on disk.
    pub fn flush_memtable(&self) -> Result<()> {
        // 1. Freeze active MemTable and swap with a fresh one
        let old_memtable = {
            let mut mem_guard = self.memtable.write();
            let frozen = std::mem::replace(&mut *mem_guard, Arc::new(MemTable::new()));
            self.imm_memtables.write().push(frozen.clone());
            frozen
        };

        if old_memtable.is_empty() {
            self.imm_memtables.write().pop();
            return Ok(());
        }

        // 2. Assign unique SSTable filename
        let sst_id = self.next_sst_id.fetch_add(1, Ordering::SeqCst);
        let sst_path = self.dir.join(format!("{:06}.sst", sst_id));

        // 3. Write sorted entries to new SSTable
        let mut builder = SsTableBuilder::create(&sst_path)?;
        for (k, v_opt) in old_memtable.iter() {
            builder.add(&k, v_opt.as_deref())?;
        }
        builder.finish()?;

        // 4. Open reader and register as newest SSTable
        let reader = SsTableReader::open(&sst_path)?;
        self.sstables.write().insert(0, reader);

        // 5. Remove from immutable queue and cycle WAL
        self.imm_memtables.write().retain(|m| !Arc::ptr_eq(m, &old_memtable));

        // Truncate WAL for the new active MemTable
        let wal_path = self.dir.join("active.wal");
        let mut writer_guard = self.wal_writer.lock();
        *writer_guard = WalWriter::open(&wal_path)?;

        Ok(())
    }

    /// Number of active SSTables on disk.
    pub fn sstable_count(&self) -> usize {
        self.sstables.read().len()
    }

    /// Total memory consumption of active MemTable in bytes.
    pub fn memtable_size(&self) -> usize {
        self.memtable.read().size_bytes()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Compacts all on-disk SSTables into a single unified SSTable,
    /// purging tombstones and deduplicating overwritten keys.
    pub fn compact(&self) -> Result<()> {
        let mut sstables_guard = self.sstables.write();
        if sstables_guard.len() <= 1 {
            return Ok(());
        }

        let new_id = self.next_sst_id.fetch_add(1, Ordering::SeqCst);
        let (new_path, _) = crate::compaction::compact_sstables(
            &self.dir,
            &mut *sstables_guard,
            new_id,
            true, // Bottom level: safe to evict tombstones
        )?;

        let compacted_reader = SsTableReader::open(&new_path)?;
        *sstables_guard = vec![compacted_reader];

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_engine_crud() -> Result<()> {
        let temp = tempdir()?;
        let db = LsmEngine::open(temp.path())?;

        db.put(b"order:100", b"status=pending")?;
        assert_eq!(db.get(b"order:100")?, Some(Bytes::from("status=pending")));

        db.put(b"order:100", b"status=completed")?;
        assert_eq!(db.get(b"order:100")?, Some(Bytes::from("status=completed")));

        db.delete(b"order:100")?;
        assert_eq!(db.get(b"order:100")?, None);

        Ok(())
    }

    #[test]
    fn test_automatic_flush_to_sstable() -> Result<()> {
        let temp = tempdir()?;
        // Small 256-byte threshold to force flush
        let db = LsmEngine::open_with_options(temp.path(), 256)?;

        assert_eq!(db.sstable_count(), 0);

        // Insert enough records to trigger automatic flush
        for i in 0..20 {
            let k = format!("sensor:{:02}", i);
            let v = format!("metric_payload_data_{:04}", i);
            db.put(k.as_bytes(), v.as_bytes())?;
        }

        // Should have created at least 1 SSTable on disk
        assert!(db.sstable_count() >= 1);

        // Verify all 20 records are still readable across MemTable and SSTables
        for i in 0..20 {
            let k = format!("sensor:{:02}", i);
            let expected_v = format!("metric_payload_data_{:04}", i);
            assert_eq!(db.get(k.as_bytes())?, Some(Bytes::from(expected_v)));
        }

        // Verify tombstone across SSTable
        db.delete(b"sensor:05")?;
        assert_eq!(db.get(b"sensor:05")?, None);

        Ok(())
    }

    #[test]
    fn test_persistence_across_full_restart() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path();

        // 1. Session 1: Put keys, force flush, put more keys, exit
        {
            let db = LsmEngine::open_with_options(path, 256)?;
            db.put(b"config:timeout", b"30s")?;
            db.put(b"config:retries", b"5")?;
            db.flush_memtable()?; // Forced disk flush -> 000001.sst
            db.put(b"config:host", b"127.0.0.1")?;
        }

        // 2. Session 2: Reopen DB from scratch and verify everything is intact
        {
            let db = LsmEngine::open(path)?;
            assert!(db.sstable_count() >= 1);
            assert_eq!(db.get(b"config:timeout")?, Some(Bytes::from("30s")));
            assert_eq!(db.get(b"config:retries")?, Some(Bytes::from("5")));
            assert_eq!(db.get(b"config:host")?, Some(Bytes::from("127.0.0.1")));
            assert_eq!(db.get(b"config:unknown")?, None);
        }

        Ok(())
    }

    #[test]
    fn test_engine_compaction() -> Result<()> {
        let temp = tempdir()?;
        let db = LsmEngine::open_with_options(temp.path(), 4096)?;

        // Batch 1: Flushed to SSTable 1
        db.put(b"k1", b"v1_old")?;
        db.put(b"k2", b"v2")?;
        db.flush_memtable()?;

        // Batch 2: Overwrite k1, delete k2, add k3 -> Flushed to SSTable 2
        db.put(b"k1", b"v1_new")?;
        db.delete(b"k2")?;
        db.put(b"k3", b"v3")?;
        db.flush_memtable()?;

        assert_eq!(db.sstable_count(), 2);

        // Run background compaction
        db.compact()?;

        // After compaction, both SSTables merged into 1 single SSTable
        assert_eq!(db.sstable_count(), 1);

        // Verify values
        assert_eq!(db.get(b"k1")?, Some(Bytes::from("v1_new")));
        assert_eq!(db.get(b"k2")?, None); // Purged
        assert_eq!(db.get(b"k3")?, Some(Bytes::from("v3")));

        Ok(())
    }
}
