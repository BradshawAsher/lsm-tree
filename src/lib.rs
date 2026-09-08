pub mod error;
pub mod memtable;
pub mod wal;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;

use crate::error::Result;
use crate::memtable::MemTable;
use crate::wal::{WalOp, WalReader, WalWriter};

pub struct LsmEngine {
    dir: PathBuf,
    memtable: Arc<MemTable>,
    wal_writer: Mutex<WalWriter>,
}

impl LsmEngine {
    /// Opens an LSM storage engine at the specified directory.
    /// If an existing WAL log is detected, it replays records to recover memory state.
    pub fn open<P: AsRef<Path>>(dir: P) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;

        let wal_path = dir.join("active.wal");
        let memtable = Arc::new(MemTable::new());

        // Crash Recovery: Replay WAL if it exists
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
            memtable,
            wal_writer,
        })
    }

    /// Stores a key-value pair.
    /// Guarantees durability by appending to disk WAL before updating the MemTable.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        {
            let mut writer = self.wal_writer.lock();
            writer.append_put(key, value)?;
            writer.sync()?;
        }
        self.memtable.put(Bytes::copy_from_slice(key), Bytes::copy_from_slice(value));
        Ok(())
    }

    /// Deletes a key by writing a tombstone.
    pub fn delete(&self, key: &[u8]) -> Result<()> {
        {
            let mut writer = self.wal_writer.lock();
            writer.append_delete(key)?;
            writer.sync()?;
        }
        self.memtable.delete(Bytes::copy_from_slice(key));
        Ok(())
    }

    /// Retrieves a value by key.
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        match self.memtable.get(key) {
            Some(Some(val)) => Ok(Some(val)),
            Some(None) => Ok(None), // Tombstone means explicitly deleted
            None => Ok(None),       // Not in memtable (Phase 2 will search SSTables)
        }
    }

    /// Returns the approximate memory usage of the active MemTable in bytes.
    pub fn memtable_size(&self) -> usize {
        self.memtable.size_bytes()
    }

    /// Returns the directory path for this database.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_engine_basic_crud() -> Result<()> {
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
    fn test_crash_recovery_from_wal() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path();

        // 1. First session: Write some records and simulate a process exit/crash
        {
            let db = LsmEngine::open(path)?;
            db.put(b"session:1", b"token_alpha")?;
            db.put(b"session:2", b"token_beta")?;
            db.put(b"session:3", b"token_gamma")?;
            db.delete(b"session:2")?;
            // Process exits here without flushing to SSTable!
        }

        // 2. Second session: Reopen DB at the exact same directory
        {
            let db = LsmEngine::open(path)?;
            assert_eq!(db.get(b"session:1")?, Some(Bytes::from("token_alpha")));
            assert_eq!(db.get(b"session:2")?, None); // Deleted tombstone verified
            assert_eq!(db.get(b"session:3")?, Some(Bytes::from("token_gamma")));
        }

        Ok(())
    }
}
