use std::sync::atomic::{AtomicUsize, Ordering};
use bytes::Bytes;
use crossbeam_skiplist::SkipMap;

/// Approximate memory overhead per SkipList node (pointers, height tower, locks).
const ESTIMATED_NODE_OVERHEAD: usize = 64;

/// In-memory sorted write buffer using a concurrent lock-free SkipList.
pub struct MemTable {
    map: SkipMap<Bytes, Option<Bytes>>,
    size_bytes: AtomicUsize,
}

impl MemTable {
    pub fn new() -> Self {
        Self {
            map: SkipMap::new(),
            size_bytes: AtomicUsize::new(0),
        }
    }

    /// Insert or overwrite a key-value pair.
    pub fn put(&self, key: Bytes, value: Bytes) {
        let added_bytes = key.len() + value.len() + ESTIMATED_NODE_OVERHEAD;
        self.map.insert(key, Some(value));
        self.size_bytes.fetch_add(added_bytes, Ordering::Relaxed);
    }

    /// Mark a key as deleted by inserting a tombstone (`None`).
    pub fn delete(&self, key: Bytes) {
        let added_bytes = key.len() + ESTIMATED_NODE_OVERHEAD;
        self.map.insert(key, None);
        self.size_bytes.fetch_add(added_bytes, Ordering::Relaxed);
    }

    /// Lookup a key in the MemTable.
    /// Returns:
    /// - `Some(Some(val))` if key exists with active value.
    /// - `Some(None)` if key has an active Tombstone (deleted).
    /// - `None` if key is not present in this MemTable.
    pub fn get(&self, key: &[u8]) -> Option<Option<Bytes>> {
        self.map.get(key).map(|entry| entry.value().clone())
    }

    /// Total approximate memory footprint of this MemTable in bytes.
    pub fn size_bytes(&self) -> usize {
        self.size_bytes.load(Ordering::Relaxed)
    }

    /// Number of entries currently in the MemTable.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Returns an iterator yielding all (Key, Option<Value>) entries in lexicographical sorted order.
    pub fn iter(&self) -> impl Iterator<Item = (Bytes, Option<Bytes>)> + '_ {
        self.map.iter().map(|entry| (entry.key().clone(), entry.value().clone()))
    }
}

impl Default for MemTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memtable_crud() {
        let memtable = MemTable::new();
        assert!(memtable.is_empty());

        memtable.put(Bytes::from("key1"), Bytes::from("val1"));
        memtable.put(Bytes::from("key2"), Bytes::from("val2"));

        assert_eq!(memtable.get(b"key1"), Some(Some(Bytes::from("val1"))));
        assert_eq!(memtable.get(b"key2"), Some(Some(Bytes::from("val2"))));
        assert_eq!(memtable.get(b"key3"), None);

        // Test delete (tombstone)
        memtable.delete(Bytes::from("key1"));
        assert_eq!(memtable.get(b"key1"), Some(None)); // Tombstone
        assert_eq!(memtable.len(), 2);
    }

    #[test]
    fn test_memtable_sorted_order() {
        let memtable = MemTable::new();
        memtable.put(Bytes::from("charlie"), Bytes::from("3"));
        memtable.put(Bytes::from("alice"), Bytes::from("1"));
        memtable.put(Bytes::from("bob"), Bytes::from("2"));

        let keys: Vec<String> = memtable
            .iter()
            .map(|(k, _)| String::from_utf8(k.to_vec()).unwrap())
            .collect();

        assert_eq!(keys, vec!["alice", "bob", "charlie"]);
    }
}
