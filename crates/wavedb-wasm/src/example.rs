//! Example WaveDB schema + flows compiled into the wasm binary.
//!
//! Two jobs:
//!
//! 1. **Keep the engine in the binary.** `nix build .#wasm` measures a
//!    release artifact; without an exported entry point that actually
//!    exercises the engine, LTO strips the entire codebase and the size
//!    number means nothing.  [`example_roundtrip`] walks the macro schema,
//!    the migration chain, the query DSL, Id packing, and IndexedDB
//!    persistence — the real client code paths.
//! 2. **Browser e2e surface.**  `tests/e2e_example.rs` drives the same
//!    entry point under `wasm-bindgen-test` (run with
//!    `wasm-pack test --headless --firefox crates/wavedb-wasm`).

use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wavedb::prelude::*;
use wavedb_core::MigrationChain;

use crate::idb::IdbStore;

// ── Example schema: a Note family with one migration step ───────────────────

#[wave_db(struct_id = 90, NonUnique)]
pub struct Note1 {
    pub title: String,
    pub stars: u64,
}

#[allow(clippy::unused_async, clippy::future_not_send)]
async fn note_v1_to_v2<Db>(_db: &Db, old: Note1) -> wavedb_core::Result<Note2> {
    Ok(Note2 {
        id: old.id,
        metadata: old.metadata,
        title: old.title,
        stars: old.stars,
        pinned: false,
    })
}

#[wave_db(
    struct_id = 90,
    NonUnique,
    migrate_from      = Note1,
    migrate_from_with = note_v1_to_v2,
)]
pub struct Note2 {
    pub title: String,
    pub stars: u64,
    pub pinned: bool,
}
pub type Note = Note2;

/// Transport-free `Db` stand-in for the local migration walk —
/// `MigrationChain` is generic over any `Db: Send + Sync`.
struct NoopDb;

/// What [`example_roundtrip`] verified, returned to JS as a plain object.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExampleReport {
    /// `HEAPABLE_FIELDS` classified `title` as heapable.
    pub heapable_fields: Vec<String>,
    /// v1 bytes were read as v2 through the migration chain.
    pub migrated_from_v1: bool,
    /// The migrated record's `pinned` default.
    pub pinned_after_migration: bool,
    /// Serialized byte length of a typed-column query expression.
    pub expr_bytes: u32,
    /// The record survived an IndexedDB put/get keyed by its 128-bit Id.
    pub idb_roundtrip_ok: bool,
    /// Objects found in the tenant's contiguous key range.
    pub tenant_objects: u32,
}

/// End-to-end client flow against the real engine code paths.
///
/// Returns an [`ExampleReport`] (as a JS object) describing each step.
#[wasm_bindgen]
#[allow(clippy::future_not_send)]
pub async fn example_roundtrip() -> Result<JsValue, JsValue> {
    let err = |e: &dyn std::fmt::Display| JsValue::from_str(&e.to_string());

    // 1. Stackable/heapable descriptor from the macro.
    let heapable_fields: Vec<String> = Note2::HEAPABLE_FIELDS
        .iter()
        .map(ToString::to_string)
        .collect();

    // 2. A stored v1 record: postcard bytes, version byte prepended —
    //    exactly the wire/storage format.
    let id = Id::new(
        /* tenant */ 42,
        /* shard */ 3,
        Note1::STRUCT_ID,
        1000,
    );
    let v1 = Note1 {
        id,
        metadata: Metadata::default(),
        title: "groceries".to_string(),
        stars: 5,
    };
    let v1_bytes = postcard::to_allocvec(&v1).map_err(|e| err(&e))?;

    // 3. Lazy migration on read: ask for Note2, hand it v1 bytes.
    let migrated: Note2 =
        Note2::read_as_self(&NoopDb, &v1_bytes, Note1::STRUCT_VERSION)
            .await
            .map_err(|e| err(&e))?;
    let migrated_from_v1 = migrated.title == "groceries" && migrated.stars == 5;
    let pinned_after_migration = migrated.pinned;

    // 4. Typed-column query expression, serialized like a real request.
    let expr = Expr::and(Note2::stars.gte(3u64), Note2::title.eq("groceries"));
    let expr_bytes = u32::try_from(expr.to_bytes().map_err(|e| err(&e))?.len())
        .unwrap_or(u32::MAX);

    // 5. Persist the migrated record in IndexedDB at its 128-bit Id —
    //    one key per object, no page emulation.
    let store = IdbStore::open_named("wavedb-example", "kv").await?;
    let v2_bytes = postcard::to_allocvec(&migrated).map_err(|e| err(&e))?;
    store.put_object(id.raw(), &v2_bytes).await?;
    let read_back = store.get_object(id.raw()).await?;
    let idb_roundtrip_ok = read_back.as_deref() == Some(&v2_bytes[..]);

    // 6. Tenant clustering: the Id's top bits make tenant 42's records one
    //    contiguous key range.
    let tenant_objects =
        u32::try_from(store.get_tenant_objects(42).await?.len())
            .unwrap_or(u32::MAX);

    let report = ExampleReport {
        heapable_fields,
        migrated_from_v1,
        pinned_after_migration,
        expr_bytes,
        idb_roundtrip_ok,
        tenant_objects,
    };
    serde_wasm_bindgen::to_value(&report).map_err(|e| err(&e))
}
