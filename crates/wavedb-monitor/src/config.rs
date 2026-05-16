//! CLI configuration for the WaveDB monitor.

use clap::Parser;

use wavedb_net::auth::ClusterKey;

#[derive(Debug, Parser)]
#[command(name = "wavedb-monitor", about = "TUI monitor for WaveDB clusters")]
pub struct Args {
    /// Comma-separated HTTP URLs of Quick-Nodes to monitor.
    ///
    /// Example: `http://127.0.0.1:7700,http://127.0.0.1:7701`
    #[arg(long, default_value = "http://127.0.0.1:7700")]
    pub quick_nodes: String,

    /// Comma-separated HTTP URLs of Slow-Nodes to monitor.
    ///
    /// Example: `http://127.0.0.1:7800,http://127.0.0.1:7801`
    #[arg(long, default_value = "http://127.0.0.1:7800")]
    pub slow_nodes: String,

    /// 64-hex-character cluster key for HMAC-SHA256 auth.
    ///
    /// Omit for open/dev clusters.
    #[arg(long)]
    pub cluster_key: Option<String>,

    /// Metrics poll interval in milliseconds.
    #[arg(long, default_value_t = 500)]
    pub refresh_ms: u64,
}

pub struct Config {
    pub quick_node_urls: Vec<String>,
    pub slow_node_urls: Vec<String>,
    pub cluster_key: Option<ClusterKey>,
    pub refresh_ms: u64,
}

impl Config {
    pub fn from_args(args: &Args) -> Self {
        let cluster_key = args
            .cluster_key
            .as_deref()
            .map(|hex| ClusterKey::from_hex(hex).expect("--cluster-key must be 64 hex characters"));
        let parse_urls = |s: &str| {
            s.split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect()
        };
        Self {
            quick_node_urls: parse_urls(&args.quick_nodes),
            slow_node_urls: parse_urls(&args.slow_nodes),
            cluster_key,
            refresh_ms: args.refresh_ms,
        }
    }
}
