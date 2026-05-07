//! Object trait hierarchy: [`UniqueObject`] and [`NonUniqueObject`].
//!
//! Every `#[wave_db]`-annotated struct implements exactly one of these two
//! traits depending on the `Unique` / `NonUnique` / `NestedNonUnique` shape
//! declared in the macro.
//!
//! Applications should import these traits via `wavedb::prelude::*`.

use crate::db::Db;
use crate::query::Expr;
use wavedb_core::{Result, Shape, WaveDbStruct};
use wavedb_net::request::RequestKind;

/// Behaviour for **Unique** records — exactly one live record per
/// `(STRUCT_ID, TENANT_ID)`.
///
/// Unique records support `search` (lookup the one live record) and `update`
/// (write a new versioned record and rotate the anchor).  There is no `delete`
/// at the record level — deleting the only record of its kind is a tenant-level
/// action.
pub trait UniqueObject: WaveDbStruct + Sized {
    /// Look up the one live record for the current tenant, if it exists.
    ///
    /// Returns `None` when no record has been written yet (e.g. a new tenant).
    fn search(db: &Db) -> impl std::future::Future<Output = Result<Option<Self>>> + Send;

    /// Persist this record as the new live version for the current tenant.
    ///
    /// Writes a versioned record and rotates the anchor to point at it.
    /// If `id` and `metadata` are at their defaults, the engine assigns
    /// fresh values at write time.
    fn update(self, db: &Db) -> impl std::future::Future<Output = Result<()>> + Send;
}

/// Behaviour for **NonUnique** records — many live records per tenant.
///
/// Supports `query` (filtered scan over the per-`(STRUCT_ID, TENANT_ID)` index),
/// `update` (versioned in place), and `delete` (tombstone anchor).
pub trait NonUniqueObject: WaveDbStruct + Sized {
    /// Query live records matching the given expression.
    ///
    /// Pass [`Expr::all()`] to return every live record for this type
    /// under the current tenant.
    fn query(db: &Db, expr: Expr) -> impl std::future::Future<Output = Result<Vec<Self>>> + Send;

    /// Persist this record as a new versioned entry, rotating the anchor.
    fn update(self, db: &Db) -> impl std::future::Future<Output = Result<()>> + Send;

    /// Delete this record: write a tombstone to the anchor.
    fn delete(self, db: &Db) -> impl std::future::Future<Output = Result<()>> + Send;
}

// ── Shape guard ──────────────────────────────────────────────────────────────

/// Validate that a struct's `SHAPE` constant matches the expected shape.
///
/// Called at the start of `search`, `query`, `update`, and `delete` to give
/// early, readable errors when a Unique struct is accidentally used as
/// NonUnique or vice versa.
pub fn assert_shape<S: WaveDbStruct>(expected: Shape) -> Result<()> {
    if S::SHAPE == expected {
        Ok(())
    } else {
        Err(wavedb_core::Error::Other(format!(
            "shape mismatch: expected {:?}, got {:?} for struct_id={}",
            expected,
            S::SHAPE,
            S::STRUCT_ID,
        )))
    }
}

// ── Concrete helpers ─────────────────────────────────────────────────────────

/// Search for a Unique record via the transport.
pub async fn do_search_unique<S>(db: &Db) -> Result<Option<S>>
where
    S: WaveDbStruct + for<'de> serde::Deserialize<'de>,
{
    let payload = db
        .send(RequestKind::SearchUnique {
            struct_id: S::STRUCT_ID,
            user: db.user(),
            tenant: db.tenant(),
        })
        .await?;

    if payload.is_empty() {
        return Ok(None);
    }

    let record: S = postcard::from_bytes(&payload)?;
    Ok(Some(record))
}

/// Query NonUnique records via the transport.
pub async fn do_query_non_unique<S>(db: &Db, expr: Expr) -> Result<Vec<S>>
where
    S: WaveDbStruct + for<'de> serde::Deserialize<'de>,
{
    let filter = expr.to_bytes()?;
    let payload = db
        .send(RequestKind::QueryNonUnique {
            struct_id: S::STRUCT_ID,
            user: db.user(),
            tenant: db.tenant(),
            filter,
        })
        .await?;

    if payload.is_empty() {
        return Ok(Vec::new());
    }

    let records: Vec<S> = postcard::from_bytes(&payload)?;
    Ok(records)
}

/// Write (create or update) a record via the transport.
pub async fn do_write<S>(db: &Db, record: &S) -> Result<()>
where
    S: WaveDbStruct + serde::Serialize + Sync,
{
    let payload: Vec<u8> = postcard::to_allocvec(record)?;
    db.send(RequestKind::Write {
        struct_id: S::STRUCT_ID,
        user: db.user(),
        tenant: db.tenant(),
        payload,
    })
    .await?;
    Ok(())
}

/// Delete a record (tombstone) via the transport.
pub async fn do_delete(db: &Db, id: u128) -> Result<()> {
    db.send(RequestKind::Delete {
        id,
        user: db.user(),
        tenant: db.tenant(),
    })
    .await?;
    Ok(())
}
