# wavedb-slow-node

The **cold tier**: the immutable journal and history archive. Quick-Nodes
continuously flush older versions and transaction logs down here, releasing
active disk on the hot tier while keeping permanent history off the latency
path.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Hardware & role

Lower CPU, moderate RAM, large-capacity HDD/SSD arrays — sized for **$/TB**,
the opposite trade from a Quick-Node's IOPS-per-watt. A Slow-Node:

- Receives `FlushBatch` POSTs from Quick-Nodes (HMAC `Flush` tokens, batched,
  failed batches re-queued **in order**).
- Stores history records in the tier-symmetric on-disk format — including heap
  entries with their owner-ID list, so the archive knows which historical
  records still reference shared heap data.
- Serves history reads off the latency path.

Runs in **History Only** file-layout mode (see
[`wavedb-storage`](../wavedb-storage/README.md#operation-modes-file-layout)).
Reed-Solomon per-page error correction is the optional archive-grade
reliability layer.

## Library target

Exposed as a library so the `wavedb-test-cluster` harness can spin up in-process
Slow-Nodes as tokio tasks — no subprocesses, no teardown scripts.
