use std::collections::{HashMap, HashSet};
use std::ops::{Add, BitAnd, BitOr, BitXor, Div, Mul, Neg, Not, Rem, Shl, Shr, Sub};
use std::rc::Rc;

use crate::oorvir::refined::{AccessMode, Constant, ExprVariant, Expression, Shift, Type};
use num::{FromPrimitive, ToPrimitive};
use ordered_float::NotNan;
use regex::bytes::Regex as BytesRegex;
use regex::Regex;
use rust_decimal::{Decimal, MathematicalOps};
use string_template::Template;

use crate::oorvir::refined::{FixedTy, FloatTy, IntTy, UIntTy};
use crate::oorvir::source::StreamIdx;
use crate::runtime::eval::{EvalFrame, PacingGate};
use crate::runtime::store::Value;

/// Convert a runtime `Value` to a refined_ir `Constant` for expression rewriting.
fn runtime_val_to_const(v: &Value) -> Constant {
    match v {
        Value::Bool(b) => Constant::Bool(*b),
        Value::Unsigned(u) => Constant::UInt(*u),
        Value::Signed(i) => Constant::Int(*i),
        Value::Float(f) => Constant::Float(f.into_inner()),
        Value::Str(s) => Constant::Str(s.to_string()),
        Value::Decimal(d) => Constant::Decimal(d.clone()),
        Value::Bytes(b) => Constant::Str(String::from_utf8_lossy(b).into_owned()),
        Value::Tuple(_) | Value::None => {
            unreachable!("unsupported parameter value in constant conversion")
        }
    }
}

/// Infer a refined_ir `Type` from a runtime `Value` (approximation).
fn runtime_val_to_type(v: &Value) -> Type {
    match v {
        Value::Bool(_) => Type::Bool,
        Value::Unsigned(_) => Type::UInt(UIntTy::UInt64),
        Value::Signed(_) => Type::Int(IntTy::Int64),
        Value::Float(_) => Type::Float(FloatTy::Float64),
        Value::Decimal(_) => Type::Fixed(FixedTy::Fixed64_32),
        Value::Str(_) => Type::String,
        Value::Bytes(_) => Type::Bytes,
        Value::Tuple(_) | Value::None => {
            unreachable!("unsupported parameter value in type inference")
        }
    }
}

fn runtime_val_to_expr(v: &Value) -> Expression {
    Expression {
        kind: ExprVariant::LoadConstant(runtime_val_to_const(v)),
        annotation: runtime_val_to_type(v),
    }
}

fn substitute_quantified_bindings(
    e: &Expression,
    mapping: &HashMap<String, Vec<Value>>,
) -> Expression {
    use ExprVariant::*;
    match &e.kind {
        LoadConstant(c) => Expression {
            kind: LoadConstant(c.clone()),
            ..e.clone()
        },
        ArithLog(op, operands) => Expression {
            kind: ArithLog(
                *op,
                operands
                    .iter()
                    .map(|o| substitute_quantified_bindings(o, mapping))
                    .collect(),
            ),
            annotation: e.annotation.clone(),
        },
        StreamAccess {
            target,
            parameters,
            access_kind,
        } => Expression {
            kind: StreamAccess {
                target: *target,
                parameters: parameters
                    .iter()
                    .map(|p| substitute_quantified_bindings(p, mapping))
                    .collect(),
                access_kind: *access_kind,
            },
            annotation: e.annotation.clone(),
        },
        ParameterAccess(sr, i) => Expression {
            kind: ParameterAccess(*sr, *i),
            annotation: e.annotation.clone(),
        },
        FunctionParameterAccess(i) => Expression {
            kind: FunctionParameterAccess(*i),
            annotation: e.annotation.clone(),
        },
        Quantified {
            quantifier,
            bindings1,
            bindings2,
            body,
        } => Expression {
            kind: Quantified {
                quantifier: quantifier.clone(),
                bindings1: bindings1.clone(),
                bindings2: bindings2.clone(),
                body: Box::new(substitute_quantified_bindings(body, mapping)),
            },
            annotation: e.annotation.clone(),
        },
        QuantifiedVar(s) => mapping
            .get(s)
            .and_then(|vals| vals.first())
            .map(runtime_val_to_expr)
            .unwrap_or_else(|| Expression {
                kind: QuantifiedVar(s.clone()),
                annotation: e.annotation.clone(),
            }),
        Ite {
            condition,
            consequence,
            alternative,
            ..
        } => Expression {
            kind: Ite {
                condition: Box::new(substitute_quantified_bindings(condition, mapping)),
                consequence: Box::new(substitute_quantified_bindings(consequence, mapping)),
                alternative: Box::new(substitute_quantified_bindings(alternative, mapping)),
            },
            annotation: e.annotation.clone(),
        },
        Tuple(entries) => Expression {
            kind: Tuple(
                entries
                    .iter()
                    .map(|en| substitute_quantified_bindings(en, mapping))
                    .collect(),
            ),
            annotation: e.annotation.clone(),
        },
        Function(name, args) => Expression {
            kind: Function(
                name.clone(),
                args.iter()
                    .map(|a| substitute_quantified_bindings(a, mapping))
                    .collect(),
            ),
            annotation: e.annotation.clone(),
        },
        Convert { expr } => Expression {
            kind: Convert {
                expr: Box::new(substitute_quantified_bindings(expr, mapping)),
            },
            annotation: e.annotation.clone(),
        },
        Default { expr, default, .. } => Expression {
            kind: Default {
                expr: Box::new(substitute_quantified_bindings(expr, mapping)),
                default: Box::new(substitute_quantified_bindings(default, mapping)),
            },
            annotation: e.annotation.clone(),
        },
        TupleAccess(ex, n) => Expression {
            kind: TupleAccess(Box::new(substitute_quantified_bindings(ex, mapping)), *n),
            annotation: e.annotation.clone(),
        },
    }
}

fn substitute_quantified_value(
    e: &Expression,
    mapping: &HashMap<String, Vec<Value>>,
) -> Expression {
    use ExprVariant::*;
    match &e.kind {
        Function(name, args) if name.as_str() == "format" => {
            let mut new_args: Vec<Expression> = Vec::new();
            if !args.is_empty() {
                new_args.push(args[0].clone());
                for a in args.iter().skip(1) {
                    new_args.push(substitute_quantified_value(a, mapping));
                }
            }
            Expression {
                kind: Function(name.clone(), new_args),
                annotation: e.annotation.clone(),
            }
        }
        StreamAccess { .. } => substitute_quantified_bindings(e, mapping),
        LoadConstant(c) => Expression {
            kind: LoadConstant(c.clone()),
            ..e.clone()
        },
        ArithLog(op, ops) => Expression {
            kind: ArithLog(
                *op,
                ops.iter()
                    .map(|o| substitute_quantified_value(o, mapping))
                    .collect(),
            ),
            annotation: e.annotation.clone(),
        },
        ParameterAccess(sr, i) => Expression {
            kind: ParameterAccess(*sr, *i),
            annotation: e.annotation.clone(),
        },
        FunctionParameterAccess(i) => Expression {
            kind: FunctionParameterAccess(*i),
            annotation: e.annotation.clone(),
        },
        Quantified {
            quantifier,
            bindings1,
            bindings2,
            body,
        } => Expression {
            kind: Quantified {
                quantifier: quantifier.clone(),
                bindings1: bindings1.clone(),
                bindings2: bindings2.clone(),
                body: Box::new(substitute_quantified_value(body, mapping)),
            },
            annotation: e.annotation.clone(),
        },
        QuantifiedVar(s) => mapping
            .get(s)
            .and_then(|vals| vals.first())
            .map(runtime_val_to_expr)
            .unwrap_or_else(|| Expression {
                kind: QuantifiedVar(s.clone()),
                annotation: e.annotation.clone(),
            }),
        Ite {
            condition,
            consequence,
            alternative,
            ..
        } => Expression {
            kind: Ite {
                condition: Box::new(substitute_quantified_value(condition, mapping)),
                consequence: Box::new(substitute_quantified_value(consequence, mapping)),
                alternative: Box::new(substitute_quantified_value(alternative, mapping)),
            },
            annotation: e.annotation.clone(),
        },
        Tuple(entries) => Expression {
            kind: Tuple(
                entries
                    .iter()
                    .map(|en| substitute_quantified_value(en, mapping))
                    .collect(),
            ),
            annotation: e.annotation.clone(),
        },
        Function(name, args) => Expression {
            kind: Function(
                name.clone(),
                args.iter()
                    .map(|a| substitute_quantified_value(a, mapping))
                    .collect(),
            ),
            annotation: e.annotation.clone(),
        },
        Convert { expr } => Expression {
            kind: Convert {
                expr: Box::new(substitute_quantified_value(expr, mapping)),
            },
            annotation: e.annotation.clone(),
        },
        Default { expr, default, .. } => Expression {
            kind: Default {
                expr: Box::new(substitute_quantified_value(expr, mapping)),
                default: Box::new(substitute_quantified_value(default, mapping)),
            },
            annotation: e.annotation.clone(),
        },
        TupleAccess(ex, n) => Expression {
            kind: TupleAccess(Box::new(substitute_quantified_value(ex, mapping)), *n),
            annotation: e.annotation.clone(),
        },
    }
}

fn collect_quantified_vars(
    e: &Expression,
    bound_names: &HashSet<String>,
    out: &mut HashSet<String>,
) {
    use ExprVariant::*;
    match &e.kind {
        LoadConstant(_) | ParameterAccess(_, _) | FunctionParameterAccess(_) => {}
        QuantifiedVar(name) => {
            if bound_names.contains(name) {
                out.insert(name.clone());
            }
        }
        ArithLog(_, ops) => {
            for op in ops {
                collect_quantified_vars(op, bound_names, out);
            }
        }
        StreamAccess { parameters, .. } => {
            for p in parameters {
                collect_quantified_vars(p, bound_names, out);
            }
        }
        Quantified {
            bindings1, body, ..
        } => {
            let mut scoped = bound_names.clone();
            for binding in bindings1 {
                scoped.remove(binding);
            }
            collect_quantified_vars(body, &scoped, out);
        }
        Ite {
            condition,
            consequence,
            alternative,
            ..
        } => {
            collect_quantified_vars(condition, bound_names, out);
            collect_quantified_vars(consequence, bound_names, out);
            collect_quantified_vars(alternative, bound_names, out);
        }
        Tuple(es) => {
            for e in es {
                collect_quantified_vars(e, bound_names, out);
            }
        }
        Function(_, args) => {
            for a in args {
                collect_quantified_vars(a, bound_names, out);
            }
        }
        Convert { expr } => collect_quantified_vars(expr, bound_names, out),
        Default { expr, default, .. } => {
            collect_quantified_vars(expr, bound_names, out);
            collect_quantified_vars(default, bound_names, out);
        }
        TupleAccess(expr, _) => collect_quantified_vars(expr, bound_names, out),
    }
}

fn collect_quantified_accesses(
    e: &Expression,
    bound_names: &HashSet<String>,
    out: &mut Vec<(StreamIdx, String)>,
) {
    use ExprVariant::*;
    match &e.kind {
        LoadConstant(_) | ParameterAccess(_, _) | FunctionParameterAccess(_) | QuantifiedVar(_) => {
        }
        ArithLog(_, ops) => {
            for op in ops {
                collect_quantified_accesses(op, bound_names, out);
            }
        }
        StreamAccess {
            target, parameters, ..
        } => {
            let mut names = HashSet::new();
            for parameter in parameters {
                collect_quantified_vars(parameter, bound_names, &mut names);
                collect_quantified_accesses(parameter, bound_names, out);
            }
            for name in names {
                out.push((*target, name));
            }
        }
        Quantified {
            bindings1, body, ..
        } => {
            let mut scoped = bound_names.clone();
            for binding in bindings1 {
                scoped.remove(binding);
            }
            collect_quantified_accesses(body, &scoped, out);
        }
        Ite {
            condition,
            consequence,
            alternative,
            ..
        } => {
            collect_quantified_accesses(condition, bound_names, out);
            collect_quantified_accesses(consequence, bound_names, out);
            collect_quantified_accesses(alternative, bound_names, out);
        }
        Tuple(es) => {
            for e in es {
                collect_quantified_accesses(e, bound_names, out);
            }
        }
        Function(_, args) => {
            for a in args {
                collect_quantified_accesses(a, bound_names, out);
            }
        }
        Convert { expr } => collect_quantified_accesses(expr, bound_names, out),
        Default { expr, default, .. } => {
            collect_quantified_accesses(expr, bound_names, out);
            collect_quantified_accesses(default, bound_names, out);
        }
        TupleAccess(expr, _) => collect_quantified_accesses(expr, bound_names, out),
    }
}

fn unique_quantified_bindings(bindings1: &[String], bindings2: &[String]) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    bindings1
        .iter()
        .zip(bindings2.iter())
        .filter_map(|(binding, domain)| {
            seen.insert(binding.clone())
                .then(|| (binding.clone(), domain.clone()))
        })
        .collect()
}

fn binding_targets(
    body: &Expression,
    value_ast: Option<&Expression>,
    unique_bindings: &[(String, String)],
) -> Vec<(String, Option<StreamIdx>)> {
    let bound_names: HashSet<String> = unique_bindings
        .iter()
        .map(|(binding, _)| binding.clone())
        .collect();
    let mut accesses = Vec::new();
    collect_quantified_accesses(body, &bound_names, &mut accesses);
    if let Some(value_ast) = value_ast {
        collect_quantified_accesses(value_ast, &bound_names, &mut accesses);
    }

    let mut by_binding: HashMap<String, StreamIdx> = HashMap::new();
    for (target, binding) in accesses {
        by_binding.entry(binding).or_insert(target);
    }

    let mut by_domain: HashMap<String, StreamIdx> = HashMap::new();
    for (binding, domain) in unique_bindings {
        if let Some(target) = by_binding.get(binding) {
            by_domain.entry(domain.clone()).or_insert(*target);
        }
    }

    unique_bindings
        .iter()
        .map(|(binding, domain)| {
            (
                domain.clone(),
                by_binding
                    .get(binding)
                    .copied()
                    .or_else(|| by_domain.get(domain).copied()),
            )
        })
        .collect()
}

fn params_for_binding_target(
    ctx: &EvalFrame<'_>,
    domain: &str,
    target: Option<StreamIdx>,
) -> Vec<Vec<Value>> {
    let Some(target) = target else {
        return ctx.fetch_domain_params(domain);
    };
    let params = ctx.fetch_instance_params(target);
    if params.is_empty() && !ctx.stream_is_parameterized(target) {
        vec![Vec::new()]
    } else {
        params
    }
}

fn enumerate_binding_combos(
    depth: usize,
    unique_bindings: &[(String, String)],
    params_per_binding: &[Vec<Vec<Value>>],
    current: &mut HashMap<String, Vec<Value>>,
    callback: &mut impl FnMut(&HashMap<String, Vec<Value>>),
) {
    if depth == unique_bindings.len() {
        callback(current);
        return;
    }

    let binding = &unique_bindings[depth].0;
    for params in params_per_binding[depth].iter() {
        current.insert(binding.clone(), params.clone());
        enumerate_binding_combos(
            depth + 1,
            unique_bindings,
            params_per_binding,
            current,
            callback,
        );
        current.remove(binding);
    }
}

fn deduplicate_results(results: Vec<Value>) -> Value {
    use std::collections::HashSet;
    let re = Regex::new(r"\d+").unwrap();
    let mut seen: HashSet<String> = HashSet::new();
    let mut uniq: Vec<Value> = Vec::new();
    for v in results {
        let key = match &v {
            Value::Str(s) => {
                let text = s.as_ref();
                let mut nums: Vec<String> =
                    re.find_iter(text).map(|m| m.as_str().to_string()).collect();
                if !nums.is_empty() {
                    nums.sort_unstable_by_key(|n| n.parse::<u64>().unwrap_or(0));
                    format!("S:N:{}", nums.join(","))
                } else {
                    format!("S:T:{}", text)
                }
            }
            _ => format!("V:{:?}", v),
        };
        if seen.insert(key) {
            uniq.push(v);
        }
    }
    if uniq.len() == 1 {
        uniq.into_iter().next().unwrap()
    } else {
        Value::Tuple(uniq.into_boxed_slice())
    }
}

/// Trait for refined_ir expression nodes that can be lowered into a [`BoundExpr`].
pub(crate) trait Compilable {
    fn lower(self) -> BoundExpr;
}

/// A compiled, reference-counted expression closure.
///
/// Evaluating it requires an [`EvalFrame`] that supplies the current runtime
/// context (stream values, freshness sets, instance parameters, …).
#[derive(Clone)]
pub(crate) struct BoundExpr(Rc<dyn Fn(&EvalFrame<'_>) -> Value>);

impl BoundExpr {
    /// Wrap a raw closure into a [`BoundExpr`].
    pub(crate) fn wrap(f: impl 'static + Fn(&EvalFrame<'_>) -> Value) -> Self {
        BoundExpr(Rc::new(f))
    }

    /// Return `value_expr`'s result when `guard_expr` is true; otherwise `Value::None`.
    pub(crate) fn guarded(guard_expr: BoundExpr, value_expr: BoundExpr) -> Self {
        BoundExpr::wrap(move |ctx| {
            if guard_expr.run(ctx).boolean_value() {
                value_expr.run(ctx)
            } else {
                Value::None
            }
        })
    }

    /// Guarded filter for the case where the guard is a `Quantified` expression.
    pub(crate) fn guarded_quantified(guard_ast: Expression, value_ast: Expression) -> Self {
        use crate::oorvir::refined::ExprVariant::*;

        if let Quantified {
            quantifier,
            bindings1,
            bindings2,
            body,
        } = guard_ast.kind
        {
            let unique_bindings = unique_quantified_bindings(&bindings1, &bindings2);
            let binding_targets = binding_targets(&body, Some(&value_ast), &unique_bindings);

            BoundExpr::wrap(move |ctx| {
                let params_per_binding: Vec<Vec<Vec<Value>>> = binding_targets
                    .iter()
                    .map(|(domain, target)| params_for_binding_target(ctx, domain, *target))
                    .collect();

                let mut all_true = true;
                let mut eval_count: usize = 0;
                let mut first_map: Option<HashMap<String, Vec<Value>>> = None;
                let mut collected: Vec<Value> = Vec::new();

                enumerate_binding_combos(
                    0,
                    &unique_bindings,
                    &params_per_binding,
                    &mut HashMap::new(),
                    &mut |mapping| {
                        if eval_count == 0 {
                            first_map = Some(mapping.clone());
                        }
                        eval_count += 1;

                        let pred = substitute_quantified_bindings(&body, mapping)
                            .lower()
                            .run(ctx)
                            .boolean_value();
                        all_true = all_true && pred;

                        if matches!(&quantifier, crate::oorvir::refined::Quantifier::Exists) && pred
                        {
                            let val = substitute_quantified_value(&value_ast, mapping)
                                .lower()
                                .run(ctx);
                            collected.push(val);
                        }
                    },
                );

                match &quantifier {
                    crate::oorvir::refined::Quantifier::Exists => {
                        if collected.is_empty() {
                            Value::None
                        } else {
                            deduplicate_results(collected)
                        }
                    }
                    crate::oorvir::refined::Quantifier::Forall => {
                        if !all_true {
                            Value::None
                        } else {
                            let empty: HashMap<String, Vec<Value>> = HashMap::new();
                            let map = first_map.as_ref().unwrap_or(&empty);
                            substitute_quantified_value(&value_ast, map)
                                .lower()
                                .run(ctx)
                        }
                    }
                }
            })
        } else {
            let guard = guard_ast.lower();
            let val = value_ast.lower();
            BoundExpr::guarded(guard, val)
        }
    }

    /// Return the first clause result that is not `Value::None`.
    pub(crate) fn first_match(clauses: Vec<BoundExpr>) -> Self {
        BoundExpr::wrap(move |ctx| {
            clauses
                .iter()
                .map(|e| e.run(ctx))
                .find(|v| !matches!(v, Value::None))
                .unwrap_or(Value::None)
        })
    }

    /// Wrap `expr` so that it only evaluates when `gate` is satisfied.
    pub(crate) fn gated(expr: BoundExpr, gate: PacingGate) -> Self {
        BoundExpr::wrap(move |ctx| {
            if ctx.gate_is_open(&gate) {
                expr.run(ctx)
            } else {
                Value::None
            }
        })
    }

    /// Evaluate this expression against the given runtime context.
    pub(crate) fn run(&self, ctx: &EvalFrame) -> Value {
        self.0(ctx)
    }
}

impl Compilable for Expression {
    /// Lower a refined_ir [`Expression`] into a [`BoundExpr`] closure.
    fn lower(self) -> BoundExpr {
        use ExprVariant::*;
        match self.kind {
            LoadConstant(c) => {
                fn const_to_val(c: Constant) -> Value {
                    match c {
                        Constant::Bool(b) => Value::Bool(b),
                        Constant::UInt(u) => Value::Unsigned(u),
                        Constant::Int(i) => Value::Signed(i),
                        Constant::Float(f) => {
                            Value::Float(NotNan::new(f).expect("float constant must not be NaN"))
                        }
                        Constant::Str(s) => Value::Str(s.into_boxed_str()),
                        Constant::Decimal(d) => Value::Decimal(d),
                    }
                }
                let v = const_to_val(c);
                BoundExpr::wrap(move |_| v.clone())
            }

            ParameterAccess(_target, idx) => BoundExpr::wrap(move |ctx| ctx.parameter[idx].clone()),
            FunctionParameterAccess(idx) => BoundExpr::wrap(move |ctx| ctx.parameter[idx].clone()),

            ArithLog(op, operands) => {
                let compiled: Vec<BoundExpr> = operands.into_iter().map(|e| e.lower()).collect();

                macro_rules! unary_op {
                    ($method:ident) => {
                        BoundExpr::wrap(move |ctx| compiled[0].run(ctx).$method())
                    };
                }
                macro_rules! binary_op {
                    ($method:ident) => {
                        BoundExpr::wrap(move |ctx| {
                            let lhs = compiled[0].run(ctx);
                            let rhs = compiled[1].run(ctx);
                            lhs.$method(rhs)
                        })
                    };
                }
                macro_rules! cmp_op {
                    ($method:ident) => {
                        BoundExpr::wrap(move |ctx| {
                            let lhs = compiled[0].run(ctx);
                            let rhs = compiled[1].run(ctx);
                            Value::Bool(lhs.$method(&rhs))
                        })
                    };
                }
                macro_rules! short_circuit {
                    ($short:expr) => {
                        BoundExpr::wrap(move |ctx| {
                            let lhs = compiled[0].run(ctx).boolean_value();
                            if lhs == $short {
                                Value::Bool($short)
                            } else {
                                let rhs = compiled[1].run(ctx);
                                debug_assert!(rhs.is_boolean());
                                rhs
                            }
                        })
                    };
                }

                use crate::oorvir::refined::ArithLogOp::*;
                match op {
                    Not | BitNot => unary_op!(not),
                    Neg => unary_op!(neg),
                    Add => binary_op!(add),
                    Sub => binary_op!(sub),
                    Mul => binary_op!(mul),
                    Div => binary_op!(div),
                    Rem => binary_op!(rem),
                    Pow => binary_op!(pow),
                    Eq => cmp_op!(eq),
                    Lt => cmp_op!(lt),
                    Le => cmp_op!(le),
                    Ne => cmp_op!(ne),
                    Ge => cmp_op!(ge),
                    Gt => cmp_op!(gt),
                    And => short_circuit!(false),
                    Or => short_circuit!(true),
                    BitAnd => binary_op!(bitand),
                    BitOr => binary_op!(bitor),
                    BitXor => binary_op!(bitxor),
                    Shl => binary_op!(shl),
                    Shr => binary_op!(shr),
                }
            }

            StreamAccess {
                target,
                parameters,
                access_kind,
            } => {
                let param_exprs: Vec<BoundExpr> =
                    parameters.into_iter().map(|e| e.lower()).collect();

                macro_rules! stream_lookup {
                    ($method:ident, $tgt:ident $(, $extra:ident)*) => {
                        BoundExpr::wrap(move |ctx| {
                            let params: Vec<Value> = param_exprs.iter().map(|p| p.run(ctx)).collect();
                            ctx.$method($tgt, params.as_slice() $(, $extra)*)
                        })
                    };
                }

                match access_kind {
                    AccessMode::Strict => stream_lookup!(read_held_value_strict, target),
                    AccessMode::Cached => stream_lookup!(read_held_value, target),
                    AccessMode::Shift(shift) => {
                        let offset: i16 = match shift {
                            Shift::Future(_) => unimplemented!("future shifts not supported"),
                            Shift::Past(u) => -(u as i16),
                        };
                        stream_lookup!(read_at_offset, target, offset)
                    }
                    AccessMode::Get => stream_lookup!(read_current_value, target),
                    AccessMode::Fresh => stream_lookup!(read_freshness, target),
                }
            }

            Quantified {
                quantifier,
                bindings1,
                bindings2,
                body,
            } => {
                let unique_bindings = unique_quantified_bindings(&bindings1, &bindings2);
                let binding_targets = binding_targets(&body, None, &unique_bindings);

                BoundExpr::wrap(move |ctx| {
                    let mut all_true = true;
                    let mut any_true = false;
                    let mut eval_count: usize = 0;

                    let params_per_binding: Vec<Vec<Vec<Value>>> = binding_targets
                        .iter()
                        .map(|(domain, target)| params_for_binding_target(ctx, domain, *target))
                        .collect();

                    enumerate_binding_combos(
                        0,
                        &unique_bindings,
                        &params_per_binding,
                        &mut HashMap::new(),
                        &mut |mapping| {
                            eval_count += 1;
                            let result = substitute_quantified_bindings(&body, mapping)
                                .lower()
                                .run(ctx);
                            let b = result.boolean_value();
                            all_true = all_true && b;
                            any_true = any_true || b;
                        },
                    );

                    if eval_count == 0 {
                        all_true = true;
                    }

                    match &quantifier {
                        crate::oorvir::refined::Quantifier::Forall => Value::Bool(all_true),
                        crate::oorvir::refined::Quantifier::Exists => Value::Bool(any_true),
                    }
                })
            }

            QuantifiedVar(_) => BoundExpr::wrap(|_| Value::None),

            Ite {
                condition,
                consequence,
                alternative,
                ..
            } => {
                let cond = condition.lower();
                let then_br = consequence.lower();
                let else_br = alternative.lower();
                BoundExpr::wrap(move |ctx| {
                    if cond.run(ctx).boolean_value() {
                        then_br.run(ctx)
                    } else {
                        else_br.run(ctx)
                    }
                })
            }

            Tuple(entries) => {
                let compiled: Vec<BoundExpr> = entries.into_iter().map(|e| e.lower()).collect();
                BoundExpr::wrap(move |ctx| {
                    Value::Tuple(compiled.iter().map(|f| f.run(ctx)).collect())
                })
            }

            Function(name, args) => {
                assert!(!args.is_empty());
                let first_arg = args[0].clone().lower();

                macro_rules! decimal_fn {
                    ($fn:ident) => {
                        BoundExpr::wrap(move |ctx| match first_arg.run(ctx) {
                            Value::Float(f) => Value::try_from(f.$fn()).unwrap(),
                            Value::Decimal(f) => Value::try_from(f.$fn()).unwrap(),
                            _ => unreachable!("unexpected value type for {}", stringify!($fn)),
                        })
                    };
                }

                macro_rules! float_fn {
                    ($fn:ident) => {
                        BoundExpr::wrap(move |ctx| match first_arg.run(ctx) {
                            Value::Float(f) => Value::try_from(f.$fn()).unwrap(),
                            _ => unreachable!("unexpected value type for {}", stringify!($fn)),
                        })
                    };
                }

                macro_rules! binary_numeric {
                    ($fn:ident) => {{
                        assert_eq!(
                            args.len(),
                            2,
                            "binary function requires exactly 2 arguments"
                        );
                        let second_arg = args[1].clone().lower();
                        BoundExpr::wrap(move |ctx| {
                            let a = first_arg.run(ctx);
                            let b = second_arg.run(ctx);
                            match (a, b) {
                                (Value::Float(f1), Value::Float(f2)) => Value::Float(f1.$fn(f2)),
                                (Value::Signed(s1), Value::Signed(s2)) => Value::Signed(s1.$fn(s2)),
                                (Value::Unsigned(u1), Value::Unsigned(u2)) => {
                                    Value::Unsigned(u1.$fn(u2))
                                }
                                (v1, v2) => unreachable!(
                                    "unexpected value types {:?}, {:?} for {}",
                                    v1,
                                    v2,
                                    stringify!($fn)
                                ),
                            }
                        })
                    }};
                }

                match name.as_ref() {
                    "sqrt" => decimal_fn!(sqrt),
                    "sin" => decimal_fn!(sin),
                    "cos" => decimal_fn!(cos),
                    "tan" => decimal_fn!(tan),
                    "arcsin" => float_fn!(asin),
                    "arccos" => float_fn!(acos),
                    "arctan" => float_fn!(atan),
                    "abs" => BoundExpr::wrap(move |ctx| match first_arg.run(ctx) {
                        Value::Float(f) => Value::try_from(f.abs()).unwrap(),
                        Value::Signed(i) => Value::Signed(i.abs()),
                        v => unreachable!("unexpected value type {:?} for abs", v),
                    }),
                    "min" => binary_numeric!(min),
                    "max" => binary_numeric!(max),
                    "matches" => {
                        assert!(args.len() >= 2, "matches requires at least 2 arguments");
                        let is_bytes = args[0].annotation == Type::Bytes;
                        let re_str = match &args[1].kind {
                            LoadConstant(Constant::Str(s)) => s,
                            _ => unreachable!("regex pattern must be a string literal"),
                        };
                        if !is_bytes {
                            let re = Regex::new(re_str).expect("invalid regular expression");
                            BoundExpr::wrap(move |ctx| match first_arg.run(ctx) {
                                Value::Str(s) => Value::Bool(re.is_match(&s)),
                                v => unreachable!("expected String, found {:?}", v),
                            })
                        } else {
                            let re =
                                BytesRegex::new(re_str).expect("invalid bytes regular expression");
                            BoundExpr::wrap(move |ctx| match first_arg.run(ctx) {
                                Value::Bytes(b) => Value::Bool(re.is_match(&b)),
                                v => unreachable!("expected Bytes, found {:?}", v),
                            })
                        }
                    }
                    "at" => {
                        assert_eq!(args.len(), 2, "at requires exactly 2 arguments");
                        let index_expr = args[1].clone().lower();
                        BoundExpr::wrap(move |ctx| {
                            let val = first_arg.run(ctx);
                            let idx = index_expr.run(ctx);
                            match (val, idx) {
                                (Value::Bytes(b), Value::Unsigned(i)) => b
                                    .get(i as usize)
                                    .map_or(Value::None, |&byte| Value::Unsigned(byte.into())),
                                (v, _) => unreachable!("expected Bytes, found {:?}", v),
                            }
                        })
                    }
                    "format" => {
                        assert!(
                            args.len() > 1,
                            "format requires a template and at least one argument"
                        );
                        let LoadConstant(Constant::Str(fstr)) = &args[0].kind else {
                            panic!("format template must be a static string literal");
                        };
                        let template = Template::new(fstr);
                        let arg_exprs: Vec<BoundExpr> =
                            args.into_iter().skip(1).map(|e| e.lower()).collect();
                        BoundExpr::wrap(move |ctx| {
                            let vals: Vec<String> =
                                arg_exprs.iter().map(|e| e.run(ctx).to_string()).collect();
                            let refs: Vec<&str> = vals.iter().map(String::as_str).collect();
                            template.render_positional(&refs).into()
                        })
                    }
                    "round" => {
                        assert!(args.len() > 1, "round requires a decimal-places argument");
                        let LoadConstant(Constant::UInt(places)) = &args[1].kind else {
                            panic!("round decimal places must be a static unsigned integer");
                        };
                        let scale = 10u64.pow(*places as u32) as f64;
                        BoundExpr::wrap(move |ctx| match first_arg.run(ctx) {
                            Value::Float(f) => {
                                Value::try_from((f * scale).round() / scale).unwrap()
                            }
                            _ => unreachable!("round expects a Float value"),
                        })
                    }
                    fname => {
                        let func_name = fname.to_string();
                        let arg_exprs: Vec<BoundExpr> =
                            args.into_iter().map(|e| e.lower()).collect();
                        BoundExpr::wrap(move |ctx| {
                            let arg_vals: Vec<Value> =
                                arg_exprs.iter().map(|a| a.run(ctx)).collect();
                            match ctx.user_functions.get(&func_name) {
                                Some(func) => func.run(&ctx.fork_with_params(&arg_vals)),
                                None => {
                                    unreachable!("unknown user function at runtime: {}", func_name)
                                }
                            }
                        })
                    }
                }
            }

            Convert { expr: inner } => {
                let from_ty = inner.annotation.clone();
                let to_ty = self.annotation.clone();
                let compiled = inner.lower();

                macro_rules! cast {
                    (Float, $to:ident, $ty:ty) => {
                        BoundExpr::wrap(move |ctx| match compiled.run(ctx) {
                            Value::Float(f) => Value::$to(f.into_inner() as $ty),
                            v => unreachable!("type mismatch in cast: {:?}", v),
                        })
                    };
                    ($from:ident, Float, $ty:ty) => {
                        BoundExpr::wrap(move |ctx| match compiled.run(ctx) {
                            Value::$from(v) => Value::try_from(v as $ty).unwrap(),
                            v => unreachable!("type mismatch in cast: {:?}", v),
                        })
                    };
                    ($from:ident, $to:ident, $ty:ty) => {
                        BoundExpr::wrap(move |ctx| match compiled.run(ctx) {
                            Value::$from(v) => Value::$to(v as $ty),
                            v => unreachable!("type mismatch in cast: {:?}", v),
                        })
                    };
                    ($from:ident, $to:ident, $fn:expr) => {
                        BoundExpr::wrap(move |ctx| match compiled.run(ctx) {
                            Value::$from(v) => Value::$to($fn(v)),
                            v => unreachable!("type mismatch in cast: {:?}", v),
                        })
                    };
                }

                use Type::*;
                match (&from_ty, &to_ty) {
                    (UInt(_), UInt(_))
                    | (Int(_), Int(_))
                    | (Fixed(_), Fixed(_))
                    | (UFixed(_), UFixed(_)) => compiled,
                    (UInt(_), Int(_)) => cast!(Unsigned, Signed, i64),
                    (UInt(_), Float(_)) => cast!(Unsigned, Float, f64),
                    (Int(_), UInt(_)) => cast!(Signed, Unsigned, u64),
                    (Int(_), Float(_)) => cast!(Signed, Float, f64),
                    (Float(_), UInt(_)) => cast!(Float, Unsigned, u64),
                    (Float(_), Int(_)) => cast!(Float, Signed, i64),
                    (UInt(_), Fixed(_) | UFixed(_)) => {
                        cast!(Signed, Decimal, |v: i64| Decimal::from(v))
                    }
                    (Int(_), Fixed(_) | UFixed(_)) => {
                        cast!(Unsigned, Decimal, |v: u64| Decimal::from(v))
                    }
                    (Float(_), Fixed(_) | UFixed(_)) => {
                        cast!(Float, Decimal, |v: NotNan<f64>| {
                            Decimal::from_f64(v.to_f64().unwrap()).unwrap()
                        })
                    }
                    (Fixed(_) | UFixed(_), Float(_)) => {
                        cast!(Decimal, Float, |v: Decimal| {
                            NotNan::try_from(v.to_f64().unwrap()).unwrap()
                        })
                    }
                    (Fixed(_) | UFixed(_), Int(_)) => {
                        cast!(Decimal, Signed, |v: Decimal| v.round().to_i64().unwrap())
                    }
                    (Fixed(_) | UFixed(_), UInt(_)) => {
                        cast!(Decimal, Unsigned, |v: Decimal| v.round().to_u64().unwrap())
                    }
                    (from, to) => unreachable!("unsupported cast from {:?} to {:?}", from, to),
                }
            }

            Default { expr, default, .. } => {
                let primary = expr.lower();
                let fallback = default.lower();
                BoundExpr::wrap(move |ctx| {
                    let v = primary.run(ctx);
                    if matches!(v, Value::None) {
                        fallback.run(ctx)
                    } else {
                        v
                    }
                })
            }

            TupleAccess(expr, num) => {
                let compiled = expr.lower();
                BoundExpr::wrap(move |ctx| match compiled.run(ctx) {
                    Value::Tuple(elems) => elems[num].clone(),
                    Value::None => Value::None,
                    _ => unreachable!("tuple access on non-tuple value (checked by type checker)"),
                })
            }
        }
    }
}
