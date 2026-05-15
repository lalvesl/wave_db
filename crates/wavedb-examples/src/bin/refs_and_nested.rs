//! References between records, plus a NestedNonUnique child that
//! complements its NonUnique parent.
//!
//! Two ideas in one example:
//!
//! 1. **Cross-references by `Id`.** Records carry the auto-injected
//!    `id: Id` field. To "reference" another record, store its `Id`
//!    on a sibling record — exactly like a foreign-key column in SQL.
//!    Here `Author` ↔ `Book` form a cyclic reference (author owns books,
//!    a book points back at its author).
//!
//! 2. **NestedNonUnique complements NonUnique.** `Chapter` is
//!    `NestedNonUnique` — physically scoped to a parent `Book`'s anchor
//!    in the storage engine. It cannot be queried as a top-level
//!    collection; the only way to reach the chapters is *through* the
//!    parent book. The `Chapter::book` field makes the parent reference
//!    explicit on the wire as well.
//!
//! Run with:
//!   cargo run --bin refs_and_nested

use wavedb::prelude::*;
use wavedb_net::MockTransport;
use wavedb_net::mock::ScriptedReply;

// ── Schema ───────────────────────────────────────────────────────────────────

#[wave_db(struct_id = 40, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Author1 {
    pub name: String,
    /// Forward reference to a `Book` — the author's featured work.
    /// `Id::ZERO` means "not set yet".
    pub featured_book: Id,
}
pub type Author = Author1;

#[wave_db(struct_id = 41, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Book1 {
    pub title: String,
    /// Back-reference to the owning `Author`.
    pub author: Id,
}
pub type Book = Book1;

#[wave_db(struct_id = 42, NestedNonUnique)]
#[derive(PartialEq, Eq)]
pub struct Chapter1 {
    pub number: u32,
    pub title: String,
    /// Parent `Book`. In production the storage engine *also* scopes
    /// nested records under the parent's anchor; carrying the id on the
    /// record makes the relationship explicit to the client too.
    pub book: Id,
}
pub type Chapter = Chapter1;

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
    assert_eq!(Author::SHAPE, Shape::NonUnique);
    assert_eq!(Book::SHAPE, Shape::NonUnique);
    assert_eq!(Chapter::SHAPE, Shape::NestedNonUnique);

    let tenant = 42u64;

    // Synthetic stable IDs — the real engine assigns these at write time;
    // hard-coding them here keeps the demo deterministic.
    let author_id = Id::new(tenant, 0, Author::STRUCT_ID, 1_000);
    let book_id = Id::new(tenant, 0, Book::STRUCT_ID, 2_000);

    // Phase 1: author without a featured book yet.
    let author_initial = Author {
        id: author_id,
        name: "Ada".into(),
        featured_book: Id::ZERO,
        ..Default::default()
    };

    // Phase 2: the book references the author.
    let book = Book {
        id: book_id,
        title: "Notes on Engines".into(),
        author: author_id,
        ..Default::default()
    };

    // Phase 3: rotate the author to point at the book — closes the cycle.
    let author_with_featured = Author {
        featured_book: book_id,
        ..author_initial.clone()
    };

    // Phase 4: two chapters nested under the book.
    let chapter_a = Chapter {
        number: 1,
        title: "Anchors".into(),
        book: book_id,
        ..Default::default()
    };
    let chapter_b = Chapter {
        number: 2,
        title: "Migrations".into(),
        book: book_id,
        ..Default::default()
    };

    // ── Scripted server replies ──────────────────────────────────────────────
    let mock = MockTransport::new();
    mock.push(ScriptedReply::connect(
        "ws://owner:7700",
        "ws://backup:7700",
    ));
    // 5 writes — author, book, author rotate, chapter_a, chapter_b
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    // query authors → [author_with_featured]
    mock.push(ScriptedReply::ok(encode_query(&[
        author_with_featured.clone()
    ])));
    // query books → [book]   (mock returns regardless of filter)
    mock.push(ScriptedReply::ok(encode_query(&[book.clone()])));
    // query chapters under the book → [chapter_a, chapter_b]
    mock.push(ScriptedReply::ok(encode_query(&[
        chapter_a.clone(),
        chapter_b.clone(),
    ])));

    let db = Db::open_with_transport(mock, /* user= */ 1, tenant).await?;

    // ── Writes ───────────────────────────────────────────────────────────────
    author_initial.update(&db).await?;
    book.clone().update(&db).await?;
    author_with_featured.update(&db).await?;
    chapter_a.clone().update(&db).await?;
    chapter_b.clone().update(&db).await?;
    println!("Wrote 1 author, 1 book, 2 chapters");

    // ── Read author, follow the featured-book reference ─────────────────────
    let authors = Author::query(&db, Expr::all()).await?;
    assert_eq!(authors.len(), 1);
    let live_author = &authors[0];
    assert_ne!(live_author.featured_book, Id::ZERO);
    println!(
        "Author {:?} featured_book = {}",
        live_author.name, live_author.featured_book
    );

    // Follow Author → Book. The engine can't filter by an `Id` field
    // through `Expr` directly (Value has no U128 variant), so we query
    // and filter client-side. Real schemas can also expose a derived
    // u64 column (e.g. `created_at`) for server-side filtering.
    let all_books = Book::query(&db, Expr::all()).await?;
    let books_for_author: Vec<&Book> = all_books
        .iter()
        .filter(|b| b.author == live_author.id || b.id == live_author.featured_book)
        .collect();
    assert_eq!(books_for_author.len(), 1);
    let live_book = books_for_author[0];
    println!(
        "  → resolved Book {:?} (author back-ref = {})",
        live_book.title, live_book.author
    );

    // ── Read chapters *through* the parent Book ─────────────────────────────
    // `Chapter` is NestedNonUnique: there is no top-level Chapter index.
    // The engine routes this query into `live_book`'s subtree using the
    // parent anchor — the client passes a filter as usual, but the scope
    // is the parent's children, not all Chapters in the tenant.
    let chapters = Chapter::query(&db, Expr::gte("number", 1u64)).await?;
    assert_eq!(chapters.len(), 2);
    assert!(chapters.iter().all(|c| c.book == live_book.id));
    println!(
        "Chapters under {:?}: {}",
        live_book.title,
        chapters
            .iter()
            .map(|c| format!("#{} {:?}", c.number, c.title))
            .collect::<Vec<_>>()
            .join(", ")
    );

    println!("refs_and_nested example OK");
    Ok(())
}
