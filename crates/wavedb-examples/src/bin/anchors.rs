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
use wavedb_net::MockTransport;
use wavedb_net::mock::ScriptedReply;

// ── Struct with explicit anchors ─────────────────────────────────────────────
//
// `username` is the natural primary key. `email` and the compound
// `(department, employee_number)` are alternate keys that should resolve
// to the same record. `btree_threshold = 32` keeps the per-tenant index
// as a flat array up to 32 employees, then promotes to a B+tree.

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
//
// No `primary_anchor` → the writing Quick-Node mints a fresh `SHARD_ID`
// for each record. No `secondary_anchor` → no alias addresses. No
// `btree_threshold` → uses the engine default.

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
    // ── Compile-time anchor metadata ─────────────────────────────────────────
    //
    // The macro emits these for any struct that declares anchors. The
    // storage engine reads them at runtime to compute anchor addresses;
    // applications can introspect them for tooling / diagnostics.
    println!("== Employee (custom anchors) ==");
    println!(
        "  primary_anchor_field    = {:?}",
        Employee::primary_anchor_field()
    );
    println!(
        "  SECONDARY_ANCHOR_FIELDS = {:?}",
        Employee::SECONDARY_ANCHOR_FIELDS
    );
    println!("  BTREE_THRESHOLD         = {}", Employee::BTREE_THRESHOLD);
    println!("  SHAPE                   = {:?}", Employee::SHAPE);
    println!("  STRUCT_ID               = {}", Employee::STRUCT_ID);

    assert_eq!(Employee::primary_anchor_field(), "username");
    assert_eq!(
        Employee::SECONDARY_ANCHOR_FIELDS,
        &["email", "department, employee_number"],
    );
    assert_eq!(Employee::BTREE_THRESHOLD, 32);

    // ── Default-addressing contrast ──────────────────────────────────────────
    //
    // `Session` has no `primary_anchor` so `primary_anchor_field()` is not
    // emitted, no `SECONDARY_ANCHOR_FIELDS` const, no `BTREE_THRESHOLD`.
    // The lines below would not compile — left as comments on purpose.
    //
    //     let _ = Session::primary_anchor_field();      // ← no such fn
    //     let _ = Session::SECONDARY_ANCHOR_FIELDS;     // ← no such const
    //     let _ = Session::BTREE_THRESHOLD;             // ← no such const
    println!();
    println!("== Session (default addressing) ==");
    println!("  STRUCT_ID = {}", Session::STRUCT_ID);
    println!("  SHAPE     = {:?}", Session::SHAPE);
    println!("  (no anchor accessors emitted — uses node-allocated SHARD_ID)");

    // ── End-to-end write / lookup against a mock transport ──────────────────
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

    let mock = MockTransport::new();
    mock.push(ScriptedReply::connect(
        "ws://owner:7700",
        "ws://backup:7700",
    ));
    // 2 writes — alice, bob
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    // query username == "alice" → primary anchor hit
    mock.push(ScriptedReply::ok(encode_query(&just_alice)));
    // query email == "alice@corp.example" → secondary anchor → primary
    mock.push(ScriptedReply::ok(encode_query(&just_alice)));
    // query (department, employee_number) == ("platform", 17) → other secondary
    mock.push(ScriptedReply::ok(encode_query(&just_alice)));
    // query all → both
    mock.push(ScriptedReply::ok(encode_query(&everyone)));

    let db = Db::open_with_transport(mock, /* user= */ 1, /* tenant= */ 42).await?;

    alice.clone().update(&db).await?;
    bob.clone().update(&db).await?;
    println!();
    println!("Wrote 2 employees");

    // Lookup via the *primary* anchor field. In production the engine
    // hashes "alice" into the SHARD_ID slot and resolves the anchor in
    // one IO — no index walk.
    let by_username = Employee::query(&db, Expr::eq("username", "alice")).await?;
    assert_eq!(by_username.len(), 1);
    println!("by username 'alice'      → {}", by_username[0].display_name);

    // Lookup via the *secondary* anchor on email. Same record, different
    // address — the secondary anchor holds a pointer back to the primary.
    let by_email = Employee::query(&db, Expr::eq("email", "alice@corp.example")).await?;
    assert_eq!(by_email.len(), 1);
    assert_eq!(by_email[0].username, by_username[0].username);
    println!("by email                 → {}", by_email[0].display_name);

    // Lookup via the *compound* secondary anchor.
    let by_dep_num = Employee::query(
        &db,
        Expr::and(
            Expr::eq("department", "platform"),
            Expr::eq("employee_number", 17u64),
        ),
    )
    .await?;
    assert_eq!(by_dep_num.len(), 1);
    println!(
        "by (department, employee_number) → {}",
        by_dep_num[0].display_name
    );

    let all = Employee::query(&db, Expr::all()).await?;
    assert_eq!(all.len(), 2);
    println!("all employees            → {}", all.len());

    println!("anchors example OK");
    Ok(())
}
