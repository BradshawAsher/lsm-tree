use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use bytes::Bytes;

use crate::error::Result;
use crate::sstable::{SsTableBuilder, SsTableReader};

/// Executes a multi-way merge compaction across multiple SSTables.
/// - Merges sorted entries from newest to oldest.
/// - Deduplicates keys: keeps only the freshest version.
/// - Discards obsolete tombstones if `is_bottom_level` is true (reclaiming disk space).
/// - Atomically writes the compacted stream into a new SSTable.
pub fn compact_sstables<P: AsRef<Path>>(
    dir: P,
    sstables: &mut [SsTableReader],
    new_sst_id: u64,
    is_bottom_level: bool,
) -> Result<(PathBuf, Vec<PathBuf>)> {
    let dir = dir.as_ref();
    let new_path = dir.join(format!("{:06}.sst", new_sst_id));

    // Map to collect the newest version of every key (BTreeMap ensures final output is sorted)
    let mut merged_entries: BTreeMap<Bytes, Option<Bytes>> = BTreeMap::new();
    let mut seen_keys = HashSet::new();
    let mut old_paths = Vec::new();

    // SSTables are ordered newest (idx 0) to oldest
    for sstable in sstables.iter_mut() {
        old_paths.push(sstable.path().to_path_buf());
        let entries = sstable.read_all_entries()?;

        for (k, v_opt) in entries {
            if !seen_keys.contains(&k) {
                seen_keys.insert(k.clone());

                // If tombstone at the bottom level, we can completely purge it!
                if v_opt.is_none() && is_bottom_level {
                    continue; // Evicted tombstone!
                }

                merged_entries.insert(k, v_opt);
            }
        }
    }

    // Build the new compacted SSTable
    let mut builder = SsTableBuilder::create(&new_path)?;
    for (k, v_opt) in merged_entries {
        builder.add(&k, v_opt.as_deref())?;
    }
    builder.finish()?;

    // Delete old SSTable files from disk
    for old_path in &old_paths {
        if old_path.exists() {
            let _ = fs::remove_file(old_path);
        }
    }

    Ok((new_path, old_paths))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_compaction_deduplication_and_tombstone_eviction() -> Result<()> {
        let temp = tempdir()?;
        let dir = temp.path();

        // SSTable 1 (Oldest): keyA=1, keyB=1, keyC=1
        let sst1_path = dir.join("000001.sst");
        {
            let mut b = SsTableBuilder::create(&sst1_path)?;
            b.add(b"keyA", Some(b"1"))?;
            b.add(b"keyB", Some(b"1"))?;
            b.add(b"keyC", Some(b"1"))?;
            b.finish()?;
        }

        // SSTable 2 (Newer): keyA=2 (overwrite), keyB=None (deleted via tombstone)
        let sst2_path = dir.join("000002.sst");
        {
            let mut b = SsTableBuilder::create(&sst2_path)?;
            b.add(b"keyA", Some(b"2"))?;
            b.add(b"keyB", None)?;
            b.finish()?;
        }

        let mut readers = vec![
            SsTableReader::open(&sst2_path)?, // Newer at idx 0
            SsTableReader::open(&sst1_path)?, // Older at idx 1
        ];

        // Perform bottom-level compaction into 000003.sst
        let (new_sst_path, old_paths) = compact_sstables(dir, &mut readers, 3, true)?;
        assert_eq!(new_sst_path, dir.join("000003.sst"));
        assert_eq!(old_paths.len(), 2);

        // Old files should have been removed
        assert!(!sst1_path.exists());
        assert!(!sst2_path.exists());

        // Inspect new compacted SSTable
        let mut compacted_reader = SsTableReader::open(&new_sst_path)?;
        let all_entries = compacted_reader.read_all_entries()?;

        // keyA should be 2 (freshest value)
        // keyB should be evicted completely (tombstone reclaimed)
        // keyC should be 1
        assert_eq!(all_entries.len(), 2);
        assert_eq!(all_entries[0].0.as_ref(), b"keyA");
        assert_eq!(all_entries[0].1, Some(Bytes::from("2")));

        assert_eq!(all_entries[1].0.as_ref(), b"keyC");
        assert_eq!(all_entries[1].1, Some(Bytes::from("1")));

        Ok(())
    }
}
