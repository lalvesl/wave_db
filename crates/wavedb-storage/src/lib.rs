//! `WaveDB` storage engine.
//!
//! Handles page layout, hash-mapped data files, anchor slots,
//! versioned records, indexes, compression, heap data, and the
//! write pipeline with journal-backed crash recovery.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod anchor;
pub mod cache;
pub mod compression;
pub mod error;
pub mod file;
pub mod hash;
pub mod heap;
pub mod index;
pub mod page;
pub mod permissions;
pub mod pipeline;
pub mod versioned;

pub use anchor::{AnchorKey, AnchorKind, AnchorMode, AnchorSlot};
pub use error::{StorageError, StorageResult};
pub use file::data::DataFile;
pub use hash::{PageKey, mix64, tuple2_page, tuple4_page};
pub use heap::HeapFile;
pub use index::{AdaptiveIndex, IndexBackend, IndexKey};
pub use page::{Page, PageHeader};
pub use pipeline::journal::Journal;
pub use versioned::VersionedRecord;
