//! WaveDB Slow-Node subprocess for the `real_example` orchestrated test.
//!
//! # Environment variables (set by the orchestrator)
//!
//! | Variable            | Description                                  |
//! |---------------------|----------------------------------------------|
//! | `WAVE_SLOW_ADDR`    | Socket address to listen on (host:port)      |
//! | `WAVE_SLOW_DATA_DIR`| Directory for persistent storage files       |
//!
//! Run via `nix run .#real_example` — do not invoke directly.

use tokio::net::TcpListener;

use wavedb_slow_node::{server, store::HistoryStore};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("WAVE_SLOW_ADDR").unwrap_or_else(|_| "127.0.0.1:0".to_string());
    let data_dir = std::env::var("WAVE_SLOW_DATA_DIR").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join("wavedb-re-slow")
            .to_string_lossy()
            .into_owned()
    });

    std::fs::create_dir_all(&data_dir)?;
    let store = HistoryStore::open(std::path::Path::new(&data_dir))?;

    let listener = TcpListener::bind(&addr).await?;
    let bound = listener.local_addr()?;

    // ── Readiness signal ──────────────────────────────────────────────────────
    // Print the exact bound address on stdout so the orchestrator can parse it
    // and know we are ready to accept connections.  The orchestrator reads one
    // line from our stdout pipe (WAVE_READY prefix).
    println!("WAVE_READY addr={bound}");
    // Flush stdout so the orchestrator sees the line immediately.
    use std::io::Write as _;
    std::io::stdout().flush().ok();

    axum::serve(listener, server::router(store, None)).await?;
    Ok(())
}
