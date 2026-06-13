//! Query expression DSL for `NonUnique` record lookups.
//!
//! The types live in [`wavedb_core::query`] so the Quick-Node can evaluate
//! the same expressions server-side (see `wavedb_core::query::eval`); this
//! module re-exports them under the established client path.
//!
//! # Example
//!
//! ```rust,ignore
//! use wavedb::query::Expr;
//!
//! // All orders where amount > 100 AND status == "shipped" — typed columns.
//! let filter = Expr::and(
//!     Order::amount.gt(100u64),
//!     Order::status.eq("shipped"),
//! );
//! ```

pub use wavedb_core::query::{Expr, Field, FieldName, Value};
