//! End-to-end tests for the data-hook pipeline: client-side `validate`,
//! node-side `validate` re-check, and node-side `preprocess`.
//!
//! The scenario mirrors a real application: the same schema crate (here, the
//! structs below) is compiled into the client **and** into the Quick-Node
//! (via `ClusterSpec::registry`).  A well-behaved client never pays a
//! round-trip for an invalid record; a bypassing client (raw `db.send`) is
//! stopped at the node — the security boundary.

use wavedb::prelude::*;
use wavedb_net::request::RequestKind;
use wavedb_test_cluster::{ClusterSpec, TestCluster};

// ── Shared schema (what an app's schema crate would hold) ───────────────────

fn validate_payment(p: &Payment1) -> Result<(), ValidationError> {
    if p.amount_cents == 0 {
        return Err(ValidationError::field("amount_cents", "must be > 0"));
    }
    if p.amount_cents > 100_000_000 {
        return Err(ValidationError::field(
            "amount_cents",
            "exceeds single-payment limit",
        ));
    }
    Ok(())
}

#[allow(clippy::unnecessary_wraps)] // hook contract requires Result
fn preprocess_payment(p: &mut Payment1) -> Result<(), ValidationError> {
    // Server-authoritative normalisation: currency codes are stored
    // uppercase, references trimmed.
    p.currency = p.currency.trim().to_uppercase();
    p.reference = p.reference.trim().to_string();
    Ok(())
}

#[wave_db(
    struct_id = 7200,
    NonUnique,
    validate = validate_payment,
    preprocess = preprocess_payment,
)]
#[derive(PartialEq, Eq)]
pub struct Payment1 {
    pub amount_cents: u64,
    pub currency: String,
    pub reference: String,
}
pub type Payment = Payment1;

declare_objects! {
    pub mod app_objects {
        payments: [Payment1],
    }
}

fn payment(amount_cents: u64, currency: &str, reference: &str) -> Payment {
    Payment {
        id: Id::default(),
        metadata: Metadata::default(),
        amount_cents,
        currency: currency.into(),
        reference: reference.into(),
    }
}

fn hooked_spec() -> ClusterSpec {
    ClusterSpec {
        num_quick_nodes: 1,
        registry: Some(app_objects::REGISTRY),
        ..ClusterSpec::default()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// A valid record passes both the client pre-send check and the node's
/// re-check, and the write is committed.
#[tokio::test]
async fn valid_record_round_trips() {
    let cluster = TestCluster::spawn(hooked_spec()).await;
    let db = cluster.open_user(7, cluster.owned_tenant()).await;

    payment(4200, "brl", "  invoice-77  ")
        .save(&db)
        .await
        .expect("valid payment must commit");

    let metrics = cluster.quick_nodes[0].node.metrics();
    assert_eq!(metrics.write_count, 1);
    assert_eq!(metrics.rejected_count, 0);

    drop(db);
    cluster.shutdown().await;
}

/// An invalid record is rejected **client-side**: typed error, zero
/// round-trip, node never sees it.
#[tokio::test]
async fn invalid_record_fails_before_the_network() {
    let cluster = TestCluster::spawn(hooked_spec()).await;
    let db = cluster.open_user(7, cluster.owned_tenant()).await;

    let err = payment(0, "BRL", "x")
        .save(&db)
        .await
        .expect_err("amount=0 must fail locally");
    match err {
        wavedb_core::Error::Validation { struct_id, source } => {
            assert_eq!(struct_id, 7200);
            assert_eq!(source.field.as_deref(), Some("amount_cents"));
        }
        other => panic!("expected Validation, got {other:?}"),
    }

    let metrics = cluster.quick_nodes[0].node.metrics();
    assert_eq!(metrics.write_count, 0, "node must never see the record");
    assert_eq!(metrics.rejected_count, 0, "rejection happened client-side");

    drop(db);
    cluster.shutdown().await;
}

/// A client that bypasses the typed API (malicious or stale build) is
/// stopped by the node's own `validate` run — the security boundary.
#[tokio::test]
async fn bypassing_client_is_rejected_at_the_node() {
    let cluster = TestCluster::spawn(hooked_spec()).await;
    let db = cluster.open_user(7, cluster.owned_tenant()).await;

    // Hand-craft the wire payload exactly like `do_write`, skipping the
    // local validate call.
    let bad = payment(0, "BRL", "x");
    let body = wavedb_core::wire::to_wire(&bad).expect("wire");
    let mut payload = Vec::with_capacity(1 + body.len());
    payload.push(Payment1::STRUCT_VERSION);
    payload.extend_from_slice(&body);

    let err = db
        .send(RequestKind::Write {
            struct_id: Payment1::STRUCT_ID,
            user: db.user(),
            tenant: db.tenant(),
            payload,
        })
        .await
        .expect_err("node must reject what the client skipped");
    match err {
        wavedb_core::Error::Validation { struct_id, source } => {
            assert_eq!(struct_id, 7200);
            assert_eq!(source.field.as_deref(), Some("amount_cents"));
        }
        other => panic!("expected Validation, got {other:?}"),
    }

    let metrics = cluster.quick_nodes[0].node.metrics();
    assert_eq!(metrics.write_count, 0);
    assert_eq!(metrics.rejected_count, 1);

    drop(db);
    cluster.shutdown().await;
}

/// A struct family the node's registry never declared is refused outright.
#[tokio::test]
async fn undeclared_struct_is_rejected_at_the_node() {
    let cluster = TestCluster::spawn(hooked_spec()).await;
    let db = cluster.open_user(7, cluster.owned_tenant()).await;

    let err = db
        .send(RequestKind::Write {
            struct_id: 9999,
            user: db.user(),
            tenant: db.tenant(),
            payload: vec![1, 0xAA, 0xBB],
        })
        .await
        .expect_err("undeclared struct must be refused");
    assert!(matches!(
        err,
        wavedb_core::Error::UnknownHeader(h)
            if h == wavedb_core::wire::pack_header(9999, 1)
    ));

    drop(db);
    cluster.shutdown().await;
}

/// The node commits the **preprocessed** bytes: reading the WAL journal back
/// shows the normalised record, not what the client sent.
#[tokio::test]
#[allow(clippy::significant_drop_tightening)]
async fn preprocess_result_is_what_gets_stored() {
    let cluster = TestCluster::spawn(hooked_spec()).await;
    let db = cluster.open_user(7, cluster.owned_tenant()).await;

    payment(999, "  usd ", "  ref-1  ")
        .save(&db)
        .await
        .expect("valid payment");

    let storage = cluster.quick_nodes[0]
        .node
        .storage()
        .expect("cluster nodes run with on-disk storage")
        .clone();
    // Guard scoped so it drops before the shutdown await below.
    let stored: Payment1 = {
        let journal = storage.journal.lock();
        let entries = journal.all_entries();
        assert_eq!(entries.len(), 1);
        let wavedb_storage::pipeline::journal::JournalEntry::WriteVersioned {
            record,
        } = &entries[0]
        else {
            panic!("expected WriteVersioned");
        };
        assert_eq!(record.data[0], Payment1::STRUCT_VERSION);
        wavedb_core::wire::from_wire(&record.data[1..]).expect("decode")
    };
    assert_eq!(stored.currency, "USD", "currency normalised server-side");
    assert_eq!(stored.reference, "ref-1", "reference trimmed server-side");
    assert_eq!(stored.amount_cents, 999);

    drop(db);
    cluster.shutdown().await;
}
