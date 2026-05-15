//! Type 1 migration: field-level transform between two Message versions.
//!
//! `Message41` → `Message42` adds an `edited: bool` field.
//!
//! # Symmetric attribute design
//!
//! Each struct declares its **immediate neighbours**:
//!
//! - `migrate_from = OldType` — older predecessor (backward link).
//! - `migrate_rollback = NewType` — newer successor (forward link;
//!   "I receive a rollback from `NewType`").
//!
//! Bounds:
//!
//! - First version (oldest): no `migrate_from`; declares `migrate_rollback` if a
//!   newer version exists.
//! - Last version (current): no `migrate_rollback`; declares `migrate_from` to
//!   its predecessor.
//!
//! Function signatures:
//!
//! - `migrate_from_with`:     `async fn<Db>(&Db, OldType) -> Result<Self>`
//! - `migrate_rollback_with`: `async fn<Db>(&Db, NewType) -> Result<Self>`
//! - `first_try`:             `async fn<Db>(&Db) -> Result<Option<OldType>>`
//! - `fallback_not_found`:    `async fn<Db>(&Db) -> Result<Option<Self>>`
//!
//! Run with:
//!   cargo run --bin migration_type_1

use wavedb::prelude::*;
use wavedb_core::migration::{MigrationRegistry, VersionRef};
use wavedb_net::MockTransport;
use wavedb_net::mock::ScriptedReply;

// ── Migration fns (async, generic over Db) ────────────────────────────────────

async fn migrate_v41_v42<Db>(_db: &Db, old: Message41) -> wavedb_core::Result<Message42> {
    Ok(Message42 {
        id: old.id,
        metadata: old.metadata,
        body: old.body,
        author: old.author,
        edited: false,
    })
}

// Rollback declared on the OLDER struct (Message41): takes Future, returns Self.
async fn rollback_v42_to_v41<Db>(_db: &Db, future: Message42) -> wavedb_core::Result<Message41> {
    Ok(Message41 {
        id: future.id,
        metadata: future.metadata,
        body: future.body,
        author: future.author,
    })
}

// first_try on the NEWER struct: synthesise a Message41 from other sources if
// needed (type-2 replacement).  Return None for the normal DB path.
async fn v42_first_try<Db>(_db: &Db) -> wavedb_core::Result<Option<Message41>> {
    Ok(None)
}

// ── Schema — v41 (first version: no migrate_from, declares its future) ───────

#[wave_db(
    struct_id = 41,
    migrate_rollback      = Message42,
    migrate_rollback_with = rollback_v42_to_v41,
)]
#[derive(PartialEq, Eq)]
pub struct Message41 {
    pub id: Id,
    pub metadata: Metadata,
    pub body: String,
    pub author: u64,
}

// ── Schema — v42 (last version: declares its predecessor, no rollback yet) ───

#[wave_db(
    struct_id = 41,
    migrate_from      = Message41,
    migrate_from_with = migrate_v41_v42,
    first_try         = v42_first_try,
)]
#[derive(PartialEq, Eq)]
pub struct Message42 {
    pub id: Id,
    pub metadata: Metadata,
    pub body: String,
    pub author: u64,
    pub edited: bool,
}

pub type Message = Message42;

// `UniqueObject` is auto-impl'd by the macro for both versions — cross-version
// reads work through the macro-generated `MigrationChain` impl with no manual
// `register_migration` call required.

/// Build the wire-format payload `[STRUCT_VERSION, ...postcard_body...]` that
/// the engine emits when a record is read back.
fn encode_versioned<T>(record: &T) -> Vec<u8>
where
    T: serde::Serialize + wavedb_core::WaveDbStruct,
{
    let body = postcard::to_allocvec(record).expect("encode");
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(T::STRUCT_VERSION);
    out.extend_from_slice(&body);
    out
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    const SID: u32 = 41;
    let v41 = VersionRef::new(SID, 41);
    let v42 = VersionRef::new(SID, 42);

    // ── Compile-time chain via both traits ────────────────────────────────────
    fn assert_backward<T: wavedb_core::MigratesFrom<Source = Message41>>() {}
    assert_backward::<Message42>();
    println!("MigratesFrom: Message42::Source = Message41 ✓");

    fn assert_forward<T: wavedb_core::RollbackFrom<Future = Message42>>() {}
    assert_forward::<Message41>();
    println!("RollbackFrom:  Message41::Future = Message42 ✓");

    // ── Each struct contributes its own edge(s) to a registry — but this is
    //    OPTIONAL.  Cross-version search/update works without any registry
    //    call thanks to the macro-generated `MigrationChain` impl.
    let mut registry = MigrationRegistry::new();
    Message41::register_migration(&mut registry); // backward v42 → v41
    Message42::register_migration(&mut registry); // forward  v41 → v42
    assert_eq!(registry.resolve(v41, v42)?.len(), 1);
    assert!(registry.can_rollback(v42, v41));
    println!("Registry edges wired (optional for graph queries).");

    // ── Tasty cross-version API: write v41, read as v42 (and vice-versa) ─────
    //
    // The mock returns the stored bytes verbatim — we encode them once with
    // the wire-format version prefix, then issue search() calls for *both*
    // versions of the same struct family.  The macro-generated MigrationChain
    // walks the chain in either direction without any extra registration.

    let stored_v41 = Message41 {
        id: Id::default(),
        metadata: Metadata::default(),
        body: "Hello, WaveDB!".into(),
        author: 99,
    };
    let stored_v42 = Message42 {
        id: Id::default(),
        metadata: Metadata::default(),
        body: "Hello, v42!".into(),
        author: 7,
        edited: true,
    };

    let mock = MockTransport::new();
    mock.push(ScriptedReply::connect(
        "ws://owner:7700",
        "ws://backup:7700",
    ));
    // 1. Write a v41 record.
    mock.push(ScriptedReply::ok(Vec::new()));
    // 2. search::<Message42>() with stored v41 bytes → engine migrates forward.
    mock.push(ScriptedReply::ok(encode_versioned(&stored_v41)));
    // 3. search::<Message41>() with stored v41 bytes → identity read.
    mock.push(ScriptedReply::ok(encode_versioned(&stored_v41)));
    // 4. Write a v42 record.
    mock.push(ScriptedReply::ok(Vec::new()));
    // 5. search::<Message41>() with stored v42 bytes → engine rolls back.
    mock.push(ScriptedReply::ok(encode_versioned(&stored_v42)));
    // 6. search::<Message42>() with stored v42 bytes → identity read.
    mock.push(ScriptedReply::ok(encode_versioned(&stored_v42)));

    let db = Db::open_with_transport(mock, 1, 100).await?;

    // Write the old version.
    stored_v41.clone().update(&db).await?;
    println!("\nWrote a Message41 record.");

    // Read it back as the NEW version — engine runs forward migration.
    let as_v42 = Message42::search(&db).await?.expect("record");
    assert_eq!(as_v42.body, "Hello, WaveDB!");
    assert!(!as_v42.edited, "migrate_from_with sets edited=false");
    println!("  → Message42::search returned: {as_v42:?}");

    // Read it back as the OLD version — identity, no migration.
    let as_v41 = Message41::search(&db).await?.expect("record");
    assert_eq!(as_v41.body, "Hello, WaveDB!");
    println!("  → Message41::search returned: {as_v41:?}");

    // Write the new version.
    stored_v42.clone().update(&db).await?;
    println!("\nWrote a Message42 record.");

    // Read it back as the OLD version — engine runs rollback.
    let as_v41 = Message41::search(&db).await?.expect("record");
    assert_eq!(as_v41.body, "Hello, v42!");
    println!("  → Message41::search returned: {as_v41:?}");

    // Read it back as the NEW version — identity, no migration.
    let as_v42 = Message42::search(&db).await?.expect("record");
    assert!(as_v42.edited);
    println!("  → Message42::search returned: {as_v42:?}");

    println!("\nmigration_type_1 example OK");
    Ok(())
}
