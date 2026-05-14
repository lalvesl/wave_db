//! Unique shape end-to-end: create, read, update.
//!
//! Roles:
//!   client  — opens a `Db`, searches for an existing profile, creates one if
//!             absent, then updates the bio.
//!   server  — simulated in-process via `MockTransport` (no network required).
//!
//! Run with:
//!   cargo run --bin unique_user_profile

use wavedb::object::{do_search_unique, do_write};
use wavedb::prelude::*;
use wavedb_net::MockTransport;
use wavedb_net::mock::ScriptedReply;

// ── Schema ───────────────────────────────────────────────────────────────────

#[wave_db(struct_id = 1)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UserProfile1 {
    pub id: Id,
    pub metadata: Metadata,
    pub display_name: String,
    pub bio: String,
}
pub type UserProfile = UserProfile1;

impl UniqueObject for UserProfile {
    async fn search(db: &Db) -> wavedb_core::Result<Option<Self>> {
        do_search_unique::<Self>(db).await
    }

    async fn update(self, db: &Db) -> wavedb_core::Result<()> {
        do_write(db, &self).await
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Step 1: New tenant — no profile yet ──────────────────────────────────
    let new_profile = UserProfile {
        id: Id::default(),
        metadata: Metadata::default(),
        display_name: "Aurora".into(),
        bio: "First write.".into(),
    };
    let serialized = postcard::to_allocvec(&new_profile)?;

    let updated_profile = UserProfile {
        bio: "Updated bio.".into(),
        ..new_profile.clone()
    };
    let serialized_updated = postcard::to_allocvec(&updated_profile)?;

    // Pre-load scripted replies in order:
    //   1. Connect handshake
    //   2. search → None (empty payload)
    //   3. update (create) → ok
    //   4. search → Some(new_profile)
    //   5. update (update bio) → ok
    //   6. search → Some(updated_profile)
    let mock = MockTransport::new();
    mock.push(ScriptedReply::connect("ws://owner:7700", "ws://backup:7700"));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(serialized));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(serialized_updated));

    let db = Db::open_with_transport(mock, /* user= */ 1, /* tenant= */ 42).await?;

    // Client role: search → absent → create
    let profile = match UserProfile::search(&db).await? {
        Some(existing) => {
            println!("Found existing profile: {existing:?}");
            existing
        }
        None => {
            println!("No profile yet — creating Aurora");
            new_profile.clone().update(&db).await?;
            new_profile
        }
    };
    assert_eq!(profile.display_name, "Aurora");

    // Read back what was written
    let found = UserProfile::search(&db).await?.expect("just written");
    assert_eq!(found.bio, "First write.");
    println!("Confirmed write: bio = {:?}", found.bio);

    // Client role: update bio
    let mut to_update = found;
    to_update.bio = "Updated bio.".into();
    to_update.update(&db).await?;

    // Read back the update
    let after_update = UserProfile::search(&db).await?.expect("still exists");
    assert_eq!(after_update.bio, "Updated bio.");
    println!("Confirmed update: bio = {:?}", after_update.bio);

    println!("unique_user_profile example OK");
    Ok(())
}
