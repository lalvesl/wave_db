use std::path::PathBuf;

use clap::Parser;

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
}

/// Validated runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub data_dir: PathBuf,
    pub min_replicas: u32,
}

impl Config {
    pub fn from_args(args: Args) -> Self {
        Self {
            listen: args.listen,
            data_dir: args.data_dir,
            min_replicas: args.min_replicas,
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
