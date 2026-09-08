use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bytes::{Buf, Bytes};

use crate::error::{LsmError, Result};
use crate::filter::BloomFilter;
use crate::sstable::block::Block;
use crate::sstable::builder::{BlockMeta, FOOTER_SIZE, SSTABLE_MAGIC};

pub struct SsTableReader {
    path: PathBuf,
    file: File,
    block_metas: Vec<BlockMeta>,
    bloom_filter: BloomFilter,
}

impl SsTableReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new().read(true).open(&path)?;
        let file_len = file.metadata()?.len();

        if file_len < FOOTER_SIZE as u64 {
            return Err(LsmError::CorruptedSSTable("File too small to contain SSTable footer".into()));
        }

        // 1. Read trailing 40-byte Footer
        file.seek(SeekFrom::Start(file_len - FOOTER_SIZE as u64))?;
        let mut footer_bytes = [0u8; FOOTER_SIZE];
        file.read_exact(&mut footer_bytes)?;

        let mut cursor = &footer_bytes[..];
        let index_offset = cursor.get_u64();
        let index_len = cursor.get_u64();
        let filter_offset = cursor.get_u64();
        let filter_len = cursor.get_u64();
        let magic = cursor.get_u64();

        if magic != SSTABLE_MAGIC {
            return Err(LsmError::CorruptedSSTable(format!(
                "Invalid SSTable magic bytes: expected {:#018x}, got {:#018x}",
                SSTABLE_MAGIC, magic
            )));
        }

        // 2. Read and decode Bloom Filter
        file.seek(SeekFrom::Start(filter_offset))?;
        let mut filter_buf = vec![0u8; filter_len as usize];
        file.read_exact(&mut filter_buf)?;
        let bloom_filter = BloomFilter::decode(Bytes::from(filter_buf));

        // 3. Read and decode Block Index
        file.seek(SeekFrom::Start(index_offset))?;
        let mut index_buf = vec![0u8; index_len as usize];
        file.read_exact(&mut index_buf)?;
        let mut index_cursor = &index_buf[..];

        let num_blocks = index_cursor.get_u32() as usize;
        let mut block_metas = Vec::with_capacity(num_blocks);

        for _ in 0..num_blocks {
            let key_len = index_cursor.get_u16() as usize;
            let first_key = Bytes::copy_from_slice(&index_cursor[..key_len]);
            index_cursor.advance(key_len);
            let offset = index_cursor.get_u64();
            let len = index_cursor.get_u64();

            block_metas.push(BlockMeta {
                first_key,
                offset,
                len,
            });
        }

        Ok(Self {
            path,
            file,
            block_metas,
            bloom_filter,
        })
    }

    /// Point lookup for a key in this SSTable.
    /// Returns:
    /// - `Some(Some(value))` if found with active value
    /// - `Some(None)` if found with tombstone (deleted)
    /// - `None` if key does not exist in this SSTable (bypassed via Bloom filter or block search)
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Option<Bytes>>> {
        // Step 1: Probabilistic check with Bloom Filter
        // If false, the key is GUARANTEED not to exist. Zero disk I/O!
        if !self.bloom_filter.may_contain(key) {
            return Ok(None);
        }

        if self.block_metas.is_empty() {
            return Ok(None);
        }

        // Step 2: If key is smaller than the first block's first key, it's not here
        if key < self.block_metas[0].first_key.as_ref() {
            return Ok(None);
        }

        // Step 3: Binary search Block Index to locate candidate block
        // Find the last block where first_key <= key
        let idx = match self.block_metas.binary_search_by(|b| b.first_key.as_ref().cmp(key)) {
            Ok(exact) => exact,
            Err(insertion_point) => {
                if insertion_point == 0 {
                    0
                } else {
                    insertion_point - 1
                }
            }
        };

        let meta = &self.block_metas[idx];

        // Step 4: Seek and read only that single block from disk
        self.file.seek(SeekFrom::Start(meta.offset))?;
        let mut block_buf = vec![0u8; meta.len as usize];
        self.file.read_exact(&mut block_buf)?;

        // Step 5: Binary search within the decoded block
        let block = Block::decode(Bytes::from(block_buf))?;
        Ok(block.get(key))
    }

    /// Reads all entries from all data blocks in sequential order.
    pub fn read_all_entries(&mut self) -> Result<Vec<(Bytes, Option<Bytes>)>> {
        let mut entries = Vec::new();
        for meta in &self.block_metas {
            self.file.seek(SeekFrom::Start(meta.offset))?;
            let mut buf = vec![0u8; meta.len as usize];
            self.file.read_exact(&mut buf)?;
            let block = Block::decode(Bytes::from(buf))?;

            for i in 0..block.len() {
                if let Some(k) = block.get_key(i) {
                    if let Some(val_opt) = block.get(&k) {
                        entries.push((k, val_opt));
                    }
                }
            }
        }
        Ok(entries)
    }

    pub fn num_blocks(&self) -> usize {
        self.block_metas.len()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sstable::builder::SsTableBuilder;
    use tempfile::NamedTempFile;

    #[test]
    fn test_sstable_build_and_read_point_lookups() -> Result<()> {
        let temp = NamedTempFile::new()?;
        let path = temp.path();

        // 1. Build an SSTable with small blocks (128 bytes) to force multiple blocks
        {
            let mut builder = SsTableBuilder::create_with_block_size(path, 128)?;
            for i in 0..50 {
                let k = format!("key:{:04}", i);
                let v = format!("val:{:04}", i);
                if i == 25 {
                    builder.add(k.as_bytes(), None)?; // Tombstone
                } else {
                    builder.add(k.as_bytes(), Some(v.as_bytes()))?;
                }
            }
            builder.finish()?;
        }

        // 2. Read and verify with SsTableReader
        let mut reader = SsTableReader::open(path)?;
        assert!(reader.num_blocks() > 1); // Confirmed multiple blocks were created

        // Verify existing values
        for i in 0..50 {
            let k = format!("key:{:04}", i);
            let res = reader.get(k.as_bytes())?;
            if i == 25 {
                assert_eq!(res, Some(None), "Key 25 should be tombstone");
            } else {
                let expected_v = format!("val:{:04}", i);
                assert_eq!(res, Some(Some(Bytes::from(expected_v))));
            }
        }

        // Verify missing keys (should be rejected via Bloom filter or block index)
        assert_eq!(reader.get(b"key:9999")?, None);
        assert_eq!(reader.get(b"non_existent")?, None);

        // Verify read all entries preserves order
        let all = reader.read_all_entries()?;
        assert_eq!(all.len(), 50);
        assert_eq!(all[0].0.as_ref(), b"key:0000");
        assert_eq!(all[49].0.as_ref(), b"key:0049");

        Ok(())
    }
}
