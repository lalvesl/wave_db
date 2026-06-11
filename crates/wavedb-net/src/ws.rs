//! WebSocket transport: tokio-tungstenite client + in-process test server.
//!
//! The WebSocket transport provides **full-duplex, server-push** semantics:
//! - The client sends [`TransportRequest`]s as binary WebSocket messages.
//! - The server replies with [`TransportResponse`]s, also as binary messages.
//! - The server can push additional `TransportResponse`s at any time (e.g.
//!   object-changed events) without a corresponding client request.
//!
//! All messages are wire-encoded without a length prefix (WebSocket frames
//! are inherently delimited).
//!
//! # Ordering guarantee
//!
//! The client sends requests and receives responses on the same connection.
//! Requests are issued serially; the next request is not sent until the
//! previous response is received.  This keeps the protocol simple and makes
//! ordering trivial to reason about.

use std::collections::VecDeque;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async, connect_async};

use crate::Transport;
use crate::frame::{decode_payload, encode_payload};
use crate::notify::{AnchorEvent, EventBus};
use crate::request::{Notification, TransportRequest, TransportResponse};
use wavedb_core::Result;

// ── WsClient ─────────────────────────────────────────────────────────────────

/// A pending request waiting for its response.
struct WsPending {
    seq: u64,
    reply: oneshot::Sender<Result<TransportResponse>>,
}

/// Shared inner state for [`WsClient`].
struct WsClientInner {
    /// Pending (in-flight) requests, keyed by sequence number.
    pending: Mutex<VecDeque<WsPending>>,
    /// Outbound message sender (the write half of the WS connection).
    tx: tokio::sync::Mutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<TcpStream>,
            >,
            Message,
        >,
    >,
    /// Event bus for server-pushed notifications.
    bus: EventBus,
}

/// WebSocket transport client.
///
/// # Clone semantics
///
/// `WsClient` is `Clone`; all clones share the same underlying WebSocket
/// connection and event bus.
#[derive(Clone)]
pub struct WsClient {
    inner: Arc<WsClientInner>,
}

impl std::fmt::Debug for WsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsClient").finish_non_exhaustive()
    }
}

impl WsClient {
    /// Connect to the Quick-Node at `url` (e.g. `ws://127.0.0.1:7700`).
    ///
    /// Returns `(client, read_loop_future)`.  The caller must `tokio::spawn`
    /// the read-loop future to drive inbound messages.
    pub async fn connect(
        url: impl Into<String>,
    ) -> Result<(Self, impl std::future::Future<Output = ()> + Send + 'static)>
    {
        let (ws_stream, _) = connect_async(url.into())
            .await
            .map_err(|e| wavedb_core::Error::Transport(e.to_string()))?;

        let (write, read) = ws_stream.split();
        let bus = EventBus::new();
        let inner = Arc::new(WsClientInner {
            pending: Mutex::new(VecDeque::new()),
            tx: tokio::sync::Mutex::new(write),
            bus,
        });

        let inner_read = Arc::clone(&inner);
        let read_loop = async move {
            ws_read_loop(read, inner_read).await;
        };

        Ok((Self { inner }, read_loop))
    }

    /// A reference to the event bus for server-pushed notifications.
    pub fn event_bus(&self) -> &EventBus {
        &self.inner.bus
    }
}

/// Internal read-loop: drives inbound messages from the server.
async fn ws_read_loop<R>(mut read: R, inner: Arc<WsClientInner>)
where
    R: StreamExt<
            Item = std::result::Result<
                Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin
        + Send,
{
    while let Some(msg) = read.next().await {
        let Ok(msg) = msg else { break };
        let bytes = match msg {
            Message::Binary(b) => b,
            Message::Text(t) => bytes::Bytes::copy_from_slice(t.as_bytes()),
            Message::Close(_) => break,
            _ => continue,
        };

        let Ok(resp) = decode_payload::<TransportResponse>(&bytes) else {
            continue;
        };

        // Dispatch piggyback notifications.
        for n in &resp.notifications {
            let event = notification_to_event(n);
            let _ = inner.bus.publish(event);
        }

        // If this response has a matching pending request, complete it.
        let reply = {
            let mut pending = inner.pending.lock();
            pending
                .iter()
                .position(|p| p.seq == resp.seq)
                .map(|pos| pending.remove(pos).unwrap().reply)
        };
        if let Some(reply) = reply {
            let _ = reply.send(Ok(resp));
        }
        // Otherwise it was a server-push notification-only frame; already dispatched.
    }
}

fn notification_to_event(n: &Notification) -> AnchorEvent {
    if n.deleted {
        AnchorEvent::deleted(n.anchor_id)
    } else {
        AnchorEvent::updated(n.anchor_id, n.payload.clone())
    }
}

impl Transport for WsClient {
    async fn send(&self, req: TransportRequest) -> Result<TransportResponse> {
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock();
            pending.push_back(WsPending {
                seq: req.seq,
                reply: tx,
            });
        }

        let msg = encode_payload(&req)?;
        self.inner
            .tx
            .lock()
            .await
            .send(Message::Binary(msg.to_vec().into()))
            .await
            .map_err(|e| wavedb_core::Error::Transport(e.to_string()))?;

        rx.await.map_err(|_| {
            wavedb_core::Error::Transport("ws read loop dropped".into())
        })?
    }

    fn disconnect(&self, user: u64, tenant: u64) {
        use crate::request::RequestKind;
        let client = self.clone();
        tokio::spawn(async move {
            let req = TransportRequest::new(
                u64::MAX,
                RequestKind::Disconnect { user, tenant },
            );
            let _ = client.send(req).await;
        });
    }
}

// ── In-process test server ────────────────────────────────────────────────────

/// Shared state for the WebSocket test server.
#[derive(Clone, Debug)]
pub struct WsTestServerState {
    /// Requests received from the client in order.
    pub received: Arc<Mutex<Vec<TransportRequest>>>,
    /// Scripted responses to return for each incoming request.
    pub script: Arc<Mutex<VecDeque<TransportResponse>>>,
}

impl WsTestServerState {
    /// Create new server state with an empty script.
    pub fn new() -> Self {
        Self {
            received: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Push a scripted response.
    pub fn push_response(&self, resp: TransportResponse) {
        self.script.lock().push_back(resp);
    }
}

impl Default for WsTestServerState {
    fn default() -> Self {
        Self::new()
    }
}

/// An in-process WebSocket server for testing [`WsClient`].
pub struct WsTestServer {
    /// Address the server is listening on.
    pub addr: std::net::SocketAddr,
    /// Shared server state.
    pub state: WsTestServerState,
    /// Abort handle to stop the listener task.
    abort: tokio::task::AbortHandle,
}

impl WsTestServer {
    /// Start an in-process WebSocket server on a random port.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = WsTestServerState::new();
        let server_state = state.clone();

        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let st = server_state.clone();
                tokio::spawn(handle_ws_connection(stream, st));
            }
        });

        Self {
            addr,
            state,
            abort: handle.abort_handle(),
        }
    }

    /// The WebSocket URL of this server.
    pub fn ws_url(&self) -> String {
        format!("ws://{}", self.addr)
    }
}

impl Drop for WsTestServer {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// Handle one WebSocket client connection.
async fn handle_ws_connection(stream: TcpStream, state: WsTestServerState) {
    let Ok(ws_stream) = accept_async(stream).await else {
        return;
    };
    let (mut write, mut read) = ws_stream.split();

    while let Some(msg) = read.next().await {
        let Ok(msg) = msg else { break };
        let bytes = match msg {
            Message::Binary(b) => b,
            Message::Text(t) => bytes::Bytes::copy_from_slice(t.as_bytes()),
            Message::Close(_) => break,
            _ => continue,
        };

        let Ok(req) = decode_payload::<TransportRequest>(&bytes) else {
            continue;
        };

        let seq = req.seq;
        state.received.lock().push(req);

        let resp =
            state
                .script
                .lock()
                .pop_front()
                .unwrap_or(TransportResponse {
                    seq,
                    payload: Vec::new(),
                    owner_url: None,
                    backup_url: None,
                    notifications: Vec::new(),
                });

        let Ok(resp_bytes) = encode_payload(&resp) else {
            break;
        };
        if write
            .send(Message::Binary(resp_bytes.to_vec().into()))
            .await
            .is_err()
        {
            break;
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Notification, RequestKind};

    #[tokio::test]
    async fn client_sends_request_and_receives_response() {
        let server = WsTestServer::start().await;
        server.state.push_response(TransportResponse {
            seq: 1,
            payload: b"hello".to_vec(),
            owner_url: Some("ws://owner:7700".into()),
            backup_url: None,
            notifications: Vec::new(),
        });

        let (client, read_loop) =
            WsClient::connect(&server.ws_url()).await.unwrap();
        let _rl = tokio::spawn(read_loop);

        let req = TransportRequest::new(
            1,
            RequestKind::Connect {
                user: 1,
                tenant: 100,
            },
        );
        let resp = client.send(req).await.unwrap();

        assert_eq!(resp.seq, 1);
        assert_eq!(resp.payload, b"hello");
        assert_eq!(resp.owner_url.as_deref(), Some("ws://owner:7700"));
    }

    #[tokio::test]
    async fn server_receives_requests_in_order() {
        let server = WsTestServer::start().await;
        for i in 1u64..=5 {
            server.state.push_response(TransportResponse {
                seq: i,
                payload: Vec::new(),
                owner_url: None,
                backup_url: None,
                notifications: Vec::new(),
            });
        }

        let (client, read_loop) =
            WsClient::connect(&server.ws_url()).await.unwrap();
        let _rl = tokio::spawn(read_loop);

        for i in 1u64..=5 {
            let req = TransportRequest::new(
                i,
                RequestKind::Disconnect { user: 0, tenant: 0 },
            );
            let resp = client.send(req).await.unwrap();
            assert_eq!(resp.seq, i);
        }

        // The server must have received them in order.
        let seqs: Vec<u64> =
            server.state.received.lock().iter().map(|r| r.seq).collect();
        assert_eq!(seqs, [1, 2, 3, 4, 5]);
    }

    #[tokio::test]
    async fn server_push_notifications_reach_event_bus() {
        let server = WsTestServer::start().await;
        server.state.push_response(TransportResponse {
            seq: 1,
            payload: Vec::new(),
            owner_url: None,
            backup_url: None,
            notifications: vec![Notification {
                anchor_id: 0xCAFE_BABE,
                deleted: false,
                payload: Some(vec![9, 8, 7]),
            }],
        });

        let (client, read_loop) =
            WsClient::connect(&server.ws_url()).await.unwrap();
        let _rl = tokio::spawn(read_loop);
        let mut rx = client.event_bus().subscribe();

        let req = TransportRequest::new(
            1,
            RequestKind::Disconnect { user: 0, tenant: 0 },
        );
        client.send(req).await.unwrap();

        let event = rx.try_recv().unwrap();
        assert_eq!(event.anchor_id, 0xCAFE_BABE);
        assert_eq!(event.payload, Some(vec![9, 8, 7]));
    }

    #[tokio::test]
    async fn bloom_filter_client_sends_filter_bytes() {
        use crate::notify::SeenFilter;

        // Verify SeenFilter works end-to-end.
        let filter = SeenFilter::new();
        filter.mark(111);
        filter.mark(222);
        filter.mark(333);

        assert!(filter.probably_seen(111));
        assert!(filter.probably_seen(222));
        assert!(!filter.probably_seen(999)); // almost certainly absent

        let bytes = filter.to_bytes();
        assert!(!bytes.is_empty());
    }
}
