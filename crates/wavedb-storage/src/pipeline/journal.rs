//! Append-only journal for crash recovery.
//!
//! Every mutation is appended here before the client receives confirmation.
//! On startup, the journal is replayed to reconcile in-memory state with
//! what was durable.

use crate::anchor::{AnchorKey, AnchorSlot};
use crate::versioned::VersionedRecord;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A single journal entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JournalEntry {
    /// Write or update an anchor slot.
    WriteAnchor {
        /// The anchor address.
        key: AnchorKey,
        /// The new slot content.
        slot: AnchorSlot,
    },
    /// Write a versioned record.
    WriteVersioned {
        /// The versioned record to persist.
        record: VersionedRecord,
    },
    /// Delete an anchor (tombstone).
    DeleteAnchor {
        /// The anchor address to tombstone.
        key: AnchorKey,
    },
    /// Free-space delta: record that a range was freed in a file.
    FreeSpace {
        /// Which file the freed range belongs to.
        file_kind: FileKind,
        /// Byte offset of the freed range.
        offset: u64,
        /// Number of bytes freed.
        size: u64,
    },
    /// Dictionary update for a STRUCT_ID.
    DictUpdate {
        /// The struct family the dictionary covers.
        struct_id: u32,
        /// The new dictionary version.
        version: u32,
        /// Raw zstd dictionary bytes.
        data: Vec<u8>,
    },
    /// Checkpoint marker: everything before this has been drained.
    Checkpoint {
        /// Monotonic sequence number of this checkpoint.
        sequence: u64,
    },
}

/// Which file a free-space entry refers to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum FileKind {
    /// The data file.
    Data,
    /// The index file.
    Index,
    /// The heap file.
    Heap,
    /// The journal itself.
    Journal,
}

/// The append-only journal.
pub struct Journal {
    path: PathBuf,
    entries: Vec<JournalEntry>,
    /// Monotonic sequence number for checkpoint ordering.
    sequence: u64,
}

impl Journal {
    /// Open or create a journal file. Replays existing entries on open.
    pub fn open(path: &Path) -> crate::StorageResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let entries = if path.exists() {
            let data = std::fs::read(path)?;
            if data.is_empty() {
                Vec::new()
            } else {
                // Each entry is length-prefixed: [u32 len][postcard bytes]
                Self::decode_entries(&data)?
            }
        } else {
            Vec::new()
        };

        let sequence = entries
            .iter()
            .filter_map(|e| {
                if let JournalEntry::Checkpoint { sequence } = e {
                    Some(*sequence)
                } else {
                    None
                }
            })
            .max()
            .unwrap_or(0);

        Ok(Self {
            path: path.to_path_buf(),
            entries,
            sequence,
        })
    }

    /// Create an in-memory journal (for tests).
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            entries: Vec::new(),
            sequence: 0,
        }
    }

    /// Append an entry to the journal.
    pub fn append(&mut self, entry: JournalEntry) -> crate::StorageResult<()> {
        self.entries.push(entry);
        Ok(())
    }

    /// Write a checkpoint marker. Returns the checkpoint sequence number.
    pub fn checkpoint(&mut self) -> crate::StorageResult<u64> {
        self.sequence += 1;
        self.append(JournalEntry::Checkpoint {
            sequence: self.sequence,
        })?;
        Ok(self.sequence)
    }

    /// Get all entries since the last checkpoint (for replay).
    pub fn entries_since_checkpoint(&self) -> &[JournalEntry] {
        let last_cp = self
            .entries
            .iter()
            .rposition(|e| matches!(e, JournalEntry::Checkpoint { .. }));

        match last_cp {
            Some(idx) => &self.entries[idx + 1..],
            None => &self.entries,
        }
    }

    /// Get all entries (for full replay on startup).
    pub fn all_entries(&self) -> &[JournalEntry] {
        &self.entries
    }

    /// Number of entries in the journal.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the journal is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Actual size of the journal file on disk; 0 for in-memory journals.
    pub fn file_bytes(&self) -> u64 {
        if self.path.as_os_str().is_empty() {
            return 0;
        }
        std::fs::metadata(&self.path).map_or(0, |m| m.len())
    }

    /// Truncate the journal, removing all entries up to and including
    /// the given checkpoint sequence.
    pub fn truncate_through(&mut self, through_sequence: u64) {
        self.entries.retain(|e| match e {
            JournalEntry::Checkpoint { sequence } => *sequence > through_sequence,
            _ => true,
        });
        // For simplicity, remove all entries before the first remaining checkpoint
        if let Some(first_cp) = self
            .entries
            .iter()
            .position(|e| matches!(e, JournalEntry::Checkpoint { .. }))
        {
            // Keep entries from the first checkpoint onward
            self.entries = self.entries.split_off(first_cp);
        }
    }

    /// Flush the journal to disk.
    pub fn flush(&self) -> crate::StorageResult<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let encoded = Self::encode_entries(&self.entries)?;
        std::fs::write(&self.path, encoded)?;
        Ok(())
    }

    /// Encode entries to bytes (length-prefixed postcard).
    fn encode_entries(entries: &[JournalEntry]) -> crate::StorageResult<Vec<u8>> {
        let mut buf = Vec::new();
        for entry in entries {
            let entry_bytes = postcard::to_allocvec(entry)?;
            let len = u32::try_from(entry_bytes.len()).expect("journal entry too large");
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(&entry_bytes);
        }
        Ok(buf)
    }

    /// Decode entries from bytes.
    fn decode_entries(data: &[u8]) -> crate::StorageResult<Vec<JournalEntry>> {
        let mut entries = Vec::new();
        let mut pos = 0;
        while pos + 4 <= data.len() {
            let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            if pos + len > data.len() {
                break; // truncated entry — journal was mid-write
            }
            let entry: JournalEntry = postcard::from_bytes(&data[pos..pos + len])?;
            entries.push(entry);
            pos += len;
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read() {
        let mut journal = Journal::in_memory();
        journal
            .append(JournalEntry::WriteAnchor {
                key: AnchorKey::from_raw(42),
                slot: AnchorSlot::inline(b"data", 1000),
            })
            .unwrap();
        assert_eq!(journal.len(), 1);
    }

    #[test]
    fn checkpoint_and_entries_since() {
        let mut journal = Journal::in_memory();
        journal
            .append(JournalEntry::WriteAnchor {
                key: AnchorKey::from_raw(1),
                slot: AnchorSlot::inline(b"a", 1),
            })
            .unwrap();
        journal.checkpoint().unwrap();
        journal
            .append(JournalEntry::WriteAnchor {
                key: AnchorKey::from_raw(2),
                slot: AnchorSlot::inline(b"b", 2),
            })
            .unwrap();

        let since = journal.entries_since_checkpoint();
        assert_eq!(since.len(), 1);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let entries = vec![
            JournalEntry::WriteAnchor {
                key: AnchorKey::from_raw(42),
                slot: AnchorSlot::inline(b"data", 1000),
            },
            JournalEntry::Checkpoint { sequence: 1 },
            JournalEntry::DeleteAnchor {
                key: AnchorKey::from_raw(42),
            },
        ];
        let encoded = Journal::encode_entries(&entries).unwrap();
        let decoded = Journal::decode_entries(&encoded).unwrap();
        assert_eq!(decoded.len(), 3);
    }

    #[test]
    fn file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");

        {
            let mut j = Journal::open(&path).unwrap();
            j.append(JournalEntry::WriteAnchor {
                key: AnchorKey::from_raw(1),
                slot: AnchorSlot::inline(b"test", 100),
            })
            .unwrap();
            j.checkpoint().unwrap();
            j.flush().unwrap();
        }

        // Reopen and verify
        let j2 = Journal::open(&path).unwrap();
        assert_eq!(j2.len(), 2); // 1 write + 1 checkpoint
    }
}
