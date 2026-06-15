# wavedb-macros

The compile-time front door. `#[wave_db]` turns a plain Rust struct into a
WaveDB object; `declare_objects!` collects them into a registry every node
shares. All schema rules are written once and enforced on client and server
because both compile this crate.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Module map

| Module       | Responsibility                                                |
| ------------ | ------------------------------------------------------------- |
| `lib`        | The `#[wave_db]` and `declare_objects!` entry points.         |
| `args`       | Parse `#[wave_db(...)]` attribute arguments.                  |
| `validation` | Compile-time checks (struct_id ≤ u20, trailing version, …).   |
| `descriptor` | Emit `ObjectDescriptor` (field offsets, heapable flags, names). |
| `wire_derive`| `WaveWire` — the no-serde `Wire` impl.                        |
| `crud`       | Generated accessors / anchor finders / CRUD glue.            |
| `declare`    | `declare_objects!` registry codegen.                         |
| `codegen` / `utils` | Shared emit helpers.                                   |

---

## What `#[wave_db]` does at compile time

1. **Implements `Id` + `Metadata` accessors.** `.tenant_id()`, `.shard_id()`,
   `.struct_id()`, `.created_at()` plus full `Metadata` getters/setters — no
   call-site boilerplate.
2. **Pins a permanent `STRUCT_ID`** via `#[wave_db(struct_id = N)]`. Incremental
   by convention, **shared across every version** of a struct family
   (`Message1`, `Message42`, … all `struct_id = N`). Uniqueness is validated
   across the codebase; once assigned, never changes.
3. **Derives `struct_version` from the type name.** The trailing integer _is_
   the version: `Message42` ⇒ `struct_version = 42`. Validated to fit `u8`. No
   separate `version =` attribute.
4. **Re-exports a stable alias.** Code imports the unversioned name; one
   `pub type Message = Message42;` line declares the live version. Roll
   forward/back is a one-line edit — `struct_id` is stable so indexes and
   cross-references survive the change.
5. **Configures anchor addressing** (see _Anchor addressing_ below).
6. **Declares migrations inline** — see [`wavedb-core`](../wavedb-core/README.md#migrations)
   for the neighbour model, chain traits, and `first_try`/`fallback_not_found`.

```rust
#[wave_db(struct_id = 7, NonUnique)]
pub struct Message42 {
    pub body: String,
    pub author: u64,
}
pub type Message = Message42;
```

All `#[wave_db]` structs serialize through WaveDB's own `Wire` format (no
serde); migration fns are generic over `Db` so the macro's `__WaveDbDb`
parameter resolves at the call site.

---

## Data shapes

Declared as a bare marker in the attribute:

| Marker (none = Unique)       | Cardinality per tenant                       | Operations                             |
| ---------------------------- | -------------------------------------------- | -------------------------------------- |
| _(default)_ **Unique**       | Exactly one live record per `(STRUCT_ID, TENANT_ID)` | `read`, `"save"`, `create`             |
| `NonUnique`                  | Many live records per tenant                 | `read`, `"save"`, `create`, `"delete"` |
| `NestedNonUnique`            | Many records bound to a single parent        | `read`, `"save"`, `create`, `"delete"` |

---

## Anchor addressing

Two optional attributes change how a struct's anchor is hashed (storage-side
semantics live in [`wavedb-storage`](../wavedb-storage/README.md#anchor-slots)):

- `primary_anchor = field` — replaces the node-allocated `SHARD_ID` with
  `hash(field)`. Content-addressed lookup (1 IO), implicit uniqueness, routing
  locality. Emits `find_by_<field>`.
- `secondary_anchor = (field)` / `= (a, b)` — extra anchor addresses that point
  back to the primary; one generated `find_by_…` accessor each. Secondaries
  live in the primary's reference list (atomic delete, consistent mutation, no
  phantom aliases). 1 IO for the primary, 2 for a secondary.

```rust
#[wave_db(struct_id = 25, NonUnique,
          primary_anchor = username,
          secondary_anchor = (email),
          secondary_anchor = (department, employee_number))]
pub struct User1 { /* … */ }
```

Tune the array→B+ tree index threshold per struct: `btree_threshold = 100`.
Small `Iter<T>` fields can opt into `try_heap_inline` (heap-inline linked list,
no cross-pointer maintenance) instead of the default full-index `AsRef`.

---

## Validation & preprocessing hooks

Two attributes, run identically native and wasm32, dispatched through a static
monomorphised fn table (no `dyn`, no async in the WASM binary):

```rust
#[wave_db(struct_id = 12, NonUnique,
          validate = validate_payment, preprocess = preprocess_payment)]
pub struct Payment1 { pub amount_cents: u64, pub currency: String }
```

| Hook         | Client (pre-send)              | Quick-Node (pre-commit)                   | Purpose                       |
| ------------ | ------------------------------ | ----------------------------------------- | ----------------------------- |
| `validate`   | ✓ — typed error, zero round-trip | ✓ — the security boundary               | invariant checks              |
| `preprocess` | ✗                              | ✓ — re-encoded result is what's committed | normalisation, derived fields |

The node runs `validate` **before** `preprocess`. Hooks are synchronous and
pure; hook-less types skip decode entirely via compile-time `HAS_VALIDATE` /
`HAS_PREPROCESS` consts. Node-side enforcement (the 4 gates) is documented in
[`wavedb-quick-node`](../wavedb-quick-node/README.md#node-side-enforcement).

---

## `declare_objects!` — the registry

```rust
declare_objects! { pub mod app_objects { payments: [Payment1], … } }
```

Emits per-family modules, a `find(header)` const-compare chain (no `dyn`,
compile-time duplicate-header check), per-struct `DESCRIPTOR: &'static
ObjectDescriptor` (field offsets, heapable flags, heap-prop name list), and
`REGISTRY: &'static ObjectRegistry`. `WaveDbStruct::HEADER = struct_id << 8 |
version`. Attaching the registry is what turns a generic node into _your_
backend.
