//! 128-bit composite ID for `WaveDB` records.
//!
//! Layout:
//! ```text
//! [ TENANT_ID (u48) | SHARD_ID (u12) | STRUCT_ID (u20) | CREATED_AT (u48) ]
//! ```

/// The three data shapes recognised by `WaveDB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Shape {
    /// Exactly one live record per `(STRUCT_ID, TENANT_ID)`.
    Unique,
    /// Many live records per tenant.
    NonUnique,
    /// Many records tightly bound to a single parent `NonUnique` record.
    NestedNonUnique,
}

/// A 128-bit composite ID.
///
/// ```text
/// [ TENANT_ID (u48) | SHARD_ID (u12) | STRUCT_ID (u20) | CREATED_AT (u48) ]
/// ```
///
/// Pack/unpack accessors are `const` so macro-generated code can build
/// IDs at compile time when needed.
#[repr(transparent)]
#[derive(Copy, Clone, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Id(u128);

// Bit-field masks
const TENANT_BITS: u32 = 48;
const SHARD_BITS: u32 = 12;
const STRUCT_BITS: u32 = 20;
const CREATED_BITS: u32 = 48;

const TENANT_MASK: u128 = (1u128 << TENANT_BITS) - 1;
const SHARD_MASK: u128 = (1u128 << SHARD_BITS) - 1;
const STRUCT_MASK: u128 = (1u128 << STRUCT_BITS) - 1;
const CREATED_MASK: u128 = (1u128 << CREATED_BITS) - 1;

const TENANT_SHIFT: u32 = SHARD_BITS + STRUCT_BITS + CREATED_BITS; // 80
const SHARD_SHIFT: u32 = STRUCT_BITS + CREATED_BITS; // 68
const STRUCT_SHIFT: u32 = CREATED_BITS; // 48

impl Id {
    /// Reserved `TENANT_ID` for the database system itself.
    pub const SYSTEM_TENANT: u64 = 0;

    /// Reserved `TENANT_ID` for unauthenticated sessions (`U48::MAX`).
    pub const UNAUTHENTICATED_USER: u64 = (1u64 << 48) - 1;

    /// The zero ID — used as a sentinel for "no ID" / default.
    pub const ZERO: Self = Self(0);

    /// Construct a new `Id` from its four components.
    ///
    /// Each value is masked to its field width; overflow bits are silently
    /// truncated.
    ///
    /// # Examples
    ///
    /// ```
    /// use wavedb_core::Id;
    /// let id = Id::new(42, 0, 7, 1_000_000);
    /// assert_eq!(id.tenant_id(), 42);
    /// assert_eq!(id.shard_id(), 0);
    /// assert_eq!(id.struct_id(), 7);
    /// assert_eq!(id.created_at(), 1_000_000);
    /// ```
    #[must_use]
    pub const fn new(tenant: u64, shard: u16, struct_id: u32, created_at: u64) -> Self {
        let v = ((tenant as u128 & TENANT_MASK) << TENANT_SHIFT)
            | ((shard as u128 & SHARD_MASK) << SHARD_SHIFT)
            | ((struct_id as u128 & STRUCT_MASK) << STRUCT_SHIFT)
            | (created_at as u128 & CREATED_MASK);
        Self(v)
    }

    /// Extract the 48-bit tenant identifier.
    #[must_use]
    pub const fn tenant_id(self) -> u64 {
        ((self.0 >> TENANT_SHIFT) & TENANT_MASK) as u64
    }

    /// Extract the 12-bit shard identifier.
    #[must_use]
    pub const fn shard_id(self) -> u16 {
        ((self.0 >> SHARD_SHIFT) & SHARD_MASK) as u16
    }

    /// Extract the 20-bit struct type identifier.
    #[must_use]
    pub const fn struct_id(self) -> u32 {
        ((self.0 >> STRUCT_SHIFT) & STRUCT_MASK) as u32
    }

    /// Extract the 48-bit creation timestamp (100µs ticks since epoch).
    #[must_use]
    pub const fn created_at(self) -> u64 {
        (self.0 & CREATED_MASK) as u64
    }

    /// Return the raw 128-bit representation.
    #[must_use]
    pub const fn raw(self) -> u128 {
        self.0
    }

    /// Construct an `Id` from a raw 128-bit value.
    #[must_use]
    pub const fn from_raw(raw: u128) -> Self {
        Self(raw)
    }

    /// Build the anchor key for this ID (same as the ID but with
    /// `created_at = 0`). This is the synthetic primary anchor address.
    #[must_use]
    pub const fn anchor_key(self) -> Self {
        Self(self.0 & !CREATED_MASK)
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::ZERO
    }
}

impl std::fmt::Debug for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Id")
            .field("tenant_id", &self.tenant_id())
            .field("shard_id", &self.shard_id())
            .field("struct_id", &self.struct_id())
            .field("created_at", &self.created_at())
            .finish()
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Id(t={}, s={}, sid={}, c={})",
            self.tenant_id(),
            self.shard_id(),
            self.struct_id(),
            self.created_at()
        )
    }
}

impl PartialOrd for Id {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Id {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_id() {
        let id = Id::ZERO;
        assert_eq!(id.tenant_id(), 0);
        assert_eq!(id.shard_id(), 0);
        assert_eq!(id.struct_id(), 0);
        assert_eq!(id.created_at(), 0);
        assert_eq!(id.raw(), 0);
    }

    #[test]
    fn basic_roundtrip() {
        let id = Id::new(42, 7, 100, 999_999);
        assert_eq!(id.tenant_id(), 42);
        assert_eq!(id.shard_id(), 7);
        assert_eq!(id.struct_id(), 100);
        assert_eq!(id.created_at(), 999_999);
    }

    #[test]
    fn max_values() {
        let max_t = (1u64 << 48) - 1;
        let max_s = (1u16 << 12) - 1;
        let max_sid = (1u32 << 20) - 1;
        let max_c = (1u64 << 48) - 1;
        let id = Id::new(max_t, max_s, max_sid, max_c);
        assert_eq!(id.tenant_id(), max_t);
        assert_eq!(id.shard_id(), max_s);
        assert_eq!(id.struct_id(), max_sid);
        assert_eq!(id.created_at(), max_c);
        // All bits set
        assert_eq!(id.raw(), u128::MAX);
    }

    #[test]
    fn overflow_truncation() {
        // Values exceeding field width should be silently truncated
        let id = Id::new(u64::MAX, u16::MAX, u32::MAX, u64::MAX);
        assert!(id.tenant_id() < (1 << 48));
        assert!(id.shard_id() < (1 << 12));
        assert!(id.struct_id() < (1 << 20));
        assert!(id.created_at() < (1 << 48));
    }

    #[test]
    fn anchor_key_zeroes_created_at() {
        let id = Id::new(42, 7, 100, 999_999);
        let anchor = id.anchor_key();
        assert_eq!(anchor.tenant_id(), 42);
        assert_eq!(anchor.shard_id(), 7);
        assert_eq!(anchor.struct_id(), 100);
        assert_eq!(anchor.created_at(), 0);
    }

    #[test]
    fn reserved_constants() {
        assert_eq!(Id::SYSTEM_TENANT, 0);
        assert_eq!(Id::UNAUTHENTICATED_USER, (1u64 << 48) - 1);
    }

    #[test]
    fn debug_display() {
        let id = Id::new(1, 2, 3, 4);
        let debug = format!("{id:?}");
        assert!(debug.contains("tenant_id: 1"));
        let display = format!("{id}");
        assert!(display.contains("t=1"));
    }

    #[test]
    fn ordering() {
        let a = Id::new(1, 0, 0, 0);
        let b = Id::new(2, 0, 0, 0);
        assert!(a < b);
    }

    #[test]
    fn postcard_roundtrip() {
        let id = Id::new(42, 7, 100, 999_999);
        let bytes = postcard::to_allocvec(&id).unwrap();
        let decoded: Id = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(id, decoded);
    }
}
