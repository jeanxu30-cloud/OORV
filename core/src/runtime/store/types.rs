use std::cmp::Ordering;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::ops;
use std::ops::{AddAssign, DivAssign, MulAssign, RemAssign, SubAssign};

use crate::oorvir::refined::Type;
use num::ToPrimitive;
use num_traits::Pow;
use ordered_float::NotNan;
use rust_decimal::Decimal;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use self::Value::*;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur when converting between `Value` and native types.
#[derive(Debug)]
pub enum ValueConvertError {
    /// The value variant does not match the expected target type.
    WrongType(Value),
    /// The byte slice cannot be decoded as UTF-8.
    InvalidUtf8(Vec<u8>),
    /// A string could not be parsed into the requested type.
    ParseFailed(Type, String),
    /// The value variant is not supported by this operation.
    Unsupported(Box<dyn std::fmt::Debug + Send>),
    /// A floating-point value is NaN, which is never allowed in `Value::Float`.
    NanNotAllowed,
}

impl Display for ValueConvertError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ValueConvertError::WrongType(val) => write!(f, "Cannot convert value: {val}"),
            ValueConvertError::InvalidUtf8(bytes) => {
                write!(f, "Expected UTF-8 encoded bytes, got: {bytes:?}")
            }
            ValueConvertError::ParseFailed(ty, text) => {
                write!(f, "Failed to parse '{text}' as type {ty}")
            }
            ValueConvertError::NanNotAllowed => {
                write!(f, "Float value is NaN, which is not a valid runtime value")
            }
            ValueConvertError::Unsupported(v) => {
                write!(f, "Value {v:?} is not supported by this operation")
            }
        }
    }
}

impl Error for ValueConvertError {}

// ---------------------------------------------------------------------------
// Arithmetic operator macros
// ---------------------------------------------------------------------------

/// Generates an owned binary operator trait impl for `Value`.
macro_rules! impl_binary_op {
    ($trait:ident, $method:ident, $op:tt) => {
        impl ops::$trait for Value {
            type Output = Value;
            fn $method(self, rhs: Value) -> Value {
                match (self, rhs) {
                    (Unsigned(a), Unsigned(b)) => Unsigned(a $op b),
                    (Signed(a),   Signed(b))   => Signed(a $op b),
                    (Float(a),    Float(b))    => Float(a $op b),
                    (Decimal(a),  Decimal(b))  => Decimal(a $op b),
                    (a, b) => panic!(
                        concat!(stringify!($method), ": incompatible operands {:?} and {:?}"),
                        a, b
                    ),
                }
            }
        }
    };
}

/// Generates a compound-assignment operator trait impl for `Value`.
macro_rules! impl_assign_op {
    ($trait:ident, $method:ident, $op:tt) => {
        impl $trait for Value {
            fn $method(&mut self, rhs: Value) {
                match (self, rhs) {
                    (Unsigned(a), Unsigned(b)) => *a $op b,
                    (Signed(a),   Signed(b))   => *a $op b,
                    (Float(a),    Float(b))    => *a $op b,
                    (Decimal(a),  Decimal(b))  => *a $op b,
                    (a, b) => panic!(
                        concat!(stringify!($method), ": incompatible operands {:?} and {:?}"),
                        a, b
                    ),
                }
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Core value type
// ---------------------------------------------------------------------------

/// Unified runtime value that can hold any data type produced or consumed by
/// stream expressions.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum Value {
    /// Absence of a value — propagates through expressions like an option type.
    None,
    /// A boolean flag.
    Bool(bool),
    /// An unsigned 64-bit integer.
    Unsigned(u64),
    /// A signed 64-bit integer.
    Signed(i64),
    /// A non-NaN double-precision floating-point number.
    Float(NotNan<f64>),
    /// A heterogeneous tuple; nested values may have different types.
    Tuple(Box<[Value]>),
    /// A UTF-8 encoded string.
    Str(Box<str>),
    /// Raw byte sequence.
    Bytes(Box<[u8]>),
    /// A signed fixed-point decimal number.
    Decimal(Decimal),
}

impl Display for Value {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            None => write!(f, "None"),
            Bool(b) => write!(f, "{b}"),
            Unsigned(u) => write!(f, "{u}"),
            Signed(s) => write!(f, "{s}"),
            Float(fl) => write!(f, "{fl}"),
            Tuple(t) => {
                write!(f, "(")?;
                if let Some(first) = t.first() {
                    write!(f, "{first}")?;
                    for elem in &t[1..] {
                        write!(f, ", {elem}")?;
                    }
                }
                write!(f, ")")
            }
            Str(s) => write!(f, "{s}"),
            Bytes(b) => write!(f, "{}", hex::encode_upper(b)),
            Decimal(d) => write!(f, "{d}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing and helper methods
// ---------------------------------------------------------------------------

impl Value {
    /// Parse a byte slice into a `Value` according to the target type `ty`.
    pub fn parse_bytes(source: &[u8], ty: &Type) -> Result<Value, ValueConvertError> {
        let text = std::str::from_utf8(source)
            .map_err(|_| ValueConvertError::InvalidUtf8(source.to_vec()))?;

        if text == "#" {
            return Ok(None);
        }

        match ty {
            Type::Bool => text
                .parse::<bool>()
                .map(Bool)
                .map_err(|_| ValueConvertError::ParseFailed(ty.clone(), text.to_string())),

            Type::Bytes => hex::decode(text)
                .map(|b| Bytes(b.into_boxed_slice()))
                .map_err(|_| ValueConvertError::ParseFailed(ty.clone(), text.to_string())),

            Type::Int(_) => text
                .parse::<i64>()
                .map(Signed)
                .map_err(|_| ValueConvertError::ParseFailed(ty.clone(), text.to_string())),

            Type::UInt(_) => {
                if text == "0.0" {
                    Ok(Unsigned(0))
                } else {
                    text.parse::<u64>()
                        .map(Unsigned)
                        .map_err(|_| ValueConvertError::ParseFailed(ty.clone(), text.to_string()))
                }
            }

            Type::Float(_) => text
                .parse::<f64>()
                .map_err(|_| ValueConvertError::ParseFailed(ty.clone(), text.to_string()))
                .and_then(Value::try_from),

            Type::String => Ok(Str(text.into())),

            Type::Tuple(inner) => {
                if inner.is_empty() {
                    (text == "()")
                        .then_some(Tuple(Box::new([])))
                        .ok_or_else(|| ValueConvertError::ParseFailed(ty.clone(), text.to_string()))
                } else {
                    unimplemented!("tuple parsing for non-empty tuples is not yet supported")
                }
            }

            Type::Option(_) | Type::Function { args: _, ret: _ } => {
                unreachable!("option/function types cannot appear as leaf parse targets")
            }

            Type::Fixed(_) | Type::UFixed(_) => text
                .parse::<Decimal>()
                .map(Decimal)
                .map_err(|_| ValueConvertError::ParseFailed(ty.clone(), text.to_string())),
        }
    }

    /// Returns `true` when this value is of variant `Bool`.
    pub(crate) fn is_boolean(&self) -> bool {
        matches!(self, Bool(_))
    }

    /// Extracts the inner boolean. Panics if the variant is not `Bool`.
    pub(crate) fn boolean_value(&self) -> bool {
        match self {
            Bool(b) => *b,
            _ => unreachable!("expected Bool variant, got {:?}", self),
        }
    }

    /// Returns `other` when `self` is `Value::None`, otherwise returns `self`.
    pub fn fallback(self, other: Value) -> Value {
        match self {
            None => other,
            _ => self,
        }
    }

    /// Wraps an `i64` into the `Value` variant that corresponds to `ty`.
    pub fn typed_integer(ty: &Type, val: i64) -> Value {
        match ty {
            Type::Int(_) => Signed(val),
            Type::UInt(_) => Unsigned(val as u64),
            Type::Float(_) => Float(NotNan::new(val as f64).unwrap()),
            Type::Fixed(_) | Type::UFixed(_) => Decimal(Decimal::from(val)),
            _ => unreachable!("typed_integer: unsupported type {:?}", ty),
        }
    }

    /// Coerces `val` into the same numeric variant as `self`.
    pub fn coerce_integer(&self, val: i64) -> Value {
        match self {
            Signed(_) => Signed(val),
            Unsigned(_) => Unsigned(val as u64),
            Float(_) => Float(NotNan::new(val as f64).unwrap()),
            Decimal(_) => Decimal(Decimal::from(val)),
            _ => unreachable!("coerce_integer: unsupported variant {:?}", self),
        }
    }
}

// ---------------------------------------------------------------------------
// Arithmetic operator impls
// ---------------------------------------------------------------------------

impl_binary_op!(Add, add, +);
impl_assign_op!(AddAssign, add_assign, +=);

impl_binary_op!(Sub, sub, -);
impl_assign_op!(SubAssign, sub_assign, -=);

impl_binary_op!(Mul, mul, *);
impl_assign_op!(MulAssign, mul_assign, *=);

impl_binary_op!(Div, div, /);
impl_assign_op!(DivAssign, div_assign, /=);

impl_binary_op!(Rem, rem, %);
impl_assign_op!(RemAssign, rem_assign, %=);

// ---------------------------------------------------------------------------
// Power, bitwise, shift, logical-not, and negation
// ---------------------------------------------------------------------------

impl Value {
    /// Raise `self` to the power `exp`.
    pub(crate) fn pow(self, exp: Value) -> Value {
        match (self, exp) {
            (Unsigned(b), Unsigned(e)) => Unsigned(b.pow(e as u32)),
            (Signed(b), Signed(e)) => Signed(b.pow(e as u32)),
            (Float(b), Float(e)) => Value::try_from(b.powf(e.into())).unwrap(),
            (Float(b), Signed(e)) => Value::try_from(b.powi(e as i32)).unwrap(),
            (Decimal(b), Decimal(e)) => Decimal(b.pow(e)),
            (a, b) => panic!("pow: incompatible operands {:?} and {:?}", a, b),
        }
    }
}

impl ops::BitAnd for Value {
    type Output = Value;
    fn bitand(self, rhs: Value) -> Value {
        match (self, rhs) {
            (Bool(a), Bool(b)) => Bool(a && b),
            (Unsigned(a), Unsigned(b)) => Unsigned(a & b),
            (Signed(a), Signed(b)) => Signed(a & b),
            (a, b) => panic!("bitand: incompatible operands {:?} and {:?}", a, b),
        }
    }
}

impl ops::BitOr for Value {
    type Output = Value;
    fn bitor(self, rhs: Value) -> Value {
        match (self, rhs) {
            (Bool(a), Bool(b)) => Bool(a || b),
            (Unsigned(a), Unsigned(b)) => Unsigned(a | b),
            (Signed(a), Signed(b)) => Signed(a | b),
            (a, b) => panic!("bitor: incompatible operands {:?} and {:?}", a, b),
        }
    }
}

impl ops::BitXor for Value {
    type Output = Value;
    fn bitxor(self, rhs: Value) -> Value {
        match (self, rhs) {
            (Unsigned(a), Unsigned(b)) => Unsigned(a ^ b),
            (Signed(a), Signed(b)) => Signed(a ^ b),
            (a, b) => panic!("bitxor: incompatible operands {:?} and {:?}", a, b),
        }
    }
}

impl ops::Shl for Value {
    type Output = Value;
    fn shl(self, rhs: Value) -> Value {
        match (self, rhs) {
            (Unsigned(a), Unsigned(b)) => Unsigned(a << b),
            (Signed(a), Unsigned(b)) => Signed(a << b),
            (a, b) => panic!("shl: incompatible operands {:?} and {:?}", a, b),
        }
    }
}

impl ops::Shr for Value {
    type Output = Value;
    fn shr(self, rhs: Value) -> Value {
        match (self, rhs) {
            (Unsigned(a), Unsigned(b)) => Unsigned(a >> b),
            (Signed(a), Unsigned(b)) => Signed(a >> b),
            (a, b) => panic!("shr: incompatible operands {:?} and {:?}", a, b),
        }
    }
}

impl ops::Not for Value {
    type Output = Value;
    fn not(self) -> Value {
        match self {
            Bool(v) => Bool(!v),
            Unsigned(u) => Unsigned(!u),
            Signed(s) => Signed(!s),
            a => panic!("not: unsupported variant {:?}", a),
        }
    }
}

impl ops::Neg for Value {
    type Output = Value;
    fn neg(self) -> Value {
        match self {
            Signed(v) => Signed(-v),
            Float(v) => Float(-v),
            Decimal(v) => Decimal(-v),
            a => panic!("neg: unsupported variant {:?}", a),
        }
    }
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Unsigned(a), Unsigned(b)) => a.cmp(b),
            (Signed(a), Signed(b)) => a.cmp(b),
            (Float(a), Float(b)) => a.cmp(b),
            (Str(a), Str(b)) => a.cmp(b),
            (Decimal(a), Decimal(b)) => a.cmp(b),
            (a, b) => panic!("cmp: incompatible variants {:?} and {:?}", a, b),
        }
    }
}

// ---------------------------------------------------------------------------
// Conversions: native types → Value (infallible)
// ---------------------------------------------------------------------------

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Bool(v)
    }
}
impl From<i16> for Value {
    fn from(v: i16) -> Self {
        Signed(v as i64)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Signed(v as i64)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Signed(v)
    }
}
impl From<u16> for Value {
    fn from(v: u16) -> Self {
        Unsigned(v as u64)
    }
}
impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Unsigned(v as u64)
    }
}
impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Unsigned(v)
    }
}
impl From<usize> for Value {
    fn from(v: usize) -> Self {
        Unsigned(v as u64)
    }
}
impl From<Decimal> for Value {
    fn from(v: Decimal) -> Self {
        Self::Decimal(v)
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Str(v.into_boxed_str())
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Str(v.into())
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Bytes(v.into_boxed_slice())
    }
}

impl From<&[u8]> for Value {
    fn from(v: &[u8]) -> Self {
        Bytes(v.into())
    }
}

impl TryFrom<f64> for Value {
    type Error = ValueConvertError;
    fn try_from(v: f64) -> Result<Self, Self::Error> {
        NotNan::try_from(v)
            .map(Float)
            .map_err(|_| ValueConvertError::NanNotAllowed)
    }
}

impl TryFrom<f32> for Value {
    type Error = ValueConvertError;
    fn try_from(v: f32) -> Result<Self, Self::Error> {
        NotNan::try_from(v as f64)
            .map(Float)
            .map_err(|_| ValueConvertError::NanNotAllowed)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(opt: Option<T>) -> Self {
        opt.map(T::into).unwrap_or(None)
    }
}

// ---------------------------------------------------------------------------
// Conversions: Value → native types (fallible)
// ---------------------------------------------------------------------------

impl TryInto<bool> for Value {
    type Error = ValueConvertError;
    fn try_into(self) -> Result<bool, Self::Error> {
        if let Bool(b) = self {
            Ok(b)
        } else {
            Err(ValueConvertError::WrongType(self))
        }
    }
}

impl TryInto<u64> for Value {
    type Error = ValueConvertError;
    fn try_into(self) -> Result<u64, Self::Error> {
        if let Unsigned(v) = self {
            Ok(v)
        } else {
            Err(ValueConvertError::WrongType(self))
        }
    }
}

impl TryInto<i64> for Value {
    type Error = ValueConvertError;
    fn try_into(self) -> Result<i64, Self::Error> {
        if let Signed(v) = self {
            Ok(v)
        } else {
            Err(ValueConvertError::WrongType(self))
        }
    }
}

impl TryInto<f64> for Value {
    type Error = ValueConvertError;
    fn try_into(self) -> Result<f64, Self::Error> {
        if let Float(v) = self {
            Ok(v.into_inner())
        } else {
            Err(ValueConvertError::WrongType(self))
        }
    }
}

impl TryInto<Box<[Value]>> for Value {
    type Error = ValueConvertError;
    fn try_into(self) -> Result<Box<[Value]>, Self::Error> {
        if let Tuple(v) = self {
            Ok(v)
        } else {
            Err(ValueConvertError::WrongType(self))
        }
    }
}

impl TryInto<Vec<Value>> for Value {
    type Error = ValueConvertError;
    fn try_into(self) -> Result<Vec<Value>, Self::Error> {
        if let Tuple(v) = self {
            Ok(v.to_vec())
        } else {
            Err(ValueConvertError::WrongType(self))
        }
    }
}

impl TryInto<Box<str>> for Value {
    type Error = ValueConvertError;
    fn try_into(self) -> Result<Box<str>, Self::Error> {
        if let Str(v) = self {
            Ok(v)
        } else {
            Err(ValueConvertError::WrongType(self))
        }
    }
}

impl TryInto<String> for Value {
    type Error = ValueConvertError;
    fn try_into(self) -> Result<String, Self::Error> {
        if let Str(v) = self {
            Ok(v.to_string())
        } else {
            Err(ValueConvertError::WrongType(self))
        }
    }
}

impl TryInto<Box<[u8]>> for Value {
    type Error = ValueConvertError;
    fn try_into(self) -> Result<Box<[u8]>, Self::Error> {
        if let Bytes(v) = self {
            Ok(v)
        } else {
            Err(ValueConvertError::WrongType(self))
        }
    }
}

impl TryInto<Vec<u8>> for Value {
    type Error = ValueConvertError;
    fn try_into(self) -> Result<Vec<u8>, Self::Error> {
        if let Bytes(v) = self {
            Ok(v.to_vec())
        } else {
            Err(ValueConvertError::WrongType(self))
        }
    }
}

impl TryInto<Decimal> for Value {
    type Error = ValueConvertError;
    fn try_into(self) -> Result<Decimal, Self::Error> {
        match self {
            Unsigned(v) => Ok(v.into()),
            Signed(v) => Ok(v.into()),
            Float(v) => Ok(v.to_f64().unwrap().try_into().unwrap()),
            Decimal(v) => Ok(v),
            other => Err(ValueConvertError::Unsupported(Box::new(other))),
        }
    }
}
