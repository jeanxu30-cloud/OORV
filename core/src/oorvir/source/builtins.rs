use std::iter;

use lazy_static::lazy_static;

use crate::oorvir::source::{FuncDecl, FuncLabel, ParameterDecl, ValueTyped};

impl ParameterDecl {
    /// Returns the declared parameter types, extending repeating tails on demand.
    pub(crate) fn type_sequence(&self) -> Box<dyn Iterator<Item = &ValueTyped> + '_> {
        match self {
            ParameterDecl::FixedAmount(parameters) => Box::new(parameters.iter()),
            ParameterDecl::ArbitaryAmount { fixed, repeating } => {
                let fixed_prefix = fixed.iter();
                let repeated_tail = iter::repeat_with(move || repeating);
                Box::new(fixed_prefix.chain(repeated_tail))
            }
        }
    }
}

lazy_static! {
    // Numeric widening helper that keeps both source and destination generic.
    static ref WIDEN: FuncDecl = FuncDecl {
        name: FuncLabel::new("widen".to_string(), &[None]),
        type_params: vec![ValueTyped::Numeric,ValueTyped::Numeric],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string())]),
        return_ty: ValueTyped::Param(1, "U".to_string()),
    };
    // Common unary math helpers preserve the fractional input type.
    static ref SQRT: FuncDecl = FuncDecl {
        name: FuncLabel::new("sqrt".to_string(), &[None]),
        type_params: vec![ValueTyped::Fractional],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string())]),
        return_ty: ValueTyped::Param(0, "T".to_string()),
    };
    // Binary numeric comparisons return the shared operand type.
    static ref MIN: FuncDecl = FuncDecl {
        name: FuncLabel::new("min".to_string(), &[None, None]),
        type_params: vec![ValueTyped::Numeric],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string()), ValueTyped::Param(0, "T".to_string())]),
        return_ty: ValueTyped::Param(0, "T".to_string()),
    };
    static ref MAX: FuncDecl = FuncDecl {
        name: FuncLabel::new("max".to_string(), &[None, None]),
        type_params: vec![ValueTyped::Numeric],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string()), ValueTyped::Param(0, "T".to_string())]),
        return_ty: ValueTyped::Param(0, "T".to_string()),
    };
    // Trigonometric functions operate on a single fractional input.
    static ref COS: FuncDecl = FuncDecl {
        name: FuncLabel::new("cos".to_string(), &[None]),
        type_params: vec![ValueTyped::Fractional],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string())]),
        return_ty: ValueTyped::Param(0, "T".to_string()),
    };
    static ref SIN: FuncDecl = FuncDecl {
        name: FuncLabel::new("sin".to_string(), &[None]),
        type_params: vec![ValueTyped::Fractional],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string())]),
        return_ty: ValueTyped::Param(0, "T".to_string()),
    };
    static ref TAN: FuncDecl = FuncDecl {
        name: FuncLabel::new("tan".to_string(), &[None]),
        type_params: vec![ValueTyped::Fractional],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string())]),
        return_ty: ValueTyped::Param(0, "T".to_string()),
    };
    // Inverse trigonometric variants keep the same numeric shape.
    static ref ARCSIN: FuncDecl = FuncDecl {
        name: FuncLabel::new("arcsin".to_string(), &[None]),
        type_params: vec![ValueTyped::Fractional],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string())]),
        return_ty: ValueTyped::Param(0, "T".to_string()),
    };
    static ref ARCCOS: FuncDecl = FuncDecl {
        name: FuncLabel::new("arccos".to_string(), &[None]),
        type_params: vec![ValueTyped::Fractional],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string())]),
        return_ty: ValueTyped::Param(0, "T".to_string()),
    };
    static ref ARCTAN: FuncDecl = FuncDecl {
        name: FuncLabel::new("arctan".to_string(), &[None]),
        type_params: vec![ValueTyped::Fractional],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string())]),
        return_ty: ValueTyped::Param(0, "T".to_string()),
    };
    // Absolute value is restricted to signed numeric inputs.
    static ref ABS: FuncDecl = FuncDecl {
        name: FuncLabel::new("abs".to_string(), &[None]),
        type_params: vec![ValueTyped::Signed],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string())]),
        return_ty: ValueTyped::Param(0, "T".to_string()),
    };
    // Regex matching consumes a sequence-compatible value and a pattern.
    static ref MATCHES: FuncDecl = FuncDecl {
        name: FuncLabel::new("matches".to_string(), &[None, Some("regex".to_string())]),
        type_params: vec![ValueTyped::Sequence],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string()), ValueTyped::String]),
        return_ty: ValueTyped::Bool,
    };
    // Explicit numeric conversion between two abstract numeric type parameters.
    static ref CAST: FuncDecl = FuncDecl {
        name: FuncLabel::new("cast".to_string(), &[None]),
        type_params: vec![ValueTyped::Numeric, ValueTyped::Numeric],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string())]),
        return_ty: ValueTyped::Param(1, "U".to_string()),
    };
    // Indexed byte access returns an optional byte value.
    static ref BYTES_AT: FuncDecl = FuncDecl {
        name: FuncLabel::new("at".to_string(), &[None, Some("index".to_string())]),
        type_params: vec![],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Bytes, ValueTyped::UInt(8)]),
        return_ty: ValueTyped::Option(ValueTyped::UInt(8).into()),
    };
    // Variadic formatting keeps the format string first and accepts any tail values.
    static ref FORMAT: FuncDecl = FuncDecl {
        name: FuncLabel::new_repeating("format".to_string()),
        type_params: vec![],
        params: ParameterDecl::ArbitaryAmount{fixed: vec![ValueTyped::String], repeating: ValueTyped::Any},
        return_ty: ValueTyped::String
    };
    // Rounding takes a numeric value and an unsigned precision argument.
    static ref ROUND: FuncDecl = FuncDecl {
        name: FuncLabel::new("round".to_string(), &[None, None]),
        type_params: vec![ValueTyped::Numeric],
        params: ParameterDecl::FixedAmount(vec![ValueTyped::Param(0, "T".to_string()), ValueTyped::UInt(8)]),
        return_ty: ValueTyped::Float(64)
    };
}

pub(crate) fn implicit_module() -> Vec<&'static FuncDecl> {
    vec![&WIDEN, &CAST, &BYTES_AT, &FORMAT]
}

pub(crate) fn math_module() -> Vec<&'static FuncDecl> {
    vec![
        &SQRT, &COS, &SIN, &TAN, &ARCSIN, &ARCCOS, &ARCTAN, &ABS, &MIN, &MAX, &ROUND,
    ]
}

#[allow(dead_code)]
pub(crate) fn regex_module() -> Vec<&'static FuncDecl> {
    vec![&MATCHES]
}

lazy_static! {
    pub(crate) static ref PRIMITIVE_TYPES: Vec<(&'static str, &'static ValueTyped)> = vec![
        ("Bool", &ValueTyped::Bool),
        ("Int8", &ValueTyped::Int(8)),
        ("Int16", &ValueTyped::Int(16)),
        ("Int32", &ValueTyped::Int(32)),
        ("Int64", &ValueTyped::Int(64)),
        ("Int128", &ValueTyped::Int(128)),
        ("Int256", &ValueTyped::Int(256)),
        ("UInt8", &ValueTyped::UInt(8)),
        ("UInt16", &ValueTyped::UInt(16)),
        ("UInt32", &ValueTyped::UInt(32)),
        ("UInt64", &ValueTyped::UInt(64)),
        ("UInt128", &ValueTyped::UInt(128)),
        ("UInt256", &ValueTyped::UInt(256)),
        ("Float16", &ValueTyped::Float(16)),
        ("Float32", &ValueTyped::Float(32)),
        ("Float64", &ValueTyped::Float(64)),
        ("Fixed64_32", &ValueTyped::Fixed(64, 32)),
        ("Fixed32_16", &ValueTyped::Fixed(32, 16)),
        ("Fixed16_8", &ValueTyped::Fixed(16, 8)),
        ("UFixed64_32", &ValueTyped::UFixed(64, 32)),
        ("UFixed32_16", &ValueTyped::UFixed(32, 16)),
        ("UFixed16_8", &ValueTyped::UFixed(16, 8)),
        ("String", &ValueTyped::String),
        ("Bytes", &ValueTyped::Bytes),
    ];
    pub(crate) static ref REDUCED_PRIMITIVE_TYPES: Vec<(&'static str, &'static ValueTyped)> = vec![
        ("Bool", &ValueTyped::Bool),
        ("Int64", &ValueTyped::Int(64)),
        ("UInt64", &ValueTyped::UInt(64)),
        ("Float64", &ValueTyped::Float(64)),
        ("String", &ValueTyped::String),
        ("Bytes", &ValueTyped::Bytes),
    ];
    pub(crate) static ref PRIMITIVE_TYPES_ALIASES: Vec<(&'static str, &'static ValueTyped)> = vec![
        ("Int", &ValueTyped::Int(64)),
        ("UInt", &ValueTyped::UInt(64)),
        ("Float", &ValueTyped::Float(64)),
        ("Fixed", &ValueTyped::Fixed(64, 32)),
        ("Fixed64", &ValueTyped::Fixed(64, 32)),
        ("UFixed", &ValueTyped::UFixed(64, 32)),
        ("UFixed64", &ValueTyped::UFixed(64, 32)),
    ];
}
