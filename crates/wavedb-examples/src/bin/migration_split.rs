#![allow(clippy::future_not_send)]
//! Splitting a struct — the inverse of the compose (Type 2) pattern.
//!
//! `Order1` carries both order data **and** shipping fields.  The split
//! breaks it into two targets, each migrating from the same source:
//!
//! - `Order2` stays in the original family (same `struct_id = 70`), drops
//!   the shipping fields, and upgrades via a normal `migrate_from`.
//! - `ShippingInfo1` starts a **new** family (`struct_id = 71`) and lifts
//!   the shipping fields out of `Order1`.  Its `first_try` hook looks up
//!   the old order **before** the DB search for `ShippingInfo1` — exactly
//!   the compose pipeline, pointed at a single source.
//!
//! Compose and split are the same mechanism viewed from opposite ends:
//! compose pulls many sources into one `first_try`; split points many
//! `first_try`s at one source.  The stored `Order1` record stays readable
//! until every split target has lazily materialised its slice.
//!
//! Run with:
//!   cargo run --bin migration_split

use wavedb::prelude::*;

// ── The source: v1 order with embedded shipping fields ───────────────────────

#[wave_db(struct_id = 70, NonUnique)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Order1 {
    pub customer: u64,
    pub amount_cents: u64,
    pub ship_street_hash: u64,
    pub ship_city_hash: u64,
}

// ── Migration fns (async, generic over Db) ────────────────────────────────────

// Split target A — same family: keep order core, drop shipping fields.
#[allow(clippy::unused_async)]
async fn order_v1_to_v2<Db>(
    _db: &Db,
    old: Order1,
) -> wavedb_core::Result<Order2> {
    Ok(Order2 {
        id: old.id,
        metadata: old.metadata,
        customer: old.customer,
        amount_cents: old.amount_cents,
    })
}

// Split target B — new family: lift the shipping fields out of the order.
#[allow(clippy::unused_async)]
async fn lift_shipping<Db>(
    _db: &Db,
    old: Order1,
) -> wavedb_core::Result<ShippingInfo1> {
    Ok(ShippingInfo1 {
        // New family ⇒ new identity; the engine assigns a fresh Id at save
        // time.  Only the lifted fields carry over.
        id: Id::default(),
        metadata: Metadata::default(),
        order_created_at: old.id.created_at(),
        street_hash: old.ship_street_hash,
        city_hash: old.ship_city_hash,
    })
}

// first_try for the new family: fetch the source `Order1` ahead of the DB
// search for `ShippingInfo1`.  Production would query the stored order by
// its natural key; the demo synthesises one to show the engine-skip path.
#[allow(clippy::unused_async)]
async fn shipping_first_try<Db>(
    _db: &Db,
) -> wavedb_core::Result<Option<Order1>> {
    Ok(Some(Order1 {
        customer: 7,
        amount_cents: 4200,
        ship_street_hash: 0xABCD,
        ship_city_hash: 0x1234,
        ..Default::default()
    }))
}

// ── Split target A: Order2 (family 70, version 2) ────────────────────────────

#[wave_db(
    struct_id = 70,
    NonUnique,
    migrate_from      = Order1,
    migrate_from_with = order_v1_to_v2,
)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Order2 {
    pub customer: u64,
    pub amount_cents: u64,
}
pub type Order = Order2;

// ── Split target B: ShippingInfo1 (new family 71) ────────────────────────────

#[wave_db(
    struct_id = 71,
    NonUnique,
    migrate_from      = Order1,
    migrate_from_with = lift_shipping,
    first_try         = shipping_first_try,
)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ShippingInfo1 {
    pub order_created_at: u64,
    pub street_hash: u64,
    pub city_hash: u64,
}
pub type ShippingInfo = ShippingInfo1;

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // ── Compile-time chain: BOTH targets migrate from the same source ────────
    const fn assert_source<T: wavedb_core::MigratesFrom<Source = Order1>>() {}
    assert_source::<Order2>();
    assert_source::<ShippingInfo1>();
    println!("MigratesFrom: Order2::Source = ShippingInfo1::Source = Order1 ✓");

    // ── Registry: one source node, two outgoing edges ─────────────────────────
    let mut registry = wavedb_core::MigrationRegistry::new();
    Order2::register_migration(&mut registry);
    ShippingInfo1::register_migration(&mut registry);

    let order_v1 = wavedb_core::VersionRef::new(Order1::STRUCT_ID, 1);
    let order_v2 = wavedb_core::VersionRef::new(Order2::STRUCT_ID, 2);
    let shipping_v1 = wavedb_core::VersionRef::new(ShippingInfo1::STRUCT_ID, 1);

    assert_eq!(registry.resolve(order_v1, order_v2).unwrap().len(), 1);
    assert_eq!(registry.resolve(order_v1, shipping_v1).unwrap().len(), 1);
    println!("Order1 → Order2 and Order1 → ShippingInfo1: 1 step each");

    let db = Db::noop(1, 100);

    let stored = Order1 {
        customer: 7,
        amount_cents: 4200,
        ship_street_hash: 0xABCD,
        ship_city_hash: 0x1234,
        ..Default::default()
    };

    // ── Target A: same-family upgrade drops the shipping fields ──────────────
    let order = Order2::__wave_db_migrate_from(&db, stored.clone())
        .await
        .unwrap();
    assert_eq!(order.customer, 7);
    assert_eq!(order.amount_cents, 4200);
    println!(
        "Order2: customer={} amount={} (shipping fields gone)",
        order.customer, order.amount_cents
    );

    // ── Target B: first_try fetches the source, lift extracts its slice ──────
    let candidate = ShippingInfo1::__wave_db_first_try(&db).await.unwrap();
    let source = candidate.expect("first_try found the old order");
    let shipping = ShippingInfo1::__wave_db_migrate_from(&db, source)
        .await
        .unwrap();
    assert_eq!(shipping.street_hash, 0xABCD);
    assert_eq!(shipping.city_hash, 0x1234);
    println!(
        "ShippingInfo1: street=0x{:X} city=0x{:X} (lifted from Order1)",
        shipping.street_hash, shipping.city_hash
    );

    // The stored Order1 bytes remain readable throughout: each target
    // materialises its slice lazily on first read, and the source is only
    // eligible for history flush once both targets have been written.

    println!("migration_split example OK");
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_main() {
        super::main();
    }
}
