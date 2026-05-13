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

use wavedb_net::frame::{decode_payload, encode_payload};

use crate::flush::{FlushAck, FlushBatch, HistoryReadRequest, HistoryReadResponse};
use crate::store::HistoryStore;

// ── Router ────────────────────────────────────────────────────────────────────

pub fn router(store: HistoryStore) -> Router {
    Router::new()
        .route("/flush", post(handle_flush))
        .route("/history", post(handle_history))
        .with_state(Arc::new(store))
}

// ── Flush handler ─────────────────────────────────────────────────────────────

async fn handle_flush(
    State(store): State<Arc<HistoryStore>>,
    body: Bytes,
) -> impl IntoResponse {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, Bytes::new());
    }

    let batch: FlushBatch = match decode_payload(&body) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, Bytes::new()),
    };

    match store.apply_flush(batch) {
        Ok(write_seq) => {
            let ack = FlushAck { write_seq };
            encode_payload(&ack).map_or(
                (StatusCode::INTERNAL_SERVER_ERROR, Bytes::new()),
                |b| (StatusCode::OK, b),
            )
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Bytes::new()),
    }
}

// ── History handler ───────────────────────────────────────────────────────────

async fn handle_history(
    State(store): State<Arc<HistoryStore>>,
    body: Bytes,
) -> impl IntoResponse {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, Bytes::new());
    }

    let req: HistoryReadRequest = match decode_payload(&body) {
        Ok(r) => r,
        Err(_) => return (StatusCode::BAD_REQUEST, Bytes::new()),
    };

    let data = store.get(req.tenant, req.record_id).unwrap_or_default();
    let resp = HistoryReadResponse { data };
    encode_payload(&resp).map_or(
        (StatusCode::INTERNAL_SERVER_ERROR, Bytes::new()),
        |b| (StatusCode::OK, b),
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flush::FlushBatch;
    use tokio::net::TcpListener;
    use wavedb_storage::VersionedRecord;

    async fn start_server() -> (std::net::SocketAddr, tokio::task::AbortHandle) {
        let store = HistoryStore::in_memory();
        let app = router(store);
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
}
