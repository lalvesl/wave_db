//! WaveDB Monitor subprocess for the `real_example` orchestrated test.
//!
//! Reads node URLs from environment variables and starts the TUI monitor.
//!
//! # Environment variables (set by the orchestrator)
//!
//! | Variable              | Description                                      |
//! |-----------------------|--------------------------------------------------|
//! | `WAVE_QN_HTTP_URLS`   | Comma-separated HTTP URLs of quick nodes         |
//! | `WAVE_SLOW_HTTP_URL`  | HTTP URL of the slow node                        |
//! | `WAVE_REFRESH_MS`     | Monitor poll interval in milliseconds (default 400) |
//!
//! The monitor runs until the user presses `q` or `Esc`, then exits.
//! Run via `nix run .#real_example` — do not invoke directly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use wavedb_monitor::{LogBuffer, new_log, push_log, run_tui_thread};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let qn_urls_raw = std::env::var("WAVE_QN_HTTP_URLS").unwrap_or_default();
    let slow_url = std::env::var("WAVE_SLOW_HTTP_URL").unwrap_or_default();
    let refresh_ms: u64 = std::env::var("WAVE_REFRESH_MS")
        .unwrap_or_else(|_| "400".to_string())
        .parse()
        .unwrap_or(400);

    let quick_node_urls: Vec<String> = qn_urls_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();

    let slow_node_urls: Vec<String> = if slow_url.is_empty() {
        vec![]
    } else {
        vec![slow_url]
    };

    let cfg = wavedb_monitor::config::Config {
        quick_node_urls,
        slow_node_urls,
        cluster_key: None,
        refresh_ms,
    };

    let log: LogBuffer = new_log();
    let done = Arc::new(AtomicBool::new(false));
    let on_quit = Arc::new(AtomicBool::new(false));

    // Signal readiness to the orchestrator.
    {
        use std::io::Write as _;
        println!("WAVE_READY monitor");
        std::io::stdout().flush().ok();
    }

    push_log(&log, "  WaveDB real_example monitor — press q to quit");
    push_log(&log, "  Watching quick-nodes and slow-node...");

    let tui_handle = run_tui_thread(cfg, log, done, on_quit.clone());

    // Wait for user to press q.
    tui_handle.join().ok();

    // Signal the orchestrator to shut down.
    on_quit.store(true, Ordering::Release);

    Ok(())
}
