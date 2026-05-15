# Building WaveDB — Step-by-Step Implementation Guide

This document is a phased plan for building WaveDB from an empty repository (with only the design `README.md` in place) to a working, tested, benchmarked, multi-node distributed database. Every phase produces a tangible, testable milestone — you can stop at the end of any phase and have something that compiles, passes tests, and demonstrates a concrete subset of the design.

The plan assumes the design described in `README.md`: 128-bit composite IDs, anchor slots, per-`(STRUCT_ID, TENANT_ID)` indexes, Quick-Node / Slow-Node tiering, ownership routing, HTTP+WebSocket transports, Type-1 and Type-2 migrations, and the four client operation modes.

---

## Table of Contents

1. [Prerequisites](#prerequisites)
2. [Workspace Layout](#workspace-layout)
3. [Phase 0 — Workspace setup](#phase-0--workspace-setup)
4. [Phase 1 — Core primitives (`wavedb-core`)](#phase-1--core-primitives-wavedb-core)
5. [Phase 2 — Proc-macros (`wavedb-macros`)](#phase-2--proc-macros-wavedb-macros)
6. [Phase 3 — Storage engine, single-file mode (`wavedb-storage`)](#phase-3--storage-engine-single-file-mode-wavedb-storage)
7. [Phase 4 — Indexes (adaptive array → B+ tree)](#phase-4--indexes-adaptive-array--b-tree)
8. [Phase 5 — Compression (zstd + per-STRUCT dictionaries)](#phase-5--compression-zstd--per-struct-dictionaries)
9. [Phase 6 — Heap data strategy](#phase-6--heap-data-strategy)
10. [Phase 7 — Journal & write pipeline](#phase-7--journal--write-pipeline)
11. [Phase 8 — Migrations (Type 1, Type 2, chains, rollback)](#phase-8--migrations-type-1-type-2-chains-rollback)
12. [Phase 9 — Permissions](#phase-9--permissions)
13. [Phase 10 — Client API (`wavedb`)](#phase-10--client-api-wavedb)
14. [Phase 11 — Network transport (`wavedb-net`)](#phase-11--network-transport-wavedb-net)
15. [Phase 12 — Quick-Node binary (`wavedb-quick-node`)](#phase-12--quick-node-binary-wavedb-quick-node)
16. [Phase 13 — Slow-Node binary (`wavedb-slow-node`)](#phase-13--slow-node-binary-wavedb-slow-node)
17. [Phase 14 — Distributed E2E test harness (user + quick + slow)](#phase-14--distributed-e2e-test-harness-user--quick--slow)
18. [Phase 15 — WASM client build (`wavedb-wasm`)](#phase-15--wasm-client-build-wavedb-wasm)
19. [Phase 16 — Examples](#phase-16--examples)
20. [Phase 17 — Benchmarks (criterion)](#phase-17--benchmarks-criterion)
21. [Phase 18 — Documentation](#phase-18--documentation)
22. [Continuous Integration](#continuous-integration)
23. [Troubleshooting & Common Pitfalls](#troubleshooting--common-pitfalls)

---

## Prerequisites

- **Rust** — latest stable via `rustup`. The workspace targets `edition = "2024"`.
- **Components** — `rustup component add rustfmt clippy rust-src`
- **WASM toolchain** (for Phase 15) — `cargo install wasm-pack` and `rustup target add wasm32-unknown-unknown`
- **System libs** — a working `cc` toolchain (for `zstd-sys` and `tokio-rustls`)
- **Optional** — `cargo install cargo-nextest` (faster test runner), `cargo install cargo-llvm-cov` (coverage), `cargo install cargo-deny` (license/security audits)

Verify with:

```sh
rustc --version          # should report current stable
cargo --version
cargo fmt --version
cargo clippy --version
```

---

## Workspace Layout

```
wavedb/
├── Cargo.toml                       # workspace manifest, shared lints, profiles
├── rust-toolchain.toml              # pins the toolchain for reproducibility
├── deny.toml                        # cargo-deny config
├── README.md                        # design doc (already present)
├── IMPLEMENTATION.md                # this file
├── LICENSE
├── .github/workflows/ci.yml         # GitHub Actions
├── crates/
│   ├── wavedb/                      # umbrella crate, re-exports the public API
│   ├── wavedb-core/                 # Id, Metadata, PermissionRef, error types
│   ├── wavedb-macros/               # proc-macros (#[wave_db])
│   ├── wavedb-storage/              # storage engine: pages, files, indexes
│   ├── wavedb-net/                  # HTTP + WebSocket transports
│   ├── wavedb-quick-node/           # Quick-Node binary
│   ├── wavedb-slow-node/            # Slow-Node binary
│   ├── wavedb-wasm/                 # browser build, gloo_net + localStorage
│   └── wavedb-test-cluster/         # in-process cluster harness for E2E tests
├── examples/
│   ├── unique_user_profile/         # Unique data lifecycle from all 3 roles
│   ├── nonunique_orders/            # NonUnique with anchor + index
│   ├── nested_invoice_lines/        # NonUnique-within-NonUnique
│   ├── migration_type_1/            # Same struct, version up/down
│   ├── migration_type_2/            # Compose new from old
│   └── distributed_cluster/         # Run user + 2 quick + 1 slow
├── tests/                           # workspace-level integration & E2E tests
│   ├── e2e_failover.rs
│   ├── e2e_replication.rs
│   ├── e2e_cold_handoff.rs
│   └── e2e_migration_rollout.rs
└── benches/                         # workspace-level benchmark suites
    ├── storage.rs
    ├── index.rs
    ├── compression.rs
    └── network.rs
```

The split is ergonomic but also _deliberate_: each crate has one job and one set of tests. Storage doesn't depend on `tokio`; networking doesn't depend on `zstd`. The proc-macro crate is its own beast (`proc-macro = true`). The umbrella `wavedb` crate exists so end users can write `use wavedb::prelude::*;` without picking sub-crates.

---

## Phase 0 — Workspace setup

**Goal:** A workspace that compiles to nothing, has shared lints, a CI scaffold, and `cargo fmt && cargo clippy && cargo test` all succeed (vacuously).

### Steps

```sh
git init
cargo new --vcs none --lib crates/wavedb
cargo new --vcs none --lib crates/wavedb-core
cargo new --vcs none --lib crates/wavedb-macros
cargo new --vcs none --lib crates/wavedb-storage
cargo new --vcs none --lib crates/wavedb-net
cargo new --vcs none --bin crates/wavedb-quick-node
cargo new --vcs none --bin crates/wavedb-slow-node
cargo new --vcs none --lib crates/wavedb-wasm
cargo new --vcs none --lib crates/wavedb-test-cluster
```

### Workspace `Cargo.toml`

```toml
[workspace]
resolver = "2"
members  = ["crates/*", "examples/*"]

[workspace.package]
edition       = "2024"
rust-version  = "1.85"           # or whatever current stable is
license       = "TBD"
repository    = "https://github.com/your-org/wavedb"

[workspace.lints.rust]
unsafe_op_in_unsafe_fn = "deny"
missing_docs           = "warn"   # we'll bump to "deny" before release

[workspace.lints.clippy]
pedantic        = { level = "warn", priority = -1 }
nursery         = { level = "warn", priority = -1 }
module_name_repetitions = "allow"
must_use_candidate      = "allow"

[workspace.dependencies]
# Shared versions; sub-crates pick what they need.
serde         = { version = "1", features = ["derive"] }
postcard      = { version = "1", features = ["use-std"] }
thiserror     = "1"
tokio         = { version = "1", features = ["full"] }
zstd          = "0.13"
byteorder     = "1"
parking_lot   = "0.12"
proptest      = "1"
criterion     = { version = "0.5", features = ["async_tokio"] }
trybuild      = "1"
tempfile      = "3"

[profile.release]
lto           = "fat"
codegen-units = 1
debug         = "line-tables-only"

[profile.bench]
inherits = "release"
```

### `rust-toolchain.toml`

```toml
[toolchain]
channel    = "stable"
components = ["rustfmt", "clippy", "rust-src"]
targets    = ["wasm32-unknown-unknown"]
```

### Acceptance

```sh
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test  --workspace
```

All three commands succeed on an empty workspace. Commit.

---

## Phase 1 — Core primitives (`wavedb-core`)

**Goal:** The 128-bit `Id`, the `Metadata` struct, `PermissionRef`, and a workspace-wide `Error` type. No I/O, no async — pure data, fully unit-tested.

### `crates/wavedb-core/Cargo.toml`

```toml
[package]
name        = "wavedb-core"
version     = "0.1.0"
edition.workspace     = true
rust-version.workspace = true

[dependencies]
serde.workspace     = true
postcard.workspace  = true
thiserror.workspace = true
byteorder.workspace = true

[dev-dependencies]
proptest.workspace  = true
```

### Public surface

```rust
//! Core primitives for WaveDB: composite IDs, metadata, permission refs,
//! and the workspace error type.
//!
//! These types contain no I/O and are safe to use in WASM, in `no_std`
//! contexts where postcard is available, and in proc-macro generated code.

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

mod error;
mod id;
mod metadata;
mod permission;

pub use error::{Error, Result};
pub use id::{Id, Shape};       // Shape = Unique | NonUnique | NestedNonUnique
pub use metadata::Metadata;
pub use permission::{PermissionRef, PermissionGroupId};
```

### `Id`

````rust
/// 128-bit composite ID:
///
/// ```text
/// [ TENANT_ID (u48) | SHARD_ID (u12) | STRUCT_ID (u20) | CREATED_AT (u48) ]
/// ```
///
/// Pack/unpack accessors are `const` so the macro-generated code can build
/// IDs at compile time when needed.
#[repr(transparent)]
#[derive(Copy, Clone, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Id(u128);

impl Id {
    pub const SYSTEM_TENANT:        u64 = 0;
    pub const UNAUTHENTICATED_USER: u64 = (1u64 << 48) - 1; // U48::MAX

    pub const fn new(tenant: u64, shard: u16, struct_id: u32, created_at: u64) -> Self {
        // assert each fits its field
        let v = ((tenant     as u128 & ((1u128 << 48) - 1)) << 80)
              | ((shard      as u128 & ((1u128 << 12) - 1)) << 68)
              | ((struct_id  as u128 & ((1u128 << 20) - 1)) << 48)
              | (created_at  as u128 & ((1u128 << 48) - 1));
        Self(v)
    }

    pub const fn tenant_id(self) -> u64    { ((self.0 >> 80) & ((1u128 << 48) - 1)) as u64 }
    pub const fn shard_id(self)  -> u16    { ((self.0 >> 68) & ((1u128 << 12) - 1)) as u16 }
    pub const fn struct_id(self) -> u32    { ((self.0 >> 48) & ((1u128 << 20) - 1)) as u32 }
    pub const fn created_at(self) -> u64   { (self.0 & ((1u128 << 48) - 1)) as u64 }

    pub const fn raw(self) -> u128 { self.0 }
}
````

### `Metadata`

```rust
/// Per-record metadata. Lives next to the object's stack data on every page.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Metadata {
    pub old_modification_id: u128,
    pub new_modification_id: u128,
    pub struct_version:      u8,
    pub user:                u64,   // u48 in spirit; u64 in storage type
    pub device_created:      u64,
    pub permission:          Option<PermissionRef>,
}

impl Default for Metadata { /* zeros, struct_version = 0 */ }
```

### Unit & property tests

Add `crates/wavedb-core/tests/id.rs`:

```rust
use wavedb_core::Id;
use proptest::prelude::*;

proptest! {
    #[test]
    fn id_roundtrip(t in 0u64..(1<<48), s in 0u16..(1<<12),
                    sid in 0u32..(1<<20), c in 0u64..(1<<48)) {
        let id = Id::new(t, s, sid, c);
        prop_assert_eq!(id.tenant_id(), t);
        prop_assert_eq!(id.shard_id(),  s);
        prop_assert_eq!(id.struct_id(), sid);
        prop_assert_eq!(id.created_at(), c);
    }

    #[test]
    fn id_truncates_overflow(t in any::<u64>(), s in any::<u16>(),
                             sid in any::<u32>(), c in any::<u64>()) {
        let id = Id::new(t, s, sid, c);
        prop_assert!(id.tenant_id()  < (1<<48));
        prop_assert!(id.shard_id()   < (1<<12));
        prop_assert!(id.struct_id()  < (1<<20));
        prop_assert!(id.created_at() < (1<<48));
    }
}
```

A second test file `metadata.rs` round-trips `Metadata` through postcard and asserts that `permission: None` serialises in **1 byte**.

### Acceptance

```sh
cargo test  -p wavedb-core
cargo doc   -p wavedb-core --no-deps
cargo clippy -p wavedb-core -- -D warnings
```

Property tests pass at the default 256 cases. `cargo doc` produces clean output with no missing-docs warnings on public items.

---

## Phase 2 — Proc-macros (`wavedb-macros`)

**Goal:** A `#[wave_db(struct_id = N, …)]` attribute macro that:

1. Validates `struct_id` fits in `u20` and is unique across the codebase (per-compilation-unit best-effort, plus a build script lint).
2. Parses the trailing integer of the struct identifier as `struct_version` (`u8`).
3. Emits trait impls so `MyStruct1` carries `STRUCT_ID = N` and `STRUCT_VERSION = 1`.
4. Accepts attribute flags: `NonUnique`, `NestedNonUnique`, `try_heap_inline`, `btree_threshold = K`, `primary_anchor(field, …)`, `secondary_anchor(field, …)` (repeatable). The two anchor attributes drive the addressing variants from the _Anchor Slots_ section of the README — the macro emits the `find_by_<fields>` accessors and a compile-time anchor-layout descriptor that the storage engine consults at run time.

### `crates/wavedb-macros/Cargo.toml`

```toml
[package]
name    = "wavedb-macros"
version = "0.1.0"
edition.workspace = true

[lib]
proc-macro = true

[dependencies]
proc-macro2 = "1"
quote       = "1"
syn         = { version = "2", features = ["full"] }
darling     = "0.20"   # ergonomic attribute parsing

[dev-dependencies]
trybuild  = "1"
wavedb-core = { path = "../wavedb-core" }
```

### Skeleton

```rust
use darling::FromMeta;
use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemStruct, AttributeArgs};

/// One declared anchor (primary or secondary) — a list of field idents
/// whose values are concatenated and hashed.
#[derive(Debug, FromMeta)]
struct AnchorSpec {
    fields: Vec<syn::Ident>,
}

#[derive(Debug, FromMeta)]
struct WaveDbArgs {
    struct_id: u32,
    #[darling(default)] non_unique:        bool,
    #[darling(default)] nested_non_unique: bool,
    #[darling(default)] try_heap_inline:   bool,
    #[darling(default)] btree_threshold:   Option<u32>,
    #[darling(default)] primary_anchor:    Option<AnchorSpec>,
    #[darling(default, multiple)] secondary_anchor: Vec<AnchorSpec>,
}

#[proc_macro_attribute]
pub fn wave_db(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args  = parse_macro_input!(attr as AttributeArgs);
    let input = parse_macro_input!(item as ItemStruct);
    let args  = match WaveDbArgs::from_list(&args) {
        Ok(a)  => a,
        Err(e) => return e.write_errors().into(),
    };

    if args.struct_id >= (1 << 20) {
        return syn::Error::new_spanned(&input, "struct_id must fit in u20")
            .to_compile_error().into();
    }

    let name    = &input.ident;
    let version = parse_trailing_version(&name.to_string()); // -> u8
    let sid     = args.struct_id;
    let shape   = match (args.non_unique, args.nested_non_unique) {
        (false, false) => quote! { ::wavedb_core::Shape::Unique },
        (true,  false) => quote! { ::wavedb_core::Shape::NonUnique },
        (_,     true)  => quote! { ::wavedb_core::Shape::NestedNonUnique },
    };

    let primary_accessor   = args.primary_anchor.as_ref()
        .map(|spec| emit_find_by(name, spec, AnchorRole::Primary));
    let secondary_accessors = args.secondary_anchor.iter()
        .map(|spec| emit_find_by(name, spec, AnchorRole::Secondary));

    quote! {
        #input

        impl ::wavedb_core::WaveDbStruct for #name {
            const STRUCT_ID:      u32 = #sid;
            const STRUCT_VERSION: u8  = #version;
            const SHAPE: ::wavedb_core::Shape = #shape;
        }

        impl #name {
            #primary_accessor
            #(#secondary_accessors)*
        }
    }.into()
}

fn parse_trailing_version(name: &str) -> u8 { /* digits from end, fit in u8 */ }
```

### Tests with `trybuild`

Compile-pass and compile-fail cases live in `crates/wavedb-macros/tests/ui/`:

```
ui/
├── ok_minimal.rs                   # #[wave_db(struct_id = 1)] pub struct Foo1 { … }
├── ok_nonunique.rs
├── ok_nested.rs
├── ok_primary_anchor.rs            # primary_anchor(email_address) — struct has the field
├── ok_secondary_anchors.rs         # multiple secondary_anchor declarations
├── ok_property_hashed_combo.rs     # primary_anchor + secondary_anchor on the same struct
├── err_no_struct_id.rs             # missing struct_id → compile-fail
├── err_struct_id_too_big.rs
├── err_no_trailing_version.rs      # struct named "Foo" with no digits
├── err_version_overflow.rs         # struct named "Foo300" → > u8
├── err_anchor_unknown_field.rs     # primary_anchor(no_such_field) → compile-fail
└── err_duplicate_secondary.rs      # two identical secondary_anchor specs → compile-fail
```

```rust
// crates/wavedb-macros/tests/compile.rs
#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass     ("tests/ui/ok_*.rs");
    t.compile_fail("tests/ui/err_*.rs");
}
```

For expansion verification, optionally add `macrotest` for snapshot tests of generated code.

### Acceptance

```sh
cargo test -p wavedb-macros
```

UI tests all pass. The macro is now usable in downstream crates.

---

## Phase 3 — Storage engine, single-file mode (`wavedb-storage`)

**Goal:** Read/write anchor slots and versioned records to a single file, in memory + journal, with crash recovery on startup. No async, no networking — the storage engine is sync and gets wrapped in a tokio actor in Phase 7.

### Cargo deps

```toml
[dependencies]
wavedb-core    = { path = "../wavedb-core" }
serde          = { workspace = true }
postcard       = { workspace = true }
byteorder      = { workspace = true }
parking_lot    = { workspace = true }
thiserror      = { workspace = true }
zstd           = { workspace = true }
crc32fast      = "1"

[dev-dependencies]
proptest       = { workspace = true }
tempfile       = { workspace = true }
```

### Module layout

```
src/
├── lib.rs
├── page.rs          # Page header, directory, layout
├── file/
│   ├── mod.rs
│   ├── data.rs      # the data file
│   ├── index.rs     # the index file
│   ├── heap.rs      # the heap file
│   └── journal.rs   # append-only journal
├── anchor.rs        # Anchor slot encode/decode + tombstone
├── versioned.rs     # Versioned record encode/decode
├── hash.rs          # (STRUCT_ID, TENANT_ID, SHARD_ID) -> page
├── cache.rs         # in-memory write/read cache
└── error.rs
```

### Key types

The anchor model has to support both addressing variants from the README — the synthetic `(STRUCT_ID, TENANT_ID, SHARD_ID)` key _and_ the property-hashed `(STRUCT_ID, TENANT_ID, SHARD_ID, hash48)` key — plus secondary anchors that redirect to a primary. The storage engine doesn't care which variant is in use; it only sees a 128-bit anchor address and the slot's payload tells it whether the slot is a primary or a secondary.

```rust
pub struct DataFile { /* mmap or Arc<File> + page directory */ }

impl DataFile {
    pub fn open(path: &Path, page_size: usize) -> Result<Self>;
    pub fn read_anchor (&self, key: AnchorKey)  -> Result<Option<AnchorSlot>>;
    pub fn write_anchor(&self, key: AnchorKey, slot: &AnchorSlot) -> Result<()>;
    pub fn read_versioned (&self, id: Id) -> Result<Option<VersionedRecord>>;
    pub fn write_versioned(&self, rec: &VersionedRecord) -> Result<()>;
}

/// 128-bit anchor address. Equal to a record's `Id` for property-hashed primaries
/// and for secondaries; equal to `Id` with `created_at = 0` for synthetic primaries.
#[repr(transparent)]
pub struct AnchorKey(u128);

pub struct AnchorSlot {
    pub kind: AnchorKind,
    pub references: Vec<AnchorKey>,
}

pub enum AnchorKind {
    /// The primary anchor for a record. Holds the data (or a pointer to it)
    /// plus the list of secondary anchors that redirect to this slot.
    Primary {
        current_version_at: u64,
        mode:               AnchorMode,            // Inline { bytes } | Pointer { id }
        secondaries:        Vec<AnchorKey>,        // addresses of all redirects
    },
    /// A secondary anchor — only ever a redirect to a primary.
    Secondary {
        primary:   AnchorKey,
        // small marker so a stale read from a recycled page is detectable
        marker:    u64,
    },
    /// Tombstone for a deleted primary; secondaries get their own tombstone kind below.
    PrimaryTombstone { final_version_at: u64 },
    SecondaryTombstone,
}
```

The storage engine resolves a read by:

1. Reading the slot at `AnchorKey`.
2. If it's `Secondary { primary, .. }`, recursing once to read the primary. (The recursion is bounded at 1; secondaries can't chain.)
3. If it's `Primary`, returning the data directly.

Resolving a delete walks the primary's `secondaries` list, tombstones each, then tombstones the primary — all journaled in a single transaction so no orphan secondary can survive a primary delete.

### Tests

`crates/wavedb-storage/tests/single_file.rs`:

```rust
use tempfile::tempdir;
use wavedb_core::Id;
use wavedb_storage::DataFile;

#[test]
fn write_then_read_anchor_inline() {
    let dir  = tempdir().unwrap();
    let file = DataFile::open(&dir.path().join("data"), 4096).unwrap();
    let id   = Id::new(42, 0, 7, 1_000_000);
    let slot = AnchorSlot::inline(&[/* bytes */]);
    file.write_anchor(id.into(), &slot).unwrap();
    let got = file.read_anchor(id.into()).unwrap().unwrap();
    assert_eq!(got.bytes(), slot.bytes());
}

#[test]
fn version_chain_links_correctly() {
    /* write v1, write v2 with old_mod=v1, assert v1.new_mod == v2.id */
}

#[test]
fn secondary_anchor_redirects_to_primary() {
    /* write a primary at addr P, write a secondary at addr S pointing to P,
       read S, assert the engine resolves through to the primary's data,
       assert the primary's `secondaries` list contains S. */
}

#[test]
fn delete_primary_tombstones_all_secondaries() {
    /* write P + S1 + S2, delete the primary, assert S1 and S2 are tombstoned
       in the same journaled transaction (replay test). */
}
```

A property test inserts N records under each of the two primary-anchor variants — synthetic and property-hashed — and asserts every one is retrievable through both its primary and (where declared) its secondaries, plus that double-hashing is invoked at the expected fill ratio (P2 from the design).

### Acceptance

```sh
cargo test -p wavedb-storage --features single-file
```

A 1 MB data file holds ~256 4 KB pages; the test fills 70 % of pages, asserts double-hashing kicks in, and round-trips every anchor.

---

## Phase 4 — Indexes (adaptive array → B+ tree)

**Goal:** Per-`(STRUCT_ID, TENANT_ID)` indexes that start as a contiguous byte array on the page and atomically convert to a 4 KB-aligned B+ tree at `MAX_NON_UNIQUE_ELEMENTS + 1`. Each index entry points to an **anchor**, never to a versioned record.

### Module layout

```
src/index/
├── mod.rs
├── array.rs          # linear scan, cache-friendly
├── btree.rs          # 4 KB BTreeNode
├── adaptive.rs       # the array → btree converter
└── discrete.rs       # hash-bucket → array-or-tree (P11 discrete index)
```

### Key shape

```rust
pub trait IndexBackend {
    fn insert(&mut self, key: IndexKey, anchor: AnchorAddr) -> Result<()>;
    fn lookup(&self, key: &IndexKey) -> Result<Option<AnchorAddr>>;
    fn iter_range<'a>(&'a self, range: Range<IndexKey>) -> impl Iterator<Item=AnchorAddr> + 'a;
}

pub struct AdaptiveIndex {
    backend: IndexState,        // Array(Vec<…>) | BTree(BTreeNodeId)
    threshold: u32,             // MAX_NON_UNIQUE_ELEMENTS
}
```

### Tests

- A property test with `proptest` inserting random key sequences, asserting (a) the conversion happens at exactly `threshold + 1` and is one-way, (b) every key inserted is retrievable from both states, and (c) ordered iteration is sorted.
- A unit test that times 50 lookups in array mode against 50 lookups in B+ tree mode and prints the difference (informational only — doesn't gate CI).
- A trybuild test that the proc-macro attribute `btree_threshold = 100` correctly overrides the default.

### Acceptance

`cargo test -p wavedb-storage` passes with index module included.

---

## Phase 5 — Compression (zstd + per-STRUCT dictionaries)

**Goal:** Heap values compressed with zstd; stack data compressed with per-STRUCT dictionaries cached in memory and persisted in `dictionaries_file`. Old pages keep working under their original dictionary version.

### Module layout

```
src/compression/
├── mod.rs
├── heap_zstd.rs
├── dict_cache.rs       # LRU bounded by max_dict_memory
├── dict_file.rs        # the on-disk versioned dictionary file
└── page_codec.rs       # encode/decode a page given its dict version
```

### Key API

```rust
pub struct DictCache { /* LRU bounded by max_dict_memory */ }

impl DictCache {
    pub fn get_or_load(&self, sid: u32, version: u32) -> Result<Arc<Dictionary>>;
    pub fn record_journal_update(&self, sid: u32, new_version: u32, payload: &[u8]);
}
```

### Tests

- Round-trip a `(STRUCT_ID, payload)` through encode/decode and assert the bytes survive.
- Build a dictionary from 10 000 sample stack records of one STRUCT and assert the compression ratio is below a configured threshold (informational).
- Write a page under dict v1, advance to v2, write another page, assert both pages still decode correctly.

### Acceptance

Compression module is in `wavedb-storage`'s test matrix and passes.

---

## Phase 6 — Heap data strategy

**Goal:** Three-step heap pipeline (inline zstd → hashed overflow block → 4 KB-aligned heap-anchor in heap file). The heap-anchor key is `(CREATED_AT, SHARD_ID, TENANT_ID)`.

### Tests

- A 1 KB value is stored inline, retrieved correctly.
- A 100 KB value overflows into the heap file, retrieved with a bounded **2 IO reads** (asserted by counting calls on a wrapped `File`).
- 4 KB alignment is asserted for every heap-file append.

---

## Phase 7 — Journal & write pipeline

**Goal:** All writes go through the journal first, then a background drain. Cache pressure causes write backpressure. Crash recovery replays the journal on startup. This is where the actor model lands.

### Module layout

```
src/pipeline/
├── mod.rs
├── cache.rs           # hash-map shaped cache mirroring on-disk layout
├── journal.rs         # append-only journal + replay
├── drain.rs           # the background actor
└── backpressure.rs
```

### Test scenarios

1. **Happy path** — write 1 000 records, drain, restart, all 1 000 are readable from the data file.
2. **Mid-drain crash** — write 1 000, kill the drain actor at random points, restart, journal replay reconciles state.
3. **Cache pressure** — write at a rate exceeding drain capacity, observe writes blocking, assert no record is lost.

This is the first phase that uses tokio:

```toml
[dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "sync", "time"] }
```

### Acceptance

`cargo test -p wavedb-storage --features pipeline` runs all three scenarios. Crash-recovery tests use `tempfile` to spawn the engine in a child task that's aborted at random.

---

## Phase 8 — Migrations (Type 1, Type 2, chains, rollback)

**Goal:** Both migration types from the design, plus rollback for both, plus chain resolution (`v_n → v_n+1 → … → v_current`).

### Crate-level API

In `wavedb-core`:

```rust
#[async_trait::async_trait]
pub trait MigrateForward {
    type Old;
    type New;
    async fn migrate(db: &Db, old: Self::Old) -> Result<Self::New>;
}

#[async_trait::async_trait]
pub trait MigrateBackward {
    type Old;
    type New;
    async fn rollback(db: &Db, new: Self::New) -> Result<Self::Old>;
}

pub struct MigrationRegistry { /* graph of registered migrations */ }

impl MigrationRegistry {
    pub fn register_simple<M: MigrateForward + MigrateBackward>(&mut self);
    pub fn register_compose<M: ComposeMigration>(&mut self, descriptor: ComposeDescriptor);
    pub async fn resolve(&self, from: VersionRef, to: VersionRef) -> Result<MigrationPlan>;
}
```

### Test cases

- **Type 1 round-trip** — register `Message41 → Message42` and its rollback, assert forward then back yields the original bytes.
- **Type 2 round-trip** — register `Order + OrderItem → OrderSummary1` and its rollback, assert lookup-first behaviour: when `OrderSummary1` exists, rollback runs; when it doesn't, the engine falls through to the source records.
- **Chain** — register `v1→v2`, `v2→v3`, `v3→v4`; ask for `v1→v4`, assert the planner produces a 3-step chain in the correct order.
- **Coexistence** — write `Message41` and `Message42` to different anchors, read both, assert the lazy migration runs only on `Message41` reads.

### Acceptance

`cargo test -p wavedb-core --features migrations` passes all four scenarios.

---

## Phase 9 — Permissions

**Goal:** `permission: Option<PermissionRef>` is enforced on every read/write. `None` is the cheap path; inline list auto-promotes; group ref dereferences a separately-stored `PermissionGroup`.

### Tests

- A record with `permission = None` is reachable only by users in the same tenant.
- A record with `permission = Some(Inline([user_a, user_b]))` rejects user_c.
- An inline list crossing the threshold (e.g. 50 entries) auto-promotes to a per-record B+ tree on next write — assert post-promotion the same membership semantics hold.
- A group ref correctly resolves through the permissions table.

### Acceptance

`cargo test -p wavedb-storage --features permissions` passes.

---

## Phase 10 — Client API (`wavedb`)

**Goal:** The umbrella crate exposes the four `Db::open` constructors, `another_tenant`, `search`, `query`, `save`, `delete`, and a working `Drop` impl that disconnects from the Quick-Node.

### `crates/wavedb/Cargo.toml`

```toml
[package]
name    = "wavedb"
version = "0.1.0"
edition.workspace = true

[features]
default = ["native"]
native  = ["dep:tokio", "dep:wavedb-net"]
wasm    = ["dep:wasm-bindgen-futures", "dep:gloo-net"]

[dependencies]
wavedb-core    = { path = "../wavedb-core" }
wavedb-macros  = { path = "../wavedb-macros" }
wavedb-storage = { path = "../wavedb-storage" }
wavedb-net     = { path = "../wavedb-net", optional = true }
tokio                  = { workspace = true, optional = true }
wasm-bindgen-futures   = { version = "0.4", optional = true }
gloo-net               = { version = "0.6", optional = true }
```

### Public surface

```rust
pub mod prelude {
    pub use wavedb_core::{Id, Metadata, PermissionRef, Shape, WaveDbStruct};
    pub use wavedb_macros::wave_db;
    pub use crate::{Db, query::Expr};
}

pub struct Db { /* connection + local cache (file or localStorage) */ }

impl Db {
    /// Native, file-backed, tenant = user.
    pub async fn open(url: &str, path: &Path, user: u64) -> Result<Self>;

    /// Native, file-backed, explicit tenant.
    pub async fn open_with_tenant(url: &str, path: &Path, user: u64, default_tenant: u64) -> Result<Self>;

    /// WASM, localStorage, explicit tenant.
    #[cfg(feature = "wasm")]
    pub async fn open_wasm_with_tenant(url: &str, user: u64, default_tenant: u64) -> Result<Self>;

    /// WASM, localStorage, tenant = user.
    #[cfg(feature = "wasm")]
    pub async fn open_wasm(url: &str, user: u64) -> Result<Self>;

    pub async fn another_tenant(&self, tenant: u64) -> Result<Self>;
}

impl Drop for Db {
    fn drop(&mut self) {
        // Notify the quick-node that the session is going away.
        // Spawn-and-forget on a global runtime handle.
    }
}

pub trait UniqueObject: WaveDbStruct + Sized {
    async fn search(db: &Db) -> Result<Option<Self>>;
    async fn save(self, db: &Db) -> Result<()>;
}

pub trait NonUniqueObject: WaveDbStruct + Sized {
    async fn query(db: &Db, expr: Expr) -> Result<Vec<Self>>;
    async fn save(self, db: &Db) -> Result<()>;
    async fn delete(self, db: &Db) -> Result<()>;
}
```

### Tests with mocked transport

Define a `MockTransport` in `wavedb-net` that records every request and replies from a scripted state machine. The unit tests for `Db` use `MockTransport` to verify:

- `Db::open(...)` returns both owner and backup URLs.
- A failed request to the owner causes a switch to the backup.
- `Drop` produces a disconnect message exactly once.
- `another_tenant` produces a session bound to the new tenant.

### Acceptance

`cargo test -p wavedb` passes the full client-side test suite.

---

## Phase 11 — Network transport (`wavedb-net`)

**Goal:** Both transports work end-to-end: a client can do `db.search()` over WebSocket and over HTTP POST, and the HTTP path implements the single-queue + piggybacked-notifications model from the README.

### Crate layout

```
src/
├── lib.rs
├── frame.rs          # request/response framing (postcard envelopes)
├── http.rs           # HTTP POST client + server + the single-queue
├── ws.rs             # tokio-tungstenite client + server
├── notify.rs         # bloom-filter screen sync, push side
└── mock.rs           # in-process transport for tests
```

### Cargo deps

```toml
[dependencies]
wavedb-core   = { path = "../wavedb-core" }
tokio         = { workspace = true }
tokio-tungstenite = "0.21"
axum          = "0.7"
reqwest       = { version = "0.12", features = ["rustls-tls"] }
bytes         = "1"
postcard      = { workspace = true }
serde         = { workspace = true }
fastbloom     = "0.5"

[dev-dependencies]
tempfile      = { workspace = true }
proptest      = { workspace = true }
```

### HTTP single-queue client (skeleton)

```rust
pub struct HttpClient {
    queue:    Mutex<VecDeque<PendingRequest>>,
    notify:   Notify,            // wakes the worker on enqueue
    interval: Duration,          // http_poll_interval
    base_url: Url,
}

impl HttpClient {
    pub async fn run(&self) {
        loop {
            // 1. wait for a queued request OR the idle tick
            let req = self.next_request_or_tick().await;
            // 2. POST it (empty body if it was a tick)
            let resp = self.post(req).await?;
            // 3. dispatch the requested response
            self.complete_request(resp.requested);
            // 4. dispatch piggyback notifications (object-changed events)
            for n in resp.notifications { self.event_bus.publish(n); }
        }
    }
}
```

The `event_bus.publish` step is what the UI subscribes to — exactly the same code path the WebSocket pushes hit. From the application's side the transport is invisible.

### Tests

- **Mock transport round-trip** — client sends Request A, server replies; client sends Request B, server replies with B + a piggyback notification; assert both arrive on the right channels.
- **HTTP idle tick** — start a client with an empty queue, observe an empty POST hitting the server within `http_poll_interval`.
- **HTTP backpressure ordering** — submit 10 requests rapidly, assert the server sees them in order.
- **WebSocket push** — server pushes 100 anchor updates, client receives all 100 in order.
- **Bloom filter sync** — client sends a filter, server pushes only the IDs not in the filter.

### Acceptance

`cargo test -p wavedb-net` passes both transport integration tests against an in-process server.

---

## Phase 12 — Quick-Node binary (`wavedb-quick-node`)

**Goal:** A standalone tokio binary that:

- Listens for HTTP POST and WebSocket connections.
- Owns a configurable set of `(TENANT_ID, SHARD_ID)` partitions via a Consistent Hash Ring.
- Replicates writes to its peer Quick-Nodes.
- Pushes periodic flushes to a configured Slow-Node.
- Publishes its bloom filter of owned IDs at `bloom_filter_publish_interval`.

### Crate Cargo.toml

```toml
[[bin]]
name = "wavedb-quick-node"
path = "src/main.rs"

[dependencies]
wavedb-core    = { path = "../wavedb-core" }
wavedb-storage = { path = "../wavedb-storage" }
wavedb-net     = { path = "../wavedb-net" }
tokio          = { workspace = true }
clap           = { version = "4", features = ["derive"] }
tracing        = "0.1"
tracing-subscriber = "0.3"
```

### CLI shape

```sh
wavedb-quick-node \
    --listen 0.0.0.0:7700 \
    --data-dir /var/lib/wavedb/quick \
    --peers   10.0.0.2:7700,10.0.0.3:7700 \
    --slow-node 10.0.0.10:7800 \
    --owns "tenant=42 shards=0..1024"
```

### Tests

Unit tests for the routing-ring logic, ownership-transfer protocol, and replication watermark live in this crate. Full E2E tests live in Phase 14.

### Acceptance

`cargo test -p wavedb-quick-node` passes ownership-transfer unit tests against an in-process peer.

---

## Phase 13 — Slow-Node binary (`wavedb-slow-node`)

**Goal:** A standalone tokio binary that:

- Receives history flushes from Quick-Nodes.
- Stores them in `data` + `journal` mode set to "History Only" (per the README's file-layout operation modes).
- Serves history reads (low priority, high latency tolerated).

The internal storage engine is the same `wavedb-storage` library as on Quick-Nodes, just with different configuration knobs and a "History Only" file profile.

### Tests

- A flush from a fake Quick-Node arrives, the records are written, the receipt watermark advances.
- A history read for a known versioned ID returns the correct bytes.

### Acceptance

`cargo test -p wavedb-slow-node` passes.

---

## Phase 14 — Distributed E2E test harness (user + quick + slow)

**Goal:** A reusable in-process cluster harness in `wavedb-test-cluster` that spins up:

- N Quick-Node instances (default 2),
- 1 Slow-Node instance,
- 1 or more user-side `Db` clients,

…all on different tokio tasks within a single test process. With this harness, every distributed scenario from the README becomes a `#[tokio::test]`.

### Harness API

```rust
pub struct TestCluster {
    pub quick_nodes: Vec<QuickNodeHandle>,
    pub slow_node:   SlowNodeHandle,
    pub front_door:  Url,
}

impl TestCluster {
    pub async fn spawn(spec: ClusterSpec) -> Self;
    pub async fn open_user(&self, user_id: u64, tenant: u64) -> wavedb::Db;
    pub async fn kill_quick_node(&mut self, idx: usize);
    pub async fn restart_quick_node(&mut self, idx: usize);
    pub async fn flush_to_slow_node(&self);
    pub async fn shutdown(self);
}

pub struct ClusterSpec {
    pub num_quick_nodes:    usize,
    pub min_replicas:       usize,
    pub http_poll_interval: Duration,
    pub force_http:         bool,        // disable WebSocket for fallback tests
}
```

### Tests

`tests/e2e_*.rs` at the workspace root:

| File                       | Scenario                                                                                              |
| -------------------------- | ----------------------------------------------------------------------------------------------------- |
| `e2e_failover.rs`          | User connects, owner Quick-Node is killed; client switches to backup without losing in-flight writes. |
| `e2e_replication.rs`       | A write on the owner is visible on the replica within `MIN_REPLICAS` seconds.                         |
| `e2e_cold_handoff.rs`      | After N writes, history records are flushed to the Slow-Node and freed on the Quick-Node.             |
| `e2e_migration_rollout.rs` | Quick-Node A on `Message42`, Quick-Node B on `Message43`. A user writing through B reads through A.   |
| `e2e_http_polling.rs`      | Force `http`-only transport, assert idle ticks and piggyback notifications work end-to-end.           |
| `e2e_ws_screen_sync.rs`    | Client sends a bloom filter, server pushes only the deltas, assert byte-count savings.                |

### Example skeleton

```rust
// tests/e2e_failover.rs
use wavedb_test_cluster::*;
use wavedb::prelude::*;

#[tokio::test]
async fn user_failover_to_backup() -> anyhow::Result<()> {
    let mut cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        min_replicas:    2,
        http_poll_interval: std::time::Duration::from_secs(1),
        force_http:      false,
    }).await;

    let db = cluster.open_user(/*user*/ 1, /*tenant*/ 42).await;

    let order = Order { id: Default::default(), metadata: Default::default(), amount: 100 };
    order.save(&db).await?;

    cluster.kill_quick_node(0).await;     // kill the owner
    let again = Order::query(&db, Expr::all()).await?;
    assert_eq!(again.len(), 1);

    cluster.shutdown().await;
    Ok(())
}
```

### Acceptance

```sh
cargo nextest run --workspace --tests
```

The full E2E matrix passes locally and in CI.

---

## Phase 15 — WASM client build (`wavedb-wasm`)

**Goal:** The same client API compiled for `wasm32-unknown-unknown`, with localStorage in place of the file backend, `gloo_net` in place of `tokio-tungstenite` and `reqwest`.

### Cargo.toml

```toml
[package]
name = "wavedb-wasm"
version = "0.1.0"
edition.workspace = true

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
wavedb       = { path = "../wavedb", default-features = false, features = ["wasm"] }
wasm-bindgen = "0.2"
wasm-bindgen-futures = "0.4"
gloo-net     = { version = "0.6", features = ["http", "websocket"] }
gloo-storage = "0.3"
js-sys       = "0.3"
web-sys      = { version = "0.3", features = ["Window", "Storage"] }
serde-wasm-bindgen = "0.6"
```

### Build

```sh
wasm-pack build crates/wavedb-wasm --target web --release
```

### Tests

- `wasm-bindgen-test` + `wasm-pack test --headless --chrome` against a small fake HTTP/WS server.
- Smoke test: open a Db with localStorage, write a Unique record, refresh, read it back.

### Acceptance

`wasm-pack test --headless --firefox crates/wavedb-wasm` passes.

---

## Phase 16 — Examples

Each example is a self-contained subdirectory under `examples/` that runs against the Phase 14 cluster harness (so they double as smoke tests).

### `examples/unique_user_profile/`

Demonstrates the **Unique** shape from all three roles:

- `client.rs` — opens a `Db`, calls `UserProfile::search`, creates if absent, updates if present.
- `quick_node.rs` — a minimal Quick-Node that owns the tenant, applies the write.
- `slow_node.rs` — receives the eventual flush.

```rust
// examples/unique_user_profile/client.rs

use wavedb::prelude::*;

#[wave_db(struct_id = 1)]
pub struct UserProfile1 {
    pub id:           Id,
    pub metadata:     Metadata,
    pub display_name: String,
    pub bio:          String,
}
pub type UserProfile = UserProfile1;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db = Db::open("ws://localhost:7700", "/tmp/wavedb-client".as_ref(), 42).await?;

    let profile = match UserProfile::search(&db).await? {
        Some(mut existing) => {
            existing.bio = "Updated.".into();
            existing.save(&db).await?;
            existing
        }
        None => {
            let new = UserProfile {
                id:           Default::default(),
                metadata:     Default::default(),
                display_name: "Aurora".into(),
                bio:          "First write.".into(),
            };
            new.save(&db).await?;
            new
        }
    };

    println!("profile = {profile:?}");
    Ok(())
}
```

### `examples/nonunique_orders/`

NonUnique shape with anchor + adaptive index:

```rust
#[wave_db(struct_id = 10, NonUnique, btree_threshold = 50)]
pub struct Order1 {
    pub amount:   u64,
    pub customer: u64,
}
pub type Order = Order1;

let recent = Order::query(&db, Expr::gt(Order::amount, 100)).await?;
```

### `examples/nested_invoice_lines/`

NonUnique-within-NonUnique:

```rust
#[wave_db(struct_id = 20, NonUnique)]
pub struct Invoice1 {
    pub customer: u64,
    pub lines:    Iter<InvoiceLine1>,        // tightly bound child collection
}
pub type Invoice = Invoice1;

#[wave_db(struct_id = 21, NestedNonUnique)]
pub struct InvoiceLine1 {
    pub product:  u64,
    pub quantity: u32,
}
pub type InvoiceLine = InvoiceLine1;
```

The example demonstrates that querying invoice lines goes through the parent invoice, not through a top-level `InvoiceLine` index — that's what makes them _nested_, not generic NonUnique.

### `examples/migration_type_1/`

Same struct, version up. Two files: `message_v41.rs` and `message_v42.rs`. The `pub type Message = Message42;` line lives in `lib.rs`, so flipping versions is a single edit.

```rust
// migration registration
async fn migrate_41_42(_: &Db, old: Message41) -> Result<Message42> {
    Ok(Message42 {
        body:    old.body,
        author:  old.author,
        edited:  false,           // new field defaulted
    })
}

async fn rollback_42_41(_: &Db, new: Message42) -> Result<Message41> {
    Ok(Message41 { id: new.id, metadata: new.metadata, body: new.body, author: new.author })
}
```

### `examples/migration_type_2/`

`Order + OrderItem → OrderSummary`. Demonstrates the lookup-first rollback path: the rollback first checks whether `OrderSummary1` exists; if not, it returns the original `Order` and `OrderItem`s.

### `examples/distributed_cluster/`

Runs the full Phase 14 cluster harness from `main`, prints the topology, accepts user input, and lets you exercise failover from a tiny REPL. Useful as a manual demo.

---

## Phase 17 — Benchmarks (criterion)

Criterion is wired in from Phase 1 with a single placeholder bench, and we add real benches as each phase lands.

### `benches/storage.rs`

```rust
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::tempdir;
use wavedb_storage::DataFile;

fn bench_anchor_read(c: &mut Criterion) {
    let dir = tempdir().unwrap();
    let f   = DataFile::open(&dir.path().join("data"), 4096).unwrap();
    /* preload N anchors */

    let mut group = c.benchmark_group("anchor_read");
    group.throughput(Throughput::Elements(1));
    for &n in &[1_000u64, 10_000, 100_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| { /* random lookup of one of the n anchors */ });
        });
    }
}

criterion_group!(benches, bench_anchor_read);
criterion_main!(benches);
```

### Suite layout

| File                     | Measures                                                                      |
| ------------------------ | ----------------------------------------------------------------------------- |
| `benches/storage.rs`     | Anchor read/write IOPs; versioned-record write; double-hashing fallback cost. |
| `benches/index.rs`       | Linear-array vs B+ tree at 1, 10, 50, 100, 1 000, 10 000 items.               |
| `benches/compression.rs` | zstd ratio on heap; per-STRUCT dictionary ratio on stack.                     |
| `benches/network.rs`     | HTTP POST round-trip; WebSocket push throughput; bloom-filter sync byte cost. |

### Running

```sh
cargo bench --workspace                                 # run everything
cargo bench --workspace -- --save-baseline pre-change   # record baseline
# … make a change …
cargo bench --workspace -- --baseline pre-change        # diff against baseline
```

Criterion outputs HTML reports under `target/criterion/`. CI uploads these as artifacts on every PR (see _Continuous Integration_).

---

## Phase 18 — Documentation

**Goal:** `cargo doc --workspace --no-deps --open` produces a complete API reference; every public item has a doc comment; every example compiles as a doc-test.

### House style

- `#![warn(missing_docs)]` in every library crate; bumped to `deny` before release.
- Every public item starts with one summary sentence, followed by a blank line, then optional details.
- Every public function has at least one **doc-test** (`/// # Examples`) that compiles. For functions that need a `Db`, use the in-process mock transport so the doc-test runs without a network.
- Cross-link with intra-doc links: ``[`Id`]`` rather than `Id`.

### Example doc-test

````rust
/// Open a native, file-backed Db where the tenant is the user.
///
/// # Examples
///
/// ```no_run
/// # use std::path::Path;
/// # use wavedb::Db;
/// # async fn _example() -> wavedb::Result<()> {
/// let db = Db::open("ws://localhost:7700", Path::new("/tmp/wavedb"), 42).await?;
/// # Ok(()) }
/// ```
pub async fn open(url: &str, path: &Path, user: u64) -> Result<Self> { /* ... */ }
````

### Top-level rustdoc

The `wavedb` crate's `lib.rs` carries a 100-line tour mirroring the README's structure: data shapes, IDs, Metadata, the four operation modes, the migration model, and a worked Unique-and-NonUnique example. This is what users see first on docs.rs.

### Optional: mdBook

For longer-form architectural narrative, generate an `mdBook` from the `README.md` plus a few extra chapters (one per Phase), and publish to GitHub Pages. The book uses `mdbook-linkcheck` to keep links honest.

---

## Continuous Integration

A single GitHub Actions workflow that runs the full matrix on every PR.

### `.github/workflows/ci.yml` (skeleton)

```yaml
name: ci
on: [push, pull_request]

jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { components: rustfmt, clippy }
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace --all-features

  e2e:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: taiki-e/install-action@nextest
      - uses: Swatinem/rust-cache@v2
      - run: cargo nextest run --workspace --tests --release

  wasm:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: wasm32-unknown-unknown }
      - run: cargo install wasm-pack
      - run: wasm-pack test --headless --firefox crates/wavedb-wasm

  bench:
    runs-on: ubuntu-latest
    if: github.event_name == 'pull_request'
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo bench --workspace -- --save-baseline pr
      - uses: actions/upload-artifact@v4
        with: { name: criterion, path: target/criterion }

  audit:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v1
```

### What's gated on green CI

- `cargo fmt --check` — no unformatted code.
- `cargo clippy -D warnings` — no clippy warnings.
- `cargo test` — every unit test passes on Linux/macOS/Windows.
- `cargo nextest run --release` — every E2E scenario passes.
- `wasm-pack test --headless` — browser smoke tests pass.
- `cargo deny check` — no banned licenses, no advisories with active CVEs.

Benchmarks run on PRs but their results are advisory (uploaded as artifacts), not gating.

---

## Troubleshooting & Common Pitfalls

**`#[wave_db]` complains about `struct_id` not fitting.**
The macro validates against `1 << 20`. If you really need more, the design needs a wider `STRUCT_ID` field — that's an ID-format change, not a macro fix.

**Trailing-version parsing fails.**
The struct must end in digits (`Message42`, not `Message_v42`). The macro emits a clear error here.

**Anchor lookup returns `None` after a write.**
Almost always the journal hasn't drained. In tests, call `db.flush().await` (Phase 7) before reading.

**`MAX_CACHED_SIZE` exceeded under benchmark load.**
Expected — Phase 7's backpressure kicks in. Either raise the limit for the benchmark or measure with backpressure as part of the metric.

**WASM build fails with `tokio` errors.**
Make sure the `wasm` feature on `wavedb` is enabled with `default-features = false` and that no transitive `tokio` dependency is leaking in. `cargo tree -e features` is your friend.

**E2E test flakes on CI but not locally.**
Almost always timing-related — replication watermarks or `http_poll_interval`. Use the harness's `cluster.wait_for_replication().await` helper instead of `tokio::time::sleep`.

**`cargo bench` produces no output.**
Make sure you're in `--release` (criterion enforces this) and that `[profile.bench]` inherits from `release`.

**A migration chain returns the wrong version.**
The planner picks the _shortest_ chain by default. If you have multiple registered paths, register them with explicit weights so the planner can disambiguate.

---

## Milestone summary

| Phase | Output                                    | Demo command                                          |
| ----- | ----------------------------------------- | ----------------------------------------------------- |
| 0     | Empty workspace, CI green                 | `cargo test --workspace`                              |
| 1     | `Id`, `Metadata`, postcard round-trip     | `cargo test -p wavedb-core`                           |
| 2     | `#[wave_db]` macro                        | `cargo test -p wavedb-macros`                         |
| 3     | Single-file storage, anchors, versioning  | `cargo test -p wavedb-storage`                        |
| 4     | Adaptive indexes                          | included in storage tests                             |
| 5     | Compression                               | included                                              |
| 6     | Heap strategy                             | included                                              |
| 7     | Journal + write pipeline + crash recovery | included                                              |
| 8     | Migrations                                | `cargo test -p wavedb-core --features migrations`     |
| 9     | Permissions                               | `cargo test -p wavedb-storage --features permissions` |
| 10    | Client API                                | `cargo test -p wavedb`                                |
| 11    | HTTP & WebSocket transports               | `cargo test -p wavedb-net`                            |
| 12    | Quick-Node binary                         | `cargo run -p wavedb-quick-node -- --help`            |
| 13    | Slow-Node binary                          | `cargo run -p wavedb-slow-node -- --help`             |
| 14    | E2E cluster harness                       | `cargo nextest run --tests`                           |
| 15    | WASM client                               | `wasm-pack build crates/wavedb-wasm`                  |
| 16    | Examples                                  | `cargo run --example unique_user_profile`             |
| 17    | Benchmark suite                           | `cargo bench --workspace`                             |
| 18    | Full rustdoc                              | `cargo doc --workspace --no-deps --open`              |

By the end of Phase 18, the repository is a ~30 K-line Rust workspace with a tested storage engine, a working distributed mode, a WASM client, six examples, four benchmark suites, and complete API documentation.

Stop wherever the design needs the most validation next.
