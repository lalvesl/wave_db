//! In-memory WaveDB store — two-slot versioned storage with modification chain.
//!
//! Every live record occupies two storage slots:
//!
//! | Slot           | Key                                   | Contents                             |
//! |----------------|---------------------------------------|--------------------------------------|
//! | **Anchor**     | `(STRUCT_ID, TENANT_ID)` — no ts      | Live data (inline) or pointer        |
//! | **Versioned**  | Full 128-bit `Id`                     | Immutable historical record          |
//!
//! The anchor is the stable address. All cross-references (indexes, M2M links,
//! sync handles) point to the anchor — so mutations never require rewriting
//! those pointers. The versioned slot is immutable after creation (except for
//! the `new_modification_id` field written once by the next mutation).
//!
//! # Mutation flow (per README §Anchor Slots)
//!
//! 1. New versioned record is written with `old_mod_id → old_id`.
//! 2. Old record's `new_modification_id` is updated to point forward.
//! 3. Anchor slot is overwritten with the new data and new `current_version_id`.

use crate::{
    anchor::AnchorSlot,
    config::Config,
    id::{Id, StructId, TenantId},
    journal::{Journal, JournalEntry},
    metadata::Metadata,
    page::PageKey,
};
use std::collections::HashMap;

// ─── StoredRecord ─────────────────────────────────────────────────────────────

/// An in-memory versioned record as held by the [`Store`].
///
/// `data` holds the serialised payload (all user-defined fields). The `id` and
/// `metadata` are stored explicitly alongside so the engine can manage the
/// modification chain without parsing `data`.
#[derive(Debug, Clone)]
pub struct StoredRecord {
    /// Full composite ID of this specific version.
    pub id: Id,
    /// Modification-chain pointers and schema version.
    pub metadata: Metadata,
    /// Serialised payload bytes (caller-controlled format).
    pub data: Vec<u8>,
}

impl StoredRecord {
    /// Construct a first-version record (no predecessor).
    pub fn new_first(id: Id, struct_version: u16, creator_id: u64, data: Vec<u8>) -> Self {
        Self { id, metadata: Metadata::new(struct_version, creator_id), data }
    }
}

// ─── StoreError ───────────────────────────────────────────────────────────────

/// Errors returned by [`Store`] operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// No anchor exists for the given `(STRUCT_ID, TENANT_ID)`.
    NotFound,
    /// A live anchor already exists — use [`Store::mutate`] to update it.
    AlreadyExists,
    /// `old_id` is not the current live version at the anchor.
    StaleVersion,
    /// The anchor slot is a tombstone (record was deleted).
    Deleted,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "record not found"),
            Self::AlreadyExists => write!(f, "live anchor already exists; use mutate"),
            Self::StaleVersion => write!(f, "old_id is not the current live version"),
            Self::Deleted => write!(f, "anchor slot is a tombstone"),
        }
    }
}

impl std::error::Error for StoreError {}

// ─── AnchorMode ───────────────────────────────────────────────────────────────

/// Anchor operating mode, chosen per deployment profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnchorMode {
    /// Full live record bytes stored in the anchor (Quick-Node default).
    ///
    /// Costs ~2× storage for live data; saves one IO per read.
    #[default]
    Inline,

    /// Only a pointer to the versioned record is stored.
    ///
    /// Storage-optimal; costs one extra IO per read.
    PointerOnly,
}

// ─── Store ────────────────────────────────────────────────────────────────────

/// The in-memory WaveDB store.
///
/// Manages the two-slot system: anchor slots and versioned records. The journal
/// inside the store records every mutation so the sequence can be replayed.
pub struct Store {
    config: Config,
    anchor_mode: AnchorMode,
    /// Anchor slots keyed by `(STRUCT_ID, TENANT_ID)`.
    anchors: HashMap<PageKey, AnchorSlot>,
    /// Versioned records keyed by full 128-bit `Id`.
    versioned: HashMap<Id, StoredRecord>,
    /// Append-only journal for durability.
    journal: Journal,
}

impl Store {
    /// Create a new store with the given configuration and anchor mode.
    pub fn new(config: Config, anchor_mode: AnchorMode) -> Self {
        Self {
            config,
            anchor_mode,
            anchors: HashMap::new(),
            versioned: HashMap::new(),
            journal: Journal::new(),
        }
    }

    /// Create a store with default config in inline mode (Quick-Node profile).
    pub fn default_inline() -> Self {
        Self::new(Config::default(), AnchorMode::Inline)
    }

    /// Create a store with development config in inline mode.
    pub fn dev() -> Self {
        Self::new(Config::dev(), AnchorMode::Inline)
    }

    // ── Write operations ─────────────────────────────────────────────────────

    /// Insert a **new** record — first version, no predecessor.
    ///
    /// Fails with [`StoreError::AlreadyExists`] if a live anchor already exists
    /// for this `(STRUCT_ID, TENANT_ID)`. Use [`Store::mutate`] to update.
    ///
    /// Automatically re-inserts into a tombstoned slot (the record was deleted
    /// and is now being re-created as a new first version).
    pub fn insert(&mut self, record: StoredRecord) -> Result<(), StoreError> {
        let key = PageKey::new(record.id.struct_id(), record.id.tenant_id());

        // Block if a *live* anchor exists.
        if let Some(slot) = self.anchors.get(&key) {
            if slot.is_live() {
                return Err(StoreError::AlreadyExists);
            }
        }

        let anchor = self.build_anchor(record.data.clone(), record.id);
        let record_id = record.id;

        self.versioned.insert(record.id, record);
        self.anchors.insert(key, anchor);

        self.journal.append(JournalEntry::Insert { record_id, anchor_key: key });
        Ok(())
    }

    /// Mutate an existing record.
    ///
    /// # Mutation steps
    /// 1. Validate that `old_id` is the current live version.
    /// 2. Update the old versioned record's `new_modification_id`.
    /// 3. Write the new versioned record with `old_modification_id = old_id`.
    /// 4. Overwrite the anchor slot, preserving `inbound_refs`.
    ///
    /// The `new_record.metadata.old_modification_id` is set by this method —
    /// callers should pass metadata with `old_modification_id = Id::ZERO`.
    pub fn mutate(&mut self, old_id: Id, mut new_record: StoredRecord) -> Result<(), StoreError> {
        let key = PageKey::new(old_id.struct_id(), old_id.tenant_id());

        // Step 1: verify the anchor exists and old_id is live.
        let current_version_id = {
            let anchor = self.anchors.get(&key).ok_or(StoreError::NotFound)?;
            if !anchor.is_live() {
                return Err(StoreError::Deleted);
            }
            anchor.current_version_id().expect("live anchor always has current_version_id")
        };

        if current_version_id != old_id {
            return Err(StoreError::StaleVersion);
        }

        let new_id = new_record.id;

        // Step 2: update old record's new_modification_id.
        if let Some(old_rec) = self.versioned.get_mut(&old_id) {
            old_rec.metadata.new_modification_id = new_id;
        }

        // Step 3: set the backward chain pointer on the new record.
        new_record.metadata.old_modification_id = old_id;

        // Step 4: build new anchor, preserving inbound_refs.
        let inbound_refs = self.anchors
            .get(&key)
            .map(|a| a.inbound_refs.clone())
            .unwrap_or_default();

        let mut new_anchor = self.build_anchor(new_record.data.clone(), new_id);
        new_anchor.inbound_refs = inbound_refs;

        self.versioned.insert(new_id, new_record);
        self.anchors.insert(key, new_anchor);

        self.journal.append(JournalEntry::Mutate { old_id, new_id, anchor_key: key });
        Ok(())
    }

    /// Delete a record: replace its anchor with a tombstone.
    ///
    /// The versioned history chain is preserved and remains traversable.
    /// Calling [`Store::insert`] afterward re-creates the record as a new
    /// independent first version (no history link to the deleted chain).
    pub fn delete(&mut self, struct_id: StructId, tenant_id: TenantId) -> Result<(), StoreError> {
        let key = PageKey::new(struct_id, tenant_id);
        let anchor = self.anchors.get_mut(&key).ok_or(StoreError::NotFound)?;

        if !anchor.is_live() {
            return Err(StoreError::Deleted);
        }

        let final_id = anchor
            .current_version_id()
            .expect("live anchor always has current_version_id");
        anchor.tombstone(final_id);

        self.journal
            .append(JournalEntry::Delete { anchor_key: key, final_version_id: final_id });
        Ok(())
    }

    // ── Read operations ──────────────────────────────────────────────────────

    /// Look up the live record for a given `(STRUCT_ID, TENANT_ID)`.
    ///
    /// Returns `None` for tombstoned or non-existent anchors.
    ///
    /// - **Inline mode**: returns the versioned record directly (O(1)).
    /// - **Pointer-only mode**: follows the pointer to the versioned record (O(1)).
    pub fn get_live(&self, struct_id: StructId, tenant_id: TenantId) -> Option<&StoredRecord> {
        let anchor = self.anchors.get(&PageKey::new(struct_id, tenant_id))?;
        let live_id = anchor.current_version_id()?;
        self.versioned.get(&live_id)
    }

    /// Return the inline data bytes for a live record without a full record
    /// lookup — only valid in [`AnchorMode::Inline`].
    ///
    /// This is the hot-path read for Quick-Nodes: the anchor already holds the
    /// full serialised bytes, so no `versioned` lookup is needed.
    pub fn get_live_inline(&self, struct_id: StructId, tenant_id: TenantId) -> Option<&[u8]> {
        let anchor = self.anchors.get(&PageKey::new(struct_id, tenant_id))?;
        if !anchor.is_live() {
            return None;
        }
        anchor.inline_data()
    }

    /// Look up a specific versioned record by its full 128-bit `Id`.
    ///
    /// Works for any historical version, not just the live one.
    pub fn get_versioned(&self, id: Id) -> Option<&StoredRecord> {
        self.versioned.get(&id)
    }

    /// Return a reference to the anchor slot for a given key.
    pub fn anchor(&self, struct_id: StructId, tenant_id: TenantId) -> Option<&AnchorSlot> {
        self.anchors.get(&PageKey::new(struct_id, tenant_id))
    }

    // ── History traversal ────────────────────────────────────────────────────

    /// Collect the full version history from **newest → oldest**.
    ///
    /// Starts from the live version (or the last version before deletion if
    /// the anchor is a tombstone) and follows `old_modification_id` links.
    pub fn history_backward(
        &self,
        struct_id: StructId,
        tenant_id: TenantId,
    ) -> Vec<&StoredRecord> {
        let key = PageKey::new(struct_id, tenant_id);
        let start_id = match self.anchors.get(&key).and_then(|a| a.most_recent_version_id()) {
            Some(id) => id,
            None => return vec![],
        };

        let mut chain = Vec::new();
        let mut current_id = Some(start_id);

        while let Some(id) = current_id {
            match self.versioned.get(&id) {
                Some(rec) => {
                    current_id = if rec.metadata.old_modification_id.is_zero() {
                        None
                    } else {
                        Some(rec.metadata.old_modification_id)
                    };
                    chain.push(rec);
                }
                None => break,
            }
        }

        chain
    }

    /// Collect the full version history from **oldest → newest**.
    ///
    /// Reverses the result of [`Store::history_backward`].
    pub fn history_forward(
        &self,
        struct_id: StructId,
        tenant_id: TenantId,
    ) -> Vec<&StoredRecord> {
        let mut chain = self.history_backward(struct_id, tenant_id);
        chain.reverse();
        chain
    }

    // ── Cross-reference management ───────────────────────────────────────────

    /// Register an inbound reference on the anchor for `(struct_id, tenant_id)`.
    ///
    /// Called when an index entry or M2M link is created that targets this anchor.
    pub fn add_inbound_ref(
        &mut self,
        struct_id: StructId,
        tenant_id: TenantId,
        ref_id: Id,
    ) -> Result<(), StoreError> {
        let anchor = self
            .anchors
            .get_mut(&PageKey::new(struct_id, tenant_id))
            .ok_or(StoreError::NotFound)?;
        anchor.add_inbound_ref(ref_id);
        Ok(())
    }

    /// Remove an inbound reference from the anchor.
    pub fn remove_inbound_ref(
        &mut self,
        struct_id: StructId,
        tenant_id: TenantId,
        ref_id: Id,
    ) {
        if let Some(anchor) = self.anchors.get_mut(&PageKey::new(struct_id, tenant_id)) {
            anchor.remove_inbound_ref(ref_id);
        }
    }

    // ── Stats & diagnostics ──────────────────────────────────────────────────

    /// Number of live anchor slots (excludes tombstones).
    pub fn live_count(&self) -> usize {
        self.anchors.values().filter(|a| a.is_live()).count()
    }

    /// Total number of anchor slots (live + tombstones).
    pub fn anchor_count(&self) -> usize {
        self.anchors.len()
    }

    /// Total number of versioned records (includes all historical versions).
    pub fn versioned_count(&self) -> usize {
        self.versioned.len()
    }

    /// Reference to the journal.
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Mutable reference to the journal (for draining during compaction).
    pub fn journal_mut(&mut self) -> &mut Journal {
        &mut self.journal
    }

    /// Runtime configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    // ── Rebuild-only loaders (for DiskStore) ─────────────────────────────────

    /// Directly insert a versioned record into the cache, bypassing the journal.
    ///
    /// **For `DiskStore` rebuild only** — called during `open` when reconstructing
    /// in-memory state from the data file. Must not be used in normal write paths.
    pub fn load_versioned(&mut self, rec: StoredRecord) {
        self.versioned.insert(rec.id, rec);
    }

    /// Directly insert an anchor slot into the cache, bypassing the journal.
    ///
    /// **For `DiskStore` rebuild only**.
    pub fn load_anchor(&mut self, key: PageKey, slot: AnchorSlot) {
        self.anchors.insert(key, slot);
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    fn build_anchor(&self, data: Vec<u8>, version_id: Id) -> AnchorSlot {
        match self.anchor_mode {
            AnchorMode::Inline => AnchorSlot::new_inline(data, version_id),
            AnchorMode::PointerOnly => AnchorSlot::new_pointer(version_id),
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{ShardId, Slider, StructId, TenantId, Timestamp};

    fn make_id(struct_id: u16, tenant: u64, ts: u64) -> Id {
        Id::new(
            TenantId::new(tenant),
            ShardId::new(0),
            StructId::new(struct_id),
            Timestamp::from_ticks(ts),
            Slider::new(0),
        )
    }

    fn record(struct_id: u16, tenant: u64, ts: u64, payload: &[u8]) -> StoredRecord {
        StoredRecord::new_first(make_id(struct_id, tenant, ts), 1, 42, payload.to_vec())
    }

    // ── insert ────────────────────────────────────────────────────────────────

    #[test]
    fn insert_and_get_live() {
        let mut store = Store::dev();
        let rec = record(2, 1, 100, b"hello");
        store.insert(rec.clone()).unwrap();

        let got = store.get_live(StructId::new(2), TenantId::new(1)).unwrap();
        assert_eq!(got.id, rec.id);
        assert_eq!(got.data, b"hello");
    }

    #[test]
    fn insert_duplicate_fails() {
        let mut store = Store::dev();
        store.insert(record(2, 1, 100, b"first")).unwrap();
        let err = store.insert(record(2, 1, 200, b"second")).unwrap_err();
        assert_eq!(err, StoreError::AlreadyExists);
    }

    #[test]
    fn insert_records_journal_entry() {
        let mut store = Store::dev();
        store.insert(record(2, 1, 100, b"data")).unwrap();
        assert_eq!(store.journal().stats().inserts, 1);
    }

    // ── mutate ────────────────────────────────────────────────────────────────

    #[test]
    fn mutate_updates_live_and_history() {
        let mut store = Store::dev();
        let v1 = record(2, 1, 100, b"v1");
        let v1_id = v1.id;
        store.insert(v1).unwrap();

        let v2 = record(2, 1, 200, b"v2");
        let v2_id = v2.id;
        store.mutate(v1_id, v2).unwrap();

        // Live record is v2.
        let live = store.get_live(StructId::new(2), TenantId::new(1)).unwrap();
        assert_eq!(live.id, v2_id);
        assert_eq!(live.data, b"v2");

        // v2 has old_modification_id pointing to v1.
        assert_eq!(live.metadata.old_modification_id, v1_id);

        // v1 has new_modification_id pointing to v2.
        let v1_rec = store.get_versioned(v1_id).unwrap();
        assert_eq!(v1_rec.metadata.new_modification_id, v2_id);
    }

    #[test]
    fn mutate_stale_old_id_fails() {
        let mut store = Store::dev();
        let v1 = record(2, 1, 100, b"v1");
        let v1_id = v1.id;
        store.insert(v1).unwrap();

        // Advance to v2.
        store.mutate(v1_id, record(2, 1, 200, b"v2")).unwrap();

        // Trying to mutate from v1 again should fail.
        let err = store.mutate(v1_id, record(2, 1, 300, b"v3")).unwrap_err();
        assert_eq!(err, StoreError::StaleVersion);
    }

    #[test]
    fn mutate_nonexistent_fails() {
        let mut store = Store::dev();
        let fake_id = make_id(2, 1, 999);
        let err = store.mutate(fake_id, record(2, 1, 1000, b"data")).unwrap_err();
        assert_eq!(err, StoreError::NotFound);
    }

    #[test]
    fn mutate_records_journal_entry() {
        let mut store = Store::dev();
        let v1 = record(2, 1, 100, b"v1");
        let v1_id = v1.id;
        store.insert(v1).unwrap();
        store.mutate(v1_id, record(2, 1, 200, b"v2")).unwrap();

        let stats = store.journal().stats();
        assert_eq!(stats.inserts, 1);
        assert_eq!(stats.mutations, 1);
    }

    // ── delete ────────────────────────────────────────────────────────────────

    #[test]
    fn delete_tombstones_anchor() {
        let mut store = Store::dev();
        store.insert(record(2, 1, 100, b"data")).unwrap();

        store.delete(StructId::new(2), TenantId::new(1)).unwrap();

        // Live lookup returns None.
        assert!(store.get_live(StructId::new(2), TenantId::new(1)).is_none());

        // Anchor is tombstoned (not gone).
        let anchor = store.anchor(StructId::new(2), TenantId::new(1)).unwrap();
        assert!(!anchor.is_live());
    }

    #[test]
    fn delete_twice_fails() {
        let mut store = Store::dev();
        store.insert(record(2, 1, 100, b"data")).unwrap();
        store.delete(StructId::new(2), TenantId::new(1)).unwrap();
        let err = store.delete(StructId::new(2), TenantId::new(1)).unwrap_err();
        assert_eq!(err, StoreError::Deleted);
    }

    #[test]
    fn insert_after_delete_succeeds() {
        let mut store = Store::dev();
        store.insert(record(2, 1, 100, b"first")).unwrap();
        store.delete(StructId::new(2), TenantId::new(1)).unwrap();
        // Re-insert after deletion should succeed (tombstoned slot).
        store.insert(record(2, 1, 200, b"reborn")).unwrap();
        let live = store.get_live(StructId::new(2), TenantId::new(1)).unwrap();
        assert_eq!(live.data, b"reborn");
    }

    // ── history ───────────────────────────────────────────────────────────────

    #[test]
    fn history_backward_three_versions() {
        let mut store = Store::dev();
        let v1 = record(2, 1, 100, b"v1");
        let v1_id = v1.id;
        store.insert(v1).unwrap();

        let v2 = record(2, 1, 200, b"v2");
        let v2_id = v2.id;
        store.mutate(v1_id, v2).unwrap();

        let v3 = record(2, 1, 300, b"v3");
        store.mutate(v2_id, v3).unwrap();

        let chain = store.history_backward(StructId::new(2), TenantId::new(1));
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].data, b"v3"); // newest first
        assert_eq!(chain[1].data, b"v2");
        assert_eq!(chain[2].data, b"v1"); // oldest last
    }

    #[test]
    fn history_forward_is_oldest_first() {
        let mut store = Store::dev();
        let v1 = record(2, 1, 100, b"v1");
        let v1_id = v1.id;
        store.insert(v1).unwrap();
        let v2 = record(2, 1, 200, b"v2");
        store.mutate(v1_id, v2).unwrap();

        let chain = store.history_forward(StructId::new(2), TenantId::new(1));
        assert_eq!(chain[0].data, b"v1");
        assert_eq!(chain[1].data, b"v2");
    }

    #[test]
    fn history_after_delete_still_works() {
        let mut store = Store::dev();
        let v1 = record(2, 1, 100, b"v1");
        let v1_id = v1.id;
        store.insert(v1).unwrap();
        store.mutate(v1_id, record(2, 1, 200, b"v2")).unwrap();
        store.delete(StructId::new(2), TenantId::new(1)).unwrap();

        let chain = store.history_backward(StructId::new(2), TenantId::new(1));
        assert_eq!(chain.len(), 2);
    }

    // ── pointer-only mode ─────────────────────────────────────────────────────

    #[test]
    fn pointer_only_mode_get_live() {
        let mut store = Store::new(Config::dev(), AnchorMode::PointerOnly);
        store.insert(record(2, 1, 100, b"ptr-mode")).unwrap();
        let live = store.get_live(StructId::new(2), TenantId::new(1)).unwrap();
        assert_eq!(live.data, b"ptr-mode");
        // No inline data in anchor.
        assert!(store.get_live_inline(StructId::new(2), TenantId::new(1)).is_none());
    }

    // ── inline fast path ──────────────────────────────────────────────────────

    #[test]
    fn inline_fast_path_returns_bytes() {
        let mut store = Store::dev();
        store.insert(record(2, 1, 100, b"fast")).unwrap();
        let bytes = store.get_live_inline(StructId::new(2), TenantId::new(1)).unwrap();
        assert_eq!(bytes, b"fast");
    }

    // ── stats ─────────────────────────────────────────────────────────────────

    #[test]
    fn live_and_versioned_counts() {
        let mut store = Store::dev();
        let v1 = record(2, 1, 100, b"v1");
        let v1_id = v1.id;
        store.insert(v1).unwrap();
        store.insert(record(3, 1, 100, b"other")).unwrap();
        store.mutate(v1_id, record(2, 1, 200, b"v2")).unwrap();

        assert_eq!(store.live_count(), 2);
        assert_eq!(store.versioned_count(), 3); // v1, v2, other
    }

    // ── inbound refs ─────────────────────────────────────────────────────────

    #[test]
    fn inbound_refs_preserved_through_mutation() {
        let mut store = Store::dev();
        let v1 = record(2, 1, 100, b"v1");
        let v1_id = v1.id;
        store.insert(v1).unwrap();

        let index_ref = make_id(5, 1, 500);
        store.add_inbound_ref(StructId::new(2), TenantId::new(1), index_ref).unwrap();

        store.mutate(v1_id, record(2, 1, 200, b"v2")).unwrap();

        // inbound_refs must survive the anchor overwrite.
        let anchor = store.anchor(StructId::new(2), TenantId::new(1)).unwrap();
        assert!(anchor.inbound_refs.contains(&index_ref));
    }
}
