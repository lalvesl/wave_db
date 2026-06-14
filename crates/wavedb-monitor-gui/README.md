# wavedb-monitor-gui

Desktop GUI monitor for WaveDB clusters — the graphical sibling of the
`wavedb-monitor` TUI, built with [egui] using the `egui_components`
(shadcn-style) widget set and `egui_charts` from the sibling `egui_shadcn`
checkout (path dependencies by folder for now).

## Running

Always build/run from the repo's dev shell — it pins the toolchain
(`rust-toolchain.toml`) and provides the GUI runtime libraries
(wayland, libxkbcommon, libGL, X11; eframe links them dynamically):

```bash
nix develop --command cargo run -p wavedb-monitor-gui -- \
  --quick-nodes http://127.0.0.1:7700,http://127.0.0.1:7701 \
  --slow-nodes  http://127.0.0.1:7800 \
  --cluster-key 0123456789012345678901234567890123456789012345678901234567890123     # optional — also settable in the Settings tab
  --refresh-ms  500 \
  --tab overview             # overview | nodes | data | settings
```

`--cluster-key` must be the cluster's full 64-hex-character (32-byte)
secret; shorter strings are rejected at startup. Running plain `cargo run`
outside the dev shell uses whatever toolchain rustup defaults to and
usually fails — either at build time (off-pin toolchain) or at startup
(`NoWaylandLib`: GUI libraries not on the loader path).

## Tabs

| Tab          | What it shows                                                                                                                                                                                                                                                                                                                                                                                      |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Overview** | Stat cards (nodes up, writes, reads, rejected, history records, hot-tier disk), cluster topology as a graph chart (gossip mesh between Quick-Nodes, flush edges to Slow-Nodes, bubble size = write/record volume), and write/read IOps area charts.                                                                                                                                                |
| **Nodes**    | Sortable cluster table (status, ring, partitions, counters, memory, disk, uptime). Selecting a row opens the per-node detail: storage breakdown (WAL / data.bin / heap.bin), a data.bin fill gauge, and the page-occupancy heat map.                                                                                                                                                               |
| **Data**     | Browses a Slow-Node's history store via `POST /browse`: tenant list, then the selected tenant's records drawn as a **force graph clustered by struct family** (hub = `STRUCT_ID`, bubble size = payload bytes) — the natural shape for a tenant-partitioned, non-SQL store. Clicking a record fetches its payload through `POST /history` into a hex inspector with the decoded 128-bit ID fields. |
| **Settings** | Cluster-key entry (HMAC-SHA256, masked input), poll cadence, and per-node authorization status.                                                                                                                                                                                                                                                                                                    |

## Authorization

Same model as the TUI monitor: when the cluster runs with a key, every
`/metrics`, `/browse`, and `/history` poll carries a short-lived
`TokenPurpose::Monitor` HMAC token (±30 s replay window). The GUI
distinguishes failure modes per node:

- `OK` — metrics decoded,
- `AUTH` — node answered **HTTP 403** (missing/wrong key; fix it in Settings
  without restarting),
- `DOWN` — connection failed,
- `BAD` — node answered but the payload didn't decode.

The key never leaves the process; tokens are minted per request.

## Architecture

A dedicated worker thread owns the Tokio runtime, the HTTP client, and the
cluster key. The egui thread reads a `Mutex`-shared snapshot and sends
commands (`SetClusterKey`, `Browse`, `FetchRecord`, `SetRefreshMs`) over an
`mpsc` channel; the worker wakes the UI with `Context::request_repaint`.

`POST /browse` is served by `wavedb-slow-node` and returns metadata only
(tenant aggregates + record summaries with the wire-envelope header);
payload bytes stay behind `/history`.

## Tests

`cargo test -p wavedb-monitor-gui` — config parsing plus integration tests
that drive the real worker against an in-process Slow-Node (metrics poll,
authorized browse/fetch, and the 403 path without a key).

[egui]: https://github.com/emilk/egui
