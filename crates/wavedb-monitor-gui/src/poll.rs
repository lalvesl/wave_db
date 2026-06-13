//! Background poll worker for the GUI monitor.
//!
//! Owns the Tokio runtime, the HTTP client and the cluster key.  Polls
//! `/metrics` on every tick, serves Browse/Fetch commands from the GUI,
//! and wakes the egui loop after each update.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use wavedb_net::auth::{ClusterKey, TokenPurpose};
use wavedb_net::browse::{
    BrowseRequest, BrowseResponse, HistoryReadRequest, HistoryReadResponse,
};
use wavedb_net::frame::{decode_payload, encode_payload};
use wavedb_net::metrics::{MetricsRequest, QuickNodeMetrics, SlowNodeMetrics};
use wavedb_storage::VersionedRecord;

use crate::state::{Command, NodeHealth, NodeKind, RecordDetail, SharedHandle};

/// Spawn the poll worker in a dedicated OS thread with its own
/// current-thread Tokio runtime.
pub fn spawn_worker(
    shared: SharedHandle,
    commands: Receiver<Command>,
    initial_key: Option<ClusterKey>,
    refresh_ms: u64,
    egui_ctx: egui::Context,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("poll worker runtime");
        rt.block_on(worker_loop(
            shared,
            commands,
            initial_key,
            refresh_ms,
            egui_ctx,
        ));
    })
}

async fn worker_loop(
    shared: SharedHandle,
    commands: Receiver<Command>,
    mut key: Option<ClusterKey>,
    mut refresh_ms: u64,
    egui_ctx: egui::Context,
) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        // All monitor polls are LAN/loopback — never route through a proxy.
        .no_proxy()
        .build()
        .expect("reqwest client");

    if let Ok(mut s) = shared.lock() {
        s.auth_enabled = key.is_some();
    }

    let mut last_poll = Instant::now()
        .checked_sub(Duration::from_millis(refresh_ms))
        .unwrap_or_else(Instant::now);

    // Last browse the GUI asked for. Re-run on every poll tick so the Data
    // view tracks a live cluster — under continuous writes the record graph
    // and tenant counts grow instead of freezing on the first snapshot.
    let mut sticky_browse: Option<(usize, Option<u64>)> = None;

    loop {
        // Drain GUI commands first so key changes apply to the next poll.
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                Command::SetClusterKey(new_key) => {
                    key = new_key;
                    if let Ok(mut s) = shared.lock() {
                        s.auth_enabled = key.is_some();
                    }
                }
                Command::SetRefreshMs(ms) => refresh_ms = ms.max(100),
                Command::Browse { slow_idx, tenant } => {
                    browse(&shared, &client, key.as_ref(), slow_idx, tenant)
                        .await;
                    sticky_browse = Some((slow_idx, tenant));
                    egui_ctx.request_repaint();
                }
                Command::FetchRecord {
                    slow_idx,
                    tenant,
                    id,
                } => {
                    fetch_record(&shared, &client, slow_idx, tenant, id).await;
                    egui_ctx.request_repaint();
                }
            }
        }

        if last_poll.elapsed() >= Duration::from_millis(refresh_ms) {
            poll_metrics(&shared, &client, key.as_ref()).await;
            // Refresh the open Data view against the live store.
            if let Some((slow_idx, tenant)) = sticky_browse {
                browse(&shared, &client, key.as_ref(), slow_idx, tenant).await;
            }
            last_poll = Instant::now();
            egui_ctx.request_repaint();
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ── Metrics poll ──────────────────────────────────────────────────────────────

/// One node's poll outcome before it's folded into shared state.
enum PollOutcome {
    Quick(QuickNodeMetrics),
    Slow(SlowNodeMetrics),
    Failed(NodeHealth),
}

async fn poll_metrics(
    shared: &SharedHandle,
    client: &reqwest::Client,
    key: Option<&ClusterKey>,
) {
    // Snapshot the node list without holding the lock across awaits.
    let targets: Vec<(usize, NodeKind, String)> = match shared.lock() {
        Ok(s) => s
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (i, n.kind, n.url.clone()))
            .collect(),
        Err(_) => return,
    };

    let req = MetricsRequest {
        token: key.map(|k| k.mint(0, TokenPurpose::Monitor)),
    };
    let Ok(body) = encode_payload(&req) else {
        return;
    };

    let polls = targets.iter().map(|(idx, kind, url)| {
        let body = body.clone();
        async move {
            let outcome = poll_one(client, url, *kind, body).await;
            (*idx, outcome)
        }
    });
    let results = futures_util::future::join_all(polls).await;

    let Ok(mut s) = shared.lock() else { return };
    for (idx, outcome) in results {
        let Some(node) = s.nodes.get_mut(idx) else {
            continue;
        };
        match outcome {
            PollOutcome::Quick(m) => {
                node.health = NodeHealth::Ok;
                node.quick = Some(m);
            }
            PollOutcome::Slow(m) => {
                node.health = NodeHealth::Ok;
                node.slow = Some(m);
            }
            PollOutcome::Failed(health) => node.health = health,
        }
    }

    // Cluster-wide IOps deltas from the quick-node counters.
    let total_writes: u64 = s
        .nodes
        .iter()
        .filter_map(|n| n.quick.as_ref().map(|m| m.write_count))
        .sum();
    let total_reads: u64 = s
        .nodes
        .iter()
        .filter_map(|n| n.quick.as_ref().map(|m| m.read_count))
        .sum();
    let dw = total_writes.saturating_sub(s.prev_writes);
    let dr = total_reads.saturating_sub(s.prev_reads);
    // First tick: counters jump from 0 to lifetime totals — skip the spike.
    if s.poll_count > 0 {
        s.write_history.pop_front();
        s.write_history.push_back(dw);
        s.read_history.pop_front();
        s.read_history.push_back(dr);
    }
    s.prev_writes = total_writes;
    s.prev_reads = total_reads;
    s.poll_count += 1;
}

async fn poll_one(
    client: &reqwest::Client,
    url: &str,
    kind: NodeKind,
    body: bytes::Bytes,
) -> PollOutcome {
    let result = client
        .post(format!("{url}/metrics"))
        .header("Content-Type", "application/octet-stream")
        .body(body.to_vec())
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            let bytes = resp.bytes().await.unwrap_or_default();
            match kind {
                NodeKind::Quick => decode_payload::<QuickNodeMetrics>(&bytes)
                    .map_or(
                        PollOutcome::Failed(NodeHealth::BadResponse),
                        PollOutcome::Quick,
                    ),
                NodeKind::Slow => decode_payload::<SlowNodeMetrics>(&bytes)
                    .map_or(
                        PollOutcome::Failed(NodeHealth::BadResponse),
                        PollOutcome::Slow,
                    ),
            }
        }
        Ok(resp) if resp.status() == reqwest::StatusCode::FORBIDDEN => {
            PollOutcome::Failed(NodeHealth::Unauthorized)
        }
        Ok(_) => PollOutcome::Failed(NodeHealth::BadResponse),
        Err(_) => PollOutcome::Failed(NodeHealth::Unreachable),
    }
}

// ── Browse / record fetch ─────────────────────────────────────────────────────

/// Resolve the URL of the `idx`-th slow node, if it exists.
fn slow_url(shared: &SharedHandle, slow_idx: usize) -> Option<String> {
    let s = shared.lock().ok()?;
    s.nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Slow)
        .nth(slow_idx)
        .map(|n| n.url.clone())
}

async fn browse(
    shared: &SharedHandle,
    client: &reqwest::Client,
    key: Option<&ClusterKey>,
    slow_idx: usize,
    tenant: Option<u64>,
) {
    let Some(url) = slow_url(shared, slow_idx) else {
        return;
    };

    let req = BrowseRequest {
        token: key.map(|k| k.mint(0, TokenPurpose::Monitor)),
        tenant,
        max_records: 0,
    };
    let outcome: Result<BrowseResponse, String> = async {
        let body =
            encode_payload(&req).map_err(|e| format!("encode: {e:?}"))?;
        let resp = client
            .post(format!("{url}/browse"))
            .header("Content-Type", "application/octet-stream")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| format!("request: {e}"))?;
        match resp.status() {
            s if s.is_success() => {
                let bytes =
                    resp.bytes().await.map_err(|e| format!("body: {e}"))?;
                decode_payload::<BrowseResponse>(&bytes)
                    .map_err(|e| format!("decode: {e:?}"))
            }
            reqwest::StatusCode::FORBIDDEN => {
                Err("403 Forbidden — set the cluster key in Settings"
                    .to_string())
            }
            s => Err(format!("HTTP {s}")),
        }
    }
    .await;

    let Ok(mut s) = shared.lock() else { return };
    s.data.source_slow_idx = slow_idx;
    match outcome {
        Ok(resp) => {
            s.data.tenants = resp.tenants;
            s.data.records = resp.records;
            s.data.truncated = resp.truncated;
            s.data.selected_tenant = tenant;
            s.data.last_error = None;
            s.data.generation += 1;
        }
        Err(e) => s.data.last_error = Some(e),
    }
}

async fn fetch_record(
    shared: &SharedHandle,
    client: &reqwest::Client,
    slow_idx: usize,
    tenant: u64,
    id: u128,
) {
    let Some(url) = slow_url(shared, slow_idx) else {
        return;
    };

    let req = HistoryReadRequest {
        tenant,
        record_id: id,
    };
    let outcome: Result<Vec<u8>, String> = async {
        let bytes =
            encode_payload(&req).map_err(|e| format!("encode: {e:?}"))?;
        let resp = client
            .post(format!("{url}/history"))
            .header("Content-Type", "application/octet-stream")
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|e| format!("request: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let body = resp.bytes().await.map_err(|e| format!("body: {e}"))?;
        let decoded: HistoryReadResponse =
            decode_payload(&body).map_err(|e| format!("decode: {e:?}"))?;
        Ok(decoded.data)
    }
    .await;

    let Ok(mut s) = shared.lock() else { return };
    match outcome {
        Ok(raw) => {
            let decoded = VersionedRecord::from_bytes(&raw).ok();
            s.data.detail = Some(RecordDetail { id, raw, decoded });
            s.data.last_error = None;
        }
        Err(e) => s.data.last_error = Some(e),
    }
}
