//! Recursive NestedNonUnique: nesting does not stop at one level.
//!
//! `Project` is NonUnique — many per tenant, queryable at the top level.
//! `Task` is NestedNonUnique — many per project.
//! `ChecklistItem` is NestedNonUnique **under Task**, which is itself
//! NestedNonUnique — the child collection recurses.  Every level stays
//! clustered under the same top-level parent's address space, so a deep
//! read never leaves the parent's pages.
//!
//! Storage routes trackers and elements purely by Id (see
//! `nested_within_nested_roundtrip` in `wavedb-storage`), so a grandchild
//! level costs the same as the first.
//!
//! Run with:
//!   cargo run --bin nested_recursive

use wavedb::prelude::*;
use wavedb_net::ChannelTransport;

// ── Schema ───────────────────────────────────────────────────────────────────

#[wave_db(struct_id = 30, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Project1 {
    pub name_hash: u64,
}
pub type Project = Project1;

#[wave_db(struct_id = 31, NestedNonUnique)]
#[derive(PartialEq, Eq)]
pub struct Task1 {
    pub title_hash: u64,
    pub done: u8,
}
pub type Task = Task1;

// Nested **under Task** — a NestedNonUnique child of a NestedNonUnique
// parent.  Same shape, one level deeper.
#[wave_db(struct_id = 32, NestedNonUnique)]
#[derive(PartialEq, Eq)]
pub struct ChecklistItem1 {
    pub label_hash: u64,
    pub checked: u8,
}
pub type ChecklistItem = ChecklistItem1;

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(Project::SHAPE, Shape::NonUnique);
    assert_eq!(Task::SHAPE, Shape::NestedNonUnique);
    assert_eq!(ChecklistItem::SHAPE, Shape::NestedNonUnique);
    println!(
        "Project={:?}  Task={:?}  ChecklistItem={:?}",
        Project::SHAPE,
        Task::SHAPE,
        ChecklistItem::SHAPE,
    );

    let project = Project {
        name_hash: 0xCAFE,
        ..Default::default()
    };
    let task = Task {
        title_hash: 0xBEEF,
        done: 0,
        ..Default::default()
    };
    let item_a = ChecklistItem {
        label_hash: 1,
        checked: 1,
        ..Default::default()
    };
    let item_b = ChecklistItem {
        label_hash: 2,
        checked: 0,
        ..Default::default()
    };
    let tasks = vec![task.clone()];
    let items = vec![item_a.clone(), item_b.clone()];

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
        server.reply_ok_n(4).await; // project, task, item_a, item_b
        // Production resolves each level through its parent's subtree:
        // project anchor → task tracker → checklist tracker.  No global
        // Task or ChecklistItem index exists at any depth.
        server.reply_data(encode_query(&tasks)).await;
        server.reply_data(encode_query(&items)).await;
    });

    let db = Db::open_with_transport(
        transport, /* user= */ 1, /* tenant= */ 42,
    )
    .await?;

    project.save(&db).await?;
    task.save(&db).await?;
    item_a.save(&db).await?;
    item_b.save(&db).await?;

    let found_tasks = Task::query(&db, Expr::all()).await?;
    assert_eq!(found_tasks.len(), 1);
    println!("Tasks under project: {}", found_tasks.len());

    let found_items = ChecklistItem::query(&db, Expr::all()).await?;
    assert_eq!(found_items.len(), 2);
    println!(
        "Checklist items under task (level 2): {}",
        found_items.len()
    );

    println!("nested_recursive example OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_main() {
        super::main().unwrap();
    }
}
