use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::{LsmError, Result};
use crate::filter::BloomFilter;
use crate::sstable::block::BlockBuilder;

pub const SSTABLE_MAGIC: u64 = 0x4C534D5452454531; // "LSMTREE1" in hex
pub const FOOTER_SIZE: usize = 40; // 8 (index_offset) + 8 (index_len) + 8 (filter_offset) + 8 (filter_len) + 8 (magic)
pub const DEFAULT_BLOCK_SIZE: usize = 4096; // 4KB

#[derive(Clone, Debug)]
pub struct BlockMeta {
    pub first_key: Bytes,
    pub offset: u64,
    pub len: u64,
}

pub struct SsTableBuilder {
    path: PathBuf,
    writer: BufWriter<File>,
    current_block: BlockBuilder,
    first_key_in_current_block: Option<Bytes>,
    block_metas: Vec<BlockMeta>,
    keys_for_bloom: Vec<Vec<u8>>,
    current_offset: u64,
    block_size: usize,
}

impl SsTableBuilder {
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::create_with_block_size(path, DEFAULT_BLOCK_SIZE)
    }

    pub fn create_with_block_size<P: AsRef<Path>>(path: P, block_size: usize) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;

        Ok(Self {
            path,
            writer: BufWriter::new(file),
            current_block: BlockBuilder::new(block_size),
            first_key_in_current_block: None,
            block_metas: Vec::new(),
            keys_for_bloom: Vec::new(),
            current_offset: 0,
            block_size,
        })
    }

    /// Appends a sorted key-value pair (or tombstone).
    /// Keys must be added in strictly non-decreasing sorted order.
    pub fn add(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        if self.first_key_in_current_block.is_none() {
            self.first_key_in_current_block = Some(Bytes::copy_from_slice(key));
        }

        // Record key for bloom filter
        self.keys_for_bloom.push(key.to_vec());

        if !self.current_block.add(key, value) {
            // Block is full. Flush it to disk and start a new block.
            self.flush_current_block()?;

            self.first_key_in_current_block = Some(Bytes::copy_from_slice(key));
            let added = self.current_block.add(key, value);
            if !added {
                return Err(LsmError::CorruptedSSTable("Single entry exceeds block size limit".into()));
            }
        }

        Ok(())
    }

    fn flush_current_block(&mut self) -> Result<()> {
        if self.current_block.is_empty() {
            return Ok(());
        }

        let first_key = self.first_key_in_current_block.take().unwrap_or_default();
        let block = std::mem::replace(&mut self.current_block, BlockBuilder::new(self.block_size));
        let block_bytes = block.build();
        let len = block_bytes.len() as u64;

        self.writer.write_all(&block_bytes)?;
        self.block_metas.push(BlockMeta {
            first_key,
            offset: self.current_offset,
            len,
        });
        self.current_offset += len;

        Ok(())
    }

    /// Finalizes the SSTable by writing any remaining block, the Block Index, the Bloom Filter, and the Footer.
    pub fn finish(mut self) -> Result<PathBuf> {
        // 1. Flush any pending block
        self.flush_current_block()?;

        // 2. Encode and write Block Index
        let index_offset = self.current_offset;
        let mut index_buf = BytesMut::new();
        index_buf.put_u32(self.block_metas.len() as u32);
        for meta in &self.block_metas {
            index_buf.put_u16(meta.first_key.len() as u16);
            index_buf.put_slice(&meta.first_key);
            index_buf.put_u64(meta.offset);
            index_buf.put_u64(meta.len);
        }
        let index_bytes = index_buf.freeze();
        let index_len = index_bytes.len() as u64;
        self.writer.write_all(&index_bytes)?;
        self.current_offset += index_len;

        // 3. Build, encode and write Bloom Filter
        let filter_offset = self.current_offset;
        let key_slices: Vec<&[u8]> = self.keys_for_bloom.iter().map(|v| v.as_slice()).collect();
        let bloom_filter = BloomFilter::build(&key_slices);
        let filter_bytes = bloom_filter.encode();
        let filter_len = filter_bytes.len() as u64;
        self.writer.write_all(&filter_bytes)?;
        self.current_offset += filter_len;

        // 4. Write Footer: [index_offset: 8B] [index_len: 8B] [filter_offset: 8B] [filter_len: 8B] [magic: 8B]
        let mut footer_buf = BytesMut::with_capacity(FOOTER_SIZE);
        footer_buf.put_u64(index_offset);
        footer_buf.put_u64(index_len);
        footer_buf.put_u64(filter_offset);
        footer_buf.put_u64(filter_len);
        footer_buf.put_u64(SSTABLE_MAGIC);
        self.writer.write_all(&footer_buf.freeze())?;

        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;

        Ok(self.path)
    }
}
