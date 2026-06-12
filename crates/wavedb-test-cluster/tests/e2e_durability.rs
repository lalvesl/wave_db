//! Durability / data-loss confidence E2E tests.
//!
//! These tests answer one question: **if a Quick-Node is hammered with writes
//! and then dies abruptly, does any acknowledged write disappear?**
//!
//! The invariant under test is the WAL durability contract: every write the
//! server confirmed (`db.send(Write{..})` returned `Ok` and was not rejected)
//! has been journalled and fsynced *before* the response was sent, so it must
//! still be readable after the process crashes and restarts — even when no
//! snapshot ran between the write and the crash.
//!
//! ## How "data loss" is measured
//!
//! Every write in a given test uses the same `(tenant, struct_id)`, so the
//! committed records occupy Ids `Id::new(tenant, 0, struct_id, k)` for
//! `k ∈ 1..=seq`, where `seq` is the node's replication counter (one bump per
//! committed write).  After a crash + restart we re-open the node's storage and
//! count how many of those Ids are readable.  `recoverable == seq` means zero
//! loss; anything less is a lost acknowledged write.
//!
//! ## How a crash is simulated
//!
//! [`TestCluster::kill_quick_node`] aborts the server task with no graceful
//! drain and no snapshot flush — the on-disk files are left exactly as the last
//! fsync left them, which is the worst case for recovery.  The writes are driven
//! to completion *before* the kill so the journal and the `seq` counter are
//! consistent (no half-applied write straddling the crash), keeping the
//! assertion exact rather than probabilistic.

use std::time::Duration;

use wavedb_net::request::RequestKind;
use wavedb_test_cluster::{ClusterSpec, TestCluster};

const STRUCT_ID: u32 = 7;
const PAYLOAD_LEN: usize = 96;

/// Open `clients` sessions against `node_idx` and have each write `writes_per`
/// owned records, awaiting every response.  Returns the total number of writes
/// issued — which, once this returns, equals the number of *committed* writes
/// because `db.send` only resolves after the server's journal-fsync commit.
async fn drive_concurrent_writes(
    cluster: &TestCluster,
    node_idx: usize,
    clients: u64,
    writes_per: u64,
) -> u64 {
    let tenant = cluster.owned_tenant();

    // Open every session first so the writes actually overlap on the wire.
    let cap = usize::try_from(clients).expect("clients fits in usize");
    let mut dbs = Vec::with_capacity(cap);
    for c in 0..clients {
        dbs.push(cluster.open_user_via(c + 1, tenant, node_idx).await);
    }

    let mut handles = Vec::with_capacity(cap);
    for db in dbs {
        handles.push(tokio::spawn(async move {
            let user = db.user();
            for w in 0..writes_per {
                let payload = vec![u8::try_from(w % 251).unwrap(); PAYLOAD_LEN];
                let resp = db
                    .send(RequestKind::Write {
                        struct_id: STRUCT_ID,
                        user,
                        tenant,
                        payload,
                    })
                    .await
                    .expect("write transport failed");
                assert_ne!(resp, b"not_owner", "owned write was rejected");
                assert_ne!(
                    resp, b"storage_error",
                    "owned write hit a storage error"
                );
            }
        }));
    }
    for h in handles {
        h.await.expect("writer task panicked");
    }

    clients * writes_per
}

/// Sanity floor: under heavy concurrent load with no crash, the in-memory data
/// file holds every committed write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_load_has_no_in_memory_loss() {
    let cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 1,
        ..Default::default()
    })
    .await;

    let expected = drive_concurrent_writes(&cluster, 0, 24, 100).await; // 2_400 writes
    let tenant = cluster.owned_tenant();

    let seq = cluster.quick_nodes[0].node.replication().current_seq();
    assert_eq!(
        seq, expected,
        "every committed write must bump the sequence"
    );

    let recoverable =
        cluster.quick_nodes[0].recoverable_versioned(tenant, STRUCT_ID, seq);
    assert_eq!(
        recoverable,
        expected,
        "in-memory data file lost {} of {expected} committed writes",
        expected - recoverable
    );

    cluster.shutdown().await;
}

/// The headline test: high write load, then the node dies *suddenly* (no drain,
/// no snapshot), then restarts.  Every acknowledged write must come back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sudden_death_under_load_loses_no_acknowledged_write() {
    let mut cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 1,
        ..Default::default()
    })
    .await;
    let tenant = cluster.owned_tenant();

    let expected = drive_concurrent_writes(&cluster, 0, 32, 80).await; // 2_560 writes
    let seq = cluster.quick_nodes[0].node.replication().current_seq();
    assert_eq!(seq, expected);

    // Pull the plug: abort the server with no graceful shutdown and no flush.
    cluster.kill_quick_node(0).await;
    assert!(!cluster.quick_nodes[0].is_alive());

    // Restart on the same data_dir — recovery replays the journal.
    cluster.restart_quick_node(0).await;

    let recoverable =
        cluster.quick_nodes[0].recoverable_versioned(tenant, STRUCT_ID, seq);
    assert_eq!(
        recoverable,
        expected,
        "crash recovery lost {} of {expected} acknowledged writes",
        expected - recoverable
    );

    cluster.shutdown().await;
}

/// The same guarantee must hold when a snapshot *did* run before the crash —
/// this exercises the `data.bin` reload path rather than the journal-replay
/// path, so both recovery sources are covered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_before_crash_recovers_all() {
    let mut cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 1,
        ..Default::default()
    })
    .await;
    let tenant = cluster.owned_tenant();

    let expected = drive_concurrent_writes(&cluster, 0, 16, 100).await; // 1_600 writes
    let seq = cluster.quick_nodes[0].node.replication().current_seq();

    // Force a page-table snapshot to data.bin, then crash + restart.
    cluster.quick_nodes[0]
        .storage()
        .expect("node has storage")
        .flush_snapshot()
        .expect("snapshot flush failed");

    cluster.kill_quick_node(0).await;
    cluster.restart_quick_node(0).await;

    let recoverable =
        cluster.quick_nodes[0].recoverable_versioned(tenant, STRUCT_ID, seq);
    assert_eq!(
        recoverable,
        expected,
        "snapshot recovery lost {} of {expected} writes",
        expected - recoverable
    );

    cluster.shutdown().await;
}

/// Recovery must be stable across *repeated* crashes: replay-then-truncate
/// leaves a clean snapshot, so a second crash with no new writes recovers the
/// same data and never duplicates or drops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_crashes_keep_recovering_the_same_data() {
    let mut cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 1,
        ..Default::default()
    })
    .await;
    let tenant = cluster.owned_tenant();

    let expected = drive_concurrent_writes(&cluster, 0, 8, 150).await; // 1_200 writes
    let seq = cluster.quick_nodes[0].node.replication().current_seq();
    assert_eq!(seq, expected);

    // Crash + restart twice in a row, no new writes in between.
    for round in 1..=2 {
        cluster.kill_quick_node(0).await;
        cluster.restart_quick_node(0).await;
        let recoverable = cluster.quick_nodes[0]
            .recoverable_versioned(tenant, STRUCT_ID, seq);
        assert_eq!(
            recoverable, expected,
            "round {round}: recovered {recoverable}, expected {expected}"
        );
    }

    cluster.shutdown().await;
}

/// When one node in a two-node cluster dies under load, the surviving node keeps
/// accepting writes and its own committed data stays intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn surviving_node_keeps_data_after_peer_dies() {
    let mut cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 2,
        ..Default::default()
    })
    .await;
    let tenant = cluster.owned_tenant();
    // The ring decides who owns the tenant; load the owner and kill the
    // OTHER node, so the loaded data must survive untouched.
    let owner = cluster.owner_idx(tenant);
    let peer = 1 - owner;

    let expected = drive_concurrent_writes(&cluster, owner, 12, 100).await; // 1_200 writes
    let survivor_seq =
        cluster.quick_nodes[owner].node.replication().current_seq();
    assert_eq!(survivor_seq, expected);

    // Kill the peer abruptly.
    cluster.kill_quick_node(peer).await;
    assert!(!cluster.quick_nodes[peer].is_alive());
    assert!(cluster.quick_nodes[owner].is_alive());

    // Survivor (the owner) still serves a fresh write…
    let db = cluster.open_user_via(999, tenant, owner).await;
    let resp = db
        .send(RequestKind::Write {
            struct_id: STRUCT_ID,
            user: 999,
            tenant,
            payload: vec![1u8; PAYLOAD_LEN],
        })
        .await
        .unwrap();
    assert_ne!(resp, b"not_owner");
    drop(db);

    // …and none of its previously-committed records vanished.
    let recoverable = cluster.quick_nodes[owner].recoverable_versioned(
        tenant,
        STRUCT_ID,
        survivor_seq,
    );
    assert_eq!(
        recoverable,
        expected,
        "survivor lost {} of {expected} writes when its peer died",
        expected - recoverable
    );

    cluster.shutdown().await;
}

/// Writes that arrive in bursts separated by short idle gaps (the journal is
/// fsynced per write the whole time) survive a crash just the same.  Guards
/// against a recovery path that only works for a single contiguous batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bursty_load_then_crash_recovers_all() {
    let mut cluster = TestCluster::spawn(ClusterSpec {
        num_quick_nodes: 1,
        ..Default::default()
    })
    .await;
    let tenant = cluster.owned_tenant();

    let mut expected = 0;
    for _ in 0..3 {
        expected += drive_concurrent_writes(&cluster, 0, 10, 40).await; // 3 × 400
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    let seq = cluster.quick_nodes[0].node.replication().current_seq();
    assert_eq!(seq, expected);

    cluster.kill_quick_node(0).await;
    cluster.restart_quick_node(0).await;

    let recoverable =
        cluster.quick_nodes[0].recoverable_versioned(tenant, STRUCT_ID, seq);
    assert_eq!(
        recoverable,
        expected,
        "bursty-load crash recovery lost {} of {expected} writes",
        expected - recoverable
    );

    cluster.shutdown().await;
}
