//! # WaveDB — A User-Partitioned, Tenant-Centric Database
//!
//! WaveDB is a research database where **every user owns their own isolated
//! data space**, history is a first-class citizen, and horizontal scaling is
//! a structural property — not an afterthought.
//!
//! ## Quick Start
//!
//! ```rust
//! use wave_db::prelude::*;
//!
//! #[wave_db(struct_id = 1)]
//! pub struct UserProfile {
//!     pub id: Id,
//!     pub metadata: Metadata,
//!     pub display_name: String,
//! }
//!
//! #[wave_db(NonUnique, struct_id = 2)]
//! pub struct Order {
//!     pub id: Id,
//!     pub metadata: Metadata,
//!     pub amount: u64,
//! }
//!
//! #[wave_db(NonUnique, btree_threshold = 100, struct_id = 3)]
//! pub struct Message {
//!     pub id: Id,
//!     pub metadata: Metadata,
//!     pub body: String,
//! }
//! ```

pub mod anchor;
pub mod config;
pub mod disk_store;
pub mod id;
pub mod journal;
pub mod metadata;
pub mod object;
pub mod page;
pub mod storage;
pub mod store;

pub use anchor::{AnchorContent, AnchorSlot};
pub use disk_store::{DiskStore, DiskStoreError};
pub use id::{Id, ShardId, Slider, StructId, TenantId, Timestamp};
pub use journal::{Journal, JournalEntry};
pub use metadata::Metadata;
pub use object::{NonUniqueObject, UniqueObject, WaveDbObject};
pub use store::{AnchorMode, StoredRecord, Store, StoreError};

// Re-export the proc-macro so users only need `use wave_db::prelude::*`.
pub use wave_db_macro::wave_db;

/// Convenience prelude — import everything a user needs.
pub mod prelude {
    pub use crate::anchor::{AnchorContent, AnchorSlot};
    pub use crate::config::Config;
    pub use crate::disk_store::{DiskStore, DiskStoreError};
    pub use crate::id::{Id, ShardId, Slider, StructId, TenantId, Timestamp};
    pub use crate::journal::{Journal, JournalEntry};
    pub use crate::metadata::Metadata;
    pub use crate::object::{NonUniqueObject, UniqueObject, WaveDbObject};
    pub use crate::store::{AnchorMode, StoredRecord, Store, StoreError};
    pub use wave_db_macro::wave_db;
}
