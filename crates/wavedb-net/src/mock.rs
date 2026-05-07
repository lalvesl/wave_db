//! In-process mock transport for unit-testing the `Db` client layer.
//!
//! [`MockTransport`] records every request sent and replies from a
//! user-provided scripted state machine.  Tests can inspect the log to
//! verify that `Db` sends the expected messages and reacts correctly to
//! server responses (including failover and disconnect).

use crate::Transport;
use crate::request::{TransportRequest, TransportResponse};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use wavedb_core::Result;

/// A scripted reply to enqueue in a [`MockTransport`].
#[derive(Debug, Clone)]
pub struct ScriptedReply {
    /// The payload the mock server returns.
    pub payload: Vec<u8>,
    /// Whether to simulate an error (transport-level failure).
    pub error: bool,
    /// Owner URL to include in a `Connect` response.
    pub owner_url: Option<String>,
    /// Backup URL to include in a `Connect` response.
    pub backup_url: Option<String>,
}

impl ScriptedReply {
    /// A simple successful reply with the given payload.
    pub const fn ok(payload: Vec<u8>) -> Self {
        Self {
            payload,
            error: false,
            owner_url: None,
            backup_url: None,
        }
    }

    /// A successful `Connect` reply with owner and backup URLs.
    pub fn connect(owner_url: impl Into<String>, backup_url: impl Into<String>) -> Self {
        Self {
            payload: Vec::new(),
            error: false,
            owner_url: Some(owner_url.into()),
            backup_url: Some(backup_url.into()),
        }
    }

    /// A reply that simulates a transport-level error.
    pub const fn err() -> Self {
        Self {
            payload: Vec::new(),
            error: true,
            owner_url: None,
            backup_url: None,
        }
    }
}

/// Shared state inside a [`MockTransport`].
#[derive(Debug, Default)]
struct Inner {
    /// Pre-programmed replies, consumed in order.
    script: VecDeque<ScriptedReply>,
    /// All requests sent through this transport, in order.
    log: Vec<TransportRequest>,
    /// All `disconnect` calls (user, tenant) received.
    disconnects: Vec<(u64, u64)>,
}

/// An in-process mock transport.
///
/// # Usage
///
/// ```rust,ignore
/// let mock = MockTransport::new();
/// mock.push(ScriptedReply::connect("ws://owner:7700", "ws://backup:7700"));
/// mock.push(ScriptedReply::ok(postcard::to_allocvec(&None::<()>).unwrap()));
///
/// let db = Db::open_with_mock("ws://owner:7700", 1, mock.clone()).await?;
/// let _: Option<MyStruct> = MyStruct::search(&db).await?;
///
/// assert_eq!(mock.log().len(), 2);
/// assert_eq!(mock.disconnects().len(), 0);
/// ```
#[derive(Debug, Clone)]
pub struct MockTransport {
    inner: Arc<Mutex<Inner>>,
}

impl MockTransport {
    /// Create a new mock with an empty script.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// Append a scripted reply to the queue.
    pub fn push(&self, reply: ScriptedReply) {
        self.inner.lock().script.push_back(reply);
    }

    /// Returns a snapshot of all requests sent through this transport.
    pub fn log(&self) -> Vec<TransportRequest> {
        self.inner.lock().log.clone()
    }

    /// Returns all disconnect calls `(user, tenant)` received so far.
    pub fn disconnects(&self) -> Vec<(u64, u64)> {
        self.inner.lock().disconnects.clone()
    }

    /// Returns how many scripted replies remain unconsumed.
    pub fn remaining(&self) -> usize {
        self.inner.lock().script.len()
    }
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for MockTransport {
    async fn send(&self, req: TransportRequest) -> Result<TransportResponse> {
        let reply = {
            let mut guard = self.inner.lock();
            guard.log.push(req.clone());
            guard.script.pop_front()
        };

        let Some(reply) = reply else {
            return Err(wavedb_core::Error::Transport(
                "MockTransport: no scripted reply remaining".into(),
            ));
        };

        if reply.error {
            return Err(wavedb_core::Error::Transport(
                "MockTransport: scripted error".into(),
            ));
        }

        Ok(TransportResponse {
            seq: req.seq,
            payload: reply.payload,
            owner_url: reply.owner_url,
            backup_url: reply.backup_url,
            notifications: Vec::new(),
        })
    }

    fn disconnect(&self, user: u64, tenant: u64) {
        self.inner.lock().disconnects.push((user, tenant));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::RequestKind;

    #[tokio::test]
    async fn records_requests_in_order() {
        let mock = MockTransport::new();
        mock.push(ScriptedReply::ok(vec![1, 2, 3]));
        mock.push(ScriptedReply::ok(vec![4, 5, 6]));

        let resp1 = mock
            .send(TransportRequest::new(
                1,
                RequestKind::Disconnect { user: 0, tenant: 0 },
            ))
            .await
            .unwrap();
        assert_eq!(resp1.seq, 1);
        assert_eq!(resp1.payload, [1, 2, 3]);

        let resp2 = mock
            .send(TransportRequest::new(
                2,
                RequestKind::Disconnect { user: 0, tenant: 0 },
            ))
            .await
            .unwrap();
        assert_eq!(resp2.seq, 2);
        assert_eq!(resp2.payload, [4, 5, 6]);

        assert_eq!(mock.log().len(), 2);
    }

    #[tokio::test]
    async fn scripted_error_propagates() {
        let mock = MockTransport::new();
        mock.push(ScriptedReply::err());

        let result = mock
            .send(TransportRequest::new(
                1,
                RequestKind::Disconnect { user: 0, tenant: 0 },
            ))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn no_reply_returns_error() {
        let mock = MockTransport::new();
        let result = mock
            .send(TransportRequest::new(
                1,
                RequestKind::Disconnect { user: 0, tenant: 0 },
            ))
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn disconnect_is_recorded() {
        let mock = MockTransport::new();
        mock.disconnect(42, 100);
        mock.disconnect(7, 200);
        let disc = mock.disconnects();
        assert_eq!(disc.len(), 2);
        assert_eq!(disc[0], (42, 100));
        assert_eq!(disc[1], (7, 200));
    }

    #[tokio::test]
    async fn connect_reply_carries_urls() {
        let mock = MockTransport::new();
        mock.push(ScriptedReply::connect(
            "ws://owner:7700",
            "ws://backup:7700",
        ));

        let resp = mock
            .send(TransportRequest::new(
                1,
                RequestKind::Connect { user: 1, tenant: 1 },
            ))
            .await
            .unwrap();

        assert_eq!(resp.owner_url.as_deref(), Some("ws://owner:7700"));
        assert_eq!(resp.backup_url.as_deref(), Some("ws://backup:7700"));
    }
}
