use std::collections::BinaryHeap;
use bytes::Bytes;

use crate::error::Result;

#[derive(Debug)]
struct HeapNode {
    key: Bytes,
    value: Option<Bytes>,
    iterator_idx: usize,
}

impl PartialEq for HeapNode {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.iterator_idx == other.iterator_idx
    }
}

impl Eq for HeapNode {}

impl PartialOrd for HeapNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse order so BinaryHeap functions as a min-heap:
        // 1. Smallest key is popped first.
        // 2. If keys are equal, smallest iterator_idx (freshest source) is popped first.
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.iterator_idx.cmp(&self.iterator_idx))
    }
}

/// A K-way merge iterator that coordinates sorted streams from MemTable,
/// immutable MemTables, and SSTables.
///
/// Features:
/// - Min-heap priority queue for O(log K) item selection.
/// - On-the-fly deduplication: only the freshest version of any key is retained.
/// - Automatic tombstone masking: deleted keys are pruned from user-facing results.
pub struct MergeIterator {
    iterators: Vec<Box<dyn Iterator<Item = Result<(Bytes, Option<Bytes>)>>>>,
    heap: BinaryHeap<HeapNode>,
    initialized: bool,
}

impl MergeIterator {
    pub fn new(iterators: Vec<Box<dyn Iterator<Item = Result<(Bytes, Option<Bytes>)>>>>) -> Self {
        Self {
            iterators,
            heap: BinaryHeap::new(),
            initialized: false,
        }
    }

    fn init(&mut self) -> Result<()> {
        if self.initialized {
            return Ok(());
        }
        self.initialized = true;

        for (idx, iter) in self.iterators.iter_mut().enumerate() {
            if let Some(item_res) = iter.next() {
                let (key, value) = item_res?;
                self.heap.push(HeapNode {
                    key,
                    value,
                    iterator_idx: idx,
                });
            }
        }

        Ok(())
    }
}

impl Iterator for MergeIterator {
    type Item = Result<(Bytes, Bytes)>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Err(e) = self.init() {
            return Some(Err(e));
        }

        loop {
            // Pop the lowest key across all active iterators (freshest priority)
            let current = self.heap.pop()?;

            // Advance the iterator that yielded `current`
            if let Some(next_res) = self.iterators[current.iterator_idx].next() {
                match next_res {
                    Ok((k, v)) => {
                        self.heap.push(HeapNode {
                            key: k,
                            value: v,
                            iterator_idx: current.iterator_idx,
                        });
                    }
                    Err(e) => return Some(Err(e)),
                }
            }

            // Deduplication: drain all older versions of `current.key` from other iterators
            while let Some(top) = self.heap.peek() {
                if top.key == current.key {
                    let stale = self.heap.pop().unwrap();
                    if let Some(next_res) = self.iterators[stale.iterator_idx].next() {
                        match next_res {
                            Ok((k, v)) => {
                                self.heap.push(HeapNode {
                                    key: k,
                                    value: v,
                                    iterator_idx: stale.iterator_idx,
                                });
                            }
                            Err(e) => return Some(Err(e)),
                        }
                    }
                } else {
                    break;
                }
            }

            // If the freshest version is a tombstone (None), discard it and continue
            if let Some(val) = current.value {
                return Some(Ok((current.key, val)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_iterator_deduplication_and_tombstones() -> Result<()> {
        // Stream 0 (Freshest - e.g. MemTable):
        // k1 -> None (Deleted!)
        // k2 -> Some("v2_new")
        let stream0: Vec<Result<(Bytes, Option<Bytes>)>> = vec![
            Ok((Bytes::from("k1"), None)),
            Ok((Bytes::from("k2"), Some(Bytes::from("v2_new")))),
        ];

        // Stream 1 (Older - e.g. SSTable 1):
        // k1 -> Some("v1_old")
        // k2 -> Some("v2_old")
        // k3 -> Some("v3")
        let stream1: Vec<Result<(Bytes, Option<Bytes>)>> = vec![
            Ok((Bytes::from("k1"), Some(Bytes::from("v1_old")))),
            Ok((Bytes::from("k2"), Some(Bytes::from("v2_old")))),
            Ok((Bytes::from("k3"), Some(Bytes::from("v3")))),
        ];

        // Stream 2 (Oldest - e.g. SSTable 2):
        // k0 -> Some("v0")
        // k3 -> Some("v3_ancient")
        let stream2: Vec<Result<(Bytes, Option<Bytes>)>> = vec![
            Ok((Bytes::from("k0"), Some(Bytes::from("v0")))),
            Ok((Bytes::from("k3"), Some(Bytes::from("v3_ancient")))),
        ];

        let iterators: Vec<Box<dyn Iterator<Item = Result<(Bytes, Option<Bytes>)>>>> = vec![
            Box::new(stream0.into_iter()),
            Box::new(stream1.into_iter()),
            Box::new(stream2.into_iter()),
        ];

        let merge_iter = MergeIterator::new(iterators);
        let results: Result<Vec<(Bytes, Bytes)>> = merge_iter.collect();
        let results = results?;

        // Expected output:
        // k0 -> v0 (from stream 2)
        // k1 -> dropped (stream 0 tombstone masked stream 1)
        // k2 -> v2_new (stream 0 took precedence over stream 1)
        // k3 -> v3 (stream 1 took precedence over stream 2)
        assert_eq!(results.len(), 3);
        assert_eq!(results[0], (Bytes::from("k0"), Bytes::from("v0")));
        assert_eq!(results[1], (Bytes::from("k2"), Bytes::from("v2_new")));
        assert_eq!(results[2], (Bytes::from("k3"), Bytes::from("v3")));

        Ok(())
    }
}
