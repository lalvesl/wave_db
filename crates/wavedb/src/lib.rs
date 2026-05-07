//! `WaveDB` — A User-Partitioned, Tenant-Centric Database.
//!
//! This is the umbrella crate that re-exports the public API surface.
//! End users write `use wavedb::prelude::*;` to get everything they need.

#![deny(unsafe_op_in_unsafe_fn)]

/// Re-exports of the most commonly used types and traits.
pub mod prelude {
    pub use wavedb_core::{Id, Metadata, PermissionRef, Shape, WaveDbStruct};
    pub use wavedb_macros::wave_db;
}

// Re-export sub-crates
pub use wavedb_core;
pub use wavedb_macros;
pub use wavedb_storage;
