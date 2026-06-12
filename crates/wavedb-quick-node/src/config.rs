//! CLI argument parsing and validated runtime configuration.

use std::path::PathBuf;

use clap::Parser;
use wavedb_net::auth::ClusterKey;

// ── Args ──────────────────────────────────────────────────────────────────────

/// WaveDB Quick-Node server.
#[derive(Debug, Parser)]
#[command(
    name = "wavedb-quick-node",
    about = "WaveDB Quick-Node — hot-tier partition owner and request router"
)]
pub struct Args {
    /// Socket address to listen on (e.g. `0.0.0.0:7700`).
    #[arg(long, default_value = "0.0.0.0:7700")]
    pub listen: String,

    /// Directory for data, index, heap, and journal files.
    #[arg(long, default_value = "/var/lib/wavedb/quick")]
    pub data_dir: PathBuf,

    /// Comma-separated peer Quick-Node addresses (e.g. `10.0.0.2:7700,10.0.0.3:7700`).
    #[arg(long, default_value = "")]
    pub peers: String,

    /// Slow-Node address for history flush (e.g. `10.0.0.10:7800`).
    #[arg(long)]
    pub slow_node: Option<String>,

    /// Bloom-filter publish interval in seconds.
    #[arg(long, default_value_t = 1)]
    pub bloom_interval_secs: u64,

    /// Shared cluster secret as a 64-hex-character string (32 bytes).
    /// When set, all gossip messages are HMAC-SHA256 signed and verified.
    /// Omit to run in open/dev mode with no node-to-node authentication.
    #[arg(long)]
    pub cluster_key: Option<String>,
}

// ── Config ────────────────────────────────────────────────────────────────────

/// Validated runtime configuration derived from [`Args`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Socket address to listen on.
    pub listen: String,
    /// Comma-separated peer addresses (raw string for lazy parsing).
    pub peers: String,
    /// Optional Slow-Node address.
    pub slow_node: Option<String>,
    /// Bloom-filter tick interval (seconds).
    pub bloom_interval_secs: u64,
    /// Journal compaction interval (seconds).  Every tick: flush data.bin,
    /// checkpoint the journal, truncate pre-checkpoint entries from memory
    /// and disk.  0 disables background compaction.
    pub journal_compact_secs: u64,
    /// Data directory.
    pub data_dir: PathBuf,
    /// Parsed cluster key for HMAC-SHA256 node-to-node auth, or `None` for open mode.
    pub cluster_key: Option<ClusterKey>,
}

impl Config {
    /// Build a [`Config`] from parsed [`Args`].
    pub fn from_args(args: Args) -> Self {
        Self {
            listen: args.listen,
            peers: args.peers,
            slow_node: args.slow_node,
            bloom_interval_secs: args.bloom_interval_secs,
            journal_compact_secs: 30,
            data_dir: args.data_dir,
            cluster_key: args.cluster_key.as_deref().map(|hex| {
                ClusterKey::from_hex(hex)
                    .expect("--cluster-key must be a 64-hex-character string (32 bytes)")
            }),
        }
    }

    /// Derive a stable node ID from this node's listen address.
    pub fn node_id(&self) -> crate::ring::NodeId {
        crate::node::addr_to_node_id(&self.listen)
    }

    /// Return peer addresses, splitting the comma-separated string.
    pub fn peer_addrs(&self) -> Vec<String> {
        self.peers
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_peer_addrs_splits_commas() {
        let config = Config {
            listen: "0.0.0.0:7700".into(),
            peers: "10.0.0.2:7700,10.0.0.3:7700".into(),
            slow_node: None,
            bloom_interval_secs: 1,
            journal_compact_secs: 30,
            data_dir: std::path::PathBuf::from("/tmp"),
            cluster_key: None,
        };
        let addrs = config.peer_addrs();
        assert_eq!(addrs, ["10.0.0.2:7700", "10.0.0.3:7700"]);
    }

    #[test]
    fn config_empty_peers_returns_empty_vec() {
        let config = Config {
            listen: "0.0.0.0:7700".into(),
            peers: String::new(),
            slow_node: None,
            bloom_interval_secs: 1,
            journal_compact_secs: 30,
            data_dir: std::path::PathBuf::from("/tmp"),
            cluster_key: None,
        };
        assert!(config.peer_addrs().is_empty());
    }

    #[test]
    fn config_peer_addrs_trims_whitespace() {
        let config = Config {
            listen: "0.0.0.0:7700".into(),
            peers: " a:1 , b:2 ".into(),
            slow_node: None,
            bloom_interval_secs: 1,
            journal_compact_secs: 30,
            data_dir: std::path::PathBuf::from("/tmp"),
            cluster_key: None,
        };
        assert_eq!(config.peer_addrs(), ["a:1", "b:2"]);
    }
}
