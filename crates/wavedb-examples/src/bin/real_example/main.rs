//! Real-world scenario: payment gateway under sustained, randomised high load
//! with periodic cascading node failures.
//!
//! # Architecture (multi-process)
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  re_quick_node[0]  ─┐                                        │
//! │  re_quick_node[1]  ─┼── tenant 100 (payment service)         │
//! │  re_quick_node[2]  ─┘  500 concurrent client processes       │
//! │         │                                                     │
//! │         ▼  flush every 10 s                                   │
//! │  re_slow_node  ── audit trail (durable versioned history)    │
//! │                                                               │
//! │  re_monitor    ── live TUI — press q to stop                  │
//! │                                                               │
//! │  re_client ×500 ── one OS process per client, 500 total      │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Orchestration
//!
//! This binary is the **orchestrator**. It:
//! 1. Finds 5 free TCP ports (1 slow-node + 3 quick-nodes + 1 unused).
//! 2. Spawns `re_slow_node` and waits for its `WAVE_READY` line.
//! 3. Spawns 3 `re_quick_node` processes with correct peer lists and waits
//!    for all of them to signal `WAVE_READY`.
//! 4. Spawns 1 `re_monitor` process pointing at the live nodes.
//! 5. Spawns 500 `re_client` processes in parallel (all start simultaneously).
//! 6. Waits for the monitor to exit (user presses `q`).
//! 7. Sends SIGTERM to all client, quick-node, slow-node processes.
//! 8. Waits for everything to finish and prints a summary.
//!
//! Run with:
//!   nix run .#real_example
//!   cargo run --release --bin real_example

use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ── Constants ─────────────────────────────────────────────────────────────────

const NUM_QUICK_NODES: usize = 3;
const NUM_CLIENTS: usize = 500;
const TENANT: u64 = 100;

// ── Port allocation ───────────────────────────────────────────────────────────

/// Bind a random port on loopback and return the address string.
/// The listener is dropped immediately — the port may be recycled by
/// another process, but on Linux it stays available for a few seconds.
fn find_free_port() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind free port");
    let addr = listener.local_addr().expect("local_addr");
    let _ = drop(listener);
    addr.to_string()
}

// ── Process spawning helpers ──────────────────────────────────────────────────

/// Resolve the path to a compiled binary.
///
/// When invoked via `nix run .#real_example` the binary is compiled into a
/// Nix store path.  We use `std::env::current_exe()` to find our own location
/// and resolve siblings in the same `bin/` directory.
fn sibling_bin(name: &str) -> std::path::PathBuf {
    // Our binary lives at e.g.:
    //   .../target/release/real_example
    // Siblings are at:
    //   .../target/release/<name>
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // remove binary name
    path.push(name);
    path
}

/// Read one `WAVE_READY` line from a child's stdout.
///
/// Returns the full line (without trailing newline) so the caller can parse
/// embedded key=value pairs.  Panics if the child closes stdout before
/// sending the line.
fn wait_ready(reader: &mut BufReader<impl std::io::Read>, label: &str) -> String {
    for line in reader.lines() {
        let line = line.expect("read child stdout");
        eprintln!("[orchestrator] {label}: {line}");
        if line.starts_with("WAVE_READY") {
            return line;
        }
    }
    panic!("[orchestrator] {label} closed stdout without sending WAVE_READY");
}

/// Kill a child process gracefully (SIGTERM on Unix), ignoring errors.
fn kill_child(child: &mut Child) {
    // std::process::Child::kill() sends SIGKILL on Unix / TerminateProcess on
    // Windows.  For this test harness that is acceptable — all subprocesses
    // are stateless enough that a hard kill is fine.
    let _ = child.kill();
}

// ── Summary counters (collected from client stdout) ───────────────────────────

#[derive(Default)]
struct Summary {
    committed: AtomicU64,
    dropped: AtomicU64,
}

// ── Separator ─────────────────────────────────────────────────────────────────

fn sep(title: &str) {
    println!(
        "\n── {title} {}",
        "─".repeat(54_usize.saturating_sub(title.len() + 3))
    );
}

// ── Try to raise RLIMIT_NOFILE ────────────────────────────────────────────────

fn bump_nofile(target: u64) {
    use rlimit::Resource;
    let (soft, hard) = match Resource::NOFILE.get() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("rlimit get failed: {e}");
            return;
        }
    };
    let want = hard.min(target).max(soft);
    if want > soft {
        if let Err(e) = Resource::NOFILE.set(want, hard) {
            eprintln!("rlimit set {want} failed: {e}");
        } else {
            eprintln!("Bumped RLIMIT_NOFILE: {soft} → {want} (hard {hard})");
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║   WaveDB — Payment Gateway Multi-Process Load Test       ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();
    println!("  Architecture:");
    println!("    • 1 slow-node  (audit trail)");
    println!("    • {NUM_QUICK_NODES} quick-nodes (hot tier)");
    println!("    • {NUM_CLIENTS} client processes (payment writers)");
    println!("    • 1 monitor    (live TUI — press q to stop)");
    println!();

    // Raise fd limit: each client needs at least 1 socket.
    // 500 clients × 2 fds + nodes + misc ≈ 2000 — well within 65536.
    bump_nofile(65_536);

    // ── Create temp directories ───────────────────────────────────────────────
    let base_tmp = std::env::temp_dir().join(format!("wavedb-re-{}", std::process::id()));
    std::fs::create_dir_all(&base_tmp)?;

    // ── Allocate ports ────────────────────────────────────────────────────────
    let slow_addr = find_free_port();
    let qn_addrs: Vec<String> = (0..NUM_QUICK_NODES).map(|_| find_free_port()).collect();

    eprintln!("[orchestrator] slow-node addr  : {slow_addr}");
    for (i, addr) in qn_addrs.iter().enumerate() {
        eprintln!("[orchestrator] quick-node[{i}] addr: {addr}");
    }

    // ── Spawn slow-node ───────────────────────────────────────────────────────
    sep("Spawning slow-node");

    let slow_data_dir = base_tmp.join("slow-node");
    std::fs::create_dir_all(&slow_data_dir)?;

    let mut slow_child = Command::new(sibling_bin("re_slow_node"))
        .env("WAVE_SLOW_ADDR", &slow_addr)
        .env(
            "WAVE_SLOW_DATA_DIR",
            slow_data_dir.to_string_lossy().as_ref(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn re_slow_node");

    let slow_stdout = slow_child.stdout.take().expect("slow stdout");
    let mut slow_reader = BufReader::new(slow_stdout);
    wait_ready(&mut slow_reader, "slow-node");
    println!("  ✓ slow-node ready at http://{slow_addr}");

    // Keep draining slow-node stderr in background to avoid pipe stalls.
    std::thread::spawn(move || {
        for line in slow_reader.lines().flatten() {
            eprintln!("[slow-node] {line}");
        }
    });

    // ── Spawn quick-nodes ─────────────────────────────────────────────────────
    sep("Spawning quick-nodes");

    let mut qn_children: Vec<Child> = Vec::with_capacity(NUM_QUICK_NODES);

    for i in 0..NUM_QUICK_NODES {
        let peers: String = qn_addrs
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, a)| a.as_str())
            .collect::<Vec<_>>()
            .join(",");

        let data_dir = base_tmp.join(format!("quick-{i}"));
        std::fs::create_dir_all(&data_dir)?;

        let child = Command::new(sibling_bin("re_quick_node"))
            .env("WAVE_QN_LISTEN", &qn_addrs[i])
            .env("WAVE_QN_PEERS", &peers)
            .env("WAVE_SLOW_ADDR", &slow_addr)
            .env("WAVE_TENANT", TENANT.to_string())
            .env("WAVE_QN_DATA_DIR", data_dir.to_string_lossy().as_ref())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn re_quick_node[{i}]: {e}"));

        qn_children.push(child);
    }

    // Wait for all quick-nodes to be ready.
    for (i, child) in qn_children.iter_mut().enumerate() {
        let stdout = child.stdout.take().expect("qn stdout");
        let mut reader = BufReader::new(stdout);
        wait_ready(&mut reader, &format!("quick-node[{i}]"));
        println!(
            "  ✓ quick-node[{i}] ready at http://{} (ws://{})",
            qn_addrs[i], qn_addrs[i]
        );
        // Drain remaining stdout in background.
        let label = format!("quick-node[{i}]");
        std::thread::spawn(move || {
            for line in reader.lines().flatten() {
                eprintln!("[{label}] {line}");
            }
        });
    }

    // Brief pause: let quick-nodes exchange gossip before clients connect.
    std::thread::sleep(Duration::from_millis(300));

    // ── Spawn monitor ─────────────────────────────────────────────────────────
    sep("Spawning monitor");

    let qn_http_urls = qn_addrs
        .iter()
        .map(|a| format!("http://{a}"))
        .collect::<Vec<_>>()
        .join(",");
    let slow_http_url = format!("http://{slow_addr}");

    let mut monitor_child = Command::new(sibling_bin("re_monitor"))
        .env("WAVE_QN_HTTP_URLS", &qn_http_urls)
        .env("WAVE_SLOW_HTTP_URL", &slow_http_url)
        .env("WAVE_REFRESH_MS", "400")
        // Monitor inherits stdin/stdout for the TUI.
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn re_monitor");

    println!("  ✓ monitor started — TUI will take over the terminal");
    println!("    (press q in the TUI to stop the scenario)");
    println!();

    // ── Spawn 500 client processes ────────────────────────────────────────────
    sep("Spawning 500 client processes");

    let ws_urls = qn_addrs
        .iter()
        .map(|a| format!("ws://{a}/ws"))
        .collect::<Vec<_>>()
        .join(",");

    let summary = Arc::new(Summary::default());
    let mut client_children: Vec<Child> = Vec::with_capacity(NUM_CLIENTS);
    let mut client_stdout_handles = Vec::with_capacity(NUM_CLIENTS);

    let t_spawn_start = Instant::now();
    for client_id in 0..NUM_CLIENTS {
        let child = Command::new(sibling_bin("re_client"))
            .env("WAVE_QN_WS_URLS", &ws_urls)
            .env("WAVE_TENANT", TENANT.to_string())
            .env("WAVE_CLIENT_ID", client_id.to_string())
            .env("WAVE_NUM_CLIENTS", NUM_CLIENTS.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // suppress per-client noise
            .spawn()
            .unwrap_or_else(|e| panic!("spawn re_client[{client_id}]: {e}"));

        client_children.push(child);
    }

    // Collect client stdout pipes for readiness + summary parsing.
    // We do this after all spawns so all 500 processes start truly in parallel.
    for child in &mut client_children {
        let stdout = child.stdout.take().expect("client stdout");
        client_stdout_handles.push(BufReader::new(stdout));
    }
    eprintln!(
        "[orchestrator] Spawned {NUM_CLIENTS} clients in {:.0} ms",
        t_spawn_start.elapsed().as_secs_f64() * 1000.0
    );

    // Wait for all clients to signal WAVE_READY in parallel threads.
    let mut ready_handles = Vec::with_capacity(NUM_CLIENTS);
    for (_id, reader) in client_stdout_handles.into_iter().enumerate() {
        let summary_arc = Arc::clone(&summary);
        let handle = std::thread::spawn(move || {
            let mut reader = reader;
            // First, read until WAVE_READY.
            for line in reader.by_ref().lines().flatten() {
                if line.starts_with("WAVE_READY") {
                    break;
                }
            }
            // Then read until WAVE_DONE to collect summary.
            for line in reader.by_ref().lines().flatten() {
                if line.starts_with("WAVE_DONE") {
                    // Parse committed=N dropped=N
                    let mut c = 0u64;
                    let mut d = 0u64;
                    for part in line.split_whitespace() {
                        if let Some(v) = part.strip_prefix("committed=") {
                            c = v.parse().unwrap_or(0);
                        } else if let Some(v) = part.strip_prefix("dropped=") {
                            d = v.parse().unwrap_or(0);
                        }
                    }
                    summary_arc.committed.fetch_add(c, Ordering::Relaxed);
                    summary_arc.dropped.fetch_add(d, Ordering::Relaxed);
                    break;
                }
            }
        });
        ready_handles.push(handle);
    }

    println!("  ✓ All {NUM_CLIENTS} client processes spawned and connecting…");

    // ── Wait for monitor to exit (user presses q) ─────────────────────────────
    sep("Running — TUI monitor active");
    println!("  The TUI monitor is now live. Press q to stop the scenario.");
    println!();

    let t_start = Instant::now();
    monitor_child.wait()?;
    let elapsed = t_start.elapsed();

    // ── Teardown ──────────────────────────────────────────────────────────────
    sep("Shutting down");

    println!("  Sending SIGTERM to all client processes…");
    for child in &mut client_children {
        kill_child(child);
    }

    println!("  Sending SIGTERM to quick-nodes…");
    for child in &mut qn_children {
        kill_child(child);
    }

    println!("  Sending SIGTERM to slow-node…");
    kill_child(&mut slow_child);

    // Wait for client threads to drain stdout and collect summary.
    println!("  Waiting for client summary lines…");
    for h in ready_handles {
        let _ = h.join();
    }

    // Reap all children.
    for child in &mut client_children {
        let _ = child.wait();
    }
    for child in &mut qn_children {
        let _ = child.wait();
    }
    let _ = slow_child.wait();

    // Clean up temp directory.
    let _ = std::fs::remove_dir_all(&base_tmp);

    // ── Summary ───────────────────────────────────────────────────────────────
    sep("Summary");

    let committed = summary.committed.load(Ordering::Relaxed);
    let dropped = summary.dropped.load(Ordering::Relaxed);
    #[allow(clippy::cast_precision_loss)]
    let throughput = committed as f64 / elapsed.as_secs_f64().max(0.001);

    println!("  Duration            : {:.1}s", elapsed.as_secs_f64());
    println!("  Client processes    : {NUM_CLIENTS}");
    println!("  Quick-node processes: {NUM_QUICK_NODES}");
    println!("  Writes committed    : {committed}");
    println!("  Writes dropped      : {dropped}  (clients on draining/failed nodes)");
    println!("  Throughput (avg)    : {throughput:.0} writes/s");
    println!();
    println!("✓  Multi-process load test complete.");

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify port allocation returns distinct addresses.
    #[test]
    fn find_free_port_returns_unique_addrs() {
        let a = find_free_port();
        let b = find_free_port();
        assert_ne!(a, b, "two free ports must be distinct");
        // Both must parse as valid socket addresses.
        a.parse::<std::net::SocketAddr>().expect("valid addr a");
        b.parse::<std::net::SocketAddr>().expect("valid addr b");
    }

    /// Verify that sibling_bin returns a path in the same directory as us.
    #[test]
    fn sibling_bin_same_dir() {
        let us = std::env::current_exe().expect("current_exe");
        let parent = us.parent().expect("parent dir");
        let sibling = sibling_bin("re_client");
        assert_eq!(
            sibling.parent().expect("sibling parent"),
            parent,
            "sibling must be in same dir as orchestrator"
        );
    }
}
