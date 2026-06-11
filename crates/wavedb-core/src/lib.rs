//! Core primitives for `WaveDB`: composite IDs, metadata, permission refs,
//! and the workspace error type.
//!
//! These types contain no I/O and are safe to use in WASM, in proc-macro
//! generated code, and on every node kind (quick, slow, client).

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

mod error;
mod id;
mod metadata;
pub mod migration;
mod permission;
pub mod registry;
mod traits;
pub mod wire;

pub use error::{Error, Result};
pub use id::{Id, Shape};
pub use metadata::Metadata;
pub use migration::{
    ErasedMigrateFn, MigrationEntry, MigrationPlan, MigrationRegistry,
    VersionRef,
};
pub use permission::{PermissionGroupId, PermissionRef};
pub use registry::{FieldDescriptor, FieldKind, ObjectDescriptor};
pub use traits::{MigratesFrom, MigrationChain, RollbackFrom, WaveDbStruct};
pub use wire::{Wire, WireError, WireReader, WireResult, WireWriter};
