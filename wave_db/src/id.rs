//! Primitive ID types.
//!
//! Every record has a composite 128-bit ID:
//!
//! ```text
//! [ TENANT_ID (u48) | SHARD_ID (u8) | STRUCT_ID (u16) | CREATED_AT (u48, 100µs) | SLIDER (u8) ]
//! ```
//!
//! Bit layout (MSB → LSB):
//! ```text
//! bits 127-80  TENANT_ID   (48 bits)
//! bits 79-72   SHARD_ID    ( 8 bits)
//! bits 71-56   STRUCT_ID   (16 bits)
//! bits 55-8    CREATED_AT  (48 bits)
//! bits 7-0     SLIDER      ( 8 bits)
//! ```

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ─── Primitive newtypes ───────────────────────────────────────────────────────

/// 48-bit tenant identifier. `0` is reserved for the database system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TenantId(u64); // stored in lower 48 bits

/// 8-bit shard identifier. Spreads high-volume `NonUnique` data across 256 shards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShardId(u8);

/// 16-bit struct/table identifier. Fixed at compile time per struct type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StructId(u16);

/// 48-bit creation timestamp at 100µs precision since [`WAVE_EPOCH`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64); // stored in lower 48 bits

/// 8-bit auto-increment / random collision slider within a 100µs tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slider(u8);

// ─── Custom epoch ─────────────────────────────────────────────────────────────

/// WaveDB epoch: 2025-01-01 00:00:00 UTC.
///
/// Using a recent epoch maximises the useful range of the 48-bit timestamp
/// (~890 years from epoch at 100µs precision).
///
/// `SystemTime::Add` is not const-stable, so this is a function.
#[inline]
pub fn wave_epoch() -> SystemTime {
    // 2025-01-01T00:00:00Z = 1735689600 seconds after Unix epoch.
    UNIX_EPOCH + Duration::from_secs(1_735_689_600)
}

const U48_MASK: u64 = (1u64 << 48) - 1;

// ─── TenantId ─────────────────────────────────────────────────────────────────

impl TenantId {
    /// System tenant (database internals).
    pub const SYSTEM: Self = Self(0);

    /// Create a new tenant ID. Values larger than 2^48-1 are masked.
    #[inline]
    pub const fn new(v: u64) -> Self {
        Self(v & U48_MASK)
    }

    /// Raw 48-bit value.
    #[inline]
    pub const fn value(self) -> u64 {
        self.0
    }

    /// Returns `true` if this is the reserved system tenant.
    #[inline]
    pub const fn is_system(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TenantId({:#014x})", self.0)
    }
}

// ─── ShardId ──────────────────────────────────────────────────────────────────

impl ShardId {
    /// Derive a shard ID by hashing a u64 property into the 0..=255 range.
    #[inline]
    pub const fn from_property(v: u64) -> Self {
        // Simple but stable: use the lower 8 bits after a cheap mix.
        let mixed = v.wrapping_mul(0x517c_c1b7_27220_a95).wrapping_add(v >> 32);
        Self(mixed as u8)
    }

    /// Create from a raw byte.
    #[inline]
    pub const fn new(v: u8) -> Self {
        Self(v)
    }

    #[inline]
    pub const fn value(self) -> u8 {
        self.0
    }
}

impl fmt::Display for ShardId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ShardId({})", self.0)
    }
}

// ─── StructId ─────────────────────────────────────────────────────────────────

impl StructId {
    /// Reserved for the database system (`Permissions`, etc.).
    pub const SYSTEM: Self = Self(0);
    /// Reserved STRUCT_ID = 1 for `Permissions` records (see P14).
    pub const PERMISSIONS: Self = Self(1);

    #[inline]
    pub const fn new(v: u16) -> Self {
        Self(v)
    }

    #[inline]
    pub const fn value(self) -> u16 {
        self.0
    }
}

impl fmt::Display for StructId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StructId({})", self.0)
    }
}

// ─── Timestamp ────────────────────────────────────────────────────────────────

impl Timestamp {
    /// Capture the current time as a WaveDB timestamp.
    ///
    /// Returns an error if the system clock is before [`WAVE_EPOCH`].
    pub fn now() -> Result<Self, TimestampError> {
        let since_epoch = SystemTime::now()
            .duration_since(wave_epoch())
            .map_err(|_| TimestampError::ClockBeforeEpoch)?;
        let ticks = since_epoch.as_micros() / 100; // 100µs precision
        if ticks > U48_MASK as u128 {
            return Err(TimestampError::Overflow);
        }
        Ok(Self(ticks as u64))
    }

    /// Create from a raw 48-bit tick value.
    #[inline]
    pub const fn from_ticks(ticks: u64) -> Self {
        Self(ticks & U48_MASK)
    }

    /// Raw tick count (100µs units since WAVE_EPOCH).
    #[inline]
    pub const fn ticks(self) -> u64 {
        self.0
    }

    /// Convert back to a [`SystemTime`].
    pub fn to_system_time(self) -> SystemTime {
        wave_epoch() + Duration::from_micros(self.0 * 100)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Timestamp({} ticks)", self.0)
    }
}

/// Errors that can occur when creating a [`Timestamp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimestampError {
    /// System clock is before the WaveDB epoch.
    ClockBeforeEpoch,
    /// Clock value exceeds the 48-bit range (~890 years from epoch).
    Overflow,
}

impl fmt::Display for TimestampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClockBeforeEpoch => write!(f, "system clock is before WaveDB epoch"),
            Self::Overflow => write!(f, "timestamp exceeds 48-bit range"),
        }
    }
}

impl std::error::Error for TimestampError {}

// ─── Slider ───────────────────────────────────────────────────────────────────

impl Slider {
    #[inline]
    pub const fn new(v: u8) -> Self {
        Self(v)
    }

    #[inline]
    pub const fn value(self) -> u8 {
        self.0
    }

    /// Increment the slider (wraps on overflow).
    #[inline]
    pub fn increment(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

impl fmt::Display for Slider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Slider({})", self.0)
    }
}

// ─── Composite Id ─────────────────────────────────────────────────────────────

/// 128-bit composite record identifier.
///
/// ```text
/// bits 127-80  TENANT_ID   (48 bits)
/// bits 79-72   SHARD_ID    ( 8 bits)
/// bits 71-56   STRUCT_ID   (16 bits)
/// bits 55-8    CREATED_AT  (48 bits)
/// bits 7-0     SLIDER      ( 8 bits)
/// ```
///
/// The zero value (`Id::ZERO`) is the null / unset ID.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Id(u128);

impl Id {
    /// The null ID (all zeros).
    pub const ZERO: Self = Self(0);

    /// Assemble an `Id` from its component fields.
    #[inline]
    pub const fn new(
        tenant: TenantId,
        shard: ShardId,
        struct_id: StructId,
        created_at: Timestamp,
        slider: Slider,
    ) -> Self {
        let raw: u128 = ((tenant.0 as u128) << 80)
            | ((shard.0 as u128) << 72)
            | ((struct_id.0 as u128) << 56)
            | ((created_at.0 as u128) << 8)
            | (slider.0 as u128);
        Self(raw)
    }

    /// Generate a new ID using the current wall clock.
    ///
    /// `slider` should be caller-managed (auto-increment or random per device).
    pub fn generate(
        tenant: TenantId,
        shard: ShardId,
        struct_id: StructId,
        slider: Slider,
    ) -> Result<Self, TimestampError> {
        let ts = Timestamp::now()?;
        Ok(Self::new(tenant, shard, struct_id, ts, slider))
    }

    // ── Field accessors ──────────────────────────────────────────────────────

    #[inline]
    pub fn tenant_id(self) -> TenantId {
        TenantId(((self.0 >> 80) as u64) & U48_MASK)
    }

    #[inline]
    pub fn shard_id(self) -> ShardId {
        ShardId((self.0 >> 72) as u8)
    }

    #[inline]
    pub fn struct_id(self) -> StructId {
        StructId((self.0 >> 56) as u16)
    }

    #[inline]
    pub fn created_at(self) -> Timestamp {
        Timestamp(((self.0 >> 8) as u64) & U48_MASK)
    }

    #[inline]
    pub fn slider(self) -> Slider {
        Slider(self.0 as u8)
    }

    /// Raw 128-bit representation.
    #[inline]
    pub const fn raw(self) -> u128 {
        self.0
    }

    /// Reconstruct an `Id` from its raw 128-bit form.
    #[inline]
    pub const fn from_raw(v: u128) -> Self {
        Self(v)
    }

    /// Returns `true` if this is the null / zero ID.
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Id {{ tenant={}, shard={}, struct={}, ts={}, slider={} }}",
            self.tenant_id(),
            self.shard_id(),
            self.struct_id(),
            self.created_at(),
            self.slider(),
        )
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#034x}", self.0)
    }
}

impl From<u128> for Id {
    fn from(v: u128) -> Self {
        Self(v)
    }
}

impl From<Id> for u128 {
    fn from(id: Id) -> Self {
        id.0
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_fields() {
        let tenant = TenantId::new(0x_AABB_CCDD_EEFF);
        let shard = ShardId::new(42);
        let struct_id = StructId::new(7);
        let ts = Timestamp::from_ticks(123_456_789);
        let slider = Slider::new(255);

        let id = Id::new(tenant, shard, struct_id, ts, slider);

        assert_eq!(id.tenant_id(), tenant, "tenant_id roundtrip");
        assert_eq!(id.shard_id(), shard, "shard_id roundtrip");
        assert_eq!(id.struct_id(), struct_id, "struct_id roundtrip");
        assert_eq!(id.created_at(), ts, "created_at roundtrip");
        assert_eq!(id.slider(), slider, "slider roundtrip");
    }

    #[test]
    fn zero_id_is_zero() {
        assert!(Id::ZERO.is_zero());
        assert_eq!(Id::ZERO.raw(), 0);
    }

    #[test]
    fn from_raw_roundtrip() {
        let raw: u128 = 0x_DEAD_BEEF_CAFE_1234_5678_9ABC_DEF0_0001;
        let id = Id::from_raw(raw);
        assert_eq!(id.raw(), raw);
    }

    #[test]
    fn timestamp_now_is_positive() {
        let ts = Timestamp::now().expect("clock should be after epoch");
        assert!(ts.ticks() > 0);
    }

    #[test]
    fn id_generate() {
        let id = Id::generate(
            TenantId::new(1),
            ShardId::new(0),
            StructId::new(2),
            Slider::new(0),
        )
        .expect("generate should succeed");
        assert!(!id.is_zero());
        assert_eq!(id.tenant_id(), TenantId::new(1));
        assert_eq!(id.struct_id(), StructId::new(2));
    }
}
