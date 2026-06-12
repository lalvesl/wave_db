//! `mini_app` — the smallest complete WaveDB application: one native app,
//! one Quick-Node backend, one shared schema file.
//!
//! ```text
//! schema.rs   Task + validate + preprocess + REGISTRY   (the shared part)
//! backend.rs  Server::bind(…).tenant(…).registry(REGISTRY).serve()
//! app.rs      Db::connect(url, user, tenant) + task.save(&db)
//! main.rs     this orchestrator: spawns backend, runs app, tears down
//! ```
//!
//! Demonstrates: a messy-but-valid record committed (and normalised
//! server-side), an invalid record rejected locally with zero round-trip,
//! a bypassing raw write stopped at the node, and an undeclared struct
//! refused outright.
//!
//! Run with:
//!   cargo run --bin mini_app

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

/// Resolve a sibling binary compiled into the same target directory.
fn sibling_bin(name: &str) -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.set_file_name(name);
    path
}

#[allow(clippy::zombie_processes)] // backend is kill()+wait()ed below
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = std::env::temp_dir()
        .join(format!("wavedb-mini-app-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir); // fresh storage every run

    // Backend on a free port (port 0); it reports the bound address back
    // on its WAVE_READY line.
    let mut backend = Command::new(sibling_bin("mini_app_backend"))
        .env("WAVE_LISTEN", "127.0.0.1:0")
        .env("WAVE_TENANT", "42")
        .env("WAVE_DATA_DIR", &data_dir)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn mini_app_backend (cargo build --bin mini_app_backend)");

    let mut ready = String::new();
    BufReader::new(backend.stdout.take().expect("stdout"))
        .read_line(&mut ready)?;
    let addr = ready
        .trim()
        .strip_prefix("WAVE_READY addr=")
        .ok_or("backend did not report WAVE_READY")?;
    println!("── mini_app ── backend ready on {addr}\n");

    let status = Command::new(sibling_bin("mini_app_client"))
        .env("WAVE_URL", format!("ws://{addr}/ws"))
        .env("WAVE_TENANT", "42")
        .status()
        .expect("spawn mini_app_client (cargo build --bin mini_app_client)");

    backend.kill().ok();
    backend.wait().ok();
    let _ = std::fs::remove_dir_all(&data_dir);

    if !status.success() {
        return Err("mini_app_client failed".into());
    }
    println!("\n── done ── one schema.rs, both sides enforced");
    Ok(())
}
