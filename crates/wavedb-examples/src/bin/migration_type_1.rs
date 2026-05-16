#![allow(clippy::future_not_send)]
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
use wavedb_net::ChannelTransport;

// ── Migration fns (async, generic over Db) ────────────────────────────────────
#[allow(clippy::unused_async)]
async fn migrate_v41_v42<Db>(_db: &Db, old: Message41) -> wavedb_core::Result<Message42> {
    Ok(Message42 {
        id: old.id,
        metadata: old.metadata,
        body: old.body,
        author: old.author,
        edited: false,
    })
}

#[allow(clippy::unused_async)]
async fn rollback_v42_to_v41<Db>(_db: &Db, future: Message42) -> wavedb_core::Result<Message41> {
    Ok(Message41 {
        id: future.id,
        metadata: future.metadata,
        body: future.body,
        author: future.author,
    })
}

#[allow(clippy::unused_async)]
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
    pub body: String,
    pub author: u64,
    pub edited: bool,
}

pub type Message = Message42;

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

    #[allow(clippy::items_after_statements)]
    const fn assert_backward<T: wavedb_core::MigratesFrom<Source = Message41>>() {}
    assert_backward::<Message42>();
    println!("MigratesFrom: Message42::Source = Message41 ✓");

    #[allow(clippy::items_after_statements)]
    const fn assert_forward<T: wavedb_core::RollbackFrom<Future = Message42>>() {}
    assert_forward::<Message41>();
    println!("RollbackFrom:  Message41::Future = Message42 ✓");

    let mut registry = MigrationRegistry::new();
    Message41::register_migration(&mut registry);
    Message42::register_migration(&mut registry);
    assert_eq!(registry.resolve(v41, v42)?.len(), 1);
    assert!(registry.can_rollback(v42, v41));
    println!("Registry edges wired (optional for graph queries).");

    let stored_v41 = Message41 { body: "Hello, WaveDB!".into(), author: 99, ..Default::default() };
    let stored_v42 = Message42 { body: "Hello, v42!".into(), author: 7, edited: true, ..Default::default() };

    let enc_v41 = encode_versioned(&stored_v41);
    let enc_v42 = encode_versioned(&stored_v42);

    let (transport, mut server) = ChannelTransport::pair();
    tokio::spawn(async move {
        server.reply_connect("ws://owner:7700", "ws://backup:7700").await;
        server.reply_ok().await;                    // write v41
        server.reply_data(enc_v41.clone()).await;   // search::<Message42> → v41 bytes → forward migrate
        server.reply_data(enc_v41).await;           // search::<Message41> → v41 bytes → identity
        server.reply_ok().await;                    // write v42
        server.reply_data(enc_v42.clone()).await;   // search::<Message41> → v42 bytes → rollback
        server.reply_data(enc_v42).await;           // search::<Message42> → v42 bytes → identity
    });

    let db = Db::open_with_transport(transport, 1, 100).await?;

    stored_v41.clone().save(&db).await?;
    println!("\nWrote a Message41 record.");

    let as_v42 = Message42::search(&db).await?.expect("record");
    assert_eq!(as_v42.body, "Hello, WaveDB!");
    assert!(!as_v42.edited, "migrate_from_with sets edited=false");
    println!("  → Message42::search returned: {as_v42:?}");

    let as_v41 = Message41::search(&db).await?.expect("record");
    assert_eq!(as_v41.body, "Hello, WaveDB!");
    println!("  → Message41::search returned: {as_v41:?}");

    stored_v42.clone().save(&db).await?;
    println!("\nWrote a Message42 record.");

    let as_v41 = Message41::search(&db).await?.expect("record");
    assert_eq!(as_v41.body, "Hello, v42!");
    println!("  → Message41::search returned: {as_v41:?}");

    let as_v42 = Message42::search(&db).await?.expect("record");
    assert!(as_v42.edited);
    println!("  → Message42::search returned: {as_v42:?}");

    println!("\nmigration_type_1 example OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_main() {
        super::main().unwrap();
    }
}
