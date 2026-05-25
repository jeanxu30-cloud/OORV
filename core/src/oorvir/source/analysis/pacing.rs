use std::collections::{BTreeSet, HashSet};
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::ast::SourceSpan;
use crate::diagnostic::{Diagnostic, OORVError};
use itertools::Itertools;
use num::{CheckedDiv, Integer};
use rusttyc::{
    Arity, Constructable, Partial, PreliminaryTypeTable, TcErr, TcKey, TypeChecker, TypeTable,
    Variant,
};
use uom::lib::collections::HashMap;
use uom::lib::fmt::Formatter;
use uom::num_rational::Ratio;
use uom::si::frequency::hertz;
use uom::si::rational64::Frequency as UOM_Frequency;

use crate::oorvir::source::analysis::solver::{CheckFailure, FaultReporter, NodeRef};
use crate::oorvir::source::{
    AccessMode, ArithLogOp, Constant, Constraint, EndView, ExecView, ExprNodeIdx, ExprVariant,
    Expression, ExpressionContext, FnExprKind, InitView, Inlined, Literal, OORVIr1, PacingNode,
    Signal, StreamIdx, StreamPacingBundle, StreamPacingKind, ValueEq, WidenExprKind,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationCondition {
    /// A set of disjunctive conjunction clauses derived from the source expression.
    Models(BTreeSet<BTreeSet<StreamIdx>>),
    /// Always active; corresponds to the constant `true`.
    True,
}

impl ActivationCondition {
    pub(crate) fn with_stream(sref: StreamIdx) -> Self {
        ActivationCondition::Models(vec![vec![sref].into_iter().collect()].into_iter().collect())
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Hash)]
pub(crate) struct FreqUnit(UOM_Frequency);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RateNode {
    /// An event-driven stream that fires when its activation condition is satisfied.
    Event(ActivationCondition),
    /// A stream with a fixed global real-time period.
    GlobalPeriodic(FreqUnit),
    /// A stream with a fixed local real-time period.
    LocalPeriodic(FreqUnit),
    /// Either a local or global real-time stream; period not yet determined.
    #[allow(dead_code)]
    AnyPeriodic,
    /// Unconstrained top element; unifies with any other RateNode.
    Any,
}

#[derive(Debug, Clone)]
pub(crate) struct ExprHash {
    context: Rc<ExpressionContext>,
    expression: Expression,
}

impl PartialEq for ExprHash {
    fn eq(&self, other: &Self) -> bool {
        self.expression
            .value_eq(&other.expression, self.context.as_ref())
    }
}

impl Eq for ExprHash {}

fn hash_expr_kind<H: Hasher>(kind: &ExprVariant, state: &mut H) {
    match kind {
        ExprVariant::LoadConstant(c) => {
            1.hash(state);
            c.hash(state);
        }
        ExprVariant::ArithLog(op, args) => {
            2.hash(state);
            op.hash(state);
            args.iter().for_each(|arg| hash_expr_kind(&arg.kind, state));
        }
        ExprVariant::StreamAccess(target, kind, _) => {
            3.hash(state);
            target.hash(state);
            kind.hash(state);
        }
        ExprVariant::ParameterAccess(_, _) => {
            4.hash(state);
        }
        ExprVariant::Ite {
            condition,
            consequence,
            alternative,
        } => {
            5.hash(state);
            hash_expr_kind(&condition.kind, state);
            hash_expr_kind(&consequence.kind, state);
            hash_expr_kind(&alternative.kind, state);
        }
        ExprVariant::Tuple(children) => {
            6.hash(state);
            children
                .iter()
                .for_each(|child| hash_expr_kind(&child.kind, state))
        }
        ExprVariant::TupleAccess(target, idx) => {
            7.hash(state);
            hash_expr_kind(&target.kind, state);
            idx.hash(state);
        }
        ExprVariant::Function(func_def) => {
            8.hash(state);
            let FnExprKind {
                name,
                args,
                type_param,
            } = &func_def;
            name.hash(state);
            args.iter().for_each(|arg| hash_expr_kind(&arg.kind, state));
            type_param.hash(state);
        }
        ExprVariant::Widen(widen_kind) => {
            9.hash(state);
            let WidenExprKind { expr, ty } = &widen_kind;
            hash_expr_kind(&expr.kind, state);
            ty.hash(state);
        }
        ExprVariant::Default { expr, default } => {
            10.hash(state);
            hash_expr_kind(&expr.kind, state);
            hash_expr_kind(&default.kind, state);
        }
        ExprVariant::Quantified(quant, bindings1, bindings2, expr) => {
            12.hash(state);
            match quant {
                crate::oorvir::source::Quantifier::Forall => 1u8.hash(state),
                crate::oorvir::source::Quantifier::Exists => 2u8.hash(state),
            }
            for id in bindings1.iter() {
                id.name.hash(state);
            }
            for id in bindings2.iter() {
                id.name.hash(state);
            }
            hash_expr_kind(&expr.kind, state);
        }
        ExprVariant::QuantifiedVar(ident) => {
            13.hash(state);
            ident.name.hash(state);
        }
        _ => {
            println!("Unhandled expression kind in hashing: {:?}", kind);
            unreachable!("tips: all expression kinds should be covered")
        }
    }
}

impl Hash for ExprHash {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_expr_kind(&self.expression.kind, state);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CondCategory {
    /// Unconstrained top; unifies with any CondCategory.
    Any,
    /// An end-of-life condition  corresponds to a Negative polarity.
    Negative(CondKind),
    /// An eval filter condition  corresponds to a Positive polarity.
    Positive(CondKind),
}

impl CondCategory {
    pub(crate) fn neg_top() -> CondCategory {
        CondCategory::Negative(CondKind::Any)
    }

    pub(crate) fn pos_top() -> CondCategory {
        CondCategory::Positive(CondKind::Any)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CondKind {
    Any,
    Conjunction(HashSet<ExprHash>),
    Disjunction(HashSet<ExprHash>),
    Mixed(ExprHash),
    Literal(ExprHash),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PacingKeySet {
    pub(crate) eval_pacing: TcKey,
    pub(crate) eval_condition: TcKey,
    pub(crate) start_pacing: TcKey,
    pub(crate) start_condition: TcKey,
    pub(crate) end_pacing: TcKey,
    pub(crate) end_condition: TcKey,
}

#[derive(Debug)]
pub(crate) struct TemplateDetails {
    pub(crate) start_pacing: Option<StreamPacingKind>,
    pub(crate) start_cond: Option<Expression>,
    pub(crate) filter: Option<Expression>,
    pub(crate) end: Option<Expression>,
}

#[derive(Debug)]
pub(crate) enum PacingFault {
    RateAnnotationMissing(SourceSpan),
    DeadActivationCondition(SourceSpan),
    InvalidActivationExpr(SourceSpan, String),
    EventPeriodicConflict(RateNode, RateNode),
    LocalGlobalConflict(RateNode, RateNode),
    FilterMergeConflict(CondCategory, CondCategory),
    FilterConditionMismatch(CondCategory, CondCategory),
    TemplateParamRequired {
        who: SourceSpan,
        why: SourceSpan,
        inferred: Box<TemplateDetails>,
    },
    RateConflict(StreamPacingKind, StreamPacingKind),
    TemplateParamForbidden(SourceSpan),
    AmbiguousPacingInference(SourceSpan, StreamPacingKind),
    Other(SourceSpan, String, Vec<Box<dyn PrintableVariant>>),
    StartAnnotationConflict {
        access_span: SourceSpan,
        target_start_span: Option<SourceSpan>,
        source_start_span: Option<SourceSpan>,
        target_start_pacing: StreamPacingKind,
        target_start_condition: Option<Expression>,
        source_start_pacing: StreamPacingKind,
        source_start_condition: Option<Expression>,
    },
    EndAnnotationConflict {
        access_span: SourceSpan,
        target_end_span: Option<SourceSpan>,
        source_end_span: Option<SourceSpan>,
        target_end_pacing: StreamPacingKind,
        target_end_condition: Option<Expression>,
        source_end_pacing: StreamPacingKind,
        source_end_condition: Option<Expression>,
    },
    SyncArgMismatch {
        target_span: SourceSpan,
        target_start_expr: Expression,
        own_start_expr: Expression,
        arg: Expression,
    },
    NonParamSyncArg(SourceSpan),
    ArgCountMismatch {
        target_span: SourceSpan,
        exp_span: SourceSpan,
        given_num: usize,
        expected_num: usize,
    },
    GetFreshPacingConflict {
        is_get: bool,
        target: SourceSpan,
        target_type: StreamPacingKind,
        source: SourceSpan,
        source_type: StreamPacingKind,
    },
    UnannotatedMultiEval(SourceSpan),
    MultiEvalRateConflict(RateNode, RateNode, SourceSpan),
}

impl std::ops::BitAnd for ActivationCondition {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (ActivationCondition::Models(left), ActivationCondition::Models(right)) => {
                ActivationCondition::Models(
                    left.iter()
                        .flat_map(|conj1| {
                            right
                                .iter()
                                .map(move |conj2| conj1.union(conj2).copied().collect())
                        })
                        .collect(),
                )
            }
            (ActivationCondition::True, other) | (other, ActivationCondition::True) => other,
        }
    }
}

impl std::ops::BitOr for ActivationCondition {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        #[allow(clippy::suspicious_arithmetic_impl)]
        let conjunction = if let ActivationCondition::Models(m) = self.clone() & rhs.clone() {
            m
        } else {
            BTreeSet::new()
        };
        match (self, rhs) {
            (ActivationCondition::Models(left), ActivationCondition::Models(right)) => {
                ActivationCondition::Models(
                    left.union(&right)
                        .cloned()
                        .collect::<BTreeSet<_>>()
                        .union(&conjunction)
                        .cloned()
                        .collect(),
                )
            }
            (ActivationCondition::True, _) | (_, ActivationCondition::True) => {
                ActivationCondition::True
            }
        }
    }
}

impl ActivationCondition {
    fn parse(src: &Expression) -> Result<Self, PacingFault> {
        use ExprVariant::*;
        match &src.kind {
            LoadConstant(c) => {
                let lit = match c {
                    Constant::Basic(l) | Constant::Inlined(Inlined { lit: l, .. }) => l,
                };
                match lit {
                    Literal::Bool(true) => Ok(ActivationCondition::True),
                    Literal::Bool(false) => Err(PacingFault::InvalidActivationExpr(
                        src.span,
                        "Only 'true' is a valid literal in an activation condition".into(),
                    )),
                    _ => Err(PacingFault::InvalidActivationExpr(
                        src.span,
                        "Only boolean literals are allowed in activation conditions".into(),
                    )),
                }
            }
            StreamAccess(sref, mode, args) => {
                if !args.is_empty() {
                    return Err(PacingFault::InvalidActivationExpr(
                        src.span,
                        "Parametrized stream access is not allowed inside an activation condition"
                            .into(),
                    ));
                }
                if !matches!(mode, AccessMode::Strict) {
                    return Err(PacingFault::InvalidActivationExpr(
                        src.span,
                        "Only direct stream access is supported in activation conditions".into(),
                    ));
                }
                if sref.is_output() {
                    return Err(PacingFault::InvalidActivationExpr(
                        src.span,
                        "Activation conditions may only reference input streams".into(),
                    ));
                }
                Ok(ActivationCondition::with_stream(*sref))
            }
            ArithLog(op, operands) => {
                if operands.len() != 2 {
                    return Err(PacingFault::InvalidActivationExpr(
                        src.span,
                        "Only binary operators are permitted in activation conditions".into(),
                    ));
                }
                let left_ac = Self::parse(&operands[0])?;
                let right_ac = Self::parse(&operands[1])?;
                match op {
                    ArithLogOp::And | ArithLogOp::BitAnd => Ok(left_ac & right_ac),
                    ArithLogOp::Or | ArithLogOp::BitOr => Ok(left_ac | right_ac),
                    _ => Err(PacingFault::InvalidActivationExpr(
                        src.span,
                        "Only '&' (and) and '|' (or) are accepted in activation conditions".into(),
                    )),
                }
            }
            _ => Err(PacingFault::InvalidActivationExpr(
                src.span,
                "Activation conditions may only contain literals and binary logical operators"
                    .into(),
            )),
        }
    }

    /// Format this activation condition for display in error messages.
    pub fn to_string(&self, stream_names: &HashMap<StreamIdx, String>) -> String {
        match self {
            ActivationCondition::True => "\u{22a4}".into(),
            ActivationCondition::Models(disjuncts) => disjuncts
                .iter()
                .map(|clause| {
                    let atoms: String = clause
                        .iter()
                        .map(|si| stream_names[si].as_str())
                        .join(" \u{2227} ");
                    if clause.len() == 1 {
                        atoms
                    } else {
                        format!("({atoms})")
                    }
                })
                .join(" \u{2228} "),
        }
    }
}

impl FaultReporter for PacingFault {
    fn into_diagnostic(
        self,
        spans: &[&HashMap<TcKey, SourceSpan>],
        names: &HashMap<StreamIdx, String>,
        key1: Option<TcKey>,
        key2: Option<TcKey>,
    ) -> Diagnostic {
        let pacing_spans = spans[0];
        let exp_spans = spans[1];
        let pspan = |k: Option<TcKey>| k.and_then(|x| pacing_spans.get(&x).cloned());
        let espan = |k: Option<TcKey>| k.and_then(|x| exp_spans.get(&x).cloned());

        use PacingFault::*;
        match self {
            RateAnnotationMissing(span) => Diagnostic::error(
                "A frequency annotation is required for this pacing declaration",
            )
            .add_span_with_label(span, Some("annotation missing here"), true),

            DeadActivationCondition(span) => Diagnostic::error(
                "This stream or expression can never be evaluated; its activation condition is unsatisfiable",
            )
            .add_span_with_label(span, Some("declared here"), true)
            .add_note("Consider adding an explicit pacing annotation"),

            InvalidActivationExpr(span, reason) => Diagnostic::error(&format!(
                "Malformed activation condition: {reason}"
            ))
            .add_span_with_label(span, Some("at this position"), true),

            EventPeriodicConflict(r1, r2) => {
                let t1 = r1.to_pretty_string(names);
                let t2 = r2.to_pretty_string(names);
                Diagnostic::error(&format!(
                    "Pacing conflict: event-based '{t1}' and periodic '{t2}' cannot be unified"
                ))
                .maybe_add_span_with_label(pspan(key1), Some(&format!("'{t1}' inferred here")), true)
                .maybe_add_span_with_label(pspan(key2), Some(&format!("'{t2}' inferred here")), false)
            }

            LocalGlobalConflict(r1, r2) => {
                let t1 = r1.to_pretty_string(names);
                let t2 = r2.to_pretty_string(names);
                Diagnostic::error(&format!(
                    "Pacing conflict: local-periodic '{t1}' and global-periodic '{t2}' are incompatible"
                ))
                .maybe_add_span_with_label(pspan(key1), Some(&format!("'{t1}' here")), true)
                .maybe_add_span_with_label(pspan(key2), Some(&format!("'{t2}' here")), false)
            }

            FilterMergeConflict(e1, e2) => {
                let s1 = e1.to_pretty_string(names);
                let s2 = e2.to_pretty_string(names);
                Diagnostic::error(&format!(
                    "Incompatible filter conditions cannot be merged: '{s1}' vs '{s2}'"
                ))
                .maybe_add_span_with_label(espan(key1), Some(&format!("'{s1}' here")), true)
                .maybe_add_span_with_label(espan(key2), Some(&format!("'{s2}' here")), false)
            }

            AmbiguousPacingInference(span, inferred) => {
                let desc = inferred.describe_pacing(names);
                Diagnostic::warning(&format!(
                    "Inferred a complex pacing type: '{desc}'; consider adding an explicit @annotation"
                ))
                .add_span_with_label(span, Some("here"), true)
                .add_note(&format!("Explicit annotation: @{desc}"))
            }

            Other(span, reason, causes) => {
                let cause_str = causes.iter().map(|c| c.to_pretty_string(names)).join(" and ");
                Diagnostic::error(&format!("{reason} {cause_str}"))
                    .add_span_with_label(span, Some("here"), true)
                    .maybe_add_span_with_label(pspan(key1), Some("here"), true)
                    .maybe_add_span_with_label(pspan(key2), Some("here"), true)
            }

            TemplateParamForbidden(span) => Diagnostic::error(
                "Synchronous access to a parameterized stream is not permitted at this position",
            )
            .add_span_with_label(span, Some("access site"), true)
            .add_note("Consider using a hold (.get) access instead"),

            TemplateParamRequired { who, why, inferred } => {
                let TemplateDetails { start_pacing, start_cond, filter, end } = *inferred;
                let start_str = match (start_pacing, start_cond) {
                    (Some(p), Some(c)) => format!("\nstart @{} when {} with <...>", p.describe_pacing(names), c.format_with_names(names)),
                    (Some(p), None) => format!("\nstart @{} with <...>", p.describe_pacing(names)),
                    (None, Some(c)) => format!("\nstart when {} with <...>", c.format_with_names(names)),
                    (None, None) => String::new(),
                };
                let filter_str = filter.map_or(String::new(), |f| format!("\neval when {}", f.format_with_names(names)));
                let end_str = end.map_or(String::new(), |e| format!("\nend when {}", e.format_with_names(names)));
                Diagnostic::error("A template parameterization is required for this stream access")
                    .add_span_with_label(who, Some("access here"), true)
                    .add_span_with_label(why, Some("synchronous access occurs here"), false)
                    .add_note(&format!("Consider adding:{start_str}{filter_str}{end_str}"))
            }

            RateConflict(declared, inferred) => {
                let ds = declared.describe_pacing(names);
                let is = inferred.describe_pacing(names);
                Diagnostic::error(&format!(
                    "Declared pacing '{ds}' is incompatible with inferred pacing '{is}'"
                ))
                .maybe_add_span_with_label(pspan(key1), Some(&format!("declared '{ds}' here")), true)
                .maybe_add_span_with_label(pspan(key2), Some(&format!("inferred '{is}' here")), true)
            }

            FilterConditionMismatch(declared, inferred) => {
                let ds = declared.to_pretty_string(names);
                let is = inferred.to_pretty_string(names);
                Diagnostic::error(&format!(
                    "Filter condition does not match: expected '{ds}' but inferred '{is}'"
                ))
                .maybe_add_span_with_label(espan(key1), Some(&format!("expected '{ds}' here")), true)
                .maybe_add_span_with_label(espan(key2), Some(&format!("inferred '{is}' here")), true)
            }

            StartAnnotationConflict {
                access_span, target_start_span, source_start_span,
                target_start_pacing, target_start_condition,
                source_start_pacing, source_start_condition,
            } => {
                let tc = target_start_condition.map_or("true".into(), |c| c.format_with_names(names));
                let sc = source_start_condition.map_or("true".into(), |c| c.format_with_names(names));
                Diagnostic::error(
                    "Periodic stream is out of sync with the accessed stream (start annotation mismatch)",
                )
                .add_span_with_label(access_span, Some("synchronous access here"), true)
                .maybe_add_span_with_label(target_start_span, Some(&format!(
                    "target: start @({}) when {} with <...>",
                    target_start_pacing.describe_pacing(names), tc
                )), false)
                .maybe_add_span_with_label(source_start_span, Some(&format!(
                    "source: start @({}) when {} with <...>",
                    source_start_pacing.describe_pacing(names), sc
                )), false)
            }

            EndAnnotationConflict {
                access_span, target_end_span, source_end_span,
                target_end_pacing, target_end_condition,
                source_end_pacing, source_end_condition,
            } => {
                let tc = target_end_condition.map_or("true".into(), |c| c.format_with_names(names));
                let sc = source_end_condition.map_or("true".into(), |c| c.format_with_names(names));
                Diagnostic::error(
                    "Periodic stream is out of sync with the accessed stream (end annotation mismatch)",
                )
                .add_span_with_label(access_span, Some("synchronous access here"), true)
                .maybe_add_span_with_label(target_end_span, Some(&format!(
                    "target: end @({}) when {}",
                    target_end_pacing.describe_pacing(names), tc
                )), false)
                .maybe_add_span_with_label(source_end_span, Some(&format!(
                    "source: end @({}) when {}",
                    source_end_pacing.describe_pacing(names), sc
                )), false)
            }

            SyncArgMismatch { target_span, target_start_expr, own_start_expr, arg } => {
                let target_e = target_start_expr.format_with_names(names);
                let own_e = own_start_expr.format_with_names(names);
                let supplied = arg.format_with_names(names);
                Diagnostic::error("Invalid argument for synchronized stream access")
                    .add_span_with_label(target_span, Some(&format!("target expects argument equal to its start expression: ({target_e})")), false)
                    .add_span_with_label(arg.span, Some(&format!("supplied ({supplied}), which resolves to start expression: ({own_e})")), true)
                    .add_note("Each parameter of the accessed stream requires a matching parameter from the accessing stream")
            }

            NonParamSyncArg(span) => Diagnostic::error(
                "Only stream parameters are allowed as arguments in a synchronous stream access",
            )
            .add_span_with_label(span, Some("non-parameter expression found here"), true),

            ArgCountMismatch { target_span, exp_span, given_num, expected_num } => {
                Diagnostic::error("Argument count mismatch in stream access")
                    .add_span_with_label(exp_span, Some(&format!("provided {given_num} argument(s)")), true)
                    .add_span_with_label(target_span, Some(&format!("target expects {expected_num} argument(s)")), false)
            }

            GetFreshPacingConflict { is_get, target, target_type, source, source_type } => {
                let (op_name, fallback) = if is_get { ("'get'", "the default value") } else { ("'is_fresh()'", "false") };
                Diagnostic::error(&format!(
                    "{op_name} access will always yield {fallback} due to a pacing type mismatch"
                ))
                .add_span_with_label(source, Some(&format!("{op_name} access with inferred pacing '{}'", source_type.describe_pacing(names))), true)
                .add_span_with_label(target, Some(&format!("target has incompatible pacing '{}'", target_type.describe_pacing(names))), false)
            }

            UnannotatedMultiEval(span) => Diagnostic::error(
                "Outputs with multiple eval clauses must carry a pacing annotation on each clause",
            )
            .add_span_with_label(span, None::<&str>, false),

            MultiEvalRateConflict(r1, r2, span) => {
                let t1 = r1.to_pretty_string(names);
                let t2 = r2.to_pretty_string(names);
                Diagnostic::error(&format!(
                    "Eval clauses have conflicting frequencies: '{t1}' and '{t2}'"
                ))
                .add_span_with_label(span, None::<&str>, false)
            }
        }
    }
}

pub(crate) trait PrintableVariant: Debug {
    fn to_pretty_string(&self, names: &HashMap<StreamIdx, String>) -> String;
}

impl<V: 'static + Variant<Err = PacingFault> + PrintableVariant> From<TcErr<V>>
    for CheckFailure<PacingFault>
{
    fn from(err: TcErr<V>) -> CheckFailure<PacingFault> {
        let (kind, k1, k2) = match err {
            TcErr::KeyEquation(a, b, e) => (e, Some(a), Some(b)),
            TcErr::Bound(a, b, e) => (e, Some(a), b),
            TcErr::ChildAccessOutOfBound(key, ty, _) => {
                let msg = "Child type out of bounds for type: ".to_string();
                (
                    PacingFault::Other(SourceSpan::Unknown, msg, vec![Box::new(ty)]),
                    Some(key),
                    None,
                )
            }
            TcErr::ArityMismatch {
                key,
                variant,
                inferred_arity,
                reported_arity,
            } => {
                let msg =
                    format!("Arity {inferred_arity} expected but got {reported_arity} for type: ");
                (
                    PacingFault::Other(SourceSpan::Unknown, msg, vec![Box::new(variant)]),
                    Some(key),
                    None,
                )
            }
            TcErr::Construction(key, _, e) => (e, Some(key), None),
            TcErr::ChildConstruction(key, idx, prelim, e) => (e, Some(key), prelim.children[idx]),
            TcErr::CyclicGraph => panic!("Cyclic pacing type constraint graph detected"),
        };
        CheckFailure {
            kind,
            key1: k1,
            key2: k2,
        }
    }
}

impl std::fmt::Display for FreqUnit {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            (self.0).into_format_args(hertz, uom::fmt::DisplayStyle::Abbreviation)
        )
    }
}

impl FreqUnit {
    pub(crate) fn divides_evenly(&self, divisor: &FreqUnit) -> Result<bool, PacingFault> {
        if self.0.get::<hertz>() < divisor.0.get::<hertz>() {
            return Ok(false);
        }
        let dividend = self.0.get::<hertz>();
        match dividend.checked_div(&divisor.0.get::<hertz>()) {
            Some(quotient) => Ok(quotient.is_integer()),
            None => Err(PacingFault::Other(
                SourceSpan::Unknown,
                format!("Frequency division failed: {:?} / {:?}", self.0, divisor.0),
                vec![],
            )),
        }
    }

    /// Compute the GCD of two frequencies (used for pacing conjunction/meet).
    fn compute_lcm(&self, other: &FreqUnit) -> FreqUnit {
        let (ln, ld) = (
            *self.0.get::<hertz>().numer(),
            *self.0.get::<hertz>().denom(),
        );
        let (rn, rd) = (
            *other.0.get::<hertz>().numer(),
            *other.0.get::<hertz>().denom(),
        );
        let g = ln.gcd(&rn);
        let l = ld.lcm(&rd);
        let combined: Ratio<i64> = Ratio::new(g, l);
        FreqUnit(UOM_Frequency::new::<hertz>(combined))
    }
}

impl Variant for RateNode {
    type Err = PacingFault;

    fn top() -> Self {
        RateNode::Any
    }

    fn meet(left: Partial<Self>, right: Partial<Self>) -> Result<Partial<Self>, Self::Err> {
        use RateNode::*;
        debug_assert_eq!(left.least_arity, 0);
        debug_assert_eq!(right.least_arity, 0);

        let merged = match (left.variant.clone(), right.variant.clone()) {
            (Any, x) | (x, Any) => Ok(x),
            (Event(a), Event(b)) => Ok(Event(a & b)),
            (LocalPeriodic(f1), LocalPeriodic(f2)) => Ok(LocalPeriodic(f1.compute_lcm(&f2))),
            (GlobalPeriodic(f1), GlobalPeriodic(f2)) => Ok(GlobalPeriodic(f1.compute_lcm(&f2))),
            (AnyPeriodic, AnyPeriodic) => Ok(AnyPeriodic),
            (ev @ Event(_), other) | (other, ev @ Event(_)) => {
                Err(PacingFault::EventPeriodicConflict(ev, other))
            }
            (loc @ LocalPeriodic(_), glob @ GlobalPeriodic(_))
            | (glob @ GlobalPeriodic(_), loc @ LocalPeriodic(_)) => {
                Err(PacingFault::LocalGlobalConflict(loc, glob))
            }
            (AnyPeriodic, LocalPeriodic(f)) | (LocalPeriodic(f), AnyPeriodic) => {
                Ok(LocalPeriodic(f))
            }
            (AnyPeriodic, GlobalPeriodic(f)) | (GlobalPeriodic(f), AnyPeriodic) => {
                Ok(GlobalPeriodic(f))
            }
        }?;

        Ok(Partial {
            variant: merged,
            least_arity: 0,
        })
    }

    fn arity(&self) -> Arity {
        Arity::Fixed(0)
    }
}

impl Constructable for RateNode {
    type Type = StreamPacingKind;

    fn construct(&self, children: &[Self::Type]) -> Result<Self::Type, Self::Err> {
        debug_assert!(children.is_empty(), "RateNode carries no child types");
        // Map each lattice node to its concrete resolved scheduling kind.
        let resolved = match self {
            RateNode::Any => StreamPacingKind::Unconditional,
            RateNode::Event(cond) => StreamPacingKind::Conditional(cond.clone()),
            RateNode::AnyPeriodic => StreamPacingKind::UnknownClock,
            RateNode::GlobalPeriodic(fu) => StreamPacingKind::GlobalClock(fu.0),
            RateNode::LocalPeriodic(fu) => StreamPacingKind::LocalClock(fu.0),
        };
        Ok(resolved)
    }
}

impl PrintableVariant for RateNode {
    fn to_pretty_string(&self, names: &HashMap<StreamIdx, String>) -> String {
        use RateNode::*;
        match self {
            Any => "Any".into(),
            AnyPeriodic => "AnyPeriodic".into(),
            Event(ac) => ac.to_string(names),
            GlobalPeriodic(f) => format!("Global({f})"),
            LocalPeriodic(f) => format!("Local({f})"),
        }
    }
}

impl PrintableVariant for CondCategory {
    fn to_pretty_string(&self, names: &HashMap<StreamIdx, String>) -> String {
        let kind = match self {
            CondCategory::Any => return "Any".into(),
            CondCategory::Negative(k) => k,
            CondCategory::Positive(k) => k,
        };
        use CondKind::*;
        match kind {
            Any => "Any".into(),
            Literal(e) => format!("Literal({})", e.expression.format_with_names(names)),
            Mixed(e) => format!("Mixed({})", e.expression.format_with_names(names)),
            Conjunction(c) => c
                .iter()
                .map(|h| h.expression.format_with_names(names))
                .join(" \u{2227} "),
            Disjunction(d) => d
                .iter()
                .map(|h| h.expression.format_with_names(names))
                .join(" \u{2228} "),
        }
    }
}

impl RateNode {
    /// Convert a pacing annotation node from the source_ir into an abstract RateNode.
    pub(crate) fn from_pacing_type(
        pt: &PacingNode,
        source_ir: &OORVIr1,
    ) -> Result<Option<(Self, SourceSpan)>, PacingFault> {
        let result = match pt {
            PacingNode::Event(eid) => {
                let e = source_ir.expression(*eid);
                let ac = ActivationCondition::parse(e)?;
                Some((RateNode::Event(ac), e.span))
            }
            PacingNode::GlobalTick(tick) => {
                Some((RateNode::GlobalPeriodic(FreqUnit(tick.rate)), tick.span))
            }
            PacingNode::LocalTick(tick) => {
                Some((RateNode::LocalPeriodic(FreqUnit(tick.rate)), tick.span))
            }
            PacingNode::Unspecified(_) => None,
        };
        Ok(result)
    }

    /// Derive the combined RateNode from a set of eval clauses.
    pub(crate) fn from_eval_expressions(
        clauses: &[ExecView],
        source_ir: &OORVIr1,
    ) -> Result<Self, PacingFault> {
        let annotated: Vec<(RateNode, SourceSpan)> = clauses
            .iter()
            .map(|ev| {
                RateNode::from_pacing_type(ev.annotated_pacing, source_ir)
                    .transpose()
                    .unwrap_or_else(|| Err(PacingFault::UnannotatedMultiEval(ev.span)))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let combined_span = annotated
            .iter()
            .map(|(_, s)| *s)
            .reduce(|a, b| a.union(&b))
            .expect("at least one eval clause is required");

        annotated
            .into_iter()
            .map(|(rate, _)| Ok(rate))
            .reduce(|acc, cur| acc?.join_events(cur?, combined_span))
            .expect("at least one eval clause is required")
    }

    /// Combine two RateNodes as a disjunction (union of activation conditions,
    /// or period equality check for periodic streams).
    fn join_events(self, other: Self, location: SourceSpan) -> Result<Self, PacingFault> {
        use RateNode::*;
        match (self, other) {
            (Event(a), Event(b)) => Ok(Event(a | b)),
            (LocalPeriodic(p), LocalPeriodic(q)) if p == q => Ok(LocalPeriodic(p)),
            (GlobalPeriodic(p), GlobalPeriodic(q)) if p == q => Ok(GlobalPeriodic(p)),
            (Any, _) | (_, Any) => Ok(Any),
            (ev @ Event(_), other) | (other, ev @ Event(_)) => {
                Err(PacingFault::EventPeriodicConflict(ev, other))
            }
            (p, q) => Err(PacingFault::MultiEvalRateConflict(p, q, location)),
        }
    }
}

impl Variant for CondCategory {
    type Err = PacingFault;

    fn top() -> Self {
        CondCategory::Any
    }

    fn meet(left: Partial<Self>, right: Partial<Self>) -> Result<Partial<Self>, Self::Err> {
        debug_assert_eq!(left.least_arity, 0);
        debug_assert_eq!(right.least_arity, 0);

        let merged = match (left.variant.clone(), right.variant.clone()) {
            (Self::Any, other) | (other, Self::Any) => {
                return Ok(Partial {
                    variant: other,
                    least_arity: 0,
                });
            }
            (Self::Positive(_), Self::Negative(_)) | (Self::Negative(_), Self::Positive(_)) => {
                unreachable!("Positive and Negative CondCategory must never be merged")
            }

            (Self::Positive(lk), Self::Positive(rk)) => match (lk, rk) {
                (CondKind::Any, x) | (x, CondKind::Any) => Ok(CondCategory::Positive(x)),
                (CondKind::Literal(a), CondKind::Literal(b)) if a == b => {
                    Ok(CondCategory::Positive(CondKind::Literal(a)))
                }
                (CondKind::Literal(a), CondKind::Literal(b)) => Ok(CondCategory::Positive(
                    CondKind::Conjunction(vec![a, b].into_iter().collect()),
                )),
                (CondKind::Mixed(a), CondKind::Mixed(b)) if a == b => {
                    Ok(CondCategory::Positive(CondKind::Mixed(a)))
                }
                (CondKind::Mixed(_), CondKind::Mixed(_)) => Err(PacingFault::FilterMergeConflict(
                    left.variant,
                    right.variant,
                )),
                (CondKind::Literal(he), CondKind::Conjunction(mut set))
                | (CondKind::Conjunction(mut set), CondKind::Literal(he)) => {
                    set.insert(he);
                    Ok(CondCategory::Positive(CondKind::Conjunction(set)))
                }
                (CondKind::Literal(he), CondKind::Disjunction(disjs))
                | (CondKind::Disjunction(disjs), CondKind::Literal(he)) => {
                    if disjs.contains(&he) {
                        Ok(CondCategory::Positive(CondKind::Literal(he)))
                    } else {
                        Err(PacingFault::FilterMergeConflict(
                            left.variant,
                            right.variant,
                        ))
                    }
                }
                (CondKind::Conjunction(a), CondKind::Conjunction(b)) => Ok(CondCategory::Positive(
                    CondKind::Conjunction(a.union(&b).cloned().collect()),
                )),
                (CondKind::Disjunction(a), CondKind::Disjunction(b)) => {
                    let shared: HashSet<ExprHash> = a.intersection(&b).cloned().collect();
                    match shared.len() {
                        0 => Err(PacingFault::FilterMergeConflict(
                            left.variant,
                            right.variant,
                        )),
                        1 => Ok(CondCategory::Positive(CondKind::Literal(
                            shared.into_iter().next().unwrap(),
                        ))),
                        _ => Ok(CondCategory::Positive(CondKind::Disjunction(shared))),
                    }
                }
                (CondKind::Conjunction(_), _)
                | (_, CondKind::Conjunction(_))
                | (CondKind::Mixed(_), _)
                | (_, CondKind::Mixed(_)) => Err(PacingFault::FilterMergeConflict(
                    left.variant,
                    right.variant,
                )),
            },

            (Self::Negative(lk), Self::Negative(rk)) => match (lk, rk) {
                (CondKind::Any, x) | (x, CondKind::Any) => Ok(CondCategory::Negative(x)),
                (CondKind::Literal(a), CondKind::Literal(b)) if a == b => {
                    Ok(CondCategory::Negative(CondKind::Literal(a)))
                }
                (CondKind::Literal(a), CondKind::Literal(b)) => Ok(CondCategory::Negative(
                    CondKind::Disjunction(vec![a, b].into_iter().collect()),
                )),
                (CondKind::Mixed(a), CondKind::Mixed(b)) if a == b => {
                    Ok(CondCategory::Negative(CondKind::Mixed(a)))
                }
                (CondKind::Mixed(_), CondKind::Mixed(_)) => Err(PacingFault::FilterMergeConflict(
                    left.variant,
                    right.variant,
                )),
                (CondKind::Literal(he), CondKind::Conjunction(conjs))
                | (CondKind::Conjunction(conjs), CondKind::Literal(he)) => {
                    if conjs.contains(&he) {
                        Ok(CondCategory::Negative(CondKind::Literal(he)))
                    } else {
                        Err(PacingFault::FilterMergeConflict(
                            left.variant,
                            right.variant,
                        ))
                    }
                }
                (CondKind::Literal(he), CondKind::Disjunction(mut set))
                | (CondKind::Disjunction(mut set), CondKind::Literal(he)) => {
                    set.insert(he);
                    Ok(CondCategory::Negative(CondKind::Disjunction(set)))
                }
                (CondKind::Conjunction(a), CondKind::Conjunction(b)) => {
                    let shared: HashSet<ExprHash> = a.intersection(&b).cloned().collect();
                    match shared.len() {
                        0 => Err(PacingFault::FilterMergeConflict(
                            left.variant,
                            right.variant,
                        )),
                        1 => Ok(CondCategory::Negative(CondKind::Literal(
                            shared.into_iter().next().unwrap(),
                        ))),
                        _ => Ok(CondCategory::Negative(CondKind::Conjunction(shared))),
                    }
                }
                (CondKind::Disjunction(a), CondKind::Disjunction(b)) => Ok(CondCategory::Negative(
                    CondKind::Disjunction(a.union(&b).cloned().collect()),
                )),
                (CondKind::Conjunction(_), _)
                | (_, CondKind::Conjunction(_))
                | (CondKind::Mixed(_), _)
                | (_, CondKind::Mixed(_)) => Err(PacingFault::FilterMergeConflict(
                    left.variant,
                    right.variant,
                )),
            },
        }?;

        Ok(Partial {
            variant: merged,
            least_arity: 0,
        })
    }

    fn arity(&self) -> Arity {
        Arity::Fixed(0)
    }
}

impl Constructable for CondCategory {
    type Type = Expression;

    fn construct(&self, children: &[Self::Type]) -> Result<Self::Type, Self::Err> {
        debug_assert!(children.is_empty(), "CondCategory has no children");

        let (polarity_false, kind) = match self {
            CondCategory::Any => {
                return Err(PacingFault::Other(
                    SourceSpan::Unknown,
                    "Cannot concretize an unconstrained CondCategory::Any".into(),
                    vec![],
                ));
            }
            CondCategory::Negative(k) => (true, k),
            CondCategory::Positive(k) => (false, k),
        };

        match (polarity_false, kind) {
            (false, CondKind::Any) => Ok(Expression {
                kind: ExprVariant::LoadConstant(Constant::Basic(Literal::Bool(true))),
                eid: ExprNodeIdx(u32::MAX),
                span: SourceSpan::Unknown,
            }),
            (true, CondKind::Any) => Ok(Expression {
                kind: ExprVariant::LoadConstant(Constant::Basic(Literal::Bool(false))),
                eid: ExprNodeIdx(u32::MAX),
                span: SourceSpan::Unknown,
            }),
            (_, CondKind::Conjunction(set)) => {
                assert!(set.len() >= 2);
                let mut iter = set.iter();
                let first = iter.next().unwrap().expression.clone();
                Ok(iter.fold(first, |acc, item| {
                    let span = acc.span.union(&item.expression.span);
                    Expression {
                        kind: ExprVariant::ArithLog(
                            ArithLogOp::And,
                            vec![acc, item.expression.clone()],
                        ),
                        eid: ExprNodeIdx(u32::MAX),
                        span,
                    }
                }))
            }
            (_, CondKind::Disjunction(set)) => {
                assert!(set.len() >= 2);
                let mut iter = set.iter();
                let first = iter.next().unwrap().expression.clone();
                Ok(iter.fold(first, |acc, item| {
                    let span = acc.span.union(&item.expression.span);
                    Expression {
                        kind: ExprVariant::ArithLog(
                            ArithLogOp::Or,
                            vec![acc, item.expression.clone()],
                        ),
                        eid: ExprNodeIdx(u32::MAX),
                        span,
                    }
                }))
            }
            (_, CondKind::Literal(he)) | (_, CondKind::Mixed(he)) => Ok(he.expression.clone()),
        }
    }
}

impl std::fmt::Display for CondCategory {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let (tag, kind) = match self {
            CondCategory::Any => return write!(f, "Any"),
            CondCategory::Negative(k) => ("End", k),
            CondCategory::Positive(k) => ("Eval", k),
        };
        match kind {
            CondKind::Any => write!(f, "{tag}Any"),
            CondKind::Literal(e) => write!(f, "{tag}Literal({})", e.expression),
            CondKind::Mixed(e) => write!(f, "{tag}Mixed({})", e.expression),
            CondKind::Conjunction(set) => write!(
                f,
                "{tag}Conjunction({})",
                set.iter().map(|h| format!("{}", h.expression)).join(", ")
            ),
            CondKind::Disjunction(set) => write!(
                f,
                "{tag}Disjunction({})",
                set.iter().map(|h| format!("{}", h.expression)).join(", ")
            ),
        }
    }
}

impl CondKind {
    /// Combine two CondKind values using a set constructor for the result type.
    fn merge<F>(self, other: Self, make_set: F) -> CondKind
    where
        F: Fn(HashSet<ExprHash>) -> CondKind,
    {
        use CondKind::*;
        match (self, other) {
            (Any, _)
            | (_, Any)
            | (Mixed(_), _)
            | (_, Mixed(_))
            | (Conjunction(_), Disjunction(_))
            | (Disjunction(_), Conjunction(_)) => {
                panic!("CondKind::merge called with incompatible variants")
            }
            (Literal(x), Literal(y)) if x == y => Literal(x),
            (Literal(x), Literal(y)) => make_set(vec![x, y].into_iter().collect()),
            (Conjunction(mut set), Literal(e)) | (Literal(e), Conjunction(mut set)) => {
                set.insert(e);
                Conjunction(set)
            }
            (Disjunction(mut set), Literal(e)) | (Literal(e), Disjunction(mut set)) => {
                set.insert(e);
                Disjunction(set)
            }
            (Conjunction(mut a), Conjunction(b)) => {
                a.extend(b);
                Conjunction(a)
            }
            (Disjunction(mut a), Disjunction(b)) => {
                a.extend(b);
                Disjunction(a)
            }
        }
    }
}

impl CondCategory {
    /// Check (iteratively) whether an expression tree contains any And/Or operator.
    fn has_logical_connectives(exp: &Expression) -> bool {
        let mut stack: Vec<&Expression> = vec![exp];
        while let Some(node) = stack.pop() {
            if let ExprVariant::ArithLog(op, args) = &node.kind {
                match op {
                    ArithLogOp::And | ArithLogOp::Or => return true,
                    ArithLogOp::Not => stack.push(&args[0]),
                    _ => {}
                }
            }
        }
        false
    }

    /// Try to decompose an expression into a pure conjunction or pure disjunction.
    /// `direction`: None=undecided, Some(true)=conjunction, Some(false)=disjunction.
    /// Returns Err if the expression mixes conjunctions and disjunctions.
    fn decompose_cond(
        exp: &Expression,
        direction: Option<bool>,
        ctx: Rc<ExpressionContext>,
    ) -> Result<CondKind, ()> {
        match &exp.kind {
            ExprVariant::ArithLog(op, args) => match (op, direction) {
                (ArithLogOp::And, None) | (ArithLogOp::And, Some(true)) => {
                    let l = Self::decompose_cond(&args[0], Some(true), ctx.clone())?;
                    let r = Self::decompose_cond(&args[1], Some(true), ctx)?;
                    Ok(l.merge(r, CondKind::Conjunction))
                }
                (ArithLogOp::Or, None) | (ArithLogOp::Or, Some(false)) => {
                    let l = Self::decompose_cond(&args[0], Some(false), ctx.clone())?;
                    let r = Self::decompose_cond(&args[1], Some(false), ctx)?;
                    Ok(l.merge(r, CondKind::Disjunction))
                }
                (ArithLogOp::And, Some(false)) | (ArithLogOp::Or, Some(true)) => Err(()),
                (ArithLogOp::Not, _) => {
                    if Self::has_logical_connectives(exp) {
                        Err(())
                    } else {
                        Ok(CondKind::Literal(ExprHash {
                            context: ctx,
                            expression: exp.clone(),
                        }))
                    }
                }
                _ => Ok(CondKind::Literal(ExprHash {
                    context: ctx,
                    expression: exp.clone(),
                })),
            },
            ExprVariant::LoadConstant(_)
            | ExprVariant::Default { .. }
            | ExprVariant::Widen(_)
            | ExprVariant::Function(_)
            | ExprVariant::TupleAccess(_, _)
            | ExprVariant::Tuple(_)
            | ExprVariant::Ite { .. }
            | ExprVariant::StreamAccess(_, _, _)
            | ExprVariant::ParameterAccess(_, _)
            | ExprVariant::Quantified(_, _, _, _) => Ok(CondKind::Literal(ExprHash {
                context: ctx,
                expression: exp.clone(),
            })),
            _ => {
                println!(
                    "Unhandled expression kind in condition decomposition: {:?}",
                    exp.kind
                );
                unreachable!("all expression kinds must be covered")
            }
        }
    }

    /// Build a CondCategory from an expression with explicit polarity.
    /// `negate=true` produces a Negative (end) guard; `false` produces a Positive (eval) guard.
    pub(crate) fn build_guard(negate: bool, exp: &Expression, ctx: Rc<ExpressionContext>) -> Self {
        let kind = Self::decompose_cond(exp, None, ctx.clone()).unwrap_or_else(|_| {
            CondKind::Mixed(ExprHash {
                context: ctx,
                expression: exp.clone(),
            })
        });
        if negate {
            CondCategory::Negative(kind)
        } else {
            CondCategory::Positive(kind)
        }
    }

    /// Build an end-of-life guard from an expression.
    pub(crate) fn build_end_guard(exp: &Expression, ctx: Rc<ExpressionContext>) -> Self {
        Self::build_guard(true, exp, ctx)
    }

    /// Build an eval filter guard from an expression.
    pub(crate) fn build_eval_guard(exp: &Expression, ctx: Rc<ExpressionContext>) -> Self {
        Self::build_guard(false, exp, ctx)
    }

    /// Collect the combined eval-filter guard from multiple optional expressions.
    /// If any expression is absent, returns an unconstrained Positive guard.
    pub(crate) fn collect_guards(exps: &[Option<&Expression>], ctx: Rc<ExpressionContext>) -> Self {
        if exps.iter().any(|e| e.is_none()) {
            return CondCategory::Positive(CondKind::Any);
        }
        let combined = exps
            .iter()
            .copied()
            .flatten()
            .cloned()
            .reduce(|acc, e| Expression {
                kind: ExprVariant::ArithLog(ArithLogOp::Or, vec![acc, e]),
                eid: ExprNodeIdx(u32::MAX),
                span: SourceSpan::Unknown,
            })
            .expect("all expressions are Some (checked above)");
        Self::build_eval_guard(&combined, ctx)
    }

    /// Return true if `self` subsumes (implies) `other` in the CondCategory lattice.
    pub(crate) fn subsumes(&self, other: &Self) -> bool {
        fn kind_subsumes(a: &CondKind, b: &CondKind) -> bool {
            match (a, b) {
                (_, CondKind::Any) => true,
                (CondKind::Any, _) => false,
                (CondKind::Literal(x), CondKind::Literal(y)) => x == y,
                (CondKind::Mixed(x), CondKind::Mixed(y)) => x == y,
                (CondKind::Literal(_), CondKind::Conjunction(_)) => false,
                (CondKind::Conjunction(set), CondKind::Literal(he)) => set.contains(he),
                (CondKind::Literal(he), CondKind::Disjunction(set)) => set.contains(he),
                (CondKind::Disjunction(_), CondKind::Literal(_)) => false,
                (CondKind::Conjunction(a), CondKind::Conjunction(b)) => b.is_subset(a),
                (CondKind::Disjunction(a), CondKind::Disjunction(b)) => a.is_subset(b),
                _ => false,
            }
        }

        match (self, other) {
            (Self::Any, _) => false,
            (_, Self::Any) => true,
            (Self::Positive(_), Self::Negative(_)) | (Self::Negative(_), Self::Positive(_)) => {
                unreachable!("Positive and Negative CondCategory must never be compared")
            }
            (Self::Positive(a), Self::Positive(b)) => kind_subsumes(a, b),
            (Self::Negative(a), Self::Negative(b)) => match (a, b) {
                (CondKind::Any, _) => true,
                (_, CondKind::Any) => false,
                _ => kind_subsumes(a, b),
            },
        }
    }
}

impl StreamPacingKind {
    /// Returns a human-readable description of this pacing kind.
    pub fn describe_pacing(&self, names: &HashMap<StreamIdx, String>) -> String {
        match self {
            StreamPacingKind::Unconditional => "Unconditional".into(),
            StreamPacingKind::UnknownClock => "UnknownClock".into(),
            StreamPacingKind::Conditional(ac) => ac.to_string(names),
            StreamPacingKind::GlobalClock(f) => format!("Global({})", FreqUnit(*f)),
            StreamPacingKind::LocalClock(f) => format!("Local({})", FreqUnit(*f)),
        }
    }

    /// Returns true when `self` is a scheduling subtype of `other` in the pacing lattice.
    /// For event-driven streams: self implies other's activation condition.
    /// For clock-driven streams: self's frequency is an integer multiple of other's.
    pub fn is_subtype_of(&self, other: &Self) -> bool {
        use StreamPacingKind::*;
        match (self, other) {
            (_, Unconditional) => true,
            (Unconditional, _) => false,
            (Conditional(ac_a), Conditional(ac_b)) => match (ac_a, ac_b) {
                (_, ActivationCondition::True) => true,
                (ActivationCondition::True, _) => false,
                (ActivationCondition::Models(da), ActivationCondition::Models(db)) => da
                    .iter()
                    .all(|clause_a| db.iter().any(|clause_b| clause_b.is_subset(clause_a))),
            },
            (Conditional(_), _) | (_, Conditional(_)) => false,
            (GlobalClock(fa), GlobalClock(fb)) | (LocalClock(fa), LocalClock(fb)) => FreqUnit(*fb)
                .divides_evenly(&FreqUnit(*fa))
                .unwrap_or(false),
            (GlobalClock(_) | LocalClock(_), StreamPacingKind::UnknownClock) => true,
            (StreamPacingKind::UnknownClock, StreamPacingKind::UnknownClock) => false,
            _ => false,
        }
    }

    /// Like `is_subtype_of` but treats `Unconditional` as the most general pacing,
    /// used when checking termination-condition pacing compatibility.
    pub fn end_is_subtype_of(&self, other: &Self) -> bool {
        match (self, other) {
            (StreamPacingKind::Unconditional, _) => true,
            (_, StreamPacingKind::Unconditional) => false,
            _ => self.is_subtype_of(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Variable(String);

impl rusttyc::TcVar for Variable {}

/// The [PacingAnalyzer] is used to infer the evaluation rate, as well as end, filter and start types for all
/// streams and expressions in the given [OORVIr1].
pub(crate) struct PacingAnalyzer<'a> {
    /// The [OORVIr1] to check.
    pub(crate) spec: &'a OORVIr1,
    /// The RustTyc [TypeChecker] used to infer the pacing type.
    pub(crate) rate_checker: TypeChecker<RateNode, Variable>,
    /// A second RustTyc [TypeChecker] instance to infer expression types, e.g. for the end and filter expression.
    pub(crate) cond_checker: TypeChecker<CondCategory, Variable>,
    /// Lookup table for stream keys
    pub(crate) key_map: HashMap<NodeRef, PacingKeySet>,
    /// Maps a RustTyc key of the rate_checker to the corresponding span for error reporting.
    pub(crate) rate_span: HashMap<TcKey, SourceSpan>,
    /// Maps a RustTyc key of the cond_checker to the corresponding span for error reporting.
    pub(crate) cond_span: HashMap<TcKey, SourceSpan>,
    /// Lookup table for the name of a given stream.
    pub(crate) names: &'a HashMap<StreamIdx, String>,
    /// Storage to register exact type implications during OORVIr1 climbing, resolved and checked during post process.
    /// First tuple element should imply the second tuple element.
    /// Boolean value indicates that the check originates from a end condition.
    pub(crate) rate_checks: Vec<(TcKey, TcKey, bool)>,
    /// Storage to register exact type bounds during OORVIr1 climbing, resolved and checked during post process.
    /// First tuple element should imply the second tuple element.
    /// Boolean value indicates that the check originates from a end condition.
    pub(crate) cond_checks: Vec<(TcKey, TcKey, bool)>,
    /// Expression context providing equivalence for parameters of different streams needed for expression equality.
    pub(crate) expr_ctx: Rc<ExpressionContext>,
}

impl<'a> PacingAnalyzer<'a> {
    /// Creates a new [ValueTypeChecker]. `names`table can be generated from the `OORVIr1`.
    /// Inits all internal hash maps.
    pub(crate) fn new(spec: &'a OORVIr1, names: &'a HashMap<StreamIdx, String>) -> Self {
        let key_map = HashMap::new();

        let expr_ctx = Rc::new(ExpressionContext::new(spec));
        let rate_checker = TypeChecker::new();
        let cond_checker = TypeChecker::new();
        let rate_span = HashMap::new();
        let cond_span = HashMap::new();
        let rate_checks = Vec::new();
        let cond_checks = Vec::new();
        let mut res = PacingAnalyzer {
            spec,
            rate_checker,
            cond_checker,
            key_map,
            rate_span,
            cond_span,
            names,
            rate_checks,
            cond_checks,
            expr_ctx,
        };
        res.allocate_stream_keys();
        res
    }

    fn alloc_stream_keys(&mut self) -> PacingKeySet {
        let end = self.cond_checker.new_term_key();
        let start = self.cond_checker.new_term_key();
        let filter = self.cond_checker.new_term_key();
        self.cond_checker
            .impose(end.concretizes_explicit(CondCategory::Negative(CondKind::Any)))
            .expect("end key cannot be bound otherwise yet");
        self.cond_checker
            .impose(start.concretizes_explicit(CondCategory::Positive(CondKind::Any)))
            .expect("end key cannot be bound otherwise yet");
        self.cond_checker
            .impose(filter.concretizes_explicit(CondCategory::Positive(CondKind::Any)))
            .expect("end key cannot be bound otherwise yet");
        PacingKeySet {
            eval_pacing: self.rate_checker.new_term_key(),
            eval_condition: filter,
            start_pacing: self.rate_checker.new_term_key(),
            start_condition: start,
            end_pacing: self.rate_checker.new_term_key(),
            end_condition: end,
        }
    }

    fn bind_span(&mut self, keys: PacingKeySet, span: SourceSpan) {
        self.rate_span.insert(keys.eval_pacing, span);
        self.rate_span.insert(keys.start_pacing, span);
        self.rate_span.insert(keys.end_pacing, span);
        self.cond_span.insert(keys.start_condition, span);
        self.cond_span.insert(keys.eval_condition, span);
        self.cond_span.insert(keys.end_condition, span);
    }

    fn impose_more_refined(
        &mut self,
        keys_l: PacingKeySet,
        keys_r: PacingKeySet,
    ) -> Result<(), CheckFailure<PacingFault>> {
        self.rate_checker
            .impose(keys_l.eval_pacing.concretizes(keys_r.eval_pacing))?;
        self.cond_checker
            .impose(keys_l.eval_condition.concretizes(keys_r.eval_condition))?;
        self.rate_checker
            .impose(keys_l.start_pacing.concretizes(keys_r.start_pacing))?;
        self.cond_checker
            .impose(keys_l.start_condition.concretizes(keys_r.start_condition))?;
        self.rate_checker
            .impose(keys_l.end_pacing.concretizes(keys_r.end_pacing))?;
        self.cond_checker
            .impose(keys_l.end_condition.concretizes(keys_r.end_condition))?;
        Ok(())
    }

    fn allocate_stream_keys(&mut self) {
        for input in self.spec.signals() {
            let key = self.alloc_stream_keys();
            self.key_map.insert(NodeRef::StreamIdx(input.si), key);
            self.rate_span.insert(key.eval_pacing, input.span);
            self.rate_span.insert(key.start_pacing, SourceSpan::Unknown);
            self.cond_span
                .insert(key.start_condition, SourceSpan::Unknown);
            self.cond_span
                .insert(key.eval_condition, SourceSpan::Unknown);
            self.cond_span
                .insert(key.end_condition, SourceSpan::Unknown);
        }
        for output in self.spec.constraints() {
            let key = self.alloc_stream_keys();
            let start_span = self
                .spec
                .start(output.si)
                .map(|start| start.span)
                .unwrap_or(SourceSpan::Unknown);
            let eval_span = self
                .spec
                .eval_unchecked(output.si)
                .iter()
                .map(|eval| eval.span)
                .reduce(|e1, e2| e1.union(&e2))
                .unwrap_or(SourceSpan::Unknown);
            let end_span = self
                .spec
                .end(output.si)
                .map(|end| end.span)
                .unwrap_or(SourceSpan::Unknown);

            self.key_map.insert(NodeRef::StreamIdx(output.si), key);
            for (i, eval) in self.spec.eval_unchecked(output.si).iter().enumerate() {
                let eval_key = self.alloc_stream_keys();
                self.key_map.insert(NodeRef::Eval(i, output.si), eval_key);
                self.bind_span(eval_key, eval.span);
            }

            self.rate_span.insert(key.eval_pacing, eval_span);
            self.rate_span.insert(key.start_pacing, start_span);
            self.rate_span.insert(key.end_pacing, end_span);
            self.cond_span.insert(
                key.start_condition,
                output
                    .start_cond()
                    .map(|id| self.spec.expression(id).span)
                    .unwrap_or(SourceSpan::Unknown),
            );
            self.cond_span.insert(
                key.eval_condition,
                output
                    .eval
                    .iter()
                    .filter_map(|eval| eval.condition)
                    .map(|id| self.spec.expression(id).span)
                    .reduce(|e1, e2| e1.union(&e2))
                    .unwrap_or(SourceSpan::Unknown),
            );
            self.cond_span.insert(
                key.end_condition,
                output
                    .end_cond()
                    .map(|cond| self.spec.expression(cond).span)
                    .unwrap_or(SourceSpan::Unknown),
            );

            // Create Stream Parameters
            for (idx, parameter) in output.params.iter().enumerate() {
                let key = self.alloc_stream_keys();
                self.key_map.insert(NodeRef::Param(idx, output.si), key);
                self.bind_span(key, parameter.span);
            }
        }
    }

    /// Binds the key to the given annotated pacing type
    fn register_pacing_check(&mut self, target: TcKey, conflict_key: TcKey, is_end: bool) {
        self.rate_checks.push((target, conflict_key, is_end));
    }

    /// Binds the key to the given annotated expression type
    fn register_cond_check(&mut self, target: TcKey, conflict_key: TcKey, is_end: bool) {
        self.cond_checks.push((target, conflict_key, is_end));
    }

    fn input_infer(&mut self, input: &Signal) -> Result<(), CheckFailure<PacingFault>> {
        let ac = RateNode::Event(ActivationCondition::with_stream(input.si));
        let keys = self.key_map[&NodeRef::StreamIdx(input.si)];
        self.rate_checker
            .impose(keys.eval_pacing.concretizes_explicit(ac))?;
        Ok(())
    }

    fn output_infer(&mut self, output: &Constraint) -> Result<(), CheckFailure<PacingFault>> {
        // Keys to capture the types of a whole stream
        let stream_keys = self.key_map[&NodeRef::StreamIdx(output.si)];
        let eval_keys = self.alloc_stream_keys();
        let eval_spans = output
            .eval
            .iter()
            .map(|eval| eval.span)
            .reduce(|eval1, eval2| eval1.union(&eval2))
            .unwrap();
        self.bind_span(eval_keys, eval_spans);

        let infer_pacing = self.spec.eval_unchecked(output.si).len() == 1;
        if !infer_pacing {
            let annotated_pacing =
                RateNode::from_eval_expressions(&self.spec.eval_unchecked(output.si), self.spec)?;
            self.rate_checker.impose(
                stream_keys
                    .eval_pacing
                    .concretizes_explicit(annotated_pacing),
            )?;
            let filters = self.spec.eval_cond(output.si).unwrap();
            self.cond_checker
                .impose(stream_keys.eval_condition.concretizes_explicit(
                    CondCategory::collect_guards(&filters, self.expr_ctx.clone()),
                ))?;
        } else {
            self.rate_checker
                .impose(stream_keys.eval_pacing.concretizes(eval_keys.eval_pacing))?;
            self.cond_checker.impose(
                stream_keys
                    .eval_condition
                    .concretizes(eval_keys.eval_condition),
            )?;
        }
        // Type filter
        for (i, eval) in self.spec.eval_unchecked(output.si).iter().enumerate() {
            let current_eval_keys = self.key_map[&NodeRef::Eval(i, output.si)];
            self.eval_infer(eval, current_eval_keys, infer_pacing)?;
            self.impose_more_refined(eval_keys, current_eval_keys)?;
        }

        // Type start condition
        if let Some(start) = self.spec.start(output.si) {
            self.start_infer(&start, stream_keys, eval_keys)?;
        }

        //Type end
        if let Some(end) = self.spec.end(output.si) {
            self.end_infer(&end, stream_keys, eval_keys)?;
        }
        Ok(())
    }

    fn eval_infer(
        &mut self,
        eval: &ExecView,
        eval_keys: PacingKeySet,
        infer: bool,
    ) -> Result<(), CheckFailure<PacingFault>> {
        let expr_keys = self.expression_infer(eval.expression)?;
        let filter_keys = eval
            .condition
            .map(|expr| self.expression_infer(expr))
            .unwrap_or_else(|| Ok(self.alloc_stream_keys()))?;

        self.impose_more_refined(eval_keys, expr_keys)?;
        self.impose_more_refined(eval_keys, filter_keys)?;

        if let Some((annotated_ty, _)) =
            RateNode::from_pacing_type(eval.annotated_pacing, self.spec)?
        {
            let annotation_key = self.alloc_stream_keys();
            self.bind_span(annotation_key, eval.annotated_pacing.span(self.spec));
            self.rate_checker.impose(
                annotation_key
                    .eval_pacing
                    .concretizes_explicit(annotated_ty),
            )?;
            self.impose_more_refined(eval_keys, annotation_key)?;
            self.register_pacing_check(annotation_key.eval_pacing, eval_keys.eval_pacing, false);
        }

        if let Some(condition) = eval.condition {
            let cond_key = self.alloc_stream_keys();
            self.bind_span(cond_key, condition.span);
            self.cond_checker
                .impose(cond_key.eval_condition.concretizes_explicit(
                    CondCategory::build_eval_guard(condition, self.expr_ctx.clone()),
                ))?;
            if infer {
                // This is only needed if the eval condition has to be inferred, i.e. if there is only one eval clause
                // Otherwise the eval condition is already set in the infer_output function.
                self.cond_checker.impose(
                    eval_keys
                        .eval_condition
                        .concretizes(cond_key.eval_condition),
                )?;
            }
            self.register_cond_check(cond_key.eval_condition, eval_keys.eval_condition, false);
        }
        Ok(())
    }

    fn start_infer(
        &mut self,
        start: &InitView,
        stream_keys: PacingKeySet,
        eval_keys: PacingKeySet,
    ) -> Result<(), CheckFailure<PacingFault>> {
        // Keys to capture the types of the eval clause
        let start_keys = self.alloc_stream_keys();
        self.bind_span(start_keys, start.span);

        let start_expr_keys = start
            .expression
            .map(|expr| self.expression_infer(expr))
            .unwrap_or_else(|| Ok(self.alloc_stream_keys()))?;
        let start_condition_keys = start
            .condition
            .map(|expr| self.expression_infer(expr))
            .unwrap_or_else(|| Ok(self.alloc_stream_keys()))?;

        self.impose_more_refined(start_keys, start_expr_keys)?;
        self.impose_more_refined(start_keys, start_condition_keys)?;

        // start pacing
        if let Some((annotated_ty, span)) =
            RateNode::from_pacing_type(start.annotated_pacing, self.spec)?
        {
            self.rate_span.insert(stream_keys.start_pacing, span);
            self.rate_checker
                .impose(stream_keys.start_pacing.concretizes_explicit(annotated_ty))?;
            self.register_pacing_check(stream_keys.start_pacing, start_keys.eval_pacing, false);
        } else {
            self.rate_checker
                .impose(stream_keys.start_pacing.concretizes(start_keys.eval_pacing))?;
        }
        // Start expression pacing implies the start pacing of the evaluation
        self.register_pacing_check(stream_keys.start_pacing, eval_keys.start_pacing, false);

        // start condition
        if let Some(condition) = start.condition {
            self.cond_checker
                .impose(stream_keys.start_condition.concretizes_explicit(
                    CondCategory::build_eval_guard(condition, self.expr_ctx.clone()),
                ))?;
            self.register_cond_check(
                stream_keys.start_condition,
                start_keys.start_condition,
                false,
            );
        } else {
            self.cond_checker.impose(
                stream_keys
                    .start_condition
                    .concretizes(eval_keys.start_condition),
            )?;
            self.cond_checker.impose(
                stream_keys
                    .start_condition
                    .concretizes(start_keys.eval_condition),
            )?;
        }
        self.register_cond_check(
            stream_keys.start_condition,
            eval_keys.start_condition,
            false,
        );
        // Start condition is more concrete than the start condition of the expression
        self.register_cond_check(
            stream_keys.start_condition,
            start_keys.eval_condition,
            false,
        );

        Ok(())
    }

    fn end_infer(
        &mut self,
        end: &EndView,
        stream_keys: PacingKeySet,
        eval_keys: PacingKeySet,
    ) -> Result<(), CheckFailure<PacingFault>> {
        let end_cond = end
            .condition
            .map(|expr| self.expression_infer(expr))
            .unwrap_or_else(|| Ok(self.alloc_stream_keys()))?;
        if end.condition.is_none() {
            self.bind_span(end_cond, end.span);
        }

        // end pacing
        if let Some((annotated_ty, span)) =
            RateNode::from_pacing_type(end.annotated_pacing, self.spec)?
        {
            self.rate_span.insert(stream_keys.end_pacing, span);
            self.rate_checker
                .impose(stream_keys.end_pacing.concretizes_explicit(annotated_ty))?;
            self.register_pacing_check(stream_keys.end_pacing, end_cond.eval_pacing, false);
        } else {
            self.rate_checker
                .impose(stream_keys.end_pacing.concretizes(end_cond.eval_pacing))?;
        }
        // end pacing of the eval condition implies the end pacing
        self.register_pacing_check(stream_keys.end_pacing, eval_keys.end_pacing, true);

        // end condition
        if let Some(condition) = end.condition {
            //Streams end condition is equal to annotated condition
            self.cond_checker
                .impose(stream_keys.end_condition.concretizes_explicit(
                    CondCategory::build_end_guard(condition, self.expr_ctx.clone()),
                ))?;
        } else {
            self.cond_checker.impose(
                stream_keys
                    .end_condition
                    .concretizes(eval_keys.end_condition),
            )?;
        }
        self.register_cond_check(stream_keys.end_condition, eval_keys.end_condition, true);

        Ok(())
    }

    fn expression_infer(
        &mut self,
        exp: &Expression,
    ) -> Result<PacingKeySet, CheckFailure<PacingFault>> {
        let term_keys: PacingKeySet = self.alloc_stream_keys();
        match &exp.kind {
            ExprVariant::LoadConstant(_) | ExprVariant::ParameterAccess(_, _) => {
                //constants have arbitrary pacing type
            }
            ExprVariant::StreamAccess(sref, kind, args) => {
                let stream_key = self.key_map[&NodeRef::StreamIdx(*sref)];

                match kind {
                    AccessMode::Strict | AccessMode::Shift(_) => {
                        self.impose_more_refined(term_keys, stream_key)?;

                        //Check that arguments are equal to start target if parameterized or the parameters for self
                        let target_start_args = self
                            .spec
                            .output(*sref)
                            .and_then(|o| o.start())
                            .map(|st| st.start_args(self.spec))
                            .unwrap_or_default();

                        let target_span = match sref {
                            StreamIdx::Signal(_) => self.spec.input(*sref).unwrap().span,
                            StreamIdx::Constraint(_) => self.spec.output(*sref).unwrap().span,
                        };

                        if target_start_args.len() != args.len() {
                            return Err(PacingFault::ArgCountMismatch {
                                target_span,
                                exp_span: exp.span,
                                given_num: args.len(),
                                expected_num: target_start_args.len(),
                            }
                            .into());
                        }
                        if !args.is_empty() {
                            // Quantified object variables are runtime instance placeholders.
                            // They are replaced by concrete stream parameters during
                            // quantifier evaluation, so they are admissible here.
                            let non_param = args.iter().find(|e| {
                                !matches!(
                                    e.kind,
                                    ExprVariant::ParameterAccess(_, _)
                                        | ExprVariant::QuantifiedVar(_)
                                )
                            });
                            if let Some(expr) = non_param {
                                return Err(PacingFault::NonParamSyncArg(expr.span).into());
                            }

                            // Check that every parameter in argument corresponds to one with an equal start expression of the target
                            for (target_idx, arg) in args.iter().enumerate() {
                                let (current_stream, current_idx) = match arg.kind {
                                    ExprVariant::ParameterAccess(c, c_idx) => (c, c_idx),
                                    ExprVariant::QuantifiedVar(_) => continue,
                                    _ => unreachable!(),
                                };
                                if !self.expr_ctx.matches(
                                    current_stream,
                                    current_idx,
                                    *sref,
                                    target_idx,
                                ) {
                                    let own_start_expr = self
                                        .spec
                                        .output(current_stream)
                                        .and_then(|o| o.start())
                                        .map(|st| st.start_args(self.spec)[current_idx].clone())
                                        .expect(
                                            "Target of sync access must have a start expression",
                                        );
                                    return Err(PacingFault::SyncArgMismatch {
                                        target_span,
                                        target_start_expr: target_start_args[target_idx].clone(),
                                        own_start_expr,
                                        arg: arg.clone(),
                                    }
                                    .into());
                                }
                            }
                        }
                    }
                    AccessMode::Cached | AccessMode::Get | AccessMode::Fresh => {}
                };

                for arg in args {
                    let arg_key = self.expression_infer(arg)?;
                    self.impose_more_refined(term_keys, arg_key)?;
                }
            }
            ExprVariant::Default { expr, default } => {
                let ex_key = self.expression_infer(expr)?;
                let def_key = self.expression_infer(default)?;

                self.impose_more_refined(term_keys, ex_key)?;
                self.impose_more_refined(term_keys, def_key)?;
            }
            ExprVariant::ArithLog(_, args) => match args.len() {
                2 => {
                    let left_key = self.expression_infer(&args[0])?;
                    let right_key = self.expression_infer(&args[1])?;

                    self.impose_more_refined(term_keys, left_key)?;
                    self.impose_more_refined(term_keys, right_key)?;
                }
                1 => {
                    let ex_key = self.expression_infer(&args[0])?;
                    self.impose_more_refined(term_keys, ex_key)?;
                }
                _ => unreachable!(),
            },
            ExprVariant::Ite {
                condition,
                consequence,
                alternative,
            } => {
                let cond_key = self.expression_infer(condition)?;
                let cons_key = self.expression_infer(consequence)?;
                let alt_key = self.expression_infer(alternative)?;

                self.impose_more_refined(term_keys, cond_key)?;
                self.impose_more_refined(term_keys, cons_key)?;
                self.impose_more_refined(term_keys, alt_key)?;
            }
            ExprVariant::Tuple(elements) => {
                for e in elements {
                    let ele_keys = self.expression_infer(e)?;
                    self.impose_more_refined(term_keys, ele_keys)?;
                }
            }
            ExprVariant::Function(FnExprKind { args, .. }) => {
                for arg in args {
                    let arg_key = self.expression_infer(arg)?;
                    self.impose_more_refined(term_keys, arg_key)?;
                }
            }
            ExprVariant::TupleAccess(t, _) => {
                let exp_key = self.expression_infer(t)?;
                self.impose_more_refined(term_keys, exp_key)?;
            }
            ExprVariant::Widen(WidenExprKind { expr: inner, .. }) => {
                let exp_key = self.expression_infer(inner)?;
                self.impose_more_refined(term_keys, exp_key)?;
            }
            ExprVariant::Quantified(_quantifier, _binding1, _binding2, _inner) => {
                // Object-domain quantifiers are evaluated by iterating runtime
                // instances.  Parameterized accesses inside the body are local
                // to that iteration and must not force the enclosing world
                // constraint to become a template stream.
            }
            ExprVariant::QuantifiedVar(ident) => {
                // Try to resolve the quantified var as a stream name; if it refers to a stream,
                // propagate the stream's keys to this expression. Otherwise leave as unconstrained.
                // Check constraints first
                let mut found = false;
                for out in self.spec.constraints() {
                    if out.name() == ident.name {
                        let stream_key = self.key_map[&NodeRef::StreamIdx(out.si)];
                        self.impose_more_refined(term_keys, stream_key)?;
                        found = true;
                        break;
                    }
                }
                if !found {
                    for inp in self.spec.signals() {
                        if inp.name == ident.name {
                            let stream_key = self.key_map[&NodeRef::StreamIdx(inp.si)];
                            self.impose_more_refined(term_keys, stream_key)?;
                            break;
                        }
                    }
                }
            }
            _ => {
                unreachable!("tips: all expression kinds should be covered")
            }
        };
        self.key_map.insert(NodeRef::Expr(exp.eid), term_keys);
        self.bind_span(term_keys, exp.span);
        Ok(term_keys)
    }

    fn check_explicit_bounds(
        pacing_checks: Vec<(TcKey, TcKey, bool)>,
        exp_checks: Vec<(TcKey, TcKey, bool)>,
        pacing_tt: &TypeTable<RateNode>,
        exp_tt: &PreliminaryTypeTable<CondCategory>,
    ) -> Vec<CheckFailure<PacingFault>> {
        let pacing_errs = pacing_checks
            .into_iter()
            .filter_map(|(left, right, is_end)| {
                let is = pacing_tt[&left].clone();
                let inferred = pacing_tt[&right].clone();
                if (is_end && !inferred.end_is_subtype_of(&is))
                    || (!is_end && !is.is_subtype_of(&inferred))
                {
                    Some(CheckFailure {
                        kind: PacingFault::RateConflict(is, inferred),
                        key1: Some(left),
                        key2: Some(right),
                    })
                } else {
                    None
                }
            });
        let exp_errs = exp_checks.into_iter().filter_map(|(left, right, is_end)| {
            let is = &exp_tt[&left].variant;
            let inferred = &exp_tt[&right].variant;
            if (is_end && !inferred.subsumes(is)) || (!is_end && !is.subsumes(inferred)) {
                Some(CheckFailure {
                    kind: PacingFault::FilterConditionMismatch(is.clone(), inferred.clone()),
                    key1: Some(left),
                    key2: Some(right),
                })
            } else {
                None
            }
        });
        pacing_errs.chain(exp_errs).collect()
    }

    fn is_parameterized(
        keys: PacingKeySet,
        pacing_tt: &TypeTable<RateNode>,
        exp_tt: &PreliminaryTypeTable<CondCategory>,
    ) -> bool {
        let start_pacing = pacing_tt[&keys.start_pacing].clone();
        let start_cond_var = &exp_tt[&keys.start_condition].variant;
        let filter = &exp_tt[&keys.eval_condition].variant;
        let end = &exp_tt[&keys.end_condition].variant;

        let pos_top = CondCategory::pos_top();
        let neg_top = CondCategory::neg_top();
        start_pacing != StreamPacingKind::Unconditional
            || start_cond_var != &pos_top
            || filter != &pos_top
            || end != &neg_top
    }

    fn get_or_fresh_targets(
        source_ir: &OORVIr1,
        expr: &Expression,
    ) -> Vec<(bool, SourceSpan, StreamIdx)> {
        match &expr.kind {
            ExprVariant::LoadConstant(_) => vec![],
            ExprVariant::ArithLog(_, children) => children
                .iter()
                .flat_map(|e| Self::get_or_fresh_targets(source_ir, e))
                .collect(),
            ExprVariant::StreamAccess(target, kind, arguments) => {
                let mut res: Vec<_> = arguments
                    .iter()
                    .flat_map(|e| Self::get_or_fresh_targets(source_ir, e))
                    .collect();
                match kind {
                    AccessMode::Get => res.push((true, expr.span, *target)),
                    AccessMode::Fresh => res.push((false, expr.span, *target)),
                    _ => {}
                };
                res
            }
            ExprVariant::ParameterAccess(_, _) => vec![],
            ExprVariant::Ite {
                condition,
                consequence,
                alternative,
            } => {
                let mut cond = Self::get_or_fresh_targets(source_ir, condition);

                cond.append(&mut Self::get_or_fresh_targets(source_ir, consequence));
                cond.append(&mut Self::get_or_fresh_targets(source_ir, alternative));

                cond
            }
            ExprVariant::Tuple(children) => children
                .iter()
                .flat_map(|e| Self::get_or_fresh_targets(source_ir, e))
                .collect(),
            ExprVariant::TupleAccess(target, _) => Self::get_or_fresh_targets(source_ir, target),
            ExprVariant::Function(def) => def
                .args
                .iter()
                .flat_map(|e| Self::get_or_fresh_targets(source_ir, e))
                .collect(),
            ExprVariant::Widen(def) => Self::get_or_fresh_targets(source_ir, def.expr.as_ref()),
            ExprVariant::Default { expr, default } => {
                let mut expr = Self::get_or_fresh_targets(source_ir, expr);

                expr.append(&mut Self::get_or_fresh_targets(source_ir, default));

                expr
            }
            ExprVariant::Quantified(_, _, _, inner) => {
                //forall z in [z_1, z_2]: z >= 2.1
                //forall a in [z_1, z_2], b in [z_1, z_2]: a+b >= 2.1
                // Also need to check binds content, since z_1, z_2 correspond to internal input/output streams
                Self::get_or_fresh_targets(source_ir, inner)
            }
            ExprVariant::QuantifiedVar(_) => vec![],
            _ => {
                unreachable!("tips: all expression kinds should be covered")
            }
        }
    }

    fn check_get_and_fresh_access(
        source_ir: &OORVIr1,
        pacing_tt: &TypeTable<RateNode>,
        nid_key: &HashMap<NodeRef, PacingKeySet>,
        expr: Option<ExprNodeIdx>,
        condition: Option<ExprNodeIdx>,
        own_pacing: &StreamPacingKind,
    ) -> Vec<CheckFailure<PacingFault>> {
        expr.map(|e| Self::get_or_fresh_targets(source_ir, source_ir.expression(e)))
            .unwrap_or_default()
            .iter()
            .chain(
                condition
                    .map(|e| Self::get_or_fresh_targets(source_ir, source_ir.expression(e)))
                    .unwrap_or_default()
                    .iter(),
            )
            .flat_map(|(is_get, span, target)| {
                let other_pacing: &StreamPacingKind =
                    &pacing_tt[&nid_key[&NodeRef::StreamIdx(*target)].eval_pacing];
                if (own_pacing.is_clock_rate() != other_pacing.is_clock_rate())
                    || (own_pacing.is_conditional() != other_pacing.is_conditional())
                {
                    let target_span = source_ir
                        .output(*target)
                        .map(|o| o.span)
                        .or_else(|| source_ir.input(*target).map(|i| i.span))
                        .expect("Ref to be either input or output");
                    Some(
                        PacingFault::GetFreshPacingConflict {
                            is_get: *is_get,
                            target: target_span,
                            target_type: other_pacing.clone(),
                            source: *span,
                            source_type: own_pacing.clone(),
                        }
                        .into(),
                    )
                } else {
                    None
                }
            })
            .collect()
    }

    fn post_process(
        source_ir: &OORVIr1,
        nid_key: &HashMap<NodeRef, PacingKeySet>,
        pacing_tt: &TypeTable<RateNode>,
        exp_tt: &PreliminaryTypeTable<CondCategory>,
    ) -> Vec<CheckFailure<PacingFault>> {
        let mut errors = vec![];

        // Check that every periodic stream has a frequency
        let streams: Vec<(StreamIdx, SourceSpan)> =
            source_ir.constraints().map(|o| (o.si, o.span)).collect();
        for (sref, span) in streams {
            let ct = &pacing_tt[&nid_key[&NodeRef::StreamIdx(sref)].eval_pacing];
            match ct {
                StreamPacingKind::UnknownClock => {
                    errors.push(PacingFault::RateAnnotationMissing(span).into());
                }
                StreamPacingKind::Unconditional => {
                    errors.push(PacingFault::DeadActivationCondition(span).into());
                }
                _ => {}
            }
        }

        //Check that start target/condition, filter, end is not again parameterized
        for output in source_ir.constraints() {
            let output_keys = nid_key[&NodeRef::StreamIdx(output.si)];
            let output_start_pacing = pacing_tt[&output_keys.start_pacing].clone();
            let output_start_cond = &exp_tt[&output_keys.start_condition].variant;
            let output_filter = &exp_tt[&output_keys.eval_condition].variant;
            let output_end = &exp_tt[&output_keys.end_condition].variant;

            if let Some(start) = output.start() {
                //Start target
                if let Some(target) = start.expression {
                    let keys = nid_key[&NodeRef::Expr(target)];
                    if Self::is_parameterized(keys, pacing_tt, exp_tt) {
                        errors.push(
                            PacingFault::TemplateParamForbidden(source_ir.expression(target).span)
                                .into(),
                        );
                    }
                }
                //Start condition
                if let Some(condition) = start.condition {
                    let keys = nid_key[&NodeRef::Expr(condition)];
                    if Self::is_parameterized(keys, pacing_tt, exp_tt) {
                        errors.push(
                            PacingFault::TemplateParamForbidden(
                                source_ir.expression(condition).span,
                            )
                            .into(),
                        );
                    }
                }
            }

            //End expression must either be non parameterized or have exactly same start / filter as stream and no filter
            if let Some(cond) = output.end_cond() {
                let keys = nid_key[&NodeRef::Expr(cond)];
                if Self::is_parameterized(keys, pacing_tt, exp_tt)
                    && (pacing_tt[&keys.start_pacing] != output_start_pacing
                        || &exp_tt[&keys.start_condition].variant != output_start_cond
                        || &exp_tt[&keys.eval_condition].variant != output_filter
                        || &exp_tt[&keys.end_condition].variant != output_end)
                {
                    errors.push(
                        PacingFault::TemplateParamForbidden(source_ir.expression(cond).span).into(),
                    );
                }
            }
        }

        //Check that start, end pacing is not constant / periodic
        for output in source_ir.constraints() {
            let keys = nid_key[&NodeRef::StreamIdx(output.si)];
            let start_pacing = pacing_tt[&keys.start_pacing].clone();
            if let Some(start) = output.start() {
                if matches!(
                    start_pacing,
                    StreamPacingKind::Unconditional | StreamPacingKind::UnknownClock
                ) {
                    let span = Some(start.pacing)
                        .and_then(|pt| match pt {
                            PacingNode::GlobalTick(f) | PacingNode::LocalTick(f) => Some(f.span),
                            PacingNode::Event(id) => Some(source_ir.expression(id).span),
                            PacingNode::Unspecified(_) => None,
                        })
                        .or_else(|| start.expression.map(|id| source_ir.expression(id).span))
                        .or_else(|| start.condition.map(|id| source_ir.expression(id).span))
                        .unwrap_or(output.span);
                    errors.push(
                        PacingFault::Other(
                            span,
                            "No instance is created as start pacing is 'Constant'".into(),
                            vec![],
                        )
                        .into(),
                    )
                }
            }
            if let Some(cond) = output.end_cond() {
                let end_pacing = pacing_tt[&keys.end_pacing].clone();
                let span = source_ir.expression(cond).span;
                if matches!(end_pacing, StreamPacingKind::UnknownClock) {
                    errors.push(PacingFault::RateAnnotationMissing(span).into())
                } else if end_pacing == StreamPacingKind::Unconditional {
                    errors.push(PacingFault::DeadActivationCondition(span).into())
                }
            }
        }
        //Check that stream without start template does not access parameterized stream
        //Check that stream without filter does not access filtered stream
        //Check that stream without end does not access ended stream
        let pos_top = CondCategory::pos_top();
        let neg_top = CondCategory::neg_top();
        for (output, (eval, node_id, key_span)) in source_ir.constraints().flat_map(|output| {
            output.eval.iter().flat_map(move |eval| {
                vec![
                    eval.condition
                        .map(|c| (eval, NodeRef::Expr(c), source_ir.expression(c).span)),
                    Some((
                        eval,
                        NodeRef::Expr(eval.expression),
                        source_ir.expression(eval.expression).span,
                    )),
                ]
                .into_iter()
                .flatten()
                .map(move |id| (output, id))
            })
        }) {
            let keys = nid_key[&node_id];
            let start_pacing = pacing_tt[&keys.start_pacing].clone();
            let start_cond_var = &exp_tt[&keys.start_condition].variant;
            let filter_type = &exp_tt[&keys.eval_condition].variant;
            let end_type = &exp_tt[&keys.end_condition].variant;

            let start_pacing = (output.start().is_none()
                && start_pacing != StreamPacingKind::Unconditional)
                .then_some(start_pacing);
            let start_cond_var = (output.start_cond().is_none() && start_cond_var != &pos_top)
                .then(|| {
                    start_cond_var
                        .construct(&[])
                        .expect("variant to not be any")
                });
            let filter = (eval.condition.is_none() && filter_type != &pos_top)
                .then(|| filter_type.construct(&[]).expect("variant to not be any"));
            let end = (output.end().is_none() && end_type != &neg_top)
                .then(|| end_type.construct(&[]).expect("variant to not be any"));

            if start_pacing.is_some()
                || start_cond_var.is_some()
                || filter.is_some()
                || end.is_some()
            {
                errors.push(
                    PacingFault::TemplateParamRequired {
                        who: output.span,
                        why: key_span,
                        inferred: Box::new(TemplateDetails {
                            start_pacing,
                            start_cond: start_cond_var,
                            filter,
                            end,
                        }),
                    }
                    .into(),
                )
            }
        }

        //Warning unintuitive start type
        for output in source_ir.constraints() {
            if let Some(start) = output.start() {
                if let Some(target_id) = start.expression {
                    let target_type =
                        pacing_tt[&nid_key[&NodeRef::Expr(target_id)].eval_pacing].clone();
                    let start_pacing =
                        pacing_tt[&nid_key[&NodeRef::StreamIdx(output.si)].start_pacing].clone();
                    if !matches!(start.pacing, PacingNode::Unspecified(_))
                        && target_type != start_pacing
                    {
                        errors.push(
                            PacingFault::AmbiguousPacingInference(
                                source_ir.expression(target_id).span,
                                start_pacing,
                            )
                            .into(),
                        );
                    }
                }
            }
        }

        //Warning unintuitive exp pacing
        for (output, eval) in source_ir
            .constraints()
            .flat_map(|output| output.eval.iter().map(move |eval| (output, eval)))
        {
            let exp_pacing =
                pacing_tt[&nid_key[&NodeRef::Expr(eval.expression)].eval_pacing].clone();
            let stream_pacing =
                pacing_tt[&nid_key[&NodeRef::StreamIdx(output.si)].eval_pacing].clone();
            if !matches!(eval.pacing, PacingNode::Unspecified(_)) && exp_pacing != stream_pacing {
                errors
                    .push(PacingFault::AmbiguousPacingInference(output.span, stream_pacing).into());
            }
        }

        let eval_is_local_periodic = |node_id: &NodeRef| -> bool {
            matches!(
                pacing_tt[&nid_key[node_id].eval_pacing],
                StreamPacingKind::LocalClock(_)
            )
        };

        let end_is_local_periodic = |node_id: &NodeRef| -> bool {
            matches!(
                pacing_tt[&nid_key[node_id].end_pacing],
                StreamPacingKind::LocalClock(_)
            )
        };

        //Check that no periodic expressions with a start access static periodic stream
        let nodes_to_check: Vec<(StreamIdx, ExprNodeIdx)> = source_ir
            .constraints
            .iter()
            .flat_map(|o| {
                o.eval()
                    .iter()
                    .enumerate()
                    .filter(move |(eval_i, _)| {
                        eval_is_local_periodic(&NodeRef::Eval(*eval_i, o.si))
                    })
                    .flat_map(move |(_, eval)| {
                        vec![
                            eval.condition.map(|c| (o.si, c)),
                            Some((o.si, eval.expression)),
                        ]
                    })
                    .chain(
                        o.end
                            .as_ref()
                            .filter(|_| end_is_local_periodic(&NodeRef::StreamIdx(o.si)))
                            .map(|c| Some((o.si, c.condition))),
                    )
            })
            .flatten()
            .collect();

        for (o, expr_id) in nodes_to_check {
            let stream_key = nid_key[&NodeRef::StreamIdx(o)];
            let stream_end_pacing = &pacing_tt[&stream_key.end_pacing];
            let stream_end_cond = &exp_tt[&stream_key.end_condition].variant;
            let stream_start_pacing = &pacing_tt[&stream_key.start_pacing];
            let stream_start_cond = &exp_tt[&stream_key.start_condition].variant;

            let expr = source_ir.expression(expr_id);
            let accesses_streams: Vec<StreamIdx> = expr.get_sync_accesses();
            for target in accesses_streams {
                let target_key = nid_key[&NodeRef::StreamIdx(target)];
                let target_end_pacing = &pacing_tt[&target_key.end_pacing];
                let target_end_cond = &exp_tt[&target_key.end_condition].variant;
                let target_start_pacing = &pacing_tt[&target_key.start_pacing];
                let target_start_cond = &exp_tt[&target_key.start_condition].variant;

                if target_end_pacing != stream_end_pacing || target_end_cond != stream_end_cond {
                    errors.push(
                        PacingFault::EndAnnotationConflict {
                            access_span: expr.span,
                            target_end_span: source_ir.end(target).map(|c| c.span),
                            source_end_span: source_ir.end(o).map(|c| c.span),
                            target_end_pacing: target_end_pacing.clone(),
                            target_end_condition: target_end_cond.construct(&[]).ok(),
                            source_end_pacing: stream_end_pacing.clone(),
                            source_end_condition: stream_end_cond.construct(&[]).ok(),
                        }
                        .into(),
                    );
                }
                if target_start_pacing != stream_start_pacing
                    || target_start_cond != stream_start_cond
                {
                    errors.push(
                        PacingFault::StartAnnotationConflict {
                            access_span: expr.span,
                            target_start_span: source_ir.start(target).map(|c| c.span),
                            source_start_span: source_ir.start(o).map(|c| c.span),
                            target_start_pacing: target_start_pacing.clone(),
                            target_start_condition: target_start_cond.construct(&[]).ok(),
                            source_start_pacing: stream_start_pacing.clone(),
                            source_start_condition: stream_start_cond.construct(&[]).ok(),
                        }
                        .into(),
                    );
                }
            }
        }

        // Check that a get / fresh access only occurs between periodic or event-based streams
        for output in &source_ir.constraints {
            let keys = &nid_key[&NodeRef::StreamIdx(output.si)];
            // Start clause
            if let Some(start) = &output.start {
                let own_pacing: &StreamPacingKind = &pacing_tt[&keys.start_pacing];
                errors.append(&mut Self::check_get_and_fresh_access(
                    source_ir,
                    pacing_tt,
                    nid_key,
                    start.expression,
                    start.condition,
                    own_pacing,
                ))
            }

            // Eval clause
            let own_pacing: &StreamPacingKind = &pacing_tt[&keys.eval_pacing];
            for eval in output.eval() {
                errors.append(&mut Self::check_get_and_fresh_access(
                    source_ir,
                    pacing_tt,
                    nid_key,
                    Some(eval.expression),
                    eval.condition,
                    own_pacing,
                ));
            }

            // End clause
            if let Some(end) = &output.end {
                let own_pacing: &StreamPacingKind = &pacing_tt[&keys.end_pacing];
                errors.append(&mut Self::check_get_and_fresh_access(
                    source_ir,
                    pacing_tt,
                    nid_key,
                    None,
                    Some(end.condition),
                    own_pacing,
                ));
            }
        }

        errors
    }

    /// The callable function to start the inference. Used by [OorvTypeChecker].
    pub(crate) fn run(mut self) -> Result<HashMap<NodeRef, StreamPacingBundle>, OORVError> {
        for input in self.spec.signals() {
            self.input_infer(input)
                .map_err(|e| e.into_diagnostic(&[&self.rate_span, &self.cond_span], self.names))?;
        }

        for output in self.spec.constraints() {
            self.output_infer(output)
                .map_err(|e| e.into_diagnostic(&[&self.rate_span, &self.cond_span], self.names))?;
        }

        let PacingAnalyzer {
            spec: source_ir,
            rate_checker,
            cond_checker,
            key_map,
            rate_span,
            cond_span,
            names,
            rate_checks,
            cond_checks,
            expr_ctx: _,
        } = self;

        let pacing_tt = rate_checker.type_check().map_err(|tc_err| {
            CheckFailure::from(tc_err).into_diagnostic(&[&rate_span, &cond_span], names)
        })?;
        let preliminary_exp_tt =
            cond_checker
                .clone()
                .type_check_preliminary()
                .map_err(|tc_err| {
                    CheckFailure::from(tc_err).into_diagnostic(&[&rate_span, &cond_span], names)
                })?;

        let mut error = OORVError::new();
        for pe in
            Self::check_explicit_bounds(rate_checks, cond_checks, &pacing_tt, &preliminary_exp_tt)
        {
            error.add(pe.into_diagnostic(&[&rate_span, &cond_span], names));
        }
        for pe in Self::post_process(source_ir, &key_map, &pacing_tt, &preliminary_exp_tt) {
            error.add(pe.into_diagnostic(&[&rate_span, &cond_span], names));
        }
        Result::from(error)?;

        let exp_tt = cond_checker.type_check().map_err(|tc_err| {
            CheckFailure::from(tc_err).into_diagnostic(&[&rate_span, &cond_span], names)
        })?;

        // Assemble each node's resolved pacing bundle from the finalized type tables.
        let ctt: HashMap<NodeRef, StreamPacingBundle> = key_map
            .iter()
            .map(|(node_ref, key)| {
                let exec_rate = pacing_tt[&key.eval_pacing].clone();
                let init_rate = pacing_tt[&key.start_pacing].clone();
                let init_cond_expr = exp_tt[&key.start_condition].clone();
                let exec_guard = exp_tt[&key.eval_condition].clone();
                let term_rate = pacing_tt[&key.end_pacing].clone();
                let term_guard = exp_tt[&key.end_condition].clone();

                (
                    *node_ref,
                    StreamPacingBundle {
                        execution_rate: exec_rate,
                        execution_guard: exec_guard,
                        init_rate,
                        init_guard: init_cond_expr,
                        termination_rate: term_rate,
                        termination_guard: term_guard,
                    },
                )
            })
            .collect();

        Ok(ctt)
    }
}
