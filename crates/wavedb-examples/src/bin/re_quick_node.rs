//! WaveDB Quick-Node subprocess for the `real_example` orchestrated test.
//!
//! # Environment variables (set by the orchestrator)
//!
//! | Variable            | Description                                       |
//! |---------------------|---------------------------------------------------|
//! | `WAVE_QN_LISTEN`    | Socket address to listen on (host:port)           |
//! | `WAVE_QN_PEERS`     | Comma-separated peer quick-node addresses         |
//! | `WAVE_SLOW_ADDR`    | Slow-node address (host:port, no scheme)          |
//! | `WAVE_TENANT`       | Tenant ID (u64) that this node owns               |
//! | `WAVE_QN_DATA_DIR`  | Directory for journal/heap/data files             |
//!
//! Run via `nix run .#real_example` — do not invoke directly.

use std::sync::Arc;

use tokio::net::TcpListener;

use wavedb_quick_node::{
    config::{Config, OwnershipSpec},
    node::QuickNode,
    server,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listen = std::env::var("WAVE_QN_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:0".to_string());
    let peers = std::env::var("WAVE_QN_PEERS").unwrap_or_default();
    let slow_node = std::env::var("WAVE_SLOW_ADDR").ok();
    let tenant: u64 = std::env::var("WAVE_TENANT")
        .unwrap_or_else(|_| "100".to_string())
        .parse()
        .unwrap_or(100);
    let data_dir = std::env::var("WAVE_QN_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::temp_dir().join(format!(
                "wavedb-re-qn-{}",
                std::process::id()
            ))
        });

    std::fs::create_dir_all(&data_dir)?;

    let config = Config {
        listen: listen.clone(),
        peers,
        slow_node,
        owns: vec![OwnershipSpec {
            tenant,
            shard_start: 0,
            shard_end: 4095,
        }],
        bloom_interval_secs: 60,
        journal_compact_secs: 30,
        data_dir,
        cluster_key: None,
    };

    let node = Arc::new(QuickNode::new(config));
    // Start background compaction loop.
    let _compact = node.start_compaction_loop(30);
    // Announce self to peers.
    let node_gossip = node.clone();
    tokio::spawn(async move {
        node_gossip.announce_self().await;
    });

    let listener = TcpListener::bind(&listen).await?;
    let bound = listener.local_addr()?;

    // ── Readiness signal ──────────────────────────────────────────────────────
    // Print the bound address so the orchestrator can parse it.
    println!("WAVE_READY addr={bound}");
    use std::io::Write as _;
    std::io::stdout().flush().ok();

    let app = server::router((*node).clone());
    axum::serve(listener, app).await?;
    Ok(())
}
