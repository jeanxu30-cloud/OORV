use std::cmp::max;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};

use crate::ast::SourceSpan;
use crate::diagnostic::{Diagnostic, OORVError};
use itertools::Itertools;
use rusttyc::{Arity, Constructable, Partial, TcErr, TcKey, TypeChecker, TypeTable, Variant};

use super::solver::{CheckFailure, FaultReporter, NodeRef};
use crate::oorvir::source::{StreamIdx, ValueTyped};

type AbstractValueType = TypeNode;
type ValErr = ValueError;

#[derive(Debug, Clone)]
pub(crate) enum ValueError {
    /// Two unification-incompatible types were merged.
    ValueConflict(TypeNode, TypeNode),
    /// A tuple element count mismatch was detected.
    TupleSizeMismatch(usize, usize),
    /// The resolved type exceeds the maximum representable width.
    WidthOverflow(TypeNode),
    /// The constraint system left the type under-determined.
    UnresolvableType(TypeNode),
    /// An annotated type width exceeds what the concrete system supports.
    AnnotationWidthExceeded(ValueTyped),
    /// An annotated type form is not accepted here.
    UnsupportedAnnotation(ValueTyped),
    /// The inferred concrete type does not match the declared bound.
    BoundViolation(DataType, DataType),
    /// A child index access fell outside the available tuple slots.
    TupleIndexOutOfRange(TypeNode, usize),
    /// The expected and inferred element counts disagree for a tuple type.
    TupleArityConflict(TypeNode, usize, usize),
    /// A child type could not be constructed inside its parent context.
    NestedConstructionError(Box<Self>, TypeNode, usize),
    /// More type arguments were supplied than the function declares.
    ExcessTypeParameter(SourceSpan),
    /// The inner expression of a widen call is wider than its declared target.
    WidenBoundViolation(DataType, DataType),
    /// An Option type appeared where it is not permitted.
    ForbiddenOptionType(DataType),
    /// The message argument of a constrain must be of type String.
    ExpectedStringMessage(DataType),
    /// Parameter count in a filtered lambda does not match the target stream.
    #[allow(dead_code)]
    LambdaParamCountMismatch(SourceSpan),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum TypeNode {
    Any,
    Numeric,
    SignedNumeric,
    FractionalNumeric,
    Integer,
    SInteger,
    SizedSInteger(u32),
    UInteger,
    SizedUInteger(u32),
    Float,
    SizedFloat(u32),
    Fixed,
    SizedFixed(u32, u32),
    UFixed,
    SizedUFixed(u32, u32),
    Bool,
    AnyTuple,
    Tuple(usize),
    Sequence,
    String,
    Bytes,
    Option,
}

impl Variant for TypeNode {
    type Err = ValueError;

    fn top() -> Self {
        Self::Any
    }

    fn meet(
        left: Partial<TypeNode>,
        right: Partial<TypeNode>,
    ) -> Result<Partial<TypeNode>, Self::Err> {
        use TypeNode::*;
        use ValueError::ValueConflict as Clash;

        // Helper: resolve meet for a fixed-size tuple where we know the exact arity.
        fn merge_tuple(min_count: usize, size: usize) -> Result<(TypeNode, usize), ValueError> {
            if min_count <= size {
                Ok((Tuple(size), size))
            } else {
                Err(ValueError::TupleSizeMismatch(min_count, size))
            }
        }

        // Helper: meet logic for purely numeric type nodes.
        fn numeric_lattice(
            a: TypeNode,
            b: TypeNode,
            orig_l: TypeNode,
            orig_r: TypeNode,
        ) -> Result<(TypeNode, usize), ValueError> {
            use TypeNode::*;
            let clash = || ValueError::ValueConflict(orig_l, orig_r);
            match (a, b) {
                (Numeric, Numeric) => Ok((Numeric, 0)),
                (SignedNumeric, SignedNumeric) => Ok((SignedNumeric, 0)),
                (Integer, Integer) => Ok((Integer, 0)),
                (SInteger, SInteger) => Ok((SInteger, 0)),
                (UInteger, UInteger) => Ok((UInteger, 0)),
                (Float, Float) => Ok((Float, 0)),
                (FractionalNumeric, FractionalNumeric) => Ok((FractionalNumeric, 0)),
                (SInteger, SizedSInteger(x)) | (SizedSInteger(x), SInteger) => {
                    Ok((SizedSInteger(x), 0))
                }
                (SizedSInteger(l), SizedSInteger(r)) if l == r => Ok((SizedSInteger(l), 0)),
                (SizedSInteger(_), SizedSInteger(_)) => Err(clash()),
                (UInteger, SizedUInteger(x)) | (SizedUInteger(x), UInteger) => {
                    Ok((SizedUInteger(x), 0))
                }
                (SizedUInteger(l), SizedUInteger(r)) if l == r => Ok((SizedUInteger(l), 0)),
                (SizedUInteger(_), SizedUInteger(_)) => Err(clash()),
                (Float, SizedFloat(x)) | (SizedFloat(x), Float) => Ok((SizedFloat(x), 0)),
                (SizedFloat(l), SizedFloat(r)) if l == r => Ok((SizedFloat(l), 0)),
                (SizedFloat(_), SizedFloat(_)) => Err(clash()),
                (Fixed, Fixed) => Ok((Fixed, 0)),
                (UFixed, UFixed) => Ok((UFixed, 0)),
                (Fixed, SizedFixed(t, f)) | (SizedFixed(t, f), Fixed) => Ok((SizedFixed(t, f), 0)),
                (SizedFixed(t1, f1), SizedFixed(t2, f2)) if t1 == t2 && f1 == f2 => {
                    Ok((SizedFixed(t1, f1), 0))
                }
                (SizedFixed(_, _), SizedFixed(_, _)) => Err(clash()),
                (UFixed, SizedUFixed(t, f)) | (SizedUFixed(t, f), UFixed) => {
                    Ok((SizedUFixed(t, f), 0))
                }
                (SizedUFixed(t1, f1), SizedUFixed(t2, f2)) if t1 == t2 && f1 == f2 => {
                    Ok((SizedUFixed(t1, f1), 0))
                }
                (SizedUFixed(_, _), SizedUFixed(_, _)) => Err(clash()),
                (Numeric, Integer) | (Integer, Numeric) => Ok((Integer, 0)),
                (Numeric, SInteger) | (SInteger, Numeric) => Ok((SInteger, 0)),
                (Numeric, SizedSInteger(w)) | (SizedSInteger(w), Numeric) => {
                    Ok((SizedSInteger(w), 0))
                }
                (Numeric, UInteger) | (UInteger, Numeric) => Ok((UInteger, 0)),
                (Numeric, SizedUInteger(w)) | (SizedUInteger(w), Numeric) => {
                    Ok((SizedUInteger(w), 0))
                }
                (Numeric, Float) | (Float, Numeric) => Ok((Float, 0)),
                (Numeric, SizedFloat(i)) | (SizedFloat(i), Numeric) => Ok((SizedFloat(i), 0)),
                (Numeric, SignedNumeric) | (SignedNumeric, Numeric) => Ok((SignedNumeric, 0)),
                (Numeric, FractionalNumeric) | (FractionalNumeric, Numeric) => {
                    Ok((FractionalNumeric, 0))
                }
                (SignedNumeric, SInteger) | (SInteger, SignedNumeric) => Ok((SInteger, 0)),
                (SignedNumeric, SizedSInteger(w)) | (SizedSInteger(w), SignedNumeric) => {
                    Ok((SizedSInteger(w), 0))
                }
                (SignedNumeric, Float) | (Float, SignedNumeric) => Ok((Float, 0)),
                (SignedNumeric, SizedFloat(w)) | (SizedFloat(w), SignedNumeric) => {
                    Ok((SizedFloat(w), 0))
                }
                (SignedNumeric | Numeric, Fixed) | (Fixed, SignedNumeric | Numeric) => {
                    Ok((Fixed, 0))
                }
                (SignedNumeric | Numeric, SizedFixed(t, f))
                | (SizedFixed(t, f), SignedNumeric | Numeric) => Ok((SizedFixed(t, f), 0)),
                (FractionalNumeric, SignedNumeric) | (SignedNumeric, FractionalNumeric) => {
                    Ok((FractionalNumeric, 0))
                }
                (Numeric, UFixed) | (UFixed, Numeric) => Ok((UFixed, 0)),
                (Numeric, SizedUFixed(t, f)) | (SizedUFixed(t, f), Numeric) => {
                    Ok((SizedUFixed(t, f), 0))
                }
                (FractionalNumeric, Float) | (Float, FractionalNumeric) => Ok((Float, 0)),
                (FractionalNumeric, SizedFloat(w)) | (SizedFloat(w), FractionalNumeric) => {
                    Ok((SizedFloat(w), 0))
                }
                (FractionalNumeric, Fixed) | (Fixed, FractionalNumeric) => Ok((Fixed, 0)),
                (FractionalNumeric, SizedFixed(t, f)) | (SizedFixed(t, f), FractionalNumeric) => {
                    Ok((SizedFixed(t, f), 0))
                }
                (FractionalNumeric, UFixed) | (UFixed, FractionalNumeric) => Ok((UFixed, 0)),
                (FractionalNumeric, SizedUFixed(t, f)) | (SizedUFixed(t, f), FractionalNumeric) => {
                    Ok((SizedUFixed(t, f), 0))
                }
                (Float | SizedFloat(_), Fixed | SizedFixed(_, _) | UFixed | SizedUFixed(_, _))
                | (Fixed | SizedFixed(_, _) | UFixed | SizedUFixed(_, _), Float | SizedFloat(_)) => {
                    Err(clash())
                }
                (Integer, SInteger) | (SInteger, Integer) => Ok((SInteger, 0)),
                (Integer, UInteger) | (UInteger, Integer) => Ok((UInteger, 0)),
                (Integer, SizedSInteger(x)) | (SizedSInteger(x), Integer) => {
                    Ok((SizedSInteger(x), 0))
                }
                (Integer, SizedUInteger(x)) | (SizedUInteger(x), Integer) => {
                    Ok((SizedUInteger(x), 0))
                }
                _ => Err(clash()),
            }
        }

        let (merged, child_count) = match (left.variant, right.variant) {
            (Any, other) => Ok((other, right.least_arity)),
            (other, Any) => Ok((other, left.least_arity)),
            (Bool, Bool) => Ok((Bool, 0)),
            (Bool, _) | (_, Bool) => Err(Clash(left.variant, right.variant)),
            (AnyTuple, AnyTuple) => Ok((AnyTuple, max(left.least_arity, right.least_arity))),
            (AnyTuple, Tuple(s)) => merge_tuple(left.least_arity, s),
            (Tuple(s), AnyTuple) => merge_tuple(right.least_arity, s),
            (AnyTuple, _) | (_, AnyTuple) => Err(Clash(left.variant, right.variant)),
            (Tuple(sl), Tuple(sr)) if sl == sr => Ok((Tuple(sl), sl)),
            (Tuple(sl), Tuple(sr)) => Err(ValueError::TupleSizeMismatch(sl, sr)),
            (Tuple(_), _) | (_, Tuple(_)) => Err(Clash(left.variant, right.variant)),
            (Sequence, String) | (String, Sequence) => Ok((String, 0)),
            (Sequence, Bytes) | (Bytes, Sequence) => Ok((Bytes, 0)),
            (Sequence, _) | (_, Sequence) => Err(Clash(left.variant, right.variant)),
            (String, String) => Ok((String, 0)),
            (String, _) | (_, String) => Err(Clash(left.variant, right.variant)),
            (Bytes, Bytes) => Ok((Bytes, 0)),
            (Bytes, _) | (_, Bytes) => Err(Clash(left.variant, right.variant)),
            (Option, Option) => Ok((Option, 1)),
            (Option, _) | (_, Option) => Err(Clash(left.variant, right.variant)),
            (l, r) => numeric_lattice(l, r, left.variant, right.variant),
        }?;

        Ok(Partial {
            variant: merged,
            least_arity: child_count,
        })
    }

    fn arity(&self) -> Arity {
        use TypeNode::*;
        match self {
            Any | AnyTuple => Arity::Variable,
            Tuple(x) => Arity::Fixed(*x),
            Option => Arity::Fixed(1),
            Numeric
            | SignedNumeric
            | Integer
            | SInteger
            | SizedSInteger(_)
            | UInteger
            | SizedUInteger(_)
            | Float
            | SizedFloat(_)
            | Fixed
            | SizedFixed(_, _)
            | UFixed
            | SizedUFixed(_, _)
            | FractionalNumeric
            | Bool
            | Sequence
            | String
            | Bytes => Arity::Fixed(0),
        }
    }
}

impl Constructable for TypeNode {
    type Type = DataType;

    fn construct(&self, children: &[DataType]) -> Result<DataType, ValueError> {
        use ValueError::UnresolvableType as Vague;
        use ValueError::WidthOverflow as TooWide;

        // Map a bit-width to the smallest fitting signed integer.
        fn pick_sint(w: u32) -> Result<DataType, ValueError> {
            let table: &[(u32, DataType)] = &[
                (8, DataType::Integer8),
                (16, DataType::Integer16),
                (32, DataType::Integer32),
                (64, DataType::Integer64),
                (128, DataType::Integer128),
                (256, DataType::Integer256),
            ];
            table
                .iter()
                .find(|(cap, _)| w <= *cap)
                .map(|(_, ty)| ty.clone())
                .ok_or_else(|| TooWide(TypeNode::SizedSInteger(w)))
        }

        // Map a bit-width to the smallest fitting unsigned integer.
        fn pick_uint(w: u32) -> Result<DataType, ValueError> {
            let table: &[(u32, DataType)] = &[
                (8, DataType::UInteger8),
                (16, DataType::UInteger16),
                (32, DataType::UInteger32),
                (64, DataType::UInteger64),
                (128, DataType::UInteger128),
                (256, DataType::UInteger256),
            ];
            table
                .iter()
                .find(|(cap, _)| w <= *cap)
                .map(|(_, ty)| ty.clone())
                .ok_or_else(|| TooWide(TypeNode::SizedUInteger(w)))
        }

        match self {
            TypeNode::Any => Err(Vague(*self)),
            TypeNode::AnyTuple => Err(Vague(*self)),
            TypeNode::Numeric => Err(Vague(*self)),
            TypeNode::SignedNumeric => Err(Vague(*self)),
            TypeNode::Sequence => Err(Vague(*self)),
            TypeNode::FractionalNumeric => Ok(DataType::Float64),
            TypeNode::Integer => Ok(DataType::Integer64),
            TypeNode::SInteger => Ok(DataType::Integer64),
            TypeNode::UInteger => Ok(DataType::UInteger64),
            TypeNode::SizedSInteger(w) => pick_sint(*w),
            TypeNode::SizedUInteger(w) => pick_uint(*w),
            TypeNode::Float => Ok(DataType::Float32),
            TypeNode::SizedFloat(w) if *w <= 32 => Ok(DataType::Float32),
            TypeNode::SizedFloat(w) if *w <= 64 => Ok(DataType::Float64),
            TypeNode::SizedFloat(_) => Err(TooWide(*self)),
            TypeNode::Fixed => Ok(DataType::Fixed64_32),
            TypeNode::SizedFixed(total, frac) if *total <= 16 && *frac <= 8 => {
                Ok(DataType::Fixed16_8)
            }
            TypeNode::SizedFixed(total, frac) if *total <= 32 && *frac <= 16 => {
                Ok(DataType::Fixed32_16)
            }
            TypeNode::SizedFixed(total, frac) if *total <= 64 && *frac <= 32 => {
                Ok(DataType::Fixed64_32)
            }
            TypeNode::SizedFixed(_, _) => Err(TooWide(*self)),
            TypeNode::UFixed => Ok(DataType::UFixed64_32),
            TypeNode::SizedUFixed(total, frac) if *total <= 16 && *frac <= 8 => {
                Ok(DataType::UFixed16_8)
            }
            TypeNode::SizedUFixed(total, frac) if *total <= 32 && *frac <= 16 => {
                Ok(DataType::UFixed32_16)
            }
            TypeNode::SizedUFixed(total, frac) if *total <= 64 && *frac <= 32 => {
                Ok(DataType::UFixed64_32)
            }
            TypeNode::SizedUFixed(_, _) => Err(TooWide(*self)),
            TypeNode::Bool => Ok(DataType::Bool),
            TypeNode::Tuple(_) => Ok(DataType::Tuple(children.to_vec())),
            TypeNode::String => Ok(DataType::TString),
            TypeNode::Bytes => Ok(DataType::Byte),
            TypeNode::Option => Ok(DataType::Option(Box::new(children[0].clone()))),
        }
    }
}

impl DataType {
    // Determine whether a declared bit-width fits within an upper limit.
    fn width_ok(declared: u32, limit: u32) -> bool {
        declared <= limit
    }

    /// Convert an annotated type expression into a concrete value type.
    pub(crate) fn resolve_annotation(at: &ValueTyped) -> Result<Self, ValueError> {
        use ValueError::{AnnotationWidthExceeded as TooWide, UnsupportedAnnotation as Invalid};
        match at {
            ValueTyped::String => Ok(DataType::TString),
            ValueTyped::Bool => Ok(DataType::Bool),
            ValueTyped::Bytes => Ok(DataType::Byte),
            ValueTyped::Float(w) if Self::width_ok(*w, 32) => Ok(DataType::Float32),
            ValueTyped::Float(w) if Self::width_ok(*w, 64) => Ok(DataType::Float64),
            ValueTyped::Float(_) => Err(TooWide(at.clone())),
            ValueTyped::Int(w) if Self::width_ok(*w, 8) => Ok(DataType::Integer8),
            ValueTyped::Int(w) if Self::width_ok(*w, 16) => Ok(DataType::Integer16),
            ValueTyped::Int(w) if Self::width_ok(*w, 32) => Ok(DataType::Integer32),
            ValueTyped::Int(w) if Self::width_ok(*w, 64) => Ok(DataType::Integer64),
            ValueTyped::Int(w) if Self::width_ok(*w, 128) => Ok(DataType::Integer128),
            ValueTyped::Int(w) if Self::width_ok(*w, 256) => Ok(DataType::Integer256),
            ValueTyped::Int(_) => Err(TooWide(at.clone())),
            ValueTyped::UInt(w) if Self::width_ok(*w, 8) => Ok(DataType::UInteger8),
            ValueTyped::UInt(w) if Self::width_ok(*w, 16) => Ok(DataType::UInteger16),
            ValueTyped::UInt(w) if Self::width_ok(*w, 32) => Ok(DataType::UInteger32),
            ValueTyped::UInt(w) if Self::width_ok(*w, 64) => Ok(DataType::UInteger64),
            ValueTyped::UInt(w) if Self::width_ok(*w, 128) => Ok(DataType::UInteger128),
            ValueTyped::UInt(w) if Self::width_ok(*w, 256) => Ok(DataType::UInteger256),
            ValueTyped::UInt(_) => Err(TooWide(at.clone())),
            ValueTyped::Fixed(t, f) if Self::width_ok(*t, 16) && Self::width_ok(*f, 8) => {
                Ok(DataType::Fixed16_8)
            }
            ValueTyped::Fixed(t, f) if Self::width_ok(*t, 32) && Self::width_ok(*f, 16) => {
                Ok(DataType::Fixed32_16)
            }
            ValueTyped::Fixed(t, f) if Self::width_ok(*t, 64) && Self::width_ok(*f, 32) => {
                Ok(DataType::Fixed64_32)
            }
            ValueTyped::Fixed(_, _) => Err(TooWide(at.clone())),
            ValueTyped::UFixed(t, f) if Self::width_ok(*t, 16) && Self::width_ok(*f, 8) => {
                Ok(DataType::UFixed16_8)
            }
            ValueTyped::UFixed(t, f) if Self::width_ok(*t, 32) && Self::width_ok(*f, 16) => {
                Ok(DataType::UFixed32_16)
            }
            ValueTyped::UFixed(t, f) if Self::width_ok(*t, 64) && Self::width_ok(*f, 32) => {
                Ok(DataType::UFixed64_32)
            }
            ValueTyped::UFixed(_, _) => Err(TooWide(at.clone())),
            ValueTyped::Tuple(elems) => {
                let resolved: Result<Vec<_>, _> =
                    elems.iter().map(DataType::resolve_annotation).collect();
                Ok(DataType::Tuple(resolved?))
            }
            ValueTyped::Option(inner) => {
                let inner_ty = DataType::resolve_annotation(inner)?;
                Ok(DataType::Option(Box::new(inner_ty)))
            }
            ValueTyped::Fractional
            | ValueTyped::Numeric
            | ValueTyped::Sequence
            | ValueTyped::Signed
            | ValueTyped::Any
            | ValueTyped::Param(..) => Err(Invalid(at.clone())),
        }
    }

    /// Return the storage bit-width of a numeric concrete type, or None for compound types.
    pub(crate) fn bit_width(&self) -> Option<usize> {
        use DataType::*;
        match self {
            Integer8 | UInteger8 => Some(8),
            Integer16 | UInteger16 => Some(16),
            Integer32 | UInteger32 => Some(32),
            Integer64 | UInteger64 => Some(64),
            Integer128 | UInteger128 => Some(128),
            Integer256 | UInteger256 => Some(256),
            Float32 => Some(32),
            Float64 => Some(64),
            Fixed16_8 | UFixed16_8 => Some(16),
            Fixed32_16 | UFixed32_16 => Some(32),
            Fixed64_32 | UFixed64_32 => Some(64),
            Bool | Tuple(_) | TString | Byte | Option(_) => None,
        }
    }
}

impl Display for TypeNode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        use TypeNode::*;
        match self {
            Any => write!(f, "Any"),
            Numeric => write!(f, "Numeric"),
            SignedNumeric => write!(f, "SignedNumeric"),
            FractionalNumeric => write!(f, "FractionalNumeric"),
            Integer => write!(f, "Integer"),
            SInteger => write!(f, "Int"),
            SizedSInteger(w) => write!(f, "Int{w}"),
            UInteger => write!(f, "UInt"),
            SizedUInteger(w) => write!(f, "UInt{w}"),
            Float => write!(f, "Float"),
            SizedFloat(w) => write!(f, "Float{w}"),
            Fixed => write!(f, "Fixed"),
            SizedFixed(t, fr) => write!(f, "Fixed{t}_{fr}"),
            UFixed => write!(f, "UFixed"),
            SizedUFixed(t, fr) => write!(f, "UFixed{t}_{fr}"),
            Bool => write!(f, "Bool"),
            AnyTuple => write!(f, "AnyTuple"),
            Tuple(n) => write!(f, "{n}Tuple"),
            Sequence => write!(f, "Sequence"),
            String => write!(f, "String"),
            Bytes => write!(f, "Bytes"),
            Option => write!(f, "Option<?>"),
        }
    }
}

impl Display for DataType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        use DataType::*;
        match self {
            Bool => write!(f, "Bool"),
            Integer8 => write!(f, "Int8"),
            Integer16 => write!(f, "Int16"),
            Integer32 => write!(f, "Int32"),
            Integer64 => write!(f, "Int64"),
            Integer128 => write!(f, "Int128"),
            Integer256 => write!(f, "Int256"),
            UInteger8 => write!(f, "UInt8"),
            UInteger16 => write!(f, "UInt16"),
            UInteger32 => write!(f, "UInt32"),
            UInteger64 => write!(f, "UInt64"),
            UInteger128 => write!(f, "UInt128"),
            UInteger256 => write!(f, "UInt256"),
            Float32 => write!(f, "Float32"),
            Float64 => write!(f, "Float64"),
            Fixed64_32 => write!(f, "Fixed64_32"),
            Fixed32_16 => write!(f, "Fixed32_16"),
            Fixed16_8 => write!(f, "Fixed16_8"),
            UFixed64_32 => write!(f, "UFixed64_32"),
            UFixed32_16 => write!(f, "UFixed32_16"),
            UFixed16_8 => write!(f, "UFixed16_8"),
            Tuple(elems) => write!(f, "({})", elems.iter().map(|c| c.to_string()).join(", ")),
            TString => write!(f, "String"),
            Byte => write!(f, "Byte"),
            Option(inner) => write!(f, "Option<{inner}>"),
        }
    }
}

impl From<TcErr<TypeNode>> for CheckFailure<ValueError> {
    fn from(err: TcErr<TypeNode>) -> Self {
        let (kind, k1, k2) = match err {
            TcErr::KeyEquation(a, b, e) => (e, Some(a), Some(b)),
            TcErr::Bound(a, b, e) => (e, Some(a), b),
            TcErr::ChildAccessOutOfBound(k, ty, ix) => {
                (ValueError::TupleIndexOutOfRange(ty, ix), Some(k), None)
            }
            TcErr::ArityMismatch {
                key,
                variant,
                inferred_arity,
                reported_arity,
            } => (
                ValueError::TupleArityConflict(variant, inferred_arity, reported_arity),
                Some(key),
                None,
            ),
            TcErr::Construction(k, _, e) => (e, Some(k), None),
            TcErr::ChildConstruction(k, idx, parent, e) => (
                ValueError::NestedConstructionError(Box::new(e), parent.variant, idx),
                Some(k),
                None,
            ),
            TcErr::CyclicGraph => panic!("Cyclic value type constraint graph detected"),
        };
        CheckFailure {
            kind,
            key1: k1,
            key2: k2,
        }
    }
}

impl FaultReporter for ValueError {
    fn into_diagnostic(
        self,
        spans: &[&HashMap<TcKey, SourceSpan>],
        _names: &HashMap<StreamIdx, String>,
        key1: Option<TcKey>,
        key2: Option<TcKey>,
    ) -> Diagnostic {
        let span_map = spans[0];
        let span_of = |k: Option<TcKey>| k.and_then(|x| span_map.get(&x).cloned());

        match self {
            ValueError::ValueConflict(ty1, ty2) => {
                let s1 = span_of(key1);
                let s2 = span_of(key2);
                Diagnostic::error(&format!(
                    "Type mismatch in value analysis: cannot unify '{ty1}' with '{ty2}'"
                ))
                .maybe_add_span_with_label(s1, Some(&format!("'{ty1}' inferred here")), true)
                .maybe_add_span_with_label(s2, Some(&format!("'{ty2}' inferred here")), false)
                .add_note(&format!("Consider inserting a type cast: cast<{ty1},{ty2}>(...)"))
            }
            ValueError::TupleSizeMismatch(a, b) => {
                let s1 = span_of(key1);
                let s2 = span_of(key2);
                Diagnostic::error(&format!(
                    "Tuple size conflict: expected {a} elements but encountered {b}"
                ))
                .maybe_add_span_with_label(s1, Some(&format!("size {a} here")), true)
                .maybe_add_span_with_label(s2, Some(&format!("size {b} here")), false)
            }
            ValueError::WidthOverflow(ty) => Diagnostic::error(&format!(
                "Type '{ty}' exceeds the maximum representable storage width"
            ))
            .maybe_add_span_with_label(span_of(key1), Some("declared here"), true),
            ValueError::UnresolvableType(ty) => Diagnostic::error(&format!(
                "Cannot determine a concrete type for '{ty}'; consider adding an explicit annotation"
            ))
            .maybe_add_span_with_label(span_of(key1), Some("ambiguous expression"), true)
            .add_note("A type annotation such as `: Int32` will resolve the ambiguity"),
            ValueError::AnnotationWidthExceeded(ty) => Diagnostic::error(&format!(
                "Annotated type '{ty}' is wider than any supported concrete type"
            ))
            .maybe_add_span_with_label(span_of(key1), Some("annotation here"), true),
            ValueError::UnsupportedAnnotation(ty) => Diagnostic::error(&format!(
                "The type '{ty}' is not valid as an explicit annotation in this position"
            ))
            .maybe_add_span_with_label(span_of(key1), Some("invalid annotation"), true)
            .add_note("Use a concrete type such as Int32, Float64, or Bool"),
            ValueError::BoundViolation(got, expected) => {
                let s1 = span_of(key1);
                let s2 = span_of(key2);
                Diagnostic::error(&format!(
                    "Declared type '{expected}' is incompatible with inferred type '{got}'"
                ))
                .maybe_add_span_with_label(s1, Some(&format!("declared as '{expected}'")), false)
                .maybe_add_span_with_label(s2, Some(&format!("inferred as '{got}'")), true)
                .add_note(&format!("Consider inserting: cast<{got},{expected}>(...)"))
            }
            ValueError::TupleIndexOutOfRange(ty, idx) => Diagnostic::error(&format!(
                "Tuple index {} is out of range for type '{ty}'",
                idx.saturating_sub(1)
            ))
            .maybe_add_span_with_label(span_of(key1), Some("access here"), true),
            ValueError::TupleArityConflict(ty, got, expected) => Diagnostic::error(&format!(
                "Expected '{ty}' to have {expected} element(s) but {got} were inferred"
            ))
            .maybe_add_span_with_label(span_of(key1), Some("tuple construction"), true),
            ValueError::NestedConstructionError(inner, parent, idx) => {
                let desc = match inner.as_ref() {
                    ValueError::WidthOverflow(t)    => format!("'{t}' is too wide"),
                    ValueError::UnresolvableType(t) => format!("'{t}' is ambiguous"),
                    _ => "unknown reason".to_string(),
                };
                Diagnostic::error(&format!(
                    "Cannot build element {idx} of '{parent}': {desc}"
                ))
                .maybe_add_span_with_label(span_of(key1), Some("here"), true)
            }
            ValueError::ExcessTypeParameter(span) => Diagnostic::error(
                "More type arguments were supplied than this function declares as generic parameters",
            )
            .add_span_with_label(span, Some("extra argument here"), true),
            ValueError::WidenBoundViolation(target, actual) => {
                let target_w = target.bit_width().unwrap_or(0);
                let actual_w = actual.bit_width().unwrap_or(0);
                let s1 = span_of(key1);
                let s2 = span_of(key2);
                Diagnostic::error(&format!(
                    "Widen target has width {target_w} but inner expression has width {actual_w}"
                ))
                .maybe_add_span_with_label(s1, Some(&format!("widen to '{target}' here")), false)
                .maybe_add_span_with_label(s2, Some(&format!("inner type '{actual}' here")), true)
            }
            ValueError::ForbiddenOptionType(ty) => Diagnostic::error(
                "An optional value is not permitted at this position",
            )
            .maybe_add_span_with_label(span_of(key1), Some(&format!("has type '{ty}'")), true)
            .add_note("Unwrap the option with the default operator before using it here"),
            ValueError::ExpectedStringMessage(ty) => Diagnostic::error(&format!(
                "A constrain message must be of type String, but '{ty}' was found"
            ))
            .maybe_add_span_with_label(span_of(key1), Some("message expression"), true),
            ValueError::LambdaParamCountMismatch(span) => Diagnostic::error(
                "Parameter count in the filtered instance does not match the target stream",
            )
            .add_span_with_label(span, None::<&str>, true),
        }
    }
}

use crate::oorvir::source::{
    AccessMode, Constant, Constraint, DataType, ExprVariant, Expression, FnExprKind, InitView,
    Inlined, Literal, OORVIr1, Shift, Signal, WidenExprKind,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TypeVar(String);

impl rusttyc::TcVar for TypeVar {}

impl TypeVar {
    /// Build a TypeVar that identifies parameter `idx` of `stream`.
    fn from_param(stream: &Constraint, idx: usize) -> Self {
        let label = format!("{}__{}", stream.name(), stream.params[idx].name);
        TypeVar(label)
    }
}

pub(crate) struct ValueAnalyzer<'a> {
    pub(crate) type_checker: TypeChecker<AbstractValueType, TypeVar>,
    pub(crate) key_map: HashMap<NodeRef, TcKey>,
    pub(crate) scope_stack: Vec<HashMap<String, TcKey>>,
    pub(crate) span_map: HashMap<TcKey, SourceSpan>,
    pub(crate) spec: &'a OORVIr1,
    pub(crate) bounds: HashMap<TcKey, (DataType, Option<TcKey>)>,
    pub(crate) widen_bounds: HashMap<TcKey, (DataType, TcKey)>,
    pub(crate) names: &'a HashMap<StreamIdx, String>,
}

impl<'a> ValueAnalyzer<'a> {
    /// Allocate RustTyc keys for every signal, constraint, and constrain stream
    /// so that subsequent passes can look them up by `NodeRef`.
    pub(crate) fn new(spec: &'a OORVIr1, names: &'a HashMap<StreamIdx, String>) -> Self {
        let mut tc = TypeChecker::new();
        let mut keys: HashMap<NodeRef, TcKey> = HashMap::new();
        let mut spans: HashMap<TcKey, SourceSpan> = HashMap::new();

        for sig in spec.signals() {
            let k = tc.get_var_key(&TypeVar(sig.name.clone()));
            keys.insert(NodeRef::StreamIdx(sig.si), k);
            spans.insert(k, sig.span);
        }

        for con in spec.constraints() {
            let k = tc.get_var_key(&TypeVar(con.name()));
            keys.insert(NodeRef::StreamIdx(con.si), k);
            spans.insert(k, con.span);
        }

        for (n, tr) in spec.constrains().enumerate() {
            let label = format!("constrain_{n}");
            let k = tc.get_var_key(&TypeVar(label));
            keys.insert(NodeRef::StreamIdx(tr.si), k);
            spans.insert(k, tr.span);
        }

        ValueAnalyzer {
            type_checker: tc,
            key_map: keys,
            span_map: spans,
            spec,
            bounds: HashMap::new(),
            widen_bounds: HashMap::new(),
            scope_stack: Vec::new(),
            names,
        }
    }

    /// Run the full four-pass inference pipeline and return the resolved type map.
    pub(crate) fn analyze(mut self) -> Result<HashMap<NodeRef, DataType>, OORVError> {
        self.collect_signal_types()?;
        self.collect_constraint_types()?;

        // Infer types for free-standing function bodies (library functions etc.)
        for (name, eid) in self.spec.func_bodies() {
            let expr = self.spec.expression(*eid);
            let body_key = self
                .analyze_expr(expr, None)
                .map_err(|e| e.into_diagnostic(&[&self.span_map], self.names))?;

            let decl = match self.spec.func_declaration_opt(name) {
                Some(d) => d,
                None => {
                    println!("[warning] function {name} has a body but no declaration");
                    continue;
                }
            };

            let param_keys: Vec<TcKey> = decl
                .type_params
                .iter()
                .map(|gen| {
                    let gk = self.type_checker.new_term_key();
                    self.impose_annotation(gk, gen).map(|_| gk)
                })
                .collect::<Result<Vec<_>, CheckFailure<ValErr>>>()
                .map_err(|e| e.into_diagnostic(&[&self.span_map], self.names))?;

            let ret_key = self
                .instantiate_param_type(&decl.return_ty, &param_keys)
                .map_err(|e| e.into_diagnostic(&[&self.span_map], self.names))?;

            self.type_checker
                .impose(body_key.concretizes(ret_key))
                .map_err(|e| {
                    CheckFailure::from(e).into_diagnostic(&[&self.span_map], self.names)
                })?;
        }

        // Solve the constraint system.
        let table =
            self.type_checker.clone().type_check().map_err(|e| {
                CheckFailure::from(e).into_diagnostic(&[&self.span_map], self.names)
            })?;

        let mut errs = OORVError::new();
        for f in Self::validate_exact_bounds(self.bounds.clone(), &table) {
            errs.add(f.into_diagnostic(&[&self.span_map], self.names));
        }
        for f in Self::validate_widen_ops(self.widen_bounds.clone(), &table) {
            errs.add(f.into_diagnostic(&[&self.span_map], self.names));
        }
        for f in Self::finalize_checks(self.spec, &self.key_map, &table) {
            errs.add(f.into_diagnostic(&[&self.span_map], self.names));
        }
        Result::from(errs)?;

        let resolved = self
            .key_map
            .into_iter()
            .map(|(node, key)| (node, table[&key].clone()))
            .collect();
        Ok(resolved)
    }

    /// Pass 2: propagate annotated types from all signal (input) stream declarations.
    fn collect_signal_types(&mut self) -> Result<(), OORVError> {
        for sig in self.spec.signals() {
            self.analyze_signal(sig)
                .map_err(|e| e.into_diagnostic(&[&self.span_map], self.names))?;
        }
        Ok(())
    }

    /// Pass 3: propagate type constraints from all constraint (output) stream declarations.
    fn collect_constraint_types(&mut self) -> Result<(), OORVError> {
        for con in self.spec.constraints() {
            self.analyze_constraint(con)
                .map_err(|e| e.into_diagnostic(&[&self.span_map], self.names))?;
        }
        Ok(())
    }

    /// Constrain `target` to the type implied by a constant literal.
    fn constrain_literal(
        &mut self,
        lit: &Literal,
        target: TcKey,
    ) -> Result<(), CheckFailure<ValErr>> {
        let node = match lit {
            Literal::Str(_) => AbstractValueType::String,
            Literal::Bool(_) => AbstractValueType::Bool,
            Literal::UInt(_) => AbstractValueType::Integer,
            Literal::SInt(_) => AbstractValueType::SInteger,
            Literal::Decimal(_) => AbstractValueType::FractionalNumeric,
        };
        self.type_checker
            .impose(target.concretizes_explicit(node))?;
        Ok(())
    }

    /// Record an exact-type bound: after solving, the resolved type of `target`
    /// must equal `bound` (or a type error is reported).
    fn register_exact_bound(
        &mut self,
        target: TcKey,
        bound: &ValueTyped,
        conflict_key: Option<TcKey>,
    ) -> Result<(), CheckFailure<ValErr>> {
        let concrete = DataType::resolve_annotation(bound).map_err(|e| CheckFailure {
            kind: e,
            key1: Some(target),
            key2: None,
        })?;
        self.bounds.insert(target, (concrete, conflict_key));
        Ok(())
    }

    /// Register a widen-consistency check: the inner expression must not be
    /// wider than the declared widen target.
    fn register_widen_bound(
        &mut self,
        result_key: TcKey,
        inner_key: TcKey,
        ty: &ValueTyped,
    ) -> Result<(), CheckFailure<ValErr>> {
        let concrete = DataType::resolve_annotation(ty).map_err(|e| CheckFailure {
            kind: e,
            key1: Some(result_key),
            key2: None,
        })?;
        self.widen_bounds.insert(inner_key, (concrete, result_key));
        Ok(())
    }

    /// Impose RustTyc lattice constraints on `target` derived from the
    /// annotated type `ty`, without recording an exact-bound check.
    fn impose_annotation(
        &mut self,
        target: TcKey,
        ty: &ValueTyped,
    ) -> Result<(), CheckFailure<ValErr>> {
        match ty {
            ValueTyped::String => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::String))?,
            ValueTyped::Int(0) => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::SInteger))?,
            ValueTyped::Int(x) => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::SizedSInteger(*x)))?,
            ValueTyped::Float(0) => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::Float))?,
            ValueTyped::Float(f) => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::SizedFloat(*f)))?,
            ValueTyped::UInt(0) => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::UInteger))?,
            ValueTyped::UInt(u) => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::SizedUInteger(*u)))?,
            ValueTyped::Fixed(0, 0) => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::Fixed))?,
            ValueTyped::Fixed(t, f) => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::SizedFixed(*t, *f)))?,
            ValueTyped::UFixed(0, 0) => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::UFixed))?,
            ValueTyped::UFixed(t, f) => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::SizedUFixed(*t, *f)))?,
            ValueTyped::Bool => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::Bool))?,
            ValueTyped::Bytes => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::Bytes))?,
            ValueTyped::Option(inner) => {
                self.type_checker
                    .impose(target.concretizes_explicit(AbstractValueType::Option))?;
                let child = self.type_checker.get_child_key(target, 0)?;
                self.impose_annotation(child, inner.as_ref())?;
            }
            ValueTyped::Tuple(elems) => {
                self.type_checker
                    .impose(target.concretizes_explicit(AbstractValueType::Tuple(elems.len())))?;
                for (i, elem) in elems.iter().enumerate() {
                    let ck = self.type_checker.get_child_key(target, i)?;
                    self.impose_annotation(ck, elem)?;
                }
            }
            ValueTyped::Numeric => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::Numeric))?,
            ValueTyped::Signed => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::SignedNumeric))?,
            ValueTyped::Fractional => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::FractionalNumeric))?,
            ValueTyped::Sequence => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::Sequence))?,
            ValueTyped::Any => self
                .type_checker
                .impose(target.concretizes_explicit(AbstractValueType::Any))?,
            ValueTyped::Param(_, _) => {
                unreachable!("Param type only valid inside function call resolution")
            }
        }
        Ok(())
    }

    /// Apply a full annotation to `target`: impose lattice constraints AND
    /// register an exact-bound check.  `conflict` is the key whose resolved
    /// type will be reported alongside any mismatch error.
    fn apply_annotation(
        &mut self,
        target: TcKey,
        ty: &ValueTyped,
        conflict: Option<TcKey>,
    ) -> Result<(), CheckFailure<ValErr>> {
        self.impose_annotation(target, ty)?;
        self.register_exact_bound(target, ty, conflict)
    }

    /// Constrain the type of a signal (input) stream from its annotation.
    pub(crate) fn analyze_signal(&mut self, sig: &Signal) -> Result<TcKey, CheckFailure<ValErr>> {
        let key = *self
            .key_map
            .get(&NodeRef::StreamIdx(sig.si))
            .expect("key allocated in constructor");
        self.apply_annotation(key, &sig.ty, None)?;
        Ok(key)
    }

    /// Constrain all types for a constraint (output) stream:
    /// parameters, start expression, end condition, eval conditions, body.
    pub(crate) fn analyze_constraint(
        &mut self,
        out: &Constraint,
    ) -> Result<TcKey, CheckFailure<ValErr>> {
        let out_key = *self
            .key_map
            .get(&NodeRef::StreamIdx(out.si))
            .expect("key allocated in constructor");

        // Parameters
        let param_keys: Vec<TcKey> = out
            .params
            .iter()
            .map(|param| {
                let pk = self
                    .type_checker
                    .get_var_key(&TypeVar::from_param(out, param.position));
                self.key_map
                    .insert(NodeRef::Param(param.position, out.si), pk);
                self.span_map.insert(pk, param.span);
                if let Some(ann) = param.ty.as_ref() {
                    self.apply_annotation(pk, ann, None)?;
                }
                Ok(pk)
            })
            .collect::<Result<Vec<_>, CheckFailure<ValErr>>>()?;

        // Start definition
        if let Some(InitView {
            expression,
            condition,
            ..
        }) = &self.spec.start(out.si)
        {
            if let Some(start_expr) = expression {
                let start_key = self.analyze_expr(start_expr, None)?;
                match param_keys.len() {
                    0 => unreachable!("ensured by pacing type checker"),
                    1 => {
                        self.type_checker
                            .impose(start_key.equate_with(param_keys[0]))?;
                    }
                    _ => {
                        self.type_checker
                            .impose(start_key.concretizes_explicit(AbstractValueType::Tuple(
                                param_keys.len(),
                            )))?;
                        let tuple_key = self.type_checker.new_term_key();
                        for (i, pk) in param_keys.iter().enumerate() {
                            let ck = self.type_checker.get_child_key(tuple_key, i)?;
                            self.type_checker.impose(ck.equate_with(*pk))?;
                        }
                        self.type_checker.impose(tuple_key.equate_with(start_key))?;
                    }
                }
            }
            if let Some(cond) = condition {
                self.analyze_expr(cond, Some(AbstractValueType::Bool))?;
            }
        }

        // End condition
        if let Some(ccond) = &self.spec.end_cond(out.si) {
            self.analyze_expr(ccond, Some(AbstractValueType::Bool))?;
        }

        // Eval conditions
        for cond in self.spec.eval_cond(out.si).unwrap().iter().flatten() {
            self.analyze_expr(cond, Some(AbstractValueType::Bool))?;
        }

        // Eval body expressions
        for ev in self.spec.eval_unchecked(out.si) {
            let expr_key = self.analyze_expr(ev.expression, None)?;
            if let Some(ann) = out.ty.as_ref() {
                self.apply_annotation(out_key, ann, Some(expr_key))?;
            }
            self.type_checker.impose(out_key.equate_with(expr_key))?;
        }

        Ok(out_key)
    }

    /// Walk a single expression node and return its freshly-allocated TcKey.
    fn analyze_expr(
        &mut self,
        exp: &Expression,
        hint: Option<AbstractValueType>,
    ) -> Result<TcKey, CheckFailure<ValErr>> {
        let tk = self.type_checker.new_term_key();
        self.key_map.insert(NodeRef::Expr(exp.eid), tk);
        self.span_map.insert(tk, exp.span);

        if let Some(h) = hint {
            self.type_checker.impose(tk.concretizes_explicit(h))?;
        }

        match &exp.kind {
            ExprVariant::LoadConstant(c) => {
                let lit = match c {
                    Constant::Basic(l) => l,
                    Constant::Inlined(Inlined { lit: l, ty: ann }) => {
                        self.apply_annotation(tk, ann, None)?;
                        l
                    }
                };
                self.constrain_literal(lit, tk)?;
            }

            ExprVariant::StreamAccess(si, mode, args) => {
                if si.is_input() {
                    assert!(args.is_empty(), "parametrized inputs are unsupported");
                }

                if !args.is_empty() {
                    let target_stream = self.spec.output(*si).expect("stream ref must be valid");
                    let param_ks: Vec<_> = target_stream
                        .params
                        .iter()
                        .map(|p| {
                            self.type_checker
                                .get_var_key(&TypeVar::from_param(target_stream, p.position))
                        })
                        .collect();

                    let arg_ks: Vec<TcKey> = args
                        .iter()
                        .map(|a| self.analyze_expr(a, None))
                        .collect::<Result<Vec<_>, _>>()?;

                    for (pk, ak) in param_ks.iter().zip(arg_ks.iter()) {
                        self.type_checker.impose(ak.equate_with(*pk))?;
                    }
                }

                let stream_key = *self
                    .key_map
                    .get(&NodeRef::StreamIdx(*si))
                    .expect("allocated in constructor");

                match mode {
                    AccessMode::Strict => {
                        self.type_checker.impose(tk.equate_with(stream_key))?;
                    }
                    AccessMode::Cached | AccessMode::Get => {
                        self.type_checker
                            .impose(tk.concretizes_explicit(AbstractValueType::Option))?;
                        let inner = self.type_checker.get_child_key(tk, 0)?;
                        self.type_checker.impose(stream_key.equate_with(inner))?;
                    }
                    AccessMode::Shift(off) => match off {
                        Shift::PastDiscrete(_) => {
                            self.type_checker
                                .impose(tk.concretizes_explicit(AbstractValueType::Option))?;
                            let inner = self.type_checker.get_child_key(tk, 0)?;
                            self.type_checker.impose(stream_key.equate_with(inner))?;
                        }
                        Shift::FutureRealTime(_) | Shift::FutureDiscrete(_) => {
                            panic!("future offsets are unsupported")
                        }
                        Shift::PastRealTime(_) => {
                            panic!("real-time offsets are not yet supported")
                        }
                    },
                    AccessMode::Fresh => {
                        self.type_checker
                            .impose(tk.concretizes_explicit(AbstractValueType::Bool))?;
                    }
                }
            }

            ExprVariant::Default { expr, default } => {
                let opt_key = if matches!(expr.kind, ExprVariant::TupleAccess(_, _)) {
                    self.analyze_tuple_option(expr.as_ref())?
                } else {
                    self.analyze_expr(expr, None)?
                };
                let def_key = self.analyze_expr(default, None)?;
                self.type_checker
                    .impose(opt_key.concretizes_explicit(AbstractValueType::Option))?;
                let inner = self.type_checker.get_child_key(opt_key, 0)?;
                self.type_checker
                    .impose(tk.is_sym_meet_of(def_key, inner))?;
            }

            ExprVariant::ArithLog(op, operands) => {
                use crate::oorvir::source::ArithLogOp;
                let op_keys: Vec<TcKey> = operands
                    .iter()
                    .map(|e| self.analyze_expr(e, None))
                    .collect::<Result<Vec<_>, _>>()?;

                match op_keys.len() {
                    2 => {
                        let (lk, rk) = (op_keys[0], op_keys[1]);
                        match op {
                            ArithLogOp::Add
                            | ArithLogOp::Sub
                            | ArithLogOp::Mul
                            | ArithLogOp::Div
                            | ArithLogOp::Rem
                            | ArithLogOp::Pow => {
                                self.type_checker
                                    .impose(lk.concretizes_explicit(AbstractValueType::Numeric))?;
                                self.type_checker
                                    .impose(rk.concretizes_explicit(AbstractValueType::Numeric))?;
                                self.type_checker.impose(tk.is_meet_of(lk, rk))?;
                                self.type_checker.impose(tk.equate_with(lk))?;
                                self.type_checker.impose(tk.equate_with(rk))?;
                            }
                            ArithLogOp::Shl
                            | ArithLogOp::Shr
                            | ArithLogOp::BitAnd
                            | ArithLogOp::BitOr
                            | ArithLogOp::BitXor => {
                                self.type_checker
                                    .impose(lk.concretizes_explicit(AbstractValueType::Integer))?;
                                self.type_checker
                                    .impose(rk.concretizes_explicit(AbstractValueType::Integer))?;
                                self.type_checker.impose(tk.is_meet_of(lk, rk))?;
                                self.type_checker.impose(tk.equate_with(lk))?;
                                self.type_checker.impose(tk.equate_with(rk))?;
                            }
                            ArithLogOp::And | ArithLogOp::Or => {
                                self.type_checker
                                    .impose(lk.concretizes_explicit(AbstractValueType::Bool))?;
                                self.type_checker
                                    .impose(rk.concretizes_explicit(AbstractValueType::Bool))?;
                                self.type_checker
                                    .impose(tk.concretizes_explicit(AbstractValueType::Bool))?;
                            }
                            ArithLogOp::Eq
                            | ArithLogOp::Lt
                            | ArithLogOp::Le
                            | ArithLogOp::Ne
                            | ArithLogOp::Ge
                            | ArithLogOp::Gt => {
                                self.type_checker.impose(lk.equate_with(rk))?;
                                self.type_checker
                                    .impose(tk.concretizes_explicit(AbstractValueType::Bool))?;
                            }
                            ArithLogOp::Not | ArithLogOp::Neg | ArithLogOp::BitNot => {
                                unreachable!("unary op with two arguments")
                            }
                        }
                    }
                    1 => {
                        let ok = op_keys[0];
                        match op {
                            ArithLogOp::Not => {
                                self.type_checker
                                    .impose(ok.concretizes_explicit(AbstractValueType::Bool))?;
                                self.type_checker
                                    .impose(tk.concretizes_explicit(AbstractValueType::Bool))?;
                            }
                            ArithLogOp::Neg => {
                                self.type_checker
                                    .impose(ok.concretizes_explicit(AbstractValueType::Numeric))?;
                                self.type_checker.impose(tk.equate_with(ok))?;
                            }
                            ArithLogOp::BitNot => {
                                self.type_checker
                                    .impose(ok.concretizes_explicit(AbstractValueType::Integer))?;
                                self.type_checker.impose(tk.equate_with(ok))?;
                            }
                            _ => unreachable!("binary op with one argument"),
                        }
                    }
                    _ => unreachable!(),
                }
            }

            ExprVariant::Ite {
                condition,
                consequence,
                alternative,
            } => {
                self.analyze_expr(condition, Some(AbstractValueType::Bool))?;
                let then_key = self.analyze_expr(consequence, None)?;
                let else_key = self.analyze_expr(alternative, None)?;
                self.type_checker
                    .impose(tk.is_sym_meet_of(then_key, else_key))?;
            }

            ExprVariant::Tuple(items) => {
                let child_keys: Vec<TcKey> = items
                    .iter()
                    .map(|e| self.analyze_expr(e, None))
                    .collect::<Result<Vec<_>, _>>()?;
                self.type_checker
                    .impose(tk.concretizes_explicit(AbstractValueType::Tuple(items.len())))?;
                for (i, ck) in child_keys.iter().enumerate() {
                    let slot = self.type_checker.get_child_key(tk, i)?;
                    self.type_checker.impose(slot.equate_with(*ck))?;
                }
            }

            ExprVariant::TupleAccess(inner, idx) => {
                let inner_key = self.analyze_expr(inner, None)?;
                self.type_checker
                    .impose(inner_key.concretizes_explicit(AbstractValueType::AnyTuple))?;
                let elem = self.type_checker.get_child_key(inner_key, *idx)?;
                self.type_checker.impose(tk.equate_with(elem))?;
            }

            ExprVariant::Widen(WidenExprKind { expr: inner, ty }) => {
                let inner_key = self.analyze_expr(inner, None)?;
                let upper = match ty {
                    ValueTyped::UInt(_) => AbstractValueType::UInteger,
                    ValueTyped::Int(_) => AbstractValueType::SInteger,
                    ValueTyped::Float(_) => AbstractValueType::Float,
                    _ => unimplemented!("unsupported widen target type"),
                };
                self.apply_annotation(tk, ty, Some(inner_key))?;
                self.type_checker
                    .impose(inner_key.concretizes_explicit(upper))?;
                self.register_widen_bound(tk, inner_key, ty)?;
            }

            ExprVariant::Function(FnExprKind {
                name,
                type_param,
                args,
            }) => {
                let decl = self.spec.func_declaration(name);

                if type_param.len() > decl.type_params.len() {
                    return Err(ValErr::ExcessTypeParameter(exp.span).into());
                }

                let gen_keys: Vec<TcKey> = decl
                    .type_params
                    .iter()
                    .map(|gen| {
                        let gk = self.type_checker.new_term_key();
                        self.impose_annotation(gk, gen).map(|_| gk)
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                for (ann, gk) in type_param.iter().zip(gen_keys.iter()) {
                    self.apply_annotation(*gk, ann, None)?;
                }

                for (param_ty, arg_expr) in decl.params.type_sequence().zip(args) {
                    let p_key = self.instantiate_param_type(param_ty, &gen_keys)?;
                    let a_key = self.analyze_expr(arg_expr, None)?;
                    self.type_checker.impose(a_key.equate_with(p_key))?;
                }

                let ret = self.instantiate_param_type(&decl.return_ty, &gen_keys)?;
                self.type_checker.impose(tk.concretizes(ret))?;
            }

            ExprVariant::ParameterAccess(current_si, ix) => {
                let owner = self
                    .spec
                    .constraints()
                    .find(|o| o.si == *current_si)
                    .expect("valid stream reference");
                let pk = self
                    .type_checker
                    .get_var_key(&TypeVar::from_param(owner, *ix));
                self.type_checker.impose(tk.equate_with(pk))?;
            }

            ExprVariant::FunctionParameterAccess(_ident, ann_ty, _ix) => {
                let pk = self.type_checker.new_term_key();
                self.impose_annotation(pk, ann_ty)?;
                self.type_checker.impose(tk.equate_with(pk))?;
            }

            ExprVariant::Quantified(_quant, bindings1, _bindings2, body) => {
                let mut scope: HashMap<String, TcKey> = HashMap::new();
                for binding in bindings1 {
                    if scope.contains_key(&binding.name) {
                        continue;
                    }
                    let key = self.type_checker.new_term_key();
                    self.impose_annotation(key, &ValueTyped::Int(64))?;
                    scope.insert(binding.name.clone(), key);
                }
                self.scope_stack.push(scope);
                self.analyze_expr(body, Some(AbstractValueType::Bool))?;
                self.scope_stack.pop();
                self.type_checker
                    .impose(tk.concretizes_explicit(AbstractValueType::Bool))?;
            }

            ExprVariant::QuantifiedVar(ident) => {
                let found = self
                    .scope_stack
                    .last()
                    .and_then(|s| s.get(&ident.name).copied())
                    .or_else(|| {
                        self.resolve_stream_by_name(&ident.name).map(|si| {
                            *self
                                .key_map
                                .get(&NodeRef::StreamIdx(si))
                                .expect("stream key exists")
                        })
                    });
                if let Some(vk) = found {
                    self.type_checker.impose(tk.equate_with(vk))?;
                }
            }
        };

        Ok(tk)
    }

    /// Find a stream index by its declared name (searches constraints then signals).
    fn resolve_stream_by_name(&self, name: &str) -> Option<StreamIdx> {
        self.spec
            .constraints()
            .find(|c| c.name() == name)
            .map(|c| c.si)
            .or_else(|| self.spec.signals().find(|s| s.name == name).map(|s| s.si))
    }

    /// Infer the type of a chained tuple-access on an Option<Tuple<>> value.
    fn analyze_tuple_option(&mut self, exp: &Expression) -> Result<TcKey, CheckFailure<ValErr>> {
        let tk = self.type_checker.new_term_key();
        self.key_map.insert(NodeRef::Expr(exp.eid), tk);
        self.span_map.insert(tk, exp.span);

        if let ExprVariant::TupleAccess(inner, idx) = &exp.kind {
            let opt_key = if matches!(inner.kind, ExprVariant::TupleAccess(_, _)) {
                self.analyze_tuple_option(inner.as_ref())?
            } else {
                self.analyze_expr(inner.as_ref(), None)?
            };

            self.type_checker
                .impose(opt_key.concretizes_explicit(AbstractValueType::Option))?;
            let tuple_key = self.type_checker.get_child_key(opt_key, 0)?;
            self.type_checker
                .impose(tuple_key.concretizes_explicit(AbstractValueType::AnyTuple))?;
            let elem_key = self.type_checker.get_child_key(tuple_key, *idx)?;

            self.type_checker
                .impose(tk.concretizes_explicit(AbstractValueType::Option))?;
            let result_inner = self.type_checker.get_child_key(tk, 0)?;
            self.type_checker
                .impose(result_inner.equate_with(elem_key))?;

            Ok(tk)
        } else {
            unreachable!()
        }
    }

    /// Replace a generic `Param(idx)` reference with the corresponding solved key,
    /// or allocate a fresh key with annotation constraints for concrete types.
    fn instantiate_param_type(
        &mut self,
        at: &ValueTyped,
        generics: &[TcKey],
    ) -> Result<TcKey, CheckFailure<ValErr>> {
        if let ValueTyped::Param(idx, _) = at {
            return Ok(generics[*idx]);
        }
        let nk = self.type_checker.new_term_key();
        self.impose_annotation(nk, at)?;
        Ok(nk)
    }

    /// Check that every key with a declared exact bound matches the resolved type.
    pub(crate) fn validate_exact_bounds(
        bounds: HashMap<TcKey, (DataType, Option<TcKey>)>,
        table: &TypeTable<AbstractValueType>,
    ) -> Vec<CheckFailure<ValErr>> {
        bounds
            .into_iter()
            .filter_map(|(key, (expected, conflict))| {
                let resolved = table[&key].clone();
                if resolved != expected {
                    Some(CheckFailure {
                        kind: ValErr::BoundViolation(resolved, expected),
                        key1: Some(key),
                        key2: conflict,
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    /// Verify that no widen inner expression is wider than its declared target.
    pub(crate) fn validate_widen_ops(
        widen_bounds: HashMap<TcKey, (DataType, TcKey)>,
        table: &TypeTable<AbstractValueType>,
    ) -> Vec<CheckFailure<ValErr>> {
        widen_bounds
            .into_iter()
            .filter_map(|(inner, (target, parent))| {
                let actual = table[&inner].clone();
                match (target.bit_width(), actual.bit_width()) {
                    (Some(tw), Some(aw)) if aw > tw => Some(CheckFailure {
                        kind: ValErr::WidenBoundViolation(target, actual),
                        key1: Some(parent),
                        key2: Some(inner),
                    }),
                    _ => None,
                }
            })
            .collect()
    }

    /// Verify that no stream or start-target has an Option type, and that
    /// all constrain-message streams are of type String.
    pub(crate) fn finalize_checks(
        source_ir: &OORVIr1,
        key_map: &HashMap<NodeRef, TcKey>,
        table: &TypeTable<AbstractValueType>,
    ) -> Vec<CheckFailure<ValErr>> {
        let mut errors: Vec<CheckFailure<ValErr>> = Vec::new();

        for output in &source_ir.constraints {
            let stream_key = key_map[&NodeRef::StreamIdx(output.si())];
            let stream_ty = &table[&stream_key];
            if matches!(stream_ty, DataType::Option(_)) {
                errors.push(CheckFailure {
                    kind: ValErr::ForbiddenOptionType(stream_ty.clone()),
                    key1: Some(stream_key),
                    key2: None,
                });
            }

            if let Some(start_eid) = output.start_expr() {
                let ek = key_map[&NodeRef::Expr(start_eid)];
                let ety = &table[&ek];
                match ety {
                    DataType::Tuple(child_tys) => {
                        if let ExprVariant::Tuple(children) = &source_ir.expression(start_eid).kind
                        {
                            if let Some((cidx, cty)) = child_tys
                                .iter()
                                .enumerate()
                                .find(|(_, t)| matches!(t, DataType::Option(_)))
                            {
                                let ck = key_map[&NodeRef::Expr(children[cidx].eid)];
                                errors.push(CheckFailure {
                                    kind: ValErr::ForbiddenOptionType(cty.clone()),
                                    key1: Some(ck),
                                    key2: None,
                                });
                            }
                        }
                    }
                    DataType::Option(_) => errors.push(CheckFailure {
                        kind: ValErr::ForbiddenOptionType(ety.clone()),
                        key1: Some(ek),
                        key2: None,
                    }),
                    _ => {}
                }
            }
        }

        for alarm in source_ir.constrains() {
            let ak = key_map[&NodeRef::StreamIdx(alarm.si)];
            let aty = &table[&ak];
            if *aty != DataType::TString {
                errors.push(CheckFailure {
                    kind: ValErr::ExpectedStringMessage(aty.clone()),
                    key1: Some(ak),
                    key2: None,
                });
            }
        }

        errors
    }
}
