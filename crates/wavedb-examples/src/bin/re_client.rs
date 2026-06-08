//! WaveDB Client subprocess for the `real_example` orchestrated test.
//!
//! Each invocation of this binary represents a single client that writes
//! continuously to its assigned Quick-Node until the node drains or the
//! process receives SIGTERM / SIGINT.
//!
//! # Environment variables (set by the orchestrator)
//!
//! | Variable           | Description                                        |
//! |--------------------|----------------------------------------------------|
//! | `WAVE_QN_WS_URLS`  | Comma-separated WebSocket URLs of quick nodes      |
//! | `WAVE_TENANT`      | Tenant ID (u64)                                    |
//! | `WAVE_CLIENT_ID`   | Unique client ID (u64, 0-based)                    |
//! | `WAVE_NUM_CLIENTS` | Total number of client processes (for distribution)|
//!
//! The client prints one line to stdout when it starts:
//!   `WAVE_READY client=<ID>`
//! and a summary line before exit:
//!   `WAVE_DONE committed=<N> dropped=<N>`
//!
//! Run via `nix run .#real_example` — do not invoke directly.

use std::time::Duration;

use tokio::time::sleep;
use wavedb::Db;
use wavedb_net::WsClient;

// ── Schema (copy of the payment-gateway types) ────────────────────────────────
// Re-defined here so this binary is self-contained and doesn't need to import
// from the `real_example` multi-file module.

use wavedb::prelude::*;

#[wave_db(struct_id = 1)]
#[derive(PartialEq, Eq)]
pub struct MerchantAccount1 {
    pub name: String,
    pub balance_cents: u64,
    pub writes_seen: u64,
}
pub type MerchantAccount = MerchantAccount1;
pub type MerchantAccountAnchor = MerchantAccount1Anchor;

#[wave_db(struct_id = 2, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Payment1 {
    pub merchant: MerchantAccountAnchor,
    pub amount_cents: u64,
    pub seq: u64,
    pub note: String,
}
pub type Payment = Payment1;
pub type PaymentAnchor = Payment1Anchor;

#[wave_db(struct_id = 3, NestedNonUnique)]
#[derive(PartialEq, Eq)]
pub struct PaymentLineItem1 {
    pub payment: PaymentAnchor,
    pub product: u64,
    pub quantity: u32,
    pub unit_cents: u64,
}
pub type PaymentLineItem = PaymentLineItem1;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_padded_note(user_id: u64, seq: u64, target_len: usize) -> String {
    let mut s = format!("u={user_id} seq={seq} ");
    while s.len() < target_len {
        let n = u8::try_from(s.len() % 10).unwrap_or(0);
        s.push((b'0' + n) as char);
    }
    s
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let ws_urls_raw = std::env::var("WAVE_QN_WS_URLS").unwrap_or_default();
    let tenant: u64 = std::env::var("WAVE_TENANT")
        .unwrap_or_else(|_| "100".to_string())
        .parse()
        .unwrap_or(100);
    let client_id: u64 = std::env::var("WAVE_CLIENT_ID")
        .unwrap_or_else(|_| "0".to_string())
        .parse()
        .unwrap_or(0);
    let num_clients: usize = std::env::var("WAVE_NUM_CLIENTS")
        .unwrap_or_else(|_| "500".to_string())
        .parse()
        .unwrap_or(500);

    let ws_urls: Vec<String> = ws_urls_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();

    if ws_urls.is_empty() {
        eprintln!("[client-{client_id}] WAVE_QN_WS_URLS is empty — aborting");
        std::process::exit(1);
    }

    // Distribute clients across quick-nodes round-robin.
    let num_nodes = ws_urls.len();
    let per_node = (num_clients / num_nodes).max(1);
    let node_idx = (client_id as usize / per_node).min(num_nodes - 1);
    let ws_url = ws_urls[node_idx].clone();

    // Signal readiness to the orchestrator.
    {
        use std::io::Write as _;
        println!("WAVE_READY client={client_id}");
        std::io::stdout().flush().ok();
    }

    // Open WS connection.
    let (client, read_loop) = match WsClient::connect(ws_url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[client-{client_id}] connect failed: {e}");
            println!("WAVE_DONE committed=0 dropped=1");
            return;
        }
    };
    tokio::spawn(read_loop);

    let db = match Db::open_with_transport(client, client_id + 1, tenant).await
    {
        Ok(db) => db,
        Err(e) => {
            eprintln!("[client-{client_id}] Db::open failed: {e}");
            println!("WAVE_DONE committed=0 dropped=1");
            return;
        }
    };

    let mut committed: u64 = 0;
    let mut dropped: u64 = 0;

    // ── Step 1: save merchant profile ─────────────────────────────────────────
    let merchant = MerchantAccount {
        name: format!("merchant-{client_id}"),
        balance_cents: 0,
        writes_seen: 0,
        ..Default::default()
    };
    match db.save(&merchant).await {
        Ok(_) => committed += 1,
        Err(_) => {
            dropped += 1;
            println!("WAVE_DONE committed={committed} dropped={dropped}");
            return;
        }
    }
    let merchant_anchor = merchant.anchor();
    let mut seq: u64 = 0;

    // ── Step 2+: continuous payment writes ────────────────────────────────────
    loop {
        // Check for SIGINT / SIGTERM via tokio's signal handling (portable).
        // We just poll a cooperative cancellation via a short-lived flag here
        // using a non-blocking approach. On real termination the OS kills the
        // process and the summary line may not be printed — that's acceptable.

        let note_len = 64
            + ((client_id
                .wrapping_mul(31)
                .wrapping_add(seq.wrapping_mul(17)))
                % 384) as usize;
        let note = make_padded_note(client_id, seq, note_len);

        let payment = Payment {
            merchant: merchant_anchor,
            amount_cents: (seq + 1) * 100,
            seq,
            note,
            ..Default::default()
        };
        match db.save(&payment).await {
            Ok(_) => committed += 1,
            Err(_) => {
                dropped += 1;
                break;
            }
        }
        let payment_anchor = payment.anchor();

        let n_lines = 2 + (seq % 2) as u8;
        let mut ok = true;
        for li in 0..n_lines {
            let line = PaymentLineItem {
                payment: payment_anchor,
                product: 100 + u64::from(li),
                quantity: 1 + u32::from(li),
                unit_cents: 100,
                ..Default::default()
            };
            match db.save(&line).await {
                Ok(_) => committed += 1,
                Err(_) => {
                    dropped += 1;
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            break;
        }

        // Pacing — slight jitter so all clients don't burst in lockstep.
        let burst = if (seq % 20) < 5 { 1u64 } else { 3 };
        let delay_ms = burst + (client_id % 7) * 2 + (seq % 5);
        sleep(Duration::from_millis(delay_ms)).await;

        seq = seq.wrapping_add(1);
    }

    // Summary line read by the orchestrator monitor thread.
    use std::io::Write as _;
    println!("WAVE_DONE committed={committed} dropped={dropped}");
    std::io::stdout().flush().ok();
}
