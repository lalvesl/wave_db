use wavedb_core::{Id, Metadata, MigrationRegistry, WaveDbStruct};
use wavedb_macros::wave_db;

// Three-version chain Message1 → Message2 → Message3.
// Each struct declares its immediate neighbours via TYPE paths.
//
// Migration functions are generic over the Db type so the macro's
// `__WaveDbDb` generic parameter resolves correctly at the call site.

// ── Forward migration fns ────────────────────────────────────────────────────

async fn migrate_v1_v2<Db>(_db: &Db, old: Message1) -> wavedb_core::Result<Message2> {
    Ok(Message2 {
        id: old.id,
        metadata: old.metadata,
        body: old.body,
        edited: false,
    })
}

async fn migrate_v2_v3<Db>(_db: &Db, old: Message2) -> wavedb_core::Result<Message3> {
    Ok(Message3 {
        id: old.id,
        metadata: old.metadata,
        body: old.body,
        edited: old.edited,
        author: 0,
    })
}

// ── Rollback fns (declared on the OLDER struct; take Future, return Self) ────

async fn rollback_v2_to_v1<Db>(_db: &Db, future: Message2) -> wavedb_core::Result<Message1> {
    Ok(Message1 {
        id: future.id,
        metadata: future.metadata,
        body: future.body,
    })
}

async fn rollback_v3_to_v2<Db>(_db: &Db, future: Message3) -> wavedb_core::Result<Message2> {
    Ok(Message2 {
        id: future.id,
        metadata: future.metadata,
        body: future.body,
        edited: future.edited,
    })
}

// ── Search-time hooks ────────────────────────────────────────────────────────

async fn v2_first_try<Db>(_db: &Db) -> wavedb_core::Result<Option<Message1>> {
    Ok(None)
}

async fn v1_fallback<Db>(_db: &Db) -> wavedb_core::Result<Option<Message1>> {
    Ok(None)
}

// ── v1 — oldest: no migrate_from; has migrate_rollback to v2 ─────────────────

#[wave_db(
    struct_id = 7,
    NonUnique,
    migrate_rollback      = Message2,
    migrate_rollback_with = rollback_v2_to_v1,
    fallback_not_found    = v1_fallback,
)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Message1 {
    pub id: Id,
    pub metadata: Metadata,
    pub body: String,
}

// ── v2 — middle: both neighbours declared ────────────────────────────────────

#[wave_db(
    struct_id = 7,
    NonUnique,
    migrate_from          = Message1,
    migrate_from_with     = migrate_v1_v2,
    migrate_rollback      = Message3,
    migrate_rollback_with = rollback_v3_to_v2,
    first_try             = v2_first_try,
)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Message2 {
    pub id: Id,
    pub metadata: Metadata,
    pub body: String,
    pub edited: bool,
}

// ── v3 — newest: no migrate_rollback (no successor yet) ──────────────────────

#[wave_db(
    struct_id = 7,
    NonUnique,
    migrate_from      = Message2,
    migrate_from_with = migrate_v2_v3,
)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Message3 {
    pub id: Id,
    pub metadata: Metadata,
    pub body: String,
    pub edited: bool,
    pub author: u64,
}

pub type Message = Message3;

fn main() {
    // ── WaveDbStruct constants ────────────────────────────────────────────────
    assert_eq!(Message1::STRUCT_VERSION, 1);
    assert_eq!(Message2::STRUCT_VERSION, 2);
    assert_eq!(Message3::STRUCT_VERSION, 3);
    assert_eq!(Message1::STRUCT_ID, 7);

    // ── Compile-time chain via MigratesFrom (backward) ────────────────────────
    fn assert_backward<T, S>()
    where
        T: wavedb_core::MigratesFrom<Source = S>,
        S: WaveDbStruct,
    {
    }
    assert_backward::<Message2, Message1>();
    assert_backward::<Message3, Message2>();
    // Message1: no MigratesFrom (first version) — wouldn't compile if asserted.

    // ── Compile-time chain via RollbackFrom (forward) ─────────────────────────
    fn assert_forward<T, F>()
    where
        T: wavedb_core::RollbackFrom<Future = F>,
        F: WaveDbStruct,
    {
    }
    assert_forward::<Message1, Message2>();
    assert_forward::<Message2, Message3>();
    // Message3: no RollbackFrom (current head) — wouldn't compile if asserted.

    // ── Registry: each struct contributes its own edge(s) ─────────────────────
    let mut registry = MigrationRegistry::new();
    Message1::register_migration(&mut registry); // backward v2→v1
    Message2::register_migration(&mut registry); // forward v1→v2, backward v3→v2
    Message3::register_migration(&mut registry); // forward v2→v3

    let v1 = wavedb_core::VersionRef::new(7, 1);
    let v2 = wavedb_core::VersionRef::new(7, 2);
    let v3 = wavedb_core::VersionRef::new(7, 3);

    // Full forward chain v1 → v3 (two-hop).
    let plan = registry.resolve(v1, v3).expect("forward chain");
    assert_eq!(plan.len(), 2);

    // Full rollback v3 → v1.
    assert!(registry.can_rollback(v3, v1));
    // Single-step rollback v2 → v1 also reachable.
    assert!(registry.can_rollback(v2, v1));
}
