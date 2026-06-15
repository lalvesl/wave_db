# 🌊 WaveDB — A User-Data-Partitioned, Tenant-Centric Database

> _"Technology works like ocean waves — going back and forth, but always advancing toward the shore (global-warm)."_

---

## Vision

Most relational databases were designed with **analytics in mind**: aggregate across millions of rows, join dozens of tables, run complex GROUP BY queries across all users simultaneously. This is powerful for data warehouses, but it's **the wrong default for application data**.

WaveDB is a research project exploring a fundamentally different approach: a database where **every user owns their own isolated data space**, history is a first-class citizen, horizontal scaling is a structural property — not an afterthought — and the server and database are **the same binary**.

> This file is the **idea + quickstart**. Every detailed mechanism now lives in
> the crate that implements it — see the [Crate map](#crate-map).

---

## The Core Idea

### The Problem with Traditional SQL (for Applications)

Conventional SQL organises storage **by type — not by who owns the data or what it relates to**. Two kinds of unrelated data end up physically interleaved on the same pages or need to navigate between leaf nodes to found correct id, by the correct user, by the correct relation with another table, and application reads pay for it twice or more depending the complexity, here start the phrase ONE QUERY, 10 JOINS. This experimental ideia of DB try to solve this problem with only one IO of disk without dozens of GB of cache in memory.

#### Mixing 1 — every tenant's rows share one table

```sql
-- Every tenant's orders share one table, so the partition key
-- ("whose data?") has to ride in every query next to the real filter.
SELECT * FROM orders
WHERE user_id = 42      -- partition key — restated on every read
  AND amount > 100;     -- the business filter you actually wanted
```

The `orders` table holds **every user's orders**, interleaved row-by-row across shared pages. To serve a single user the engine index-walks or scans through bytes belonging to thousands of unrelated tenants, then filters down to the few rows that matter. Every page pulled into the buffer pool is mostly data this query will throw away.

In WaveDB the partition key is **structural, not a predicate**: the tenant is bound once at connect and never appears in a query again — only the business filter is left.

#### Mixing 2 — unrelated child rows share one table (the NonUnique-within-NonUnique case)

Even inside a single tenant, a classic schema puts every invoice's line items in one `invoice_lines` table:

```sql
SELECT * FROM invoice_lines WHERE invoice_id = 1001;
```

The lines for invoice `1001` sit scattered among the lines of every other invoice — and, combined with Mixing 1, every other tenant's invoices too. The rows you actually want (one parent's children) are never colocated on disk. Fetching them is a scatter of random page reads guided by an index, never one sequential pull.

#### The shared root cause

Both problems stem from laying out storage **by table/type instead of by access pattern**. Application reads are almost always _"give me this one tenant's — and often this one parent's — data."_ But the bytes that answer that question are sprayed across pages shared with data nobody asked for. The cost surfaces as **cache misses and wasted disk IOPS**: every read drags in neighbours you didn't request, the page cache holds a low fraction of useful bytes, and the CPU burns cycles on joins and filters that exist only to undo the mixing. Horizontal scaling then forces "manual" sharding — splitting tenants across databases by hand, which is just reintroducing partitioning after the fact.

**WaveDB starts from that endpoint.** Data is partitioned by tenant from the first byte, and NonUnique-within-NonUnique children are clustered under their parent's address space — so the bytes a query needs sit together on the same pages and nothing else does. A read touches only the interested data: high cache hit rate, minimal IOPS, no joins. The CPU saved from join processing goes to compression instead.

### The Ocean Wave Analogy

| Era     | Web Frontend                                          | Database                                       |
| ------- | ----------------------------------------------------- | ---------------------------------------------- |
| Past    | Static pages (fast, not dynamic)                      | DB and server tightly coupled                  |
| Present | Client-side rendering (dynamic, slow)                 | Independent DBs with ORMs as glue              |
| Future  | Server-side dynamic rendering (Next.js, Nuxt, Leptos) | **Tenant-partitioned, application-centric DB** |

The cycle closes. Each iteration looks like regression but carries forward the best properties of both worlds.

---

## Data Model (overview)

### Ownership hierarchy

```
TENANT (u48)
 └── Root user or Company — the ultimate data owner
      └── USER
           └── A person granted access; defined via a Tenant-scoped permissions struct
```

Every piece of data belongs to a **TENANT**. A user acts on that data under the
tenant's permission rules. Sharing is granting a user access inside the tenant's
own data space — no cross-partition references for the common case.

### Three data shapes

| Shape                          | Cardinality per tenant                        | Operations                             |
| ------------------------------ | --------------------------------------------- | -------------------------------------- |
| **Unique**                     | Exactly one live record per `(STRUCT_ID, TENANT_ID)` | `read`, `"save"`, `create`             |
| **NonUnique**                  | Many live records per tenant                  | `read`, `"save"`, `create`, `"delete"` |
| **NonUnique-within-NonUnique** | Many records tightly bound to a single parent | `read`, `"save"`, `create`, `"delete"` |

`"save"`/`"delete"` are quoted because WaveDB is **versioned**: an update writes
a new versioned record and rotates the anchor; a delete writes a tombstone. The
bytes never disappear. The nested shape is stored as a tightly-coupled child
collection under the parent's address space, recursively.

Every record carries a **128-bit composite `Id`**
(`TENANT_ID·SHARD_ID·STRUCT_ID·CREATED_AT`) and a `Metadata` (version chain,
authorship, permission). Full bit layout and semantics:
[`wavedb-core`](crates/wavedb-core/README.md).

---

## Simple usage

Objects are declared with the `#[wave_db]` macro — the trailing integer of the
type name _is_ the schema version, and `struct_id` is pinned per family:

```rust
#[wave_db(struct_id = 7, NonUnique)]
pub struct Message42 {
    pub body: String,
    pub author: u64,
}
pub type Message = Message42;   // one line declares the live version
```

Connect once (tenant is structural), then read/write with typed columns:

```rust
use wavedb::prelude::*;

// For a B2C app the tenant *is* the user — bound once, at connect time.
let db = Db::connect("wss://wavedb.example", /* user */ 42, /* tenant */ 42).await?;

// No WHERE user_id, no join: the engine reads only tenant 42's orders.
// `Order::amount` is a macro-generated typed column — a misspelt field is a
// compile error, not an empty result.
let orders: Vec<Order> = Order::query(&db, Order::amount.gt(100u64)).await?;

order.save(&db).await?;     // versioned update in place
order.delete(&db).await?;   // tombstone (NonUnique only)
```

A whole application backend is one expression — attaching the registry turns a
generic node into _your_ backend:

```rust
declare_objects! { pub mod app_objects { messages: [Message42], … } }

Server::bind("0.0.0.0:7700")
    .tenant(42)
    .data_dir("./data")
    .registry(app_objects::REGISTRY)
    .serve()
    .await
```

The full client API, operation modes, and object lifecycle live in
[`wavedb`](crates/wavedb/README.md).

---

## Crate map

The detail moved out of this file and into the crate that owns it. Start at the
[`wavedb`](crates/wavedb/README.md) facade for usage, drop into the others for
mechanism.

| Crate | What it owns | Read for |
| ----- | ------------ | -------- |
| [`wavedb`](crates/wavedb/README.md) | Client `Db` handle, typed CRUD, query surface | Quickstart, entry points, object lifecycle |
| [`wavedb-core`](crates/wavedb-core/README.md) | `Id`, `Metadata`, migrations, permissions, query tree, wire | ID layout, schema versioning, **migrations** |
| [`wavedb-macros`](crates/wavedb-macros/README.md) | `#[wave_db]`, `declare_objects!` | Object declaration, anchor addressing, validate/preprocess |
| [`wavedb-storage`](crates/wavedb-storage/README.md) | The per-node engine | **Pages, page directory, block allocator**, anchors, indexes, compression, heap, write pipeline |
| [`wavedb-quick-node`](crates/wavedb-quick-node/README.md) | Hot tier | Ownership ring, replication, routing/failover, node enforcement |
| [`wavedb-slow-node`](crates/wavedb-slow-node/README.md) | Cold tier | History archive, flush-down |
| [`wavedb-net`](crates/wavedb-net/README.md) | Transport | WebSocket / HTTP queue, Bloom screen-sync |
| [`wavedb-wasm`](crates/wavedb-wasm/README.md) | Browser client | IndexedDB key→value storage (no pages) |

Tooling: `wavedb-examples`, `wavedb-bench`, `wavedb-test-cluster`,
`wavedb-monitor`, `wavedb-monitor-gui`.

---

## Status of known problems

Cross-cutting design ledger; each resolution is detailed in its crate.

| #   | Problem                    | State | Where |
| --- | -------------------------- | ----- | ----- |
| P1  | Heap overflow strategy     | ✅ | [storage](crates/wavedb-storage/README.md#heap-data-strategy) |
| P2  | Hash collision / page full | ✅ | [storage](crates/wavedb-storage/README.md#collision--fullness-strategy) |
| P3  | Heap compression           | ✅ | [storage](crates/wavedb-storage/README.md#compression) |
| P4  | Cross-tenant queries       | ✅ out of scope by design | — |
| P5  | STRUCT versioning          | ✅ | [core](crates/wavedb-core/README.md#migrations) |
| P6  | Multi-tenant sharing       | ✅ | [core](crates/wavedb-core/README.md#permissions) |
| P7  | Many-to-many relations     | ✅ anchor slots | [storage](crates/wavedb-storage/README.md#anchor-slots) |
| P8  | Stack data compression     | ✅ per-`(STRUCT_ID, version)` dictionaries | [storage](crates/wavedb-storage/README.md#compression) |
| P11 | Index maintenance cost     | ✅ | [storage](crates/wavedb-storage/README.md#index-structures-nonunique) |
| P13 | Anchor storage cost        | ✅ accepted trade-off / pointer-only opt-in | [storage](crates/wavedb-storage/README.md#anchor-slots) |
| P14 | Permissions struct design  | ✅ | [core](crates/wavedb-core/README.md#permissions) |
| P9  | Rebalancing under load      | 🟡 largely structural (page-local growth) | [storage](crates/wavedb-storage/README.md#per-type-page-directory) |
| P10 | Offline-first reconciliation | ⏸ deferred | — |
| P15 | Cross-tenant permission sharing | 🔴 reciprocal capability records (direction) | — |

---

## Non-Goals

- **Not OLAP.** Cross-tenant aggregations belong in a dedicated analytics pipeline.
- **Not a general consensus system.** Consistency is tenant-scoped; multi-tenant eventual consistency is by design.
- **Not a SQL replacement.** The query model is deliberately constrained.
- **Not offline-first (yet).** See P10.

---

## Implementation language

Rust — the `#[wave_db]` proc-macro enforces ID/Metadata structure at compile
time, `repr(C)` page structs map directly to disk, `async` end to end, and the
same source compiles to native (Tokio) and browser (WASM via
`wasm_bindgen_futures`, `fetch`, `gloo_net`).

---

## Status

🔬 **Research Phase** — a living design record. The core architecture is
defined; remaining open problems (P9, P15) are operational rather than
foundational, and P10 is intentionally deferred.

## License

TBD — research project, not yet licensed.
