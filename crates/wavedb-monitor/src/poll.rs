//! Metrics polling: POST /metrics to each node and decode the response.

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
            NodeEntry::Quick { url, .. } | NodeEntry::Slow { url, .. } => url,
        }
    }

    /// `true` if the last poll failed.
    pub fn error(&self) -> bool {
        match self {
            NodeEntry::Quick { error, .. } | NodeEntry::Slow { error, .. } => *error,
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
pub async fn poll_all(cfg: &Config, client: &reqwest::Client) -> ClusterSnapshot {
    let mut nodes = Vec::with_capacity(cfg.quick_node_urls.len() + cfg.slow_node_urls.len());

    for url in &cfg.quick_node_urls {
        nodes.push(poll_quick(url, cfg, client).await);
    }
    for url in &cfg.slow_node_urls {
        nodes.push(poll_slow(url, cfg, client).await);
    }

    ClusterSnapshot { nodes }
}

async fn poll_quick(url: &str, cfg: &Config, client: &reqwest::Client) -> NodeEntry {
    let req = build_request(cfg, TokenPurpose::Monitor);
    let body = match encode_payload(&req) {
        Ok(b) => b,
        Err(_) => {
            return NodeEntry::Quick {
                url: url.to_string(),
                metrics: None,
                error: true,
            };
        }
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
        _ => NodeEntry::Quick {
            url: url.to_string(),
            metrics: None,
            error: true,
        },
    }
}

async fn poll_slow(url: &str, cfg: &Config, client: &reqwest::Client) -> NodeEntry {
    let req = build_request(cfg, TokenPurpose::Monitor);
    let body = match encode_payload(&req) {
        Ok(b) => b,
        Err(_) => {
            return NodeEntry::Slow {
                url: url.to_string(),
                metrics: None,
                error: true,
            };
        }
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
        _ => NodeEntry::Slow {
            url: url.to_string(),
            metrics: None,
            error: true,
        },
    }
}

fn build_request(cfg: &Config, purpose: TokenPurpose) -> MetricsRequest {
    let token = cfg.cluster_key.as_ref().map(|k| k.mint(0, purpose));
    MetricsRequest { token }
}
