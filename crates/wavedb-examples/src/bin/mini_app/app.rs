//! **The application** — a native client binary.
//!
//! Compiles the *same* `schema.rs` as the backend; `task.save(&db)` is the
//! whole write path.  No HTTP client, no JSON, no request structs — the
//! typed record is the protocol.
//!
//! Run via `cargo run --bin mini_app` — not directly.  The orchestrator
//! sets `WAVE_URL` / `WAVE_TENANT`.

use wavedb::Db;
use wavedb::prelude::*;
use wavedb_net::request::RequestKind;

#[allow(dead_code)]
mod schema;
use schema::Task;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("WAVE_URL")?;
    let tenant: u64 = std::env::var("WAVE_TENANT")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(42);

    let db = Db::connect(&url, /* user */ 7, tenant).await?;
    println!("[app] connected — owner {}", db.owner_url());

    // 1. Valid but messy input — accepted; the node's `preprocess` trims
    //    the title and lowercases the tag before committing.
    Task {
        title: "   Ship the MVP   ".into(),
        tag: "  URGENT ".into(),
        priority: 2,
        ..Default::default()
    }
    .save(&db)
    .await?;
    println!("[app] 1. messy task saved — node normalises it before commit");

    // 2. Invalid record — `save` runs `validate` locally: typed error,
    //    zero round-trip, the backend never sees these bytes.
    let err = Task {
        title: "   ".into(),
        ..Default::default()
    }
    .save(&db)
    .await
    .expect_err("blank title");
    println!("[app] 2. rejected locally, zero round-trip: {err}");

    // 3. Bypass the typed API (malicious/stale client): raw write, no
    //    local check.  The node re-runs the SAME `validate` and refuses —
    //    the client maps it back to the same typed error as step 2.
    let evil = Task {
        title: String::new(),
        ..Default::default()
    };
    let mut payload = vec![Task::STRUCT_VERSION];
    payload.extend_from_slice(&wavedb_core::wire::to_wire(&evil)?);
    let err = db
        .send(RequestKind::Write {
            struct_id: Task::STRUCT_ID,
            user: db.user(),
            tenant: db.tenant(),
            payload,
        })
        .await
        .expect_err("node must refuse");
    println!("[app] 3. bypass rejected by the node: {err}");

    // 4. A struct_id the backend's registry never declared.
    let err = db
        .send(RequestKind::Write {
            struct_id: 9999,
            user: db.user(),
            tenant: db.tenant(),
            payload: vec![1, 0xAA],
        })
        .await
        .expect_err("undeclared struct");
    println!("[app] 4. undeclared struct refused: {err}");

    println!("[app] done — one schema file, both sides enforced");
    Ok(())
}
