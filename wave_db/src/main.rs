//! WaveDB — demo binary.
//!
//! Exercises the core types and the `#[wave_db]` proc-macro, then prints a
//! short report showing IDs, metadata, and page stats.

use wave_db::prelude::*;

// ── Example structs annotated with the proc-macro ────────────────────────────

/// A unique object: one live record per tenant.
#[wave_db(struct_id = 2)]
#[derive(Debug)]
pub struct UserProfile {
    pub id: Id,
    pub metadata: Metadata,
    pub display_name: String,
    pub email: String,
}

/// A non-unique object: many records per tenant (e.g. orders).
#[wave_db(NonUnique, struct_id = 3)]
#[derive(Debug)]
pub struct Order {
    pub id: Id,
    pub metadata: Metadata,
    pub amount_cents: u64,
    pub description: String,
}

/// Non-unique with a custom B+ tree conversion threshold.
#[wave_db(NonUnique, btree_threshold = 100, struct_id = 4)]
#[derive(Debug)]
pub struct Message {
    pub id: Id,
    pub metadata: Metadata,
    pub body: String,
}

// ─────────────────────────────────────────────────────────────────────────────

fn main() {
    let config = Config::default();
    println!("{config}");
    println!();

    // Create a tenant and struct IDs.
    let tenant = TenantId::new(0x_0000_CAFE_1234);
    let shard = ShardId::new(0);
    let profile_struct = StructId::new(UserProfile::STRUCT_ID);
    let order_struct = StructId::new(Order::STRUCT_ID);
    let msg_struct = StructId::new(Message::STRUCT_ID);

    // ── UserProfile ──────────────────────────────────────────────────────────
    let profile_id =
        Id::generate(tenant, shard, profile_struct, Slider::new(0)).expect("generate profile id");

    let profile = UserProfile {
        id: profile_id,
        metadata: Metadata::new(1, 999),
        display_name: "Alice".to_owned(),
        email: "alice@example.com".to_owned(),
    };

    println!("=== UserProfile ===");
    println!("  id:           {}", profile.id());
    println!("  tenant:       {}", profile.id().tenant_id());
    println!("  struct:       {}", profile.id().struct_id());
    println!("  created_at:   {}", profile.id().created_at());
    println!("  is_live:      {}", profile.is_live());
    println!("  is_first_ver: {}", profile.is_first_version());
    println!();

    // ── Order (NonUnique) ────────────────────────────────────────────────────
    let order_id =
        Id::generate(tenant, shard, order_struct, Slider::new(1)).expect("generate order id");

    let order = Order {
        id: order_id,
        metadata: Metadata::new(1, 999),
        amount_cents: 4999,
        description: "Widget Pro ×2".to_owned(),
    };

    println!(
        "=== Order (NonUnique, threshold={}) ===",
        Order::BTREE_THRESHOLD
    );
    println!("  id:      {}", order.id());
    println!("  amount:  {} cents", order.amount_cents);
    println!();

    // ── Message (custom btree_threshold) ────────────────────────────────────
    let msg_id =
        Id::generate(tenant, shard, msg_struct, Slider::new(2)).expect("generate message id");

    let msg = Message {
        id: msg_id,
        metadata: Metadata::new(1, 999),
        body: "Hello, WaveDB!".to_owned(),
    };

    println!(
        "=== Message (NonUnique, threshold={}) ===",
        Message::BTREE_THRESHOLD
    );
    println!("  id:   {}", msg.id());
    println!("  body: {}", msg.body);
    println!();

    // ── Page demo ────────────────────────────────────────────────────────────
    use wave_db::page::Page;
    let mut page = Page::new(config.page_size);
    let payload = b"serialised-profile-bytes";

    page.insert(profile_id, payload).expect("page insert");
    println!("=== Page ===");
    println!("  capacity:  {} bytes", config.page_size);
    println!("  occupied:  {} bytes", page.occupied());
    println!("  fill:      {}%", page.fill_pct());
    println!("  warn_full: {}", page.is_warn_full());
    println!(
        "  get(id):   {:?}",
        page.get(profile_id)
            .map(|b| std::str::from_utf8(b).unwrap())
    );
}
