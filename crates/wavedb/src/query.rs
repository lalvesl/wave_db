//! Query expression DSL for `NonUnique` record lookups.
//!
//! Expressions are built combinatorially and serialised to bytes before
//! being sent to the Quick-Node.  The engine on the server side deserialises
//! them and applies them while scanning the per-`(STRUCT_ID, TENANT_ID)` index.
//!
//! # Columns
//!
//! Comparison constructors take a [`Field`] — a typed column handle. The
//! `#[wave_db]` macro emits one `const` per struct field, so a query names its
//! columns through the type (`Order::amount`) and a misspelt column is a
//! compile error rather than a silently-empty result. A `&'static str` literal
//! also converts into a `Field`, for dynamic or ad-hoc field names.
//!
//! # Example
//!
//! ```rust,ignore
//! use wavedb::query::Expr;
//!
//! // All orders where amount > 100 AND status == "shipped" — typed columns.
//! let filter = Expr::and(
//!     Order::amount.gt(100u64),
//!     Order::status.eq("shipped"),
//! );
//!
//! // The same, with string-literal field names (the escape hatch).
//! let filter = Expr::and(
//!     Expr::gt("amount", 100u64),
//!     Expr::eq("status", "shipped"),
//! );
//! ```

use wavedb_macros::WaveWire;

/// A field name used in an expression.
pub type FieldName = String;

/// A scalar value that can be compared in an expression.
///
/// Every primitive numeric width gets its own variant so a query value
/// round-trips at exactly the width the caller wrote — the server compares
/// against the record's field using the descriptor's
/// [`FieldKind`](::wavedb_core::FieldKind), no silent promotion.
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
    /// use wavedb::query::Expr;
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
    /// use wavedb::query::Expr;
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
    /// use wavedb::query::Expr;
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
    /// use wavedb::query::Expr;
    /// let e = Expr::field("amount").gt(100u64);
    /// assert!(e.to_bytes().is_ok());
    /// ```
    pub const fn field(name: &'static str) -> Field {
        Field::new(name)
    }

    /// Serialise this expression to bytes (wire format).
    pub fn to_bytes(&self) -> wavedb_core::Result<Vec<u8>> {
        Ok(wavedb_core::wire::to_wire(self)?)
    }

    /// Deserialise an expression from bytes (wire format).
    pub fn from_bytes(bytes: &[u8]) -> wavedb_core::Result<Self> {
        Ok(wavedb_core::wire::from_wire(bytes)?)
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
/// use wavedb::query::{Expr, Field};
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

#[cfg(test)]
mod tests {
    use core::f64;

    use super::*;

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
}
