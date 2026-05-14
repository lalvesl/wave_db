//! NestedNonUnique shape: invoices with tightly-bound line items.
//!
//! `Invoice` is NonUnique — many per tenant, queryable at the top level.
//! `InvoiceLine` is NestedNonUnique — many per invoice, **not** queryable at
//! the top level; in production storage, line-item lookups go through the
//! parent invoice's anchor rather than a global InvoiceLine index.
//!
//! Run with:
//!   cargo run --bin nested_invoice_lines

use wavedb::object::{do_query_non_unique, do_write};
use wavedb::prelude::*;
use wavedb_net::MockTransport;
use wavedb_net::mock::ScriptedReply;

// ── Schema ───────────────────────────────────────────────────────────────────

#[wave_db(struct_id = 20, NonUnique)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Invoice1 {
    pub id: Id,
    pub metadata: Metadata,
    pub customer: u64,
    pub total_cents: u64,
}
pub type Invoice = Invoice1;

#[wave_db(struct_id = 21, NestedNonUnique)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InvoiceLine1 {
    pub id: Id,
    pub metadata: Metadata,
    pub product: u64,
    pub quantity: u32,
    pub unit_cents: u64,
}
pub type InvoiceLine = InvoiceLine1;

impl NonUniqueObject for Invoice {
    async fn query(db: &Db, expr: Expr) -> wavedb_core::Result<Vec<Self>> {
        do_query_non_unique::<Self>(db, expr).await
    }

    async fn update(self, db: &Db) -> wavedb_core::Result<()> {
        do_write(db, &self).await
    }

    async fn delete(self, db: &Db) -> wavedb_core::Result<()> {
        use wavedb::object::do_delete;
        do_delete(db, self.id.raw()).await
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Verify the shape constants are as expected.
    assert_eq!(Invoice::SHAPE, Shape::NonUnique);
    assert_eq!(InvoiceLine::SHAPE, Shape::NestedNonUnique);
    println!(
        "Invoice SHAPE={:?}  InvoiceLine SHAPE={:?}",
        Invoice::SHAPE,
        InvoiceLine::SHAPE,
    );

    let invoice = Invoice {
        id: Id::default(),
        metadata: Metadata::default(),
        customer: 7,
        total_cents: 3000,
    };
    let line_a = InvoiceLine {
        id: Id::default(),
        metadata: Metadata::default(),
        product: 101,
        quantity: 2,
        unit_cents: 1000,
    };
    let line_b = InvoiceLine {
        id: Id::default(),
        metadata: Metadata::default(),
        product: 202,
        quantity: 1,
        unit_cents: 1000,
    };
    let invoices = vec![invoice.clone()];
    let lines = vec![line_a.clone(), line_b.clone()];

    let mock = MockTransport::new();
    mock.push(ScriptedReply::connect(
        "ws://owner:7700",
        "ws://backup:7700",
    ));
    // write invoice → ok
    mock.push(ScriptedReply::ok(Vec::new()));
    // write line_a → ok
    mock.push(ScriptedReply::ok(Vec::new()));
    // write line_b → ok
    mock.push(ScriptedReply::ok(Vec::new()));
    // query invoices → [invoice]
    mock.push(ScriptedReply::ok(postcard::to_allocvec(&invoices)?));
    // query lines through parent → [line_a, line_b]
    // In production this request carries the parent invoice ID so the engine
    // looks up only lines attached to that invoice, not all InvoiceLines.
    mock.push(ScriptedReply::ok(postcard::to_allocvec(&lines)?));

    let db = Db::open_with_transport(mock, /* user= */ 1, /* tenant= */ 42).await?;

    // Write parent invoice
    invoice.update(&db).await?;

    // Write nested lines via do_write (bypasses top-level query index)
    do_write(&db, &line_a).await?;
    do_write(&db, &line_b).await?;

    // Query invoices at the top level — works because Invoice is NonUnique
    let found_invoices = Invoice::query(&db, Expr::all()).await?;
    assert_eq!(found_invoices.len(), 1);
    println!("Invoices: {}", found_invoices.len());

    // Query lines through the parent invoice anchor.
    // Production storage routes this to the parent's subtree, not the global
    // InvoiceLine index (which doesn't exist — InvoiceLine is NestedNonUnique).
    let found_lines = do_query_non_unique::<InvoiceLine>(&db, Expr::eq("product", 101u64)).await?;
    assert_eq!(found_lines.len(), 2);
    println!(
        "Invoice lines for customer {}: {}",
        found_invoices[0].customer,
        found_lines.len()
    );

    println!("nested_invoice_lines example OK");
    Ok(())
}
