# 🌊 WaveDB — A User-Partitioned, Tenant-Centric Database

> _"Technology works like ocean waves — going back and forth, but always advancing toward the shore (global-warm)."_

---

## Vision

Most relational databases were designed with **analytics in mind**: aggregate across millions of rows, join dozens of tables, run complex GROUP BY queries across all users simultaneously. This is powerful for data warehouses, but it's **the wrong default for application data**.

WaveDB is a research project exploring a fundamentally different approach: a database where **every user owns their own isolated data space**, history is a first-class citizen, horizontal scaling is a structural property — not an afterthought — and the server and database are **the same binary**.

---

## The Core Idea

### The Problem with Traditional SQL (for Applications)

In a conventional SQL database, all user data lives in shared tables:

```sql
SELECT * FROM orders
LEFT JOIN users ON orders.user_id = users.id
WHERE users.id = 42;
```

Every query for a single user filters a table containing **every user's data**. The CPU is constantly running hash joins, filter pipelines, and aggregations just to serve one person's data. Horizontal scaling requires "manual" sharding — which means splitting user data across databases anyway.

**WaveDB starts from that endpoint**: data is partitioned by tenant from the very beginning. There are no joins. The CPU saved from join processing is available for compression instead.

### The Ocean Wave Analogy

| Era     | Web Frontend                                          | Database                                      |
| ------- | ----------------------------------------------------- | --------------------------------------------- |
| Past    | Static pages (fast, not dynamic)                      | DB and server tightly coupled                 |
| Present | Client-side rendering (dynamic, slow)                 | Independent DBs with ORMs as glue             |
| Future  | Server-side dynamic rendering (Next.js, Nuxt, Leptos) | **Tenant-partitioned, application-centric DB** |

The cycle closes. Each iteration looks like regression but carries forward the best properties of both worlds.

---

## Data Model

### Ownership Hierarchy

```
TENANT (u48)
 └── Root user or Company — the ultimate data owner
      └── USER
           └── A person granted access; defined via a Tenant-scoped permissions struct
```

Every piece of data belongs to a **TENANT**. A user is someone who can act on that data under the tenant's permission struct. Sharing is modelled as granting a user access inside the tenant's own data space — no cross-partition references needed for the common case.

### The ID

Every record has a composite ID of exactly 128 bits:

```
[ TENANT_ID (u48) | SHARD_ID (u8) | STRUCT_ID (u16) | CREATED_AT (u48, 100µs precision) | SLIDER (u8) ]
```

| Field         | Type  | Description                                                                                                                                              |
| ------------- | ----- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `TENANT_ID`   | `u48` | Identifies the owner of this record. `0` = database system                                                                                               |
| `SHARD_ID`    | `u8`  | Hashed property range for Large Tenants — **256 shards** to spread high-volume Non-Unique data across pages                                              |
| `STRUCT_ID`   | `u16` | The table / object type, fixed at compile time                                                                                                           |
| `CREATED_AT`  | `u48` | Microseconds (at 100µs precision) since a custom epoch defined in code                                                                                   |
| `SLIDER`      | `u8`  | Auto-increments to prevent collisions within the same 100µs tick. May be assigned **randomly per device** to pre-empt collisions (used for NonUnique data) |

### Mandatory Object Structure

Every WaveDB object is defined with a Rust proc-macro:

```rust
#[wave_db]
pub struct SomeObject {
    pub id: Id,
    pub metadata: Metadata,
    pub some_property: String,
    // ...
}
```

- **`Id`** — exposes `.tenant_id()`, `.shard_id()`, `.struct_id()`, `.created_at()`, `.slider()` by trait impl
- **`Metadata`** — exposes:
  - `old_modification_id`: ID of the previous version of this object
  - `new_modification_id`: ID of the next version (`0u128` if this is the live object)
  - `struct_version`: schema version at write time, used for lazy migration
  - `creator_id`: ID of the user who created or modified this version
  *(Note: `struct_version` (16 bits) and `creator_id` (48 bits) are packed into a single 64-bit field for memory and storage optimization).*

### Schema Versioning & Lazy Migration

`struct_version` is stored in every object's `Metadata`. When a record is read and its `struct_version` is behind the current compiled version, the migration transform runs in memory and the updated record is written back **in the background**. Migrations are partial and progressive — no global lock, no downtime.

---

## Anchor Slots — Solving All Cross-Pointer References

A core problem with versioned IDs: when an object mutates, every record that referenced its old ID would need rewriting. WaveDB resolves this with **Anchor Slots**.

**Anchors hold all cross-pointers for a given record**, giving the system a stable place to track index updates and follow forward references. Every cross-reference (index entry, M2M link, sync handle) targets the anchor — never a versioned record — so when the underlying data mutates, none of those pointers need to be rewritten.

Anchors are the universal solution for all cross-pointer references, including:

- **Many-to-Many (M2M)** — orders ↔ products, users ↔ shared documents
- **Single-to-Many (S2M)** — tenant → orders, post → comments
- **Indexes** — every index entry points to an anchor, never to a versioned record
- **Bloom filter sync** — clients track anchors, not historical versions

Anchors keep the full list of inbound references in an array on the slot itself, so the system can track every pointer that resolves through it.

### Two Operating Modes

Anchors support two modes, chosen per deployment / per node profile:

| Mode             | Slot contents                  | Read cost           | Storage cost | Typical use                                  |
| ---------------- | ------------------------------ | ------------------- | ------------ | -------------------------------------------- |
| **Inline data**  | Full live record bytes + marker | 1 IO                | Higher (~2x live data) | **Quick-Nodes** — hot, latency-sensitive paths |
| **Pointer-only** | Pointer to versioned record only | 1 extra IO per read | Lower (no duplication) | Storage-constrained or cold-leaning deployments |

Inline mode trades disk space for one fewer I/O on the read path. Pointer-only mode keeps anchors tiny (just the address + reference array) at the cost of an extra hop to fetch data. The Quick-Node tier defaults to inline; archive-leaning deployments can opt into pointer-only.

### Two Slots Per Live Record

| Slot          | Hashed at                              | Contents                                                                                                  |
| ------------- | -------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| **Anchor**    | `(STRUCT_ID, TENANT_ID)` — no timestamp | Live data (inline mode) **or** pointer (pointer-only mode), plus marker `current_version_at: created_at` |
| **Versioned** | `(STRUCT_ID, TENANT_ID, CREATED_AT)`    | Full data + modification chain (`old_mod_id`, `new_mod_id`)                                              |

### How Mutation Works

1. New versioned record is written at the new `created_at` hash with `old_mod_id` pointing to the previous version
2. The previous versioned record's `new_mod_id` is updated to point forward
3. The anchor slot is overwritten with the new data and the new `current_version_at` marker

That's still 2–3 IOPs per write — same cost as the no-anchor design — but the structural payoff is large:

- **References never need rewriting on mutation** — they all target the anchor address
- **Sync queries by versioned ID always resolve** — the historical record exists at its versioned hash from the moment it was written
- **Cross-references work even before sync completes** — the anchor is the stable handle

### Tombstone Anchors

When an object is deleted, its anchor is replaced with a **tombstone marker** containing the `created_at` of the final live version. References to deleted objects can distinguish "deleted" from "never existed" with a single read.

### Storage Cost

Anchors duplicate live record bytes — 2x storage for live data only. Historical records remain single-copy. For typical application workloads where each record may carry many cross-references, this is a clear win. See P13 for an opt-in degradation mode for storage-constrained deployments.

---

## Page Layout

Each page is internally organised as:

```
┌──────────────────────────────────────────────────┐
│  Vec<(ID, offset, size)>   ← object directory    │
│  ──────────────────────────────────────────────  │
│  [object A bytes][object B bytes][object C ...]  │  ← stacked forward
│                                                  │
│                     [heap value C][heap value B] │  ← growing from end
└──────────────────────────────────────────────────┘
```

Multiple `(STRUCT_ID, TENANT_ID)` pairs coexist in the same page. The directory at the front allows O(1) lookup of any object's byte range. Stack-allocated data lives in the object bytes; heap data grows from the end toward the middle.

---

## Compression

WaveDB applies **two complementary compression strategies**:

### Heap Compression (zstd)

Variable-length values (strings, blobs) are compressed with zstd before being written to the page heap region. zstd's aggressive ratio is the right trade because WaveDB has no join processing competing for CPU.

### Per-STRUCT Dictionary Compression for Stack Data

The fixed-width region of pages (integers, enums, IDs) compresses extremely well using **dictionaries scoped per STRUCT**, because every record of the same type has identical field layout.

#### Architecture

```
┌─────────────────────────────────────────────────┐
│  In-Memory Dictionary Cache                     │
│  ┌──────────────────────────────────────────┐   │
│  │ STRUCT 1 → dict (hot)                    │   │
│  │ STRUCT 2 → dict (hot)                    │   │
│  │ STRUCT 7 → dict (warm)                   │   │
│  │ ... bounded by max_dict_memory ...       │   │
│  └──────────────────────────────────────────┘   │
└─────────────────────────────────────────────────┘
                    ↓ read on miss
┌─────────────────────────────────────────────────┐
│  dictionaries_file (on disk)                    │
│  one entry per STRUCT, versioned                │
└─────────────────────────────────────────────────┘
                    ↑ background rewrite
┌─────────────────────────────────────────────────┐
│  Journal (current dictionary mutations)         │
└─────────────────────────────────────────────────┘
```

#### Memory Management

A configurable parameter `max_dict_memory` caps total RAM used by dictionaries. Hot STRUCTs stay loaded; cold ones are evicted under LRU pressure and re-read from `dictionaries_file` on demand. Each dictionary access updates a hot-counter, similar to the page buffer cache.

#### Update Flow

1. Dictionary updates (rebuilt as the data distribution shifts) are first written to the **journal**
2. A **background task** consumes journal entries and rewrites the affected entries in `dictionaries_file`
3. Pages written under an old dictionary version remain readable — each page header carries the dictionary version it was compressed with
4. Lazy re-compression: when a page is rewritten for any other reason, it picks up the latest dictionary

This means **dictionary rebuilds are never on the write hot path** — the journal absorbs the cost and the background task amortises the disk writes.

#### Why This Works

- All records of one STRUCT share enum values, ID prefixes, common timestamp ranges, and field-position layout — dictionaries achieve very high compression ratios
- Per-STRUCT scoping keeps dictionaries small (often <64KB) so many fit in memory simultaneously
- Cold STRUCTs don't waste RAM
- Dictionary versioning means old pages stay readable forever — no migration cliff

---

## Heap Data Strategy (Detailed)

### Step 1 — Inline compression

zstd compress and store at the end of the page.

### Step 2 — Overflow to a hashed block

If still too large, the page slot stores a pointer `(STRUCT_ID, TENANT_ID, data_id)` and the value goes to a separate overflow block addressed by that pointer's hash.

### Step 3 — Heap Anchor for very large values

When the value exceeds what an overflow block can hold cleanly, WaveDB writes a **Heap Anchor**: a small indirection record in the data file that points to the actual bytes living at the tail of the heap file.

- The heap anchor is hashed by `(CREATED_AT, SLIDER)` instead of the usual `(STRUCT_ID, TENANT_ID)`. Because `SLIDER` auto-increments — or is randomised by the client — within each 100µs tick, this hash is effectively unique per record, so heap anchors don't collide with normal Anchor Slots.
- The anchor record itself is tiny: a single pointer `(offset, size)` into the heap file.
- The actual bytes are appended to the **tail of the heap file**, padded out to keep every entry **4KB-aligned**. This guarantees that heap reads never straddle a page boundary, regardless of the underlying value's size.

A read for an oversized field costs a bounded **2 IOs**: one for the heap anchor, one for the heap data — independent of how large the value actually is.

> **Trade-off vs. earlier deduplicating linked-list design:** This approach drops the per-tail ownership-dedup that an earlier sketch carried. In exchange, large-value reads stop scaling with chain depth and heap writes are pure appends. Content-addressed dedup can be layered on top later if profile data shows it's worth the complexity.

---

## Files on Disk

WaveDB splits its on-disk state across **four files**, each tuned for a different access pattern. (How these are physically combined is controlled by *Operation Modes* further down — single-file mode merges them all; production splits them.)

| File           | Contents                                                                | Layout                                    | Access pattern                                   |
| -------------- | ----------------------------------------------------------------------- | ----------------------------------------- | ------------------------------------------------ |
| `data` file    | Hash-mapped pages — Anchor Slots, versioned records, heap-anchor stubs   | Page-addressed (hash → page)             | Random IO, page-sized                            |
| `index` file   | B+ tree nodes and small-collection array indexes                        | **Highly contiguous**; nodes 4KB-aligned  | Sequential within a tree, random across trees    |
| `heap` file    | Compressed / oversized payloads + the heap-anchor append region         | Append-mostly, 4KB-padded                | Append on write, point IO on read                |
| `journal` file | In-flight mutations, dictionary updates, free-space deltas              | Append-only                               | Append + sequential replay on startup            |

### Why split

Index entries reference data-file records **by ID**, never by file offset. That keeps B+ trees portable across rebalances and lets the index file be compacted independently of the data file without rewriting any pointers. The same logic applies to heap-anchor stubs: they address the heap by `(offset, size)` only at the moment of read — between reads, ranges are free to be relocated by cleanup.

The index file's contiguous layout is the payoff of the per-(STRUCT_ID, TENANT_ID) tree design: millions of tiny trees laid out next to each other compress and prefetch well, and they never share pages with the random-access data file.

---

## Collision & Fullness Strategy

Multiple `(STRUCT_ID, TENANT_ID)` pairs naturally share pages. When a target page is full:

1. **Double hashing** finds an alternative candidate page
2. Double hashing firing at a meaningful rate **is the signal** — the file is approaching capacity and a rebalance triggers automatically

The collision resolution mechanism _is_ the alert system.

---

## Unique vs. Non-Unique Data

### Unique (default)

One live record per `(STRUCT_ID, TENANT_ID)`. Examples: a user's profile, a company's settings.

### Non-Unique

Declared with `#[wave_db(NonUnique)]`. Multiple records per tenant. Examples: a tenant's orders, a tenant's messages.

```rust
#[wave_db(NonUnique)]
pub struct Order {
    pub id: Id,
    pub metadata: Metadata,
    pub amount: u64,
}
```

---

## Index Structures for Non-Unique Data

```
  History ← History ← History
                ↑
             [ Object ]
            ↗↑  ↑  ↑  ↑↗
┌──────────────────────────────────────────────────────┐
│ Current     B+ tree (or array if small) by created_at │  desc by created_at
│ Deleted     B+ tree (or array if small)               │  deleted kept for history
│ Index(n)    B+ tree by custom property                │  ordered by property
│ [ordered]                                             │
│ Index[n]    Hash buckets [val|hash][val|hash] ──→ …   │  discrete value index
│ [discrete]      ↓                                     │
│             B+ tree per bucket                        │
└──────────────────────────────────────────────────────┘
```

All index entries point to **anchors**, never to versioned records — so property mutations don't require index rewrites unless the indexed property itself changed.

> **Storage location:** Indexes live in a **separate file** from the hash-mapped data file. Index data is small relative to the hash-mapped data, and the index entries fold cleanly into the anchor lookup path — keeping them isolated lets each file specialise its layout without one trampling the other's cache locality.

### Per-(STRUCT_ID, TENANT_ID) B+ Trees

Ordered indexes use **B+ trees scoped to a single tenant's collection** — small, shallow, independent. A tenant with 10,000 orders is ~7 levels deep. Insert / update / lookup is O(log n).

### ⚡ Adaptive B+ Tree Indexes (`NonUnique` Objects)

Because data is partitioned by tenant, WaveDB builds **millions of tiny, highly optimised indexes** rather than one massive global index. Indexes adapt dynamically based on collection size, controlled by a configurable threshold (`MAX_NON_UNIQUE_ELEMENTS`, default **50**).

#### State 1 — Linear Array (small collections)

For collections under the threshold, the index is stored as a cache-friendly contiguous byte array directly on the page. Lookups are O(N), but for small N this beats a tree on real hardware due to branch prediction and cache locality.

#### Conversion Trigger

The instant the **51st** item (`MAX_NON_UNIQUE_ELEMENTS + 1`) is inserted, the array atomically upgrades. Conversion is one-way and journaled — there is no fallback path back to an array.

#### State 2 — Page-Aligned B+ Tree (large collections)

Data is bulk-loaded into a new `BTreeNode` sized **exactly to the OS page size** (e.g., 4 KB). The page-alignment is the whole point of the design:

- A single 4 KB node holds **~170 index entries**
- A tree of depth 2 covers nearly **30,000 items**
- **>99% of tenant index lookups complete in 1 or fewer disk I/O reads**

The threshold is tunable per-STRUCT via a proc-macro attribute:

```rust
#[wave_db(NonUnique, btree_threshold = 100)]
pub struct Message { ... }
```

> **Index types:** Ordered indexes use the standard B+ tree shape described above. Discrete (value-bucketed) indexes use a hash bucket → array-or-tree model — each bucket starts as an array and promotes to a per-bucket B+ tree if it grows past the threshold.

### Deleted is a First-Class Index

**Deleted records are never removed from indexing** — they move from the Current B+ tree to the Deleted B+ tree. Reconstructing collection state at a past timestamp requires both trees.

### Two Storage Strategies for Iter<T> Fields

#### `try_heap_inline` — Small iterables, minimal overhead

```rust
#[wave_db(wave_db::try_heap_inline)]
pub struct TagList {
    tags: Iter<u8>,
}
```

Stored as a heap-inline linked-list chunked by `MAX_HEAPED_SIZE`. Each chunk has its own ID. No cross-pointer maintenance.

#### Default (`AsRef`) — Full index support

```rust
#[wave_db]
pub struct Order {
    items: Iter<OrderItem>,
}
```

Maintains the full index graph above. Property mutations only rewrite the index for the changed property — and because index entries point to anchors, even those rewrites only update keys, not pointer values.

---

## History

History records live at their natural versioned hash `(STRUCT_ID, TENANT_ID, CREATED_AT)`. The Anchor Slot holds the live state. They never collide in the address space.

### Traversal

- **Backward** (present → past): start at anchor, follow `current_version_at` to the live versioned record, then `old_modification_id` chain
- **Forward** (past → present): from any historical record, follow `new_modification_id` until you reach `new_mod_id == 0`. The anchor mirrors that record.
- **Lookup miss on versioned hash**: the record at that timestamp doesn't exist
- **Stale ID via anchor**: anchor's `current_version_at` reveals the latest version

---

## Distributed Architecture

### Same Binary

Server and database are **the same binary**. No separate DB process, no ORM, no protocol translation.

### Ownership Routing

On user connect, a server node takes **routing ownership** of the relevant `(STRUCT_ID, TENANT_ID)` pairs, pushes mutations to DB-tier nodes asynchronously, and publishes a **Bloom filter** of currently-owned pairs to larger DB nodes.

### 🌐 Distributed Topology: Compute / Storage Separation

WaveDB uses a Cassandra-inspired distributed architecture built around the **Coordinator Pattern**, separating fast transactional state from cold historical storage. The cluster is split into two physical tiers serving different roles in the same partition map.

#### ⚡ Quick-Nodes (the "Hot" layer)

- **Hardware:** High CPU, high RAM, fast (smaller-capacity) NVMe SSDs.
- **Role:** Owns specific `TENANT_ID` + `SHARD_ID` partitions via a Consistent Hash Ring.
- **Behaviour:** Holds active **Anchor Slots in memory**. Validates interactions, processes mutations, and pushes asynchronous replication to backup Quick-Nodes. Anchors here run in **inline-data mode** — the extra storage is cheap relative to the read-IO it saves.

#### 🧊 Slow-Nodes (the "Cold" layer)

- **Hardware:** Lower CPU, moderate RAM, high-capacity HDD or SSD arrays.
- **Role:** Acts as the **Immutable Journal** and history archive.
- **Behaviour:** Quick-Nodes continuously flush older versions and transaction logs down to Slow-Nodes, releasing active disk space on the hot tier and maintaining permanent history off the latency path.

This separation lets each tier specialise its hardware: Quick-Nodes are sized for IOPS-per-watt, Slow-Nodes are sized for $/TB.

### 📡 Bloom Filter Screen-Sync

A state-sync mechanism for the live read path. Clients maintain a **Bloom filter of the 128-bit IDs currently rendered on screen** and send it to the primary Quick-Node over WebSocket. The node compares the filter against its live Anchor Slots and pushes back only the **exact deltas** — new or changed objects — preventing the massive over-fetching that array-based polling would require.

> **When to use what:** Bloom filter sync is the right tool for **online clients** with a small, evolving working set. For clients that have been offline for a long time — where the screen state is far behind reality — it is cheaper for the client to send the array of on-screen IDs back to the server for explicit revalidation, rather than trying to round-trip an extremely lossy filter.

### Offline-First via Slip ID

Clients write locally while offline. On reconnect, the server resolves any timestamp collisions by ±1 tick adjustments and sends corrected IDs back. Anchors keep cross-references stable across slips.

### Consistency Model

- **Within one tenant**: strong (single writer via routing ownership)
- **Across tenants**: eventual (Bloom filter sync)
- **On conflict**: most recent state wins via anchor; loser becomes a branch in the history chain

---

## Operation Modes

| Mode                  | Description                             | Use Case              |
| --------------------- | --------------------------------------- | --------------------- |
| **Single File**       | Data + history + journal in one file    | Development, embedded |
| **Separated History** | Live data and history in separate files | Production            |
| **History Only**      | History records only                    | Archive/backup nodes  |

---

## ⚙️ Write Pipeline & Concurrency Model

The write path is structured as a **journaled cache that fronts the durable files**. The cache is shaped exactly like the on-disk hash-map, so reads can serve directly from it without an extra format conversion. Two parameters govern the pipeline's behaviour:

| Parameter         | Description                                                  |
| ----------------- | ------------------------------------------------------------ |
| `MAX_DISK_IOPS`   | Soft IO budget per second across all background actors        |
| `MAX_CACHED_SIZE` | Total RAM budget for the in-memory write/read cache           |

### The write path

1. A mutation arrives at a Quick-Node.
2. It is inserted into the **in-memory cache** (hash-map shape).
3. The mutation is **appended to the journal** in the same atomic step.
4. Once both writes complete, the **client receives confirmation** — durability is journal-backed, so the client never waits for the `data` / `index` / `heap` files to settle.
5. A separate background actor drains the cache + journal into the durable files at its own pace.

### Cache pressure → backpressure

When the cache approaches `MAX_CACHED_SIZE`, **incoming writes block** until the drain actor releases enough cache and journal pages to make room. This is the **only** place in the system where a writer is forced to wait on the durable layer — under normal load the only bound is journal-append latency.

### Reads beat writes

Reads sit higher on the priority ladder than pending background writes:

- If the cache has any headroom, **read scheduling pre-empts queued background writes**.
- The journal is **never used as a read path for the client.** If a record isn't in the cache, the read goes to the proper files (`data` → `heap` if needed). Teaching the read path to interpret journal entries would double the formats it has to understand, for a marginal hit-rate gain — not worth it.

### Concurrency: tokio broadcast + actors

Each storage file is owned by an **actor** holding an `Arc<File>` plus a **`Mutex` over the currently-being-written block** (not the whole file). Concurrency is shaped per-file:

| File                                    | Concurrency model                                                                                         |
| --------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `data` file (random-access)             | Concurrent writes allowed across **different pages**; same-page writers serialize on the page mutex       |
| `index`, `heap`, `journal` (append-mostly) | Multiple writers prepare bytes in parallel; only the actual append step takes the block-level mutex     |

A **tokio broadcast channel** publishes page invalidations and free-space events so each actor can drop stale cache entries without polling. The only contended primitive is the per-block mutex inside each file — there is no global write lock.

### Idle work

When the actor pool isn't busy with reads or writes and has IO budget left under `MAX_DISK_IOPS`, it picks up background work — in this priority order:

1. **Rebalance task** — intentionally low-priority. The normal write path already rebalances incrementally through the double-hashing collision mechanism (see *Collision & Fullness Strategy*); standalone rebalance is only catching up corner cases, which is why it doesn't need its own scheduler tier.
2. **Cleanup / compaction** for files that have accumulated free space (see below).

### Free-space tracking & cleanup

When a record leaves a file — a journal entry that has been fully drained, or a versioned record being pushed down to a Slow-Node — the **freed range is recorded back in the journal as an empty-space delta**. The journal is the single source of truth for "what space can be reclaimed where".

Cleanup batches these deltas during idle windows and compacts each file in this priority order:

1. **`journal`** — most critical. An oversized journal slows replay on startup and competes for cache slots with hot data. Compacted and truncated **first**.
2. **`index`** — fragmentation here directly hurts every B+ tree walk. Compacted **second**.
3. **`heap`** — large but append-natural; reclaiming heap-tail space rarely changes hot-path latency. Compacted **last**.

Truncation is a real `truncate` syscall on the tail of each file, after the live ranges have been compacted toward the head.

---

## Transactions & Locking

Locks are **ID-scoped**, held as `Mutex` entries in the DB process memory. For records with anchors, the anchor lock covers both the anchor slot and any concurrent versioned-record write — they're treated as one atomic unit.

---

## Reliability

**Journal:** Append-only file recording in-flight page mutations, dictionary updates, **and free-space deltas**. Replayed on startup. Anchor + versioned writes are journaled together so crashes mid-mutation can never leave the two slots inconsistent. The full write/cleanup discipline is described in *Write Pipeline & Concurrency Model* above.

**Checksums:** Every page carries a checksum verified on read.

**Reed-Solomon (optional):** Per-page error correction for archive nodes.

---

## Configuration Parameters

| Parameter                       | Description                                          | Default              |
| ------------------------------- | ---------------------------------------------------- | -------------------- |
| `page_size`                     | Bytes per page                                       | varies by deployment |
| `page_counter`                  | Number of pages in file                              | grows as needed      |
| `max_heap_inline`               | Largest heap value stored inline before overflow     | 25% of page_size     |
| `warning_size_page_occupation`  | Fill threshold to alert                              | 70%                  |
| `max_dict_memory`               | RAM budget for STRUCT_ID dictionaries                | 64 MB                |
| `MAX_NON_UNIQUE_ELEMENTS`       | Per-STRUCT_ID array → B+ tree conversion threshold   | 50                   |
| `MAX_CACHED_SIZE`               | RAM budget for the in-memory write/read cache; writes block when near limit | tunable |
| `MAX_DISK_IOPS`                 | Soft IO budget per second across all background actors | hardware-dependent |
| `lock_timeout`                  | Max hold time for an ID lock                         | 30 s                 |
| `bloom_filter_publish_interval` | How often nodes share ownership filters              | 1 s                  |

---

## Known Problems & Research Areas

### ✅ Resolved

| #   | Problem                    | Resolution                                                                                                                                                                                          |
| --- | -------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| P1  | Heap overflow strategy     | zstd inline → hashed overflow block → **Heap Anchor stub** in the data file (hashed by `CREATED_AT + SLIDER`) addressing 4KB-padded entries appended to the heap-file tail; bounded 2-IO read regardless of value size |
| P2  | Hash collision / page full | Tenants share pages naturally; double hashing as fallback; double hashing rate = rebalance trigger                                                                                                  |
| P3  | Heap compression           | zstd; CPU is free because no join processing                                                                                                                                                        |
| P4  | Cross-tenant queries       | Out of scope by design — application context always knows the tenant                                                                                                                                |
| P5  | STRUCT versioning          | `struct_version` in `Metadata`; lazy migration on read, background rewrite                                                                                                                          |
| P6  | Multi-tenant sharing       | Tenant-defined permissions struct scoping user access                                                                                                                                               |
| P7  | Many-to-many relations     | **Anchor Slots** — fixed `(STRUCT_ID, TENANT_ID)` address with live data; references never need rewriting. Same mechanism handles S2M and indexes.                                                  |
| P8  | Stack data compression     | **Per-STRUCT_ID dictionaries** in memory bounded by `max_dict_memory`; updates journaled, applied to `dictionaries_file` by background task; pages carry dictionary version for backward compatibility |
| P11 | Index maintenance cost     | Per-(STRUCT_ID, TENANT_ID) B+ trees with adaptive conversion from array at threshold; index entries point to anchors so property mutations don't cascade                                            |
| P12 | Adaptive index threshold   | Default 50 items (`MAX_NON_UNIQUE_ELEMENTS`), tunable per-STRUCT_ID via proc-macro attribute; one-way atomic conversion                                                                             |
| P13 | Anchor storage cost        | Accepted as design trade-off (2x live data only, history single-copy); opt-in pointer-only mode available for storage-constrained deployments                                                       |

---

### 🟡 P9 — Rebalancing Under Load

Background rebalance task with backpressure. Force-rebalance API for maintenance windows. Multiple simultaneous rebalancing epochs theoretically possible in extreme growth; only the most recent is primary.

---

### 🟡 P10 — Slip ID Collision at Scale

Routing ownership prevents true multi-writer conflicts. Slip is only needed for offline rejoin. Worst case: apply slips sequentially with a small queue per `(STRUCT_ID, TENANT_ID)`.

---

### 🔴 P14 — Permissions Struct Design (new)

**Problem:** The tenant-defines-permissions model is mentioned but not specified. How does a tenant express "user X can read these structs but only write that one"? How are permission changes propagated and enforced? How are permissions versioned alongside data?

**Direction:** A reserved `STRUCT_ID = 1` for `Permissions` records with a well-defined schema: `(user_id, struct_id, allow_read, allow_write, allow_delete)`. Every access goes through a permission check on the tenant's permissions table — which itself is just another WaveDB lookup, so it benefits from the same caching.

---

### 🔴 P15 — Cross-Tenant Permission Sharing (new)

**Problem:** When a tenant shares data with another tenant (e.g., a contractor accessing client files), the anchor lives in the original tenant's partition but needs to be reachable from the recipient's session.

**Direction:** Reciprocal capability records. The granting tenant writes a `Capability` record (a small struct containing the target anchor address) into the recipient's partition. The recipient's queries that include "shared with me" walk capability records as a discovery mechanism. The actual data access still routes back to the original tenant's node.

---

## Non-Goals

- **Not OLAP.** Cross-tenant aggregations belong in a dedicated analytics pipeline.
- **Not a general consensus system.** Consistency is tenant-scoped; multi-tenant eventual consistency is by design.
- **Not a SQL replacement.** The query model is deliberately constrained.

---

## Implementation Language

Rust. Rationale:

- `#[wave_db]` proc-macro enforces and generates ID/Metadata structure at compile time
- `STRUCT_ID (u16)` numbering is a compile-time counter — the type system guarantees stability
- `Mutex`-based ID locking is idiomatic
- `repr(C)` page structs map directly to disk
- `async` iterators integrate naturally for the `Iter<T>` collection API

---

## Status

🔬 **Research Phase** — This document is a living design record. The core architecture is now defined; remaining open problems (P9, P10, P14, P15) are operational rather than foundational.

---

## License

TBD — research project, not yet licensed.
