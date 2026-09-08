use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::{Buf, Bytes};
use parking_lot::Mutex;

use crate::error::{LsmError, Result};
use crate::filter::BloomFilter;
use crate::sstable::block::Block;
use crate::sstable::builder::{BlockMeta, FOOTER_SIZE, SSTABLE_MAGIC};
use crate::sstable::cache::{BlockCache, BlockKey};

pub struct SsTableReader {
    path: PathBuf,
    file: File,
    block_metas: Vec<BlockMeta>,
    bloom_filter: BloomFilter,
    sst_id: u64,
    block_cache: Option<Arc<Mutex<BlockCache>>>,
}

impl SsTableReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with_cache(path, None)
    }

    pub fn open_with_cache<P: AsRef<Path>>(
        path: P,
        block_cache: Option<Arc<Mutex<BlockCache>>>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let sst_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

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
            sst_id,
            block_cache,
        })
    }

    /// Reads and decodes a data block, leveraging the in-memory LRU block cache if available.
    pub fn read_block(&mut self, block_idx: usize) -> Result<Arc<Block>> {
        let meta = &self.block_metas[block_idx];
        let cache_key = BlockKey {
            sst_id: self.sst_id,
            offset: meta.offset,
        };

        if let Some(cache) = &self.block_cache {
            if let Some(block) = cache.lock().get(&cache_key) {
                return Ok(block);
            }
        }

        self.file.seek(SeekFrom::Start(meta.offset))?;
        let mut block_buf = vec![0u8; meta.len as usize];
        self.file.read_exact(&mut block_buf)?;
        let block = Arc::new(Block::decode(Bytes::from(block_buf))?);

        if let Some(cache) = &self.block_cache {
            cache.lock().insert(cache_key, block.clone());
        }

        Ok(block)
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

        // Step 4: Fetch block (from LRU block cache or disk)
        let block = self.read_block(idx)?;

        // Step 5: Binary search within the decoded block
        Ok(block.get(key))
    }

    /// Reads all entries from all data blocks in sequential order.
    pub fn read_all_entries(&mut self) -> Result<Vec<(Bytes, Option<Bytes>)>> {
        let mut entries = Vec::new();
        for i in 0..self.block_metas.len() {
            let block = self.read_block(i)?;
            for j in 0..block.len() {
                if let Some(entry) = block.get_entry(j) {
                    entries.push(entry);
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

    pub fn sst_id(&self) -> u64 {
        self.sst_id
    }

    /// Sets or updates the block cache for this reader.
    pub fn set_cache(&mut self, cache: Arc<Mutex<BlockCache>>) {
        self.block_cache = Some(cache);
    }

    /// Creates an iterator over this SSTable within [start, end).
    pub fn iter_range(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<SsTableIterator> {
        SsTableIterator::new_with_cache(
            &self.path,
            self.sst_id,
            self.block_metas.clone(),
            start,
            end,
            self.block_cache.clone(),
        )
    }

    /// Creates an iterator over all entries in this SSTable.
    pub fn iter(&self) -> Result<SsTableIterator> {
        self.iter_range(None, None)
    }
}

/// Sequential, on-demand block iterator over an SSTable.
pub struct SsTableIterator {
    file: File,
    sst_id: u64,
    block_metas: Vec<BlockMeta>,
    current_block_idx: usize,
    current_block: Option<Arc<Block>>,
    current_entry_idx: usize,
    end_bound: Option<Bytes>,
    block_cache: Option<Arc<Mutex<BlockCache>>>,
}

impl SsTableIterator {
    pub fn new<P: AsRef<Path>>(
        path: P,
        block_metas: Vec<BlockMeta>,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<Self> {
        Self::new_with_cache(path, 0, block_metas, start, end, None)
    }

    pub fn new_with_cache<P: AsRef<Path>>(
        path: P,
        sst_id: u64,
        block_metas: Vec<BlockMeta>,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
        block_cache: Option<Arc<Mutex<BlockCache>>>,
    ) -> Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        if block_metas.is_empty() {
            return Ok(Self {
                file,
                sst_id,
                block_metas,
                current_block_idx: 0,
                current_block: None,
                current_entry_idx: 0,
                end_bound: None,
                block_cache,
            });
        }

        let start_block_idx = match start {
            None => 0,
            Some(s) => {
                if s < block_metas[0].first_key.as_ref() {
                    0
                } else {
                    match block_metas.binary_search_by(|b| b.first_key.as_ref().cmp(s)) {
                        Ok(exact) => exact,
                        Err(ins) => {
                            if ins == 0 {
                                0
                            } else {
                                ins - 1
                            }
                        }
                    }
                }
            }
        };

        let mut iter = Self {
            file,
            sst_id,
            block_metas,
            current_block_idx: start_block_idx,
            current_block: None,
            current_entry_idx: 0,
            end_bound: end.map(Bytes::copy_from_slice),
            block_cache,
        };

        iter.load_block_and_seek(start_block_idx, start)?;
        Ok(iter)
    }

    fn load_block_and_seek(&mut self, block_idx: usize, seek_key: Option<&[u8]>) -> Result<()> {
        if block_idx >= self.block_metas.len() {
            self.current_block = None;
            return Ok(());
        }

        let meta = &self.block_metas[block_idx];
        let cache_key = BlockKey {
            sst_id: self.sst_id,
            offset: meta.offset,
        };

        let block = if let Some(cache) = &self.block_cache {
            if let Some(b) = cache.lock().get(&cache_key) {
                b
            } else {
                self.file.seek(SeekFrom::Start(meta.offset))?;
                let mut buf = vec![0u8; meta.len as usize];
                self.file.read_exact(&mut buf)?;
                let b = Arc::new(Block::decode(Bytes::from(buf))?);
                cache.lock().insert(cache_key, b.clone());
                b
            }
        } else {
            self.file.seek(SeekFrom::Start(meta.offset))?;
            let mut buf = vec![0u8; meta.len as usize];
            self.file.read_exact(&mut buf)?;
            Arc::new(Block::decode(Bytes::from(buf))?)
        };

        self.current_entry_idx = match seek_key {
            Some(k) => block.seek_to_key(k),
            None => 0,
        };
        self.current_block_idx = block_idx + 1;
        self.current_block = Some(block);
        Ok(())
    }
}

impl Iterator for SsTableIterator {
    type Item = Result<(Bytes, Option<Bytes>)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(block) = &self.current_block {
                if self.current_entry_idx < block.len() {
                    if let Some((key, val)) = block.get_entry(self.current_entry_idx) {
                        self.current_entry_idx += 1;

                        if let Some(end) = &self.end_bound {
                            if key.as_ref() >= end.as_ref() {
                                self.current_block = None;
                                return None;
                            }
                        }

                        return Some(Ok((key, val)));
                    }
                }
            }

            if self.current_block_idx >= self.block_metas.len() {
                self.current_block = None;
                return None;
            }

            if let Some(end) = &self.end_bound {
                if self.block_metas[self.current_block_idx].first_key.as_ref() >= end.as_ref() {
                    self.current_block = None;
                    return None;
                }
            }

            let next_idx = self.current_block_idx;
            if let Err(e) = self.load_block_and_seek(next_idx, None) {
                return Some(Err(e));
            }
        }
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

        // 3. Test SsTableIterator with ranges
        // Range [key:0010, key:0020) -> should yield 10 items (key:0010 to key:0019)
        let range_items: Result<Vec<(Bytes, Option<Bytes>)>> = reader
            .iter_range(Some(b"key:0010"), Some(b"key:0020"))?
            .collect();
        let range_items = range_items?;
        assert_eq!(range_items.len(), 10);
        assert_eq!(range_items[0].0.as_ref(), b"key:0010");
        assert_eq!(range_items[9].0.as_ref(), b"key:0019");

        // Full range iterator
        let full_iter_items: Result<Vec<(Bytes, Option<Bytes>)>> = reader.iter()?.collect();
        let full_iter_items = full_iter_items?;
        assert_eq!(full_iter_items.len(), 50);

        Ok(())
    }
}
