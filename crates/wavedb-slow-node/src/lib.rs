//! WaveDB Slow-Node library.
//!
//! Exposes Slow-Node internals as a library target so the
//! `wavedb-test-cluster` harness can spin up in-process Slow-Nodes without
//! spawning separate processes.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod config;
pub mod flush;
pub mod server;
pub mod store;
