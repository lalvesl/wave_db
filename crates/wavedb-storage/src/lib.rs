//! WaveDB storage engine.
//!
//! Handles page layout, hash-mapped data files, anchor slots,
//! versioned records, indexes, compression, and the write pipeline.
//!
//! This crate is sync-only — no async runtime dependency. The async
//! actor wrapper lives in the pipeline module (Phase 7).

#![deny(unsafe_op_in_unsafe_fn)]

pub mod anchor;
pub mod cache;
pub mod error;
pub mod file;
pub mod hash;
pub mod page;
pub mod versioned;

pub use anchor::{AnchorKey, AnchorKind, AnchorMode, AnchorSlot};
pub use error::{StorageError, StorageResult};
pub use file::data::DataFile;
pub use hash::page_hash;
pub use page::{Page, PageHeader};
pub use versioned::VersionedRecord;
