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

| Era     | Web Frontend                                          | Database                                       |
| ------- | ----------------------------------------------------- | ---------------------------------------------- |
| Past    | Static pages (fast, not dynamic)                      | DB and server tightly coupled                  |
| Present | Client-side rendering (dynamic, slow)                 | Independent DBs with ORMs as glue              |
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

Every piece of data belongs to a **TENANT**. A user is someone who can act on that data under the tenant's permission rules. Sharing is modelled as granting a user access inside the tenant's own data space — no cross-partition references needed for the common case.

### Data Shapes

WaveDB recognises **three** data shapes, each with different ownership and indexing rules:

| Shape                          | Cardinality per tenant                                         | Examples                                    | Allowed operations                       |
| ------------------------------ | -------------------------------------------------------------- | ------------------------------------------- | ---------------------------------------- |
| **Unique**                     | Exactly one live record per `(STRUCT_ID, TENANT_ID)`           | User profile, company settings              | `read`, `"update"`, `create`             |
| **NonUnique**                  | Many live records per tenant                                   | Orders, messages, files                     | `read`, `"update"`, `create`, `"delete"` |
| **NonUnique-within-NonUnique** | Many records tightly bound to a single parent NonUnique record | Lines on an invoice, tasks inside a project | `read`, `"update"`, `create`, `"delete"` |

> `"update"` and `"delete"` are quoted because WaveDB is versioned: an update writes a new versioned record and rotates the anchor; a delete writes a tombstone. The bytes never disappear, and Unique records have no `delete` because there is nothing single-record-deletable about an exclusive record per tenant.

The third shape — **NonUnique-within-NonUnique** — is _not_ modelled as a generic many-to-many. Lines on an invoice have no independent identity; they exist only in the context of their parent invoice. Treating them as M2M cross-references would force needless anchor maintenance for relationships that never escape their parent. Instead, WaveDB stores them as a tightly-coupled child collection under the parent's address space.

### The ID

Every record has a composite ID of exactly **128 bits**:

```
[ TENANT_ID (u48) | SHARD_ID (u12) | STRUCT_ID (u20) | CREATED_AT (u48, 100µs precision) ]
```

| Field        | Type  | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| ------------ | ----- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `TENANT_ID`  | `u48` | Identifies the owner of this record. `0` is reserved for the database system; `U48::MAX` is reserved for unauthenticated sessions.                                                                                                                                                                                                                                                                                                                             |
| `SHARD_ID`   | `u12` | Range-allocated to a Quick-Node. `0` for **Unique** data — tenant ownership solves uniqueness on its own. For **NonUnique** data, the writing Quick-Node mints `SHARD_ID` from the shard range it currently owns for this tenant — **unless** the struct's `#[wave_db]` declaration sets `primary_anchor = field`, in which case `SHARD_ID` is the hash of that field, giving the anchor a content-addressed location. See _Anchor Slots → Anchor Addressing_. |
| `STRUCT_ID`  | `u20` | The table / object type, fixed at compile time. Incremental and unique across **all structs**; **shared between all versions of the same struct** (`Message1`, `Message2`, … all carry the same `STRUCT_ID`). See _Procedure Macros_.                                                                                                                                                                                                                          |
| `CREATED_AT` | `u48` | 100µs ticks since a custom epoch defined in code.                                                                                                                                                                                                                                                                                                                                                                                                              |

#### Why no slider anymore

Earlier drafts carried an 8-bit `SLIDER` field to break collisions inside a 100µs tick. With **range-mode shard ownership**, that's no longer needed:

- For **Unique** data, the owning Quick-Node is the only writer for the tenant — there's only one writer per `(STRUCT_ID, TENANT_ID)`, so collisions on the `(TENANT_ID, 0, STRUCT_ID, CREATED_AT)` hash can't happen by construction.
- For **NonUnique** data, the writing Quick-Node mints `SHARD_ID` from its owned shard range. Two writers on different nodes can't collide because they hold disjoint ranges; two writers on the same node serialise on the per-node shard counter.

Eight bits of address space were reclaimed: 4 went into `SHARD_ID` (8 → 12, growing from 256 to 4096 shards) and 4 went into `STRUCT_ID` (16 → 20, raising the struct ceiling from 65 K to over 1 M).

### Object Structure

Every WaveDB object is defined with a Rust proc-macro:

```rust
#[wave_db(struct_id = 1)]
pub struct UserProfile1 {
    pub id: Id,
    pub metadata: Metadata,
    pub display_name: String,
    pub bio: String,
}

pub type UserProfile = UserProfile1;
```

- **`Id`** — exposes `.tenant_id()`, `.shard_id()`, `.struct_id()`, `.created_at()` by trait impl.
- **`Metadata`** — exposes the version chain, authorship, and access rules described next.

### Metadata

```rust
pub struct Metadata {
    /// ID of the previous version of this object (`0u128` if this is the first).
    pub old_modification_id: u128,
    /// ID of the next version of this object (`0u128` if this is the live one).
    pub new_modification_id: u128,
    /// Schema version at write time. Used for lazy migration.
    pub struct_version: u8,
    /// User who created or modified this version.
    pub user: u48,
    /// Device of the user that wrote this version. A single user may
    /// be online from multiple devices simultaneously; this field lets
    /// the engine attribute each version to the device that produced it,
    /// which is useful for audit, conflict diagnosis, and per-device
    /// session bookkeeping during failover.
    pub device_created: u64,
    /// Optional access-control rule. `None` is the common case (only the
    /// tenant's own users can touch this record); postcard encodes `None`
    /// in a single byte.
    pub permission: Option<PermissionRef>,
}
```

`(struct_version, user)` is 56 bits and packs efficiently alongside `device_created` and the optional permission field through postcard. See _Permissions_ for the shape of `PermissionRef` and _Procedure Macros_ for how `struct_version` is derived.

### Schema Versioning & Lazy Migration

`struct_version` is stored in every object's `Metadata`. When a record is read and its `struct_version` is behind the current compiled version, the migration transform runs in memory and the updated record is written back **in the background**. Migrations are partial and progressive — no global lock, no downtime. See _Migrations_ for the full forward / rollback / chain story.

---

## Procedure Macros

WaveDB's object types are declared through a single `#[wave_db]` proc-macro that does six jobs at compile time:

1. **Implements `Id` and `Metadata` accessors.** Every annotated struct gets `.tenant_id()`, `.shard_id()`, `.struct_id()`, `.created_at()` plus full `Metadata` getters / setters by trait impl — no boilerplate at the call site.
2. **Pins a permanent `STRUCT_ID` declared by the developer** via `#[wave_db(struct_id = N, …)]`. The convention is incremental — the next struct family takes the next free integer — and the value is **shared across every version of the same struct family**: `Message1`, `Message2`, …, `Message42` all declare the same `struct_id = N`. The macro validates uniqueness across the whole codebase at compile time; once assigned, the ID never changes.
3. **Derives `struct_version` from the type name.** The trailing integer of the struct identifier _is_ the schema version. `Message42` ⇒ `struct_version = 42`. The macro reads the suffix, validates it fits in `u8`, and emits the corresponding `impl`. There is no separate `version = …` attribute — the version lives in the type name, the family ID lives in the macro arguments.
4. **Re-exports a stable alias.** The codebase always imports the unversioned name (`Message`), and your file declares which version is live with a single `pub type` line:

```rust
#[wave_db(struct_id = 7, NonUnique)]
pub struct Message42 {
    pub id: Id,
    pub metadata: Metadata,
    pub body: String,
    pub author: u48,
}

pub type Message = Message42;
```

Rolling forward or back is then **a one-line edit** — change `Message42` to `Message43` (or back to `Message41`) in the `pub type` and the rest of the application picks up the change. The `struct_id` does **not** change between versions, so cross-references and indexes remain valid across the upgrade. This naming convention is exactly what makes the rollback path in _Migrations_ a viable everyday operation.

5. **Configures anchor addressing.** Two optional attributes change how the struct's anchor is hashed and what aliases it exposes:
   - `primary_anchor = field` replaces the default node-allocated `SHARD_ID` with `hash(field)`, giving the struct a content-addressed primary anchor and a generated `find_by_<field>` accessor.
   - `secondary_anchor = (field)` (one or more times, including compound keys like `secondary_anchor = (department, employee_number)`) registers additional anchor addresses that point back to the primary, with one generated accessor per declared key.

   Both attributes round out as ordinary `async fn`s on the struct, so the application code never builds an anchor address by hand — the macro is the only thing that knows the precise hash layout. See _Anchor Slots → Anchor Addressing_ for the full semantics.

6. **Declares migrations inline — symmetric chain.** Each versioned struct declares its **immediate neighbours** via TYPE paths.  The macro reconstructs the full version chain at compile time, no naming convention required:

   | Attribute | Direction | Kind | Signature / value |
   | --------- | --------- | ---- | ----------------- |
   | `migrate_from = OldType` | backward | **type** | The predecessor struct. |
   | `migrate_from_with = fn` | backward | async fn | `async fn<Db>(&Db, OldType) -> Result<Self>` |
   | `migrate_rollback = NewType` | forward | **type** | The successor struct (this struct receives its rollback). |
   | `migrate_rollback_with = fn` | forward | async fn | `async fn<Db>(&Db, NewType) -> Result<Self>` |
   | `first_try = fn` | — | async fn | `async fn<Db>(&Db) -> Result<Option<OldType>>` — called **before** the DB search; if `Some`, skip the DB and run the forward migration instead.  Replaces the legacy Type-2 (compose) pattern. |
   | `fallback_not_found = fn` | — | async fn | `async fn<Db>(&Db) -> Result<Option<Self>>` — called when neither `first_try` nor the DB search returned a record. |

   **Chain bounds:**

   - **First (oldest) version:** has no `migrate_from`.  Declares `migrate_rollback` once a `vN+1` exists.
   - **Middle versions:** have both `migrate_from` (predecessor) and `migrate_rollback` (successor).
   - **Last (current) version:** has `migrate_from` but no `migrate_rollback` (no successor yet).

   Rollback is declared on the **older** struct ("I know how to receive a rollback from `NewType` and produce me"), so the inverse operation co-locates with the type it produces.  Each struct contributes its **own** edge to the registry — `Message41::register_migration` adds the backward `v42→v41` edge, `Message42::register_migration` adds the forward `v41→v42` edge.

   Both adjacent structs must implement `serde::Serialize` and `serde::Deserialize`; all migration fns must be generic over `Db` so the macro's `__WaveDbDb` parameter resolves at the call site.

   ```rust
   // ── Migration fns (async, generic over Db) ─────────────────────────────────
   async fn migrate_v41_v42<Db>(_db: &Db, old: Message41) -> Result<Message42> {
       Ok(Message42 { edited: false, ..old })
   }
   async fn rollback_v42_to_v41<Db>(_db: &Db, future: Message42) -> Result<Message41> {
       Ok(Message41 { id: future.id, metadata: future.metadata, body: future.body, author: future.author })
   }
   async fn v42_first_try<Db>(_db: &Db) -> Result<Option<Message41>> { Ok(None) }

   // ── v41 — first version: no migrate_from; declares its future ──────────────
   #[wave_db(
       struct_id = 7,
       NonUnique,
       migrate_rollback      = Message42,
       migrate_rollback_with = rollback_v42_to_v41,
   )]
   #[derive(serde::Serialize, serde::Deserialize)]
   pub struct Message41 { pub id: Id, pub metadata: Metadata, pub body: String, pub author: u64 }

   // ── v42 — current head: declares its predecessor; no migrate_rollback ──────
   #[wave_db(
       struct_id = 7,
       NonUnique,
       migrate_from      = Message41,
       migrate_from_with = migrate_v41_v42,
       first_try         = v42_first_try,
   )]
   #[derive(serde::Serialize, serde::Deserialize)]
   pub struct Message42 { pub id: Id, pub metadata: Metadata, pub body: String, pub author: u64, pub edited: bool }
   pub type Message = Message42;
   ```

   **Generated trait impls (compile-time chain):**

   | Trait | Direction | Generated impl |
   | ----- | --------- | -------------- |
   | `MigratesFrom { type Source }` | backward | `impl MigratesFrom for Message42 { type Source = Message41; }` |
   | `RollbackFrom { type Future }` | forward  | `impl RollbackFrom for Message41 { type Future = Message42; }` |

   Walk `Message42::Source::Source…` backward / `Message41::Future::Future…` forward to traverse the full chain in the type system — the registry can be **fully reconstructed from types alone**.

   **Generated associated functions:**

   | Method | Where | Description |
   | ------ | ----- | ----------- |
   | `Self::register_migration(&mut MigrationRegistry)` | each struct that declares any neighbour | **Optional.**  Wires the forward edge (if `migrate_from`) and/or backward edge (if `migrate_rollback`) into the runtime registry — useful for cluster routing.  Not required for cross-version reads (see below). |
   | `Self::__wave_db_migrate_from<Db>(&Db, OldType) -> Result<Self>` | on the newer struct | Upgrade an old record to `Self`. |
   | `Self::__wave_db_migrate_rollback<Db>(&Db, NewType) -> Result<Self>` | on the **older** struct | Receive a rolled-back future record. |
   | `Self::__wave_db_first_try<Db>(&Db) -> Result<Option<OldType>>` | on the newer struct | Pre-search hook. |
   | `Self::__wave_db_fallback_not_found<Db>(&Db) -> Result<Option<Self>>` | any struct | Post-search fallback. |

   **Generated trait impl — the tasty cross-version read:** every `#[wave_db]`-annotated struct also receives an `impl<Db> MigrationChain<Db> for Self`.  Its `read_as_self(db, bytes, stored_version)` method picks the right deserialisation path based on how `stored_version` compares to `Self::STRUCT_VERSION`:

   - `stored_version == Self::STRUCT_VERSION` → postcard-deserialize directly.
   - `stored_version < Self::STRUCT_VERSION` → recursively read as `Self::Source` (via `MigratesFrom`), then run `Self::__wave_db_migrate_from`.
   - `stored_version > Self::STRUCT_VERSION` → recursively read as `Self::Future` (via `RollbackFrom`), then run `Self::__wave_db_migrate_rollback`.

   The recursion terminates at chain ends (no `MigratesFrom` on the oldest version, no `RollbackFrom` on the current head).  Because the chain is encoded in the type system, **`MigrationChain` works without any `register_migration` call** — `Message42::search(&db)` automatically forward-migrates a stored Message41 record, and `Message41::search(&db)` automatically rolls back a stored Message42 record.  The wire format prepends a one-byte `STRUCT_VERSION` to each stored payload so the engine knows which direction to walk on read.

   **Lazy migration on read.** When the engine reads a record whose `struct_version` is behind the current compiled version, `MigrationChain::read_as_self` upgrades the bytes in memory (and the engine writes the result back in the background).  `first_try` runs ahead of the DB search; `fallback_not_found` runs after.  Rollback during mixed-version cluster deployments uses the symmetric backward chain.

---

## Anchor Slots — Solving All Cross-Pointer References

A core problem with versioned IDs: when an object mutates, every record that referenced its old ID would need rewriting. WaveDB resolves this with **Anchor Slots**.

**Anchors hold all cross-pointers for a given record**, giving the system a stable place to track index updates and follow forward references. Every cross-reference (index entry, M2M link, sync handle) targets the anchor — never a versioned record — so when the underlying data mutates, none of those pointers need to be rewritten.

Anchors are the universal solution for all cross-pointer references, including:

- **Many-to-Many (M2M)** — orders ↔ products, users ↔ shared documents
- **Single-to-Many (S2M)** — tenant → orders, post → comments
- **Indexes** — every index entry points to an anchor, never to a versioned record
- **Bloom filter sync** — clients track anchors, not historical versions
- **Alternate-key lookups** — a record reachable through more than one natural identifier (a `User` reachable by both `username` and `email`) uses **secondary anchors** described below.

Anchors keep the full list of inbound references in an array on the slot itself, so the system can track every pointer that resolves through it. That same list also tracks any **secondary anchors** the record exposes, so the primary always knows about every alias that points at it — which is what makes deletes, moves, and property mutations safe across all of a record's anchor addresses.

### Anchor Addressing

By default, an anchor is hashed at `(STRUCT_ID, TENANT_ID, SHARD_ID)`. For Unique data, `SHARD_ID = 0` collapses this to `(STRUCT_ID, TENANT_ID)` — exactly one anchor per type per tenant. For NonUnique data, the writing Quick-Node mints `SHARD_ID` from its owned range, giving each new record a fresh anchor address.

That's the default, and it's the right choice for the majority of structs. The `#[wave_db]` macro lets a struct opt into two more powerful addressing strategies when a record has natural keys.

#### Property-hashed primary anchors

Instead of using a node-allocated `SHARD_ID`, a struct can be addressed by **a hash of one of its fields**:

```rust
#[wave_db(struct_id = 25, NonUnique, primary_anchor = username)]
pub struct User1 {
    pub id:           Id,
    pub metadata:     Metadata,
    pub username:     String,
    pub email:        String,
    pub display_name: String,
}
pub type User = User1;
```

The macro hashes `username` into the `SHARD_ID` slot at write time. Three consequences:

- **Content-addressed lookup.** Anyone who knows the username can find the user with one IO — no index walk required. The macro emits `User::find_by_username(&db, "alice").await?` and that resolves directly to the anchor.
- **Implicit uniqueness.** Two users with the same username collide on the same anchor address; the engine surfaces this as a write error rather than silently duplicating, giving the struct "primary key" semantics for free.
- **Routing locality.** Records cluster by hash, not by allocation order. A Quick-Node owning a shard range owns a contiguous slice of the property-hash space.

Property-hashed anchors are the right choice when a struct has a stable natural key. Use the default node-allocated `SHARD_ID` when there isn't one, or when you specifically want the random distribution that a counter gives you.

#### Secondary anchors

A struct can also declare **additional anchor addresses that point back to the primary**:

```rust
#[wave_db(
    struct_id        = 25,
    NonUnique,
    primary_anchor   = username,
    secondary_anchor = (email),
    secondary_anchor = (department, employee_number),
)]
pub struct User1 {
    pub id:               Id,
    pub metadata:         Metadata,
    pub username:         String,
    pub email:            String,
    pub department:       String,
    pub employee_number:  u32,
    pub display_name:     String,
}
pub type User = User1;
```

This produces three anchor addresses, all resolving to the same record:

| Anchor        | Hashed by                       | Slot contents                                  |
| ------------- | ------------------------------- | ---------------------------------------------- |
| **Primary**   | `username`                      | Live data + ref list (incl. secondary anchors) |
| **Secondary** | `email`                         | Pointer back to the primary anchor             |
| **Secondary** | `(department, employee_number)` | Pointer back to the primary anchor             |

The macro emits one accessor per declared anchor:

```rust
impl User {
    pub async fn find_by_username       (db: &Db, name: &str)          -> Result<Option<User>>;
    pub async fn find_by_email          (db: &Db, email: &str)         -> Result<Option<User>>;
    pub async fn find_by_department_and_employee_number
                                        (db: &Db, dep: &str, n: u32)   -> Result<Option<User>>;
}
```

Each call is a single hash → 1 IO for the primary, 2 IOs for the secondaries (secondary anchor → primary anchor → done).

Secondary anchors live in the **primary anchor's reference list**, just like any other inbound pointer. The consequences:

- **Atomic delete.** Deleting the primary cascades into deleting every secondary anchor in the same write batch — the primary's own reference list is the worklist.
- **Consistent property mutation.** When the user changes their `email`, the engine deletes the old `hash(email)` secondary anchor, writes a new one at the new hash, and updates the primary's reference list — all under the same anchor lock.
- **No phantom aliases.** Because the primary owns the list of secondaries, an orphan secondary anchor is impossible by construction. There is no scenario where you can read a secondary that points at a vanished primary.

Secondary anchors are an alternative to discrete-value indexes (see _Index Structures_) for the common case of _"look up an object by exactly one of N natural keys."_ They cost one extra anchor slot per declared key (and one extra IO per non-primary lookup) and give O(1) point-lookup with no separate index file walk. They are not, however, a replacement for **range-ordered** indexes — for `WHERE created_at > X`, you still want a B+ tree.

#### How this interacts with the rest of the engine

- **Operating modes.** Inline-vs-pointer-only (below) applies to primary anchors. Secondary anchors are always pointer-only by construction — their entire job is to redirect.
- **Versioning.** Versioned records are still hashed at `(STRUCT_ID, TENANT_ID, SHARD_ID, CREATED_AT)` regardless of how the anchor is addressed; only the _anchor's_ `SHARD_ID` is the property hash. The version chain is unchanged.
- **Routing.** The Quick-Node owning the shard range that contains `hash(primary_anchor_field)` is the writer. The same range-ownership protocol from _Distributed Architecture_ covers it.
- **`Id` semantics.** A record written through a property-hashed anchor still has a `created_at` field in its `Id`; only the `SHARD_ID` slot is committed to the property hash. Time-of-creation is not lost.

### Two Operating Modes

Anchors support two modes, chosen per deployment / per node profile:

| Mode             | Slot contents                    | Read cost           | Storage cost           | Typical use                                     |
| ---------------- | -------------------------------- | ------------------- | ---------------------- | ----------------------------------------------- |
| **Inline data**  | Full live record bytes + marker  | 1 IO                | Higher (~2x live data) | **Quick-Nodes** — hot, latency-sensitive paths  |
| **Pointer-only** | Pointer to versioned record only | 1 extra IO per read | Lower (no duplication) | Storage-constrained or cold-leaning deployments |

Inline mode trades disk space for one fewer I/O on the read path. Pointer-only mode keeps anchors tiny (just the address + reference array) at the cost of an extra hop to fetch data. The Quick-Node tier defaults to inline; archive-leaning deployments can opt into pointer-only.

### Two Slots Per Live Record

| Slot          | Hashed at                                         | Contents                                                                                                 |
| ------------- | ------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| **Anchor**    | `(STRUCT_ID, TENANT_ID, SHARD_ID)` — no timestamp | Live data (inline mode) **or** pointer (pointer-only mode), plus marker `current_version_at: created_at` |
| **Versioned** | `(STRUCT_ID, TENANT_ID, SHARD_ID, CREATED_AT)`    | Full data + modification chain (`old_mod_id`, `new_mod_id`)                                              |

`SHARD_ID` is `0` for Unique data, so its anchor key collapses to `(STRUCT_ID, TENANT_ID)` exactly as before. For NonUnique data, `SHARD_ID` is either node-allocated or set to a property hash, depending on the struct's anchor addressing strategy (see _Anchor Addressing_ above).

### How Mutation Works

1. New versioned record is written at the new `created_at` hash with `old_mod_id` pointing to the previous version.
2. The previous versioned record's `new_mod_id` is updated to point forward.
3. The anchor slot is overwritten with the new data and the new `current_version_at` marker.

That's still 2–3 IOPs per write — same cost as the no-anchor design — but the structural payoff is large:

- **References never need rewriting on mutation** — they all target the anchor address.
- **Sync queries by versioned ID always resolve** — the historical record exists at its versioned hash from the moment it was written.
- **Cross-references work even before sync completes** — the anchor is the stable handle.

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

1. Dictionary updates (rebuilt as the data distribution shifts) are first written to the **journal**.
2. A **background task** consumes journal entries and rewrites the affected entries in `dictionaries_file`.
3. Pages written under an old dictionary version remain readable — each page header carries the dictionary version it was compressed with.
4. Lazy re-compression: when a page is rewritten for any other reason, it picks up the latest dictionary.

This means **dictionary rebuilds are never on the write hot path** — the journal absorbs the cost and the background task amortises the disk writes.

#### Why This Works

- All records of one STRUCT share enum values, ID prefixes, common timestamp ranges, and field-position layout — dictionaries achieve very high compression ratios.
- Per-STRUCT scoping keeps dictionaries small (often <64KB) so many fit in memory simultaneously.
- Cold STRUCTs don't waste RAM.
- Dictionary versioning means old pages stay readable forever — no migration cliff.

---

## Heap Data Strategy (Detailed)

### Step 1 — Inline compression

zstd compress and store at the end of the page.

### Step 2 — Overflow to a hashed block

If still too large, the page slot stores a pointer `(STRUCT_ID, TENANT_ID, data_id)` and the value goes to a separate overflow block addressed by that pointer's hash.

### Step 3 — Heap Anchor for very large values

When the value exceeds what an overflow block can hold cleanly, WaveDB writes a **Heap Anchor**: a small indirection record in the data file that points to the actual bytes living at the tail of the heap file.

- The heap anchor is hashed by `(CREATED_AT, SHARD_ID, TENANT_ID)` instead of the usual `(STRUCT_ID, TENANT_ID)`. Because `SHARD_ID` is range-owned by a single Quick-Node and that node serialises within each 100µs tick, this hash is unique per record without coordination — heap anchors don't collide with normal Anchor Slots.
- The anchor record itself is tiny: a single pointer `(offset, size)` into the heap file.
- The actual bytes are appended to the **tail of the heap file**, padded out to keep every entry **4KB-aligned**. This guarantees that heap reads never straddle a page boundary, regardless of the underlying value's size.

A read for an oversized field costs a bounded **2 IOs**: one for the heap anchor, one for the heap data — independent of how large the value actually is.

> **Trade-off vs. earlier deduplicating linked-list design:** This approach drops the per-tail ownership-dedup that an earlier sketch carried. In exchange, large-value reads stop scaling with chain depth and heap writes are pure appends. Content-addressed dedup can be layered on top later if profile data shows it's worth the complexity.

---

## Files on Disk

WaveDB splits its on-disk state across **four files**, each tuned for a different access pattern. (How these are physically combined is controlled by _Operation Modes (file layout)_ further down — single-file mode merges them all; production splits them.)

| File           | Contents                                                               | Layout                                   | Access pattern                                |
| -------------- | ---------------------------------------------------------------------- | ---------------------------------------- | --------------------------------------------- |
| `data` file    | Hash-mapped pages — Anchor Slots, versioned records, heap-anchor stubs | Page-addressed (hash → page)             | Random IO, page-sized                         |
| `index` file   | B+ tree nodes and small-collection array indexes                       | **Highly contiguous**; nodes 4KB-aligned | Sequential within a tree, random across trees |
| `heap` file    | Compressed / oversized payloads + the heap-anchor append region        | Append-mostly, 4KB-padded                | Append on write, point IO on read             |
| `journal` file | In-flight mutations, dictionary updates, free-space deltas             | Append-only                              | Append + sequential replay on startup         |

### Why split

Index entries reference data-file records **by ID**, never by file offset. That keeps B+ trees portable across rebalances and lets the index file be compacted independently of the data file without rewriting any pointers. The same logic applies to heap-anchor stubs: they address the heap by `(offset, size)` only at the moment of read — between reads, ranges are free to be relocated by cleanup.

The index file's contiguous layout is the payoff of the per-(STRUCT_ID, TENANT_ID) tree design: millions of tiny trees laid out next to each other compress and prefetch well, and they never share pages with the random-access data file.

---

## Collision & Fullness Strategy

Multiple `(STRUCT_ID, TENANT_ID)` pairs naturally share pages. When a target page is full:

1. **Double hashing** finds an alternative candidate page.
2. Double hashing firing at a meaningful rate **is the signal** — the file is approaching capacity and a rebalance triggers automatically.

The collision resolution mechanism _is_ the alert system.

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

- A single 4 KB node holds **~170 index entries**.
- A tree of depth 2 covers nearly **30,000 items**.
- **>99% of tenant index lookups complete in 1 or fewer disk I/O reads.**

The threshold is tunable per-STRUCT via a proc-macro attribute:

```rust
#[wave_db(struct_id = 7, NonUnique, btree_threshold = 100)]
pub struct Message1 { ... }
```

> **Index types:** Ordered indexes use the standard B+ tree shape described above. Discrete (value-bucketed) indexes use a hash bucket → array-or-tree model — each bucket starts as an array and promotes to a per-bucket B+ tree if it grows past the threshold.

> **When to prefer secondary anchors over a discrete index:** if the only thing you need from a property is a _point lookup_ by exact value (no range scans, no ordered iteration), a `secondary_anchor` declared on the struct is cheaper than a discrete index — it costs one anchor slot per record instead of a per-tenant index file walk, and the macro generates the typed accessor for you. Reach for a discrete index when the same property needs to support listing all records that share a value, or when the property is added retroactively to an existing struct without a schema migration.

### Deleted is a First-Class Index

**Deleted records are never removed from indexing** — they move from the Current B+ tree to the Deleted B+ tree. Reconstructing collection state at a past timestamp requires both trees.

### Two Storage Strategies for Iter<T> Fields

#### `try_heap_inline` — Small iterables, minimal overhead

```rust
#[wave_db(struct_id = 15, wave_db::try_heap_inline)]
pub struct TagList1 {
    tags: Iter<u8>,
}
```

Stored as a heap-inline linked-list chunked by `MAX_HEAPED_SIZE`. Each chunk has its own ID. No cross-pointer maintenance.

#### Default (`AsRef`) — Full index support

```rust
#[wave_db(struct_id = 10)]
pub struct Order1 {
    items: Iter<OrderItem1>,
}
```

Maintains the full index graph above. Property mutations only rewrite the index for the changed property — and because index entries point to anchors, even those rewrites only update keys, not pointer values.

---

## History

History records live at their natural versioned hash `(STRUCT_ID, TENANT_ID, SHARD_ID, CREATED_AT)`. The Anchor Slot holds the live state. They never collide in the address space.

### Traversal

- **Backward** (present → past): start at anchor, follow `current_version_at` to the live versioned record, then `old_modification_id` chain.
- **Forward** (past → present): from any historical record, follow `new_modification_id` until you reach `new_mod_id == 0`. The anchor mirrors that record.
- **Lookup miss on versioned hash**: the record at that timestamp doesn't exist.
- **Stale ID via anchor**: anchor's `current_version_at` reveals the latest version.

---

## Permissions

WaveDB stores access control **inline in `Metadata`**, scoped per record. The `permission` field is an `Option<PermissionRef>` and behaves as follows:

| Value                                  | Semantics                                                                                                                                                                                             | Storage cost (postcard) |
| -------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------- |
| `None`                                 | Only the tenant's own users can read/write/delete this record (the common case for B2C apps).                                                                                                         | 1 byte                  |
| `Some(PermissionRef::Inline(list))`    | A small inline ACL — a linked list of user IDs allowed to act on this record. Used for small tenants and short-lived shares. Auto-promotes to a per-record B+ tree once the list crosses a threshold. | 1 byte tag + list bytes |
| `Some(PermissionRef::Group(group_id))` | Reference to a separately-stored permission group, suited to large tenants where many records share an ACL.                                                                                           | 1 byte tag + group ref  |

### Granted Operations by Data Shape

| Data shape                 | Operations                               |
| -------------------------- | ---------------------------------------- |
| Unique                     | `read`, `"update"`, `create`             |
| NonUnique                  | `read`, `"update"`, `create`, `"delete"` |
| NonUnique-within-NonUnique | `read`, `"update"`, `create`, `"delete"` |

Unique records have no `delete` because deleting the _only_ record of its kind for a tenant is semantically a tenant-level action, not a record-level one.

> All permission checks are local to the tenant — only users that belong to the same tenant are checked against the rule. Cross-tenant sharing (a contractor on another tenant accessing your records) is a separate problem, tracked as P15 in _Known Problems_.

---

## Migrations

WaveDB uses **a single migration model**: each versioned struct declares its **immediate neighbours** in the chain via TYPE paths, and async forward / rollback functions.  The legacy "Type 2 compose" pattern is folded into the same model via the `first_try` pre-search hook.

### The neighbour model

A versioned struct can declare up to four migration attributes:

```rust
#[wave_db(
    struct_id = 7,
    NonUnique,

    // ── Backward edge: who comes before me, and how to upgrade from them ───
    migrate_from          = Message41,
    migrate_from_with     = migrate_v41_v42,     // async fn<Db>(&Db, Message41) -> Result<Self>

    // ── Forward edge: who comes after me, and how to receive a rollback ────
    migrate_rollback      = Message43,
    migrate_rollback_with = rollback_v43_to_v42, // async fn<Db>(&Db, Message43) -> Result<Self>

    // ── Search-time hooks ────────────────────────────────────────────────
    first_try             = v42_first_try,       // before DB search → Option<Message41>
    fallback_not_found    = v42_fallback,        // after DB returns None → Option<Self>
)]
pub struct Message42 { /* … */ }
```

**Chain bounds:**

- The **first (oldest) version** has no `migrate_from`.  It may declare `migrate_rollback` once a successor exists.
- The **last (current) version** has no `migrate_rollback` yet — nothing has been written for it to receive a rollback from.
- Every other struct declares both.

The rollback function is co-located with the **older** struct ("I know how to receive my future self and become me again"), which keeps the inverse operation next to the type it produces.  Each struct's `register_migration` contributes only its own edge(s); the full chain emerges as every versioned struct's `register_migration` runs at startup.

### Compile-time chain visibility

Two traits expose the chain to the type system:

| Trait | Reads as | Walks |
| ----- | -------- | ----- |
| `MigratesFrom { type Source }` | "I migrate from `Source`" | backward (`Message42::Source = Message41`) |
| `RollbackFrom { type Future }` | "I receive rollbacks from `Future`" | forward (`Message41::Future = Message42`) |

The full chain is traversable at compile time:

```text
Message42::Source          → Message41
Message42::Source::Source  → does not compile (Message41 has no MigratesFrom)
Message41::Future          → Message42
Message41::Future::Future  → does not compile (Message42 has no RollbackFrom yet)
```

The "does not compile" outcome is the **chain-bound check**: the type system tells you you've reached an end of the chain without running anything.  This is the property that lets the registry be reconstructed entirely from types.

### `first_try` and `fallback_not_found` — replacing Type 2

A pre-version of WaveDB had a separate "Type 2" migration kind for the case where a new struct (`OrderSummary`) is computed from multiple older structs (`Order`, `OrderItem`).  That pattern is now expressed with `first_try`:

1. The engine starts a search for `OrderSummary`.
2. **`first_try(&db)` runs first.**  It looks up the constituent records and either returns `Some(synthesised_source)` (which then flows through `migrate_from_with` to produce the new struct), or `None` to fall through.
3. If the normal DB search also returns `None`, **`fallback_not_found(&db)` runs** as a last resort — useful for default-record synthesis when no data has ever been written.

This unifies the "compose from multiple sources" case with normal Type-1 migrations: it's the same pipeline; the application just plugs into a different stage.

### Lazy migration on read

When a record is read with a `struct_version` behind the current compiled version:

1. The engine resolves the forward path through `MigrationRegistry::resolve(stored_v, current_v)`.
2. It walks each step, calling the typed `__wave_db_migrate_from` wrapper on each successive version's struct.
3. The upgraded record is written back to the anchor **in the background** — no global lock, no downtime.

This is the **lazy-migration trigger**: stored bytes are upgraded on first contact, transparently to the application.

### Rollback during mixed-version deployments

WaveDB is a distributed system.  A cluster can simultaneously contain:

- Slow-Nodes still serving an old `struct_version`,
- Quick-Nodes already on the new one,
- User clients running mixed builds.

When a node on an older build receives a record at a newer version, it walks the registered **backward** edges (`MigrationRegistry::can_rollback` / the typed `__wave_db_migrate_rollback` wrappers on the older structs) to bring the record down to a version it can read.  Forward and backward translations must both exist for **any two adjacent versions to coexist live**.

### Code-side rollback

Rolling back the application is a one-line edit:

```rust
pub type Message = Message43;
```

back to

```rust
pub type Message = Message42;
```

…and the database keeps both readable as long as the `migrate_rollback_with` function on `Message42` (which receives `Message43` and produces `Message42`) is still compiled in.  No separate "downgrade" deployment; the older `pub type` alias just becomes the active head again.

### Migration chains

A node arriving at the cluster on a very old version doesn't need a direct migration to the current version.  The engine **chains migrations** — `v_n → v_{n+1} → … → v_current` — picking a path through registered edges.  Each step is the same async fn, and each step's rollback is also chainable, so a node can step backward by the same mechanism.

---

## Distributed Architecture

### Same Binary

Server and database are **the same binary**. No separate DB process, no ORM, no protocol translation.

### Two Tiers (Cassandra-inspired)

The cluster is split into two physical tiers serving different roles in the same partition map.

#### ⚡ Quick-Nodes (the "hot" layer)

- **Hardware:** Good CPU, good RAM, fast NVMe SSDs of moderate capacity.
- **Role:** Owns specific `(TENANT_ID, SHARD_ID)` partitions via a Consistent Hash Ring; takes routing ownership for connected users; validates interactions; replicates to peers.
- **Behaviour:** Holds active **Anchor Slots in memory**. Anchors here run in **inline-data mode** — the extra storage is cheap relative to the read-IO it saves.

#### 🧊 Slow-Nodes (the "cold" layer)

- **Hardware:** Lower CPU, moderate RAM, large-capacity HDD or SSD arrays.
- **Role:** Acts as the **Immutable Journal** and history archive.
- **Behaviour:** Quick-Nodes continuously flush older versions and transaction logs down to Slow-Nodes, releasing active disk space on the hot tier and maintaining permanent history off the latency path.

This separation lets each tier specialise its hardware: Quick-Nodes are sized for IOPS-per-watt, Slow-Nodes for $/TB.

### Ownership Model

Two ownership scopes, both following the same protocol — **one writer, n replicas**:

1. **Tenant ownership** (default for Unique data). Exactly one Quick-Node owns each tenant. Other replicas receive the data for redundancy and read-fallback. If the owner crashes, a friendly replica takes over.
2. **Shard ownership** (for NonUnique data). The 12-bit `SHARD_ID` space is partitioned into **runtime-negotiated ranges**, with one Quick-Node owning each range for a given tenant. Each NonUnique struct picks a property to hash into its `SHARD_ID` so the hash is the routing key. The range subdivision granularity adapts to the tenant's NonUnique cardinality — a tenant with millions of orders has many shards split across many nodes; a tenant with dozens has few shards on one node.

In both cases the **owner is the only writer**. It is responsible for:

- Validating the mutation,
- Notifying the replicas that also store the same `(TENANT_ID, SHARD_ID)` partition,
- Forwarding to a Slow-Node for cold persistence and history release.

### Replication Policy

Each `(TENANT_ID, SHARD_ID)` partition lives on **at least 2 Quick-Nodes** by default (configurable via `MIN_REPLICAS`). The placement algorithm picks **physically distant nodes** — different sub-networks, different racks — so a single sub-network failure cannot take both copies offline.

### Routing & Failover

A client always knows **two** Quick-Nodes for its session: the **owner** and a **backup**. Both URLs are returned at connect time.

- The client sends mutations to the owner.
- The owner replicates and acks.
- If the client times out on the owner, it switches to the backup and asks for the new owner — a small pause, no data loss.

If a mutation hits the wrong node by mistake, the receiving node either proxies forward to the owner or — usually cheaper — _requests_ ownership transfer for that partition. Range moves are a runtime operation, not a deployment.

### 📡 Bloom Filter Screen-Sync

A state-sync mechanism for the **online** read path. Clients maintain a Bloom filter of the 128-bit IDs currently rendered on screen and send it to the primary Quick-Node over WebSocket. The owner compares the filter against its live Anchor Slots and pushes back **only the deltas**: new objects, updated anchors, tombstones. This is event-driven — every mutation the owner accepts triggers a "new state" notification to the subscribers whose filters might match.

> **When to use what:** Bloom filter sync is the right tool for **online clients** with a small, evolving working set. For clients that have been offline for a long time — where the screen state is far behind reality — it is cheaper for the client to send the array of on-screen IDs back to the server for explicit revalidation.

### Distribution philosophy

The distribution machinery exists primarily to let Quick-Nodes flush history to Slow-Nodes and reclaim hot-tier space. Past that, the ownership model is deliberately simple — most of the engineering energy goes into the per-node storage engine, not into cluster choreography.

### Consistency Model

- **Within one tenant:** strong (single writer via routing ownership).
- **Across tenants:** eventual (Bloom filter sync).
- **On conflict:** most recent state wins via anchor; loser becomes a branch in the history chain.

### Offline-First (deferred)

> An earlier draft proposed an offline-first user-side mode using a Slip-ID reconciliation protocol. **This is on hold.** The current design assumes the client is online whenever it writes; offline durability for end-user clients is not in scope for the first cut.

---

## Client API & Operation Modes

WaveDB ships as a library with the same code path on servers, native clients, and (compiled to WASM) browsers. The four supported entry-points cover both server-managed and self-hosted setups:

| Mode                                        | Storage location                  | Tenant model                |
| ------------------------------------------- | --------------------------------- | --------------------------- |
| `Db::open(url, path, user)`                 | Local file at `path`              | `tenant = user_id`          |
| `Db::open(url, path, user, default_tenant)` | Local file at `path`              | Tenant explicit (companies) |
| `Db::open(url, user, default_tenant)`       | Browser localStorage (WASM build) | Tenant explicit             |
| `Db::open(url, user)`                       | Browser localStorage (WASM build) | `tenant = user_id`          |

In all four, `url` resolves to the cluster's front door — the request is then redirected to the Quick-Node currently owning the user's tenant, and the second URL of the **backup** Quick-Node is returned alongside it for failover. The native modes use the local `path` as a write-through cache and as the file layer that sits underneath `tokio::broadcast`; the WASM modes use browser localStorage in the same role.

Once a `Db` instance exists, it can spawn **another `Db` for a different tenant**:

```rust
let other = db.another_tenant(other_tenant_id).await?;
```

…which routes against the same cluster but with a different ownership target.

### Object lifecycle

Objects can never be **created in isolation**. Every `create` is preceded by a "does this exist?" check — if it does, you `update`; if it doesn't, the engine assigns a fresh `Id` and `Metadata` and saves. Local code uses `Default::default()` for both `Id` and `Metadata`, and the engine fills in the real values at `save` / `send` time.

```rust
// Lookup a Unique record — returns Option because the record may not exist yet.
let profile: Option<UserProfile> = UserProfile::search(&db).await?;

// Lookup NonUnique with a query (sea_orm-flavoured Expression).
let recent: Vec<Order> = Order::query(&db, expr.gt(Order::amount, 100)).await?;

// Update (versioned in place) and delete (NonUnique only).
order.update(&db).await?;
order.delete(&db).await?;
```

The `Drop` impl on `Db` notifies its Quick-Node so the node can release the session promptly.

### Unauthenticated sessions

A client without credentials connects with `user = U48::MAX`. This session sees only **public data** — the API surface is restricted to login (password, Google, etc.) and reading data tagged as world-readable.

---

## Connection Methods

WaveDB **is** the wire protocol. Clients and servers don't communicate through a separate REST API or RPC layer built on top of the database — the same operations that read and write objects locally also flow over the network as the application's only protocol. There is no DTO layer, no API schema to keep in sync with the storage schema, and no "DB models vs. API models" split. Whatever the client serializes as a query, the server deserializes directly into the storage engine.

| Transport                   | Native client | Browser client | Notes                                                          |
| --------------------------- | ------------- | -------------- | -------------------------------------------------------------- |
| **WebSocket**               | preferred     | preferred      | Bidirectional, push-capable; carries Bloom-filter screen-sync  |
| **HTTP POST**               | fallback      | fallback       | Used when WebSocket is blocked (proxies, restrictive networks) |
| **Future native transport** | planned       | n/a            | A higher-throughput native-only transport is in scoping        |

### HTTP POST: single-queue with piggybacked notifications

Plain HTTP is request/response and unidirectional from the server's perspective — it cannot push. To make HTTP POST behave like the WebSocket transport for the bits that matter (delivering "object changed" notifications to the UI), the client side runs a small queue:

1. **One queue per client.** All outbound requests from a session are funnelled into a single FIFO queue. The Quick-Node processes them in order; there is **no concurrent in-flight POST** per session. This keeps ordering deterministic and lets the server attach state changes to whatever response is going back next.
2. **Responses can carry more than was asked for.** A POST response is allowed to include data the request didn't ask for — specifically, **notifications about objects on the user's screen that have changed since the last exchange**. From the application's point of view, this is the same `new state` event the WebSocket pushes; it just hitches a ride on the next HTTP response. The client UI updates from these piggyback payloads exactly as it would from a WebSocket push.
3. **Idle ticks when the queue empties.** When the client has nothing to ask for and the queue is empty, it doesn't go silent. It starts ticking **empty POSTs at a configured interval** (`http_poll_interval`) so the server has a regular opportunity to flush pending notifications. The interval is configurable per deployment and the client backs off gradually when no notifications arrive for a while.
4. **WebSocket and the future native transport skip all of this.** Both are bidirectional and push-capable, so notifications arrive without polling and the request queue can run with normal concurrency.

The net result: the application code is identical across transports — it always reacts to "object changed" events. The transport layer is responsible for getting those events to it, by push (WebSocket / native) or by piggyback (HTTP).

### Browser specifics

The WASM build replaces the Tokio runtime: futures run via `wasm_bindgen_futures`, HTTP goes through the browser `fetch` API, WebSockets go through `gloo_net::websocket`. The public API is identical.

---

## Operation Modes (file layout)

| Mode                  | Description                             | Use case              |
| --------------------- | --------------------------------------- | --------------------- |
| **Single File**       | Data + history + journal in one file    | Development, embedded |
| **Separated History** | Live data and history in separate files | Production            |
| **History Only**      | History records only                    | Archive / Slow-Node   |

---

## Everything is `async`

Every public API on `Db`, every storage actor, every migration — `async` end to end. There is no blocking IO surface and no thread-pool dispatch hidden from the caller. On native this runs on Tokio; in the browser it runs on `wasm_bindgen_futures`.

---

## ⚙️ Write Pipeline & Concurrency Model

The write path is structured as a **journaled cache that fronts the durable files**. The cache is shaped exactly like the on-disk hash-map, so reads can serve directly from it without an extra format conversion. Two parameters govern the pipeline's behaviour:

| Parameter         | Description                                            |
| ----------------- | ------------------------------------------------------ |
| `MAX_DISK_IOPS`   | Soft IO budget per second across all background actors |
| `MAX_CACHED_SIZE` | Total RAM budget for the in-memory write/read cache    |

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

| File                                       | Concurrency model                                                                                   |
| ------------------------------------------ | --------------------------------------------------------------------------------------------------- |
| `data` file (random-access)                | Concurrent writes allowed across **different pages**; same-page writers serialise on the page mutex |
| `index`, `heap`, `journal` (append-mostly) | Multiple writers prepare bytes in parallel; only the actual append step takes the block-level mutex |

A **tokio broadcast channel** publishes page invalidations and free-space events so each actor can drop stale cache entries without polling. The only contended primitive is the per-block mutex inside each file — there is no global write lock.

### Idle work

When the actor pool isn't busy with reads or writes and has IO budget left under `MAX_DISK_IOPS`, it picks up background work — in this priority order:

1. **Rebalance task** — intentionally low-priority. The normal write path already rebalances incrementally through the double-hashing collision mechanism (see _Collision & Fullness Strategy_); standalone rebalance is only catching up corner cases, which is why it doesn't need its own scheduler tier.
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

**Journal:** Append-only file recording in-flight page mutations, dictionary updates, **and free-space deltas**. Replayed on startup. Anchor + versioned writes are journaled together so crashes mid-mutation can never leave the two slots inconsistent. The full write/cleanup discipline is described in _Write Pipeline & Concurrency Model_ above.

**Checksums:** Every page carries a checksum verified on read.

**Reed-Solomon (optional):** Per-page error correction for archive nodes.

---

## Configuration Parameters

| Parameter                       | Description                                                                  | Default              |
| ------------------------------- | ---------------------------------------------------------------------------- | -------------------- |
| `page_size`                     | Bytes per page                                                               | varies by deployment |
| `page_counter`                  | Number of pages in file                                                      | grows as needed      |
| `max_heap_inline`               | Largest heap value stored inline before overflow                             | 25% of page_size     |
| `warning_size_page_occupation`  | Fill threshold to alert                                                      | 70%                  |
| `max_dict_memory`               | RAM budget for STRUCT_ID dictionaries                                        | 64 MB                |
| `MAX_NON_UNIQUE_ELEMENTS`       | Per-STRUCT_ID array → B+ tree conversion threshold                           | 50                   |
| `MAX_CACHED_SIZE`               | RAM budget for the in-memory write/read cache; writes block when near limit  | tunable              |
| `MAX_DISK_IOPS`                 | Soft IO budget per second across all background actors                       | hardware-dependent   |
| `MIN_REPLICAS`                  | Minimum number of Quick-Nodes holding each `(TENANT_ID, SHARD_ID)` partition | 2                    |
| `lock_timeout`                  | Max hold time for an ID lock                                                 | 30 s                 |
| `bloom_filter_publish_interval` | How often nodes share ownership filters                                      | 1 s                  |
| `http_poll_interval`            | Idle tick rate of the HTTP-POST client queue when no requests are pending    | 2 s                  |

---

## Known Problems & Research Areas

### ✅ Resolved

| #   | Problem                    | Resolution                                                                                                                                                                                                                           |
| --- | -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| P1  | Heap overflow strategy     | zstd inline → hashed overflow block → **Heap Anchor stub** in the data file (hashed by `(CREATED_AT, SHARD_ID, TENANT_ID)`) addressing 4KB-padded entries appended to the heap-file tail; bounded 2-IO read regardless of value size |
| P2  | Hash collision / page full | Tenants share pages naturally; double hashing as fallback; double hashing rate = rebalance trigger                                                                                                                                   |
| P3  | Heap compression           | zstd; CPU is free because no join processing                                                                                                                                                                                         |
| P4  | Cross-tenant queries       | Out of scope by design — application context always knows the tenant                                                                                                                                                                 |
| P5  | STRUCT versioning          | `struct_version` (u8) in `Metadata`; lazy migration on read, background rewrite; chained migrations between any two registered versions                                                                                              |
| P6  | Multi-tenant sharing       | Tenant-defined permissions struct scoping user access (see P14)                                                                                                                                                                      |
| P7  | Many-to-many relations     | **Anchor Slots** — fixed `(STRUCT_ID, TENANT_ID, SHARD_ID)` address with live data; references never need rewriting. Same mechanism handles S2M and indexes.                                                                         |
| P8  | Stack data compression     | **Per-STRUCT_ID dictionaries** in memory bounded by `max_dict_memory`; updates journaled, applied to `dictionaries_file` by background task; pages carry dictionary version for backward compatibility                               |
| P11 | Index maintenance cost     | Per-(STRUCT_ID, TENANT_ID) B+ trees with adaptive conversion from array at threshold; index entries point to anchors so property mutations don't cascade                                                                             |
| P12 | Adaptive index threshold   | Default 50 items (`MAX_NON_UNIQUE_ELEMENTS`), tunable per-STRUCT_ID via proc-macro attribute; one-way atomic conversion                                                                                                              |
| P13 | Anchor storage cost        | Accepted as design trade-off (2x live data only, history single-copy); opt-in pointer-only mode available for storage-constrained deployments                                                                                        |
| P14 | Permissions struct design  | `permission: Option<PermissionRef>` in `Metadata`; `None` is 1 byte under postcard; inline-list (auto-promoting to a small B+ tree) for ad-hoc shares; group reference for large-tenant ACLs                                         |

---

### 🟡 P9 — Rebalancing Under Load

Background rebalance task with backpressure. Force-rebalance API for maintenance windows. Multiple simultaneous rebalancing epochs theoretically possible in extreme growth; only the most recent is primary.

---

### ⏸ P10 — Offline-First Reconciliation (deferred)

Earlier drafts described a Slip-ID reconciliation mechanism for clients that wrote while offline. **Deferred**: the current design assumes online-while-writing. Routing ownership prevents true multi-writer conflicts in the online case, so the slip mechanism is no longer on the critical path. Returning to offline-first is planned but explicitly out of scope for the first cut.

---

### 🔴 P15 — Cross-Tenant Permission Sharing

**Problem:** When a tenant shares data with another tenant (e.g., a contractor accessing client files), the anchor lives in the original tenant's partition but needs to be reachable from the recipient's session.

**Direction:** Reciprocal capability records. The granting tenant writes a `Capability` record (a small struct containing the target anchor address) into the recipient's partition. The recipient's queries that include "shared with me" walk capability records as a discovery mechanism. The actual data access still routes back to the original tenant's node.

---

## Non-Goals

- **Not OLAP.** Cross-tenant aggregations belong in a dedicated analytics pipeline.
- **Not a general consensus system.** Consistency is tenant-scoped; multi-tenant eventual consistency is by design.
- **Not a SQL replacement.** The query model is deliberately constrained.
- **Not offline-first (yet).** See P10.

---

## Implementation Language

Rust. Rationale:

- `#[wave_db]` proc-macro enforces and generates ID/Metadata structure at compile time.
- `STRUCT_ID (u20)` numbering is a compile-time counter — the type system guarantees stability.
- `struct_version (u8)` is parsed from the trailing integer of the type name — `Message42` ⇒ version 42.
- `Mutex`-based ID locking is idiomatic.
- `repr(C)` page structs map directly to disk.
- `async` iterators integrate naturally for the `Iter<T>` collection API.
- The same source compiles to native (Tokio) and to browser (WASM via `wasm_bindgen_futures`, `fetch`, `gloo_net`).

---

## Status

🔬 **Research Phase** — This document is a living design record. The core architecture is now defined; remaining open problems (P9, P15) are operational rather than foundational, and P10 is intentionally deferred.

---

## License

TBD — research project, not yet licensed.
