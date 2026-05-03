//! Append-only mutation journal.
//!
//! Every durable state change — insert, mutate, delete, or freed-space delta —
//! is appended here before the client receives confirmation. On startup the
//! journal is replayed to recover in-flight mutations that did not make it into
//! the durable data/index/heap files.
//!
//! In the research phase this is entirely in-memory. A production
//! implementation would memory-map the journal file and flush entries with an
//! `fsync` before confirming writes.

use crate::id::Id;
use crate::page::PageKey;

// ─── Journal entry ────────────────────────────────────────────────────────────

/// A single durable mutation event.
#[derive(Debug, Clone)]
pub enum JournalEntry {
    /// A brand-new record was inserted (first version, no predecessor).
    Insert {
        /// Full composite ID of the new versioned record.
        record_id: Id,
        /// The anchor slot that was created/updated.
        anchor_key: PageKey,
    },

    /// An existing record was mutated.
    ///
    /// Guarantees:
    /// - `new_id` is now the live version at `anchor_key`
    /// - `old_id`'s `new_modification_id` was updated to point to `new_id`
    Mutate {
        /// ID of the previous live version.
        old_id: Id,
        /// ID of the newly written versioned record.
        new_id: Id,
        /// Anchor slot overwritten by this mutation.
        anchor_key: PageKey,
    },

    /// A record was logically deleted.
    ///
    /// The anchor at `anchor_key` now holds a tombstone; the versioned history
    /// chain starting at `final_version_id` remains intact.
    Delete {
        /// The anchor that became a tombstone.
        anchor_key: PageKey,
        /// Full ID of the last live version before deletion.
        final_version_id: Id,
    },

    /// A byte range was freed and can be reclaimed during the next compaction.
    ///
    /// These deltas are the single source of truth for reclaimable space.
    FreeSpace {
        /// The page this delta belongs to.
        page_key: PageKey,
        /// Number of bytes freed.
        freed_bytes: u32,
    },
}

// ─── Journal ──────────────────────────────────────────────────────────────────

/// Append-only mutation journal.
///
/// Anchor + versioned writes are always journaled together so that a crash
/// mid-mutation can never leave the two slots inconsistent after replay.
#[derive(Debug, Default)]
pub struct Journal {
    entries: Vec<JournalEntry>,
}

impl Journal {
    /// Create an empty journal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a new entry (the fast write path).
    pub fn append(&mut self, entry: JournalEntry) {
        self.entries.push(entry);
    }

    /// Iterate over all entries in append order.
    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }

    /// Number of entries currently in the journal.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the journal contains no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drain all entries for replay or compaction, leaving the journal empty.
    ///
    /// In a production implementation this would also truncate the on-disk file
    /// after persisting drained entries to the durable data/index/heap files.
    pub fn drain(&mut self) -> Vec<JournalEntry> {
        std::mem::take(&mut self.entries)
    }

    /// Count entries by variant for diagnostics.
    pub fn stats(&self) -> JournalStats {
        let mut stats = JournalStats::default();
        for entry in &self.entries {
            match entry {
                JournalEntry::Insert { .. } => stats.inserts += 1,
                JournalEntry::Mutate { .. } => stats.mutations += 1,
                JournalEntry::Delete { .. } => stats.deletes += 1,
                JournalEntry::FreeSpace { .. } => stats.free_space_deltas += 1,
            }
        }
        stats
    }
}

/// Summary counts of journal entry types.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct JournalStats {
    pub inserts: usize,
    pub mutations: usize,
    pub deletes: usize,
    pub free_space_deltas: usize,
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{Id, ShardId, Slider, StructId, TenantId, Timestamp};

    fn make_id(ts: u64) -> Id {
        Id::new(
            TenantId::new(1),
            ShardId::new(0),
            StructId::new(2),
            Timestamp::from_ticks(ts),
            Slider::new(0),
        )
    }

    fn make_key() -> PageKey {
        PageKey::new(StructId::new(2), TenantId::new(1))
    }

    #[test]
    fn append_and_len() {
        let mut j = Journal::new();
        assert!(j.is_empty());
        j.append(JournalEntry::Insert { record_id: make_id(1), anchor_key: make_key() });
        assert_eq!(j.len(), 1);
        assert!(!j.is_empty());
    }

    #[test]
    fn drain_clears_journal() {
        let mut j = Journal::new();
        j.append(JournalEntry::Insert { record_id: make_id(1), anchor_key: make_key() });
        j.append(JournalEntry::Delete {
            anchor_key: make_key(),
            final_version_id: make_id(1),
        });
        let drained = j.drain();
        assert_eq!(drained.len(), 2);
        assert!(j.is_empty());
    }

    #[test]
    fn stats() {
        let mut j = Journal::new();
        j.append(JournalEntry::Insert { record_id: make_id(1), anchor_key: make_key() });
        j.append(JournalEntry::Mutate {
            old_id: make_id(1),
            new_id: make_id(2),
            anchor_key: make_key(),
        });
        j.append(JournalEntry::Mutate {
            old_id: make_id(2),
            new_id: make_id(3),
            anchor_key: make_key(),
        });
        j.append(JournalEntry::Delete { anchor_key: make_key(), final_version_id: make_id(3) });
        j.append(JournalEntry::FreeSpace { page_key: make_key(), freed_bytes: 128 });

        let s = j.stats();
        assert_eq!(s.inserts, 1);
        assert_eq!(s.mutations, 2);
        assert_eq!(s.deletes, 1);
        assert_eq!(s.free_space_deltas, 1);
    }
}
