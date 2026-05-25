use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use crate::ast::SourceSpan;
use itertools::iproduct;
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct ExprNodeIdx(pub(crate) u32);
#[derive(Debug, Clone, PartialEq)]
/// A typed source_ir expression together with its registry id and source span.
pub struct Expression {
    pub kind: ExprVariant,
    pub(crate) eid: ExprNodeIdx,
    pub(crate) span: SourceSpan,
}

impl Expression {
    /// Returns the stable expression id assigned by the registry.
    pub fn id(&self) -> ExprNodeIdx {
        self.eid
    }

    /// Returns the source span that produced this expression.
    pub fn span(&self) -> SourceSpan {
        self.span
    }

    /// Collects every stream that is accessed synchronously inside this expression tree.
    pub(crate) fn get_sync_accesses(&self) -> Vec<StreamIdx> {
        match &self.kind {
            ExprVariant::ArithLog(_, children)
            | ExprVariant::Tuple(children)
            | ExprVariant::Function(FnExprKind { args: children, .. }) => children
                .iter()
                .flat_map(|c| c.get_sync_accesses())
                .collect(),
            ExprVariant::StreamAccess(target, kind, children) => match kind {
                AccessMode::Strict => vec![*target]
                    .into_iter()
                    .chain(children.iter().flat_map(|c| c.get_sync_accesses()))
                    .collect(),
                _ => children
                    .iter()
                    .flat_map(|c| c.get_sync_accesses())
                    .collect(),
            },
            ExprVariant::Ite {
                condition,
                consequence,
                alternative,
            } => condition
                .as_ref()
                .get_sync_accesses()
                .into_iter()
                .chain(consequence.as_ref().get_sync_accesses())
                .chain(alternative.as_ref().get_sync_accesses())
                .collect(),
            ExprVariant::TupleAccess(child, _)
            | ExprVariant::Widen(WidenExprKind { expr: child, .. }) => {
                child.as_ref().get_sync_accesses()
            }
            ExprVariant::Default { expr, default } => expr
                .as_ref()
                .get_sync_accesses()
                .into_iter()
                .chain(default.as_ref().get_sync_accesses())
                .collect(),
            _ => vec![],
        }
    }
}

impl ValueEq for Expression {
    fn value_eq(&self, other: &Self, parameter_map: &ExpressionContext) -> bool {
        self.kind.value_eq(&other.kind, parameter_map)
    }

    fn value_eq_ignore_parameters(&self, other: &Self) -> bool {
        self.kind.value_eq_ignore_parameters(&other.kind)
    }
}

/// The expression forms supported by the source_ir.
#[derive(Debug, Clone, PartialEq)]
pub enum ExprVariant {
    LoadConstant(Constant),
    ArithLog(ArithLogOp, Vec<Expression>),
    StreamAccess(StreamIdx, AccessMode, Vec<Expression>),
    ParameterAccess(StreamIdx, usize),
    Ite {
        condition: Box<Expression>,
        consequence: Box<Expression>,
        alternative: Box<Expression>,
    },
    Tuple(Vec<Expression>),
    TupleAccess(Box<Expression>, usize),
    Function(FnExprKind),
    Widen(WidenExprKind),
    Default {
        expr: Box<Expression>,
        default: Box<Expression>,
    },
    Quantified(Quantifier, Vec<Ident>, Vec<Ident>, Box<Expression>),
    QuantifiedVar(Ident),
    FunctionParameterAccess(Ident, ValueTyped, usize),
}

#[derive(Debug, Clone, PartialEq)]
/// Quantifier used by quantified expressions.
pub enum Quantifier {
    Forall,
    Exists,
}

#[derive(Debug, Clone, PartialEq)]
/// Named identifier used in quantified and function-parameter expressions.
pub struct Ident {
    pub name: String,
}

/// Function-call payload stored inside ExprVariant::Function.
#[derive(Debug, Clone, PartialEq)]
pub struct FnExprKind {
    /// Callee name.
    pub name: String,
    /// Positional call arguments.
    pub args: Vec<Expression>,
    /// Explicit type arguments attached to the function call.
    pub(crate) type_param: Vec<ValueTyped>,
}

/// Payload for an explicit widening conversion expression.
#[derive(Debug, Clone, PartialEq)]
pub struct WidenExprKind {
    /// Expression being widened.
    pub expr: Box<Expression>,
    /// Destination type after widening.
    pub(crate) ty: ValueTyped,
}

/// Literal constant value embedded directly in an expression.
#[derive(Debug, Clone)]
pub enum Literal {
    /// String literal.
    Str(String),
    /// Boolean literal.
    Bool(bool),
    /// Unsigned integer literal.
    UInt(i64),
    /// Signed integer literal.
    SInt(i128),
    /// Decimal literal.
    Decimal(Decimal),
}

impl Hash for Literal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match &self {
            Literal::Str(str) => {
                1.hash(state);
                str.hash(state);
            }
            Literal::Bool(b) => {
                2.hash(state);
                b.hash(state);
            }
            Literal::UInt(i) => {
                3.hash(state);
                i.hash(state);
            }
            Literal::SInt(si) => {
                4.hash(state);
                si.hash(state);
            }
            Literal::Decimal(_) => {
                5.hash(state);
            }
        }
    }
}

impl PartialEq for Literal {
    fn eq(&self, other: &Self) -> bool {
        use self::Literal::*;
        match (self, other) {
            (Decimal(f1), Decimal(f2)) => f1 == f2,
            (Decimal(_), _) | (_, Decimal(_)) => false,
            (Str(s1), Str(s2)) => s1 == s2,
            (Str(_), _) | (_, Str(_)) => false,
            (Bool(b1), Bool(b2)) => b1 == b2,
            (Bool(_), _) | (_, Bool(_)) => false,
            (UInt(i1), UInt(i2)) => i1 == i2,
            (UInt(_), _) | (_, UInt(_)) => false,
            (SInt(i1), SInt(i2)) => i1 == i2,
        }
    }
}

impl Eq for Literal {}

/// Constant expression payload, either inline or embedded directly in an expression.
#[derive(Debug, PartialEq, Clone, Eq, Hash)]
pub enum Constant {
    /// Literal written directly in an expression.
    Basic(Literal),
    /// Value inlined from a named constant stream declaration.
    Inlined(Inlined),
}

/// Inlined constant value originating from a declared constant stream.
#[derive(Debug, PartialEq, Clone, Eq, Hash)]
pub struct Inlined {
    /// Literal payload.
    pub lit: Literal,
    /// Declared type of the constant stream.
    pub(crate) ty: ValueTyped,
}

/// Access modes supported for stream references.
#[derive(Debug, PartialEq, Clone, Copy, Hash, Eq)]
pub enum AccessMode {
    Strict,
    Cached,
    Shift(Shift),
    Get,
    Fresh,
}

/// Arithmetic, logical, comparison, and bitwise operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArithLogOp {
    Not,
    Neg,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    And,
    Or,
    BitXor,
    BitAnd,
    BitOr,
    BitNot,
    Shl,
    Shr,
    Eq,
    Lt,
    Le,
    Ne,
    Ge,
    Gt,
}

#[derive(Debug, Clone, PartialEq)]
/// Selects which stream instances participate in an aggregation.
pub enum InstanceSelection {
    /// Only instances updated in the current cycle participate.
    Fresh,
    /// All instances participate.
    All,
    /// Updated instances participate if they satisfy the filter condition.
    FilteredFresh {
        /// Lambda-style binding parameters.
        parameters: Vec<ParamDecl>,
        /// Filter condition applied to each instance.
        cond: Box<Expression>,
    },
    /// All matching instances participate regardless of update status.
    FilteredAll {
        /// Lambda-style binding parameters.
        parameters: Vec<ParamDecl>,
        /// Filter condition applied to each instance.
        cond: Box<Expression>,
    },
}

/// Tracks parameter equivalence induced by matching initialization expressions.
#[derive(Debug, Clone)]
pub(crate) struct ExpressionContext(
    HashMap<StreamIdx, HashMap<(StreamIdx, usize), HashSet<usize>>>,
);

impl ExpressionContext {
    /// Builds the parameter-equivalence table induced by matching start clauses.
    pub(crate) fn new(source_ir: &OORVIr1) -> ExpressionContext {
        let mut inner = HashMap::with_capacity(source_ir.constraints.len());
        for current in source_ir.constraints() {
            let mut para_mapping: HashMap<(StreamIdx, usize), HashSet<usize>> = HashMap::new();

            let cur_start_cond = current.start().and_then(|st| st.start_cond(source_ir));

            let current_start_args = current
                .start()
                .map(|st| st.start_args(source_ir))
                .unwrap_or_default();

            assert_eq!(current.params.len(), current_start_args.len());

            if !current.params.is_empty() {
                for target in source_ir.constraints() {
                    // Compare parameter mappings only when the initialization conditions match.
                    let target_start_cond = target.start().and_then(|st| st.start_cond(source_ir));
                    let cond_match = match (cur_start_cond, target_start_cond) {
                        (Some(e1), Some(e2)) => e1.value_eq_ignore_parameters(e2),
                        (None, None) => true,
                        _ => false,
                    };
                    if !target.params.is_empty() && cond_match {
                        let target_start_args = target
                            .start()
                            .map(|st| st.start_args(source_ir))
                            .unwrap_or_default();

                        assert_eq!(target.params.len(), target_start_args.len());

                        iproduct!(
                            current_start_args.iter().enumerate(),
                            target_start_args.iter().enumerate()
                        )
                        .filter_map(|((current_para, current_exp), (target_para, target_exp))| {
                            if current_exp.value_eq_ignore_parameters(target_exp) {
                                Some(((target.si, target_para), current_para))
                            } else {
                                None
                            }
                        })
                        .for_each(|(k, v)| {
                            if let Some(paras) = para_mapping.get_mut(&k) {
                                paras.insert(v);
                            } else {
                                para_mapping
                                    .insert(k, vec![v].into_iter().collect::<HashSet<usize>>());
                            }
                        });
                    }
                }
            }

            inner.insert(current.si, para_mapping);
        }
        ExpressionContext(inner)
    }

    /// Returns whether one parameter is equivalent to another under this context.
    pub(crate) fn matches(
        &self,
        source: StreamIdx,
        source_parameter: usize,
        target: StreamIdx,
        target_parameter: usize,
    ) -> bool {
        self.0
            .get(&source)
            .and_then(|para_map| para_map.get(&(target, target_parameter)))
            .map(|para_set| para_set.contains(&source_parameter))
            .unwrap_or(false)
    }
}

pub(crate) trait ValueEq {
    fn value_eq(&self, other: &Self, parameter_map: &ExpressionContext) -> bool;

    fn value_eq_ignore_parameters(&self, other: &Self) -> bool;
}

impl ValueEq for ExprVariant {
    fn value_eq(&self, other: &Self, parameter_map: &ExpressionContext) -> bool {
        use self::ExprVariant::*;
        match (self, other) {
            (ParameterAccess(sref, idx), ParameterAccess(sref2, idx2)) => {
                parameter_map.matches(*sref, *idx, *sref2, *idx2)
            }
            (LoadConstant(c1), LoadConstant(c2)) => c1 == c2,
            (ArithLog(op, args), ArithLog(op2, args2)) => {
                op == op2
                    && args.len() == args2.len()
                    && args
                        .iter()
                        .zip(args2.iter())
                        .all(|(a1, a2)| a1.value_eq(a2, parameter_map))
            }
            (StreamAccess(sref, kind, args), StreamAccess(sref2, kind2, args2)) => {
                sref == sref2
                    && kind == kind2
                    && args.len() == args2.len()
                    && args
                        .iter()
                        .zip(args2.iter())
                        .all(|(a1, a2)| a1.value_eq(a2, parameter_map))
            }
            (
                Ite {
                    condition: c1,
                    consequence: c2,
                    alternative: c3,
                },
                Ite {
                    condition: b1,
                    consequence: b2,
                    alternative: b3,
                },
            ) => {
                c1.value_eq(b1, parameter_map)
                    && c2.value_eq(b2, parameter_map)
                    && c3.value_eq(b3, parameter_map)
            }
            (Tuple(args), Tuple(args2)) => {
                args.len() == args2.len()
                    && args
                        .iter()
                        .zip(args2.iter())
                        .all(|(a1, a2)| a1.value_eq(a2, parameter_map))
            }
            (TupleAccess(inner, i1), TupleAccess(inner2, i2)) => {
                i1 == i2 && inner.value_eq(inner2, parameter_map)
            }
            (
                Function(FnExprKind {
                    name,
                    args,
                    type_param,
                }),
                Function(FnExprKind {
                    name: name2,
                    args: args2,
                    type_param: type_param2,
                }),
            ) => {
                name == name2
                    && type_param == type_param2
                    && args.len() == args2.len()
                    && args
                        .iter()
                        .zip(args2.iter())
                        .all(|(a1, a2)| a1.value_eq(a2, parameter_map))
            }
            (
                Widen(WidenExprKind {
                    expr: inner,
                    ty: t1,
                }),
                Widen(WidenExprKind {
                    expr: inner2,
                    ty: t2,
                }),
            ) => t1 == t2 && inner.value_eq(inner2, parameter_map),
            (
                Default { expr, default },
                Default {
                    expr: expr2,
                    default: default2,
                },
            ) => expr.value_eq(expr2, parameter_map) && default.value_eq(default2, parameter_map),
            (Quantified(q1, binds1, binds11, inner1), Quantified(q2, binds2, binds22, inner2)) => {
                if q1 != q2 || binds1.len() != binds2.len() || binds11.len() != binds22.len() {
                    return false;
                }
                for (b1, b2) in binds1.iter().zip(binds2.iter()) {
                    if b1.name != b2.name {
                        return false;
                    }
                }
                for (b1, b2) in binds11.iter().zip(binds22.iter()) {
                    if b1.name != b2.name {
                        return false;
                    }
                }
                inner1.value_eq(inner2, parameter_map)
            }
            (QuantifiedVar(id1), QuantifiedVar(id2)) => id1.name == id2.name,

            _ => false,
        }
    }

    fn value_eq_ignore_parameters(&self, other: &Self) -> bool {
        use ExprVariant::*;
        match (self, other) {
            (ParameterAccess(sref, idx), ParameterAccess(sref2, idx2)) => {
                sref == sref2 && idx == idx2
            }
            (LoadConstant(c1), LoadConstant(c2)) => c1 == c2,
            (ArithLog(op, args), ArithLog(op2, args2)) => {
                op == op2
                    && args.len() == args2.len()
                    && args
                        .iter()
                        .zip(args2.iter())
                        .all(|(a1, a2)| a1.value_eq_ignore_parameters(a2))
            }
            (StreamAccess(sref, kind, args), StreamAccess(sref2, kind2, args2)) => {
                sref == sref2
                    && kind == kind2
                    && args.len() == args2.len()
                    && args
                        .iter()
                        .zip(args2.iter())
                        .all(|(a1, a2)| a1.value_eq_ignore_parameters(a2))
            }
            (
                Ite {
                    condition: c1,
                    consequence: c2,
                    alternative: c3,
                },
                Ite {
                    condition: b1,
                    consequence: b2,
                    alternative: b3,
                },
            ) => {
                c1.value_eq_ignore_parameters(b1)
                    && c2.value_eq_ignore_parameters(b2)
                    && c3.value_eq_ignore_parameters(b3)
            }
            (Tuple(args), Tuple(args2)) => {
                args.len() == args2.len()
                    && args
                        .iter()
                        .zip(args2.iter())
                        .all(|(a1, a2)| a1.value_eq_ignore_parameters(a2))
            }
            (TupleAccess(inner, i1), TupleAccess(inner2, i2)) => {
                i1 == i2 && inner.value_eq_ignore_parameters(inner2)
            }
            (
                Function(FnExprKind {
                    name,
                    args,
                    type_param,
                }),
                Function(FnExprKind {
                    name: name2,
                    args: args2,
                    type_param: type_param2,
                }),
            ) => {
                name == name2
                    && type_param == type_param2
                    && args.len() == args2.len()
                    && args
                        .iter()
                        .zip(args2.iter())
                        .all(|(a1, a2)| a1.value_eq_ignore_parameters(a2))
            }
            (
                Widen(WidenExprKind {
                    expr: inner,
                    ty: t1,
                }),
                Widen(WidenExprKind {
                    expr: inner2,
                    ty: t2,
                }),
            ) => t1 == t2 && inner.value_eq_ignore_parameters(inner2),
            (
                Default { expr, default },
                Default {
                    expr: expr2,
                    default: default2,
                },
            ) => {
                expr.value_eq_ignore_parameters(expr2)
                    && default.value_eq_ignore_parameters(default2)
            }
            (Quantified(q1, binds1, binds11, inner1), Quantified(q2, binds2, binds22, inner2)) => {
                if q1 != q2 || binds1.len() != binds2.len() || binds11.len() != binds22.len() {
                    return false;
                }
                for (b1, b2) in binds1.iter().zip(binds2.iter()) {
                    if b1.name != b2.name {
                        return false;
                    }
                }
                for (b1, b2) in binds11.iter().zip(binds22.iter()) {
                    if b1.name != b2.name {
                        return false;
                    }
                }
                inner1.value_eq_ignore_parameters(inner2)
            }
            (QuantifiedVar(id1), QuantifiedVar(id2)) => id1.name == id2.name,
            _ => false,
        }
    }
}

use std::fmt::{Display, Formatter, Result};

use itertools::Itertools;

fn join_rendered<I>(items: I, separator: &str) -> String
where
    I: IntoIterator<Item = String>,
{
    items.into_iter().join(separator)
}

fn quantifier_name(quantifier: &Quantifier) -> &'static str {
    match quantifier {
        Quantifier::Forall => "forall",
        Quantifier::Exists => "exists",
    }
}

impl Expression {
    pub(crate) fn format_with_names(&self, names: &HashMap<StreamIdx, String>) -> String {
        use ExprVariant;

        match &self.kind {
            ExprVariant::LoadConstant(constant) => constant.to_string(),
            ExprVariant::Function(FnExprKind { name, args, .. }) => {
                let rendered_args =
                    join_rendered(args.iter().map(|expr| expr.format_with_names(names)), ", ");
                format!("{name}({rendered_args})")
            }
            ExprVariant::Tuple(elements) => {
                let rendered = join_rendered(
                    elements.iter().map(|expr| expr.format_with_names(names)),
                    ", ",
                );
                format!("({rendered})")
            }
            ExprVariant::Ite {
                condition,
                consequence,
                alternative,
            } => format!(
                "if {} then {} else {}",
                condition.format_with_names(names),
                consequence.format_with_names(names),
                alternative.format_with_names(names)
            ),
            ExprVariant::ArithLog(operator, operands) => match operands.as_slice() {
                [operand] => format!("{operator}{}", operand.format_with_names(names)),
                _ => {
                    let rendered = join_rendered(
                        operands.iter().map(|expr| expr.format_with_names(names)),
                        &format!(" {operator} "),
                    );
                    format!("({rendered})")
                }
            },
            ExprVariant::Default { expr, default } => format!(
                "{}.default({})",
                expr.format_with_names(names),
                default.format_with_names(names)
            ),
            ExprVariant::Widen(WidenExprKind { expr, ty }) => {
                format!("{ty}({})", expr.format_with_names(names))
            }
            ExprVariant::TupleAccess(expr, index) => {
                format!("{}.{index}", expr.format_with_names(names))
            }
            ExprVariant::ParameterAccess(stream_ref, index) => {
                format!("Param({}, {index})", names[stream_ref])
            }
            ExprVariant::FunctionParameterAccess(ident, _, index) => {
                format!("FnParam({}, {index})", ident.name)
            }
            ExprVariant::StreamAccess(stream_ref, mode, params) => {
                let stream_name = &names[stream_ref];
                let rendered_params = if params.is_empty() {
                    String::new()
                } else {
                    let entries = join_rendered(
                        params.iter().map(|expr| expr.format_with_names(names)),
                        ", ",
                    );
                    format!("({entries})")
                };
                let suffix = match mode {
                    AccessMode::Shift(offset) => format!(".offset(by: {offset})"),
                    AccessMode::Cached => ".hold()".to_string(),
                    AccessMode::Fresh => ".is_fresh()".to_string(),
                    AccessMode::Get => ".get()".to_string(),
                    AccessMode::Strict => String::new(),
                };
                format!("{stream_name}{rendered_params}{suffix}")
            }
            ExprVariant::Quantified(quantifier, left_bindings, right_bindings, expr) => {
                let left =
                    join_rendered(left_bindings.iter().map(|ident| ident.name.clone()), ", ");
                let right =
                    join_rendered(right_bindings.iter().map(|ident| ident.name.clone()), ", ");
                format!(
                    "{} [{}] [{}]: {}",
                    quantifier_name(quantifier),
                    left,
                    right,
                    expr.format_with_names(names)
                )
            }
            ExprVariant::QuantifiedVar(ident) => ident.name.clone(),
        }
    }
}

impl Display for Expression {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let rendered = match &self.kind {
            ExprVariant::LoadConstant(constant) => constant.to_string(),
            ExprVariant::Function(FnExprKind { name, args, .. }) => {
                let arguments = join_rendered(args.iter().map(ToString::to_string), ", ");
                format!("{name}({arguments})")
            }
            ExprVariant::Tuple(elements) => {
                let rendered = join_rendered(elements.iter().map(ToString::to_string), ", ");
                format!("({rendered})")
            }
            ExprVariant::Ite {
                condition,
                consequence,
                alternative,
            } => format!("if {condition} then {consequence} else {alternative}"),
            ExprVariant::ArithLog(operator, operands) => match operands.as_slice() {
                [operand] => format!("{operator}{operand}"),
                _ => {
                    let rendered = join_rendered(
                        operands.iter().map(ToString::to_string),
                        &format!(" {operator} "),
                    );
                    format!("({rendered})")
                }
            },
            ExprVariant::Default { expr, default } => format!("{expr}.default({default})"),
            ExprVariant::Widen(WidenExprKind { expr, ty }) => format!("{ty}({expr})"),
            ExprVariant::TupleAccess(expr, index) => format!("{expr}.{index}"),
            ExprVariant::ParameterAccess(stream_ref, index) => {
                format!("Param(ref: {stream_ref}, idx: {index})")
            }
            ExprVariant::FunctionParameterAccess(ident, _, index) => {
                format!("FnParam(name: {}, idx: {index})", ident.name)
            }
            ExprVariant::StreamAccess(stream_ref, mode, params) => {
                let arguments = join_rendered(params.iter().map(ToString::to_string), ", ");
                let suffix = match mode {
                    AccessMode::Shift(offset) => format!(".offset(by: {offset})"),
                    AccessMode::Cached => ".hold()".to_string(),
                    AccessMode::Fresh => ".is_fresh()".to_string(),
                    AccessMode::Get => ".get()".to_string(),
                    AccessMode::Strict => String::new(),
                };
                format!("Stream(ref: {stream_ref}, params: ({arguments})){suffix}")
            }
            ExprVariant::Quantified(quantifier, left_bindings, right_bindings, expr) => {
                let left =
                    join_rendered(left_bindings.iter().map(|ident| ident.name.clone()), ", ");
                let right =
                    join_rendered(right_bindings.iter().map(|ident| ident.name.clone()), ", ");
                format!(
                    "{} [{}] [{}]: {expr}",
                    quantifier_name(quantifier),
                    left,
                    right
                )
            }
            ExprVariant::QuantifiedVar(ident) => ident.name.clone(),
        };

        write!(f, "{rendered}")
    }
}

impl Display for Literal {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let rendered = match self {
            Literal::SInt(value) => value.to_string(),
            Literal::UInt(value) => value.to_string(),
            Literal::Decimal(value) => value.to_string(),
            Literal::Bool(value) => value.to_string(),
            Literal::Str(value) => value.to_string(),
        };
        write!(f, "{rendered}")
    }
}

impl Display for Constant {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let literal = match self {
            Constant::Inlined(Inlined { lit, .. }) => lit,
            Constant::Basic(lit) => lit,
        };
        write!(f, "{literal}")
    }
}

impl Display for ArithLogOp {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let token = match self {
            ArithLogOp::Not => "!",
            ArithLogOp::Neg => "~",
            ArithLogOp::Add => "+",
            ArithLogOp::Sub => "-",
            ArithLogOp::Mul => "*",
            ArithLogOp::Div => "/",
            ArithLogOp::Rem => "%",
            ArithLogOp::Pow => "^",
            ArithLogOp::And => "∧",
            ArithLogOp::Or => "∨",
            ArithLogOp::Eq => "=",
            ArithLogOp::Lt => "<",
            ArithLogOp::Le => "≤",
            ArithLogOp::Ne => "≠",
            ArithLogOp::Ge => "≥",
            ArithLogOp::Gt => ">",
            ArithLogOp::BitNot => "~",
            ArithLogOp::BitAnd => "&",
            ArithLogOp::BitOr => "|",
            ArithLogOp::BitXor => "^",
            ArithLogOp::Shl => "<<",
            ArithLogOp::Shr => ">>",
        };
        write!(f, "{token}")
    }
}

impl Display for Shift {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        match self {
            Shift::PastDiscrete(offset) => write!(f, "-{offset}"),
            _ => unimplemented!(),
        }
    }
}

impl Display for StreamIdx {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let rendered = match self {
            StreamIdx::Constraint(index) => format!("Constraint({index})"),
            StreamIdx::Signal(index) => format!("Signal({index})"),
        };
        write!(f, "{rendered}")
    }
}

impl Display for ValueTyped {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        use ValueTyped::*;

        let rendered = match self {
            Int(width) => format!("Int{width}"),
            Float(width) => format!("Float{width}"),
            UInt(width) => format!("UInt{width}"),
            Bool => "Bool".to_string(),
            String => "String".to_string(),
            Bytes => "Bytes".to_string(),
            Option(inner) => format!("Option<{inner}>"),
            Tuple(types) => format!(
                "({})",
                join_rendered(types.iter().map(ToString::to_string), ",")
            ),
            Numeric => "Numeric".to_string(),
            Signed => "Signed".to_string(),
            Sequence => "Sequence".to_string(),
            Param(index, name) => format!("FunctionParam({index}, {name})"),
            Any => "Any".to_string(),
            Fixed(total, fractional) => format!("Fixed{total}_{fractional}"),
            UFixed(total, fractional) => format!("UFixed{total}_{fractional}"),
            Fractional => "Fractional".to_string(),
        };

        write!(f, "{rendered}")
    }
}

use serde::{Deserialize, Serialize};
use uom::si::rational64::Frequency as UOM_Frequency;

pub use crate::oorvir::source::analysis::ast_build::LoweringError;
pub use crate::oorvir::source::analysis::pacing::ActivationCondition;
pub use crate::oorvir::source::analysis::{
    AccessIndex, HasDependencies, HasMemory, HasSchedule, HasTypeInfo, LayerAssignment, StorageMap,
    TypeRegistry,
};
pub use crate::oorvir::source::analysis::{
    AccessSite, FlowGraph, GraphViolation, LayerIndex, StorageRequirement, StreamEdge, StreamLayer,
};

/// Describes how a stream or sub-expression is scheduled to produce values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamPacingKind {
    /// Fires whenever the embedded activation condition evaluates to true.
    Conditional(ActivationCondition),
    /// Fires at a single fixed clock rate shared by all global-clock streams.
    GlobalClock(UOM_Frequency),
    /// Fires at a fixed clock rate that is private to this stream instance.
    LocalClock(UOM_Frequency),
    /// Fires at some clock rate whose global/local distinction is unresolved.
    UnknownClock,
    /// Always available; the value is a constant with no scheduling constraint.
    Unconditional,
}

impl StreamPacingKind {
    /// Returns `true` when the scheduling is a concrete fixed-rate clock.
    pub fn is_clock_rate(&self) -> bool {
        matches!(
            self,
            StreamPacingKind::LocalClock(_) | StreamPacingKind::GlobalClock(_)
        )
    }

    /// Returns `true` when the scheduling depends on an activation condition.
    pub fn is_conditional(&self) -> bool {
        matches!(self, StreamPacingKind::Conditional(_))
    }

    /// Returns `true` when the value is unconditionally always present.
    pub fn is_unconditional(&self) -> bool {
        matches!(self, StreamPacingKind::Unconditional)
    }
}

/// Concrete value type assigned to a stream output or sub-expression.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum DataType {
    Bool,
    Integer8,
    Integer16,
    Integer32,
    Integer64,
    Integer128,
    Integer256,
    UInteger8,
    UInteger16,
    UInteger32,
    UInteger64,
    UInteger128,
    UInteger256,
    Float32,
    Float64,
    /// 64-bit signed fixed-point number with 32 fractional bits.
    Fixed64_32,
    /// 32-bit signed fixed-point number with 16 fractional bits.
    Fixed32_16,
    /// 16-bit signed fixed-point number with 8 fractional bits.
    Fixed16_8,
    /// 64-bit unsigned fixed-point number with 32 fractional bits.
    UFixed64_32,
    /// 32-bit unsigned fixed-point number with 16 fractional bits.
    UFixed32_16,
    /// 16-bit unsigned fixed-point number with 8 fractional bits.
    UFixed16_8,
    /// Heterogeneous fixed-length product type, e.g. `(Bool, Int32, Float64)`.
    Tuple(Vec<DataType>),
    TString,
    /// Unsigned single byte value; produced by byte-array indexing operations.
    Byte,
    /// Optional wrapper that allows an inner value to be absent.
    Option(Box<DataType>),
}

/// Groups the execution, initialisation, and termination pacing annotations
/// that together describe the full scheduling behaviour of one stream.
#[derive(Debug, Clone)]
pub struct StreamPacingBundle {
    /// Rate at which the stream's main body expression is evaluated.
    pub execution_rate: StreamPacingKind,
    /// Boolean guard that gates each evaluation step.
    pub execution_guard: Expression,
    /// Rate at which the stream is initialised (instantiated).
    pub init_rate: StreamPacingKind,
    /// Boolean guard for the stream initialisation clause.
    pub init_guard: Expression,
    /// Rate at which the stream is terminated.
    pub termination_rate: StreamPacingKind,
    /// Boolean guard that triggers stream termination when it holds.
    pub termination_guard: Expression,
}

/// Combines the resolved value type with the three pacing annotations into a
/// single descriptor that is stored in the TypeRegistry for each IR node.
#[derive(Debug, Clone)]
pub struct StreamProfile {
    /// Concrete data type of the values produced by this stream.
    pub data_kind: DataType,
    /// Scheduling rate for the main evaluation body.
    pub execution_rate: StreamPacingKind,
    /// Guard expression for the evaluation step.
    pub execution_guard: Expression,
    /// Scheduling rate for the stream initialisation clause.
    pub init_rate: StreamPacingKind,
    /// Guard expression for the stream initialisation clause.
    pub init_guard: Expression,
    /// Scheduling rate for the stream termination clause.
    pub termination_rate: StreamPacingKind,
    /// Guard expression that, when true, ends this stream instance.
    pub termination_guard: Expression,
}

#[derive(Debug, Clone)]
pub struct OORVIr1 {
    pub(crate) signals: Vec<Signal>,
    pub(crate) constraints: Vec<Constraint>,
    pub(crate) object_domains: HashMap<String, String>,
    pub(crate) expr_registry: ExpressionRegistry,
    pub(crate) types: Option<TypeRegistry>,
    pub(crate) dependencies: Option<AccessIndex>,
    pub(crate) layers: Option<LayerAssignment>,
    pub(crate) memory: Option<StorageMap>,
}

impl OORVIr1 {
    /// Create a fresh source_ir container with no streams, expressions, or analyses.
    pub fn empty() -> Self {
        Self {
            signals: Vec::new(),
            constraints: Vec::new(),
            object_domains: HashMap::new(),
            expr_registry: ExpressionRegistry::new(HashMap::new(), HashMap::new(), HashMap::new()),
            types: None,
            dependencies: None,
            layers: None,
            memory: None,
        }
    }

    /// Compatibility wrapper for older call sites.
    pub fn default() -> Self {
        Self::empty()
    }
}

#[derive(Debug, Clone)]
pub struct Signal {
    pub name: String,
    pub(crate) si: StreamIdx,
    pub(crate) ty: ValueTyped,
    pub(crate) span: SourceSpan,
}

#[derive(Debug, Clone)]
pub struct Constraint {
    pub kind: ConstraintKind,
    pub(crate) ty: Option<ValueTyped>,
    pub(crate) params: Vec<ParamDecl>,
    pub(crate) start: Option<StartNode>,
    pub(crate) eval: Vec<EvalNode>,
    pub(crate) end: Option<EndNode>,
    pub(crate) level: String,
    pub(crate) si: StreamIdx,
    pub(crate) span: SourceSpan,
}

#[derive(Clone, Debug)]
pub(crate) struct ExpressionRegistry {
    expr_nodes: HashMap<ExprNodeIdx, Expression>,
    func_decls: HashMap<String, FuncDecl>,
    func_bodies: HashMap<String, ExprNodeIdx>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct StartNode {
    pub(crate) pacing: PacingNode,
    pub(crate) condition: Option<ExprNodeIdx>,
    pub(crate) expression: Option<ExprNodeIdx>,
    pub(crate) span: SourceSpan,
}

#[derive(Debug, Clone)]
pub(crate) struct EvalNode {
    pub(crate) pacing: PacingNode,
    pub(crate) condition: Option<ExprNodeIdx>,
    pub(crate) expression: ExprNodeIdx,
    pub(crate) span: SourceSpan,
}

#[derive(Debug, Clone)]
pub(crate) struct EndNode {
    pub(crate) pacing: PacingNode,
    pub(crate) condition: ExprNodeIdx,
    pub(crate) span: SourceSpan,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
pub enum ConstraintKind {
    Output(String),
    Alarm(usize),
}

#[derive(Debug, PartialEq, Clone, Eq)]
pub struct ParamDecl {
    pub name: String,
    pub(crate) ty: Option<ValueTyped>,
    pub(crate) position: usize,
    pub(crate) span: SourceSpan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Frequency metadata for scheduled execution
pub struct TimedFrequency {
    /// Location of the frequency annotation in source
    pub span: SourceSpan,
    /// Frequency value
    pub rate: UOM_Frequency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingNode {
    /// Global evaluation tick frequency
    GlobalTick(TimedFrequency),
    /// Local evaluation tick frequency
    LocalTick(TimedFrequency),
    /// Event evaluation condition
    Event(ExprNodeIdx),
    /// No scheduling annotation provided
    Unspecified(SourceSpan),
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamIdx {
    Signal(usize),
    Constraint(usize),
}

#[derive(Debug, Clone)]
pub struct FuncDecl {
    pub name: FuncLabel,
    pub(crate) type_params: Vec<ValueTyped>,
    pub(crate) params: ParameterDecl,
    pub(crate) return_ty: ValueTyped,
}

#[derive(Debug, Clone)]
pub enum ParameterDecl {
    FixedAmount(Vec<ValueTyped>),
    ArbitaryAmount {
        fixed: Vec<ValueTyped>,
        repeating: ValueTyped,
    },
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum ValueTyped {
    /// Boolean
    Bool,
    /// UTF-8 string
    String,
    /// Raw bytes
    Bytes,

    /// Integer with bit width
    Int(u32),
    /// Unsigned integer
    UInt(u32),
    /// Floating point with width
    Float(u32),
    /// Fixed-point with (integer_bits, fractional_bits)
    Fixed(u32, u32),
    /// Unsigned fixed-point
    UFixed(u32, u32),

    /// Tuple of annotated element types
    Tuple(Vec<ValueTyped>),
    /// Optional value
    Option(Box<ValueTyped>),
    /// Sequence/array-like
    Sequence,

    /// Parameterized type (index, name)
    Param(usize, String),

    /// Generic numeric placeholder
    Numeric,
    /// Fractional numeric placeholder
    Fractional,
    /// Signedness placeholder
    Signed,

    /// Any type (dynamic/unknown)
    Any,
}

/*************************************************************************************************** */
impl OORVIr1 {
    /// Iterates over every signal stream stored in the source_ir.
    pub fn input_streams(&self) -> impl Iterator<Item = &Signal> {
        self.signals.iter()
    }

    /// Iterates over every constraint stream stored in the source_ir.
    pub fn output_streams(&self) -> impl Iterator<Item = &Constraint> {
        self.constraints.iter()
    }

    /// Iterates over constraint streams that represent alarms.
    pub fn alarm_outputs(&self) -> impl Iterator<Item = &Constraint> {
        self.output_streams()
            .filter(|constraint| matches!(constraint.kind, ConstraintKind::Alarm(_)))
    }

    /// Returns the number of signal streams currently stored in the source_ir.
    pub fn signal_count(&self) -> usize {
        self.signals.len()
    }

    /// Returns the number of constraint streams currently stored in the source_ir.
    pub fn constraint_count(&self) -> usize {
        self.constraints.len()
    }

    /// Returns the number of alarm-like constraint outputs.
    pub fn alarm_count(&self) -> usize {
        self.alarm_outputs().count()
    }

    /// Iterates over every stream index in declaration order.
    pub fn all_stream_refs(&'_ self) -> impl Iterator<Item = StreamIdx> + '_ {
        self.signals
            .iter()
            .map(|signal| signal.si)
            .chain(self.constraints.iter().map(|constraint| constraint.si))
    }

    /// Finds a signal stream by its user-visible name.
    pub fn find_signal(&self, name: &str) -> Option<&Signal> {
        self.signals.iter().find(|signal| signal.name == name)
    }

    /// Finds a constraint stream by its user-visible name.
    pub fn find_constraint(&self, name: &str) -> Option<&Constraint> {
        self.constraints
            .iter()
            .find(|constraint| constraint.name() == name)
    }

    /// Resolves a stream reference to a constraint stream when possible.
    pub fn get_constraint(&self, stream_ref: StreamIdx) -> Option<&Constraint> {
        self.output_streams()
            .find(|constraint| constraint.si == stream_ref)
    }

    /// Resolves a stream reference to a signal stream when possible.
    pub fn get_signal(&self, stream_ref: StreamIdx) -> Option<&Signal> {
        self.input_streams().find(|signal| signal.si == stream_ref)
    }

    /// Retrieves an expression for a given expression id.
    ///
    /// # Panic
    /// Panics if the expression does not exist.
    pub fn get_expression(&self, id: ExprNodeIdx) -> &Expression {
        &self.expr_registry.expr_nodes[&id]
    }

    /// Retrieves a function declaration for a given function name.
    ///
    /// # Panic
    /// Panics if the declaration does not exist.
    pub fn get_func(&self, func_name: &str) -> &FuncDecl {
        &self.expr_registry.func_decls[func_name]
    }

    /// Safe variant of `func_declaration` that returns `None` when the function
    /// declaration is not present in the source_ir's function table.
    pub fn find_func(&self, func_name: &str) -> Option<&FuncDecl> {
        self.expr_registry.func_decls.get(func_name)
    }

    /// Provides access to the lowered function bodies mapping: name -> ExprNodeIdx
    pub fn func_body_iter(&self) -> impl Iterator<Item = (&String, &ExprNodeIdx)> {
        self.expr_registry.func_bodies.iter()
    }

    pub fn signals(&self) -> impl Iterator<Item = &Signal> {
        self.input_streams()
    }

    pub fn constraints(&self) -> impl Iterator<Item = &Constraint> {
        self.output_streams()
    }

    pub fn object_domains(&self) -> impl Iterator<Item = (&String, &String)> {
        self.object_domains.iter()
    }

    pub fn constrains(&self) -> impl Iterator<Item = &Constraint> {
        self.alarm_outputs()
    }

    pub fn num_inputs(&self) -> usize {
        self.signal_count()
    }

    pub fn num_outputs(&self) -> usize {
        self.constraint_count()
    }

    pub fn num_constrains(&self) -> usize {
        self.alarm_count()
    }

    pub fn all_streams(&'_ self) -> impl Iterator<Item = StreamIdx> + '_ {
        self.all_stream_refs()
    }

    pub fn get_input_with_name(&self, name: &str) -> Option<&Signal> {
        self.find_signal(name)
    }

    pub fn get_output_with_name(&self, name: &str) -> Option<&Constraint> {
        self.find_constraint(name)
    }

    pub fn output(&self, sref: StreamIdx) -> Option<&Constraint> {
        self.get_constraint(sref)
    }

    pub fn input(&self, sref: StreamIdx) -> Option<&Signal> {
        self.get_signal(sref)
    }

    pub fn expression(&self, id: ExprNodeIdx) -> &Expression {
        self.get_expression(id)
    }

    pub fn func_declaration(&self, func_name: &str) -> &FuncDecl {
        self.get_func(func_name)
    }

    pub fn func_declaration_opt(&self, func_name: &str) -> Option<&FuncDecl> {
        self.find_func(func_name)
    }

    pub fn func_bodies(&self) -> impl Iterator<Item = (&String, &ExprNodeIdx)> {
        self.func_body_iter()
    }

    /// Returns the initialization clause view for a constraint stream.
    pub fn init_clause(&self, si: StreamIdx) -> Option<InitView<'_>> {
        match si {
            StreamIdx::Signal(_) => None,
            StreamIdx::Constraint(_) => {
                let output = self.constraints.iter().find(|o| o.si == si);
                output.and_then(|o| o.start()).map(|st| {
                    InitView::new(
                        st.expression.map(|e| self.expression(e)),
                        st.condition.map(|e| self.expression(e)),
                        &st.pacing,
                        st.span,
                    )
                })
            }
        }
    }

    /// Returns the initialization condition expression of a constraint stream.
    pub fn init_condition(&self, si: StreamIdx) -> Option<&Expression> {
        match si {
            StreamIdx::Signal(_) => None,
            StreamIdx::Constraint(_) => self
                .constraints
                .iter()
                .find(|o| o.si == si)
                .and_then(|o| o.start_cond())
                .map(|eid| self.expression(eid)),
        }
    }

    /// Returns the initialization expression of a constraint stream.
    pub fn init_expression(&self, si: StreamIdx) -> Option<&Expression> {
        match si {
            StreamIdx::Signal(_) => None,
            StreamIdx::Constraint(_) => self
                .constraints
                .iter()
                .find(|o| o.si == si)
                .and_then(|o| o.start_expr())
                .map(|eid| self.expression(eid)),
        }
    }

    /// Returns the initialization pacing annotation of a constraint stream.
    pub fn init_rate(&self, si: StreamIdx) -> Option<&PacingNode> {
        match si {
            StreamIdx::Signal(_) => None,
            StreamIdx::Constraint(_) => self
                .constraints
                .iter()
                .find(|o| o.si == si)
                .and_then(|o| o.start_pacing()),
        }
    }

    pub fn start(&self, si: StreamIdx) -> Option<InitView<'_>> {
        self.init_clause(si)
    }

    pub fn start_cond(&self, si: StreamIdx) -> Option<&Expression> {
        self.init_condition(si)
    }

    pub fn start_expr(&self, si: StreamIdx) -> Option<&Expression> {
        self.init_expression(si)
    }

    pub fn start_pacing(&self, si: StreamIdx) -> Option<&PacingNode> {
        self.init_rate(si)
    }

    /// Retrieves the eval definitions of a particular output stream or constrain or `None` for input references.
    pub fn execution_clauses(&self, si: StreamIdx) -> Option<Vec<ExecView<'_>>> {
        match si {
            StreamIdx::Signal(_) => None,
            StreamIdx::Constraint(_) => {
                let output = self.constraints.iter().find(|o| o.si == si);
                output.map(|o| {
                    o.eval()
                        .iter()
                        .map(|eval| {
                            ExecView::new(
                                eval.condition.map(|id| self.expression(id)),
                                self.expression(eval.expression),
                                &eval.pacing,
                                eval.span,
                            )
                        })
                        .collect()
                })
            }
        }
    }

    /// Retrieves all eval conditions of the clauses of a particular output stream or `None` for input and constrain references.
    /// For each eval clause of the stream, the element in the Vec is `None` if no condition is
    /// or the coresponding condition otherwise.
    /// If all parts of the [ExecView] are needed, see [OORVIr1::eval]
    pub fn execution_conditions(&self, si: StreamIdx) -> Option<Vec<Option<&Expression>>> {
        match si {
            StreamIdx::Signal(_) => None,
            StreamIdx::Constraint(o) => {
                if o < self.constraints.len() {
                    self.constraints.iter().find(|o| o.si == si).map(|output| {
                        output
                            .eval
                            .iter()
                            .map(|e| e.condition.map(|eid| self.expression(eid)))
                            .collect()
                    })
                } else {
                    Some(vec![None])
                }
            }
        }
    }

    /// Retrieves the eval expressions of all eval clauses of a particular output stream or constrain and `None` for input references.
    /// If all parts of the [ExecView] are needed, see [OORVIr1::eval]
    pub fn execution_expressions(&self, si: StreamIdx) -> Option<Vec<&Expression>> {
        match si {
            StreamIdx::Signal(_) => None,
            StreamIdx::Constraint(_) => {
                self.constraints.iter().find(|o| o.si == si).map(|output| {
                    output
                        .eval
                        .iter()
                        .map(|eval| self.expression(eval.expression))
                        .collect()
                })
            }
        }
    }

    /// Retrieves the annotated eval pacing of each eval clause of a particular output stream or constrain `None` for input references.
    /// If all parts of the [ExecView] are needed, see [OORVIr1::eval]
    pub fn execution_rate(&self, si: StreamIdx) -> Option<Vec<&PacingNode>> {
        match si {
            StreamIdx::Signal(_) => None,
            StreamIdx::Constraint(_) => {
                let output = self.constraints.iter().find(|o| o.si == si)?;
                Some(output.eval.iter().map(|eval| &eval.pacing).collect())
            }
        }
    }

    pub fn eval(&self, si: StreamIdx) -> Option<Vec<ExecView<'_>>> {
        self.execution_clauses(si)
    }

    pub fn eval_cond(&self, si: StreamIdx) -> Option<Vec<Option<&Expression>>> {
        self.execution_conditions(si)
    }

    pub fn eval_expr(&self, si: StreamIdx) -> Option<Vec<&Expression>> {
        self.execution_expressions(si)
    }

    pub fn eval_pacing(&self, si: StreamIdx) -> Option<Vec<&PacingNode>> {
        self.execution_rate(si)
    }

    /// Same behavior as [`eval`](fn@OORVIr1).
    /// # Panic
    /// Panics if the stream does not exist or is an input.
    pub(crate) fn eval_unchecked(&self, si: StreamIdx) -> Vec<ExecView<'_>> {
        self.eval(si).expect("Invalid for input references")
    }

    /// Returns the termination clause view for a constraint stream.
    pub fn termination_clause(&self, si: StreamIdx) -> Option<EndView<'_>> {
        match si {
            StreamIdx::Signal(_) => None,
            StreamIdx::Constraint(_) => {
                let ct = self
                    .constraints
                    .iter()
                    .find(|o| o.si == si)
                    .and_then(|o| o.end());
                ct.map(|ct| EndView::new(Some(self.expression(ct.condition)), &ct.pacing, ct.span))
            }
        }
    }

    /// Returns the termination condition expression of a constraint stream.
    pub fn termination_condition(&self, si: StreamIdx) -> Option<&Expression> {
        match si {
            StreamIdx::Signal(_) => None,
            StreamIdx::Constraint(_) => self
                .constraints
                .iter()
                .find(|o| o.si == si)
                .and_then(|o| o.end_cond())
                .map(|eid| self.expression(eid)),
        }
    }

    /// Returns the termination pacing annotation of a constraint stream.
    pub fn termination_rate(&self, si: StreamIdx) -> Option<&PacingNode> {
        match si {
            StreamIdx::Signal(_) => None,
            StreamIdx::Constraint(_) => self
                .constraints
                .iter()
                .find(|o| o.si == si)
                .and_then(|o| o.end_pacing()),
        }
    }

    pub fn end(&self, si: StreamIdx) -> Option<EndView<'_>> {
        self.termination_clause(si)
    }

    pub fn end_cond(&self, si: StreamIdx) -> Option<&Expression> {
        self.termination_condition(si)
    }

    pub fn end_pacing(&self, si: StreamIdx) -> Option<&PacingNode> {
        self.termination_rate(si)
    }

    /// Builds a stream-index-to-name table for diagnostics and debugging.
    pub fn build_name_map(&self) -> HashMap<StreamIdx, String> {
        self.input_streams()
            .map(|signal| (signal.si, signal.name.clone()))
            .chain(
                self.output_streams()
                    .map(|constraint| (constraint.si, constraint.name())),
            )
            .collect()
    }

    pub fn names(&self) -> HashMap<StreamIdx, String> {
        self.build_name_map()
    }
}

impl ExpressionRegistry {
    /// Creates a new expression map.
    pub(crate) fn new(
        expr_nodes: HashMap<ExprNodeIdx, Expression>,
        func_decls: HashMap<String, FuncDecl>,
        func_bodies: HashMap<String, ExprNodeIdx>,
    ) -> Self {
        Self {
            expr_nodes,
            func_decls,
            func_bodies,
        }
    }
}

/// Represents the name of a function including its arguments.
#[derive(Debug, Clone)]
pub enum FuncLabel {
    /// the function has a fixed number of (possibly named) arguments
    FixedParameters {
        /// The name of the function
        name: String,
        /// For each argument its name (or None if it does not have a name)
        arg_names: Vec<Option<String>>,
    },
    /// The function has an arbitrary amount of (unnamed) arguments
    ArbitraryParameters {
        /// The name of the function
        name: String,
    },
}

impl FuncLabel {
    /// Creates a new FuncLabel with a predefined number of arguments.
    pub(crate) fn new(name: String, arg_names: &[Option<String>]) -> Self {
        Self::FixedParameters {
            name,
            arg_names: Vec::from(arg_names),
        }
    }

    pub(crate) fn new_repeating(name: String) -> Self {
        Self::ArbitraryParameters { name }
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            FuncLabel::FixedParameters { name, .. } => name,
            FuncLabel::ArbitraryParameters { name } => name,
        }
    }
}

impl PartialEq for FuncLabel {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::ArbitraryParameters { name }, other)
            | (other, Self::ArbitraryParameters { name }) => name == other.name(),
            (
                Self::FixedParameters {
                    name: s_name,
                    arg_names: s_arg_names,
                },
                Self::FixedParameters {
                    name: o_name,
                    arg_names: o_arg_names,
                },
            ) => s_name == o_name && s_arg_names == o_arg_names,
        }
    }
}

impl std::hash::Hash for FuncLabel {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            FuncLabel::FixedParameters { name, arg_names: _ } => name,
            FuncLabel::ArbitraryParameters { name } => name,
        }
        .hash(state)
    }
}

impl Eq for FuncLabel {}

impl Signal {
    /// Yields the reference referring to this input stream.
    pub fn si(&self) -> StreamIdx {
        self.si
    }

    /// Yields the span referring to a part of the specification from which this stream originated.
    pub fn span(&self) -> SourceSpan {
        self.span
    }
}

impl Constraint {
    /// Returns the name of this stream.
    pub fn name(&self) -> String {
        match &self.kind {
            ConstraintKind::Output(s) => s.clone(),
            ConstraintKind::Alarm(position) => format!("constrain_{position}"),
        }
    }

    /// Returns an iterator over the parameters of this stream.
    pub fn params(&self) -> impl Iterator<Item = &ParamDecl> {
        self.params.iter()
    }

    /// Yields the reference referring to this input stream.
    pub fn si(&self) -> StreamIdx {
        self.si
    }

    /// Returns the [StartNode] template of the stream
    pub(crate) fn start(&self) -> Option<&StartNode> {
        self.start.as_ref()
    }

    /// Returns the expression id for the start condition of this stream
    /// If all parts of [StartNode] are required, see [start](fn@OORVIr1)
    pub(crate) fn start_cond(&self) -> Option<ExprNodeIdx> {
        self.start.as_ref().and_then(|st| st.condition)
    }

    /// Returns the expression id for the start expression of this stream
    /// If all parts of [StartNode] are required, see [start](fn@OORVIr1)
    pub(crate) fn start_expr(&self) -> Option<ExprNodeIdx> {
        self.start.as_ref().and_then(|st| st.expression)
    }

    /// Returns the pacing for the start condition of this stream
    /// If all parts of [StartNode] are required, see [start](fn@OORVIr1)
    #[allow(dead_code)]
    pub(crate) fn start_pacing(&self) -> Option<&PacingNode> {
        self.start.as_ref().map(|st| &st.pacing)
    }

    /// Returns the [EndNode] template of the stream
    pub(crate) fn end(&self) -> Option<&EndNode> {
        self.end.as_ref()
    }

    /// Returns the expression id for the end condition of this stream
    /// If all parts of [EndNode] are required, see [end](fn@OORVIr1)
    pub(crate) fn end_cond(&self) -> Option<ExprNodeIdx> {
        self.end.as_ref().map(|ct| ct.condition)
    }

    /// Returns the pacing for the end condition of this stream
    /// If all parts of [EndNode] are required, see [end](fn@OORVIr1))
    #[allow(dead_code)]
    pub(crate) fn end_pacing(&self) -> Option<&PacingNode> {
        self.end.as_ref().map(|ct| &ct.pacing)
    }

    /// Returns the [EvalNode] template of the stream
    pub(crate) fn eval(&self) -> &[EvalNode] {
        &self.eval
    }

    /// Yields the span referring to a part of the specification from which this stream originated.
    pub fn span(&self) -> SourceSpan {
        self.span
    }
}

impl ParamDecl {
    /// Yields the index of this parameter.  If the index is 3, then the parameter is the fourth parameter of the respective stream.
    pub fn index(&self) -> usize {
        self.position
    }

    /// Yields the span referring to a part of the specification where this parameter occurs.
    pub fn span(&self) -> SourceSpan {
        self.span
    }
}

impl Default for PacingNode {
    fn default() -> Self {
        PacingNode::Unspecified(SourceSpan::default())
    }
}

impl PacingNode {
    /// Returns the span of the annotated type.
    pub fn span(&self, source_ir: &OORVIr1) -> SourceSpan {
        match self {
            PacingNode::GlobalTick(freq) | PacingNode::LocalTick(freq) => freq.span,
            PacingNode::Event(id) => source_ir.expression(*id).span,
            PacingNode::Unspecified(span) => *span,
        }
    }
}

impl StartNode {
    /// Returns a reference to the `Expression` representing the start expression if it exists
    pub(crate) fn start_expr<'a>(&self, spec: &'a OORVIr1) -> Option<&'a Expression> {
        self.expression.map(|eid| spec.expression(eid))
    }

    /// Returns a vector of `Expression` references representing the expressions with which the parameters of the stream are initialized
    pub(crate) fn start_args<'a>(&self, spec: &'a OORVIr1) -> Vec<&'a Expression> {
        self.start_expr(spec)
            .map(|se| match &se.kind {
                ExprVariant::Tuple(starts) => starts.iter().collect(),
                _ => vec![se],
            })
            .unwrap_or_default()
    }

    /// Returns a reference to the `Expression` representing the start condition if it exists
    pub(crate) fn start_cond<'a>(&self, spec: &'a OORVIr1) -> Option<&'a Expression> {
        self.condition.map(|eid| spec.expression(eid))
    }
}

/// The OORVIr1 StartNode definition is composed of two optional expressions and the annotated pacing.
/// The first one refers to the start expression while the second one represents the start condition.
#[derive(Debug, Clone, Copy)]
pub struct InitView<'a> {
    /// The expression of the stream is started with, setting the parameters, e.g. start with (3,x)
    pub expression: Option<&'a Expression>,
    /// The conditional expression of the start, e.g. when x > 5
    pub condition: Option<&'a Expression>,
    /// The pacing type  of the start, e.g. @1Hz or @input_i
    pub annotated_pacing: &'a PacingNode,
    /// The range in the specification corresponding to the start clause.
    pub span: SourceSpan,
}

impl<'a> InitView<'a> {
    /// Constructs a new initialization-clause view.
    pub fn new(
        expression: Option<&'a Expression>,
        condition: Option<&'a Expression>,
        annotated_pacing: &'a PacingNode,
        span: SourceSpan,
    ) -> Self {
        Self {
            expression,
            condition,
            annotated_pacing,
            span,
        }
    }
}

/// The OORVIr1 EvalNode definition is composed of three expressions and the annotated pacing.
/// The first one refers to the evaluation condition, while the second one represents the evaluation expression, defining the value of the stream.
#[derive(Debug, Clone, Copy)]
pub struct ExecView<'a> {
    /// The evaluation condition has to evaluated to true in order for the stream expression to be evaluated.
    pub condition: Option<&'a Expression>,
    /// The stream expression defines the computed value of the stream.
    pub expression: &'a Expression,
    /// The annotated pacing of the stream evaluation, describing when the condition and expression should be evaluated in a temporal manner.
    pub annotated_pacing: &'a PacingNode,
    /// The range in the specification corresponding to the eval clause.
    pub span: SourceSpan,
}

impl<'a> ExecView<'a> {
    /// Constructs a new execution-clause view.
    pub fn new(
        condition: Option<&'a Expression>,
        expr: &'a Expression,
        annotated_pacing: &'a PacingNode,
        span: SourceSpan,
    ) -> Self {
        Self {
            condition,
            expression: expr,
            annotated_pacing,
            span,
        }
    }
}

/// The OORVIr1 EndNode definition is composed of the EndNode condition expression and the annotated pacing.
#[derive(Debug, Clone, Copy)]
pub struct EndView<'a> {
    /// The end condition, defining when a stream instance is terminated and no longer evaluated.
    pub condition: Option<&'a Expression>,
    /// The annotated pacing, indicating when the condition should be evaluated.
    pub annotated_pacing: &'a PacingNode,
    /// The range in the specification corresponding to the end clause.
    pub span: SourceSpan,
}

impl<'a> EndView<'a> {
    /// Constructs a new termination-clause view.
    pub fn new(
        condition: Option<&'a Expression>,
        annotated_pacing: &'a PacingNode,
        span: SourceSpan,
    ) -> Self {
        Self {
            condition,
            annotated_pacing,
            span,
        }
    }
}

impl ValueTyped {
    /// Yields a collection of primitive types and their names.
    pub(crate) fn primitive_types() -> Vec<(&'static str, &'static ValueTyped)> {
        let mut types = vec![];
        types.extend_from_slice(&crate::oorvir::source::builtins::PRIMITIVE_TYPES);
        types.extend_from_slice(&crate::oorvir::source::builtins::PRIMITIVE_TYPES_ALIASES);

        types
    }
}

impl StreamIdx {
    /// Returns the index inside the reference if it is an output reference.  Panics otherwise.
    pub fn out_ix(&self) -> usize {
        match self {
            StreamIdx::Signal(_) => unreachable!(),
            StreamIdx::Constraint(ix) => *ix,
        }
    }

    /// Returns the index inside the reference if it is an input reference.  Panics otherwise.
    pub fn in_ix(&self) -> usize {
        match self {
            StreamIdx::Signal(ix) => *ix,
            StreamIdx::Constraint(_) => unreachable!(),
        }
    }

    /// Returns the index inside the reference disregarding whether it is an input or output reference.
    pub fn ix_unchecked(&self) -> usize {
        match self {
            StreamIdx::Signal(ix) | StreamIdx::Constraint(ix) => *ix,
        }
    }

    /// True if the reference is an instance of [StreamIdx::Signal], false otherwise.
    pub fn is_input(&self) -> bool {
        match self {
            StreamIdx::Constraint(_) => false,
            StreamIdx::Signal(_) => true,
        }
    }

    /// True if the reference is an instance of [StreamIdx::Constraint], false otherwise.
    pub fn is_output(&self) -> bool {
        match self {
            StreamIdx::Constraint(_) => true,
            StreamIdx::Signal(_) => false,
        }
    }
}

impl PartialOrd for StreamIdx {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StreamIdx {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (StreamIdx::Signal(i), StreamIdx::Signal(i2)) => i.cmp(i2),
            (StreamIdx::Constraint(o), StreamIdx::Constraint(o2)) => o.cmp(o2),
            (StreamIdx::Signal(_), StreamIdx::Constraint(_)) => Ordering::Less,
            (StreamIdx::Constraint(_), StreamIdx::Signal(_)) => Ordering::Greater,
        }
    }
}

/// Shift used in the lookup expression
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum Shift {
    /// A strictly positive discrete offset, e.g., `4`, or `42`
    FutureDiscrete(u32),
    /// A non-negative discrete offset, e.g., `0`, `-4`, or `-42`
    PastDiscrete(u32),
    /// A positive real-time offset, e.g., `-3ms`, `-4min`, `-2.3h`
    FutureRealTime(Duration),
    /// A non-negative real-time offset, e.g., `0`, `4min`, `2.3h`
    PastRealTime(Duration),
}

impl Shift {
    /// Returns `true`, iff the Shift is negative
    pub(crate) fn is_backward_shift(&self) -> bool {
        match self {
            Shift::FutureDiscrete(_) | Shift::FutureRealTime(_) => false,
            Shift::PastDiscrete(o) => *o != 0,
            Shift::PastRealTime(o) => o.as_nanos() != 0,
        }
    }

    pub(crate) fn as_storage_bound(&self) -> StorageRequirement {
        match self {
            Shift::PastDiscrete(o) => {
                StorageRequirement::Bounded(*o) + StorageRequirement::Bounded(1)
            }
            Shift::FutureDiscrete(_) => unimplemented!(),
            Shift::FutureRealTime(_) => unimplemented!(),
            Shift::PastRealTime(_) => unimplemented!(),
        }
    }
}

impl PartialOrd for Shift {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Shift {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        use Shift::*;
        match (self, other) {
            (PastDiscrete(_), FutureDiscrete(_))
            | (PastRealTime(_), FutureRealTime(_))
            | (PastDiscrete(_), FutureRealTime(_))
            | (PastRealTime(_), FutureDiscrete(_)) => Ordering::Less,

            (FutureDiscrete(_), PastDiscrete(_))
            | (FutureDiscrete(_), PastRealTime(_))
            | (FutureRealTime(_), PastDiscrete(_))
            | (FutureRealTime(_), PastRealTime(_)) => Ordering::Greater,

            (FutureDiscrete(a), FutureDiscrete(b)) => a.cmp(b),
            (PastDiscrete(a), PastDiscrete(b)) => b.cmp(a),

            (_, _) => unimplemented!(),
        }
    }
}
