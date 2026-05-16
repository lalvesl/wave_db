//! Gossip protocol for dynamic peer discovery and graceful drain.
//!
//! # Join flow
//!
//! When a Quick-Node starts it sends [`GossipKind::Announce`] to every peer
//! listed in `--peers`.  Each receiver:
//!
//! 1. Inserts the new node into its consistent ring.
//! 2. Fans the message one hop to its own peers (deduped by `(origin, epoch)`
//!    so the flood terminates in one round).
//! 3. Replies with [`GossipResponse::NodeList`] so the joining node learns
//!    the full cluster topology in a single round-trip.
//!
//! # Drain flow
//!
//! An operator POSTs to `/drain`.  The node:
//!
//! 1. Sets an internal `draining` flag — all data requests immediately return
//!    a redirect hint so connected clients reconnect elsewhere.
//! 2. Syncs in-memory writes to the Slow-Node (Phase 14 hook; no-op until
//!    the storage engine is wired).
//! 3. Sends [`GossipKind::Withdraw`] to all known peers so they remove this
//!    node from their rings before it stops accepting connections.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use wavedb_net::auth::NodeToken;

use crate::ring::NodeId;

// ── Wire types ────────────────────────────────────────────────────────────────

/// A gossip message exchanged between Quick-Nodes via `POST /gossip`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipMessage {
    /// Per-node monotonically increasing epoch.  Together with `origin` it
    /// forms the dedup key `(origin, epoch)` — each pair is processed at most
    /// once per receiving node.
    pub epoch: u64,
    /// Stable [`NodeId`] of the node that originated this message.
    pub origin: NodeId,
    /// Listen address of the originating node — `host:port`, no scheme.
    pub addr: String,
    /// What this message communicates.
    pub kind: GossipKind,
    /// HMAC-SHA256 cluster membership proof. `None` when the cluster runs
    /// without a shared key (open / dev mode).
    pub token: Option<NodeToken>,
}

/// Kind of gossip event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipKind {
    /// Node at [`GossipMessage::addr`] joined the cluster.
    Announce,
    /// Node at [`GossipMessage::addr`] is draining and will stop serving.
    Withdraw,
}

/// Response the receiving node sends back to the gossip originator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipResponse {
    /// Simple acknowledgement (returned for [`GossipKind::Withdraw`]).
    Ack,
    /// Returned for [`GossipKind::Announce`] so the joining node bootstraps
    /// its ring without a second round-trip per discovered peer.
    NodeList {
        /// All `(node_id, listen_addr)` pairs currently in the receiver's ring.
        nodes: Vec<(NodeId, String)>,
    },
}

// ── GossipState ───────────────────────────────────────────────────────────────

/// Per-node gossip bookkeeping: outgoing epoch counter and received-event dedup
/// set.
#[derive(Debug, Clone, Default)]
pub struct GossipState {
    seen: Arc<Mutex<HashSet<(NodeId, u64)>>>,
    epoch: Arc<AtomicU64>,
}

impl GossipState {
    /// Create a fresh state (epoch starts at 0; no events seen yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint the next local epoch for an outgoing gossip message.
    pub fn next_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Mark `(origin, epoch)` as seen and return `true` if it was novel.
    /// Returns `false` for duplicates — the caller should drop the message.
    pub fn mark_seen(&self, origin: NodeId, epoch: u64) -> bool {
        self.seen.lock().insert((origin, epoch))
    }
}

// ── GossipClient ─────────────────────────────────────────────────────────────

/// HTTP client for sending gossip messages to peer Quick-Nodes.
///
/// All errors are soft: a failed send is logged and ignored so that a
/// temporarily unreachable peer never blocks the caller.
pub struct GossipClient {
    pub(crate) http: reqwest::Client,
}

impl std::fmt::Debug for GossipClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GossipClient").finish_non_exhaustive()
    }
}

impl GossipClient {
    /// Create a client with a 5-second per-request timeout.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
        }
    }

    /// POST `msg` to the Quick-Node at `addr` and decode the
    /// [`GossipResponse`].  Returns `None` on any transport or decode error.
    pub async fn send(&self, addr: &str, msg: &GossipMessage) -> Option<GossipResponse> {
        let url = format!("http://{addr}/gossip");
        let body = postcard::to_allocvec(msg).ok()?;
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/octet-stream")
            .body(body)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        postcard::from_bytes::<GossipResponse>(&bytes).ok()
    }
}

impl Default for GossipClient {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gossip_state_dedup() {
        let gs = GossipState::new();
        assert!(gs.mark_seen(1, 1));
        assert!(!gs.mark_seen(1, 1)); // duplicate
        assert!(gs.mark_seen(1, 2)); // same node, new epoch
        assert!(gs.mark_seen(2, 1)); // different node
    }

    #[test]
    fn epoch_is_monotone() {
        let gs = GossipState::new();
        let a = gs.next_epoch();
        let b = gs.next_epoch();
        assert!(b > a);
    }

    #[test]
    fn gossip_message_serde_roundtrip() {
        let msg = GossipMessage {
            epoch: 7,
            origin: 42,
            addr: "10.0.0.1:7700".into(),
            kind: GossipKind::Announce,
            token: None,
        };
        let bytes = postcard::to_allocvec(&msg).unwrap();
        let decoded: GossipMessage = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.epoch, 7);
        assert_eq!(decoded.origin, 42);
        assert!(matches!(decoded.kind, GossipKind::Announce));
    }

    #[test]
    fn gossip_response_node_list_serde() {
        let resp = GossipResponse::NodeList {
            nodes: vec![(1, "a:7700".into()), (2, "b:7700".into())],
        };
        let bytes = postcard::to_allocvec(&resp).unwrap();
        let decoded: GossipResponse = postcard::from_bytes(&bytes).unwrap();
        if let GossipResponse::NodeList { nodes } = decoded {
            assert_eq!(nodes.len(), 2);
        } else {
            panic!("wrong variant");
        }
    }
}
