//! `WaveDB` — A User-Partitioned, Tenant-Centric Database.
//!
//! This is the umbrella crate that re-exports the public API surface.
//! End users write `use wavedb::prelude::*;` to get everything they need.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use wavedb::prelude::*;
//! use wavedb_net::{MockTransport, mock::ScriptedReply};
//!
//! let mock = MockTransport::new();
//! mock.push(ScriptedReply::connect("ws://owner:7700", "ws://backup:7700"));
//!
//! let db = Db::open_with_transport(mock, /* user */ 1, /* tenant */ 100).await?;
//! let profile: Option<UserProfile> = UserProfile::search(&db).await?;
//! ```

#![deny(unsafe_op_in_unsafe_fn)]

pub mod db;
pub mod object;
pub mod query;

pub use db::Db;

/// Re-exports of the most commonly used types and traits.
pub mod prelude {
    pub use crate::{
        Db,
        object::{NonUniqueObject, UniqueObject},
        query::Expr,
    };
    pub use wavedb_core::{Id, Metadata, PermissionRef, Shape, WaveDbStruct};
    pub use wavedb_macros::wave_db;
}

// Pass-through re-exports so advanced users don't have to add sub-crates.
pub use wavedb_core;
pub use wavedb_macros;
#[cfg(feature = "native")]
pub use wavedb_storage;
