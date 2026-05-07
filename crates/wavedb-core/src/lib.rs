//! Core primitives for `WaveDB`: composite IDs, metadata, permission refs,
//! and the workspace error type.
//!
//! These types contain no I/O and are safe to use in WASM, in proc-macro
//! generated code, and anywhere postcard serialization is available.

#![deny(unsafe_op_in_unsafe_fn)]

mod error;
mod id;
mod metadata;
mod permission;
mod traits;

pub use error::{Error, Result};
pub use id::{Id, Shape};
pub use metadata::Metadata;
pub use permission::{PermissionGroupId, PermissionRef};
pub use traits::WaveDbStruct;
