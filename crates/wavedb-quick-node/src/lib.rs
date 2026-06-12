//! WaveDB Quick-Node library.
//!
//! Application backends use the high-level [`Server`] builder:
//!
//! ```rust,ignore
//! Server::bind("127.0.0.1:7700")
//!     .tenant(42)
//!     .registry(app_objects::REGISTRY)
//!     .serve()
//!     .await
//! ```
//!
//! The submodules expose the internals for advanced setups and for the
//! `wavedb-test-cluster` harness, which spins up in-process Quick-Nodes
//! without spawning separate processes.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod config;
pub mod gossip;
pub mod node;
pub mod ownership;
pub mod replication;
pub mod ring;
pub mod serve;
pub mod server;

pub use serve::{Running, Server};
