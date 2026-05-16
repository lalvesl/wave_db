//! Anchor addressing: `primary_anchor`, `secondary_anchor`, `btree_threshold`.
//!
//! Every record in WaveDB lives at an **anchor** — a stable address that
//! never changes when the underlying data is mutated.  By default the
//! anchor is hashed at `(STRUCT_ID, TENANT_ID, node-allocated SHARD_ID)`.
//! The `#[wave_db]` macro lets a struct opt into two stronger addressing
//! strategies:
//!
//! * `primary_anchor = field`
//!   Replace the node-allocated `SHARD_ID` with `hash(field)`. Anyone who
//!   knows the field value can resolve the record in one IO. Two records
//!   with the same value collide on the same anchor → "primary key"
//!   semantics for free.
//!
//! * `secondary_anchor = "field"` (repeatable, supports compound keys
//!   written as `"a, b"`)
//!   Extra anchor addresses that point back to the primary anchor. One
//!   alias accessor per declared key. Cheaper than a full discrete index
//!   when you only need point lookups.
//!
//! * `btree_threshold = K`
//!   Per-struct override for the adaptive index's array→B+tree promotion
//!   threshold. Small tables stay as a flat array (cheap scan); past `K`
//!   entries the index converts to a B+tree (cheap range/point lookup).
//!
//! This example declares one struct that uses *all three* and one struct
//! that uses the default addressing, then prints the compile-time anchor
//! metadata the macro injects.
//!
//! Run with:
//!   cargo run --bin anchors

use wavedb::prelude::*;
use wavedb_net::ChannelTransport;

// ── Struct with explicit anchors ─────────────────────────────────────────────

#[wave_db(
    struct_id = 50,
    NonUnique,
    primary_anchor = username,
    secondary_anchor = "email",
    secondary_anchor = "department, employee_number",
    btree_threshold = 32,
)]
#[derive(PartialEq, Eq)]
pub struct Employee1 {
    pub username: String,
    pub email: String,
    pub department: String,
    pub employee_number: u32,
    pub display_name: String,
}
pub type Employee = Employee1;

// ── Struct with default addressing ───────────────────────────────────────────

#[wave_db(struct_id = 51, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Session1 {
    pub token: String,
    pub user: u64,
}
pub type Session = Session1;

// ── Wire helper ──────────────────────────────────────────────────────────────

fn encode_query<T>(records: &[T]) -> Vec<u8>
where
    T: serde::Serialize + wavedb_core::WaveDbStruct,
{
    let entries: Vec<(u8, Vec<u8>)> = records
        .iter()
        .map(|r| (T::STRUCT_VERSION, postcard::to_allocvec(r).unwrap()))
        .collect();
    postcard::to_allocvec(&entries).unwrap()
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("== Employee (custom anchors) ==");
    println!("  primary_anchor_field    = {:?}", Employee::primary_anchor_field());
    println!("  SECONDARY_ANCHOR_FIELDS = {:?}", Employee::SECONDARY_ANCHOR_FIELDS);
    println!("  BTREE_THRESHOLD         = {}", Employee::BTREE_THRESHOLD);
    println!("  SHAPE                   = {:?}", Employee::SHAPE);
    println!("  STRUCT_ID               = {}", Employee::STRUCT_ID);

    assert_eq!(Employee::primary_anchor_field(), "username");
    assert_eq!(Employee::SECONDARY_ANCHOR_FIELDS, &["email", "department, employee_number"]);
    assert_eq!(Employee::BTREE_THRESHOLD, 32);

    println!();
    println!("== Session (default addressing) ==");
    println!("  STRUCT_ID = {}", Session::STRUCT_ID);
    println!("  SHAPE     = {:?}", Session::SHAPE);
    println!("  (no anchor accessors emitted — uses node-allocated SHARD_ID)");

    let alice = Employee {
        username: "alice".into(),
        email: "alice@corp.example".into(),
        department: "platform".into(),
        employee_number: 17,
        display_name: "Alice Liddell".into(),
        ..Default::default()
    };
    let bob = Employee {
        username: "bob".into(),
        email: "bob@corp.example".into(),
        department: "platform".into(),
        employee_number: 18,
        display_name: "Bob Cratchit".into(),
        ..Default::default()
    };
    let everyone = vec![alice.clone(), bob.clone()];
    let just_alice = vec![alice.clone()];

    let enc_just_alice = encode_query(&just_alice);
    let enc_everyone = encode_query(&everyone);

    let (transport, mut server) = ChannelTransport::pair();
    tokio::spawn(async move {
        server.reply_connect("ws://owner:7700", "ws://backup:7700").await;
        server.reply_ok_n(2).await;                        // write alice, bob
        server.reply_data(enc_just_alice.clone()).await;   // query username == "alice"
        server.reply_data(enc_just_alice.clone()).await;   // query email == "alice@..."
        server.reply_data(enc_just_alice).await;           // query (department, employee_number)
        server.reply_data(enc_everyone).await;             // query all
    });

    let db = Db::open_with_transport(transport, /* user= */ 1, /* tenant= */ 42).await?;

    alice.clone().save(&db).await?;
    bob.clone().save(&db).await?;
    println!();
    println!("Wrote 2 employees");

    let by_username = Employee::query(&db, Expr::eq("username", "alice")).await?;
    assert_eq!(by_username.len(), 1);
    println!("by username 'alice'      → {}", by_username[0].display_name);

    let by_email = Employee::query(&db, Expr::eq("email", "alice@corp.example")).await?;
    assert_eq!(by_email.len(), 1);
    assert_eq!(by_email[0].username, by_username[0].username);
    println!("by email                 → {}", by_email[0].display_name);

    let by_dep_num = Employee::query(
        &db,
        Expr::and(
            Expr::eq("department", "platform"),
            Expr::eq("employee_number", 17u64),
        ),
    )
    .await?;
    assert_eq!(by_dep_num.len(), 1);
    println!("by (department, employee_number) → {}", by_dep_num[0].display_name);

    let all = Employee::query(&db, Expr::all()).await?;
    assert_eq!(all.len(), 2);
    println!("all employees            → {}", all.len());

    println!("anchors example OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_main() {
        super::main().unwrap();
    }
}
