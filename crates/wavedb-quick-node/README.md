# wavedb-quick-node

The **hot tier**. A Quick-Node owns `(TENANT_ID, SHARD_ID)` partitions via a
consistent-hash ring, takes routing ownership for connected users, validates
and preprocesses writes, holds active anchor slots in memory (inline mode), and
replicates to peers. Server and database are the **same binary**.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Module map

| Module        | Responsibility                                                |
| ------------- | ------------------------------------------------------------- |
| `server`      | The `Server` builder — bind, tenant, data_dir, registry, serve. |
| `serve`       | Request handling loop.                                        |
| `ring`        | Consistent-hash ring; derives partition ownership.            |
| `gossip`      | Membership: `Announce` / `Withdraw`, heartbeat.              |
| `ownership`   | Tenant + shard ownership scopes (one writer, n replicas).    |
| `replication` | Post-commit fan-out to the replica set.                      |
| `config`      | Node configuration.                                          |

```rust
Server::bind("0.0.0.0:7700")
    .tenant(42)
    .data_dir("./data")
    .registry(app_objects::REGISTRY)
    .serve()
    .await
```

---

## Hardware & role

Good CPU/RAM, fast NVMe of moderate capacity. Sized for IOPS-per-watt. Holds
active **anchor slots in memory** in **inline-data mode** — the extra storage is
cheap relative to the read-IO it saves. Continuously flushes older versions and
logs down to Slow-Nodes (see [`wavedb-slow-node`](../wavedb-slow-node/README.md))
to reclaim hot-tier space.

---

## Ownership model

**Ownership is never configured — it is computed.** Every node derives who owns
a `(TENANT_ID, SHARD_ID)` partition from the consistent-hash ring: same
membership view ⇒ same answer, so agreement needs no handshake. Gossip moves the
membership:

- **Join:** `Announce` → immediately owns its ring share (minimal reassignment).
- **Solo:** one node _is_ the ring — owns every tenant, zero setup.
- **Graceful leave:** drain → `Withdraw` → ring drops it.
- **Crash:** heartbeat (default 1 s) evicts after 3 misses; ownership
  re-derives to survivors instantly. No records move — replicas already hold the
  data.

Two scopes, both **one writer, n replicas**: **tenant ownership** (Unique data,
shard `0`) and **shard ownership** (NonUnique, 12-bit `SHARD_ID` subdivides a
tenant). The owner is the only writer; it validates, notifies replicas, and
forwards to a Slow-Node.

---

## Replication

Each partition lives on **≥ 2 Quick-Nodes** (`MIN_REPLICAS`): the ring owner
plus the next distinct nodes clockwise. After the owner's WAL commit it pushes
committed bytes to replicas (`POST /replicate`, fire-and-forget with per-peer
ack watermarks) — the client's confirmation never waits on replicas. Durability
to the caller is the owner's journal; redundancy is asynchronous. Replicas store
canonical (validated + preprocessed) bytes verbatim and never accept client
writes. Rack-aware placement is a planned refinement of the same walk.

---

## Routing & failover

A client knows **two** Quick-Nodes — owner and backup (both URLs returned at
connect). It writes to the owner; on timeout it switches to the backup and asks
for the new owner. A mutation that hits the wrong node is proxied forward or —
usually cheaper — triggers an ownership-transfer request. Range moves are a
runtime operation, not a deployment.

---

## Consistency

- **Within one tenant:** strong (single writer via routing ownership).
- **Across tenants:** eventual (Bloom-filter sync — see
  [`wavedb-net`](../wavedb-net/README.md#bloom-filter-screen-sync)).
- **On conflict:** most recent state wins via the anchor; the loser becomes a
  branch in the history chain.

---

## Node-side enforcement

Every incoming write passes four gates **before the WAL commit**:

1. **Header check** — `(struct_id, version)` must be declared in
   `declare_objects!`; unknown headers refused.
2. **Decode check** — the payload must parse as the declared type.
3. **`validate`** — the same fn the client ran (catches bypassers / stale rules).
4. **`preprocess`** — the re-encoded result replaces the client's bytes.

Rejections travel back as a structured `NodeError {code, struct_id, field,
message}` and the client maps it to the same typed `Error::Validation` its local
pre-send check raises. A node built without a registry (`QuickNode::new`) keeps
the legacy schema-blind behaviour. Hook declaration lives in
[`wavedb-macros`](../wavedb-macros/README.md#validation--preprocessing-hooks).
