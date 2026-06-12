# TO DO

- Falta de clareza sobre aquizição de dados por clients;

# DOING

# DONE

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
