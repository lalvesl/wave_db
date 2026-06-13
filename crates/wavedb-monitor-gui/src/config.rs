//! CLI configuration for the WaveDB GUI monitor.

use clap::Parser;

use wavedb_net::auth::ClusterKey;

#[derive(Debug, Parser)]
#[command(
    name = "wavedb-monitor-gui",
    about = "Desktop GUI monitor for WaveDB clusters"
)]
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
    /// Omit for open/dev clusters; it can also be entered later in the
    /// Settings tab.
    #[arg(long)]
    pub cluster_key: Option<String>,

    /// Metrics poll interval in milliseconds.
    #[arg(long, default_value_t = 500)]
    pub refresh_ms: u64,

    /// Tab to open on launch.
    #[arg(long, value_enum, default_value_t = StartTab::Overview)]
    pub tab: StartTab,
}

/// Launch tab selector (`--tab data` opens the data explorer directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum StartTab {
    Overview,
    Nodes,
    Data,
    Settings,
}

impl StartTab {
    pub const fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Nodes => 1,
            Self::Data => 2,
            Self::Settings => 3,
        }
    }
}

pub struct Config {
    pub quick_node_urls: Vec<String>,
    pub slow_node_urls: Vec<String>,
    pub cluster_key: Option<ClusterKey>,
    pub refresh_ms: u64,
}

impl Config {
    pub fn from_args(args: &Args) -> Self {
        let cluster_key = args.cluster_key.as_deref().map(|hex| {
            ClusterKey::from_hex(hex).unwrap_or_else(|_| {
                eprintln!(
                    "error: --cluster-key must be the cluster's 64-hex-character \
                     (32-byte) secret; got {} characters.\n\
                     The key can also be pasted into the Settings tab at runtime.",
                    hex.len()
                );
                std::process::exit(2);
            })
        });
        let parse_urls = |s: &str| {
            s.split(',')
                .map(|u| u.trim().trim_end_matches('/').to_string())
                .filter(|u| !u.is_empty())
                .collect()
        };
        Self {
            quick_node_urls: parse_urls(&args.quick_nodes),
            slow_node_urls: parse_urls(&args.slow_nodes),
            cluster_key,
            refresh_ms: args.refresh_ms.max(100),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_and_trim() {
        let args = Args {
            quick_nodes: " http://a:1 ,http://b:2/ ,".into(),
            slow_nodes: "http://c:3".into(),
            cluster_key: None,
            refresh_ms: 500,
            tab: StartTab::Overview,
        };
        let cfg = Config::from_args(&args);
        assert_eq!(cfg.quick_node_urls, vec!["http://a:1", "http://b:2"]);
        assert_eq!(cfg.slow_node_urls, vec!["http://c:3"]);
    }

    #[test]
    fn refresh_floor_is_100ms() {
        let args = Args {
            quick_nodes: String::new(),
            slow_nodes: String::new(),
            cluster_key: None,
            refresh_ms: 1,
            tab: StartTab::Overview,
        };
        assert_eq!(Config::from_args(&args).refresh_ms, 100);
    }
}
