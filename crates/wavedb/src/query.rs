//! Query expression DSL for `NonUnique` record lookups.
//!
//! Expressions are built combinatorially and serialised to bytes before
//! being sent to the Quick-Node.  The engine on the server side deserialises
//! them and applies them while scanning the per-`(STRUCT_ID, TENANT_ID)` index.
//!
//! # Example
//!
//! ```rust,ignore
//! use wavedb::query::Expr;
//!
//! // All orders where amount > 100 AND status == "shipped"
//! let filter = Expr::and(
//!     Expr::gt("amount", 100u64),
//!     Expr::eq("status", "shipped"),
//! );
//! ```

use serde::{Deserialize, Serialize};

/// A field name used in an expression.
pub type FieldName = String;

/// A scalar value that can be compared in an expression.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Value {
    /// Unsigned 64-bit integer.
    U64(u64),
    /// Signed 64-bit integer.
    I64(i64),
    /// 64-bit float.
    F64(f64),
    /// UTF-8 string.
    Str(String),
    /// Boolean.
    Bool(bool),
    /// Raw bytes.
    Bytes(Vec<u8>),
}

impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Self::U64(v)
    }
}
impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Self::U64(v.into())
    }
}
impl From<u16> for Value {
    fn from(v: u16) -> Self {
        Self::U64(v.into())
    }
}
impl From<u8> for Value {
    fn from(v: u8) -> Self {
        Self::U64(v.into())
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Self::I64(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Self::F64(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Str(v.to_owned())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

/// A query expression used to filter `NonUnique` records.
///
/// Expressions are composable and serialisable via postcard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// # Examples
    ///
    /// ```
    /// use wavedb::query::Expr;
    /// let e = Expr::eq("status", "active");
    /// let bytes = e.to_bytes().unwrap();
    /// let decoded = Expr::from_bytes(&bytes).unwrap();
    /// assert_eq!(e, decoded);
    /// ```
    pub fn eq(field: impl Into<FieldName>, value: impl Into<Value>) -> Self {
        Self::Eq {
            field: field.into(),
            value: value.into(),
        }
    }

    /// `field != value`
    pub fn ne(field: impl Into<FieldName>, value: impl Into<Value>) -> Self {
        Self::Ne {
            field: field.into(),
            value: value.into(),
        }
    }

    /// `field > value`
    pub fn gt(field: impl Into<FieldName>, value: impl Into<Value>) -> Self {
        Self::Gt {
            field: field.into(),
            value: value.into(),
        }
    }

    /// `field >= value`
    pub fn gte(field: impl Into<FieldName>, value: impl Into<Value>) -> Self {
        Self::Gte {
            field: field.into(),
            value: value.into(),
        }
    }

    /// `field < value`
    pub fn lt(field: impl Into<FieldName>, value: impl Into<Value>) -> Self {
        Self::Lt {
            field: field.into(),
            value: value.into(),
        }
    }

    /// `field <= value`
    pub fn lte(field: impl Into<FieldName>, value: impl Into<Value>) -> Self {
        Self::Lte {
            field: field.into(),
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

    /// Serialise this expression to bytes (postcard).
    pub fn to_bytes(&self) -> wavedb_core::Result<Vec<u8>> {
        Ok(postcard::to_allocvec(self)?)
    }

    /// Deserialise an expression from bytes (postcard).
    pub fn from_bytes(bytes: &[u8]) -> wavedb_core::Result<Self> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

#[cfg(test)]
mod tests {
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
        let expr = Expr::and(Expr::gt("amount", 100u64), Expr::eq("status", "shipped"));
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
        assert_eq!(Value::from(3.14f64), Value::F64(3.14));
        assert_eq!(Value::from("hello"), Value::Str("hello".into()));
        assert_eq!(Value::from(true), Value::Bool(true));
    }
}
