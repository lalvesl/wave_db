//! Unique shape end-to-end: create, read, update.
//!
//! Roles:
//!   client  — opens a `Db`, searches for an existing profile, creates one if
//!             absent, then updates the bio.
//!   server  — simulated in-process via `MockTransport` (no network required).
//!
//! Run with:
//!   cargo run --bin unique_user_profile

use wavedb::prelude::*;
use wavedb_net::MockTransport;
use wavedb_net::mock::ScriptedReply;

// ── Schema ───────────────────────────────────────────────────────────────────
//
// The `#[wave_db]` macro auto-derives `Debug, Clone, Serialize, Deserialize`
// and auto-impls `UniqueObject` (search + update) for Unique-shaped structs.
// `PartialEq, Eq` stay manual because not every struct needs them.

#[wave_db(struct_id = 1)]
#[derive(PartialEq, Eq)]
pub struct UserProfile1 {
    pub display_name: String,
    pub bio: String,
}
pub type UserProfile = UserProfile1;

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Step 1: New tenant — no profile yet ──────────────────────────────────
    let new_profile = UserProfile {
        display_name: "Aurora".into(),
        bio: "First write.".into(),
        ..Default::default()
    };

    let updated_profile = UserProfile {
        bio: "Updated bio.".into(),
        ..new_profile.clone()
    };

    // Wire format prepends `STRUCT_VERSION` so the migration chain knows
    // which version each record was written at.  See `do_search_unique`.
    let encode_versioned = |r: &UserProfile| -> Vec<u8> {
        let body = postcard::to_allocvec(r).expect("encode");
        let mut out = Vec::with_capacity(1 + body.len());
        out.push(UserProfile::STRUCT_VERSION);
        out.extend_from_slice(&body);
        out
    };
    let serialized = encode_versioned(&new_profile);
    let serialized_updated = encode_versioned(&updated_profile);

    // Pre-load scripted replies in order:
    //   1. Connect handshake
    //   2. search → None (empty payload)
    //   3. update (create) → ok
    //   4. search → Some(new_profile)
    //   5. update (update bio) → ok
    //   6. search → Some(updated_profile)
    let mock = MockTransport::new();
    mock.push(ScriptedReply::connect(
        "ws://owner:7700",
        "ws://backup:7700",
    ));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(serialized));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(serialized_updated));

    let db = Db::open_with_transport(mock, /* user= */ 1, /* tenant= */ 42).await?;

    // Client role: search → absent → create
    let profile = if let Some(existing) = UserProfile::search(&db).await? {
        println!("Found existing profile: {existing:?}");
        existing
    } else {
        println!("No profile yet — creating Aurora");
        new_profile.clone().update(&db).await?;
        new_profile
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
