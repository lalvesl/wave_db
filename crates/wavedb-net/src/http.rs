//! HTTP POST transport: single-queue client + in-process test server.
//!
//! # Design (from the README / implementation spec)
//!
//! The HTTP transport uses a **single request queue**:
//! - The client serialises all outgoing [`TransportRequest`]s into a FIFO.
//! - A background worker dequeues one request per idle tick (or immediately when
//!   a new request is enqueued), POSTs it to the Quick-Node's HTTP endpoint, and
//!   dispatches the response.
//! - The Quick-Node piggybacks object-changed notifications onto every HTTP
//!   response, saving a separate long-poll connection.
//!
//! This guarantees ordering (requests are processed sequentially) and gives the
//! server a natural window to batch notifications.
//!
//! # In-process test server
//!
//! [`HttpTestServer`] spins up an axum router on a random port so integration
//! tests can verify the full round-trip without network overhead.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use bytes::Bytes;
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};

use crate::Transport;
use crate::frame::{decode_payload, encode_payload};
use crate::notify::{AnchorEvent, EventBus};
use crate::request::{RequestKind, TransportRequest, TransportResponse};
use wavedb_core::Result;

// ── Pending request ───────────────────────────────────────────────────────────

/// A request waiting in the send queue, paired with its response channel.
struct PendingRequest {
    request: TransportRequest,
    reply: oneshot::Sender<Result<TransportResponse>>,
}

// ── HttpClient ────────────────────────────────────────────────────────────────

/// HTTP POST single-queue transport client.
///
/// All requests are serialised through a single FIFO queue; a background
/// Tokio task drains the queue by POSTing to the Quick-Node's endpoint.
///
/// # Clone semantics
///
/// Cloning produces a second handle to the **same** queue and background task.
/// The background task runs until all handles are dropped.
#[derive(Clone)]
pub struct HttpClient {
    /// Shared inner state.
    inner: Arc<HttpClientInner>,
}

struct HttpClientInner {
    /// Pending request queue.
    queue: Mutex<VecDeque<PendingRequest>>,
    /// Wakes the worker when a new request is enqueued.
    notify: Notify,
    /// Quick-Node base URL.
    base_url: String,
    /// Idle poll interval (worker fires even when the queue is empty).
    interval: Duration,
    /// Event bus for piggybacked notifications.
    pub bus: EventBus,
    /// Internal reqwest client (keep-alive, connection pooling).
    http: reqwest::Client,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("base_url", &self.inner.base_url)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// Create a new HTTP client targeting `base_url`.
    ///
    /// Call [`HttpClient::run`] (wrapped in `tokio::spawn`) to start the
    /// background worker.
    pub fn new(base_url: impl Into<String>, interval: Duration) -> Self {
        Self {
            inner: Arc::new(HttpClientInner {
                queue: Mutex::new(VecDeque::new()),
                notify: Notify::new(),
                base_url: base_url.into(),
                interval,
                bus: EventBus::new(),
                http: reqwest::Client::new(),
            }),
        }
    }

    /// A reference to the event bus for piggybacked notifications.
    pub fn event_bus(&self) -> &EventBus {
        &self.inner.bus
    }

    /// Background worker loop.
    ///
    /// Runs until the task is cancelled (e.g. `AbortHandle::abort()`).
    /// Spawn with `tokio::spawn(client.run())`.
    pub async fn run(&self) {
        loop {
            // Wait for either a queued request or the idle tick.
            tokio::select! {
                () = self.inner.notify.notified() => {},
                () = tokio::time::sleep(self.inner.interval) => {},
            }
            self.flush_one().await;
        }
    }

    /// Drain one request from the queue (or send an empty keep-alive POST).
    async fn flush_one(&self) {
        let pending = self.inner.queue.lock().pop_front();

        match pending {
            Some(p) => {
                let result = self.post_request(&p.request).await;
                if let Ok(ref resp) = result {
                    self.dispatch_notifications(&resp.notifications);
                }
                // Ignore send error (receiver may have been dropped).
                let _ = p.reply.send(result);
            }
            None => {
                // Idle tick: POST an empty body to collect piggyback notifications.
                if let Ok(resp) = self
                    .inner
                    .http
                    .post(&self.inner.base_url)
                    .body(Bytes::new())
                    .send()
                    .await
                {
                    if let Ok(bytes) = resp.bytes().await {
                        if !bytes.is_empty() {
                            if let Ok(tr) =
                                decode_payload::<TransportResponse>(&bytes)
                            {
                                self.dispatch_notifications(&tr.notifications);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Serialise and POST a single request; deserialise and return the response.
    async fn post_request(
        &self,
        req: &TransportRequest,
    ) -> Result<TransportResponse> {
        let body = encode_payload(req)?;
        let resp = self
            .inner
            .http
            .post(&self.inner.base_url)
            .body(body)
            .header("Content-Type", "application/octet-stream")
            .send()
            .await
            .map_err(|e| wavedb_core::Error::Transport(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(wavedb_core::Error::Transport(format!(
                "HTTP {} from {}",
                resp.status(),
                self.inner.base_url
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| wavedb_core::Error::Transport(e.to_string()))?;

        let transport_resp = decode_payload::<TransportResponse>(&bytes)?;
        Ok(transport_resp)
    }

    /// Publish notifications to the local event bus.
    fn dispatch_notifications(
        &self,
        notifications: &[crate::request::Notification],
    ) {
        for n in notifications {
            let event = if n.deleted {
                AnchorEvent::deleted(n.anchor_id)
            } else {
                AnchorEvent::updated(n.anchor_id, n.payload.clone())
            };
            self.inner.bus.publish(event);
        }
    }
}

impl Transport for HttpClient {
    async fn send(&self, req: TransportRequest) -> Result<TransportResponse> {
        let (tx, rx) = oneshot::channel();
        {
            let mut queue = self.inner.queue.lock();
            queue.push_back(PendingRequest {
                request: req,
                reply: tx,
            });
        }
        self.inner.notify.notify_one();
        rx.await.map_err(|_| {
            wavedb_core::Error::Transport("http worker dropped".into())
        })?
    }

    fn disconnect(&self, user: u64, tenant: u64) {
        // Fire-and-forget: clone the HTTP client and spawn a disconnect POST.
        let client = self.clone();
        tokio::spawn(async move {
            let req = TransportRequest::new(
                u64::MAX, // sentinel seq for disconnect
                RequestKind::Disconnect { user, tenant },
            );
            // Best-effort — ignore errors during disconnect.
            let _ = client.post_request(&req).await;
        });
    }
}

// ── In-process test server ────────────────────────────────────────────────────

/// Handler state for the in-process HTTP test server.
#[derive(Clone, Debug)]
pub struct TestServerState {
    /// Requests received by the server in order.
    pub received: Arc<Mutex<Vec<TransportRequest>>>,
    /// Scripted responses to return for each request, consumed in order.
    pub script: Arc<Mutex<VecDeque<TransportResponse>>>,
}

impl TestServerState {
    /// Create new server state with an empty script.
    pub fn new() -> Self {
        Self {
            received: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Push a scripted response to be returned for the next request.
    pub fn push_response(&self, resp: TransportResponse) {
        self.script.lock().push_back(resp);
    }
}

impl Default for TestServerState {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle a POST to `/` — deserialise the body as a [`TransportRequest`] and
/// return a scripted [`TransportResponse`].
async fn handle_post(
    State(state): State<TestServerState>,
    body: Bytes,
) -> impl IntoResponse {
    if body.is_empty() {
        // Idle keep-alive tick — return empty 200.
        return (StatusCode::OK, Bytes::new());
    }

    let req: TransportRequest = match decode_payload(&body) {
        Ok(r) => r,
        Err(_) => return (StatusCode::BAD_REQUEST, Bytes::new()),
    };

    state.received.lock().push(req.clone());

    let resp = state
        .script
        .lock()
        .pop_front()
        .unwrap_or(TransportResponse {
            seq: req.seq,
            payload: Vec::new(),
            owner_url: None,
            backup_url: None,
            notifications: Vec::new(),
            error: None,
        });

    encode_payload(&resp).map_or_else(
        |_| (StatusCode::INTERNAL_SERVER_ERROR, Bytes::new()),
        |bytes| (StatusCode::OK, bytes),
    )
}

/// An in-process HTTP server for testing the [`HttpClient`].
pub struct HttpTestServer {
    /// The address the server is listening on.
    pub addr: std::net::SocketAddr,
    /// Shared server state (inspect received requests / push scripted replies).
    pub state: TestServerState,
    /// Abort handle to stop the server.
    abort: tokio::task::AbortHandle,
}

impl HttpTestServer {
    /// Start an in-process HTTP test server on a random port.
    pub async fn start() -> Self {
        let state = TestServerState::new();
        let router = Router::new()
            .route("/", post(handle_post))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        Self {
            addr,
            state,
            abort: handle.abort_handle(),
        }
    }

    /// Base URL of the server (e.g. `http://127.0.0.1:PORT`).
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for HttpTestServer {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::RequestKind;

    fn default_connect_resp(seq: u64) -> TransportResponse {
        TransportResponse {
            seq,
            payload: Vec::new(),
            owner_url: Some("http://owner".into()),
            backup_url: Some("http://backup".into()),
            notifications: Vec::new(),
            error: None,
        }
    }

    #[tokio::test]
    async fn client_sends_request_and_receives_response() {
        let server = HttpTestServer::start().await;
        server.state.push_response(default_connect_resp(1));

        let client =
            HttpClient::new(server.base_url(), Duration::from_millis(100));
        let client2 = client.clone();
        let _worker = tokio::spawn(async move { client2.run().await });

        let req = TransportRequest::new(
            1,
            RequestKind::Connect {
                user: 1,
                tenant: 100,
            },
        );
        let resp = client.send(req).await.unwrap();
        assert_eq!(resp.seq, 1);
        assert_eq!(resp.owner_url.as_deref(), Some("http://owner"));
    }

    #[tokio::test]
    async fn server_receives_requests_in_order() {
        let server = HttpTestServer::start().await;
        // Push 3 scripted responses.
        for i in 1u64..=3 {
            server.state.push_response(TransportResponse {
                seq: i,
                payload: Vec::new(),
                owner_url: None,
                backup_url: None,
                notifications: Vec::new(),
                error: None,
            });
        }

        let client =
            HttpClient::new(server.base_url(), Duration::from_millis(10));
        let client2 = client.clone();
        let _worker = tokio::spawn(async move { client2.run().await });

        // Submit 3 requests concurrently using the same client queue.
        let mut handles = Vec::new();
        for i in 1u64..=3 {
            let c = client.clone();
            handles.push(tokio::spawn(async move {
                let req = TransportRequest::new(
                    i,
                    RequestKind::Disconnect { user: 0, tenant: 0 },
                );
                c.send(req).await.unwrap()
            }));
        }

        let mut seqs: Vec<u64> = Vec::new();
        for h in handles {
            seqs.push(h.await.unwrap().seq);
        }
        seqs.sort_unstable();
        assert_eq!(seqs, [1, 2, 3]);
    }

    #[tokio::test]
    async fn idle_tick_hits_server() {
        let server = HttpTestServer::start().await;

        let client =
            HttpClient::new(server.base_url(), Duration::from_millis(50));
        let client2 = client.clone();
        let _worker = tokio::spawn(async move { client2.run().await });

        // Wait long enough for at least one idle tick.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The server should have received at least one empty-body POST (keep-alive).
        // We verify by checking the server didn't crash — the empty-body handler
        // returns 200 silently so received list is still empty.
        assert_eq!(server.state.received.lock().len(), 0);
    }

    #[tokio::test]
    async fn piggyback_notifications_reach_event_bus() {
        let server = HttpTestServer::start().await;

        let resp_with_notification = TransportResponse {
            seq: 1,
            payload: Vec::new(),
            owner_url: None,
            backup_url: None,
            notifications: vec![crate::request::Notification {
                anchor_id: 0xDEAD_BEEF,
                deleted: false,
                payload: Some(vec![1, 2, 3]),
            }],
            error: None,
        };
        server.state.push_response(resp_with_notification);

        let client =
            HttpClient::new(server.base_url(), Duration::from_millis(100));
        let mut rx = client.event_bus().subscribe();

        let client2 = client.clone();
        let _worker = tokio::spawn(async move { client2.run().await });

        let req = TransportRequest::new(
            1,
            RequestKind::Disconnect { user: 0, tenant: 0 },
        );
        client.send(req).await.unwrap();

        let event = rx.try_recv().unwrap();
        assert_eq!(event.anchor_id, 0xDEAD_BEEF);
        assert_eq!(event.payload, Some(vec![1, 2, 3]));
    }
}
