//! WaveDB Slow-Node — cold history store.

use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;

use wavedb_slow_node::{
    config::{Args, Config},
    server,
    store::HistoryStore,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let args = Args::parse();
    let config = Config::from_args(args);

    let store = HistoryStore::open(&config.data_dir)?;

    info!(listen = %config.listen, "wavedb-slow-node starting");

    let listener = TcpListener::bind(&config.listen).await?;
    axum::serve(listener, server::router(store)).await?;

    Ok(())
}
