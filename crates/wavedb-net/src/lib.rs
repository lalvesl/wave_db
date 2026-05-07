//! `WaveDB` network transport layer.
//!
//! Provides the [`Transport`] trait and:
//! - A [`MockTransport`] for in-process tests (Phase 10).
//! - Stubs for HTTP POST and WebSocket transports (Phase 11).

#![deny(unsafe_op_in_unsafe_fn)]

pub mod mock;
pub mod request;

pub use mock::MockTransport;
pub use request::{TransportRequest, TransportResponse};

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
