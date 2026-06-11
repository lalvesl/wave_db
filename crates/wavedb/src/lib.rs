//! # WaveDB
//!
//! A user-partitioned, tenant-centric database where every record is owned by
//! a specific tenant, versioned immutably, and optionally scoped to a user or
//! device.  The public API is three crates wide:
//!
//! | Crate | Role |
//! |-------|------|
//! | [`wavedb`](crate) | Client handle, object traits, query DSL |
//! | [`wavedb_core`] | Composite IDs, metadata, migrations, error type |
//! | [`wavedb_storage`] | Storage engine (pages, anchors, indexes, compression) |
//!
//! ## Data shapes
//!
//! Every `#[wave_db]`-annotated struct declares one of three [`Shape`]s:
//!
//! | Shape | Records per `(STRUCT_ID, TENANT_ID)` | Typical use |
//! |-------|--------------------------------------|-------------|
//! | `Unique` (default) | Exactly 1 live record | User profile, settings |
//! | `NonUnique` | Many live records | Orders, events, messages |
//! | `NestedNonUnique` | Many, child of one parent record (recurses: a child may own its own child collection) | Invoice lines, checklist items |
//!
//! ## 128-bit composite IDs
//!
//! Every record has a stable [`Id`] packed into 128 bits:
//!
//! ```text
//! [ TENANT_ID: u48 | SHARD_ID: u12 | STRUCT_ID: u20 | CREATED_AT: u48 ]
//! ```
//!
//! `STRUCT_ID` is assigned at compile time by `#[wave_db(struct_id = N)]` and
//! is permanent across schema migrations.  `CREATED_AT` is a 100 µs counter.
//!
//! ## Metadata
//!
//! [`Metadata`] lives beside every object on every page and tracks:
//!
//! - **Version chain** — `old_modification_id` / `new_modification_id` link
//!   the complete immutable history of each record.
//! - **Authorship** — `user` (u48) and `device_created` capture who wrote each
//!   version and from which device.
//! - **Access control** — <code>permission: Option<[PermissionRef]></code>.  `None`
//!   (the common case) means tenant-wide access.
//!
//! ## Operation modes
//!
//! The four CRUD operations map to trait methods:
//!
//! | Operation | [`UniqueObject`] | [`NonUniqueObject`] |
//! |-----------|-----------------|---------------------|
//! | **search** | `search(&db)` → `Option<Self>` | — |
//! | **query** | — | `query(&db, expr)` → `Vec<Self>` |
//! | **save** | `save(self, &db)` | `save(self, &db)` |
//! | **delete** | — | `delete(self, &db)` |
//!
//! Queries use the composable [`Expr`] DSL.  Expressions are serialised with
//! the wire format and sent to the Quick-Node for server-side evaluation.
//!
//! ## Migration model
//!
//! Schema versions are embedded in type names: `UserProfile1`, `UserProfile2`.
//! The trailing integer becomes `STRUCT_VERSION` at compile time.  Old pages
//! are **never force-migrated**; versions are resolved lazily on the first
//! read that sees an old `struct_version` in [`Metadata`].  Multi-hop chains
//! (v1 → v3 via v1 → v2 → v3) are resolved at startup through a
//! [`MigrationRegistry`](wavedb_core::MigrationRegistry).
//!
//! ## Quick start
//!
//! ```ignore
//! use wavedb::prelude::*;
//! use wavedb_net::{MockTransport, mock::ScriptedReply};
//!
//! // One UserProfile per tenant (Unique shape, default).
//! #[wave_db(struct_id = 1)]
//! struct UserProfile1 {
//!     name: String,
//!     email: String,
//! }
//!
//! impl UniqueObject for UserProfile1 {
//!     fn search(db: &Db)
//!         -> impl std::future::Future<Output = wavedb_core::Result<Option<Self>>> + Send
//!     {
//!         wavedb::object::do_search_unique::<Self>(db)
//!     }
//!     fn save(self, db: &Db)
//!         -> impl std::future::Future<Output = wavedb_core::Result<()>> + Send
//!     {
//!         wavedb::object::do_write(db, &self)
//!     }
//! }
//!
//! // Many Orders per tenant (NonUnique shape).
//! #[wave_db(struct_id = 2, NonUnique)]
//! #[derive(Debug, Serialize, Deserialize)]
//! struct Order1 { amount: u64 }
//!
//! impl NonUniqueObject for Order1 {
//!     fn query(db: &Db, expr: Expr)
//!         -> impl std::future::Future<Output = wavedb_core::Result<Vec<Self>>> + Send
//!     {
//!         wavedb::object::do_query_non_unique::<Self>(db, expr)
//!     }
//!     fn save(self, db: &Db)
//!         -> impl std::future::Future<Output = wavedb_core::Result<()>> + Send
//!     {
//!         wavedb::object::do_write(db, &self)
//!     }
//!     fn delete(self, db: &Db)
//!         -> impl std::future::Future<Output = wavedb_core::Result<()>> + Send
//!     {
//!         wavedb::object::do_delete(db, 0)
//!     }
//! }
//!
//! async fn run() -> wavedb_core::Result<()> {
//!     // Use MockTransport in tests; WsClient in production.
//!     let mock = MockTransport::new();
//!     mock.push(ScriptedReply::connect("ws://owner:7700", "ws://backup:7700"));
//!     mock.push(ScriptedReply::ok(vec![])); // search returns None
//!
//!     let db = Db::open_with_transport(mock, /* user */ 1, /* tenant */ 100).await?;
//!
//!     let _profile: Option<UserProfile1> = UserProfile1::search(&db).await?;
//!     let _orders: Vec<Order1> = Order1::query(&db, Order1::amount.gt(100u64)).await?;
//!     Ok(())
//! }
//! ```

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod db;
pub mod object;
pub mod query;

pub use db::Db;

/// Network transport re-exports.
///
/// Lets users build a custom transport without depending on `wavedb-net`
/// directly:
///
/// ```ignore
/// use wavedb::net::{Transport, MockTransport};
/// ```
///
/// Native-only transports (`WsClient`, `HttpClient`, `ChannelTransport`,
/// `MockServer`, `EventBus`) are re-exported behind the `native` feature.
pub mod net {
    #[cfg(feature = "native")]
    pub use wavedb_net::{
        ChannelTransport, EventBus, HttpClient, MockServer, WsClient,
    };
    pub use wavedb_net::{
        MockTransport, Transport, TransportRequest, TransportResponse,
        mock::ScriptedReply, request::RequestKind,
    };
}

/// Re-exports of the most commonly used types and traits.
///
/// Import everything with `use wavedb::prelude::*` to get [`Db`],
/// [`UniqueObject`](object::UniqueObject), [`NonUniqueObject`](object::NonUniqueObject),
/// [`Expr`](query::Expr), the core primitives, and the `#[wave_db]` macro.
pub mod prelude {
    pub use crate::{
        Db,
        object::{NonUniqueObject, UniqueObject},
        query::{Expr, Field},
    };
    pub use wavedb_core::{Id, Metadata, PermissionRef, Shape, WaveDbStruct};
    pub use wavedb_macros::{declare_objects, wave_db};
}

// Pass-through re-exports so advanced users don't have to add sub-crates.
pub use wavedb_core;
pub use wavedb_macros;
#[cfg(feature = "native")]
pub use wavedb_storage;
