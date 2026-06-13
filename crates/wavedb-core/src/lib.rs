//! Core primitives for `WaveDB`: composite IDs, metadata, permission refs,
//! and the workspace error type.
//!
//! These types contain no I/O and are safe to use in WASM, in proc-macro
//! generated code, and on every node kind (quick, slow, client).

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

// `#[derive(WaveWire)]` emits absolute `::wavedb_core::` paths; this alias
// makes them resolve when the derive is used inside this crate itself.
extern crate self as wavedb_core;

mod error;
mod id;
mod metadata;
pub mod migration;
mod permission;
pub mod query;
pub mod registry;
mod traits;
pub mod wire;

pub use error::{Error, Result, ValidationError};
pub use id::{Id, Shape};
pub use metadata::Metadata;
pub use migration::{
    ErasedMigrateFn, MigrationEntry, MigrationPlan, MigrationRegistry,
    VersionRef,
};
pub use permission::{PermissionGroupId, PermissionRef};
pub use registry::{
    FieldDescriptor, FieldKind, ObjectDescriptor, ObjectRegistry,
};
pub use traits::{
    MigratesFrom, MigrationChain, RollbackFrom, WaveDbHooks, WaveDbStruct,
};
pub use wire::{Wire, WireError, WireReader, WireResult, WireWriter};
