//! WaveDB Quick-Node server binary.
//!
//! A Quick-Node owns a configurable set of `(TENANT_ID, SHARD_ID)` partitions,
//! serves client requests over HTTP POST and WebSocket, replicates writes to
//! peer nodes, and periodically flushes history to a Slow-Node.

use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;

mod config;
mod node;
mod ownership;
mod replication;
mod ring;
mod server;

use config::{Args, Config};
use node::QuickNode;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let args = Args::parse();
    let config = Config::from_args(args);

    let listen = config.listen.clone();
    let bloom_secs = config.bloom_interval_secs;

    let node = QuickNode::new(config);

    info!(
        node_id = node.node_id(),
        listen = %listen,
        partitions = node.ownership().len(),
        "Quick-Node starting",
    );

    // Background: periodic bloom-filter publish tick.
    let node_bg = node.clone();
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(bloom_secs);
        loop {
            tokio::time::sleep(interval).await;
            let partitions = node_bg.ownership().all();
            tracing::debug!(count = partitions.len(), "bloom filter tick");
        }
    });

    let app = server::router(node);
    let listener = TcpListener::bind(&listen).await?;
    info!(%listen, "listening for connections");
    axum::serve(listener, app).await?;

    Ok(())
}
