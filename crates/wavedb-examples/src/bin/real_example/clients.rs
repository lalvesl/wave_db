//! Continuous payment-processor client load generation.
//!
//! Each client is a tokio task that loops indefinitely, writing typed
//! `#[wave_db]` records of all three shapes (`Unique`, `NonUnique`,
//! `NestedNonUnique`) through the typed `Db::save` sugar.  When a node
//! drains or errors the client exits its loop (its task ends naturally).
//!
//! ## Per-iteration write pattern
//!
//! 1. First iteration only — save the [`MerchantAccount`] (Unique).
//! 2. Every iteration — save a [`Payment`] (NonUnique) anchored to the
//!    merchant.
//! 3. Every iteration — save 2–3 [`PaymentLineItem`]s (NestedNonUnique)
//!    anchored to the freshly-saved payment.
//!
//! Roughly 4 writes per client per loop turn, exercising all routing
//! strategies in the storage layer (`tuple2` for the merchant + payment
//! tracker, `tuple4` for payment elements and nested line items).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::sleep;

use wavedb_test_cluster::TestCluster;

use crate::TENANT;
use crate::schema::{MerchantAccount, Payment, PaymentLineItem};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Total concurrent clients (divisible by 3 for even distribution).
///
/// 13 per Quick-Node — small enough that an in-process `TestCluster` can
/// open every WS connection in well under a second.
pub const NUM_CLIENTS: usize = 3900;
pub const PER_NODE: usize = NUM_CLIENTS / 3;

// ── Counters ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Counters {
    pub committed: Arc<AtomicU64>,
    pub dropped: Arc<AtomicU64>,
}

impl Counters {
    pub fn new() -> Self {
        Self {
            committed: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }
    pub fn total_committed(&self) -> u64 {
        self.committed.load(Ordering::Relaxed)
    }
    pub fn total_dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

// ── Launch ────────────────────────────────────────────────────────────────────

/// Connect all [`NUM_CLIENTS`] clients and start continuous typed writes.
///
/// Each client task loops until:
/// - its node returns `draining` or any save errors (task exits), or
/// - the task is externally aborted (call `.abort()` on the handle).
///
/// Per-client write delay: `base_ms + jitter_ms` where both factors depend
/// on `user_id` so all clients have distinct duty cycles.
pub async fn launch_continuous(cluster: &TestCluster) -> (Vec<JoinHandle<()>>, Counters) {
    let counters = Counters::new();
    let mut tasks = Vec::with_capacity(NUM_CLIENTS);

    #[allow(clippy::cast_possible_truncation)]
    for user_id in 0u64..NUM_CLIENTS as u64 {
        let node_idx = (user_id as usize / PER_NODE).min(2);
        let db = cluster.open_user_via(user_id + 1, TENANT, node_idx).await;
        let ctr = counters.clone();

        tasks.push(tokio::spawn(async move {
            // ── Step 1: Unique save — the merchant profile ────────────────
            //
            // One MerchantAccount per (tenant, user).  Saved once at
            // start; routing uses `tuple2_page(struct_id, tenant)`.
            let merchant = MerchantAccount {
                name: format!("merchant-{user_id}"),
                balance_cents: 0,
                writes_seen: 0,
                ..Default::default()
            };
            if db.save(&merchant).await.is_err() {
                ctr.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            ctr.committed.fetch_add(1, Ordering::Relaxed);
            let merchant_anchor = merchant.anchor();

            let mut seq: u64 = 0;

            loop {
                // ── Step 2: NonUnique save — a payment record ─────────────
                //
                // Routing uses `tuple4_page` over the element's full Id,
                // so each `seq` lands on a fresh page slot.
                let note_len = 64
                    + ((user_id.wrapping_mul(31).wrapping_add(seq.wrapping_mul(17))) % 384)
                        as usize;
                let note = make_padded_note(user_id, seq, note_len);

                let payment = Payment {
                    merchant: merchant_anchor,
                    amount_cents: (seq + 1) * 100,
                    seq,
                    note,
                    ..Default::default()
                };
                if db.save(&payment).await.is_err() {
                    ctr.dropped.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                ctr.committed.fetch_add(1, Ordering::Relaxed);

                // Anchor of the payment we just saved — used to bind
                // line-items to it as their NestedNonUnique parent.
                let payment_anchor = payment.anchor();

                // ── Step 3: NestedNonUnique saves — 2 or 3 line items ─────
                //
                // Routing also uses `tuple4_page`.  Nested elements share
                // the same routing as NonUnique elements per the design
                // (see the `PageKey` strategy table).
                let n_lines = 2 + (seq % 2) as u8; // 2 or 3 per payment
                let mut ok = true;
                for li in 0..n_lines {
                    let line = PaymentLineItem {
                        payment: payment_anchor,
                        product: 100 + u64::from(li),
                        quantity: 1 + u32::from(li),
                        unit_cents: 100,
                        ..Default::default()
                    };
                    if db.save(&line).await.is_err() {
                        ctr.dropped.fetch_add(1, Ordering::Relaxed);
                        ok = false;
                        break;
                    }
                    ctr.committed.fetch_add(1, Ordering::Relaxed);
                }
                if !ok {
                    break;
                }

                // ── Pacing — same jitter shape as the original load ───────
                let burst = if (seq % 20) < 5 { 1u64 } else { 3 };
                let delay_ms = burst + (user_id % 7) * 2 + (seq % 5);
                sleep(Duration::from_millis(delay_ms)).await;

                seq = seq.wrapping_add(1);
            }
        }));
    }

    (tasks, counters)
}

/// Build a deterministic ASCII note string padded to `target_len` bytes.
///
/// Replaces the hand-rolled JSON byte-builder from the previous version —
/// `Payment.note` is `String` because the record is serde-encoded by the
/// macro, so we just construct a regular String.
fn make_padded_note(user_id: u64, seq: u64, target_len: usize) -> String {
    let mut s = format!("u={user_id} seq={seq} ");
    while s.len() < target_len {
        // Cycle through digits so the payload is non-trivial to compress.
        let n = u8::try_from(s.len() % 10).unwrap_or(0);
        s.push((b'0' + n) as char);
    }
    s
}
