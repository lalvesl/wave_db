//! Query expression DSL shared by clients and nodes.
//!
//! Clients build an [`Expr`], serialise it with the wire format, and send
//! it inside `QueryNonUnique`.  The Quick-Node deserialises it and
//! evaluates it against each candidate record's wire bytes using the
//! struct's [`ObjectDescriptor`] — see [`eval`].  Both halves live here so
//! the node never depends on the client crate.
//!
//! # Columns
//!
//! Comparison constructors take a [`Field`] — a typed column handle. The
//! `#[wave_db]` macro emits one `const` per struct field, so a query names
//! its columns through the type (`Order::amount`) and a misspelt column is
//! a compile error rather than a silently-empty result. A `&'static str`
//! literal also converts into a `Field`, for dynamic or ad-hoc field names.

use wavedb_macros::WaveWire;

use crate::registry::{FieldDescriptor, FieldKind, ObjectDescriptor};

/// A field name used in an expression.
pub type FieldName = String;

/// A scalar value that can be compared in an expression.
///
/// Every primitive numeric width gets its own variant so a query value
/// round-trips at exactly the width the caller wrote — the server compares
/// against the record's field using the descriptor's [`FieldKind`], no
/// silent promotion on the wire.
#[derive(Debug, Clone, WaveWire, PartialEq)]
pub enum Value {
    /// Unsigned 8-bit integer.
    U8(u8),
    /// Unsigned 16-bit integer.
    U16(u16),
    /// Unsigned 32-bit integer.
    U32(u32),
    /// Unsigned 64-bit integer.
    U64(u64),
    /// Unsigned 128-bit integer.
    U128(u128),
    /// Signed 8-bit integer.
    I8(i8),
    /// Signed 16-bit integer.
    I16(i16),
    /// Signed 32-bit integer.
    I32(i32),
    /// Signed 64-bit integer.
    I64(i64),
    /// Signed 128-bit integer.
    I128(i128),
    /// 32-bit float.
    F32(f32),
    /// 64-bit float.
    F64(f64),
    /// UTF-8 string.
    Str(String),
    /// Boolean.
    Bool(bool),
    /// Raw bytes.
    Bytes(Vec<u8>),
}

/// Exact-width `From` impls — `42u16` becomes `Value::U16`, not a promoted
/// `U64`.
macro_rules! value_from_exact {
    ( $( $ty:ty => $variant:ident ),* $(,)? ) => {
        $(
            impl From<$ty> for Value {
                fn from(v: $ty) -> Self {
                    Self::$variant(v)
                }
            }
        )*
    };
}

value_from_exact! {
    u8 => U8,
    u16 => U16,
    u32 => U32,
    u64 => U64,
    u128 => U128,
    i8 => I8,
    i16 => I16,
    i32 => I32,
    i64 => I64,
    i128 => I128,
    f32 => F32,
    f64 => F64,
    bool => Bool,
    String => Str,
}

impl From<usize> for Value {
    fn from(v: usize) -> Self {
        Self::U64(v as u64)
    }
}
impl From<isize> for Value {
    fn from(v: isize) -> Self {
        Self::I64(v as i64)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Str(v.to_owned())
    }
}
impl From<&String> for Value {
    fn from(v: &String) -> Self {
        Self::Str(v.clone())
    }
}
impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Self::Bytes(v)
    }
}
impl From<&[u8]> for Value {
    fn from(v: &[u8]) -> Self {
        Self::Bytes(v.to_vec())
    }
}

/// A query expression used to filter `NonUnique` records.
///
/// Expressions are composable and serialised with the wire format.
#[derive(Debug, Clone, WaveWire, PartialEq)]
pub enum Expr {
    /// Match all records (no filter).
    All,
    /// `field == value`
    Eq {
        /// The field to compare.
        field: FieldName,
        /// The value to compare against.
        value: Value,
    },
    /// `field != value`
    Ne {
        /// The field to compare.
        field: FieldName,
        /// The value to compare against.
        value: Value,
    },
    /// `field > value`
    Gt {
        /// The field to compare.
        field: FieldName,
        /// The lower bound (exclusive).
        value: Value,
    },
    /// `field >= value`
    Gte {
        /// The field to compare.
        field: FieldName,
        /// The lower bound (inclusive).
        value: Value,
    },
    /// `field < value`
    Lt {
        /// The field to compare.
        field: FieldName,
        /// The upper bound (exclusive).
        value: Value,
    },
    /// `field <= value`
    Lte {
        /// The field to compare.
        field: FieldName,
        /// The upper bound (inclusive).
        value: Value,
    },
    /// `left AND right`
    And {
        /// Left sub-expression.
        left: Box<Self>,
        /// Right sub-expression.
        right: Box<Self>,
    },
    /// `left OR right`
    Or {
        /// Left sub-expression.
        left: Box<Self>,
        /// Right sub-expression.
        right: Box<Self>,
    },
    /// `NOT expr`
    Not(Box<Self>),
}

impl Expr {
    /// Match all records.
    ///
    /// # Examples
    ///
    /// ```
    /// use wavedb_core::query::Expr;
    /// let e = Expr::all();
    /// assert!(e.to_bytes().is_ok());
    /// ```
    pub const fn all() -> Self {
        Self::All
    }

    /// `field == value`
    ///
    /// `field` is a typed [`Field`] handle — pass the macro-generated column
    /// constant (`Order::amount`) for a compile-time-checked name. A
    /// `&'static str` literal also converts, for dynamic or ad-hoc field names.
    ///
    /// # Examples
    ///
    /// ```
    /// use wavedb_core::query::Expr;
    /// let e = Expr::eq("status", "active");
    /// let bytes = e.to_bytes().unwrap();
    /// let decoded = Expr::from_bytes(&bytes).unwrap();
    /// assert_eq!(e, decoded);
    /// ```
    pub fn eq(field: impl Into<Field>, value: impl Into<Value>) -> Self {
        Self::Eq {
            field: field.into().name().to_owned(),
            value: value.into(),
        }
    }

    /// `field != value`
    pub fn ne(field: impl Into<Field>, value: impl Into<Value>) -> Self {
        Self::Ne {
            field: field.into().name().to_owned(),
            value: value.into(),
        }
    }

    /// `field > value`
    pub fn gt(field: impl Into<Field>, value: impl Into<Value>) -> Self {
        Self::Gt {
            field: field.into().name().to_owned(),
            value: value.into(),
        }
    }

    /// `field >= value`
    pub fn gte(field: impl Into<Field>, value: impl Into<Value>) -> Self {
        Self::Gte {
            field: field.into().name().to_owned(),
            value: value.into(),
        }
    }

    /// `field < value`
    pub fn lt(field: impl Into<Field>, value: impl Into<Value>) -> Self {
        Self::Lt {
            field: field.into().name().to_owned(),
            value: value.into(),
        }
    }

    /// `field <= value`
    pub fn lte(field: impl Into<Field>, value: impl Into<Value>) -> Self {
        Self::Lte {
            field: field.into().name().to_owned(),
            value: value.into(),
        }
    }

    /// `left AND right`
    ///
    /// # Examples
    ///
    /// ```
    /// use wavedb_core::query::Expr;
    /// let e = Expr::and(Expr::gt("amount", 100u64), Expr::eq("status", "open"));
    /// assert!(e.to_bytes().is_ok());
    /// ```
    pub fn and(left: Self, right: Self) -> Self {
        Self::And {
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    /// `left OR right`
    pub fn or(left: Self, right: Self) -> Self {
        Self::Or {
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    /// Negate an expression: `NOT expr`.
    ///
    /// Named `negate` rather than `not` to avoid ambiguity with
    /// `std::ops::Not`.
    pub fn negate(expr: Self) -> Self {
        Self::Not(Box::new(expr))
    }

    /// Build a [`Field`] handle for combinator-style construction.
    ///
    /// Sugar for the macro-generated `XxxFields` constants; useful when the
    /// field name is dynamic.
    ///
    /// ```
    /// use wavedb_core::query::Expr;
    /// let e = Expr::field("amount").gt(100u64);
    /// assert!(e.to_bytes().is_ok());
    /// ```
    pub const fn field(name: &'static str) -> Field {
        Field::new(name)
    }

    /// Serialise this expression to bytes (wire format).
    pub fn to_bytes(&self) -> crate::Result<Vec<u8>> {
        Ok(crate::wire::to_wire(self)?)
    }

    /// Deserialise an expression from bytes (wire format).
    pub fn from_bytes(bytes: &[u8]) -> crate::Result<Self> {
        Ok(crate::wire::from_wire(bytes)?)
    }
}

// ── Operator overloads ───────────────────────────────────────────────────────
//
// Sugar so users can write `a & b`, `a | b`, `!a` instead of
// `Expr::and(a, b)`, `Expr::or(a, b)`, `Expr::negate(a)`.

impl ::core::ops::BitAnd for Expr {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self::and(self, rhs)
    }
}

impl ::core::ops::BitOr for Expr {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self::or(self, rhs)
    }
}

impl ::core::ops::Not for Expr {
    type Output = Self;
    fn not(self) -> Self {
        Self::negate(self)
    }
}

// ── Field DSL ────────────────────────────────────────────────────────────────

/// A typed field handle used to build [`Expr`] values fluently.
///
/// `Field` is the receiver type of the macro-generated `XxxFields` const
/// accessors.  Each comparison method returns an [`Expr`].
///
/// # Example
///
/// ```
/// use wavedb_core::query::{Expr, Field};
/// const AMOUNT: Field = Field::new("amount");
/// let e: Expr = AMOUNT.gt(100u64);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    name: &'static str,
}

impl Field {
    /// Create a new field handle.
    ///
    /// `const`-compatible so the proc-macro can emit `const FOO: Field = ...`.
    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }

    /// The underlying field name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// `self == value`
    pub fn eq(self, value: impl Into<Value>) -> Expr {
        Expr::eq(self.name, value)
    }

    /// `self != value`
    pub fn ne(self, value: impl Into<Value>) -> Expr {
        Expr::ne(self.name, value)
    }

    /// `self > value`
    pub fn gt(self, value: impl Into<Value>) -> Expr {
        Expr::gt(self.name, value)
    }

    /// `self >= value`
    pub fn gte(self, value: impl Into<Value>) -> Expr {
        Expr::gte(self.name, value)
    }

    /// `self < value`
    pub fn lt(self, value: impl Into<Value>) -> Expr {
        Expr::lt(self.name, value)
    }

    /// `self <= value`
    pub fn lte(self, value: impl Into<Value>) -> Expr {
        Expr::lte(self.name, value)
    }
}

/// Build a [`Field`] from a static string literal.
///
/// Lets the [`Expr`] constructors keep accepting `&'static str` field names
/// (`Expr::gt("amount", 100u64)`) alongside the typed, macro-generated column
/// constants (`Expr::gt(Order::amount, 100u64)`).
impl From<&'static str> for Field {
    fn from(name: &'static str) -> Self {
        Self::new(name)
    }
}

// ── Node-side evaluation ─────────────────────────────────────────────────────

/// Evaluate `expr` against a record's wire `body` using its descriptor.
///
/// `body` is the record's wire bytes **without** the leading version byte
/// — the descriptor already encodes which version the offsets belong to.
///
/// Comparison semantics:
///
/// - Numeric fields compare numerically across widths (querying a `u32`
///   field with `100u64` works); signed/unsigned/float mixes go through a
///   total-order widening, no silent wraparound.
/// - `String` fields compare lexicographically (UTF-8 byte order),
///   `Vec<u8>` fields bytewise, `bool` by `false < true`.
/// - A comparison on a missing field, a field kind the evaluator can't
///   read, or a body too short for the descriptor is **`false`** — and a
///   `Not(...)` of it is `true`, mirroring SQL's treatment of unknowable
///   predicates as non-matches.
///
/// Heap fields (`String` / `Vec<u8>`) are located by walking the heap
/// section in declaration order; the walk only crosses field kinds whose
/// heap length is a plain `u32` slot (`Str`, `Bytes`, `List`).  A filter
/// on a heap field that sits *after* an `Option`/nested-struct heap field
/// is unlocatable from the descriptor alone and evaluates to `false`.
#[must_use]
pub fn eval(desc: &ObjectDescriptor, body: &[u8], expr: &Expr) -> bool {
    match expr {
        Expr::All => true,
        Expr::And { left, right } => {
            eval(desc, body, left) && eval(desc, body, right)
        }
        Expr::Or { left, right } => {
            eval(desc, body, left) || eval(desc, body, right)
        }
        Expr::Not(inner) => !eval(desc, body, inner),
        Expr::Eq { field, value } => {
            compare(desc, body, field, value)
                == Some(core::cmp::Ordering::Equal)
        }
        Expr::Ne { field, value } => matches!(
            compare(desc, body, field, value),
            Some(o) if o != core::cmp::Ordering::Equal
        ),
        Expr::Gt { field, value } => {
            compare(desc, body, field, value)
                == Some(core::cmp::Ordering::Greater)
        }
        Expr::Gte { field, value } => matches!(
            compare(desc, body, field, value),
            Some(core::cmp::Ordering::Greater | core::cmp::Ordering::Equal)
        ),
        Expr::Lt { field, value } => {
            compare(desc, body, field, value) == Some(core::cmp::Ordering::Less)
        }
        Expr::Lte { field, value } => matches!(
            compare(desc, body, field, value),
            Some(core::cmp::Ordering::Less | core::cmp::Ordering::Equal)
        ),
    }
}

/// `record_field <=> query_value`, or `None` when unreadable/incomparable.
fn compare(
    desc: &ObjectDescriptor,
    body: &[u8],
    field: &str,
    value: &Value,
) -> Option<core::cmp::Ordering> {
    let fd = desc.field(field)?;
    if body.len() < desc.stack_size {
        return None;
    }
    let slot = body.get(fd.stack_offset..fd.stack_offset + fd.stack_size)?;

    match fd.kind {
        FieldKind::Str => {
            let bytes = heap_bytes(desc, body, fd)?;
            let s = core::str::from_utf8(bytes).ok()?;
            match value {
                Value::Str(v) => Some(s.cmp(v.as_str())),
                _ => None,
            }
        }
        FieldKind::Bytes => {
            let bytes = heap_bytes(desc, body, fd)?;
            match value {
                Value::Bytes(v) => Some(bytes.cmp(v.as_slice())),
                _ => None,
            }
        }
        FieldKind::Bool => {
            let b = *slot.first()? != 0;
            match value {
                Value::Bool(v) => Some(b.cmp(v)),
                _ => None,
            }
        }
        _ => {
            let field_num = read_numeric(fd.kind, slot)?;
            let value_num = Numeric::from_value(value)?;
            field_num.partial_cmp(&value_num)
        }
    }
}

/// Locate a heap field's payload bytes.
///
/// Heap payloads append in field-declaration order; each `Str`/`Bytes`/
/// `List` field's heap length is the `u32` in its stack slot.  Other
/// heapable kinds (`Option<T>`, nested structs — including the injected
/// `metadata`) don't expose one region length, so the walk can't hop over
/// them:
///
/// - **forward** from `stack_size` covers targets with only u32-slot
///   heap fields before them;
/// - **backward** from the body's end covers targets with only u32-slot
///   heap fields after them — the common real layout, where `metadata`
///   (heapable, opaque) is injected first and user `String`s follow.
///
/// A target squeezed between opaque heapables on both sides is
/// unlocatable and the comparison evaluates to `false`.
fn heap_bytes<'b>(
    desc: &ObjectDescriptor,
    body: &'b [u8],
    target: &FieldDescriptor,
) -> Option<&'b [u8]> {
    let region_len = |fd: &FieldDescriptor| -> Option<usize> {
        let slot = body.get(fd.stack_offset..fd.stack_offset + 4)?;
        Some(u32::from_le_bytes(slot.try_into().ok()?) as usize)
    };
    let hoppable = |fd: &FieldDescriptor| {
        matches!(fd.kind, FieldKind::Str | FieldKind::Bytes | FieldKind::List)
    };

    // Forward walk.
    let mut cursor = Some(desc.stack_size);
    for fd in desc.fields {
        if !fd.heapable {
            continue;
        }
        if core::ptr::eq(fd, target) {
            if let Some(start) = cursor {
                let len = region_len(fd)?;
                return body.get(start..start + len);
            }
            break;
        }
        cursor = match cursor {
            Some(c) if hoppable(fd) => Some(c + region_len(fd)?),
            _ => None,
        };
    }

    // Backward walk.
    let mut end = body.len();
    for fd in desc.fields.iter().rev() {
        if !fd.heapable {
            continue;
        }
        if core::ptr::eq(fd, target) {
            let len = region_len(fd)?;
            return body.get(end.checked_sub(len)?..end);
        }
        if !hoppable(fd) {
            return None;
        }
        end = end.checked_sub(region_len(fd)?)?;
    }
    None
}

/// Total-order numeric widening: every integer width plus floats compare
/// in one domain without wraparound.
#[derive(Clone, Copy, Debug)]
enum Numeric {
    Int(i128),
    /// `u128` values above `i128::MAX` — bigger than every `Int`.
    BigUint(u128),
    Float(f64),
}

impl Numeric {
    fn from_value(v: &Value) -> Option<Self> {
        Some(match *v {
            Value::U8(x) => Self::Int(i128::from(x)),
            Value::U16(x) => Self::Int(i128::from(x)),
            Value::U32(x) => Self::Int(i128::from(x)),
            Value::U64(x) => Self::Int(i128::from(x)),
            Value::U128(x) => {
                i128::try_from(x).map_or(Self::BigUint(x), Self::Int)
            }
            Value::I8(x) => Self::Int(i128::from(x)),
            Value::I16(x) => Self::Int(i128::from(x)),
            Value::I32(x) => Self::Int(i128::from(x)),
            Value::I64(x) => Self::Int(i128::from(x)),
            Value::I128(x) => Self::Int(x),
            Value::F32(x) => Self::Float(f64::from(x)),
            Value::F64(x) => Self::Float(x),
            Value::Str(_) | Value::Bool(_) | Value::Bytes(_) => return None,
        })
    }

    #[allow(clippy::cast_precision_loss)]
    const fn as_f64(self) -> f64 {
        match self {
            Self::Int(x) => x as f64,
            Self::BigUint(x) => x as f64,
            Self::Float(x) => x,
        }
    }

    fn partial_cmp(self, other: &Self) -> Option<core::cmp::Ordering> {
        use core::cmp::Ordering;
        match (self, *other) {
            (Self::Int(a), Self::Int(b)) => Some(a.cmp(&b)),
            (Self::BigUint(a), Self::BigUint(b)) => Some(a.cmp(&b)),
            // BigUint is by construction > i128::MAX ≥ any Int.
            (Self::BigUint(_), Self::Int(_)) => Some(Ordering::Greater),
            (Self::Int(_), Self::BigUint(_)) => Some(Ordering::Less),
            (a, b @ Self::Float(_)) | (a @ Self::Float(_), b) => {
                a.as_f64().partial_cmp(&b.as_f64())
            }
        }
    }
}

/// Decode a fixed-width numeric field from its stack slot.
fn read_numeric(kind: FieldKind, slot: &[u8]) -> Option<Numeric> {
    macro_rules! le {
        ($ty:ty) => {
            <$ty>::from_le_bytes(slot.try_into().ok()?)
        };
    }
    Some(match kind {
        FieldKind::U8 => Numeric::Int(i128::from(le!(u8))),
        FieldKind::U16 => Numeric::Int(i128::from(le!(u16))),
        FieldKind::U32 => Numeric::Int(i128::from(le!(u32))),
        FieldKind::U64 => Numeric::Int(i128::from(le!(u64))),
        FieldKind::U128 => {
            let x = le!(u128);
            i128::try_from(x).map_or(Numeric::BigUint(x), Numeric::Int)
        }
        FieldKind::I8 => Numeric::Int(i128::from(le!(i8))),
        FieldKind::I16 => Numeric::Int(i128::from(le!(i16))),
        FieldKind::I32 => Numeric::Int(i128::from(le!(i32))),
        FieldKind::I64 => Numeric::Int(i128::from(le!(i64))),
        FieldKind::I128 => Numeric::Int(le!(i128)),
        FieldKind::F32 => Numeric::Float(f64::from(le!(f32))),
        FieldKind::F64 => Numeric::Float(le!(f64)),
        _ => return None,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use core::f64;

    use super::*;
    use crate::Shape;

    #[test]
    fn all_expr_roundtrip() {
        let expr = Expr::all();
        let bytes = expr.to_bytes().unwrap();
        let decoded = Expr::from_bytes(&bytes).unwrap();
        assert_eq!(expr, decoded);
    }

    #[test]
    fn gt_expr_roundtrip() {
        let expr = Expr::gt("amount", 100u64);
        let bytes = expr.to_bytes().unwrap();
        let decoded = Expr::from_bytes(&bytes).unwrap();
        assert_eq!(expr, decoded);
    }

    #[test]
    fn compound_and_or_roundtrip() {
        let expr = Expr::and(
            Expr::gt("amount", 100u64),
            Expr::eq("status", "shipped"),
        );
        let bytes = expr.to_bytes().unwrap();
        let decoded = Expr::from_bytes(&bytes).unwrap();
        assert_eq!(expr, decoded);
    }

    #[test]
    fn not_expr_roundtrip() {
        let expr = Expr::negate(Expr::eq("deleted", true));
        let bytes = expr.to_bytes().unwrap();
        let decoded = Expr::from_bytes(&bytes).unwrap();
        assert_eq!(expr, decoded);
    }

    #[test]
    fn value_conversions() {
        assert_eq!(Value::from(42u64), Value::U64(42));
        assert_eq!(Value::from(42i64), Value::I64(42));
        assert_eq!(Value::from(f64::consts::PI), Value::F64(f64::consts::PI));
        assert_eq!(Value::from("hello"), Value::Str("hello".into()));
        assert_eq!(Value::from(true), Value::Bool(true));
    }

    #[test]
    fn value_conversions_are_exact_width() {
        assert_eq!(Value::from(7u8), Value::U8(7));
        assert_eq!(Value::from(7u16), Value::U16(7));
        assert_eq!(Value::from(7u32), Value::U32(7));
        assert_eq!(Value::from(7u128), Value::U128(7));
        assert_eq!(Value::from(-7i8), Value::I8(-7));
        assert_eq!(Value::from(-7i16), Value::I16(-7));
        assert_eq!(Value::from(-7i32), Value::I32(-7));
        assert_eq!(Value::from(-7i128), Value::I128(-7));
        assert_eq!(Value::from(1.5f32), Value::F32(1.5));
        // Platform-width integers normalise to 64-bit.
        assert_eq!(Value::from(7usize), Value::U64(7));
        assert_eq!(Value::from(-7isize), Value::I64(-7));
    }

    #[test]
    fn all_numeric_widths_roundtrip() {
        let exprs = [
            Expr::eq("a", u8::MAX),
            Expr::eq("a", u16::MAX),
            Expr::eq("a", u32::MAX),
            Expr::eq("a", u64::MAX),
            Expr::eq("a", u128::MAX),
            Expr::eq("a", i8::MIN),
            Expr::eq("a", i16::MIN),
            Expr::eq("a", i32::MIN),
            Expr::eq("a", i64::MIN),
            Expr::eq("a", i128::MIN),
            Expr::eq("a", f32::MIN_POSITIVE),
            Expr::eq("a", f64::MAX),
        ];
        for expr in exprs {
            let bytes = expr.to_bytes().unwrap();
            let decoded = Expr::from_bytes(&bytes).unwrap();
            assert_eq!(expr, decoded);
        }
    }

    // ── eval ────────────────────────────────────────────────────────────────

    /// Hand-built descriptor mirroring:
    ///
    /// ```ignore
    /// struct T { amount: u64, active: bool, name: String, note: String }
    /// ```
    ///
    /// Stack: amount 8 + active 1 + name-len 4 + note-len 4 = 17 bytes.
    fn test_descriptor() -> ObjectDescriptor {
        static FIELDS: [FieldDescriptor; 4] = [
            FieldDescriptor {
                name: "amount",
                stack_offset: 0,
                stack_size: 8,
                heapable: false,
                kind: FieldKind::U64,
            },
            FieldDescriptor {
                name: "active",
                stack_offset: 8,
                stack_size: 1,
                heapable: false,
                kind: FieldKind::Bool,
            },
            FieldDescriptor {
                name: "name",
                stack_offset: 9,
                stack_size: 4,
                heapable: true,
                kind: FieldKind::Str,
            },
            FieldDescriptor {
                name: "note",
                stack_offset: 13,
                stack_size: 4,
                heapable: true,
                kind: FieldKind::Str,
            },
        ];
        ObjectDescriptor {
            header: (9000 << 8) | 1,
            struct_id: 9000,
            version: 1,
            shape: Shape::NonUnique,
            type_name: "EvalTest1",
            stack_size: 17,
            fixed: false,
            fields: &FIELDS,
            heap_fields: &["name", "note"],
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn test_body(amount: u64, active: bool, name: &str, note: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&amount.to_le_bytes());
        b.push(u8::from(active));
        b.extend_from_slice(&(name.len() as u32).to_le_bytes());
        b.extend_from_slice(&(note.len() as u32).to_le_bytes());
        b.extend_from_slice(name.as_bytes());
        b.extend_from_slice(note.as_bytes());
        b
    }

    #[test]
    fn eval_numeric_comparisons() {
        let desc = test_descriptor();
        let body = test_body(150, true, "alice", "x");

        assert!(eval(&desc, &body, &Expr::gt("amount", 100u64)));
        assert!(!eval(&desc, &body, &Expr::gt("amount", 150u64)));
        assert!(eval(&desc, &body, &Expr::gte("amount", 150u64)));
        assert!(eval(&desc, &body, &Expr::lt("amount", 151u64)));
        assert!(eval(&desc, &body, &Expr::eq("amount", 150u64)));
        assert!(eval(&desc, &body, &Expr::ne("amount", 149u64)));
    }

    #[test]
    fn eval_cross_width_numeric() {
        let desc = test_descriptor();
        let body = test_body(150, true, "a", "b");
        // u64 field queried with narrower / wider / signed / float values.
        assert!(eval(&desc, &body, &Expr::gt("amount", 100u8)));
        assert!(eval(&desc, &body, &Expr::gt("amount", 100i32)));
        assert!(eval(&desc, &body, &Expr::lt("amount", 200u128)));
        assert!(eval(&desc, &body, &Expr::gt("amount", 149.5f64)));
        assert!(eval(&desc, &body, &Expr::gt("amount", -1i64)));
    }

    #[test]
    fn eval_string_fields_including_second_heap_field() {
        let desc = test_descriptor();
        let body = test_body(1, false, "alice", "hello world");

        assert!(eval(&desc, &body, &Expr::eq("name", "alice")));
        assert!(!eval(&desc, &body, &Expr::eq("name", "bob")));
        // `note` sits after `name` in the heap — exercises the heap walk.
        assert!(eval(&desc, &body, &Expr::eq("note", "hello world")));
        assert!(eval(&desc, &body, &Expr::gt("name", "aaa")));
    }

    #[test]
    fn eval_bool_field() {
        let desc = test_descriptor();
        let body = test_body(1, true, "a", "b");
        assert!(eval(&desc, &body, &Expr::eq("active", true)));
        assert!(eval(&desc, &body, &Expr::ne("active", false)));
    }

    #[test]
    fn eval_compound_and_or_not() {
        let desc = test_descriptor();
        let body = test_body(150, true, "alice", "n");

        let e =
            Expr::and(Expr::gt("amount", 100u64), Expr::eq("name", "alice"));
        assert!(eval(&desc, &body, &e));

        let e =
            Expr::or(Expr::gt("amount", 1000u64), Expr::eq("name", "alice"));
        assert!(eval(&desc, &body, &e));

        assert!(eval(&desc, &body, &Expr::negate(Expr::eq("name", "bob"))));
    }

    #[test]
    fn eval_unknown_field_is_false_and_not_makes_it_true() {
        let desc = test_descriptor();
        let body = test_body(1, true, "a", "b");
        let cmp = Expr::eq("no_such_field", 1u64);
        assert!(!eval(&desc, &body, &cmp));
        assert!(eval(&desc, &body, &Expr::negate(cmp)));
    }

    #[test]
    fn eval_type_mismatch_is_false() {
        let desc = test_descriptor();
        let body = test_body(1, true, "a", "b");
        // String compared against a numeric field and vice versa.
        assert!(!eval(&desc, &body, &Expr::eq("amount", "150")));
        assert!(!eval(&desc, &body, &Expr::eq("name", 42u64)));
    }

    #[test]
    fn eval_truncated_body_is_false() {
        let desc = test_descriptor();
        assert!(!eval(&desc, &[0u8; 4], &Expr::eq("amount", 0u64)));
        assert!(eval(&desc, &[0u8; 4], &Expr::all()));
    }

    /// Real `#[wave_db]` layout: the injected `metadata` field is heapable
    /// but opaque (nested struct) and sits *before* every user field.
    /// String filters must still work via the backward heap walk.
    ///
    /// ```ignore
    /// struct T { metadata: Metadata, qty: u64, label: String }
    /// ```
    fn metadata_like_descriptor() -> ObjectDescriptor {
        static FIELDS: [FieldDescriptor; 3] = [
            FieldDescriptor {
                name: "metadata",
                stack_offset: 0,
                stack_size: 38,
                heapable: true,
                kind: FieldKind::Other,
            },
            FieldDescriptor {
                name: "qty",
                stack_offset: 38,
                stack_size: 8,
                heapable: false,
                kind: FieldKind::U64,
            },
            FieldDescriptor {
                name: "label",
                stack_offset: 46,
                stack_size: 4,
                heapable: true,
                kind: FieldKind::Str,
            },
        ];
        ObjectDescriptor {
            header: (9001 << 8) | 1,
            struct_id: 9001,
            version: 1,
            shape: Shape::NonUnique,
            type_name: "MetaTest1",
            stack_size: 50,
            fixed: false,
            fields: &FIELDS,
            heap_fields: &["metadata", "label"],
        }
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn eval_string_after_opaque_heapable_metadata() {
        let desc = metadata_like_descriptor();
        // metadata: 38 zero bytes (no heap contribution), qty, label slot.
        let mut body = vec![0u8; 38];
        body.extend_from_slice(&77u64.to_le_bytes());
        body.extend_from_slice(&(5u32).to_le_bytes());
        body.extend_from_slice(b"hello");

        assert!(eval(&desc, &body, &Expr::eq("label", "hello")));
        assert!(eval(
            &desc,
            &body,
            &Expr::and(Expr::gt("qty", 10u64), Expr::eq("label", "hello"))
        ));
        assert!(!eval(&desc, &body, &Expr::eq("label", "other")));

        // Same struct with metadata carrying heap bytes: label is still
        // located correctly from the back.
        let mut body2 = vec![0u8; 38];
        // Fake metadata heap region of 7 bytes (its length lives inside
        // the opaque slot — the evaluator must NOT need it).
        body2.extend_from_slice(&77u64.to_le_bytes());
        body2.extend_from_slice(&(2u32).to_le_bytes());
        body2.extend_from_slice(&[0xEE; 7]); // metadata heap
        body2.extend_from_slice(b"hi");
        assert!(eval(&desc, &body2, &Expr::eq("label", "hi")));
    }
}
