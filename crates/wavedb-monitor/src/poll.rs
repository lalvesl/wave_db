//! Metrics polling: POST /metrics to each node and decode the response.

use wavedb_net::auth::TokenPurpose;
use wavedb_net::frame::{decode_payload, encode_payload};
use wavedb_net::metrics::{MetricsRequest, QuickNodeMetrics, SlowNodeMetrics};

use crate::config::Config;

/// Snapshot of all cluster metrics collected in a single poll cycle.
#[derive(Debug, Clone, Default)]
pub struct ClusterSnapshot {
    pub quick: Vec<NodeSnapshot>,
    pub slow: Option<SlowNodeMetrics>,
}

/// Per-Quick-Node metrics, annotated with whether the poll succeeded.
#[derive(Debug, Clone)]
pub struct NodeSnapshot {
    /// The URL this node was polled at.
    pub url: String,
    pub metrics: Option<QuickNodeMetrics>,
    /// `true` if the last poll returned an error (auth failure, timeout, etc.).
    pub error: bool,
}

/// Poll all nodes once and return a fresh snapshot.
pub async fn poll_all(cfg: &Config, client: &reqwest::Client) -> ClusterSnapshot {
    let mut quick = Vec::with_capacity(cfg.quick_node_urls.len());

    for url in &cfg.quick_node_urls {
        let snapshot = poll_quick(url, cfg, client).await;
        quick.push(snapshot);
    }

    let slow = poll_slow(&cfg.slow_node_url, cfg, client).await;

    ClusterSnapshot { quick, slow }
}

async fn poll_quick(url: &str, cfg: &Config, client: &reqwest::Client) -> NodeSnapshot {
    let req = build_request(cfg, TokenPurpose::Monitor);
    let body = match encode_payload(&req) {
        Ok(b) => b,
        Err(_) => {
            return NodeSnapshot {
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
            let metrics: Option<QuickNodeMetrics> = decode_payload(&bytes).ok();
            NodeSnapshot {
                url: url.to_string(),
                metrics,
                error: false,
            }
        }
        _ => NodeSnapshot {
            url: url.to_string(),
            metrics: None,
            error: true,
        },
    }
}

async fn poll_slow(url: &str, cfg: &Config, client: &reqwest::Client) -> Option<SlowNodeMetrics> {
    let req = build_request(cfg, TokenPurpose::Monitor);
    let body = encode_payload(&req).ok()?;

    let resp = client
        .post(format!("{url}/metrics"))
        .header("Content-Type", "application/octet-stream")
        .body(body.to_vec())
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let bytes = resp.bytes().await.ok()?;
    decode_payload(&bytes).ok()
}

fn build_request(cfg: &Config, purpose: TokenPurpose) -> MetricsRequest {
    let token = cfg
        .cluster_key
        .as_ref()
        .map(|k| k.mint(0, purpose));
    MetricsRequest { token }
}
