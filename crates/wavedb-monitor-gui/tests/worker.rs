//! Integration tests for the poll worker against an in-process Slow-Node.
//!
//! Drives the real worker thread (metrics poll + browse + record fetch)
//! over HTTP with HMAC auth, asserting on the shared state the GUI renders.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use wavedb_monitor_gui::poll::spawn_worker;
use wavedb_monitor_gui::state::{Command, NodeHealth, Shared, SharedHandle};
use wavedb_net::auth::ClusterKey;
use wavedb_slow_node::flush::FlushBatch;
use wavedb_slow_node::server::router;
use wavedb_slow_node::store::HistoryStore;
use wavedb_storage::VersionedRecord;

const KEY_HEX: &str =
    "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

/// Envelope-prefixed payload: `header = struct_id << 8 | version`.
fn payload(struct_id: u32, version: u8, body: &[u8]) -> Vec<u8> {
    let header = (struct_id << 8) | u32::from(version);
    let mut out = header.to_le_bytes().to_vec();
    out.extend_from_slice(body);
    out
}

/// Start a keyed Slow-Node with two tenants of seeded history.
async fn start_slow_node() -> (std::net::SocketAddr, ClusterKey) {
    let key = ClusterKey::from_hex(KEY_HEX).unwrap();
    let store = HistoryStore::in_memory();
    store
        .apply_flush(FlushBatch {
            write_seq: 1,
            tenant: 42,
            records: vec![
                VersionedRecord::new(100, payload(7, 1, b"hello world")),
                VersionedRecord::new(101, payload(7, 2, b"second")),
                VersionedRecord::new(102, payload(3, 1, b"other family")),
            ],
            token: None,
        })
        .unwrap();
    store
        .apply_flush(FlushBatch {
            write_seq: 2,
            tenant: 9,
            records: vec![VersionedRecord::new(200, payload(1, 1, b"t9"))],
            token: None,
        })
        .unwrap();

    let app = router(store, Some(key.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, key)
}

/// Block until `cond` holds on the shared state or the timeout fires.
fn wait_for(
    shared: &SharedHandle,
    timeout: Duration,
    cond: impl Fn(&Shared) -> bool,
) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond(&shared.lock().unwrap()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_polls_browses_and_fetches_with_auth() {
    let (addr, key) = start_slow_node().await;
    let slow_url = format!("http://{addr}");

    let shared: SharedHandle = std::sync::Arc::new(std::sync::Mutex::new(
        Shared::new(&[], &[slow_url]),
    ));
    let (tx, rx) = mpsc::channel();
    let _worker = spawn_worker(
        shared.clone(),
        rx,
        Some(key),
        100,
        egui::Context::default(),
    );

    // Metrics poll marks the node healthy and decodes SlowNodeMetrics.
    assert!(
        wait_for(&shared, Duration::from_secs(5), |s| {
            s.nodes[0].health == NodeHealth::Ok
                && s.nodes[0]
                    .slow
                    .as_ref()
                    .is_some_and(|m| m.record_count == 4)
        }),
        "metrics poll never succeeded"
    );

    // Tenant-list browse.
    tx.send(Command::Browse {
        slow_idx: 0,
        tenant: None,
    })
    .unwrap();
    assert!(
        wait_for(&shared, Duration::from_secs(5), |s| {
            s.data.tenants.len() == 2
        }),
        "browse never returned tenants"
    );
    {
        let tenants = shared.lock().unwrap().data.tenants.clone();
        assert_eq!(tenants[0].tenant, 9);
        assert_eq!(tenants[1].tenant, 42);
        assert_eq!(tenants[1].record_count, 3);
    }

    // Per-tenant browse: record summaries with decoded envelope headers.
    tx.send(Command::Browse {
        slow_idx: 0,
        tenant: Some(42),
    })
    .unwrap();
    assert!(
        wait_for(&shared, Duration::from_secs(5), |s| {
            s.data.records.len() == 3
        }),
        "browse never returned records"
    );
    {
        let data = shared.lock().unwrap().data.clone();
        assert_eq!(data.selected_tenant, Some(42));
        assert_eq!(data.records[0].id, 100);
        assert_eq!(data.records[0].header >> 8, 7);
        assert_eq!(data.records[0].header & 0xFF, 1);
        assert_eq!(data.records[2].header >> 8, 3);
    }

    // Record fetch through /history decodes the VersionedRecord payload.
    tx.send(Command::FetchRecord {
        slow_idx: 0,
        tenant: 42,
        id: 100,
    })
    .unwrap();
    assert!(
        wait_for(&shared, Duration::from_secs(5), |s| {
            s.data.detail.is_some()
        }),
        "record fetch never landed"
    );
    {
        let detail = shared.lock().unwrap().data.detail.clone().unwrap();
        assert_eq!(detail.id, 100);
        let rec = detail.decoded.expect("payload decodes");
        assert_eq!(rec.data, payload(7, 1, b"hello world"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_without_key_reports_unauthorized() {
    let (addr, _key) = start_slow_node().await;
    let slow_url = format!("http://{addr}");

    let shared: SharedHandle = std::sync::Arc::new(std::sync::Mutex::new(
        Shared::new(&[], &[slow_url]),
    ));
    let (tx, rx) = mpsc::channel();
    let _worker =
        spawn_worker(shared.clone(), rx, None, 100, egui::Context::default());

    // Metrics poll against a keyed node → 403 → Unauthorized health.
    assert!(
        wait_for(&shared, Duration::from_secs(5), |s| {
            s.nodes[0].health == NodeHealth::Unauthorized
        }),
        "unauthorized health never reported"
    );

    // Browse without a token → friendly 403 error surfaced to the UI.
    tx.send(Command::Browse {
        slow_idx: 0,
        tenant: None,
    })
    .unwrap();
    assert!(
        wait_for(&shared, Duration::from_secs(5), |s| {
            s.data
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("403"))
        }),
        "browse 403 never surfaced"
    );
}
