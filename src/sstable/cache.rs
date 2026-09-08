use std::collections::HashMap;
use std::sync::Arc;

use crate::sstable::block::Block;

/// Unique identifier for an SSTable block in the cache: (SSTable ID, Byte Offset).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub sst_id: u64,
    pub offset: u64,
}

struct LruNode {
    key: BlockKey,
    block: Arc<Block>,
    prev: Option<usize>,
    next: Option<usize>,
}

/// An in-memory Least-Recently-Used (LRU) cache for decoded 4KB SSTable data blocks.
/// Provides O(1) cache lookups, insertions, and evictions.
pub struct BlockCache {
    capacity: usize,
    nodes: Vec<Option<LruNode>>,
    map: HashMap<BlockKey, usize>,
    head: Option<usize>, // Most recently used slot
    tail: Option<usize>, // Least recently used slot
    free_slots: Vec<usize>,
}

impl BlockCache {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            capacity: cap,
            nodes: Vec::with_capacity(cap),
            map: HashMap::with_capacity(cap),
            head: None,
            tail: None,
            free_slots: Vec::new(),
        }
    }

    /// Number of cached blocks currently in memory.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Retrieves a block from cache, promoting it to Most Recently Used (MRU).
    pub fn get(&mut self, key: &BlockKey) -> Option<Arc<Block>> {
        let idx = *self.map.get(key)?;
        self.move_to_head(idx);
        Some(self.nodes[idx].as_ref().unwrap().block.clone())
    }

    /// Inserts a block into the cache. If capacity is exceeded, evicts the LRU block.
    pub fn insert(&mut self, key: BlockKey, block: Arc<Block>) {
        if let Some(&idx) = self.map.get(&key) {
            if let Some(node) = self.nodes[idx].as_mut() {
                node.block = block;
            }
            self.move_to_head(idx);
            return;
        }

        if self.map.len() >= self.capacity {
            if let Some(evicted_key) = self.evict_tail() {
                self.map.remove(&evicted_key);
            }
        }

        let idx = self.allocate_slot();
        self.nodes[idx] = Some(LruNode {
            key,
            block,
            prev: None,
            next: None,
        });

        self.attach_head(idx);
        self.map.insert(key, idx);
    }

    fn allocate_slot(&mut self) -> usize {
        if let Some(slot) = self.free_slots.pop() {
            slot
        } else {
            let slot = self.nodes.len();
            self.nodes.push(None);
            slot
        }
    }

    fn move_to_head(&mut self, idx: usize) {
        if self.head == Some(idx) {
            return;
        }
        self.detach(idx);
        self.attach_head(idx);
    }

    fn attach_head(&mut self, idx: usize) {
        let old_head = self.head;
        if let Some(node) = self.nodes[idx].as_mut() {
            node.prev = None;
            node.next = old_head;
        }

        if let Some(h) = old_head {
            if let Some(h_node) = self.nodes[h].as_mut() {
                h_node.prev = Some(idx);
            }
        } else {
            self.tail = Some(idx);
        }

        self.head = Some(idx);
    }

    fn detach(&mut self, idx: usize) {
        let (prev, next) = match &self.nodes[idx] {
            Some(node) => (node.prev, node.next),
            None => return,
        };

        if let Some(p) = prev {
            if let Some(p_node) = self.nodes[p].as_mut() {
                p_node.next = next;
            }
        } else {
            self.head = next;
        }

        if let Some(n) = next {
            if let Some(n_node) = self.nodes[n].as_mut() {
                n_node.prev = prev;
            }
        } else {
            self.tail = prev;
        }

        if let Some(node) = self.nodes[idx].as_mut() {
            node.prev = None;
            node.next = None;
        }
    }

    fn evict_tail(&mut self) -> Option<BlockKey> {
        let tail_idx = self.tail?;
        let key = self.nodes[tail_idx].as_ref().map(|n| n.key)?;

        self.detach(tail_idx);
        self.nodes[tail_idx] = None;
        self.free_slots.push(tail_idx);

        Some(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sstable::block::BlockBuilder;

    fn make_test_block(k: &str, v: &str) -> Arc<Block> {
        let mut builder = BlockBuilder::new(1024);
        builder.add(k.as_bytes(), Some(v.as_bytes()));
        let bytes = builder.build();
        Arc::new(Block::decode(bytes).unwrap())
    }

    #[test]
    fn test_block_cache_lru_eviction() {
        let mut cache = BlockCache::new(2);

        let k1 = BlockKey { sst_id: 1, offset: 0 };
        let k2 = BlockKey { sst_id: 1, offset: 4096 };
        let k3 = BlockKey { sst_id: 2, offset: 0 };

        let b1 = make_test_block("k1", "v1");
        let b2 = make_test_block("k2", "v2");
        let b3 = make_test_block("k3", "v3");

        cache.insert(k1, b1.clone());
        cache.insert(k2, b2.clone());
        assert_eq!(cache.len(), 2);

        // Access k1, making k2 the LRU block
        assert!(cache.get(&k1).is_some());

        // Insert k3 -> should evict k2
        cache.insert(k3, b3.clone());
        assert_eq!(cache.len(), 2);

        assert!(cache.get(&k1).is_some(), "k1 should still be present");
        assert!(cache.get(&k3).is_some(), "k3 should be present");
        assert!(cache.get(&k2).is_none(), "k2 should have been evicted by LRU policy");
    }
}
