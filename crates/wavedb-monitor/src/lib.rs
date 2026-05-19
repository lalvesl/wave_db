//! Library interface for the WaveDB monitor TUI.
//!
//! Exposes the `config`, `poll`, and `ui` modules, plus `run_tui_thread` for
//! embedding the live monitor inside other binaries (e.g. `real_example`).

pub mod config;
pub mod poll;
pub mod ui;

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::poll::poll_all;
use crate::ui::{AppState, handle_key, poll_key, render};

// ── Public types ──────────────────────────────────────────────────────────────

/// Shared buffer of log messages pushed from an embedding application.
///
/// The monitor TUI drains this buffer on every render tick and appends each
/// message to the Events panel.
pub type LogBuffer = Arc<Mutex<VecDeque<String>>>;

/// Push a message into a [`LogBuffer`].
///
/// Silently drops the message if the lock is poisoned.
pub fn push_log(log: &LogBuffer, msg: impl Into<String>) {
    if let Ok(mut buf) = log.lock() {
        buf.push_back(msg.into());
    }
}

/// Allocate a new, empty [`LogBuffer`].
pub fn new_log() -> LogBuffer {
    Arc::new(Mutex::new(VecDeque::new()))
}

// ── TUI thread ────────────────────────────────────────────────────────────────

/// Spawn the monitor TUI in a dedicated OS thread.
///
/// The thread creates its own Tokio runtime and polls the node URLs in `cfg`.
/// External log messages can be injected via `log`; they appear in the Events
/// panel on the next render tick.
///
/// When `done` is set to `true` the Events panel shows a "scenario complete"
/// banner, but the TUI stays alive until the user presses `q` / `Esc`.
///
/// `on_quit` is set to `true` immediately before the TUI exits — use it as a
/// shutdown signal for any background scenario loop.
///
/// Call `handle.join()` to wait for the terminal to be restored before
/// printing further output.
pub fn run_tui_thread(
    cfg: config::Config,
    log: LogBuffer,
    done: Arc<AtomicBool>,
    on_quit: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("TUI runtime");
        rt.block_on(async move {
            if let Err(e) = run_tui(cfg, log, done, on_quit.clone()).await {
                let _ = disable_raw_mode();
                eprintln!("TUI error: {e}");
            }
            on_quit.store(true, Ordering::Release);
        });
    })
}

// ── Internal TUI loop ─────────────────────────────────────────────────────────

async fn run_tui(
    cfg: config::Config,
    log: LogBuffer,
    done: Arc<AtomicBool>,
    _on_quit: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(
            cfg.refresh_ms.saturating_sub(50).max(200),
        ))
        // All monitor polls are loopback — never route through a system proxy.
        // Without this, HTTP_PROXY env vars silently break every metrics poll.
        .no_proxy()
        .build()?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = AppState::new();
    let refresh = Duration::from_millis(cfg.refresh_ms);
    let mut last_poll = Instant::now()
        .checked_sub(refresh)
        .unwrap_or_else(Instant::now);
    let mut sys = System::new();
    let pid = Pid::from_u32(std::process::id());
    let mut done_announced = false;

    loop {
        // Drain external log messages into the UI state.
        if let Ok(mut buf) = log.try_lock() {
            while let Some(msg) = buf.pop_front() {
                state.push_log(msg);
            }
        }

        // Poll metrics on the configured schedule.
        if last_poll.elapsed() >= refresh {
            let snapshot = poll_all(&cfg, &client).await;
            state.update(snapshot);
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

        terminal.draw(|f| render(f, &mut state))?;

        // Announce completion once (no auto-exit — user must press q).
        if done.load(Ordering::Acquire) && !done_announced {
            done_announced = true;
            state.push_log(
                "══ Stress test complete — press q to exit and see summary ══".to_string(),
            );
        }

        if let Some(key) = poll_key() {
            if !handle_key(&mut state, key) {
                break;
            }
        }

        tokio::time::sleep(Duration::from_millis(16)).await;
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
