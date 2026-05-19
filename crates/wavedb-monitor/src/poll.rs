//! Metrics polling: POST /metrics to each node and decode the response.

use futures_util::future;
use wavedb_net::auth::TokenPurpose;
use wavedb_net::frame::{decode_payload, encode_payload};
use wavedb_net::metrics::{MetricsRequest, QuickNodeMetrics, SlowNodeMetrics};

use crate::config::Config;

/// Unified entry for a node in the cluster — quick or slow.
#[derive(Debug, Clone)]
pub enum NodeEntry {
    Quick {
        url: String,
        metrics: Option<QuickNodeMetrics>,
        error: bool,
    },
    Slow {
        url: String,
        metrics: Option<SlowNodeMetrics>,
        error: bool,
    },
}

impl NodeEntry {
    /// URL this node was polled at.
    pub fn url(&self) -> &str {
        match self {
            Self::Quick { url, .. } | Self::Slow { url, .. } => url,
        }
    }

    /// `true` if the last poll failed.
    pub const fn error(&self) -> bool {
        match self {
            Self::Quick { error, .. } | Self::Slow { error, .. } => *error,
        }
    }
}

/// Snapshot of all cluster metrics collected in a single poll cycle.
///
/// Quick nodes appear first (in config order), slow nodes follow.
#[derive(Debug, Clone, Default)]
pub struct ClusterSnapshot {
    pub nodes: Vec<NodeEntry>,
}

/// Poll all nodes once and return a fresh snapshot.
///
/// All quick-node and slow-node polls run concurrently so the total wall
/// time is one round-trip, not `N × round-trip`.
pub async fn poll_all(cfg: &Config, client: &reqwest::Client) -> ClusterSnapshot {
    let quick_futs = cfg
        .quick_node_urls
        .iter()
        .map(|url| poll_quick(url, cfg, client));
    let slow_futs = cfg
        .slow_node_urls
        .iter()
        .map(|url| poll_slow(url, cfg, client));

    let (quick_results, slow_results) = tokio::join!(
        future::join_all(quick_futs),
        future::join_all(slow_futs),
    );

    let nodes = quick_results.into_iter().chain(slow_results).collect();
    ClusterSnapshot { nodes }
}

async fn poll_quick(url: &str, cfg: &Config, client: &reqwest::Client) -> NodeEntry {
    let req = build_request(cfg, TokenPurpose::Monitor);
    let Ok(body) = encode_payload(&req) else {
        return NodeEntry::Quick {
            url: url.to_string(),
            metrics: None,
            error: true,
        };
    };

    let result = client
        .post(format!("{url}/metrics"))
        .header("Content-Type", "application/octet-stream")
        .body(body.to_vec())
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            let bytes = resp.bytes().await.unwrap_or_default();
            NodeEntry::Quick {
                url: url.to_string(),
                metrics: decode_payload(&bytes).ok(),
                error: false,
            }
        }
        Ok(resp) => {
            eprintln!("[monitor] metrics {url}: HTTP {}", resp.status());
            NodeEntry::Quick {
                url: url.to_string(),
                metrics: None,
                error: true,
            }
        }
        Err(e) => {
            let mut msg = format!("[monitor] quick {url}: {e}");
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                msg.push_str(&format!(" | {s}"));
                src = std::error::Error::source(s);
            }
            eprintln!("{msg}");
            NodeEntry::Quick {
                url: url.to_string(),
                metrics: None,
                error: true,
            }
        }
    }
}

async fn poll_slow(url: &str, cfg: &Config, client: &reqwest::Client) -> NodeEntry {
    let req = build_request(cfg, TokenPurpose::Monitor);
    let Ok(body) = encode_payload(&req) else {
        return NodeEntry::Slow {
            url: url.to_string(),
            metrics: None,
            error: true,
        };
    };

    let result = client
        .post(format!("{url}/metrics"))
        .header("Content-Type", "application/octet-stream")
        .body(body.to_vec())
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            let bytes = resp.bytes().await.unwrap_or_default();
            NodeEntry::Slow {
                url: url.to_string(),
                metrics: decode_payload(&bytes).ok(),
                error: false,
            }
        }
        Ok(resp) => {
            eprintln!("[monitor] metrics {url}: HTTP {}", resp.status());
            NodeEntry::Slow {
                url: url.to_string(),
                metrics: None,
                error: true,
            }
        }
        Err(e) => {
            let mut msg = format!("[monitor] slow {url}: {e}");
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                msg.push_str(&format!(" | {s}"));
                src = std::error::Error::source(s);
            }
            eprintln!("{msg}");
            NodeEntry::Slow {
                url: url.to_string(),
                metrics: None,
                error: true,
            }
        }
    }
}

fn build_request(cfg: &Config, purpose: TokenPurpose) -> MetricsRequest {
    let token = cfg.cluster_key.as_ref().map(|k| k.mint(0, purpose));
    MetricsRequest { token }
}
