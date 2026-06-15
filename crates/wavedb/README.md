# wavedb

The user-facing **client library** — the `Db` handle, typed object CRUD, and the
query surface. Same code path on servers, native clients, and (compiled to WASM)
browsers.

> For the project-wide idea see the [root README](../../readme.md). For the
> wire/transport layer see [`wavedb-net`](../wavedb-net/README.md); for object
> declaration see [`wavedb-macros`](../wavedb-macros/README.md).

## Entry points

| Mode                                        | Storage location               | Tenant model                |
| ------------------------------------------- | ------------------------------ | --------------------------- |
| `Db::open(url, path, user)`                 | Local file at `path`           | `tenant = user_id`          |
| `Db::open(url, path, user, default_tenant)` | Local file at `path`           | Tenant explicit (companies) |
| `Db::open(url, user, default_tenant)`       | Browser IndexedDB (WASM)       | Tenant explicit             |
| `Db::open(url, user)`                       | Browser IndexedDB (WASM)       | `tenant = user_id`          |

`url` resolves to the cluster's front door; the request is redirected to the
Quick-Node owning the user's tenant, and the **backup** Quick-Node URL is
returned alongside for failover. Native modes use the local `path` as a
write-through cache under `tokio::broadcast`; WASM modes use IndexedDB in the
same role. A `Db` can spawn another for a different tenant:

```rust
let other = db.another_tenant(other_tenant_id).await?;
```

The `Drop` impl notifies the Quick-Node so the session is released promptly.

## The partition key is structural

The tenant is bound **once, at connect** — it never appears in a query again.
Only the business filter is left:

```rust
use wavedb::prelude::*;

let db = Db::connect("wss://wavedb.example", /* user */ 42, /* tenant */ 42).await?;

// No WHERE user_id, no join — the engine reads only tenant 42's orders.
// `Order::amount` is a macro-generated typed column: a misspelt field is a
// compile error, not an empty result.
let orders: Vec<Order> = Order::query(&db, Order::amount.gt(100u64)).await?;
```

## Object lifecycle

Objects are never **created in isolation**. Every `create` is preceded by a
"does this exist?" check — if it does you `save`, if not the engine assigns a
fresh `Id` and `Metadata` and saves. Local code uses `Default::default()` for
both; the engine fills the real values at `save`/`send` time.

```rust
// Unique lookup — Option because the record may not exist yet.
let profile: Option<UserProfile> = UserProfile::search(&db).await?;

// NonUnique with a query (sea_orm-flavoured Expression).
let recent: Vec<Order> = Order::query(&db, Order::amount.gt(100)).await?;

// Update (versioned in place) and delete (NonUnique only).
order.save(&db).await?;
order.delete(&db).await?;
```

## Everything is `async`

Every public API on `Db`, every storage actor, every migration is `async` end to
end — no blocking IO surface, no hidden thread-pool dispatch. Native runs on
Tokio; the browser on `wasm_bindgen_futures`. The public API is identical.

## Unauthenticated sessions

A client without credentials connects with `user = U48::MAX`. The session sees
only **public data**; the API is restricted to login (password, Google, …) and
reading world-readable data.
