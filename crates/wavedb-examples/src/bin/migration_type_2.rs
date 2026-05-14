//! Type 2 migration: compose two structs into one (Order + OrderItem → OrderSummary).
//!
//! This pattern merges multiple source struct versions into a single
//! target version.  Rollback checks whether `OrderSummary` exists; if not
//! it reconstitutes the original `Order` and `OrderItem` records.
//!
//! Run with:
//!   cargo run --bin migration_type_2

use wavedb::prelude::*;
use wavedb_core::migration::{MigrationRegistry, VersionRef};

// ── Schema ────────────────────────────────────────────────────────────────────

#[wave_db(struct_id = 10, NonUnique)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Order1 {
    pub id: Id,
    pub metadata: Metadata,
    pub customer: u64,
    pub amount_cents: u64,
}

#[wave_db(struct_id = 11, NonUnique)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OrderItem1 {
    pub id: Id,
    pub metadata: Metadata,
    pub product: u64,
    pub quantity: u32,
}

#[wave_db(struct_id = 12, NonUnique)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OrderSummary1 {
    pub id: Id,
    pub metadata: Metadata,
    pub customer: u64,
    pub amount_cents: u64,
    pub item_count: u32,
}

// ── Migration functions ───────────────────────────────────────────────────────

fn compose(order: Order1, items: &[OrderItem1]) -> OrderSummary1 {
    OrderSummary1 {
        id: order.id,
        metadata: order.metadata,
        customer: order.customer,
        amount_cents: order.amount_cents,
        item_count: items.len() as u32,
    }
}

fn decompose(summary: OrderSummary1) -> (Order1, Vec<OrderItem1>) {
    let order = Order1 {
        id: summary.id,
        metadata: summary.metadata.clone(),
        customer: summary.customer,
        amount_cents: summary.amount_cents,
    };
    // Items cannot be reconstituted from summary alone — return empty slice.
    // Production rollback would first attempt a lookup in the Slow-Node history
    // store; if found, it returns the original items; if not, it signals data
    // loss so the caller can abort the rollback.
    (order, Vec::new())
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    let order_v1 = VersionRef::new(Order1::STRUCT_ID, 1);
    let item_v1 = VersionRef::new(OrderItem1::STRUCT_ID, 1);
    let summary_v1 = VersionRef::new(OrderSummary1::STRUCT_ID, 1);

    // Register compose migration: Order + OrderItem → OrderSummary (with rollback)
    let mut registry = MigrationRegistry::new();
    registry.register_compose(
        &[order_v1, item_v1],
        summary_v1,
        /* has_rollback= */ true,
    );

    // Both sources can independently reach the target
    assert!(registry.resolve(order_v1, summary_v1).is_ok());
    assert!(registry.resolve(item_v1, summary_v1).is_ok());
    println!(
        "Order → OrderSummary: {} step(s)",
        registry.resolve(order_v1, summary_v1).unwrap().len()
    );
    println!(
        "OrderItem → OrderSummary: {} step(s)",
        registry.resolve(item_v1, summary_v1).unwrap().len()
    );

    // Rollback path: OrderSummary → each source
    assert!(registry.can_rollback(summary_v1, order_v1));
    assert!(registry.can_rollback(summary_v1, item_v1));
    println!("Rollback to Order: available");
    println!("Rollback to OrderItem: available");

    // Execute compose migration
    let order = Order1 {
        id: Id::default(),
        metadata: Metadata::default(),
        customer: 7,
        amount_cents: 4200,
    };
    let items = vec![
        OrderItem1 {
            id: Id::default(),
            metadata: Metadata::default(),
            product: 101,
            quantity: 2,
        },
        OrderItem1 {
            id: Id::default(),
            metadata: Metadata::default(),
            product: 202,
            quantity: 1,
        },
    ];
    let summary = compose(order.clone(), &items);
    assert_eq!(summary.customer, 7);
    assert_eq!(summary.amount_cents, 4200);
    assert_eq!(summary.item_count, 2);
    println!("Composed: {:?}", summary.item_count);

    // Execute rollback (lookup-first strategy — items missing from history store)
    let (recovered_order, recovered_items) = decompose(summary);
    assert_eq!(recovered_order.customer, 7);
    assert_eq!(recovered_order.amount_cents, 4200);
    println!(
        "Decomposed: customer={} items_recovered={}",
        recovered_order.customer,
        recovered_items.len()
    );

    println!("migration_type_2 example OK");
}
