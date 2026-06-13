//! End-to-end read-path tests (Phase 14): the full client loop over a real
//! cluster — `save` → `search`/`query` → `delete` — plus history flush to
//! the Slow-Node.
//!
//! The schema crate pattern mirrors a real application: the same structs
//! compile into the client and into the Quick-Nodes via
//! `ClusterSpec::registry`.

use wavedb::prelude::*;
use wavedb::query::Expr;
use wavedb_test_cluster::{ClusterSpec, TestCluster};

// ── Shared schema ────────────────────────────────────────────────────────────

#[wave_db(struct_id = 7400)]
pub struct Settings1 {
    pub display_name: String,
    pub volume: u64,
}
pub type Settings = Settings1;

#[wave_db(struct_id = 7401, NonUnique)]
pub struct Order1 {
    pub amount: u64,
    pub status: String,
}
pub type Order = Order1;

declare_objects! {
    pub mod app_objects {
        settings: [Settings1],
        orders: [Order1],
    }
}

fn spec() -> ClusterSpec {
    ClusterSpec {
        registry: Some(app_objects::REGISTRY),
        ..ClusterSpec::default()
    }
}

fn order(amount: u64, status: &str) -> Order {
    Order {
        id: Id::default(),
        metadata: Metadata::default(),
        amount,
        status: status.into(),
    }
}

// ── Unique ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn unique_save_then_search_roundtrip() {
    let cluster = TestCluster::spawn(spec()).await;
    let db = cluster.open_user_at_owner(7, cluster.owned_tenant()).await;

    // Nothing written yet → None.
    let none: Option<Settings> = Settings::search(&db).await.unwrap();
    assert!(none.is_none());

    Settings {
        id: Id::default(),
        metadata: Metadata::default(),
        display_name: "wave".into(),
        volume: 11,
    }
    .save(&db)
    .await
    .unwrap();

    let found: Settings = Settings::search(&db)
        .await
        .unwrap()
        .expect("saved record must be found");
    assert_eq!(found.display_name, "wave");
    assert_eq!(found.volume, 11);
    // The engine stamped its assigned Id into the stored bytes.
    assert_ne!(found.id, Id::ZERO);
    assert_eq!(found.id.tenant_id(), cluster.owned_tenant());

    drop(db);
    cluster.shutdown().await;
}

#[tokio::test]
async fn unique_save_twice_returns_latest() {
    let cluster = TestCluster::spawn(spec()).await;
    let db = cluster.open_user_at_owner(7, cluster.owned_tenant()).await;

    for (i, name) in ["first", "second"].iter().enumerate() {
        Settings {
            id: Id::default(),
            metadata: Metadata::default(),
            display_name: (*name).into(),
            volume: i as u64,
        }
        .save(&db)
        .await
        .unwrap();
    }

    let found: Settings = Settings::search(&db).await.unwrap().expect("found");
    assert_eq!(found.display_name, "second", "anchor must have rotated");

    drop(db);
    cluster.shutdown().await;
}

// ── NonUnique query ──────────────────────────────────────────────────────────

#[tokio::test]
async fn nonunique_save_then_query_all() {
    let cluster = TestCluster::spawn(spec()).await;
    let db = cluster.open_user_at_owner(7, cluster.owned_tenant()).await;

    for amount in [10u64, 20, 30] {
        order(amount, "open").save(&db).await.unwrap();
    }

    let all: Vec<Order> = Order::query(&db, Expr::all()).await.unwrap();
    assert_eq!(all.len(), 3);
    // Commit order is preserved.
    assert_eq!(
        all.iter().map(|o| o.amount).collect::<Vec<_>>(),
        vec![10, 20, 30]
    );

    drop(db);
    cluster.shutdown().await;
}

#[tokio::test]
async fn nonunique_filtered_query() {
    let cluster = TestCluster::spawn(spec()).await;
    let db = cluster.open_user_at_owner(7, cluster.owned_tenant()).await;

    order(50, "open").save(&db).await.unwrap();
    order(150, "open").save(&db).await.unwrap();
    order(250, "shipped").save(&db).await.unwrap();

    // Numeric comparison on a stack field.
    let big: Vec<Order> =
        Order::query(&db, Expr::gt("amount", 100u64)).await.unwrap();
    assert_eq!(big.len(), 2);

    // Compound: numeric AND string (heap field).
    let shipped_big: Vec<Order> = Order::query(
        &db,
        Expr::and(Expr::gt("amount", 100u64), Expr::eq("status", "shipped")),
    )
    .await
    .unwrap();
    assert_eq!(shipped_big.len(), 1);
    assert_eq!(shipped_big[0].amount, 250);

    // Typed column constants from the macro.
    let typed: Vec<Order> =
        Order::query(&db, Order::amount.lte(50u64)).await.unwrap();
    assert_eq!(typed.len(), 1);
    assert_eq!(typed[0].amount, 50);

    drop(db);
    cluster.shutdown().await;
}

#[tokio::test]
async fn delete_tombstones_record() {
    let cluster = TestCluster::spawn(spec()).await;
    let db = cluster.open_user_at_owner(7, cluster.owned_tenant()).await;

    order(1, "a").save(&db).await.unwrap();
    order(2, "b").save(&db).await.unwrap();

    let all: Vec<Order> = Order::query(&db, Expr::all()).await.unwrap();
    assert_eq!(all.len(), 2);

    // Delete via the engine-assigned id carried in the queried record.
    let victim = all.iter().find(|o| o.amount == 1).unwrap();
    db.delete(victim.id).await.unwrap();

    let remaining: Vec<Order> = Order::query(&db, Expr::all()).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].amount, 2);

    drop(db);
    cluster.shutdown().await;
}

// ── History flush (sync_to_slow) ─────────────────────────────────────────────

#[tokio::test]
async fn sync_to_slow_ships_history_to_slow_node() {
    let cluster = TestCluster::spawn(spec()).await;
    let tenant = cluster.owned_tenant();
    let db = cluster.open_user_at_owner(7, tenant).await;

    order(11, "x").save(&db).await.unwrap();
    order(22, "y").save(&db).await.unwrap();

    assert!(cluster.slow_node.store.is_empty());

    // The owner ships its pending history.
    let owner = cluster.owner_idx(tenant);
    cluster.quick_nodes[owner].node.sync_to_slow().await;

    assert_eq!(
        cluster.slow_node.store.len(),
        2,
        "both committed records must reach the slow node"
    );
    // Re-sync with nothing pending is a no-op.
    cluster.quick_nodes[owner].node.sync_to_slow().await;
    assert_eq!(cluster.slow_node.store.len(), 2);

    drop(db);
    cluster.shutdown().await;
}
