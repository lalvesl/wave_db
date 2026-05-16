//! `WaveDB` network transport layer.
//!
//! Provides the [`Transport`] trait and concrete implementations:
//!
//! | Module | Implementation |
//! |--------|---------------|
//! | [`mock`] | In-process scripted transport for unit tests |
//! | [`http`] | HTTP POST single-queue client + piggyback notifications |
//! | [`ws`]   | WebSocket full-duplex client (server-push capable) |
//! | [`notify`] | Bloom-filter event bus for object-changed events |
//! | [`frame`]  | Postcard-based framing helpers |

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod auth;
pub mod frame;
pub mod metrics;
pub mod mock;
pub mod request;

// Native-only modules (tokio / tungstenite / axum / reqwest).
#[cfg(feature = "native")]
pub mod http;
#[cfg(feature = "native")]
pub mod notify;
#[cfg(feature = "native")]
pub mod ws;

pub use auth::{ClusterKey, NodeToken, TokenPurpose};
#[cfg(feature = "native")]
pub use http::HttpClient;
pub use mock::MockTransport;
#[cfg(feature = "native")]
pub use mock::{ChannelTransport, MockServer};
#[cfg(feature = "native")]
pub use notify::EventBus;
pub use request::{TransportRequest, TransportResponse};
#[cfg(feature = "native")]
pub use ws::WsClient;

use std::future::Future;
use wavedb_core::Result;

/// The transport abstraction used by [`wavedb::Db`].
///
/// In production this resolves to an HTTP or WebSocket client.
/// In tests it resolves to [`MockTransport`].
pub trait Transport: Send + Sync + 'static {
    /// Send a request to the Quick-Node and await a response.
    fn send(&self, req: TransportRequest)
    -> impl Future<Output = Result<TransportResponse>> + Send;

    /// Notify the Quick-Node that this session is ending.
    ///
    /// Called from `Drop`; implementations **must not** block the caller.
    fn disconnect(&self, user: u64, tenant: u64);
}
