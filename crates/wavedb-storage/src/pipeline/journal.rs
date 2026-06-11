//! Append-only journal for crash recovery.
//!
//! Every mutation is appended here before the client receives confirmation.
//! On startup, the journal is replayed to reconcile in-memory state with
//! what was durable.
//!
//! # Durability guarantee
//!
//! Each `append()` call writes the entry bytes and calls `sync_all()` before
//! returning.  The file is always opened in `O_APPEND` mode — the kernel
//! never truncates it implicitly.  Compaction (via `truncate_through`) writes
//! surviving entries to a sibling `.tmp` file, syncs it, then atomically
//! renames it over the original — the live journal is never truncated.

use crate::anchor::{AnchorKey, AnchorSlot};
use crate::versioned::VersionedRecord;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use wavedb_macros::WaveWire;

/// A single journal entry.
#[derive(Debug, Clone, WaveWire)]
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
#[derive(Debug, Clone, Copy, WaveWire)]
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
///
/// On-disk invariant: the file grows monotonically.  It is only rewritten
/// during `truncate_through`, which uses an atomic rename so the live file
/// is never empty.
pub struct Journal {
    path: PathBuf,
    /// Open append-mode handle; `None` for in-memory journals.
    file: Option<File>,
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

        // O_APPEND: kernel never implicitly seeks back — safe concurrent writes
        // are a bonus; the real guarantee is we can never truncate by accident.
        let file = OpenOptions::new().create(true).append(true).open(path)?;

        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
            entries,
            sequence,
        })
    }

    /// Create an in-memory journal (for tests).
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            file: None,
            entries: Vec::new(),
            sequence: 0,
        }
    }

    /// Append an entry to the journal.
    ///
    /// For file-backed journals this writes the entry bytes immediately and
    /// calls `sync_all()` before returning — the entry is durable on `Ok(())`.
    pub fn append(&mut self, entry: JournalEntry) -> crate::StorageResult<()> {
        if let Some(file) = &mut self.file {
            let entry_bytes = wavedb_core::wire::to_wire(&entry)?;
            let len = u32::try_from(entry_bytes.len())
                .expect("journal entry too large");
            file.write_all(&len.to_le_bytes())?;
            file.write_all(&entry_bytes)?;
            file.sync_all()?;
        }
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

    /// Compact the journal, discarding all entries up to and including
    /// `through_sequence`.
    ///
    /// Surviving entries are written to a `.tmp` sibling, fsynced, then
    /// atomically renamed over the live file — the journal is **never empty**
    /// during the operation.  The in-memory `file` handle is reopened after
    /// the rename.
    pub fn truncate_through(
        &mut self,
        through_sequence: u64,
    ) -> crate::StorageResult<()> {
        // Drop everything up to and including the last checkpoint whose
        // sequence is <= through_sequence.  Entries after that point survive.
        let cp_pos = self.entries.iter().rposition(
            |e| matches!(e, JournalEntry::Checkpoint { sequence } if *sequence <= through_sequence),
        );
        if let Some(pos) = cp_pos {
            self.entries = self.entries.split_off(pos + 1);
        }

        if self.path.as_os_str().is_empty() {
            return Ok(());
        }

        // Write surviving entries to a temp file, fsync, then atomic rename.
        let tmp = self.path.with_extension("tmp");
        {
            let mut tmp_file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            let encoded = Self::encode_entries(&self.entries)?;
            tmp_file.write_all(&encoded)?;
            tmp_file.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;

        // Reopen in append mode after rename so the handle points at the new inode.
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?,
        );

        Ok(())
    }

    /// Ensure any OS-buffered writes are on stable storage.
    ///
    /// Each `append()` already calls `sync_all()`, so this is a no-op in the
    /// normal write path.  Callers that hold this as an extra safety barrier
    /// (e.g. after a batch of appends without intervening syncs) may keep
    /// calling it.
    pub fn flush(&self) -> crate::StorageResult<()> {
        if let Some(file) = &self.file {
            file.sync_all()?;
        }
        Ok(())
    }

    /// Encode entries to bytes (length-prefixed wire format).
    fn encode_entries(
        entries: &[JournalEntry],
    ) -> crate::StorageResult<Vec<u8>> {
        let mut buf = Vec::new();
        for entry in entries {
            let entry_bytes = wavedb_core::wire::to_wire(entry)?;
            let len = u32::try_from(entry_bytes.len())
                .expect("journal entry too large");
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
            let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap())
                as usize;
            pos += 4;
            if pos + len > data.len() {
                break; // truncated entry — journal was mid-write at crash
            }
            let entry: JournalEntry =
                wavedb_core::wire::from_wire(&data[pos..pos + len])?;
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

        let j2 = Journal::open(&path).unwrap();
        assert_eq!(j2.len(), 2); // 1 write + 1 checkpoint
    }

    #[test]
    fn journal_grows_monotonically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let mut j = Journal::open(&path).unwrap();

        let size_before = j.file_bytes();
        j.append(JournalEntry::WriteAnchor {
            key: AnchorKey::from_raw(1),
            slot: AnchorSlot::inline(b"x", 1),
        })
        .unwrap();
        let size_after = j.file_bytes();

        assert!(
            size_after > size_before,
            "journal must grow on every append"
        );
    }

    #[test]
    fn truncate_through_uses_atomic_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let mut j = Journal::open(&path).unwrap();

        j.append(JournalEntry::WriteAnchor {
            key: AnchorKey::from_raw(1),
            slot: AnchorSlot::inline(b"a", 1),
        })
        .unwrap();
        let seq = j.checkpoint().unwrap();
        j.append(JournalEntry::WriteAnchor {
            key: AnchorKey::from_raw(2),
            slot: AnchorSlot::inline(b"b", 2),
        })
        .unwrap();

        // Compact — removes everything up through the checkpoint.
        j.truncate_through(seq).unwrap();

        // The .tmp file must be gone (renamed away).
        assert!(
            !dir.path().join("journal.tmp").exists(),
            ".tmp must not remain after compaction"
        );

        // The live file must still exist (never empty).
        assert!(path.exists(), "journal.log must exist after compaction");

        // Reopen and verify only the post-checkpoint entry survived.
        let j2 = Journal::open(&path).unwrap();
        assert_eq!(
            j2.len(),
            1,
            "only post-checkpoint entries survive compaction"
        );
    }

    #[test]
    fn append_after_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.log");
        let mut j = Journal::open(&path).unwrap();

        j.append(JournalEntry::Checkpoint { sequence: 1 }).unwrap();
        j.truncate_through(1).unwrap();

        // Must still be able to append after compaction.
        j.append(JournalEntry::WriteAnchor {
            key: AnchorKey::from_raw(99),
            slot: AnchorSlot::inline(b"post", 99),
        })
        .unwrap();

        let j2 = Journal::open(&path).unwrap();
        assert_eq!(j2.len(), 1);
    }
}
