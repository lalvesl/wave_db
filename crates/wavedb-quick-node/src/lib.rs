//! WaveDB Quick-Node library.
//!
//! Exposes Quick-Node internals as a library target so the
//! `wavedb-test-cluster` harness can spin up in-process Quick-Nodes without
//! spawning separate processes.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod config;
pub mod gossip;
pub mod node;
pub mod ownership;
pub mod replication;
pub mod ring;
pub mod server;
