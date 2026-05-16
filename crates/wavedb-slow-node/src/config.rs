use std::path::PathBuf;

use clap::Parser;
use wavedb_net::auth::ClusterKey;

#[derive(Parser, Debug)]
#[command(
    name = "wavedb-slow-node",
    about = "WaveDB Slow-Node — cold history store"
)]
pub struct Args {
    /// Socket address to listen on.
    #[arg(long, default_value = "0.0.0.0:7800")]
    pub listen: String,

    /// Directory for persistent journal files.
    #[arg(long, default_value = "/var/lib/wavedb-slow")]
    pub data_dir: PathBuf,

    /// Minimum replicas that must ack a flush before it is acknowledged.
    #[arg(long, default_value_t = 1)]
    pub min_replicas: u32,

    /// Shared cluster secret as a 64-hex-character string (32 bytes).
    /// When set, incoming flush requests must carry a valid HMAC-SHA256 token.
    /// Omit to run in open/dev mode with no node-to-node authentication.
    #[arg(long)]
    pub cluster_key: Option<String>,
}

/// Validated runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub data_dir: PathBuf,
    pub min_replicas: u32,
    /// Parsed cluster key for HMAC-SHA256 flush auth, or `None` for open mode.
    pub cluster_key: Option<ClusterKey>,
}

impl Config {
    pub fn from_args(args: Args) -> Self {
        Self {
            listen: args.listen,
            data_dir: args.data_dir,
            min_replicas: args.min_replicas,
            cluster_key: args.cluster_key.as_deref().map(|hex| {
                ClusterKey::from_hex(hex)
                    .expect("--cluster-key must be a 64-hex-character string (32 bytes)")
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let args = Args::parse_from(["wavedb-slow-node"]);
        let cfg = Config::from_args(args);
        assert_eq!(cfg.listen, "0.0.0.0:7800");
        assert_eq!(cfg.min_replicas, 1);
    }

    #[test]
    fn custom_flags_parse() {
        let args = Args::parse_from([
            "wavedb-slow-node",
            "--listen",
            "127.0.0.1:8000",
            "--data-dir",
            "/tmp/slow",
            "--min-replicas",
            "2",
        ]);
        let cfg = Config::from_args(args);
        assert_eq!(cfg.listen, "127.0.0.1:8000");
        assert_eq!(cfg.data_dir, PathBuf::from("/tmp/slow"));
        assert_eq!(cfg.min_replicas, 2);
    }
}
