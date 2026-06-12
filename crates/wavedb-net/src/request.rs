//! Request and response types for the `WaveDB` transport layer.

use wavedb_macros::WaveWire;

/// The kind of operation being requested.
#[derive(Debug, Clone, WaveWire)]
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
        /// Serialised filter expression (wire-encoded).
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
        /// Wire-encoded record bytes.
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
#[derive(Debug, Clone, WaveWire)]
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

/// Machine-readable failure category carried in a [`NodeError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, WaveWire)]
pub enum ErrorCode {
    /// The record header named a `(struct_id, version)` the node's registry
    /// has not declared.  For this code, [`NodeError::struct_id`] carries the
    /// **full u32 header**, not just the family id.
    UnknownHeader,
    /// The payload bytes don't decode as the header's declared type.
    MalformedPayload,
    /// The struct's `validate` hook rejected the record.
    ValidationFailed,
    /// The struct's `preprocess` hook rejected the record.
    PreprocessFailed,
    /// The node's storage engine failed to commit the write.
    Storage,
}

/// A structured failure returned by a node in place of a response payload.
///
/// Carried in [`TransportResponse::error`]; the client maps it back to a
/// typed [`wavedb_core::Error`] so application code never string-matches.
#[derive(Debug, Clone, WaveWire)]
pub struct NodeError {
    /// Failure category.
    pub code: ErrorCode,
    /// Struct family the failure relates to (`0` when not applicable;
    /// the full header for [`ErrorCode::UnknownHeader`]).
    pub struct_id: u32,
    /// Offending field name, when the failure is attributable to one.
    pub field: Option<String>,
    /// Human-readable detail.
    pub message: String,
}

/// A response returned from the Quick-Node to the client.
#[derive(Debug, Clone, WaveWire)]
pub struct TransportResponse {
    /// Echoed sequence number from the corresponding request.
    pub seq: u64,
    /// Response payload (wire-encoded, schema depends on the request kind).
    pub payload: Vec<u8>,
    /// Owner Quick-Node URL (only filled on `Connect` responses).
    pub owner_url: Option<String>,
    /// Backup Quick-Node URL (only filled on `Connect` responses).
    pub backup_url: Option<String>,
    /// Piggyback notifications — object-changed events for the UI (may be empty).
    pub notifications: Vec<Notification>,
    /// Structured failure — `Some` means the request was rejected and
    /// `payload` is empty.
    pub error: Option<NodeError>,
}

impl TransportResponse {
    /// A successful response carrying `payload`.
    #[must_use]
    pub const fn ok(seq: u64, payload: Vec<u8>) -> Self {
        Self {
            seq,
            payload,
            owner_url: None,
            backup_url: None,
            notifications: Vec::new(),
            error: None,
        }
    }

    /// A rejection response carrying a structured [`NodeError`].
    #[must_use]
    pub const fn err(seq: u64, error: NodeError) -> Self {
        Self {
            seq,
            payload: Vec::new(),
            owner_url: None,
            backup_url: None,
            notifications: Vec::new(),
            error: Some(error),
        }
    }
}

/// An object-changed notification piggybacked onto a response.
#[derive(Debug, Clone, WaveWire)]
pub struct Notification {
    /// The raw 128-bit anchor ID that changed.
    pub anchor_id: u128,
    /// `true` if the object was deleted (tombstone), `false` if updated.
    pub deleted: bool,
    /// Optional wire-encoded payload for the new live data (inline-mode anchors).
    pub payload: Option<Vec<u8>>,
}
