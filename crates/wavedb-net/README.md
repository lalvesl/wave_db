# wavedb-net

The network transport layer. **WaveDB _is_ the wire protocol** — there is no
separate REST/RPC layer, no DTO split, no API schema to keep in sync with
storage. Whatever the client serializes as a query, the server deserializes
straight into the storage engine.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Module map

| Module    | Responsibility                                          |
| --------- | ------------------------------------------------------- |
| `frame`   | Wire frame encode/decode.                               |
| `ws`      | WebSocket transport (preferred).                        |
| `http`    | HTTP POST transport (fallback) + the client queue.     |
| `notify`  | "object changed" notifications (push / piggyback).     |
| `browse`  | Live-browse / screen-sync surface.                     |
| `auth`    | Session auth, HMAC tokens.                              |
| `request` | Request/response envelopes (`TransportResponse`, `NodeError`). |
| `metrics` | Per-node transport metrics.                             |
| `mock`    | In-process transport for tests.                         |

The crate provides a `Transport` trait with concrete WebSocket / HTTP / mock
implementations; the same operations run on servers, native clients, and (via
`wavedb-wasm`) browsers.

---

## Transports

| Transport         | Native | Browser | Notes                                                |
| ----------------- | ------ | ------- | ---------------------------------------------------- |
| **WebSocket**     | pref.  | pref.   | Bidirectional, push-capable; carries Bloom sync.     |
| **HTTP POST**     | back.  | back.   | When WebSocket is blocked (proxies, restrictive nets). |
| **Future native** | plan.  | n/a     | Higher-throughput native-only transport, in scoping. |

### HTTP POST: single-queue with piggybacked notifications

Plain HTTP can't push, so the client runs a small queue:

1. **One FIFO queue per client** — no concurrent in-flight POST per session;
   ordering is deterministic and the server can attach state changes to the next
   response.
2. **Responses carry more than was asked for** — notifications about on-screen
   objects that changed ride along on the next response (same `new state` event
   the WebSocket pushes).
3. **Idle ticks** — when the queue empties, the client sends empty POSTs at
   `http_poll_interval` so the server can flush pending notifications, backing
   off when none arrive.
4. WebSocket and the future native transport skip all of this — they push
   directly and run the request queue with normal concurrency.

The application code is identical across transports: it always reacts to "object
changed" events; the transport decides push vs. piggyback.

---

## Bloom Filter Screen-Sync

State-sync for the **online** read path. Clients keep a Bloom filter of the
128-bit IDs currently on screen and send it over WebSocket. The owner Quick-Node
compares it against its live anchor slots and pushes back **only the deltas** —
new objects, updated anchors, tombstones. Event-driven: every accepted mutation
triggers a notification to subscribers whose filters might match. For clients
long offline (screen state far behind reality), sending the explicit array of
on-screen IDs back for revalidation is cheaper than the filter.
