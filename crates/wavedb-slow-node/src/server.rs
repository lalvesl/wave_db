//! Axum HTTP server for the Slow-Node.
//!
//! Two endpoints:
//! - `POST /flush`   — receive a [`FlushBatch`] from a Quick-Node.
//! - `POST /history` — serve a [`HistoryReadRequest`] from any client.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use bytes::Bytes;

use wavedb_net::auth::{ClusterKey, TokenPurpose};
use wavedb_net::frame::{decode_payload, encode_payload};

use crate::flush::{FlushAck, FlushBatch, HistoryReadRequest, HistoryReadResponse};
use crate::store::HistoryStore;

// ── AppState ──────────────────────────────────────────────────────────────────

struct AppState {
    store: HistoryStore,
    auth_key: Option<ClusterKey>,
}

// ── Router ────────────────────────────────────────────────────────────────────

pub fn router(store: HistoryStore, auth_key: Option<ClusterKey>) -> Router {
    let state = Arc::new(AppState { store, auth_key });
    Router::new()
        .route("/flush", post(handle_flush))
        .route("/history", post(handle_history))
        .with_state(state)
}

// ── Flush handler ─────────────────────────────────────────────────────────────

async fn handle_flush(State(state): State<Arc<AppState>>, body: Bytes) -> impl IntoResponse {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, Bytes::new());
    }

    let batch: FlushBatch = match decode_payload(&body) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, Bytes::new()),
    };

    // Verify the sender's cluster membership token when a key is configured.
    if let Some(key) = &state.auth_key {
        let valid = batch
            .token
            .as_ref()
            .is_some_and(|t| key.verify(t, TokenPurpose::Flush));
        if !valid {
            return (StatusCode::FORBIDDEN, Bytes::new());
        }
    }

    state.store.apply_flush(batch).map_or_else(
        |_| (StatusCode::INTERNAL_SERVER_ERROR, Bytes::new()),
        |write_seq| {
            let ack = FlushAck { write_seq };
            encode_payload(&ack).map_or((StatusCode::INTERNAL_SERVER_ERROR, Bytes::new()), |b| {
                (StatusCode::OK, b)
            })
        },
    )
}

// ── History handler ───────────────────────────────────────────────────────────

async fn handle_history(State(state): State<Arc<AppState>>, body: Bytes) -> impl IntoResponse {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, Bytes::new());
    }

    let req: HistoryReadRequest = match decode_payload(&body) {
        Ok(r) => r,
        Err(_) => return (StatusCode::BAD_REQUEST, Bytes::new()),
    };

    let data = state
        .store
        .get(req.tenant, req.record_id)
        .unwrap_or_default();
    let resp = HistoryReadResponse { data };
    encode_payload(&resp).map_or((StatusCode::INTERNAL_SERVER_ERROR, Bytes::new()), |b| {
        (StatusCode::OK, b)
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flush::FlushBatch;
    use tokio::net::TcpListener;
    use wavedb_storage::VersionedRecord;

    async fn start_server() -> (std::net::SocketAddr, tokio::task::AbortHandle) {
        start_server_with_key(None).await
    }

    async fn start_server_with_key(
        auth_key: Option<ClusterKey>,
    ) -> (std::net::SocketAddr, tokio::task::AbortHandle) {
        let store = HistoryStore::in_memory();
        let app = router(store, auth_key);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, handle.abort_handle())
    }

    #[tokio::test]
    async fn flush_stores_and_returns_ack() {
        let (addr, _srv) = start_server().await;

        let batch = FlushBatch {
            write_seq: 3,
            tenant: 42,
            records: vec![VersionedRecord::new(100, b"hello".to_vec())],
            token: None,
        };
        let body = encode_payload(&batch).unwrap();

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/flush"))
            .header("Content-Type", "application/octet-stream")
            .body(body.to_vec())
            .send()
            .await
            .unwrap();

        assert!(resp.status().is_success());
        let bytes = resp.bytes().await.unwrap();
        let ack: FlushAck = decode_payload(&bytes).unwrap();
        assert_eq!(ack.write_seq, 3);
    }

    #[tokio::test]
    async fn history_read_returns_correct_record() {
        let (addr, _srv) = start_server().await;

        // Flush a record first.
        let batch = FlushBatch {
            write_seq: 1,
            tenant: 7,
            records: vec![VersionedRecord::new(55, b"the payload".to_vec())],
            token: None,
        };
        let flush_body = encode_payload(&batch).unwrap();
        reqwest::Client::new()
            .post(format!("http://{addr}/flush"))
            .header("Content-Type", "application/octet-stream")
            .body(flush_body.to_vec())
            .send()
            .await
            .unwrap();

        // Now read it back.
        let req = HistoryReadRequest {
            tenant: 7,
            record_id: 55,
        };
        let req_body = encode_payload(&req).unwrap();
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/history"))
            .header("Content-Type", "application/octet-stream")
            .body(req_body.to_vec())
            .send()
            .await
            .unwrap();

        assert!(resp.status().is_success());
        let bytes = resp.bytes().await.unwrap();
        let hr: HistoryReadResponse = decode_payload(&bytes).unwrap();
        assert!(!hr.data.is_empty());
        let rec = VersionedRecord::from_bytes(&hr.data).unwrap();
        assert_eq!(rec.data, b"the payload");
    }

    #[tokio::test]
    async fn history_read_unknown_returns_empty_data() {
        let (addr, _srv) = start_server().await;

        let req = HistoryReadRequest {
            tenant: 99,
            record_id: 1234,
        };
        let req_body = encode_payload(&req).unwrap();
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/history"))
            .header("Content-Type", "application/octet-stream")
            .body(req_body.to_vec())
            .send()
            .await
            .unwrap();

        assert!(resp.status().is_success());
        let bytes = resp.bytes().await.unwrap();
        let hr: HistoryReadResponse = decode_payload(&bytes).unwrap();
        assert!(hr.data.is_empty());
    }

    #[tokio::test]
    async fn flush_malformed_body_returns_400() {
        let (addr, _srv) = start_server().await;

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/flush"))
            .header("Content-Type", "application/octet-stream")
            .body(vec![0xDE, 0xAD, 0xBE, 0xEF])
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn flush_with_valid_token_succeeds() {
        let key = ClusterKey::from_hex(
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        )
        .unwrap();
        let (addr, _srv) = start_server_with_key(Some(key.clone())).await;

        let token = key.mint(42, TokenPurpose::Flush);
        let batch = FlushBatch {
            write_seq: 1,
            tenant: 42,
            records: vec![VersionedRecord::new(1, b"data".to_vec())],
            token: Some(token),
        };
        let body = encode_payload(&batch).unwrap();

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/flush"))
            .header("Content-Type", "application/octet-stream")
            .body(body.to_vec())
            .send()
            .await
            .unwrap();

        assert!(resp.status().is_success());
    }

    #[tokio::test]
    async fn flush_without_token_when_key_set_returns_403() {
        let key = ClusterKey::from_hex(
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        )
        .unwrap();
        let (addr, _srv) = start_server_with_key(Some(key)).await;

        let batch = FlushBatch {
            write_seq: 1,
            tenant: 42,
            records: vec![],
            token: None,
        };
        let body = encode_payload(&batch).unwrap();

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/flush"))
            .header("Content-Type", "application/octet-stream")
            .body(body.to_vec())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn flush_with_wrong_purpose_token_returns_403() {
        let key = ClusterKey::from_hex(
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        )
        .unwrap();
        let (addr, _srv) = start_server_with_key(Some(key.clone())).await;

        // Gossip token used against flush endpoint — must be rejected.
        let gossip_token = key.mint(42, TokenPurpose::Gossip);
        let batch = FlushBatch {
            write_seq: 1,
            tenant: 42,
            records: vec![],
            token: Some(gossip_token),
        };
        let body = encode_payload(&batch).unwrap();

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/flush"))
            .header("Content-Type", "application/octet-stream")
            .body(body.to_vec())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
    }
}
