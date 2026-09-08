use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use crc32fast::Hasher;

use crate::error::{LsmError, Result};

pub const HEADER_SIZE: usize = 11; // 4 (crc) + 2 (key_len) + 4 (val_len) + 1 (op)
pub const MAX_KEY_SIZE: usize = u16::MAX as usize; // 65,535 bytes

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WalOp {
    Put = 0,
    Delete = 1,
}

impl TryFrom<u8> for WalOp {
    type Error = LsmError;

    fn try_from(val: u8) -> Result<Self> {
        match val {
            0 => Ok(WalOp::Put),
            1 => Ok(WalOp::Delete),
            other => Err(LsmError::InvalidOpCode(other)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalEntry {
    pub op: WalOp,
    pub key: Bytes,
    pub value: Option<Bytes>,
}

/// Write-Ahead Log writer for append-only durability.
pub struct WalWriter {
    path: PathBuf,
    writer: BufWriter<File>,
}

impl WalWriter {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        Ok(Self {
            path,
            writer: BufWriter::new(file),
        })
    }

    pub fn append_put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.append_record(WalOp::Put, key, Some(value))
    }

    pub fn append_delete(&mut self, key: &[u8]) -> Result<()> {
        self.append_record(WalOp::Delete, key, None)
    }

    fn append_record(&mut self, op: WalOp, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        if key.len() > MAX_KEY_SIZE {
            return Err(LsmError::KeyTooLarge {
                actual: key.len(),
                max: MAX_KEY_SIZE,
            });
        }

        let key_len = key.len() as u16;
        let val_len = value.map(|v| v.len() as u32).unwrap_or(0);

        // Calculate CRC32 over payload metadata + payload data
        let mut hasher = Hasher::new();
        hasher.update(&key_len.to_be_bytes());
        hasher.update(&val_len.to_be_bytes());
        hasher.update(&[op as u8]);
        hasher.update(key);
        if let Some(v) = value {
            hasher.update(v);
        }
        let checksum = hasher.finalize();

        // Write frame: [CRC32: 4B][KeyLen: 2B][ValLen: 4B][Op: 1B][Key: N B][Val: M B]
        self.writer.write_all(&checksum.to_be_bytes())?;
        self.writer.write_all(&key_len.to_be_bytes())?;
        self.writer.write_all(&val_len.to_be_bytes())?;
        self.writer.write_all(&[op as u8])?;
        self.writer.write_all(key)?;
        if let Some(v) = value {
            self.writer.write_all(v)?;
        }
        self.writer.flush()?;
        Ok(())
    }

    /// Flushes internal buffers and triggers an OS fsync to guarantee disk persistence.
    pub fn sync(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Reads and validates a WAL file, recovering all valid entries.
pub struct WalReader;

impl WalReader {
    pub fn read_all<P: AsRef<Path>>(path: P) -> Result<Vec<WalEntry>> {
        let file = OpenOptions::new().read(true).open(path)?;
        let mut reader = BufReader::new(file);
        let mut entries = Vec::new();
        let mut offset = 0u64;

        loop {
            let mut header = [0u8; HEADER_SIZE];
            match reader.read_exact(&mut header) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Clean EOF at record boundary
                    break;
                }
                Err(e) => return Err(LsmError::Io(e)),
            }

            let expected_crc = u32::from_be_bytes(header[0..4].try_into().unwrap());
            let key_len = u16::from_be_bytes(header[4..6].try_into().unwrap()) as usize;
            let val_len = u32::from_be_bytes(header[6..10].try_into().unwrap()) as usize;
            let op = WalOp::try_from(header[10])?;

            let mut body = vec![0u8; key_len + val_len];
            if let Err(e) = reader.read_exact(&mut body) {
                return Err(LsmError::TornWrite {
                    offset,
                    detail: format!("Failed to read complete payload (expected {} bytes): {}", key_len + val_len, e),
                });
            }

            // Verify CRC32
            let mut hasher = Hasher::new();
            hasher.update(&header[4..11]); // key_len (2B) + val_len (4B) + op (1B)
            hasher.update(&body);
            let actual_crc = hasher.finalize();

            if expected_crc != actual_crc {
                return Err(LsmError::CrcMismatch {
                    expected: expected_crc,
                    actual: actual_crc,
                });
            }

            let key = Bytes::copy_from_slice(&body[..key_len]);
            let value = if op == WalOp::Delete {
                None
            } else {
                Some(Bytes::copy_from_slice(&body[key_len..]))
            };

            entries.push(WalEntry { op, key, value });
            offset += HEADER_SIZE as u64 + (key_len + val_len) as u64;
        }

        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};
    use tempfile::NamedTempFile;

    #[test]
    fn test_wal_write_and_recover() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let path = temp_file.path();

        {
            let mut writer = WalWriter::open(path)?;
            writer.append_put(b"user:101", b"Bradshaw")?;
            writer.append_put(b"user:102", b"Asher")?;
            writer.append_delete(b"user:101")?;
            writer.sync()?;
        }

        let entries = WalReader::read_all(path)?;
        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0].op, WalOp::Put);
        assert_eq!(entries[0].key.as_ref(), b"user:101");
        assert_eq!(entries[0].value.as_deref(), Some(&b"Bradshaw"[..]));

        assert_eq!(entries[1].op, WalOp::Put);
        assert_eq!(entries[1].key.as_ref(), b"user:102");
        assert_eq!(entries[1].value.as_deref(), Some(&b"Asher"[..]));

        assert_eq!(entries[2].op, WalOp::Delete);
        assert_eq!(entries[2].key.as_ref(), b"user:101");
        assert_eq!(entries[2].value, None);

        Ok(())
    }

    #[test]
    fn test_wal_crc_corruption_detection() -> Result<()> {
        let temp_file = NamedTempFile::new()?;
        let path = temp_file.path();

        {
            let mut writer = WalWriter::open(path)?;
            writer.append_put(b"test_key", b"test_value")?;
            writer.sync()?;
        }

        // Corrupt 1 byte in the file
        {
            let mut file = OpenOptions::new().write(true).open(path)?;
            file.seek(SeekFrom::Start(HEADER_SIZE as u64 + 2))?;
            file.write_all(b"X")?;
            file.sync_all()?;
        }

        // Replay should catch CRC mismatch
        let result = WalReader::read_all(path);
        assert!(matches!(result, Err(LsmError::CrcMismatch { .. })));

        Ok(())
    }
}
