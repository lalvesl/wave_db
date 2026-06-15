# wavedb-core

Core primitives shared by **every** node kind (quick, slow, client) and by
proc-macro generated code: the composite `Id`, `Metadata`, the migration
registry, permission refs, the `Wire` serialization trait, and the query
expression tree. **No I/O** — safe in WASM, in macros, everywhere.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Module map

| Module       | Responsibility                                                  |
| ------------ | -------------------------------------------------------------- |
| `id`         | The 128-bit composite `Id` and its field accessors.            |
| `metadata`   | `Metadata` — version chain, authorship, permission ref.        |
| `migration`  | `MigrationRegistry`, chain traits, `MigrationChain` read path. |
| `permission` | `PermissionRef` shapes.                                        |
| `wire`       | The `Wire` trait + `WaveWire` (no serde). See `docs/wire_format.md`. |
| `query`      | `Expr` / `Value` / `Field` query expression tree.             |
| `registry`   | `ObjectDescriptor` / `ObjectRegistry` lookup by header.       |
| `traits`     | `WaveDbStruct`, shape markers.                                 |
| `error`      | Workspace error type.                                          |

---

## The ID

Every record has a composite ID of exactly **128 bits**:

```
[ TENANT_ID (u48) | SHARD_ID (u12) | STRUCT_ID (u20) | CREATED_AT (u48, 100µs precision) ]
```

| Field        | Type  | Description                                                                                              |
| ------------ | ----- | ------------------------------------------------------------------------------------------------------- |
| `TENANT_ID`  | `u48` | Owner. `0` reserved for the system; `U48::MAX` for unauthenticated sessions.                            |
| `SHARD_ID`   | `u12` | Range-allocated to a Quick-Node. `0` for Unique data. For NonUnique, node-minted or `hash(primary_anchor)`. |
| `STRUCT_ID`  | `u20` | Table/object type, fixed at compile time, shared across every version of a struct family.              |
| `CREATED_AT` | `u48` | 100µs ticks since a custom epoch.                                                                       |

**Why no slider.** With range-mode shard ownership there is one writer per
`(STRUCT_ID, TENANT_ID)` for Unique data, and disjoint shard ranges per node for
NonUnique — collisions inside a 100µs tick can't happen by construction, so the
old 8-bit `SLIDER` was reclaimed (4 bits → `SHARD_ID` 8→12, 4 bits → `STRUCT_ID`
16→20).

---

## Metadata

```rust
pub struct Metadata {
    pub old_modification_id: u128, // previous version (0 = first)
    pub new_modification_id: u128, // next version (0 = live)
    pub struct_version: u8,        // schema version at write time (lazy migration)
    pub user: u48,                 // who wrote this version
    pub device_created: u64,       // which device produced it
    pub permission: Option<PermissionRef>, // access rule; None = tenant-only
}
```

`(struct_version, user)` is 56 bits and packs alongside `device_created` and the
optional permission field in the wire stack.

---

## Schema Versioning & Lazy Migration

`struct_version` lives in every object's `Metadata`. On read, if it is behind
the compiled head, the migration transform runs in memory and the record is
written back **in the background** — partial, progressive, no global lock.

---

## Migrations

**One model.** Each versioned struct declares its **immediate neighbours** via
TYPE paths plus async forward/rollback fns; the macro (see `wavedb-macros`)
reconstructs the whole chain at compile time. The legacy "Type 2 compose"
pattern folds in via the `first_try` pre-search hook.

### Neighbour attributes

| Attribute               | Direction | Kind     | Signature / value                              |
| ----------------------- | --------- | -------- | ---------------------------------------------- |
| `migrate_from`          | backward  | type     | The predecessor struct.                        |
| `migrate_from_with`     | backward  | async fn | `async fn<Db>(&Db, Old) -> Result<Self>`       |
| `migrate_rollback`      | forward   | type     | The successor (this struct receives rollback). |
| `migrate_rollback_with` | forward   | async fn | `async fn<Db>(&Db, New) -> Result<Self>`       |
| `first_try`             | —         | async fn | `async fn<Db>(&Db) -> Result<Option<Old>>` — runs **before** the DB search. |
| `fallback_not_found`    | —         | async fn | `async fn<Db>(&Db) -> Result<Option<Self>>` — runs **after** a `None`.      |

**Chain bounds:** oldest has no `migrate_from`; current head has no
`migrate_rollback`; every middle version declares both. Rollback co-locates with
the **older** struct ("I know how to receive my future self and become me").

### Compile-time chain

| Trait                          | Reads as                            | Walks                            |
| ------------------------------ | ----------------------------------- | -------------------------------- |
| `MigratesFrom { type Source }` | "I migrate from `Source`"           | backward (`M42::Source = M41`)   |
| `RollbackFrom { type Future }` | "I receive rollbacks from `Future`" | forward (`M41::Future = M42`)    |

Reaching a chain end is a compile error (`M41::Source` doesn't compile) — the
type system **is** the chain-bound check, so the registry is reconstructable
from types alone.

### Cross-version read

Every struct gets `impl<Db> MigrationChain<Db>`. `read_as_self(db, bytes,
stored_version)`:

- `stored == Self::STRUCT_VERSION` → wire-decode directly.
- `stored < ...` → recurse as `Self::Source`, then `__wave_db_migrate_from`.
- `stored > ...` → recurse as `Self::Future`, then `__wave_db_migrate_rollback`.

Works **without** any `register_migration` call. The wire format prepends a
one-byte `STRUCT_VERSION` so the engine knows which way to walk.

### Compose & split

`first_try` covers both: **compose** synthesises a source from several records
before the search; **split** points several new families' `first_try`s at one
old record, each lifting its slice. Same pipeline, opposite ends.

### Rollback during mixed-version deployments

A node on an older build walks the registered **backward** edges to bring a
newer record down to a version it can read. Forward + backward must both exist
for any two adjacent versions to coexist live. Code-side rollback is a one-line
`pub type` edit; the DB keeps both readable while the `migrate_rollback_with`
fn is compiled in.

---

## Permissions

Access control is stored **inline in `Metadata`**, scoped per record:

| Value                                  | Semantics                                                         | Wire cost |
| -------------------------------------- | ----------------------------------------------------------------- | --------- |
| `None`                                 | Only the tenant's own users (the common B2C case).                | 1 byte    |
| `Some(PermissionRef::Inline(list))`    | Small inline ACL (user IDs); auto-promotes to a per-record B+ tree. | 1 + list  |
| `Some(PermissionRef::Group(group_id))` | Reference to a shared permission group (large tenants).           | 1 + ref   |

All checks are local to the tenant. Cross-tenant sharing is a separate problem
(P15 — reciprocal capability records).

---

## Query expressions

`Expr` / `Value` / `Field` form the typed query tree evaluated node-side over
descriptor offsets. `Value` covers every numeric width (`U8`…`U128`, `I8`…`I128`,
`F32`, `F64`) plus `Str`/`Bool`/`Bytes`; `From` impls are exact-width. The
macro generates one typed `Field` per column so a misspelt field is a compile
error, not an empty result.
