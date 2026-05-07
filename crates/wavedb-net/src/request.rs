//! Request and response types for the `WaveDB` transport layer.

use serde::{Deserialize, Serialize};

/// The kind of operation being requested.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RequestKind {
    /// Open / establish a session. Returns the owner and backup Quick-Node URLs.
    Connect {
        /// The authenticating user.
        user: u64,
        /// The tenant this session targets.
        tenant: u64,
    },
    /// Look up a Unique record.
    SearchUnique {
        /// Struct family identifier.
        struct_id: u32,
        /// User making the request.
        user: u64,
        /// Tenant scope.
        tenant: u64,
    },
    /// Query NonUnique records with an optional filter expression.
    QueryNonUnique {
        /// Struct family identifier.
        struct_id: u32,
        /// User making the request.
        user: u64,
        /// Tenant scope.
        tenant: u64,
        /// Serialised filter expression (postcard-encoded).
        filter: Vec<u8>,
    },
    /// Write (create or update) a record.
    Write {
        /// Struct family identifier.
        struct_id: u32,
        /// User making the request.
        user: u64,
        /// Tenant scope.
        tenant: u64,
        /// postcard-encoded record bytes.
        payload: Vec<u8>,
    },
    /// Delete a NonUnique record (tombstone it).
    Delete {
        /// The raw 128-bit record ID.
        id: u128,
        /// User making the request.
        user: u64,
        /// Tenant scope.
        tenant: u64,
    },
    /// Gracefully end a session.
    Disconnect {
        /// User whose session is ending.
        user: u64,
        /// Tenant scope.
        tenant: u64,
    },
}

/// A request sent from the client to the Quick-Node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportRequest {
    /// Monotonically increasing per-session sequence number.
    pub seq: u64,
    /// The operation to perform.
    pub kind: RequestKind,
}

impl TransportRequest {
    /// Construct a new request with the given sequence number and kind.
    pub const fn new(seq: u64, kind: RequestKind) -> Self {
        Self { seq, kind }
    }
}

/// A response returned from the Quick-Node to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportResponse {
    /// Echoed sequence number from the corresponding request.
    pub seq: u64,
    /// Response payload (postcard-encoded, schema depends on the request kind).
    pub payload: Vec<u8>,
    /// Owner Quick-Node URL (only filled on `Connect` responses).
    pub owner_url: Option<String>,
    /// Backup Quick-Node URL (only filled on `Connect` responses).
    pub backup_url: Option<String>,
    /// Piggyback notifications — object-changed events for the UI (may be empty).
    pub notifications: Vec<Notification>,
}

/// An object-changed notification piggybacked onto a response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    /// The raw 128-bit anchor ID that changed.
    pub anchor_id: u128,
    /// `true` if the object was deleted (tombstone), `false` if updated.
    pub deleted: bool,
    /// Optional postcard-encoded payload for the new live data (inline-mode anchors).
    pub payload: Option<Vec<u8>>,
}
