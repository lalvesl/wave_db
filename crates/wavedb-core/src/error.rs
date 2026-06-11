//! Workspace-wide error type for `WaveDB`.

/// The unified error type used across all `WaveDB` crates.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An I/O error from the underlying filesystem or network.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A wire-format encode/decode failure.
    #[error("wire error: {0}")]
    Wire(#[from] crate::wire::WireError),

    /// The `struct_id` value exceeds the 20-bit limit.
    #[error("struct_id {0} does not fit in u20 (max 1048575)")]
    StructIdOverflow(u32),

    /// The `tenant_id` value exceeds the 48-bit limit.
    #[error("tenant_id {0} does not fit in u48")]
    TenantIdOverflow(u64),

    /// The `shard_id` value exceeds the 12-bit limit.
    #[error("shard_id {0} does not fit in u12")]
    ShardIdOverflow(u16),

    /// The `created_at` value exceeds the 48-bit limit.
    #[error("created_at {0} does not fit in u48")]
    CreatedAtOverflow(u64),

    /// An anchor lookup resolved to a secondary that pointed nowhere.
    #[error("orphan secondary anchor detected")]
    OrphanSecondary,

    /// Page checksum mismatch.
    #[error(
        "page checksum mismatch: expected {expected:#010x}, got {actual:#010x}"
    )]
    ChecksumMismatch {
        /// The checksum stored in the page header.
        expected: u32,
        /// The checksum recomputed from the page bytes.
        actual: u32,
    },

    /// The page is full and double-hashing is needed.
    #[error("page full, double-hashing triggered")]
    PageFull,

    /// A record was not found at the expected location.
    #[error("record not found")]
    NotFound,

    /// A permission check failed.
    #[error("permission denied for user {user} on tenant {tenant}")]
    PermissionDenied {
        /// The user who attempted the operation.
        user: u64,
        /// The tenant that owns the record.
        tenant: u64,
    },

    /// A migration path could not be resolved.
    #[error("no migration path from version {from} to {to}")]
    NoMigrationPath {
        /// Source version.
        from: u8,
        /// Target version.
        to: u8,
    },

    /// Serialization failed inside a type-erased migration fn.
    #[error("migration serialization failed: {0}")]
    MigrationSer(String),

    /// Deserialization failed inside a type-erased migration fn.
    #[error("migration deserialization failed: {0}")]
    MigrationDe(String),

    /// A transport-level failure (connection refused, timeout, etc.).
    #[error("transport error: {0}")]
    Transport(String),

    /// Generic catch-all for cases not covered above.
    #[error("{0}")]
    Other(String),
}

/// Convenience alias used throughout `WaveDB`.
pub type Result<T> = std::result::Result<T, Error>;
