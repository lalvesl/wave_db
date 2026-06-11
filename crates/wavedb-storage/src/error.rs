//! Storage-specific error types.

/// Errors specific to the storage engine.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// An I/O error from the underlying filesystem.
    #[error("storage I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization error (wire format).
    #[error("serialization error: {0}")]
    Serialization(#[from] wavedb_core::WireError),

    /// Page checksum mismatch.
    #[error(
        "checksum mismatch on page {page_index}: expected {expected:#010x}, got {actual:#010x}"
    )]
    ChecksumMismatch {
        /// Index of the page with the bad checksum.
        page_index: u64,
        /// The checksum stored in the page header.
        expected: u32,
        /// The checksum recomputed from the page bytes.
        actual: u32,
    },

    /// The target page is full; double-hashing should be tried.
    #[error("page {0} is full")]
    PageFull(u64),

    /// A record was not found at the expected hash location.
    #[error("record not found")]
    NotFound,

    /// The data file needs to grow (rebalance trigger).
    #[error(
        "data file approaching capacity ({fill_ratio:.1}%), rebalance needed"
    )]
    RebalanceNeeded {
        /// Current fill ratio as a percentage.
        fill_ratio: f64,
    },

    /// Anchor resolution hit a secondary that pointed to nothing.
    #[error("orphan secondary anchor")]
    OrphanSecondary,

    /// A generic storage error.
    #[error("{0}")]
    Other(String),
}

/// Convenience alias.
pub type StorageResult<T> = std::result::Result<T, StorageError>;
