//! Typed schema for the payment-gateway scenario.
//!
//! All three `#[wave_db]` shapes are exercised so the real_example load
//! covers every routing path through the storage engine:
//!
//! | Struct              | Shape              | struct_id | Role                                  |
//! |---------------------|--------------------|-----------|---------------------------------------|
//! | [`MerchantAccount`] | `Unique`           | 1         | One profile per `(tenant, user)`      |
//! | [`Payment`]         | `NonUnique`        | 2         | Many payments per tenant              |
//! | [`PaymentLineItem`] | `NestedNonUnique`  | 3         | Many line items per parent `Payment`  |
//!
//! Cross-references use the macro-generated `XxxAnchor` types so the link
//! survives every mutation of the target record (`CREATED_AT` is zeroed in
//! the anchor key at the type level).

use wavedb::prelude::*;

// ── MerchantAccount (Unique) ──────────────────────────────────────────────────

/// Top-level merchant profile — one per `(tenant, user)`.  Saved once at
/// client startup and re-saved when the balance changes.
#[wave_db(struct_id = 1)]
#[derive(PartialEq, Eq)]
pub struct MerchantAccount1 {
    /// Merchant display name.
    pub name: String,
    /// Accumulated balance in cents.
    pub balance_cents: u64,
    /// Total writes seen by this merchant (heartbeat).
    pub writes_seen: u64,
}
/// Stable alias — bump to `MerchantAccount2` on schema change.
pub type MerchantAccount = MerchantAccount1;
/// Stable anchor alias for cross-references.
pub type MerchantAccountAnchor = MerchantAccount1Anchor;

// ── Payment (NonUnique) ───────────────────────────────────────────────────────

/// One payment event.  Many per tenant; queryable at the top level.
#[wave_db(struct_id = 2, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Payment1 {
    /// Cross-reference to the issuing [`MerchantAccount`].
    ///
    /// Stored as an anchor (`CREATED_AT = 0`) so it survives merchant
    /// balance updates without an explicit history walk.
    pub merchant: MerchantAccountAnchor,
    /// Total payment amount in cents.
    pub amount_cents: u64,
    /// Per-client monotonic sequence — distinguishes payments by the same
    /// merchant on the same tick.
    pub seq: u64,
    /// Free-form note (used to fatten the payload to the target size).
    pub note: String,
}
/// Stable alias.
pub type Payment = Payment1;
/// Stable anchor alias.
pub type PaymentAnchor = Payment1Anchor;

// ── PaymentLineItem (NestedNonUnique) ─────────────────────────────────────────

/// One line item belonging to a parent [`Payment`].  Many per payment;
/// **not** queryable at the top level — line-item lookups go through the
/// parent payment's tracker.
#[wave_db(struct_id = 3, NestedNonUnique)]
#[derive(PartialEq, Eq)]
pub struct PaymentLineItem1 {
    /// Anchor of the parent [`Payment`].
    pub payment: PaymentAnchor,
    /// Product SKU.
    pub product: u64,
    /// Quantity sold.
    pub quantity: u32,
    /// Unit price in cents.
    pub unit_cents: u64,
}
/// Stable alias.
pub type PaymentLineItem = PaymentLineItem1;
