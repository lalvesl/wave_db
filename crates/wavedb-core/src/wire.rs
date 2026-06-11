//! WaveDB's own wire format — replaces serde + postcard.
//!
//! See `docs/wire_format.md` for the full specification. The short version:
//!
//! ```text
//! [ STACK section — exactly T::STACK_SIZE bytes, compile-time constant ]
//! [ HEAP section  — variable bytes, length = value.heap_size()          ]
//! ```
//!
//! Fixed-width fields pack little-endian into the stack section in
//! declaration order, no padding. Each dynamic field (String, Vec, enum with
//! payload) contributes a fixed slot to the stack section — a `u32`
//! heap-length plus flag/tag bytes — so every stack offset is a compile-time
//! constant, and the heap section is parsed sequentially from those slots.
//!
//! Serialisation allocates **once**:
//! `Vec::with_capacity(T::STACK_SIZE + value.heap_size())`.

/// Errors produced by wire encoding / decoding.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// Ran out of bytes while reading.
    #[error("unexpected end of wire data: needed {needed} bytes at offset {offset}, buffer is {len}")]
    UnexpectedEnd {
        /// Bytes the reader needed.
        needed: usize,
        /// Offset the read started at.
        offset: usize,
        /// Total buffer length.
        len: usize,
    },

    /// A heap region was larger than a `u32` length slot can describe.
    #[error("heap region of {0} bytes exceeds u32 length slot")]
    LengthOverflow(usize),

    /// A platform-width integer from the wire does not fit this target's
    /// pointer width (e.g. a 32-bit node decoding a value above `u32::MAX`).
    #[error(
        "platform-width integer {0:#x} does not fit usize/isize on this target"
    )]
    PlatformIntOverflow(u64),

    /// A `bool` byte was neither 0 nor 1.
    #[error("invalid bool byte: {0:#04x}")]
    InvalidBool(u8),

    /// A `char` slot held an invalid Unicode scalar value.
    #[error("invalid char scalar: {0:#010x}")]
    InvalidChar(u32),

    /// A `String` heap region was not valid UTF-8.
    #[error("invalid UTF-8 in string heap region")]
    InvalidUtf8,

    /// An enum tag byte did not match any variant.
    #[error("invalid enum tag {tag} for {type_name}")]
    InvalidTag {
        /// The type being decoded.
        type_name: &'static str,
        /// The offending tag byte.
        tag: u8,
    },

    /// A heap region's contents did not end exactly at the region boundary.
    #[error("malformed heap region: consumed {consumed} of {expected} bytes")]
    RegionMismatch {
        /// Bytes actually consumed.
        consumed: usize,
        /// Region length from the u32 slot.
        expected: usize,
    },

    /// The buffer had bytes left over after a strict top-level decode.
    #[error("trailing bytes after value: consumed {consumed} of {len}")]
    TrailingBytes {
        /// Bytes consumed by the value.
        consumed: usize,
        /// Total buffer length.
        len: usize,
    },
}

/// Result alias for wire operations.
pub type WireResult<T> = std::result::Result<T, WireError>;

// ─── Header ──────────────────────────────────────────────────────────────────

/// Pack a record header: `(struct_id as u24) << 8 | version`.
///
/// The registry is searchable by this `u32`. `struct_id` is validated
/// elsewhere to fit in u20, so the u24 field never truncates.
#[must_use]
pub const fn pack_header(struct_id: u32, version: u8) -> u32 {
    (struct_id << 8) | version as u32
}

/// Split a record header into `(struct_id, version)`.
#[must_use]
pub const fn unpack_header(header: u32) -> (u32, u8) {
    (header >> 8, (header & 0xFF) as u8)
}

/// Peek the `u32` header of a header-prefixed record buffer.
#[must_use]
pub fn peek_header(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?))
}

// ─── The trait ───────────────────────────────────────────────────────────────

/// WaveDB's serialisation trait. Implemented manually for primitives and core
/// types, and derived by `#[derive(WaveWire)]` / `#[wave_db]` for objects.
///
/// Layout contract: `write_stack` writes **exactly** [`Self::STACK_SIZE`]
/// bytes at the writer's stack cursor (length slots, tags, flags, fixed
/// fields) and appends heap payloads; `read` mirrors it byte for byte.
pub trait Wire: Sized {
    /// Exact byte size of this type's stack section. Compile-time constant.
    const STACK_SIZE: usize;

    /// `true` when values of this type can never carry heap bytes.
    /// Lets containers take contiguous fast paths.
    const FIXED: bool;

    /// Bytes this value will append to the heap section. `0` for fixed types.
    fn heap_size(&self) -> usize;

    /// Total serialised size: one allocation of exactly this many bytes.
    fn wire_size(&self) -> usize {
        Self::STACK_SIZE + self.heap_size()
    }

    /// Write the stack slots at the stack cursor and append heap payloads.
    fn write_stack(&self, w: &mut WireWriter) -> WireResult<()>;

    /// Read a value, mirroring `write_stack`.
    fn read(r: &mut WireReader<'_>) -> WireResult<Self>;
}

/// Serialise a value into a fresh, exactly-sized buffer (single allocation).
pub fn to_wire<T: Wire>(value: &T) -> WireResult<Vec<u8>> {
    let total = T::STACK_SIZE + value.heap_size();
    let mut buf = Vec::with_capacity(total);
    buf.resize(T::STACK_SIZE, 0);
    let mut w = WireWriter { buf, stack_pos: 0 };
    value.write_stack(&mut w)?;
    debug_assert_eq!(w.stack_pos, T::STACK_SIZE, "stack section not fully written");
    debug_assert_eq!(w.buf.len(), total, "heap_size() disagreed with bytes written");
    Ok(w.buf)
}

/// Serialise with a `u32` record header prefix: `[header][stack][heap]`.
pub fn to_wire_with_header<T: Wire>(
    header: u32,
    value: &T,
) -> WireResult<Vec<u8>> {
    let total = 4 + T::STACK_SIZE + value.heap_size();
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(&header.to_le_bytes());
    buf.resize(4 + T::STACK_SIZE, 0);
    let mut w = WireWriter { buf, stack_pos: 4 };
    value.write_stack(&mut w)?;
    debug_assert_eq!(w.buf.len(), total, "heap_size() disagreed with bytes written");
    Ok(w.buf)
}

/// Deserialise a value, requiring the buffer to be consumed exactly.
pub fn from_wire<T: Wire>(bytes: &[u8]) -> WireResult<T> {
    let (value, consumed) = read_unit::<T>(bytes, 0)?;
    if consumed != bytes.len() {
        return Err(WireError::TrailingBytes { consumed, len: bytes.len() });
    }
    Ok(value)
}

/// Deserialise a header-prefixed buffer, returning `(header, value)`.
pub fn from_wire_with_header<T: Wire>(bytes: &[u8]) -> WireResult<(u32, T)> {
    let header = peek_header(bytes).ok_or(WireError::UnexpectedEnd {
        needed: 4,
        offset: 0,
        len: bytes.len(),
    })?;
    let value = from_wire::<T>(&bytes[4..])?;
    Ok((header, value))
}

/// Read one self-contained `[stack][heap]` unit starting at `offset`.
/// Returns the value and the offset of the first byte after it.
pub fn read_unit<T: Wire>(
    buf: &[u8],
    offset: usize,
) -> WireResult<(T, usize)> {
    let stack_end = offset
        .checked_add(T::STACK_SIZE)
        .filter(|&end| end <= buf.len())
        .ok_or(WireError::UnexpectedEnd {
            needed: T::STACK_SIZE,
            offset,
            len: buf.len(),
        })?;
    let mut r = WireReader { buf, stack_pos: offset, heap_pos: stack_end };
    let value = T::read(&mut r)?;
    debug_assert_eq!(r.stack_pos, stack_end, "stack section not fully read");
    Ok((value, r.heap_pos))
}

// ─── Writer ──────────────────────────────────────────────────────────────────

/// Cursor pair over the single output buffer: `stack_pos` walks the current
/// stack region; heap payloads append at the buffer's end.
pub struct WireWriter {
    buf: Vec<u8>,
    stack_pos: usize,
}

impl WireWriter {
    /// Copy fixed bytes at the stack cursor.
    pub fn put_stack(&mut self, bytes: &[u8]) {
        let end = self.stack_pos + bytes.len();
        self.buf[self.stack_pos..end].copy_from_slice(bytes);
        self.stack_pos = end;
    }

    /// Advance the stack cursor over pre-zeroed slots (e.g. `None` padding).
    pub const fn skip_stack(&mut self, n: usize) {
        self.stack_pos += n;
    }

    /// Append raw bytes to the heap section.
    pub fn put_heap(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Write a `u32` heap-length slot at the stack cursor.
    pub fn put_len_slot(&mut self, len: usize) -> WireResult<()> {
        let len = u32::try_from(len).map_err(|_| WireError::LengthOverflow(len))?;
        self.put_stack(&len.to_le_bytes());
        Ok(())
    }

    /// Open a self-contained unit region of `stack_size` bytes at the heap
    /// cursor, run `f` with the stack cursor redirected into it, then restore.
    /// The unit's own heap payloads append directly after its stack region,
    /// producing the contiguous `[stack][heap]` unit layout.
    pub fn with_unit(
        &mut self,
        stack_size: usize,
        f: impl FnOnce(&mut Self) -> WireResult<()>,
    ) -> WireResult<()> {
        let saved = self.stack_pos;
        self.stack_pos = self.buf.len();
        let unit_stack_end = self.stack_pos + stack_size;
        self.buf.resize(unit_stack_end, 0);
        f(self)?;
        debug_assert_eq!(self.stack_pos, unit_stack_end, "unit stack not fully written");
        self.stack_pos = saved;
        Ok(())
    }

    /// Write one value as a self-contained `[stack][heap]` unit in the heap.
    pub fn put_unit<T: Wire>(&mut self, value: &T) -> WireResult<()> {
        self.with_unit(T::STACK_SIZE, |w| value.write_stack(w))
    }
}

// ─── Reader ──────────────────────────────────────────────────────────────────

/// Cursor pair over the input buffer: `stack_pos` walks the current stack
/// region, `heap_pos` walks the heap section that follows it.
pub struct WireReader<'a> {
    buf: &'a [u8],
    stack_pos: usize,
    heap_pos: usize,
}

impl<'a> WireReader<'a> {
    /// Take fixed bytes at the stack cursor.
    pub fn take_stack(&mut self, n: usize) -> WireResult<&'a [u8]> {
        let end = self.stack_pos.checked_add(n).filter(|&e| e <= self.buf.len());
        match end {
            Some(end) => {
                let s = &self.buf[self.stack_pos..end];
                self.stack_pos = end;
                Ok(s)
            }
            None => Err(WireError::UnexpectedEnd {
                needed: n,
                offset: self.stack_pos,
                len: self.buf.len(),
            }),
        }
    }

    /// Advance the stack cursor without reading (e.g. `None` padding).
    pub fn skip_stack(&mut self, n: usize) -> WireResult<()> {
        self.take_stack(n).map(|_| ())
    }

    /// Take a heap region of `n` bytes at the heap cursor.
    pub fn take_heap(&mut self, n: usize) -> WireResult<&'a [u8]> {
        let end = self.heap_pos.checked_add(n).filter(|&e| e <= self.buf.len());
        match end {
            Some(end) => {
                let s = &self.buf[self.heap_pos..end];
                self.heap_pos = end;
                Ok(s)
            }
            None => Err(WireError::UnexpectedEnd {
                needed: n,
                offset: self.heap_pos,
                len: self.buf.len(),
            }),
        }
    }

    /// Read a `u32` heap-length slot and take that heap region.
    pub fn take_len_slot_region(&mut self) -> WireResult<&'a [u8]> {
        let len = u32::read(self)? as usize;
        self.take_heap(len)
    }

    /// Build a reader over one self-contained `[stack][heap]` unit region —
    /// used by derived enum impls to parse a variant payload.
    pub const fn for_unit(region: &'a [u8], stack_size: usize) -> WireResult<Self> {
        if stack_size > region.len() {
            return Err(WireError::UnexpectedEnd {
                needed: stack_size,
                offset: 0,
                len: region.len(),
            });
        }
        Ok(Self { buf: region, stack_pos: 0, heap_pos: stack_size })
    }

    /// Assert a unit region was consumed exactly to its end.
    pub const fn finish_unit(&self) -> WireResult<()> {
        if self.heap_pos == self.buf.len() {
            Ok(())
        } else {
            Err(WireError::RegionMismatch {
                consumed: self.heap_pos,
                expected: self.buf.len(),
            })
        }
    }
}

// ─── Primitive impls ─────────────────────────────────────────────────────────

macro_rules! impl_wire_int {
    ($($t:ty),* $(,)?) => {$(
        impl Wire for $t {
            const STACK_SIZE: usize = ::core::mem::size_of::<$t>();
            const FIXED: bool = true;
            fn heap_size(&self) -> usize { 0 }
            fn write_stack(&self, w: &mut WireWriter) -> WireResult<()> {
                w.put_stack(&self.to_le_bytes());
                Ok(())
            }
            fn read(r: &mut WireReader<'_>) -> WireResult<Self> {
                let bytes = r.take_stack(::core::mem::size_of::<$t>())?;
                Ok(<$t>::from_le_bytes(bytes.try_into().expect("exact length")))
            }
        }
    )*};
}

impl_wire_int!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128, f32, f64);

// `usize`/`isize` always travel as 8 bytes (u64/i64) so the layout is
// identical on 32- and 64-bit targets; decoding errors rather than truncates
// when the value does not fit the local pointer width (e.g. wasm32).
impl Wire for usize {
    const STACK_SIZE: Self = 8;
    const FIXED: bool = true;
    fn heap_size(&self) -> usize {
        0
    }
    fn write_stack(&self, w: &mut WireWriter) -> WireResult<()> {
        (*self as u64).write_stack(w)
    }
    fn read(r: &mut WireReader<'_>) -> WireResult<Self> {
        let v = u64::read(r)?;
        Self::try_from(v).map_err(|_| WireError::PlatformIntOverflow(v))
    }
}

impl Wire for isize {
    const STACK_SIZE: usize = 8;
    const FIXED: bool = true;
    fn heap_size(&self) -> usize {
        0
    }
    fn write_stack(&self, w: &mut WireWriter) -> WireResult<()> {
        (*self as i64).write_stack(w)
    }
    fn read(r: &mut WireReader<'_>) -> WireResult<Self> {
        let v = i64::read(r)?;
        #[allow(clippy::cast_sign_loss)]
        Self::try_from(v).map_err(|_| WireError::PlatformIntOverflow(v as u64))
    }
}

impl Wire for bool {
    const STACK_SIZE: usize = 1;
    const FIXED: Self = true;
    fn heap_size(&self) -> usize {
        0
    }
    fn write_stack(&self, w: &mut WireWriter) -> WireResult<()> {
        w.put_stack(&[u8::from(*self)]);
        Ok(())
    }
    fn read(r: &mut WireReader<'_>) -> WireResult<Self> {
        match r.take_stack(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(WireError::InvalidBool(b)),
        }
    }
}

impl Wire for char {
    const STACK_SIZE: usize = 4;
    const FIXED: bool = true;
    fn heap_size(&self) -> usize {
        0
    }
    fn write_stack(&self, w: &mut WireWriter) -> WireResult<()> {
        (*self as u32).write_stack(w)
    }
    fn read(r: &mut WireReader<'_>) -> WireResult<Self> {
        let scalar = u32::read(r)?;
        Self::from_u32(scalar).ok_or(WireError::InvalidChar(scalar))
    }
}

impl Wire for String {
    const STACK_SIZE: usize = 4;
    const FIXED: bool = false;
    fn heap_size(&self) -> usize {
        self.len()
    }
    fn write_stack(&self, w: &mut WireWriter) -> WireResult<()> {
        w.put_len_slot(self.len())?;
        w.put_heap(self.as_bytes());
        Ok(())
    }
    fn read(r: &mut WireReader<'_>) -> WireResult<Self> {
        let region = r.take_len_slot_region()?;
        std::str::from_utf8(region)
            .map(str::to_owned)
            .map_err(|_| WireError::InvalidUtf8)
    }
}

impl<T: Wire> Wire for Vec<T> {
    const STACK_SIZE: usize = 4;
    const FIXED: bool = false;

    fn heap_size(&self) -> usize {
        if T::FIXED {
            self.len() * T::STACK_SIZE
        } else {
            self.iter().map(|e| T::STACK_SIZE + e.heap_size()).sum()
        }
    }

    fn write_stack(&self, w: &mut WireWriter) -> WireResult<()> {
        w.put_len_slot(self.heap_size())?;
        if T::FIXED {
            // Fixed elements have no heap: one contiguous run of stack slots.
            w.with_unit(self.len() * T::STACK_SIZE, |w| {
                self.iter().try_for_each(|e| e.write_stack(w))
            })
        } else {
            self.iter().try_for_each(|e| w.put_unit(e))
        }
    }

    fn read(r: &mut WireReader<'_>) -> WireResult<Self> {
        let region = r.take_len_slot_region()?;
        if T::FIXED {
            if T::STACK_SIZE == 0 || region.len() % T::STACK_SIZE != 0 {
                return Err(WireError::RegionMismatch {
                    consumed: region.len() % T::STACK_SIZE.max(1),
                    expected: region.len(),
                });
            }
            let count = region.len() / T::STACK_SIZE;
            let mut sub = WireReader {
                buf: region,
                stack_pos: 0,
                heap_pos: region.len(),
            };
            let mut out = Self::with_capacity(count);
            for _ in 0..count {
                out.push(T::read(&mut sub)?);
            }
            Ok(out)
        } else {
            let mut out = Self::new();
            let mut offset = 0;
            while offset < region.len() {
                let (value, next) = read_unit::<T>(region, offset)?;
                out.push(value);
                offset = next;
            }
            if offset != region.len() {
                return Err(WireError::RegionMismatch {
                    consumed: offset,
                    expected: region.len(),
                });
            }
            Ok(out)
        }
    }
}

impl<T: Wire> Wire for Option<T> {
    // 1 flag byte + T's stack slots (zero-filled when None) — offsets of any
    // following field stay compile-time constants either way.
    const STACK_SIZE: usize = 1 + T::STACK_SIZE;
    const FIXED: bool = T::FIXED;

    fn heap_size(&self) -> usize {
        self.as_ref().map_or(0, Wire::heap_size)
    }

    fn write_stack(&self, w: &mut WireWriter) -> WireResult<()> {
        match self {
            None => {
                w.put_stack(&[0]);
                w.skip_stack(T::STACK_SIZE);
                Ok(())
            }
            Some(v) => {
                w.put_stack(&[1]);
                v.write_stack(w)
            }
        }
    }

    fn read(r: &mut WireReader<'_>) -> WireResult<Self> {
        match r.take_stack(1)?[0] {
            0 => {
                r.skip_stack(T::STACK_SIZE)?;
                Ok(None)
            }
            1 => Ok(Some(T::read(r)?)),
            b => Err(WireError::InvalidBool(b)),
        }
    }
}

impl<T: Wire, const N: usize> Wire for [T; N] {
    const STACK_SIZE: usize = N * T::STACK_SIZE;
    const FIXED: bool = T::FIXED;

    fn heap_size(&self) -> usize {
        if T::FIXED { 0 } else { self.iter().map(Wire::heap_size).sum() }
    }

    fn write_stack(&self, w: &mut WireWriter) -> WireResult<()> {
        self.iter().try_for_each(|e| e.write_stack(w))
    }

    fn read(r: &mut WireReader<'_>) -> WireResult<Self> {
        let mut out = Vec::with_capacity(N);
        for _ in 0..N {
            out.push(T::read(r)?);
        }
        Ok(out.try_into().unwrap_or_else(|_| unreachable!("read exactly N elements")))
    }
}

macro_rules! impl_wire_tuple {
    ($(($($name:ident : $idx:tt),+)),* $(,)?) => {$(
        impl<$($name: Wire),+> Wire for ($($name,)+) {
            const STACK_SIZE: usize = 0 $(+ $name::STACK_SIZE)+;
            const FIXED: bool = true $(&& $name::FIXED)+;
            fn heap_size(&self) -> usize {
                0 $(+ self.$idx.heap_size())+
            }
            fn write_stack(&self, w: &mut WireWriter) -> WireResult<()> {
                $(self.$idx.write_stack(w)?;)+
                Ok(())
            }
            fn read(r: &mut WireReader<'_>) -> WireResult<Self> {
                Ok(($($name::read(r)?,)+))
            }
        }
    )*};
}

impl_wire_tuple!(
    (A: 0),
    (A: 0, B: 1),
    (A: 0, B: 1, C: 2),
    (A: 0, B: 1, C: 2, D: 3),
);

impl<T: Wire> Wire for Box<T> {
    const STACK_SIZE: usize = T::STACK_SIZE;
    const FIXED: bool = T::FIXED;
    fn heap_size(&self) -> usize {
        self.as_ref().heap_size()
    }
    fn write_stack(&self, w: &mut WireWriter) -> WireResult<()> {
        self.as_ref().write_stack(w)
    }
    fn read(r: &mut WireReader<'_>) -> WireResult<Self> {
        Ok(Self::new(T::read(r)?))
    }
}

impl Wire for crate::Id {
    const STACK_SIZE: usize = 16;
    const FIXED: bool = true;
    fn heap_size(&self) -> usize {
        0
    }
    fn write_stack(&self, w: &mut WireWriter) -> WireResult<()> {
        self.raw().write_stack(w)
    }
    fn read(r: &mut WireReader<'_>) -> WireResult<Self> {
        Ok(Self::from_raw(u128::read(r)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T: Wire + PartialEq + std::fmt::Debug>(value: &T) {
        let bytes = to_wire(value).unwrap();
        assert_eq!(
            bytes.len(),
            value.wire_size(),
            "buffer length must equal wire_size"
        );
        let back: T = from_wire(&bytes).unwrap();
        assert_eq!(&back, value);
    }

    #[test]
    fn ints_roundtrip() {
        roundtrip(&0u8);
        roundtrip(&u8::MAX);
        roundtrip(&0x1234_5678u32);
        roundtrip(&u64::MAX);
        roundtrip(&u128::MAX);
        roundtrip(&-1i8);
        roundtrip(&i64::MIN);
        roundtrip(&i128::MIN);
        roundtrip(&1.5f32);
        roundtrip(&-0.0f64);
    }

    #[test]
    fn ints_are_little_endian_packed() {
        assert_eq!(to_wire(&0x0102_0304u32).unwrap(), [4, 3, 2, 1]);
        assert_eq!(u32::STACK_SIZE, 4);
        assert_eq!(<(u8, u32, bool)>::STACK_SIZE, 6, "packed, no padding");
    }

    #[test]
    fn bool_and_char() {
        roundtrip(&true);
        roundtrip(&false);
        roundtrip(&'🌊');
        assert!(matches!(
            from_wire::<bool>(&[2]),
            Err(WireError::InvalidBool(2))
        ));
        assert!(matches!(
            from_wire::<char>(&0xD800u32.to_le_bytes()),
            Err(WireError::InvalidChar(0xD800))
        ));
    }

    #[test]
    fn string_layout() {
        let s = "wave".to_owned();
        let bytes = to_wire(&s).unwrap();
        // [u32 len = 4][b"wave"]
        assert_eq!(bytes, [4, 0, 0, 0, b'w', b'a', b'v', b'e']);
        roundtrip(&s);
        roundtrip(&String::new());
    }

    #[test]
    fn vec_fixed_layout() {
        let v = vec![1u16, 2, 3];
        let bytes = to_wire(&v).unwrap();
        // [u32 region len = 6][1,0][2,0][3,0]
        assert_eq!(bytes, [6, 0, 0, 0, 1, 0, 2, 0, 3, 0]);
        roundtrip(&v);
        roundtrip(&Vec::<u64>::new());
        roundtrip(&vec![0u8, 255, 7]);
    }

    #[test]
    fn vec_dynamic_roundtrip() {
        roundtrip(&vec!["a".to_owned(), String::new(), "long-ish".to_owned()]);
        roundtrip(&vec![vec![1u8, 2], vec![], vec![3]]);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn option_layout() {
        let none: Option<u32> = None;
        assert_eq!(to_wire(&none).unwrap(), [0, 0, 0, 0, 0]);
        let some: Option<u32> = Some(7);
        assert_eq!(to_wire(&some).unwrap(), [1, 7, 0, 0, 0]);
        roundtrip(&none);
        roundtrip(&some);
        roundtrip(&Some("text".to_owned()));
        roundtrip(&Option::<String>::None);
        assert_eq!(Option::<u64>::STACK_SIZE, 9);
        assert!(Option::<u64>::FIXED);
        assert!(!Option::<String>::FIXED);
    }

    #[test]
    fn arrays_and_tuples() {
        roundtrip(&[1u8, 2, 3, 4]);
        roundtrip(&[String::from("a"), String::from("bb")]);
        roundtrip(&(1u8, 0xFFFF_FFFFu32, true));
        roundtrip(&(42u64, "payload".to_owned()));
    }

    #[test]
    fn id_roundtrip() {
        let id = crate::Id::new(42, 7, 1000, 123_456);
        roundtrip(&id);
        assert_eq!(crate::Id::STACK_SIZE, 16);
    }

    #[test]
    fn nested_vec_of_pairs_mirrors_query_envelope() {
        // The shape `Vec<(u8, Vec<u8>)>` is the query response envelope.
        let entries: Vec<(u8, Vec<u8>)> =
            vec![(1, vec![9, 9]), (2, vec![]), (42, vec![1, 2, 3, 4])];
        roundtrip(&entries);
    }

    #[test]
    fn header_pack_unpack() {
        let h = pack_header(0x000F_4240, 42); // 1_000_000 fits u20 < u24
        assert_eq!(unpack_header(h), (0x000F_4240, 42));
        let bytes = to_wire_with_header(h, &7u64).unwrap();
        assert_eq!(peek_header(&bytes), Some(h));
        let (header, value) = from_wire_with_header::<u64>(&bytes).unwrap();
        assert_eq!(header, h);
        assert_eq!(value, 7);
    }

    #[test]
    fn single_allocation_capacity_is_exact() {
        let v = vec!["abc".to_owned(), "defg".to_owned()];
        let bytes = to_wire(&v).unwrap();
        // capacity reserved up front must not have grown
        assert_eq!(bytes.capacity(), v.wire_size());
    }

    #[test]
    fn trailing_bytes_rejected() {
        let mut bytes = to_wire(&7u32).unwrap();
        bytes.push(0);
        assert!(matches!(
            from_wire::<u32>(&bytes),
            Err(WireError::TrailingBytes { consumed: 4, len: 5 })
        ));
    }

    #[test]
    fn truncated_heap_rejected() {
        let bytes = to_wire(&"wave".to_owned()).unwrap();
        assert!(matches!(
            from_wire::<String>(&bytes[..6]),
            Err(WireError::UnexpectedEnd { .. })
        ));
    }
}
