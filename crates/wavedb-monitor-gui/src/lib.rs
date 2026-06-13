//! Library interface for the WaveDB GUI monitor.
//!
//! Exposes the poll worker, shared state, and app so integration tests can
//! drive the monitor headlessly (the binary in `main.rs` is a thin shell).

pub mod app;
pub mod config;
pub mod poll;
pub mod state;
pub mod tabs;
