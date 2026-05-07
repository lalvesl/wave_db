//! WaveDB — demo binary.
//!
//! Exercises the core types, the `#[wave_db]` proc-macro, the two-slot storage
//! engine (anchor + versioned), mutation chain, and journal.

use wave_db::prelude::*;

// ── Example structs annotated with the proc-macro ────────────────────────────

/// A unique object: one live record per tenant.
#[wave_db(struct_id = 2, struct_version = 1)]
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
pub mod message {
    use super::*;
    pub type Message = Message1;

    #[wave_db(NonUnique, btree_threshold = 100, struct_id = 4, struct_version = 1)]
    #[derive(Debug)]
    pub struct Message1 {
        pub id: Id,
        pub metadata: Metadata,
        pub body: String,
    }
}

pub use message::Message;

// ─────────────────────────────────────────────────────────────────────────────

fn main() {
    let tenant = TenantId::new(0x_0000_CAFE_1234);
    let shard = ShardId::new(0);

    println!("╔══════════════════════════════════════════════╗");
    println!("║            WaveDB — Research Demo            ║");
    println!("╚══════════════════════════════════════════════╝\n");

    // ── Config ───────────────────────────────────────────────────────────────
    let config = Config::dev();
    println!("=== Config (dev) ===");
    println!("{config}\n");

    // ── Inline Store demo ────────────────────────────────────────────────────
    println!("=== Store: Insert → Mutate → Delete → Re-insert ===\n");

    let mut store = Store::new(config, AnchorMode::Inline);

    // Build two IDs for the same tenant/struct (two different timestamps).
    let profile_struct = StructId::new(UserProfile::STRUCT_ID);
    let v1_id = Id::generate(tenant, shard, profile_struct, Slider::new(0))
        .expect("generate v1 id");

    // Insert v1.
    let v1_data = b"Alice | alice@example.com".to_vec();
    let v1 = StoredRecord::new_first(v1_id, UserProfile::STRUCT_VERSION, 999, v1_data.clone());
    store.insert(v1).expect("insert v1");
    println!("Inserted v1  id={}", v1_id);

    // Inline fast-path read (no versioned lookup needed).
    let inline = store
        .get_live_inline(profile_struct, tenant)
        .expect("inline read");
    println!("  inline read: {:?}", std::str::from_utf8(inline).unwrap());

    // Mutate → v2.
    let v2_id = Id::generate(tenant, shard, profile_struct, Slider::new(1))
        .expect("generate v2 id");
    let v2_data = b"Alice Smith | alice@example.com".to_vec();
    let v2 = StoredRecord::new_first(v2_id, UserProfile::STRUCT_VERSION, 999, v2_data.clone());
    store.mutate(v1_id, v2).expect("mutate to v2");
    println!("Mutated to v2 id={}", v2_id);

    // Mutate → v3.
    let v3_id = Id::generate(tenant, shard, profile_struct, Slider::new(2))
        .expect("generate v3 id");
    let v3_data = b"Alice Smith | alice.smith@example.com".to_vec();
    let v3 = StoredRecord::new_first(v3_id, UserProfile::STRUCT_VERSION, 999, v3_data);
    store.mutate(v2_id, v3).expect("mutate to v3");
    println!("Mutated to v3 id={}", v3_id);

    // ── Live read ─────────────────────────────────────────────────────────────
    println!();
    let live = store.get_live(profile_struct, tenant).expect("get live");
    println!("Live record: {:?}", std::str::from_utf8(&live.data).unwrap());
    println!("  old_mod_id={}", live.metadata.old_modification_id);
    println!(
        "  is_first_ver={}  is_live={}",
        live.metadata.is_first_version(),
        live.metadata.is_live()
    );

    // ── History walk (newest → oldest) ────────────────────────────────────────
    println!();
    println!("=== History (newest → oldest) ===");
    let chain = store.history_backward(profile_struct, tenant);
    for (i, rec) in chain.iter().enumerate() {
        println!(
            "  [{}] {} | old={} | new={}",
            i,
            std::str::from_utf8(&rec.data).unwrap(),
            rec.metadata.old_modification_id,
            rec.metadata.new_modification_id,
        );
    }

    // ── Stale version protection ──────────────────────────────────────────────
    println!();
    println!("=== Stale-version protection ===");
    let stale_id = Id::generate(tenant, shard, profile_struct, Slider::new(9))
        .expect("stale id");
    let stale_rec =
        StoredRecord::new_first(stale_id, UserProfile::STRUCT_VERSION, 999, b"stale".to_vec());
    match store.mutate(v1_id, stale_rec) {
        Err(StoreError::StaleVersion) => println!("  Correctly rejected stale mutation (v1 is no longer live)"),
        other => println!("  Unexpected: {other:?}"),
    }

    // ── Delete ────────────────────────────────────────────────────────────────
    println!();
    println!("=== Delete ===");
    store.delete(profile_struct, tenant).expect("delete");
    println!("  Deleted. get_live → {:?}", store.get_live(profile_struct, tenant));
    let anchor = store.anchor(profile_struct, tenant).expect("anchor");
    println!(
        "  Anchor is tombstone={}, most_recent_id={}",
        !anchor.is_live(),
        anchor.most_recent_version_id().unwrap()
    );

    // History still works after deletion.
    let chain = store.history_backward(profile_struct, tenant);
    println!("  History depth after delete: {}", chain.len());

    // Re-insert after delete.
    let v4_id = Id::generate(tenant, shard, profile_struct, Slider::new(3))
        .expect("v4 id");
    let v4 = StoredRecord::new_first(v4_id, UserProfile::STRUCT_VERSION, 999, b"Bob | bob@example.com".to_vec());
    store.insert(v4).expect("re-insert after delete");
    println!("  Re-inserted v4  id={}", v4_id);
    println!(
        "  New live: {:?}",
        std::str::from_utf8(&store.get_live(profile_struct, tenant).unwrap().data).unwrap()
    );

    // ── Inbound references ────────────────────────────────────────────────────
    println!();
    println!("=== Inbound references ===");
    let index_ref = Id::generate(tenant, shard, StructId::new(10), Slider::new(0))
        .expect("index ref id");
    store
        .add_inbound_ref(profile_struct, tenant, index_ref)
        .expect("add inbound ref");
    let anchor = store.anchor(profile_struct, tenant).unwrap();
    println!("  inbound_refs count: {}", anchor.inbound_refs.len());

    // Inbound refs survive a mutation.
    let v5_id = Id::generate(tenant, shard, profile_struct, Slider::new(4))
        .expect("v5 id");
    let v5 = StoredRecord::new_first(v5_id, UserProfile::STRUCT_VERSION, 999, b"Bob Smith | bob@example.com".to_vec());
    store.mutate(v4_id, v5).expect("mutate v4→v5");
    let anchor = store.anchor(profile_struct, tenant).unwrap();
    println!("  inbound_refs after mutation: {}", anchor.inbound_refs.len());

    // ── Journal stats ─────────────────────────────────────────────────────────
    println!();
    println!("=== Journal stats ===");
    let stats = store.journal().stats();
    println!("  inserts:   {}", stats.inserts);
    println!("  mutations: {}", stats.mutations);
    println!("  deletes:   {}", stats.deletes);
    println!("  total entries: {}", store.journal().len());

    // ── Page demo ─────────────────────────────────────────────────────────────
    println!();
    println!("=== Page ===");
    use wave_db::page::Page;
    let mut page = Page::new(store.config().page_size);
    let payload = b"serialised-profile-bytes";
    page.insert(v5_id, payload).expect("page insert");
    println!("  capacity:  {} bytes", store.config().page_size);
    println!("  occupied:  {} bytes", page.occupied());
    println!("  fill:      {}%", page.fill_pct());
    println!(
        "  get(id):   {:?}",
        page.get(v5_id).map(|b| std::str::from_utf8(b).unwrap())
    );

    // ── Store summary ─────────────────────────────────────────────────────────
    println!();
    println!("=== Store summary ===");
    println!("  live anchors:     {}", store.live_count());
    println!("  total anchors:    {}", store.anchor_count());
    println!("  versioned records:{}", store.versioned_count());
    println!("  (live < total because deleted anchor is a tombstone,");
    println!("   versioned count includes all historical versions)");
}
