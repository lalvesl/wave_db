//! **The backend** — this application's Quick-Node binary.
//!
//! The entire backend is one expression: bind, own a tenant, attach the
//! app's registry, serve.  No routes, no DTOs, no handlers, no manual
//! validation calls — attaching `REGISTRY` is what makes this node *this
//! app's* backend.  Every write now passes, before the WAL commit:
//!
//! 1. header declared → 2. payload decodes → 3. `validate` → 4. `preprocess`
//!
//! (That pipeline is exercised end-to-end in
//! `wavedb-test-cluster/tests/e2e_hooks.rs`, including reading the WAL back
//! to prove the *preprocessed* bytes are what's stored.)
//!
//! Run via `cargo run --bin mini_app` — not directly.  The orchestrator
//! sets `WAVE_LISTEN` / `WAVE_TENANT` / `WAVE_DATA_DIR`.

use std::io::Write as _;

use wavedb_quick_node::Server;

#[allow(dead_code)]
mod schema;

#[tokio::main]
async fn main() -> wavedb_core::Result<()> {
    let listen = std::env::var("WAVE_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:0".to_string());
    let tenant: u64 = std::env::var("WAVE_TENANT")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(42);

    let mut server = Server::bind(listen)
        .tenant(tenant)
        .registry(schema::app_objects::REGISTRY);
    if let Ok(dir) = std::env::var("WAVE_DATA_DIR") {
        server = server.data_dir(dir);
    }

    let running = server.start().await?;
    println!("WAVE_READY addr={}", running.addr());
    std::io::stdout().flush().ok();
    running.wait().await
}
