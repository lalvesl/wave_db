//! WaveDB TUI Monitor — standalone binary.
//!
//! Run with:
//!   cargo run --release --bin wavedb-monitor -- --quick-nodes http://127.0.0.1:7700 --slow-node http://127.0.0.1:7800
//!   nix run .#wavedb_monitor

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clap::Parser;

use wavedb_monitor::config::{Args, Config};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::from_args(Args::parse());
    let log = wavedb_monitor::new_log();
    let done = Arc::new(AtomicBool::new(false));

    // Standalone mode: TUI runs until the user presses q (done is never set).
    let on_quit = Arc::new(AtomicBool::new(false));
    let handle = wavedb_monitor::run_tui_thread(cfg, log, done, on_quit);
    handle.join().ok();
    Ok(())
}
