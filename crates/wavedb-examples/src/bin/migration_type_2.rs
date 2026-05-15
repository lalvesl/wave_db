#![allow(clippy::future_not_send)]
//! Type 2 (compose) pattern — now expressed through `first_try` and
//! `migrate_from_with`.
//!
//! `OrderSummary1` is a *new* struct family (different `struct_id`) computed
//! from existing `Order1` + `OrderItem1` records.  In the old design this was
//! a separate "Type 2 migration" with `register_compose`.  In the new design
//! it's just a Type-1-shaped declaration with two hooks:
//!
//! - `first_try(&db) → Option<Order1>` — runs **before** the DB search for
//!   `OrderSummary1`.  If it returns `Some(order)`, the engine skips the
//!   normal DB lookup and feeds the order into `migrate_from_with` to
//!   synthesise the summary on the fly.
//! - `migrate_from_with(&db, order) → OrderSummary1` — receives the source
//!   `Order1` (whether it came from `first_try` or an actual stored record)
//!   plus `&db` so it can query for the related `OrderItem1`s.
//! - `fallback_not_found(&db) → Option<OrderSummary1>` — last-resort default
//!   when nothing exists yet.
//!
//! No `register_compose` call required — the chain comes from the type system
//! via `MigratesFrom`.
//!
//! Run with:
//!   cargo run --bin migration_type_2

use wavedb::prelude::*;

// ── Schema ────────────────────────────────────────────────────────────────────

#[wave_db(struct_id = 10, NonUnique)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Order1 {
    pub customer: u64,
    pub amount_cents: u64,
}

#[wave_db(struct_id = 11, NonUnique)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OrderItem1 {
    pub product: u64,
    pub quantity: u32,
}

// ── Compose hooks (async, generic over Db) ────────────────────────────────────

// first_try: synthesise an `Order1` to feed into `migrate_from_with`.
//
// In production this would query the DB for the relevant Order record by some
// natural key.  Returning `Some(order)` skips the normal DB search for
// `OrderSummary1` and runs `migrate_from_with` directly.  Returning `None`
// falls through to the regular DB path.
#[allow(clippy::unused_async)]
async fn summary_first_try<Db>(_db: &Db) -> wavedb_core::Result<Option<Order1>> {
    // Demo: return None to show the fallthrough path.  Production code lives here.
    Ok(None)
}

// migrate_from_with: compose `Order1` + (DB-fetched items) into `OrderSummary1`.
//
// The Db handle gives the migration full access to the rest of the database so
// it can pull whatever side data the composition needs.  Here we hard-code a
// fake item count; production code would `OrderItem::query(db, …).await?`.
#[allow(clippy::unused_async)]
async fn compose_summary<Db>(_db: &Db, order: Order1) -> wavedb_core::Result<OrderSummary1> {
    let item_count = 2; // would be: OrderItem::query(db, parent=order.id).await?.len()
    Ok(OrderSummary1 {
        id: order.id,
        metadata: order.metadata,
        customer: order.customer,
        amount_cents: order.amount_cents,
        item_count,
    })
}

// fallback_not_found: empty/default summary when no record exists yet.
#[allow(clippy::unused_async)]
async fn summary_fallback<Db>(_db: &Db) -> wavedb_core::Result<Option<OrderSummary1>> {
    Ok(Some(OrderSummary1 {
        id: Id::default(),
        metadata: Metadata::default(),
        customer: 0,
        amount_cents: 0,
        item_count: 0,
    }))
}

// ── The composed struct ───────────────────────────────────────────────────────

#[wave_db(
    struct_id = 12,
    NonUnique,
    migrate_from       = Order1,
    migrate_from_with  = compose_summary,
    first_try          = summary_first_try,
    fallback_not_found = summary_fallback,
)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OrderSummary1 {
    pub customer: u64,
    pub amount_cents: u64,
    pub item_count: u32,
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // ── Compile-time chain: OrderSummary1 migrates from Order1 ────────────────
    const fn assert_source<T: wavedb_core::MigratesFrom<Source = Order1>>() {}
    assert_source::<OrderSummary1>();
    println!("MigratesFrom: OrderSummary1::Source = Order1 ✓");

    // ── Optional registry wiring (compile-time chain → runtime graph) ─────────
    // This is only needed for cluster-routing queries on the version graph.
    // The cross-version read/write API works without it (see migration_type_1).
    let mut registry = wavedb_core::MigrationRegistry::new();
    OrderSummary1::register_migration(&mut registry);
    let order_v1 = wavedb_core::VersionRef::new(Order1::STRUCT_ID, 1);
    let summary_v1 = wavedb_core::VersionRef::new(OrderSummary1::STRUCT_ID, 1);
    assert_eq!(registry.resolve(order_v1, summary_v1).unwrap().len(), 1);
    println!("Order1 → OrderSummary1: 1 step (registry edge wired)");

    let db = Db::noop(1, 100);

    // ── Hook 1: first_try — synthesise the source ahead of the DB search ─────
    let candidate = OrderSummary1::__wave_db_first_try(&db).await.unwrap();
    assert!(candidate.is_none(), "demo first_try returns None");
    println!("first_try: None (would fall through to DB search)");

    // ── Hook 2: migrate_from_with — compose the summary from Order1 + items ──
    // Whether the Order1 came from first_try or an actual DB read, the engine
    // funnels it through this same function.
    let order = Order1 {
        customer: 7,
        amount_cents: 4200,
        ..Default::default()
    };
    let summary = OrderSummary1::__wave_db_migrate_from(&db, order.clone())
        .await
        .unwrap();
    assert_eq!(summary.customer, 7);
    assert_eq!(summary.amount_cents, 4200);
    assert_eq!(summary.item_count, 2);
    println!(
        "Composed: customer={} amount={} item_count={}",
        summary.customer, summary.amount_cents, summary.item_count
    );

    // ── Hook 3: fallback_not_found — default when nothing exists ─────────────
    let default = OrderSummary1::__wave_db_fallback_not_found(&db)
        .await
        .unwrap();
    let default = default.expect("fallback synthesises a default");
    assert_eq!(default.customer, 0);
    println!(
        "fallback_not_found: provided default (customer={}, item_count={})",
        default.customer, default.item_count
    );

    // ── Note on rollback for type-2 patterns ─────────────────────────────────
    // Symmetric rollback (OrderSummary1 → Order1 + items) would be declared on
    // Order1 via `migrate_rollback = OrderSummary1, migrate_rollback_with = …`.
    // Items lost during compose require a Slow-Node history lookup, which is
    // out of scope for this in-memory example.

    println!("migration_type_2 example OK");
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_main() {
        super::main();
    }
}
