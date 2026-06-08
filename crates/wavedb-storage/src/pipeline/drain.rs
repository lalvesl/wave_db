//! Background drain actor: flushes cached mutations to the durable files.

use crate::cache::Cache;
use crate::file::data::DataFile;
use crate::pipeline::journal::{Journal, JournalEntry};

/// Drain result after processing a batch of journal entries.
#[derive(Debug)]
pub struct DrainResult {
    /// Number of entries successfully drained.
    pub drained: usize,
    /// Checkpoint sequence number written after drain.
    pub checkpoint_seq: u64,
}

/// Drain pending journal entries into the data file.
///
/// In production this runs as an async task; here it's a sync function
/// that can be called from tests or wrapped in a tokio::spawn.
pub fn drain_journal(
    journal: &mut Journal,
    data_file: &DataFile,
    _cache: &Cache,
) -> crate::StorageResult<DrainResult> {
    let entries: Vec<JournalEntry> =
        journal.entries_since_checkpoint().to_vec();
    let mut drained = 0;

    for entry in &entries {
        match entry {
            JournalEntry::WriteAnchor { key, slot } => {
                data_file.write_anchor(*key, slot)?;
                drained += 1;
            }
            JournalEntry::WriteVersioned { record } => {
                data_file.write_versioned(record)?;
                drained += 1;
            }
            JournalEntry::DeleteAnchor { key } => {
                // Write a tombstone
                let tombstone = crate::anchor::AnchorSlot::primary_tombstone(0);
                data_file.write_anchor(*key, &tombstone)?;
                drained += 1;
            }
            JournalEntry::FreeSpace { .. } => {
                // Free-space bookkeeping — handled during compaction
                drained += 1;
            }
            JournalEntry::DictUpdate { .. } => {
                // Dictionary updates — handled by the dict cache
                drained += 1;
            }
            JournalEntry::Checkpoint { .. } => {
                // Checkpoints are not drained
            }
        }
    }

    let checkpoint_seq = journal.checkpoint()?;

    Ok(DrainResult {
        drained,
        checkpoint_seq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::{AnchorKey, AnchorSlot};
    use crate::versioned::VersionedRecord;

    #[test]
    fn drain_writes_to_data_file() {
        let dir = tempfile::tempdir().unwrap();
        let data_file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        let cache = Cache::new();
        let mut journal = Journal::in_memory();

        let key = AnchorKey::from_raw(
            wavedb_core::Id::new(1, 0, 1, 1000).anchor_key().raw(),
        );
        let slot = AnchorSlot::inline(b"drain test", 1000);

        journal
            .append(JournalEntry::WriteAnchor { key, slot })
            .unwrap();

        let result = drain_journal(&mut journal, &data_file, &cache).unwrap();
        assert_eq!(result.drained, 1);

        // Verify the anchor is now in the data file
        let got = data_file.read_anchor(key).unwrap();
        assert!(got.is_some());
    }

    #[test]
    fn drain_multiple_entry_types() {
        let dir = tempfile::tempdir().unwrap();
        let data_file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
        let cache = Cache::new();
        let mut journal = Journal::in_memory();

        let id1 = wavedb_core::Id::new(1, 0, 1, 1000);
        let key1 = AnchorKey::from(id1);

        journal
            .append(JournalEntry::WriteAnchor {
                key: key1,
                slot: AnchorSlot::inline(b"hello", 1000),
            })
            .unwrap();

        journal
            .append(JournalEntry::WriteVersioned {
                record: VersionedRecord::new(
                    id1.raw(),
                    b"version data".to_vec(),
                ),
            })
            .unwrap();

        let result = drain_journal(&mut journal, &data_file, &cache).unwrap();
        assert_eq!(result.drained, 2);
    }
}
