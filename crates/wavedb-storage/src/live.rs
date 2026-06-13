//! Live-record tracker: the per-`(STRUCT_ID, TENANT_ID)` list of record IDs
//! the read path serves.
//!
//! `handle_write` commits every record under a fresh full Id (tuple4 slot)
//! — addressable only when the Id is already known.  The tracker is what
//! makes records *discoverable*: an ordered list of live IDs per
//! `(struct, tenant)`, stored inside the engine's own page file.
//!
//! # Layout
//!
//! One page holds at most a few hundred IDs, so the list is **chained**:
//!
//! - **Head** — an [`AnchorSlot`] in the `(struct_id, tenant)` tuple2 slot
//!   (the "NonUnique tracker" namespace).  Inline bytes are a wire-encoded
//!   [`LiveHead`]: the count of sealed segments plus the open (append)
//!   segment's IDs.
//! - **Sealed segments** — once the open list reaches
//!   [`LIVE_SEGMENT_CAP`] entries it is written out as a
//!   [`VersionedRecord`] under the synthetic Id
//!   `(tenant, LIVE_TRACKER_SHARD, struct_id, segment_no)` and the open
//!   list restarts empty.
//!
//! `SHARD_ID = 0xFFF` ([`LIVE_TRACKER_SHARD`]) is reserved for tracker
//! segments; client writes always carry shard `0` today, and the reserved
//! value keeps segment slots out of every user-visible namespace.
//!
//! # Ordering and semantics
//!
//! IDs append in commit order (`created_at` carries the node's monotonic
//! write sequence), so:
//!
//! - the **last** entry is the newest record — what `SearchUnique` returns,
//! - the full list is the live set — what `QueryNonUnique` scans,
//! - `live_remove` is the delete path (the record itself stays put as
//!   history; it just stops being discoverable).
//!
//! # Durability
//!
//! Tracker writes go straight into the page file (no WAL entry of their
//! own).  Crash recovery replays journalled `WriteVersioned` records and
//! calls [`DataFile::live_rebuild`] per touched `(struct, tenant)` — the
//! union of the on-disk tracker and the replayed IDs, deduplicated and
//! re-sorted, so the tracker converges no matter where the crash landed.

use wavedb_core::Id;
use wavedb_macros::WaveWire;

use crate::anchor::AnchorSlot;
use crate::file::data::DataFile;
use crate::versioned::VersionedRecord;
use crate::{StorageError, StorageResult};

/// IDs per sealed segment.  256 × 16 B ≈ 4 KiB of payload — comfortably
/// inside one page entry at the default 8 KiB page size, including the
/// head that carries an open list of the same capacity.
pub const LIVE_SEGMENT_CAP: usize = 256;

/// Reserved `SHARD_ID` for tracker segment records.
pub const LIVE_TRACKER_SHARD: u16 = 0xFFF;

/// Wire shape of the tracker head stored in the tuple2 anchor slot.
#[derive(Debug, Clone, Default, WaveWire)]
struct LiveHead {
    /// Number of sealed segments (`0..sealed` are valid segment numbers).
    sealed: u64,
    /// The open (append) segment.
    open: Vec<u128>,
}

/// Synthetic Id of sealed segment `segment_no`.
const fn segment_id(struct_id: u32, tenant: u64, segment_no: u64) -> Id {
    Id::new(tenant, LIVE_TRACKER_SHARD, struct_id, segment_no)
}

impl DataFile {
    // ── Head plumbing ─────────────────────────────────────────────────────

    fn live_head(
        &self,
        struct_id: u32,
        tenant: u64,
    ) -> StorageResult<LiveHead> {
        let Some(slot) = self.read_nonunique_tracker(struct_id, tenant)? else {
            return Ok(LiveHead::default());
        };
        let Some(bytes) = slot.inline_bytes() else {
            return Ok(LiveHead::default());
        };
        wavedb_core::wire::from_wire(bytes)
            .map_err(|e| StorageError::Other(format!("live head decode: {e}")))
    }

    fn write_live_head(
        &self,
        struct_id: u32,
        tenant: u64,
        head: &LiveHead,
    ) -> StorageResult<()> {
        let bytes = wavedb_core::wire::to_wire(head).map_err(|e| {
            StorageError::Other(format!("live head encode: {e}"))
        })?;
        let newest = head.open.last().copied().unwrap_or(0);
        #[allow(clippy::cast_possible_truncation)]
        let slot = AnchorSlot::inline(&bytes, newest as u64 & 0xFFFF_FFFF_FFFF);
        self.write_nonunique_tracker(struct_id, tenant, &slot)
    }

    fn read_segment(
        &self,
        struct_id: u32,
        tenant: u64,
        segment_no: u64,
    ) -> StorageResult<Vec<u128>> {
        let Some(rec) =
            self.read_versioned(segment_id(struct_id, tenant, segment_no))?
        else {
            return Ok(Vec::new());
        };
        wavedb_core::wire::from_wire(&rec.data).map_err(|e| {
            StorageError::Other(format!("live segment decode: {e}"))
        })
    }

    fn write_segment(
        &self,
        struct_id: u32,
        tenant: u64,
        segment_no: u64,
        ids: &[u128],
    ) -> StorageResult<()> {
        let data = wavedb_core::wire::to_wire(&ids.to_vec()).map_err(|e| {
            StorageError::Other(format!("live segment encode: {e}"))
        })?;
        let rec = VersionedRecord::new(
            segment_id(struct_id, tenant, segment_no).raw(),
            data,
        );
        self.write_versioned(&rec)
    }

    // ── Public tracker API ────────────────────────────────────────────────

    /// Append `id` to the live list, sealing the open segment when full.
    pub fn live_append(
        &self,
        struct_id: u32,
        tenant: u64,
        id: u128,
    ) -> StorageResult<()> {
        let mut head = self.live_head(struct_id, tenant)?;
        head.open.push(id);
        if head.open.len() >= LIVE_SEGMENT_CAP {
            self.write_segment(struct_id, tenant, head.sealed, &head.open)?;
            head.sealed += 1;
            head.open.clear();
        }
        self.write_live_head(struct_id, tenant, &head)
    }

    /// Replace the entire live list with a single `id`.
    ///
    /// The Unique-shape write path: each save rotates the one live record;
    /// prior versions stay stored but leave the live set.
    pub fn live_set_single(
        &self,
        struct_id: u32,
        tenant: u64,
        id: u128,
    ) -> StorageResult<()> {
        // Sealed segments from a previous shape mix-up (or recovery noise)
        // are orphaned by resetting `sealed` — they were never part of a
        // Unique struct's contract.
        let head = LiveHead {
            sealed: 0,
            open: vec![id],
        };
        self.write_live_head(struct_id, tenant, &head)
    }

    /// Remove `id` from the live list.  Returns `true` when it was present.
    ///
    /// The record itself is untouched — delete only makes it
    /// undiscoverable (versioned history is append-only by design).
    pub fn live_remove(
        &self,
        struct_id: u32,
        tenant: u64,
        id: u128,
    ) -> StorageResult<bool> {
        let mut head = self.live_head(struct_id, tenant)?;
        if let Some(pos) = head.open.iter().position(|&x| x == id) {
            head.open.remove(pos);
            self.write_live_head(struct_id, tenant, &head)?;
            return Ok(true);
        }
        // Newest sealed segment first — recent records get deleted most.
        for seg in (0..head.sealed).rev() {
            let mut ids = self.read_segment(struct_id, tenant, seg)?;
            if let Some(pos) = ids.iter().position(|&x| x == id) {
                ids.remove(pos);
                self.write_segment(struct_id, tenant, seg, &ids)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The full live list, oldest first.
    pub fn live_ids(
        &self,
        struct_id: u32,
        tenant: u64,
    ) -> StorageResult<Vec<u128>> {
        let head = self.live_head(struct_id, tenant)?;
        let mut out = Vec::with_capacity(
            usize::try_from(head.sealed).unwrap_or(0) * LIVE_SEGMENT_CAP
                + head.open.len(),
        );
        for seg in 0..head.sealed {
            out.extend(self.read_segment(struct_id, tenant, seg)?);
        }
        out.extend(&head.open);
        Ok(out)
    }

    /// Rebuild the tracker as the union of its current contents and
    /// `replayed`, deduplicated and ordered by `created_at` (Id low bits).
    ///
    /// Crash-recovery entry point: replayed journal records may or may not
    /// have made it into the tracker before the crash; the union converges
    /// either way.
    pub fn live_rebuild(
        &self,
        struct_id: u32,
        tenant: u64,
        replayed: &[u128],
    ) -> StorageResult<()> {
        let mut ids = self.live_ids(struct_id, tenant)?;
        ids.extend_from_slice(replayed);
        ids.sort_unstable_by_key(|&id| {
            let parsed = Id::from_raw(id);
            (parsed.created_at(), id)
        });
        ids.dedup();

        let mut head = LiveHead::default();
        let mut chunks = ids.chunks_exact(LIVE_SEGMENT_CAP);
        for chunk in chunks.by_ref() {
            self.write_segment(struct_id, tenant, head.sealed, chunk)?;
            head.sealed += 1;
        }
        head.open = chunks.remainder().to_vec();
        self.write_live_head(struct_id, tenant, &head)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::file::data::DEFAULT_PAGE_SIZE;

    fn data_file() -> DataFile {
        DataFile::open_in_memory(DEFAULT_PAGE_SIZE).unwrap()
    }

    fn rid(tenant: u64, struct_id: u32, seq: u64) -> u128 {
        Id::new(tenant, 0, struct_id, seq).raw()
    }

    #[test]
    fn append_and_read_back_in_order() {
        let df = data_file();
        for seq in 1..=5 {
            df.live_append(7, 42, rid(42, 7, seq)).unwrap();
        }
        let ids = df.live_ids(7, 42).unwrap();
        assert_eq!(ids.len(), 5);
        assert_eq!(ids[0], rid(42, 7, 1));
        assert_eq!(ids[4], rid(42, 7, 5));
    }

    #[test]
    fn empty_tracker_reads_empty() {
        let df = data_file();
        assert!(df.live_ids(9, 1).unwrap().is_empty());
    }

    #[test]
    fn trackers_are_isolated_per_struct_and_tenant() {
        let df = data_file();
        df.live_append(7, 42, rid(42, 7, 1)).unwrap();
        df.live_append(8, 42, rid(42, 8, 2)).unwrap();
        df.live_append(7, 43, rid(43, 7, 3)).unwrap();

        assert_eq!(df.live_ids(7, 42).unwrap(), vec![rid(42, 7, 1)]);
        assert_eq!(df.live_ids(8, 42).unwrap(), vec![rid(42, 8, 2)]);
        assert_eq!(df.live_ids(7, 43).unwrap(), vec![rid(43, 7, 3)]);
    }

    #[test]
    fn seals_segments_past_cap_and_keeps_order() {
        let df = data_file();
        let n = LIVE_SEGMENT_CAP as u64 * 2 + 17;
        for seq in 0..n {
            df.live_append(7, 42, rid(42, 7, seq)).unwrap();
        }
        let ids = df.live_ids(7, 42).unwrap();
        assert_eq!(ids.len(), n as usize);
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(id, rid(42, 7, i as u64), "order broken at {i}");
        }
    }

    #[test]
    fn remove_from_open_and_sealed_segments() {
        let df = data_file();
        let n = LIVE_SEGMENT_CAP as u64 + 10;
        for seq in 0..n {
            df.live_append(7, 42, rid(42, 7, seq)).unwrap();
        }
        // From the open segment (a recent id)…
        assert!(df.live_remove(7, 42, rid(42, 7, n - 1)).unwrap());
        // …and from the sealed segment (an old id).
        assert!(df.live_remove(7, 42, rid(42, 7, 3)).unwrap());
        // Unknown id → false.
        assert!(!df.live_remove(7, 42, rid(42, 7, 999_999)).unwrap());

        let ids = df.live_ids(7, 42).unwrap();
        assert_eq!(ids.len(), n as usize - 2);
        assert!(!ids.contains(&rid(42, 7, 3)));
        assert!(!ids.contains(&rid(42, 7, n - 1)));
    }

    #[test]
    fn set_single_replaces_list() {
        let df = data_file();
        for seq in 0..5 {
            df.live_append(7, 42, rid(42, 7, seq)).unwrap();
        }
        df.live_set_single(7, 42, rid(42, 7, 100)).unwrap();
        assert_eq!(df.live_ids(7, 42).unwrap(), vec![rid(42, 7, 100)]);
    }

    #[test]
    fn rebuild_unions_and_dedupes() {
        let df = data_file();
        // Tracker knows 1, 2; journal replays 2, 3 (2 = the crash overlap).
        df.live_append(7, 42, rid(42, 7, 1)).unwrap();
        df.live_append(7, 42, rid(42, 7, 2)).unwrap();
        df.live_rebuild(7, 42, &[rid(42, 7, 2), rid(42, 7, 3)])
            .unwrap();

        let ids = df.live_ids(7, 42).unwrap();
        assert_eq!(
            ids,
            vec![rid(42, 7, 1), rid(42, 7, 2), rid(42, 7, 3)],
            "union must dedupe and stay created_at-ordered"
        );
    }

    #[test]
    fn rebuild_spanning_segments() {
        let df = data_file();
        let n = LIVE_SEGMENT_CAP as u64 + 50;
        let all: Vec<u128> = (0..n).map(|s| rid(42, 7, s)).collect();
        df.live_rebuild(7, 42, &all).unwrap();
        assert_eq!(df.live_ids(7, 42).unwrap(), all);
    }
}
