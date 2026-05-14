//! Type 1 migration: field-level transform between two Message versions.
//!
//! `Message41` → `Message42` adds an `edited: bool` field.
//! The rollback strips it back out.
//!
//! The `MigrationRegistry` tracks which version transitions exist and
//! resolves multi-hop chains at startup.  Migration functions themselves
//! are plain async fns that transform the old struct into the new one.
//!
//! Run with:
//!   cargo run --bin migration_type_1

use wavedb::prelude::*;
use wavedb_core::migration::{MigrationRegistry, VersionRef};

// ── Schema — v41 ─────────────────────────────────────────────────────────────

#[wave_db(struct_id = 41)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Message41 {
    pub id: Id,
    pub metadata: Metadata,
    pub body: String,
    pub author: u64,
}

// ── Schema — v42 ─────────────────────────────────────────────────────────────

#[wave_db(struct_id = 41)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Message42 {
    pub id: Id,
    pub metadata: Metadata,
    pub body: String,
    pub author: u64,
    pub edited: bool,
}
pub type Message = Message42;

// ── Migration functions ───────────────────────────────────────────────────────

fn migrate_41_42(old: Message41) -> Message42 {
    Message42 {
        id: old.id,
        metadata: old.metadata,
        body: old.body,
        author: old.author,
        edited: false,
    }
}

fn rollback_42_41(new: Message42) -> Message41 {
    Message41 {
        id: new.id,
        metadata: new.metadata,
        body: new.body,
        author: new.author,
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    const SID: u32 = 41;
    let v41 = VersionRef::new(SID, 41);
    let v42 = VersionRef::new(SID, 42);

    // Register the migration with rollback support
    let mut registry = MigrationRegistry::new();
    registry.register_with_rollback(v41, v42);

    // Forward plan: v41 → v42 (one step)
    let plan = registry.resolve(v41, v42).expect("path exists");
    assert_eq!(plan.len(), 1);
    assert_eq!(plan.steps[0].from, v41);
    assert_eq!(plan.steps[0].to, v42);
    println!("Forward plan: {} step(s)", plan.len());

    // Rollback plan: v42 → v41
    assert!(
        registry.can_rollback(v42, v41),
        "rollback must be registered"
    );
    println!("Rollback v42 → v41: available");

    // Execute forward migration on a v41 record
    let old_msg = Message41 {
        id: Id::default(),
        metadata: Metadata::default(),
        body: "Hello, WaveDB!".into(),
        author: 99,
    };
    let new_msg = migrate_41_42(old_msg.clone());
    assert_eq!(new_msg.body, old_msg.body);
    assert_eq!(new_msg.author, old_msg.author);
    assert!(!new_msg.edited, "new field defaults to false");
    println!("Migrated: {new_msg:?}");

    // Execute rollback
    let rolled_back = rollback_42_41(new_msg);
    assert_eq!(rolled_back.body, old_msg.body);
    println!("Rolled back: {rolled_back:?}");

    // Multi-hop: register v42 → v43 and resolve v41 → v43
    let v43 = VersionRef::new(SID, 43);
    registry.register_forward(v42, v43);
    let chain = registry.resolve(v41, v43).expect("chain exists");
    assert_eq!(chain.len(), 2, "v41→v43 is a two-step chain");
    println!("Multi-hop chain v41→v43: {} steps", chain.len());

    println!("migration_type_1 example OK");
}
