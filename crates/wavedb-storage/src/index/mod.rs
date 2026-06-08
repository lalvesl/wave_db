//! Adaptive per-`(STRUCT_ID, TENANT_ID)` indexes.
//!
//! Indexes start as a cache-friendly contiguous array and atomically convert
//! to a page-aligned B+ tree when the collection exceeds the threshold.
//! Every index entry points to an **anchor**, never to a versioned record.

pub mod adaptive;
pub mod array;
pub mod btree;
pub mod discrete;

pub use adaptive::AdaptiveIndex;
pub use array::ArrayIndex;
pub use btree::BTreeIndex;
pub use discrete::DiscreteIndex;

use crate::anchor::AnchorKey;
use std::ops::Range;

/// Default array → B+ tree conversion threshold.
pub const DEFAULT_MAX_NON_UNIQUE_ELEMENTS: u32 = 50;

/// A key used for index lookups — wraps a `u64` for ordered comparisons.
///
/// For `created_at`-ordered indexes this is the timestamp.
/// For property-ordered indexes this is a hash of the property value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexKey(pub u64);

impl IndexKey {
    /// Minimum possible key.
    pub const MIN: Self = Self(0);
    /// Maximum possible key.
    pub const MAX: Self = Self(u64::MAX);
}

/// The trait that both array and B+ tree backends implement.
pub trait IndexBackend {
    /// Insert a key→anchor mapping.
    fn insert(
        &mut self,
        key: IndexKey,
        anchor: AnchorKey,
    ) -> crate::StorageResult<()>;

    /// Point lookup by exact key. Returns the first match.
    fn lookup(&self, key: &IndexKey) -> Option<AnchorKey>;

    /// Remove the entry for the given key. Returns `true` if found.
    fn remove(&mut self, key: &IndexKey) -> bool;

    /// Iterate all entries in the given key range, in sorted order.
    fn range(&self, range: Range<IndexKey>) -> Vec<(IndexKey, AnchorKey)>;

    /// Number of entries in the index.
    fn len(&self) -> usize;

    /// Whether the index is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return all entries in sorted order.
    fn all_sorted(&self) -> Vec<(IndexKey, AnchorKey)> {
        self.range(IndexKey::MIN..IndexKey::MAX)
    }
}
