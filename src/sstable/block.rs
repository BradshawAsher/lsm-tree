use bytes::{Buf, BufMut, Bytes, BytesMut};
use crate::error::{LsmError, Result};

/// A Data Block containing sorted key-value pairs with a trailing offset array
/// to allow O(log N) binary search within the block.
#[derive(Clone)]
pub struct Block {
    data: Bytes,
    offsets: Vec<u16>,
}

impl Block {
    /// Decodes a raw byte buffer into an in-memory Block.
    /// Format:
    /// [Entry 0] [Entry 1] ... [Entry N-1]
    /// [Offset 0: u16] [Offset 1: u16] ... [Offset N-1: u16]
    /// [Num Entries: u16]
    pub fn decode(data: Bytes) -> Result<Self> {
        if data.len() < 2 {
            return Err(LsmError::CorruptedSSTable("Block data too short".into()));
        }

        let num_entries_offset = data.len() - 2;
        let num_entries = (&data[num_entries_offset..]).get_u16() as usize;

        let offsets_start = num_entries_offset
            .checked_sub(num_entries * 2)
            .ok_or_else(|| LsmError::CorruptedSSTable("Block offset table out of bounds".into()))?;

        let mut offsets = Vec::with_capacity(num_entries);
        let mut cursor = &data[offsets_start..num_entries_offset];
        for _ in 0..num_entries {
            offsets.push(cursor.get_u16());
        }

        Ok(Self { data, offsets })
    }

    /// Number of key-value entries in this block.
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Retrieves the key at index `idx`.
    pub fn get_key(&self, idx: usize) -> Option<Bytes> {
        let offset = *self.offsets.get(idx)? as usize;
        let mut cursor = &self.data[offset..];
        if cursor.len() < 7 {
            return None;
        }
        let key_len = cursor.get_u16() as usize;
        let _val_len = cursor.get_u32() as usize;
        let _is_tombstone = cursor.get_u8();

        if cursor.len() < key_len {
            return None;
        }
        Some(self.data.slice(offset + 7..offset + 7 + key_len))
    }

    /// Returns the first key in this block, if present.
    pub fn first_key(&self) -> Option<Bytes> {
        self.get_key(0)
    }

    /// Looks up a key within the block using binary search over the offset index.
    /// Returns:
    /// - `Some(Some(value))` if found with active value
    /// - `Some(None)` if found with tombstone (deleted)
    /// - `None` if not found in this block
    pub fn get(&self, target_key: &[u8]) -> Option<Option<Bytes>> {
        if self.offsets.is_empty() {
            return None;
        }

        let mut low = 0;
        let mut high = self.offsets.len() - 1;

        while low <= high {
            let mid = low + (high - low) / 2;
            let mid_key = self.get_key(mid)?;

            match mid_key.as_ref().cmp(target_key) {
                std::cmp::Ordering::Equal => {
                    // Found the exact entry! Decode value / tombstone.
                    let offset = self.offsets[mid] as usize;
                    let mut cursor = &self.data[offset..];
                    let key_len = cursor.get_u16() as usize;
                    let val_len = cursor.get_u32() as usize;
                    let is_tombstone = cursor.get_u8() == 1;

                    return if is_tombstone {
                        Some(None) // Tombstone
                    } else {
                        let val_start = offset + 7 + key_len;
                        Some(Some(self.data.slice(val_start..val_start + val_len)))
                    };
                }
                std::cmp::Ordering::Less => {
                    low = mid + 1;
                }
                std::cmp::Ordering::Greater => {
                    if mid == 0 {
                        break;
                    }
                    high = mid - 1;
                }
            }
        }

        None
    }
}

/// Helper to build a 4KB Data Block.
pub struct BlockBuilder {
    data: BytesMut,
    offsets: Vec<u16>,
    block_size: usize,
}

impl BlockBuilder {
    pub fn new(block_size: usize) -> Self {
        Self {
            data: BytesMut::new(),
            offsets: Vec::new(),
            block_size,
        }
    }

    /// Appends a key-value entry (or tombstone) to the block.
    pub fn add(&mut self, key: &[u8], value: Option<&[u8]>) -> bool {
        let val_len = value.map(|v| v.len()).unwrap_or(0);
        let entry_size = 7 + key.len() + val_len;

        // Check if adding this entry exceeds block capacity (leaving space for offset table)
        if !self.is_empty() && self.current_size() + entry_size + 2 > self.block_size {
            return false;
        }

        let offset = self.data.len() as u16;
        self.offsets.push(offset);

        // Header: [key_len: u16][val_len: u32][is_tombstone: u8]
        self.data.put_u16(key.len() as u16);
        self.data.put_u32(val_len as u32);
        self.data.put_u8(if value.is_none() { 1 } else { 0 });

        // Payload
        self.data.put_slice(key);
        if let Some(val) = value {
            self.data.put_slice(val);
        }

        true
    }

    /// Estimated total size including offsets and count trailer.
    pub fn current_size(&self) -> usize {
        self.data.len() + (self.offsets.len() * 2) + 2
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Finalizes the block and serializes the trailing offset table.
    pub fn build(mut self) -> Bytes {
        for offset in self.offsets.iter() {
            self.data.put_u16(*offset);
        }
        self.data.put_u16(self.offsets.len() as u16);
        self.data.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_build_and_binary_search() -> Result<()> {
        let mut builder = BlockBuilder::new(4096);

        assert!(builder.add(b"key:01", Some(b"val:01")));
        assert!(builder.add(b"key:05", Some(b"val:05")));
        assert!(builder.add(b"key:10", None)); // Tombstone
        assert!(builder.add(b"key:20", Some(b"val:20")));

        let block_bytes = builder.build();
        let block = Block::decode(block_bytes)?;

        assert_eq!(block.len(), 4);
        assert_eq!(block.first_key(), Some(Bytes::from("key:01")));

        // Exact match active value
        assert_eq!(block.get(b"key:01"), Some(Some(Bytes::from("val:01"))));
        assert_eq!(block.get(b"key:05"), Some(Some(Bytes::from("val:05"))));

        // Tombstone
        assert_eq!(block.get(b"key:10"), Some(None));

        // Missing key
        assert_eq!(block.get(b"key:03"), None);
        assert_eq!(block.get(b"key:25"), None);

        Ok(())
    }
}
