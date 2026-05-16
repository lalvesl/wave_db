//! WaveDB Quick-Node server binary.

use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;

use wavedb_quick_node::{
    config::{Args, Config},
    node::QuickNode,
    server,
};

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

    // Bind before spawning tasks so the port is reserved when gossip fires.
    let listener = TcpListener::bind(&listen).await?;
    info!(%listen, "listening for connections");

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

    // Gossip: announce this node to all configured peers.  Runs concurrently
    // with axum::serve so the server is already accepting connections by the
    // time peers try to verify we're alive.
    let node_gossip = node.clone();
    tokio::spawn(async move {
        node_gossip.announce_self().await;
    });

    let app = server::router(node);
    axum::serve(listener, app).await?;

    Ok(())
}
