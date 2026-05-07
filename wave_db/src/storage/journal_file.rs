//! Append-only on-disk journal file.
//!
//! Every mutation is recorded here before returning to the caller. On startup
//! the journal is replayed to recover any operations that reached the journal
//! but not yet the data file.
//!
//! # Entry wire format
//!
//! ```text
//! [u32 payload_len][u8 tag][payload bytes]
//! ```
//!
//! `payload_len` includes the tag byte.
//!
//! | Tag  | Payload (all LE)                                             |
//! |------|--------------------------------------------------------------|
//! | 0x01 | `[u128 record_id][u16 struct_id][u64 tenant_id]` (26 B)     |
//! | 0x02 | `[u128 old_id][u128 new_id][u16 struct_id][u64 tenant_id]` (42 B) |
//! | 0x03 | `[u16 struct_id][u64 tenant_id][u128 final_version_id]` (26 B) |
//! | 0x04 | `[u16 struct_id][u64 tenant_id][u32 freed_bytes]` (14 B)    |

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

use crate::{
    id::{Id, StructId, TenantId},
    journal::JournalEntry,
    page::PageKey,
};

const TAG_INSERT: u8 = 0x01;
const TAG_MUTATE: u8 = 0x02;
const TAG_DELETE: u8 = 0x03;
const TAG_FREE_SPACE: u8 = 0x04;

// ─── JournalFile ─────────────────────────────────────────────────────────────

/// Append-only journal file for WaveDB mutations.
pub struct JournalFile {
    file: File,
    path: std::path::PathBuf,
}

impl JournalFile {
    /// Open an existing journal file or create a new empty one.
    pub fn open_or_create(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    // ── Append ───────────────────────────────────────────────────────────────

    /// Encode and append a [`JournalEntry`] to the file.
    pub fn append(&mut self, entry: &JournalEntry) -> io::Result<()> {
        let payload = encode_entry(entry);
        let total_len = payload.len() as u32;
        let mut buf = Vec::with_capacity(4 + payload.len());
        buf.extend_from_slice(&total_len.to_le_bytes());
        buf.extend_from_slice(&payload);
        self.file.write_all(&buf)
    }

    // ── Replay ───────────────────────────────────────────────────────────────

    /// Read and decode all valid entries from the beginning of the file.
    ///
    /// On a partial write at the tail (caused by a crash), decoding stops at
    /// the last complete entry.
    pub fn replay(&mut self) -> io::Result<Vec<JournalEntry>> {
        let file_len = self.file.seek(SeekFrom::End(0))?;
        self.file.seek(SeekFrom::Start(0))?;

        let mut entries = Vec::new();
        let mut pos: u64 = 0;

        while pos + 4 <= file_len {
            // Read the 4-byte length prefix
            let mut len_buf = [0u8; 4];
            self.file.read_exact(&mut len_buf)?;
            let payload_len = u32::from_le_bytes(len_buf) as u64;

            if pos + 4 + payload_len > file_len {
                // Partial write — truncate to last good entry
                self.truncate_to(pos)?;
                break;
            }

            let mut payload = vec![0u8; payload_len as usize];
            self.file.read_exact(&mut payload)?;

            match decode_entry(&payload) {
                Ok(entry) => entries.push(entry),
                Err(_) => {
                    // Corrupt entry — stop here
                    self.truncate_to(pos)?;
                    break;
                }
            }

            pos += 4 + payload_len;
        }

        Ok(entries)
    }

    // ── Maintenance ──────────────────────────────────────────────────────────

    /// Flush and fsync the journal file.
    pub fn flush(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.sync_data()
    }

    /// Truncate the journal to zero bytes (called after drain into data file).
    pub fn truncate(&self) -> io::Result<()> {
        // Re-open with truncate flag; the append-mode handle can't truncate.
        let _ = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        Ok(())
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    fn truncate_to(&self, byte_pos: u64) -> io::Result<()> {
        let file = OpenOptions::new().write(true).open(&self.path)?;
        file.set_len(byte_pos)
    }
}

// ─── Encoding ─────────────────────────────────────────────────────────────────

fn encode_entry(entry: &JournalEntry) -> Vec<u8> {
    let mut buf = Vec::new();
    match entry {
        JournalEntry::Insert {
            record_id,
            anchor_key,
        } => {
            buf.push(TAG_INSERT);
            buf.extend_from_slice(&record_id.raw().to_le_bytes());
            buf.extend_from_slice(&anchor_key.struct_id.value().to_le_bytes());
            buf.extend_from_slice(&anchor_key.tenant_id.value().to_le_bytes());
        }
        JournalEntry::Mutate {
            old_id,
            new_id,
            anchor_key,
        } => {
            buf.push(TAG_MUTATE);
            buf.extend_from_slice(&old_id.raw().to_le_bytes());
            buf.extend_from_slice(&new_id.raw().to_le_bytes());
            buf.extend_from_slice(&anchor_key.struct_id.value().to_le_bytes());
            buf.extend_from_slice(&anchor_key.tenant_id.value().to_le_bytes());
        }
        JournalEntry::Delete {
            anchor_key,
            final_version_id,
        } => {
            buf.push(TAG_DELETE);
            buf.extend_from_slice(&anchor_key.struct_id.value().to_le_bytes());
            buf.extend_from_slice(&anchor_key.tenant_id.value().to_le_bytes());
            buf.extend_from_slice(&final_version_id.raw().to_le_bytes());
        }
        JournalEntry::FreeSpace {
            page_key,
            freed_bytes,
        } => {
            buf.push(TAG_FREE_SPACE);
            buf.extend_from_slice(&page_key.struct_id.value().to_le_bytes());
            buf.extend_from_slice(&page_key.tenant_id.value().to_le_bytes());
            buf.extend_from_slice(&freed_bytes.to_le_bytes());
        }
    }
    buf
}

// ─── Decoding ─────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum DecodeError {
    UnknownTag(()),
    TooShort,
}

fn decode_entry(payload: &[u8]) -> Result<JournalEntry, DecodeError> {
    if payload.is_empty() {
        return Err(DecodeError::TooShort);
    }
    let tag = payload[0];
    let body = &payload[1..];

    match tag {
        TAG_INSERT => {
            if body.len() < 26 {
                return Err(DecodeError::TooShort);
            }
            let record_id = Id::from_raw(u128::from_le_bytes(body[0..16].try_into().unwrap()));
            let struct_id = StructId::new(u16::from_le_bytes(body[16..18].try_into().unwrap()));
            let tenant_id = TenantId::new(u64::from_le_bytes(body[18..26].try_into().unwrap()));
            Ok(JournalEntry::Insert {
                record_id,
                anchor_key: PageKey::new(struct_id, tenant_id),
            })
        }
        TAG_MUTATE => {
            if body.len() < 42 {
                return Err(DecodeError::TooShort);
            }
            let old_id = Id::from_raw(u128::from_le_bytes(body[0..16].try_into().unwrap()));
            let new_id = Id::from_raw(u128::from_le_bytes(body[16..32].try_into().unwrap()));
            let struct_id = StructId::new(u16::from_le_bytes(body[32..34].try_into().unwrap()));
            let tenant_id = TenantId::new(u64::from_le_bytes(body[34..42].try_into().unwrap()));
            Ok(JournalEntry::Mutate {
                old_id,
                new_id,
                anchor_key: PageKey::new(struct_id, tenant_id),
            })
        }
        TAG_DELETE => {
            if body.len() < 26 {
                return Err(DecodeError::TooShort);
            }
            let struct_id = StructId::new(u16::from_le_bytes(body[0..2].try_into().unwrap()));
            let tenant_id = TenantId::new(u64::from_le_bytes(body[2..10].try_into().unwrap()));
            let final_id = Id::from_raw(u128::from_le_bytes(body[10..26].try_into().unwrap()));
            Ok(JournalEntry::Delete {
                anchor_key: PageKey::new(struct_id, tenant_id),
                final_version_id: final_id,
            })
        }
        TAG_FREE_SPACE => {
            if body.len() < 14 {
                return Err(DecodeError::TooShort);
            }
            let struct_id = StructId::new(u16::from_le_bytes(body[0..2].try_into().unwrap()));
            let tenant_id = TenantId::new(u64::from_le_bytes(body[2..10].try_into().unwrap()));
            let freed = u32::from_le_bytes(body[10..14].try_into().unwrap());
            Ok(JournalEntry::FreeSpace {
                page_key: PageKey::new(struct_id, tenant_id),
                freed_bytes: freed,
            })
        }
        _other => Err(DecodeError::UnknownTag(())),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{ShardId, Slider, Timestamp};
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    fn make_id(ts: u64) -> Id {
        Id::new(
            TenantId::new(1),
            ShardId::new(0),
            StructId::new(2),
            Timestamp::from_ticks(ts),
            Slider::new(0),
        )
    }

    fn key() -> PageKey {
        PageKey::new(StructId::new(2), TenantId::new(1))
    }

    #[test]
    fn append_and_replay() {
        let p = tmp("wave_jf_test.bin");
        let _ = std::fs::remove_file(&p);
        let entries = vec![
            JournalEntry::Insert {
                record_id: make_id(1),
                anchor_key: key(),
            },
            JournalEntry::Mutate {
                old_id: make_id(1),
                new_id: make_id(2),
                anchor_key: key(),
            },
            JournalEntry::Delete {
                anchor_key: key(),
                final_version_id: make_id(2),
            },
            JournalEntry::FreeSpace {
                page_key: key(),
                freed_bytes: 256,
            },
        ];
        {
            let mut jf = JournalFile::open_or_create(&p).unwrap();
            for e in &entries {
                jf.append(e).unwrap();
            }
        }
        let mut jf2 = JournalFile::open_or_create(&p).unwrap();
        let replayed = jf2.replay().unwrap();
        assert_eq!(replayed.len(), 4);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn truncate_empties_journal() {
        let p = tmp("wave_jf_trunc.bin");
        let _ = std::fs::remove_file(&p);
        {
            let mut jf = JournalFile::open_or_create(&p).unwrap();
            jf.append(&JournalEntry::Insert {
                record_id: make_id(1),
                anchor_key: key(),
            })
            .unwrap();
            jf.truncate().unwrap();
        }
        let mut jf2 = JournalFile::open_or_create(&p).unwrap();
        let replayed = jf2.replay().unwrap();
        assert_eq!(replayed.len(), 0);
        let _ = std::fs::remove_file(&p);
    }
}
