//! **The shared schema** — the one file both sides of the application
//! compile in.
//!
//! This is the whole point of WaveDB's "the DB *is* the server" model:
//!
//! ```text
//!                ┌──────────────── schema.rs ────────────────┐
//!                │  Task1 + validate_task + preprocess_task  │
//!                │  declare_objects! { … }  →  REGISTRY      │
//!                └───────────┬───────────────────┬───────────┘
//!                            │ compiled into     │ compiled into
//!                            ▼                   ▼
//!                  app.rs (native client)   backend.rs (Quick-Node)
//!                  runs `validate` before   runs `validate` AGAIN, then
//!                  sending — instant typed  `preprocess` — before the
//!                  error, zero round-trip   WAL commit (the boundary)
//! ```
//!
//! There is no DTO layer and no API schema: the struct below *is* the wire
//! protocol, the storage layout, and the validation contract.

use wavedb::prelude::*;

// ── Rules ────────────────────────────────────────────────────────────────────
//
// `validate` — the shared contract.  Pure and synchronous, so the exact same
// fn runs on the client (fast feedback) and on the Quick-Node (the security
// boundary).  Same fn + same bytes ⇒ same verdict on both sides.

pub fn validate_task(t: &Task1) -> Result<(), ValidationError> {
    if t.title.trim().is_empty() {
        return Err(ValidationError::field("title", "must not be blank"));
    }
    if t.title.len() > 120 {
        return Err(ValidationError::field("title", "longer than 120 bytes"));
    }
    if t.priority > 3 {
        return Err(ValidationError::field("priority", "must be 0–3"));
    }
    Ok(())
}

// `preprocess` — server-authoritative normalisation.  Runs ONLY on the
// Quick-Node, after `validate`, before the WAL commit.  The client never
// runs this; whatever it sends, the *node's* transform decides what is
// stored.  (It may also still reject — it returns Result too.)

#[allow(clippy::unnecessary_wraps)] // the hook contract requires Result
pub fn preprocess_task(t: &mut Task1) -> Result<(), ValidationError> {
    t.title = t.title.trim().to_string();
    t.tag = t.tag.trim().to_lowercase();
    Ok(())
}

// ── The object ───────────────────────────────────────────────────────────────

#[wave_db(
    struct_id = 80,
    NonUnique,
    validate = validate_task,
    preprocess = preprocess_task,
)]
#[derive(PartialEq, Eq)]
pub struct Task1 {
    pub title: String,
    pub tag: String,
    pub priority: u8,
    pub done: bool,
}
pub type Task = Task1;

// ── The registry ─────────────────────────────────────────────────────────────
//
// Every binary of the application declares the same registry.  The backend
// passes `app_objects::REGISTRY` to `QuickNode::with_registry`, which is
// what turns a generic byte-bucket node into *this application's* backend:
// unknown headers refused, `validate` re-checked, `preprocess` applied.

declare_objects! {
    pub mod app_objects {
        tasks: [Task1],
    }
}
