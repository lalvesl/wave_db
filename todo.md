# TO DO

- Client-side `not_owner` redirect: `Db::send` should re-dial the
  `owner_url` hint instead of surfacing the redirect payload (today the
  caller must connect at the owner; see `open_user_at_owner` in e2e tests);
- Update-in-place semantics: a `save` with a non-zero `id` should rotate
  that record's version instead of minting a new element (today every save
  is a create — the live tracker grows per save for NonUnique shapes);
- Prune flushed history from the hot tier after `sync_to_slow` acks
  (records currently stay in `data.bin`; flush ships copies, doesn't
  release space yet);

# DOING

# DONE

- Oh fix the wavedb monitor gui;
  → Root cause of "GUI won't run": `cargo run -p wavedb-monitor-gui` outside
  `nix develop` — off-pin toolchain broke `ring v0.17.14` ("unresolved module
  or unlinked crate `featureflags`") and the shell had no GUI libs
  (`NoWaylandLib`). Fixes: wave_db dev shell now ships the eframe runtime
  libs (wayland/libxkbcommon/libGL/X11 + `LD_LIBRARY_PATH`), and `ring` is
  **gone from the dep graph entirely** — egui_shadcn's font-downloader
  build-dep (`ureq`) switched rustls→native-tls. Chart fixes in egui_charts:
  `annular_sector` filled concave ring outlines with `convex_polygon` (the
  gauge "velocimeter" drew as a pacman disk; pie/sunburst/polar_bar shared
  the bug) → now a triangle-strip mesh + outline; `GraphNode` gained
  `hide_label()` / `group(n)` (Data tab: 150 labeled rainbow dots → labeled
  family hubs with unlabeled color-grouped records); `HeatmapSeries::sqrt_scale()`;
  explicit `Axis::value().min(0)` bounds now win over nice-rounding (idle
  throughput chart no longer spans [-1,1]). Monitor data fixes: browse
  summaries derive struct family from the **Id bits** + version from payload
  byte 0 (Phase-14 `[version][body]` flushes all showed family "?");
  `page_map` is scaled relative to the busiest slot with a floor of 1
  (fixed 2 MiB/slot threshold truncated light traffic to all-zeros = blank
  heat map) and `apply_replica` now feeds the slots (replica-only nodes
  showed an empty map over a multi-MB data.bin). GUI polish: overview
  charts fill the window height, topology colors by tier, invalid
  `--cluster-key` exits with a clear message instead of a panic.
  450 tests wave_db + egui_shadcn suite green, clippy 0 both repos.

- Falta de clareza sobre aquizição de dados por clients;
  → Landed as Phase 14, the storage-backed read path. The response
  contracts the client already documented are now served for real:
  `SearchUnique` → `[stored_version][wire body]` (empty = not found);
  `QueryNonUnique` → wire `Vec<(stored_version, body)>`; `Delete` →
  tombstone (live-tracker removal; the versioned record stays as history).
  Pieces: per-`(STRUCT_ID, TENANT_ID)` **live tracker** in the page file
  (chained head + sealed segments of 256 ids, `SHARD_ID 0xFFF` reserved,
  crash recovery rebuilds via journal replay union); `Page::upsert` +
  in-place compaction (hash-page rewrites used to append duplicate
  directory entries and leak the page); `Expr`/`Value`/`Field` moved to
  `wavedb_core::query` with a node-side `eval` over descriptor offsets
  (numeric widening across widths, string/bytes via forward+backward heap
  walks around opaque heapables like the injected `metadata`); registry
  nodes stamp the engine-assigned `Id` into the stored body so query
  results are deletable; Unique shapes rotate a single live entry, and
  schema-blind nodes serve `Expr::All` but answer filtered queries with
  the new `ErrorCode::Unsupported`. `sync_to_slow` is real: per-tenant
  pending buffer → `FlushBatch` POSTs (HMAC `Flush` tokens, ≤512
  records/batch, failed batches re-queued in order) on a 5 s loop
  (`start_flush_loop`, wired in `Server::start` + the binary) and on
  drain. 463 tests across 64 suites, incl. `e2e_read_path.rs` driving the
  full client loop (`save` → typed/filtered `query` → `delete` by stamped
  id → flush lands in the Slow-Node store).

- Validation of data from client-side and preprocessing from backend;
  → Landed as: `#[wave_db(validate = fn, preprocess = fn)]` data hooks
  (sync, pure: `fn(&Self)` / `fn(&mut Self) -> Result<(), ValidationError>`).
  `validate` runs on the client in `do_write` (typed `Error::Validation`,
  zero round-trip) AND on the Quick-Node before the WAL commit (security
  boundary); `preprocess` runs node-only after validate — the re-encoded
  result is what gets committed (proven by journal read-back in
  `e2e_hooks.rs`). Plumbing: `WaveDbHooks` trait + `HAS_*` consts (hook-less
  types skip decode entirely), `declare_objects!` emits `validate(header,
body)` / `preprocess(header, body)` compare-chains + `REGISTRY: &'static
ObjectRegistry`, `QuickNode::with_registry(config, REGISTRY)` attaches the
  schema (4 gates: header declared → decodes → validate → preprocess),
  structured `NodeError {code, struct_id, field, message}` on
  `TransportResponse.error` replaces stringly `b"storage_error"` payloads,
  client maps it back to the same typed error in `Db::send`. New
  `rejected_count` node metric. `ClusterSpec.registry` spawns schema-aware
  test clusters. 409 tests green incl. 5-test e2e (client reject / node
  reject on bypass / unknown header / malformed / preprocess persisted).

- I need to remove serde and postcard dependencies, i need to own procedure-macros, the objective is reduce size of wasm, in procedure macro create methods to get size_of at compile time and add with a method to get size of heap data, to request allocation of memory only once, the data is exacly the memory for stack elements and for dynamic use u32 to determinate size and in the sequence the heap data, to parse data the object need to be knowed by bolf parts, think this when i create objects with macro of wave_db create space of declaration of all objects, this are exposed by all nodes and can searchable by header u32(u24 of struct_id and u8 with the version of data), the implementation use another procedure macro to generate code for each version and expose a module for specific struct_id to need declared all to start quick,slow and client nodes, with this method is extreme more easy to access heap properties(such as a current list of names of heap props), know what properties and how to organize data for Anchors indexes, NonUnique and also NestedNonUnique, and also reduce usage of dyn traits because all cases are compiled statically, yes in the future there is possible to share cfg conde between clients, quick and slow nodes like nextjs but not only client/server because the DB are server also;
  → Landed as: `wavedb_core::Wire` trait (`STACK_SIZE` const + `heap_size()`, single
  `Vec::with_capacity(STACK_SIZE + heap_size)` allocation, u32 length slots, see
  `docs/wire_format.md`); `#[derive(WaveWire)]` + Wire impl emitted directly by
  `#[wave_db]`; `WaveDbStruct::HEADER = struct_id << 8 | version`; per-struct
  `DESCRIPTOR: &'static ObjectDescriptor` (field offsets, heapable flags, heap-prop
  name list); `declare_objects!` registry macro (per-family modules, `find(header)`
  as const-compare chain — no dyn, compile-time duplicate-header check).

- Remove serde,postcard crates from this repository, create own implementation of serde
  → serde/postcard removed from every crate and the workspace dependency list;
  gloo-net `json` default feature disabled. wasm32 dependency graph is 100%
  serde/postcard-free (only wavedb-bench keeps serde_json for its native JSONL
  perf recorder). Canonical size (nix, wasm-opt -Oz, rustc 1.96): 95,494 → 104,377
  raw bytes — net +8.9KB because the same change set added the registry/descriptor
  statics, the 15-variant `Value`, and the exported `ExampleReport` class; the
  serde codegen itself was already mostly LTO-stripped in the old binary.

- In query there's an implementation of enum @crates/wavedb/src/query.rs#L39-53 to describe data to quering, add all types of number f|u|i/8|16|32|64|128;
  → `Value` now has U8…U128, I8…I128, F32, F64 (+Str/Bool/Bytes); `From` impls are
  exact-width (`42u16` → `Value::U16`), usize/isize normalise to 64-bit.

- read the @readme.md and undestand this project. There is a problem with expressions, i need to write the name of column in str, i want to replace this with enum os each column. Take as much time as you need!
- The description on @readme.md#L17-28 is not describe correcly this project, read again the @readme.md and describe the problems of common sql, mixing data of all users, and mixing data of elements not reletead (the NestedNonUnique) when data are storege and searcheable only with interested data, reducing cache and diskIOps;
