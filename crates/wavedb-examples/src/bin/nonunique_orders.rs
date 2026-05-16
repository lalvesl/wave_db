//! NonUnique shape end-to-end: write multiple orders, filter by amount, delete.
//!
//! Roles:
//!   client  — opens a `Db`, writes three orders, queries with `Expr::gt`, deletes one.
//!   server  — simulated in-process via `ChannelTransport` (no network required).
//!
//! Run with:
//!   cargo run --bin nonunique_orders

use wavedb::prelude::*;
use wavedb_net::ChannelTransport;

// ── Schema ───────────────────────────────────────────────────────────────────
//
// `#[wave_db]` auto-derives `Debug, Clone, Serialize, Deserialize` and
// auto-impls `NonUniqueObject` (query, update, delete) for NonUnique structs.

#[wave_db(struct_id = 10, NonUnique, btree_threshold = 50)]
#[derive(PartialEq, Eq)]
pub struct Order1 {
    pub amount: u64,
    pub customer: u64,
}
pub type Order = Order1;

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let orders_all = vec![
        Order { amount: 50,  customer: 1, ..Default::default() },
        Order { amount: 150, customer: 2, ..Default::default() },
        Order { amount: 200, customer: 3, ..Default::default() },
    ];
    let orders_filtered: Vec<Order> = orders_all.iter().filter(|o| o.amount > 100).cloned().collect();
    let orders_after_delete: Vec<Order> = orders_all.iter().filter(|o| o.amount != 150).cloned().collect();

    let encode_query = |records: &[Order]| -> Vec<u8> {
        let entries: Vec<(u8, Vec<u8>)> = records
            .iter()
            .map(|r| (Order::STRUCT_VERSION, postcard::to_allocvec(r).unwrap()))
            .collect();
        postcard::to_allocvec(&entries).unwrap()
    };

    let enc_all = encode_query(&orders_all);
    let enc_filtered = encode_query(&orders_filtered);
    let enc_after = encode_query(&orders_after_delete);

    let (transport, mut server) = ChannelTransport::pair();
    tokio::spawn(async move {
        server.reply_connect("ws://owner:7700", "ws://backup:7700").await;
        server.reply_ok_n(3).await;                   // 3 writes
        server.reply_data(enc_all).await;             // query all
        server.reply_data(enc_filtered).await;        // query amount > 100
        server.reply_ok().await;                      // delete
        server.reply_data(enc_after).await;           // query after delete
    });

    let db = Db::open_with_transport(transport, /* user= */ 1, /* tenant= */ 42).await?;

    // Write three orders
    for order in &orders_all {
        order.clone().save(&db).await?;
    }
    println!("Wrote {} orders", orders_all.len());

    // Query all
    let all = Order::query(&db, Expr::all()).await?;
    assert_eq!(all.len(), 3);
    println!("All orders: {}", all.len());

    // Query filtered: amount > 100
    let large = Order::query(&db, Expr::gt("amount", 100u64)).await?;
    assert_eq!(large.len(), 2);
    println!("Orders with amount > 100: {}", large.len());

    // Delete the 150-amount order
    let to_delete = orders_all[1].clone();
    to_delete.delete(&db).await?;
    println!("Deleted order with amount=150");

    // Confirm deletion
    let remaining = Order::query(&db, Expr::all()).await?;
    assert_eq!(remaining.len(), 2);
    assert!(remaining.iter().all(|o| o.amount != 150));
    println!("Remaining orders: {}", remaining.len());

    println!("nonunique_orders example OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_main() {
        super::main().unwrap();
    }
}
