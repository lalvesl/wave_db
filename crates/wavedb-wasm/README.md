# wavedb-wasm

The **browser client**. All code is gated on `target_arch = "wasm32"`; on native
targets the crate is empty (it exists so `cargo test --workspace` compiles). The
public `Db` API is identical to native — only the storage adapter and runtime
change shape.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Runtime swap

The WASM build replaces the Tokio runtime: futures run via
`wasm_bindgen_futures`, HTTP goes through the browser `fetch` API, WebSockets
through `gloo_net::websocket`, and persistence through IndexedDB.

## Storage: key→value, not pages

The native engine manages 4 KiB pages because it owns the physical disk layout.
In the browser it owns none of that — IndexedDB is already an ordered key→value
store (LevelDB under Chrome, SQLite under Firefox) doing its own page management.
Emulating WaveDB pages on top would stack two block managers and pay write
amplification for zero IO benefit. So the WASM build **skips the page layer
entirely**:

- **Key = the 128-bit `Id`** (big-endian). Anchors at their anchor key,
  versioned records at their full `Id`, heap-dedup entries at their content hash.
- **Value = the wire-encoded record**, compressed exactly as on native.
- Because `TENANT_ID` occupies the `Id`'s top bits, IndexedDB's ordered keyspace
  **clusters a tenant's records naturally** — a `getAll(range)` over a
  tenant/struct prefix is the browser equivalent of reading a hot page.
- The page-pressure machinery (heapable eviction, page-local block growth,
  directory rehash) simply does not run client-side. Quota is the browser's job;
  the local store is a write-through cache that can always be re-fetched from the
  cluster.

The engine layers above storage — anchors, versioning, migration chains, query
evaluation — are identical on both targets. Sync needs no page parity either:
the Bloom-filter protocol exchanges objects and anchors, never pages.
(`localStorage` — synchronous, string-only, ~5 MB — cannot fill this role.)
