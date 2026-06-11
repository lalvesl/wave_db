//! NestedNonUnique shape: invoices with tightly-bound line items.
//!
//! `Invoice` is NonUnique — many per tenant, queryable at the top level.
//! `InvoiceLine` is NestedNonUnique — many per invoice, **not** queryable at
//! the top level; in production storage, line-item lookups go through the
//! parent invoice's anchor rather than a global InvoiceLine index.
//!
//! Run with:
//!   cargo run --bin nested_invoice_lines

use wavedb::prelude::*;
use wavedb_net::ChannelTransport;

// ── Schema ───────────────────────────────────────────────────────────────────
//
// `#[wave_db]` auto-derives `Debug, Clone, Serialize, Deserialize` and
// auto-impls `NonUniqueObject` for both shapes (Invoice and InvoiceLine).

#[wave_db(struct_id = 20, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Invoice1 {
    pub customer: u64,
    pub total_cents: u64,
}
pub type Invoice = Invoice1;

#[wave_db(struct_id = 21, NestedNonUnique)]
#[derive(PartialEq, Eq)]
pub struct InvoiceLine1 {
    pub product: u64,
    pub quantity: u32,
    pub unit_cents: u64,
}
pub type InvoiceLine = InvoiceLine1;

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(Invoice::SHAPE, Shape::NonUnique);
    assert_eq!(InvoiceLine::SHAPE, Shape::NestedNonUnique);
    println!(
        "Invoice SHAPE={:?}  InvoiceLine SHAPE={:?}",
        Invoice::SHAPE,
        InvoiceLine::SHAPE,
    );

    let invoice = Invoice {
        customer: 7,
        total_cents: 3000,
        ..Default::default()
    };
    let line_a = InvoiceLine {
        product: 101,
        quantity: 2,
        unit_cents: 1000,
        ..Default::default()
    };
    let line_b = InvoiceLine {
        product: 202,
        quantity: 1,
        unit_cents: 1000,
        ..Default::default()
    };
    let invoices = vec![invoice.clone()];
    let lines = vec![line_a.clone(), line_b.clone()];

    #[allow(clippy::items_after_statements)]
    fn encode_query<T>(records: &[T]) -> Vec<u8>
    where
        T: wavedb_core::Wire + wavedb_core::WaveDbStruct,
    {
        let entries: Vec<(u8, Vec<u8>)> = records
            .iter()
            .map(|r| {
                (T::STRUCT_VERSION, wavedb_core::wire::to_wire(r).unwrap())
            })
            .collect();
        wavedb_core::wire::to_wire(&entries).unwrap()
    }

    let (transport, mut server) = ChannelTransport::pair();
    tokio::spawn(async move {
        server
            .reply_connect("ws://owner:7700", "ws://backup:7700")
            .await;
        server.reply_ok_n(3).await; // write invoice, line_a, line_b
        server.reply_data(encode_query(&invoices)).await; // query invoices
        // Production routes this to the parent invoice's subtree, not a global
        // InvoiceLine index (which doesn't exist — InvoiceLine is NestedNonUnique).
        server.reply_data(encode_query(&lines)).await; // query lines through parent
    });

    let db = Db::open_with_transport(
        transport, /* user= */ 1, /* tenant= */ 42,
    )
    .await?;

    invoice.save(&db).await?;
    line_a.save(&db).await?;
    line_b.save(&db).await?;

    let found_invoices = Invoice::query(&db, Expr::all()).await?;
    assert_eq!(found_invoices.len(), 1);
    println!("Invoices: {}", found_invoices.len());

    let found_lines =
        InvoiceLine::query(&db, InvoiceLine::product.eq(101u64)).await?;
    assert_eq!(found_lines.len(), 2);
    println!(
        "Invoice lines for customer {}: {}",
        found_invoices[0].customer,
        found_lines.len()
    );

    println!("nested_invoice_lines example OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_main() {
        super::main().unwrap();
    }
}
