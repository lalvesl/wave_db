# WaveDB Application Platform — Roadmap

Goal: make WaveDB usable as the foundation of a real application — one schema
crate shared by browser/native clients and by the Quick-Node / Slow-Node
backends, "Next.js-style" code sharing where the DB **is** the server.

This document plans the path from the current state (engine + cluster
plumbing + research examples) to a developer-facing platform where:

```
my-app/
├── crates/
│   ├── app-schema/       # #[wave_db] structs + declare_objects! + migrations
│   │                     #   compiled into EVERY binary below
│   ├── app-server/       # server-side hooks (validation, jobs) — cfg(server)
│   ├── app-quick-node/   # fn main() { wavedb_quick_node::run(config, REGISTRY, hooks) }
│   ├── app-slow-node/    # fn main() { wavedb_slow_node::run(config, REGISTRY) }
│   ├── app-client/       # native client binary / library
│   └── app-web/          # wasm32 build, typed API over IndexedDB + WebSocket
```

One `cargo build` workspace, four artifacts, zero DTO layer: the schema crate
is the protocol.

---

## Current state (inventory, 2026-06)

| Layer           | Crate               | State                                                                                                                                                                                                         |
| --------------- | ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Wire + registry | `wavedb-core`       | **Done.** `Wire` trait, `ObjectDescriptor`, `declare_objects!`, migration chains (`MigratesFrom`/`RollbackFrom`).                                                                                             |
| Storage engine  | `wavedb-storage`    | **Substantial.** Pages, anchors, adaptive indexes, zstd + dict compression, journal/drain pipeline, `NodeStorage`.                                                                                            |
| Client handle   | `wavedb`            | **Native-only.** `Db::connect`, typed `find/query/save/delete`, `Expr` DSL. No local cache file, no wasm32 support.                                                                                           |
| Transport       | `wavedb-net`        | WS + HTTP frames, `RequestKind`, notifications scaffold, **node-to-node HMAC auth only**.                                                                                                                     |
| Quick-Node      | `wavedb-quick-node` | Binary with ownership/ring/gossip/replication; write path WAL-commits to `NodeStorage`; **`handle_search_unique` / `handle_query` are stubs** (`node.rs` "Phase 14"). Schema-blind: registry never linked in. |
| Slow-Node       | `wavedb-slow-node`  | Flush receiver + `HistoryStore` (in-memory `HashMap` index over journal). No query surface.                                                                                                                   |
| Browser         | `wavedb-wasm`       | Raw-bytes `JsDb` (u128 + `Vec<u8>`) + IndexedDB adapter + demo. Not the typed API.                                                                                                                            |
| Examples        | `wavedb-examples`   | `real_example` cluster demo (3 quick + 1 slow + 500 clients) — uses `#[wave_db]` structs client-side, nodes treat payloads as opaque bytes, `declare_objects!` unused.                                        |

### The structural gap

Everything below the network line treats records as **opaque bytes keyed by
Id**. The registry/descriptor system (commit `2067ad5`) exists precisely to
fix that, but nothing consumes it yet. Every milestone below is some form of
"wire the registry into X".

---

## Milestone M1 — Registry-aware nodes (the keystone)

> A Quick-Node that knows the schema can evaluate queries, enforce shapes,
> and maintain anchors. Without this, nothing else matters.

> **Landed early (2026-06-12), pulled out of M1:**
>
> - **M1.1 partially** — `wavedb_quick_node::Server` builder
>   (`Server::bind(…).registry(REGISTRY).serve()`), the library-mode
>   entrypoint; the generic `main.rs` binary still exists for schema-less
>   smoke tests.
> - **Ring-derived runtime ownership** — `Config.owns`/`--owns` deleted;
>   `QuickNode::owns()` computes ownership from the consistent-hash ring
>   (solo node owns everything), gossip moves membership, heartbeat
>   (1 s × 3 strikes) evicts crashed peers, ownership re-derives to
>   survivors. `OwnershipMap` remains as transfer-pin override only.
> - **Replica fan-out** — owner pushes committed bytes to the next
>   `MIN_REPLICAS - 1` ring nodes (`POST /replicate`, HMAC purpose
>   `Replicate`, ack-fed watermarks); replicas store verbatim, never accept
>   client writes. e2e: `e2e_ownership.rs` (solo-owns-all, single-writer
>   agreement, redirect, replica copy, crash takeover).

1. **Library-mode node entrypoints.** Split `main.rs` from node logic:
   - `wavedb_quick_node::run(config, &'static ObjectRegistry) -> Result<()>`
   - `wavedb_slow_node::run(config, &'static ObjectRegistry) -> Result<()>`
   - The shipped `main.rs` binaries keep working for schema-less smoke tests
     (current behaviour, opaque bytes) by passing an empty registry.
   - An app's node binary is then 5 lines: parse args, link `app-schema`'s
     `REGISTRY`, call `run`.
2. **Header-checked writes.** `handle_write` validates the `u32` record
   envelope against the registry: unknown `(struct_id, version)` → typed
   error response instead of silent opaque storage. Known structs route by
   `Shape` (Unique → anchor at `(struct_id, tenant, 0)`, NonUnique → minted
   shard, NestedNonUnique → parent address space).
3. **Implement the read stubs.** `handle_search_unique` resolves the unique
   anchor through `NodeStorage`; `handle_query` deserialises the `Expr`
   filter and evaluates it **against wire bytes via descriptor offsets** — no
   full deserialisation for stack-section predicates (e.g. `amount > 100`
   reads 8 bytes at a compile-time offset).
4. **Lazy migration server-side.** On read, compare stored version byte with
   the registry head; walk the migration chain; schedule background
   write-back (reuse the idle-work tier of the pipeline).

**Exit criteria:** `real_example` clients' `find`/`query` calls return real
data from `NodeStorage` through a registry-linked quick-node;
`e2e_durability` suite extended with a kill-during-query case.

---

## Milestone M2 — Typed client API end-to-end

> Native client apps become possible here.

1. **Server-evaluated `Expr` round-trip.** `Order::query(&db, Order::amount.gt(100))`
   serialises the typed-column expression (already done), node evaluates
   (M1.3), response carries wire records; client decodes through
   `MigrationChain::read_as_self` so mixed-version clusters work.
2. **Anchor accessors over the network.** `find_by_<field>` (primary-anchor
   hash) and secondary-anchor lookups become `RequestKind` variants; node
   resolves via anchor slots. This is the "1 IO point lookup" promise made
   in the readme — currently macro-generated but with no transport.
3. **NestedNonUnique CRUD path.** Parent-scoped create/query
   (`invoice.lines(&db)`), clustered under the parent's address space.
4. **Delete + tombstones** through the typed API, with tombstone-anchor reads
   distinguishing "deleted" from "never existed".

**Exit criteria:** `wavedb-examples` binaries (`nonunique_orders`,
`nested_invoice_lines`, `anchors`, `cross_references`) run against a live
node instead of in-process mocks.

---

## Milestone M3 — Browser target: typed `Db` on wasm32

> Client-side applications in the browser, same Rust API as native.

1. **`wavedb` + `wavedb-net` compile for `wasm32-unknown-unknown`.**
   `cfg(target_arch = "wasm32")` transport backends: `gloo_net::websocket`
   and `fetch` (deps already in workspace, unused). Tokio types isolated
   behind a small runtime-abstraction layer (`spawn`, `sleep`, channels —
   the wasm side uses `wasm_bindgen_futures`).
2. **IndexedDB as the local store** behind the same trait the native local
   cache uses (see M4): key = big-endian Id, value = wire bytes, no page
   emulation (per readme "Browser storage: key→value, not pages").
   `wavedb-wasm/src/idb.rs` already has the adapter core — promote it from
   demo to storage backend.
3. **`JsDb` becomes a thin veneer** over the typed `Db` for JS consumers;
   Rust wasm apps (Leptos/Yew/vanilla) use `wavedb::Db` directly with their
   schema crate.
4. **Size budget.** Extend `scripts/wasm_size.sh` gate: typed client + small
   schema ≤ a defined budget (current canonical baseline: 104,377 bytes
   `-Oz`). Registry statics grow linearly with schema size — measure
   per-struct cost and document it.

**Exit criteria:** a browser demo app (counter/todo over `app-schema`)
performs typed `save`/`query` against a local quick-node over WebSocket,
with IndexedDB caching reads.

---

## Milestone M4 — Local cache & the `Db::open` family

> The readme promises four entry-points; only `connect` exists.

1. **`LocalStore` trait** — `get/put/scan_prefix/delete` over wire bytes:
   - native: write-through file store (reuse `wavedb-storage` in
     single-file mode — the engine already runs embedded; this is the
     "Development, embedded" operation mode),
   - wasm: IndexedDB adapter (M3.2).
2. **`Db::open(url, path, user[, tenant])`** — reads hit the local store
   first, misses fetch from the owner node and back-fill; writes go to the
   owner and write-through locally on ack.
3. **Read-your-writes consistency** between the local store and
   notifications: a notification for an Id newer than the local copy
   invalidates/overwrites it.

**Exit criteria:** client survives node restart with warm local reads;
disconnect → reads still served from cache (writes fail fast, as designed —
offline-first stays deferred per P10).

---

## Milestone M5 — Live sync: notifications → application events

> What makes client apps feel "connected to the DB".

1. **Server push pipeline.** Owner node fans out anchor mutations to
   subscribed sessions (the `notify.rs` scaffold + `tokio::broadcast` from
   the write pipeline). WS: push frames. HTTP: piggyback on the
   single-queue responses + `http_poll_interval` idle ticks (readme design,
   not yet implemented).
2. **Bloom-filter screen-sync.** Client maintains a `fastbloom` filter of
   on-screen Ids (dep already in workspace), publishes on change; node
   filters its mutation stream against subscriber filters.
3. **Client event API.** `db.subscribe() -> impl Stream<Event>` where
   `Event = { Changed(Id), Tombstoned(Id) }`; typed convenience
   `Order::watch(&db, expr)` re-runs the query on relevant events.

**Exit criteria:** two clients on the same tenant; client A saves an order;
client B's watcher fires within one round-trip (WS) / one poll tick (HTTP).

---

## Milestone M6 — End-user auth & permission enforcement

> Cannot ship a backend that trusts the client's `user` field.

1. **Session authentication.** Today `Connect { user, tenant }` is taken on
   faith. Add a login `RequestKind`: credential → session token (HMAC,
   reusing `auth.rs` machinery with a new `TokenPurpose::Session`); every
   subsequent request carries it; node derives `user`/`tenant` from the
   token, never from the request body.
2. **Unauthenticated tier.** `user = U48::MAX` sessions restricted to login
   - world-readable reads (readme contract).
3. **Permission checks on the node.** Enforce `Metadata.permission`
   (`None` → tenant-internal; `Inline` ACL walk; `Group` lookup) on every
   read/write/delete. Client-side checks are UX, node-side checks are the
   security boundary.
4. **Server-side hooks (the "backend" in backend).** ✅ **Landed early
   (2026-06-11), ahead of the rest of M6.** Shipped as `#[wave_db(validate
= fn, preprocess = fn)]` + `WaveDbHooks` + registry-dispatched
   `validate(header, body)` / `preprocess(header, body)` +
   `QuickNode::with_registry`. Design deviation from the original sketch:
   hooks are **typed** (`fn(&Self)` / `fn(&mut Self)`, sync, pure) and the
   registry decodes through each type's `Wire` impl — not bytes+descriptor.
   The `declare_objects!` compare-chain makes typed dispatch free
   (one monomorphised arm per declared version, no `dyn`), and writing
   hooks against the real struct beats poking descriptor offsets. Rejection
   travels as structured `NodeError` on `TransportResponse`; the client
   maps it back to the same `Error::Validation` the local pre-send check
   raises. Async/DB-access hooks remain future work (separate attribute
   family). Remaining M6 scope (sessions, permission enforcement) unchanged.

**Exit criteria:** cross-tenant read attempt rejected at the node; hook
rejecting an invalid write surfaces as a typed client error; threat-model
note added to docs (what is and isn't enforced server-side).

---

## Milestone M7 — Slow-Node as a real history backend

1. **Persistent index.** Replace `HistoryStore`'s in-memory `HashMap` with
   the storage engine in "History Only" operation mode.
2. **History read path.** `RequestKind::History { id, range }` — version
   chain walks that fall off the Quick-Node's hot window route down to the
   Slow-Node (readme traversal semantics: anchor → `current_version_at` →
   `old_modification_id` chain).
3. **Registry-aware flush.** Heap entries travel in block format with owner
   lists; Slow-Node validates headers like M1.2.

**Exit criteria:** `re_monitor` shows history queries served by the
slow-node for versions evicted from quick-node storage.

---

## Milestone M8 — Developer experience & packaging

1. **`wavedb-app-template`** (cargo-generate): the workspace skeleton from
   the top of this document, with one example struct per shape, a migration
   pair (v1→v2), node binaries, native client, wasm client, `flake.nix`
   wiring, `nix run .#dev-cluster` for a 1-quick/1-slow local cluster.
2. **Restructure `real_example`** into the template shape — it becomes the
   living proof the template works under load.
3. **Docs:** "Building an app on WaveDB" guide (schema crate → binaries →
   deploy), wire-format/registry reference (exists), migration cookbook
   (compose/split via `first_try` — examples exist as bins, need prose).
4. **Versioning policy** for the platform crates (`wavedb`, node `run()`
   APIs) — apps now depend on them as libraries, breakage cost is real.

**Exit criteria:** `cargo generate … && nix run .#dev-cluster && cargo run
--bin app-client` works on a clean machine; template CI builds native +
wasm32 + both node binaries from the shared schema crate.

---

## Sequencing & dependencies

```
M1 (registry-aware nodes) ──► M2 (typed E2E) ──► M5 (live sync)
        │                           │
        │                           └──► M6 (auth/permissions/hooks)
        ├──► M7 (slow-node history)
        └──► M3 (wasm client) ──► M4 (local cache: native+wasm share trait)
                                          M8 (template) — after M2+M3, polish last
```

- **M1 is the keystone** — it converts the registry from a data structure
  into the platform's spine. Do it first, alone.
- M3/M4 can proceed in parallel with M5/M6 after M2 (different crates,
  different reviewers).
- M6.4 (hooks) is deliberately late: hook API design gets better after M2
  shows what node-side code actually needs.

## Risks / open questions

| Risk                                                                                       | Mitigation                                                                                                                                                                                 |
| ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Registry statics bloat the wasm binary as schemas grow                                     | M3.4 measures per-struct cost early; descriptors are `'static` data (no code), dictionary-compressible by wasm-opt — verify, don't assume.                                                 |
| `Expr` evaluation against wire bytes needs heap-field comparisons (e.g. `String` equality) | Stack-only predicates first (covers `amount > 100` class); heap predicates decode just the one field via descriptor offset + heap walk. Phase the work.                                    |
| Runtime abstraction (tokio vs wasm) leaks into public API                                  | Keep it internal to `wavedb`/`wavedb-net`; public API stays `async fn` — already the contract ("Everything is async").                                                                     |
| Hook signature (`&[u8]` + descriptor vs typed dispatch)                                    | **Resolved (2026-06-11):** typed dispatch via the `declare_objects!` compare chain — one monomorphised arm per declared version, `HAS_*` consts skip decode for hook-less types. See M6.4. |
| P15 (cross-tenant sharing) intersects M6                                                   | Out of scope here; M6 enforces the tenant-local model only, capability records stay a research item.                                                                                       |
