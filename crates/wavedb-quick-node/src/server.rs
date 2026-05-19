//! Axum HTTP + WebSocket server for the Quick-Node.
//!
//! Endpoints:
//! - `POST /`       — HTTP single-queue transport with piggyback notifications.
//! - `GET /ws`      — WebSocket upgrade for full-duplex, push-capable comms.
//! - `POST /gossip` — Peer-to-peer gossip (join / withdraw announcements).
//! - `POST /drain`  — Operator command: start a graceful drain of this node.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};

use wavedb_net::auth::TokenPurpose;
use wavedb_net::frame::{decode_payload, encode_payload};
use wavedb_net::metrics::MetricsRequest;
use wavedb_net::request::{TransportRequest, TransportResponse};

use crate::gossip::{GossipMessage, GossipResponse};
use crate::node::QuickNode;

// ── Router ────────────────────────────────────────────────────────────────────

/// Build the axum [`Router`] for a Quick-Node.
///
/// The returned router is ready to serve via `axum::serve`.
pub fn router(node: QuickNode) -> Router {
    Router::new()
        .route("/", post(handle_http))
        .route("/ws", get(handle_ws_upgrade))
        .route("/gossip", post(handle_gossip))
        .route("/drain", post(handle_drain))
        .route("/metrics", post(handle_metrics))
        .with_state(Arc::new(node))
}

// ── HTTP handler ──────────────────────────────────────────────────────────────

async fn handle_http(State(node): State<Arc<QuickNode>>, body: Bytes) -> impl IntoResponse {
    if body.is_empty() {
        // Idle keep-alive tick: return empty 200 so the client can collect
        // any piggybacked notifications queued on the node.
        return (StatusCode::OK, Bytes::new());
    }

    let req: TransportRequest = match decode_payload(&body) {
        Ok(r) => r,
        Err(_) => return (StatusCode::BAD_REQUEST, Bytes::new()),
    };

    let resp: TransportResponse = node.handle(req).await;
    encode_payload(&resp).map_or((StatusCode::INTERNAL_SERVER_ERROR, Bytes::new()), |b| {
        (StatusCode::OK, b)
    })
}

// ── WebSocket handler ─────────────────────────────────────────────────────────

async fn handle_ws_upgrade(
    ws: WebSocketUpgrade,
    State(node): State<Arc<QuickNode>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws_session(socket, node))
}

async fn handle_ws_session(socket: WebSocket, node: Arc<QuickNode>) {
    let (mut sink, mut stream) = socket.split();

    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else { break };

        let bytes: Bytes = match msg {
            Message::Binary(b) => b,
            Message::Text(t) => Bytes::copy_from_slice(t.as_bytes()),
            Message::Close(_) => break,
            _ => continue,
        };

        let Ok(req) = decode_payload::<TransportRequest>(&bytes) else {
            continue;
        };

        let resp = node.handle(req).await;

        let Ok(resp_bytes) = encode_payload(&resp) else {
            break;
        };

        if sink.send(Message::Binary(resp_bytes)).await.is_err() {
            break;
        }
    }
}

// ── Gossip handler ────────────────────────────────────────────────────────────

async fn handle_gossip(State(node): State<Arc<QuickNode>>, body: Bytes) -> impl IntoResponse {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, Bytes::new());
    }
    let msg: GossipMessage = match decode_payload(&body) {
        Ok(m) => m,
        Err(_) => return (StatusCode::BAD_REQUEST, Bytes::new()),
    };

    // Reject unsigned messages when a cluster key is configured.
    if let Some(key) = node.auth_key() {
        let valid = msg
            .token
            .as_ref()
            .is_some_and(|t| key.verify(t, TokenPurpose::Gossip));
        if !valid {
            return (StatusCode::FORBIDDEN, Bytes::new());
        }
    }

    let resp: GossipResponse = node.handle_gossip(msg).await;
    encode_payload(&resp).map_or((StatusCode::INTERNAL_SERVER_ERROR, Bytes::new()), |b| {
        (StatusCode::OK, b)
    })
}

// ── Drain handler ─────────────────────────────────────────────────────────────

/// `POST /drain` — trigger a graceful drain of this Quick-Node.
///
/// Sets the draining flag, syncs to the Slow-Node, and fans out a Withdraw
/// gossip to all peers.  Returns 200 OK once the withdraw gossip is sent;
/// the operator should then wait for connected clients to redirect before
/// terminating the process.
async fn handle_drain(State(node): State<Arc<QuickNode>>) -> impl IntoResponse {
    node.drain().await;
    StatusCode::OK
}

// ── Metrics handler ───────────────────────────────────────────────────────────

async fn handle_metrics(State(node): State<Arc<QuickNode>>, body: Bytes) -> impl IntoResponse {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, Bytes::new());
    }
    let req: MetricsRequest = match decode_payload(&body) {
        Ok(r) => r,
        Err(_) => return (StatusCode::BAD_REQUEST, Bytes::new()),
    };

    if let Some(key) = node.auth_key() {
        let valid = req
            .token
            .as_ref()
            .is_some_and(|t| key.verify(t, TokenPurpose::Monitor));
        if !valid {
            return (StatusCode::FORBIDDEN, Bytes::new());
        }
    }

    let snapshot = node.metrics();
    encode_payload(&snapshot).map_or((StatusCode::INTERNAL_SERVER_ERROR, Bytes::new()), |b| {
        (StatusCode::OK, b)
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, OwnershipSpec};
    use tokio::net::TcpListener;
    use wavedb_net::Transport;
    use wavedb_net::request::RequestKind;

    fn make_node() -> QuickNode {
        QuickNode::new(Config {
            listen: "127.0.0.1:0".into(),
            peers: String::new(),
            slow_node: None,
            owns: vec![OwnershipSpec {
                tenant: 42,
                shard_start: 0,
                shard_end: 4095,
            }],
            bloom_interval_secs: 1,
            journal_compact_secs: 0,
            // Empty path → in-memory storage; tests don't assert disk state.
            data_dir: std::path::PathBuf::new(),
            cluster_key: None,
        })
    }

    async fn start_server() -> (std::net::SocketAddr, tokio::task::AbortHandle) {
        let node = make_node();
        let app = router(node);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, handle.abort_handle())
    }

    #[tokio::test]
    async fn http_connect_returns_owner_url() {
        let (addr, _srv) = start_server().await;

        let req = TransportRequest::new(
            1,
            RequestKind::Connect {
                user: 1,
                tenant: 42,
            },
        );
        let body = encode_payload(&req).unwrap();

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/"))
            .header("Content-Type", "application/octet-stream")
            .body(body.to_vec())
            .send()
            .await
            .unwrap();

        assert!(resp.status().is_success());
        let bytes = resp.bytes().await.unwrap();
        let tr: TransportResponse = decode_payload(&bytes).unwrap();
        assert_eq!(tr.seq, 1);
        assert!(tr.owner_url.is_some());
    }

    #[tokio::test]
    async fn http_idle_tick_returns_empty_200() {
        let (addr, _srv) = start_server().await;

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/"))
            .body(Vec::<u8>::new())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.bytes().await.unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn http_malformed_body_returns_400() {
        let (addr, _srv) = start_server().await;

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/"))
            .header("Content-Type", "application/octet-stream")
            .body(vec![0xDE, 0xAD, 0xBE, 0xEF])
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ws_connect_roundtrip() {
        let (addr, _srv) = start_server().await;

        // Brief pause so the server task has a chance to start.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let (client, rl) = wavedb_net::WsClient::connect(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let _rl = tokio::spawn(rl);

        let req = TransportRequest::new(
            1,
            RequestKind::Connect {
                user: 1,
                tenant: 42,
            },
        );
        let resp = client.send(req).await.unwrap();

        assert_eq!(resp.seq, 1);
        assert!(resp.owner_url.is_some());
    }

    #[tokio::test]
    async fn ws_write_owned_partition_succeeds() {
        let (addr, _srv) = start_server().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let (client, rl) = wavedb_net::WsClient::connect(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let _rl = tokio::spawn(rl);

        let req = TransportRequest::new(
            2,
            RequestKind::Write {
                struct_id: 1,
                user: 1,
                tenant: 42,
                payload: b"hello".to_vec(),
            },
        );
        let resp = client.send(req).await.unwrap();
        assert_eq!(resp.seq, 2);
        assert_ne!(resp.payload, b"not_owner" as &[u8]);
    }

    #[tokio::test]
    async fn ws_write_unowned_partition_returns_not_owner() {
        let (addr, _srv) = start_server().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let (client, rl) = wavedb_net::WsClient::connect(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let _rl = tokio::spawn(rl);

        let req = TransportRequest::new(
            3,
            RequestKind::Write {
                struct_id: 1,
                user: 1,
                tenant: 99, // not owned
                payload: Vec::new(),
            },
        );
        let resp = client.send(req).await.unwrap();
        assert_eq!(resp.payload, b"not_owner");
    }
}
