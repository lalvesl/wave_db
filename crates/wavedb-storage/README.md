# wavedb-storage

The per-node **storage engine**: page layout, the per-type page directory,
the block allocator, anchor slots, versioned records, indexes, compression,
heap data, and the journal-backed write pipeline.

> This is where most of WaveDB's engineering energy lives. The cluster layer
> (`wavedb-quick-node`, `wavedb-slow-node`) is deliberately thin; the storage
> engine is not. For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Module map

| Module | Responsibility |
| ------ | -------------- |
| `page` | In-page layout: header, object directory, stack/heap regions, checksum. |
| `file::block_alloc` | `BlockAllocator` — alloc/free/coalesce/truncate contiguous 4 KiB block runs. |
| `file::data` | The `data` file: page directories, anchor slots, versioned records. |
| `hash` | Page-routing hash strategies (`tuple2`/`tuple4`) and double-hash probe. |
| `anchor` | Anchor keys, slots, modes (inline / pointer-only), tombstones. |
| `versioned` | Versioned records and the modification chain. |
| `live` | Per-`(STRUCT_ID, TENANT_ID)` live tracker (chained head + sealed segments). |
| `index` | Adaptive indexes: array → B+ tree, discrete (hash-bucketed) indexes. |
| `heap` | Content-addressed heap entries, dedup by owner-ID list. |
| `compression` | Heap zstd + per-`(STRUCT_ID, version)` dictionary cache + page codec. |
| `pipeline` | Journal, drain actor, backpressure. |
| `permissions` | On-page permission ref encoding. |
| `node_storage` | Top-level façade tying the files together for a node. |

---

## Page Layout

A page is a **homogeneous bucket**: it holds records of **exactly one
`(STRUCT_ID, struct_version)`** and nothing else. It is not a fixed 16 KiB
slot — it is a **run of `block_count` contiguous 4 KiB blocks** in the `data`
file, sized to the bucket it carries and grown in place when the bucket fills
(see _Per-Type Page Directory_).

Internally a page is organised as:

```
┌──────────────────────────────────────────────────┐
│  PageHeader (checksum, entry_count, dict_version)│
│  ──────────────────────────────────────────────  │
│  Vec<(ID, offset, size)>   ← object directory    │
│  ──────────────────────────────────────────────  │
│  [object A bytes][object B bytes][object C ...]  │  ← stacked forward
│                                                  │
│                     [heap value C][heap value B] │  ← growing from end
└──────────────────────────────────────────────────┘
```

The in-page directory gives O(1) lookup of any record's byte range. Fixed-width
(stackable) data lives in the object bytes and compresses against the page's
per-`(STRUCT_ID, version)` dictionary; inline heap data grows from the end
toward the middle.

**Why one type per page is the whole game.** Every record on a page shares the
_exact same byte layout_ — same fields, same widths, same enum domains, same ID
prefixes. That makes the page the ideal unit for dictionary compression, and
the engine never interleaves unrelated tenants' or types' bytes on a hot page.
Mixing is gone at the storage layer too: the page _is_ the access pattern.

---

## Per-Type Page Directory

The `data` file is **partitioned by type**: each live
`(STRUCT_ID, struct_version)` owns its own **page directory** — an in-memory
`Vec<u64>` where every entry is a 64-bit **page descriptor** pointing at one
homogeneous page in the `data` file.

### Page descriptor (`u64`)

| Bits   | Field         | Width | Meaning                                                                     |
| ------ | ------------- | ----- | --------------------------------------------------------------------------- |
| 63..16 | `start_block` | `u48` | Index of the page's first 4 KiB block in the `data` file.                   |
| 15..8  | `block_count` | `u8`  | Contiguous blocks the page occupies (1 ≤ n ≤ 255 ⇒ ≤ ~1 MiB per page).      |
| 7..2   | `occupation`  | `u6`  | Coarse fill gauge in 1/64ths of the page's capacity (0 = empty, 63 = full). |
| 1      | `in_journal`  | bit   | Page lives in the **journal**, not `data.bin`; `start_block` addresses the journal. |
| 0      | `in_memory`   | bit   | Page resident in memory (reserved — not yet implemented).                   |

`start_block` + `block_count` locate the page's bytes; `occupation` is a cached
summary the allocator reads **without touching the page** — enough to decide
"this page must grow" or "this page is a good relocation victim" from the
directory alone. `u48` block addressing covers 2⁴⁸ × 4 KiB = 1 EiB per file.

**The `in_journal` flag.** When set, the page's bytes live in the journal
rather than `data.bin`, and `start_block` is reinterpreted as a journal
address. Journal metadata records when the page is migrated down; once the
drain copies it into `data.bin`, the descriptor is rewritten in place (flag
cleared, address = the new data block). This lets pages be **staged in the
journal during balancing / transitions** without forcing the whole working set
into memory — a relocation can land in the append-only journal first and settle
into `data.bin` lazily. (`in_memory`, bit 0, is reserved for a future
resident-page path.)

### Addressing

A record's page is found by hashing its routing key (its `Id`) and reducing it
against the **length of that type's directory**:

```
page = directory_for[(struct_id, version)][ hash(id) % directory.len() ]
```

Two records that reduce to the same index simply **share the bucket** — the
page holds many records — and a full bucket **grows** rather than spilling to a
neighbour (see _Collision & Fullness_).

- **Anchors** route by `(STRUCT_ID, TENANT_ID)` into the **head version's**
  directory (the live record always carries the current compiled layout).
- **Historical versioned records** route by their full `Id` into **their own
  `struct_version`'s** directory.

A type's history at version `N` and its live data at version `M` therefore live
in physically separate, individually-compressible directories — exactly what
lazy migration already implies.

### In memory, journaled for durability

The directories live in RAM — one `u64` per page, on the hot path of every
read. Every descriptor mutation (a page grew, moved, or the directory resized)
is **appended to the journal in the same atomic step as the data write**, so a
crash never loses the key → block-run map. On startup the directories rebuild
by journal replay over the last snapshot.

### Two kinds of growth

| Growth             | Trigger                                          | Cost                                                                                                                                       |
| ------------------ | ------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------- |
| **Page grow**      | one bucket's `occupation` crosses the fill limit | allocate a larger block run, copy the page, free the old run, rewrite **one** `u64` descriptor. No keys move — `hash % len` is unchanged.   |
| **Directory grow** | most pages of a type are large _and_ full          | append slots to the `Vec<u64>` and rehash that **one type's** records into the longer directory. Scoped to one `(STRUCT_ID, version)`.       |

Page grow is the common case and is **why rebalancing under load is cheap**:
relocate one contiguous run to a bigger hole, patch one pointer. The expensive
directory rehash is rare, per-type, and off the write hot path.

---

## Block Allocator

The `data` file is an **array of fixed 4 KiB blocks**. The page directories
address it; `file::block_alloc::BlockAllocator` owns it.

### Responsibilities

- **Allocate** a contiguous run of `n` blocks (`n = block_count`) for a new or
  growing page, returning its `start_block`.
- **Free** a run when a page is relocated, emptied, or its type is dropped.
- **Coalesce** adjacent free extents so large pages always have somewhere to
  land, and **truncate** the file tail when its trailing blocks are all free.

### Free-space model

Free blocks are tracked as **extents** indexed two ways: by position (`by_start`
— to coalesce neighbours on free) and by size (`by_len` — best-fit, to satisfy
"give me `n` contiguous blocks"). Both maps describe the same maximal,
non-overlapping set; every mutation goes through `insert_free`/`remove_free` so
they never drift apart. Allocation is coarse (whole 4 KiB blocks, ≤ 255 per
page) so the free map stays small.

The allocator is a **pure in-memory structure** — it does not touch the
filesystem and does not journal. Durability is the caller's: `data` journals
each alloc/free as a free-space delta and replays them on startup
(`BlockAllocator::from_state`).

### Dictionary region

Because pages are homogeneous, each `(STRUCT_ID, version)` has **one
dictionary**, stored in the **same `data` file** in allocator-handed blocks. A
small **dictionary directory** maps `(STRUCT_ID, version, dict_version)` →
block run. Page headers carry the `dict_version` they were compressed with, so
pages stay readable across a rebuild; the superseded dictionary run is freed
once no live page references it.

---

## Anchor Slots

**Anchors hold all cross-pointers for a record**, giving the system a stable
place to track index updates and forward references. Every cross-reference
(index entry, M2M link, sync handle) targets the anchor — never a versioned
record — so when data mutates, none of those pointers need rewriting.

### Two slots per live record

| Slot          | Hashed at                                         | Contents                                                                   |
| ------------- | ------------------------------------------------- | -------------------------------------------------------------------------- |
| **Anchor**    | `(STRUCT_ID, TENANT_ID, SHARD_ID)` — no timestamp | Live data (inline) **or** pointer (pointer-only), plus `current_version_at`. |
| **Versioned** | `(STRUCT_ID, TENANT_ID, SHARD_ID, CREATED_AT)`    | Full data + modification chain (`old_mod_id`, `new_mod_id`).               |

The resulting hash selects a slot **within the type's own page directory**
(`hash % directory.len()`), not a global page array.

### Two operating modes

| Mode             | Slot contents                   | Read cost  | Storage     | Use                              |
| ---------------- | ------------------------------- | ---------- | ----------- | -------------------------------- |
| **Inline data**  | Full live record bytes + marker | 1 IO       | ~2× live    | Quick-Nodes — hot paths          |
| **Pointer-only** | Pointer to versioned record     | +1 IO/read | no dup      | Storage-constrained / cold tiers |

### Anchor addressing

Default anchors hash at `(STRUCT_ID, TENANT_ID, SHARD_ID)`. The `#[wave_db]`
macro (see `wavedb-macros`) can opt a struct into **property-hashed primary
anchors** (`SHARD_ID = hash(field)`, content-addressed, implicit uniqueness)
and **secondary anchors** (extra addresses pointing back to the primary, living
in the primary's reference list so deletes/moves stay atomic).

### Mutation, tombstones

1. New versioned record at the new `created_at` hash, `old_mod_id` → previous.
2. Previous record's `new_mod_id` → forward.
3. Anchor slot overwritten with new data + `current_version_at`.

Delete replaces the anchor with a **tombstone** carrying the last live
`created_at`, so "deleted" is distinguishable from "never existed" in one read.

---

## Compression

Two complementary strategies. CPU is free here — there is no join processing
competing for it.

- **Heap (zstd).** Variable-length values (strings, blobs) are zstd-compressed
  before hitting the page heap region.
- **Per-`(STRUCT_ID, version)` dictionaries** for the fixed-width region. Now
  that a page holds exactly one type, the dictionary applies to the whole page
  with nothing foreign to dilute it. Dictionaries live in the `data` file's
  dictionary region (above), bounded in RAM by `max_dict_memory` with LRU
  eviction; updates are journaled and applied by a background task so rebuilds
  never hit the write hot path. Page headers carry their `dict_version` so old
  pages stay readable forever.

---

## Heap Data Strategy

The macro classifies each field **stackable** (fixed-width) or **heapable**
(`String`, `Vec<T>`, blobs) via the struct's layout descriptor; both node tiers
carry it, so either can make the inline-vs-evict decision.

1. **Inline first.** On write, every heapable value is zstd-compressed and
   stored inline in the page heap region. Values larger than `max_heap_inline`
   skip straight to the heap file.
2. **Evict under pressure.** When a page crosses `warning_size_page_occupation`,
   the first response is to evict heapables to the heap file behind
   content-hashed **Heap Anchors** (`u128 = hash(value)`; payload = `u64` heap
   block position).

### Heap file block layout

4 KiB-aligned blocks, an entry spanning as many whole blocks as it needs:

```
┌─────────────────────────── 4KB block(s) ────────────────────────────┐
│ size: u64 │ value bytes …            │ [owner ID, owner ID, …]      │
└──────────────────────────────────────────────────────────────────────┘
```

Identical values **dedup**: the content hash lands on the existing anchor and
the new record's `Id` is appended to the owner list. Empty owner list ⇒ dead ⇒
blocks reclaimed by cleanup. Bounded **2-IO reads** (anchor → block) regardless
of value size.

---

## Index Structures (NonUnique)

Indexes live in a **separate `index` file** and all entries point to **anchors**,
never versioned records — so property mutations don't cascade index rewrites
unless the indexed property itself changed.

Per-`(STRUCT_ID, TENANT_ID)` trees stay small and shallow. Adaptive by size
(`MAX_NON_UNIQUE_ELEMENTS`, default 50):

- **State 1 — linear array** for small collections (cache-friendly, O(N) but N
  is tiny).
- **State 2 — page-aligned B+ tree** once the threshold is crossed (one-way,
  journaled). A 4 KiB node holds ~170 entries; depth 2 covers ~30 K items ⇒
  > 99% of tenant lookups in ≤ 1 IO.

Discrete (value-bucketed) indexes use hash bucket → array-or-tree. Ordered
indexes use the B+ tree. **Deleted is a first-class index** — deleted records
move from the Current tree to the Deleted tree (history reconstruction needs
both).

---

## History

History records live at their natural versioned hash
`(STRUCT_ID, TENANT_ID, SHARD_ID, CREATED_AT)`; the anchor slot holds live
state. Backward traversal: anchor → `current_version_at` → live record →
`old_modification_id` chain. Forward: any record → `new_modification_id` until
`0`.

---

## Files on Disk

Four files, each tuned for an access pattern (single-file mode merges them):

| File      | Contents                                                                                              | Layout                                          | Access                              |
| --------- | ----------------------------------------------------------------------------------------------------- | ----------------------------------------------- | ----------------------------------- |
| `data`    | Per-`(STRUCT_ID, version)` page directories, homogeneous pages, dictionary region; heap-anchor stubs  | Block-addressed (descriptor `u64` → block run)  | Random IO, variable-size pages      |
| `index`   | B+ tree nodes and small-collection array indexes                                                      | Highly contiguous; nodes 4 KiB-aligned          | Sequential within a tree            |
| `heap`    | Content-addressed entries: `size` + bytes + owner-ID list, per 4 KiB block(s)                         | Append-mostly, 4 KiB blocks                     | Append on write, point IO on read   |
| `journal` | In-flight mutations, page-directory updates, block alloc/free + free-space deltas, dictionary updates | Append-only                                     | Append + sequential replay on start |

Index entries reference data records **by ID**, never by file offset, and heap
anchors hold only a `u64` block position — so both files compact independently
of the `data` file without rewriting any pointers.

---

## Collision & Fullness Strategy

Unrelated types never share a page — each `(STRUCT_ID, version)` owns its own
directory. A "collision" is two records of the _same_ type reducing to the same
bucket, which is fine (a page is a multi-record bucket). On crossing
`warning_size_page_occupation`:

1. **Heapable eviction first** — strings/`Vec`s to the heap file behind
   content-hashed Heap Anchors.
2. **Grow the page in place** — allocator hands a larger contiguous run; copy,
   free old, rewrite one descriptor. No keys move (`hash % len` invariant).
3. **Grow the directory** — only when a type's pages are both large
   (near the 255-block ceiling) and uniformly full. Rare, background.

The `occupation` gauge climbing across a directory **is the signal** that a
directory grow is coming — readable without touching a page.

---

## Write Pipeline & Concurrency

A journaled cache fronts the durable files; the cache is shaped like the on-disk
hash-map so reads serve from it directly. `MAX_DISK_IOPS` (soft IO budget) and
`MAX_CACHED_SIZE` (cache RAM budget) govern it.

1. Mutation → in-memory cache (hash-map shape).
2. Same atomic step → journal append.
3. Both done → client confirmed (durability is journal-backed).
4. A background **drain** actor settles `data`/`index`/`heap` at its own pace.

Cache near `MAX_CACHED_SIZE` ⇒ writes block (the only place a writer waits on
the durable layer). Reads pre-empt queued background writes; the journal is
**never** a client read path. Each file is owned by an actor with a per-block
mutex (not whole-file); a tokio broadcast channel publishes invalidations and
free-space events. Idle work: low-priority rebalance, then cleanup/compaction
(`journal` → `index` → `heap`).

---

## Transactions, Locking, Reliability

- **Locks** are ID-scoped `Mutex` entries in process memory; an anchor lock
  covers both the anchor slot and any concurrent versioned write as one unit.
- **Journal** — append-only, replayed on startup; anchor + versioned writes
  journaled together so a mid-mutation crash can't desync the two slots.
- **Checksums** — every page carries a CRC32 verified on read.
- **Reed-Solomon (optional)** — per-page error correction for archive nodes.

---

## Operation Modes (file layout)

| Mode                  | Description                              | Use case              |
| --------------------- | ---------------------------------------- | --------------------- |
| **Single File**       | Data + history + journal in one file     | Development, embedded |
| **Separated History** | Live data and history in separate files  | Production            |
| **History Only**      | History records only                     | Archive / Slow-Node   |

---

## Configuration Parameters

| Parameter                      | Description                                                            | Default          |
| ------------------------------ | --------------------------------------------------------------------- | ---------------- |
| `block_size`                   | Allocation unit of the `data` file; a page is `block_count` of these  | 4 KiB            |
| `max_blocks_per_page`          | Ceiling on one page's run (`block_count` is `u8`)                     | 255 (~1 MiB)     |
| `initial_blocks_per_page`      | Block run handed to a freshly created page                            | 4 (16 KiB)       |
| `page_grow_occupation`         | `occupation` gauge (0–63) at which a page is reallocated larger        | 48 (75%)         |
| `directory_grow_threshold`     | Fraction of a type's pages large + full before the directory rehashes | 0.75             |
| `max_heap_inline`              | Largest heapable stored inline; bigger goes straight to the heap file | 25% of page size |
| `warning_size_page_occupation` | Fill threshold to alert                                               | 70%              |
| `max_dict_memory`              | RAM budget for dictionaries                                           | 64 MB            |
| `MAX_NON_UNIQUE_ELEMENTS`      | Array → B+ tree conversion threshold                                  | 50               |
| `MAX_CACHED_SIZE`              | RAM budget for the write/read cache; writes block near the limit      | tunable          |
| `MAX_DISK_IOPS`                | Soft IO budget/sec across background actors                           | hardware         |
| `lock_timeout`                 | Max hold time for an ID lock                                          | 30 s             |
