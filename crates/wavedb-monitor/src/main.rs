//! WaveDB TUI Monitor
//!
//! Polls Quick-Nodes and the Slow-Node for live metrics and renders them in a
//! terminal UI.  Vim motions (j/k/g/G//) drive navigation.
//!
//! Run with:
//!   cargo run --release --bin wavedb-monitor -- --quick-nodes http://127.0.0.1:7700 --slow-node http://127.0.0.1:7800
//!   nix run .#wavedb_monitor

mod config;
mod poll;
mod ui;

use std::io;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use config::{Args, Config};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::from_args(Args::parse());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(cfg.refresh_ms.saturating_sub(50).max(200)))
        .build()?;

    // ── Terminal setup ────────────────────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &cfg, &client).await;

    // ── Cleanup ───────────────────────────────────────────────────────────
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Monitor error: {e}");
    }
    Ok(())
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cfg: &Config,
    client: &reqwest::Client,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = ui::AppState::new();
    let refresh = Duration::from_millis(cfg.refresh_ms);
    let mut last_poll = Instant::now().checked_sub(refresh).unwrap_or(Instant::now());

    // sysinfo: only need current process.
    let mut sys = System::new();
    let pid = Pid::from_u32(std::process::id());

    loop {
        // ── Poll metrics ──────────────────────────────────────────────────
        if last_poll.elapsed() >= refresh {
            let snapshot = poll::poll_all(cfg, client).await;
            state.update(snapshot);

            // Refresh process metrics.
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                false,
                ProcessRefreshKind::new().with_memory().with_cpu(),
            );
            if let Some(proc) = sys.process(pid) {
                state.process_rss = proc.memory();
                state.process_cpu = proc.cpu_usage();
            }

            last_poll = Instant::now();
        }

        // ── Render ────────────────────────────────────────────────────────
        terminal.draw(|f| ui::render(f, &mut state))?;

        // ── Handle input (non-blocking) ───────────────────────────────────
        if let Some(key) = ui::poll_key() {
            if !ui::handle_key(&mut state, key) {
                break;
            }
        }

        // Yield briefly so we don't spin at 100 % CPU.
        tokio::time::sleep(Duration::from_millis(16)).await;
    }

    Ok(())
}
