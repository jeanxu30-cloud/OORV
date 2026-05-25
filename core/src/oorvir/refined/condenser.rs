use std::collections::{BTreeSet, HashMap};

use crate::oorvir::refined::ActivationCondition as RefinedActivationCondition;
use crate::oorvir::source::ActivationCondition as SourceActivationCondition;

use itertools::Itertools;
use num::ToPrimitive;

use crate::oorvir::refined::{
    AccessMode, Alarm, ArithLogOp, Constant, ConstraintStream, End, Eval, EvalDecls,
    EventTaskStream, ExprVariant, Expression, FixedTy, FloatTy, IntTy, PacingLocality, PacingNode,
    ParamDecl as RefinedParamDecl, PeriodicTaskStream, Quantifier, Shift, SignalStream, Start,
    Type, UIntTy, OORVIR,
};
use crate::oorvir::source::OORVIr1;
use crate::oorvir::source::{
    AccessMode as SourceAccessMode, AccessSite, ArithLogOp as SourceArithLogOp,
    Constant as SourceConstant, ConstraintKind, DataType, ExprVariant as SourceExprVariant,
    Expression as SourceExpression, FnExprKind, HasDependencies, HasMemory, HasSchedule,
    HasTypeInfo, Inlined, Literal, Shift as SourceShift, StreamIdx, StreamPacingKind,
    WidenExprKind,
};

struct IrCondenser<'h> {
    source: &'h OORVIr1,
    remap: HashMap<StreamIdx, StreamIdx>,
}

impl OORVIR {
    pub fn compile_from_source(source_ir: OORVIr1) -> OORVIR {
        IrCondenser::init(&source_ir).condense()
    }
}

impl<'h> IrCondenser<'h> {
    // Build the lowerer and eagerly populate the index remapping table so all
    // IR StreamIdx values can be translated to contiguous refined_ir indices in O(1).
    fn init(source_ir: &'h OORVIr1) -> Self {
        let mut remap: HashMap<StreamIdx, StreamIdx> = HashMap::new();

        // Signal streams are numbered from 0 in stable IR order.
        let signal_entries = source_ir
            .signals()
            .sorted_by(|a, b| Ord::cmp(&a.si(), &b.si()))
            .enumerate()
            .map(|(rank, sig)| (sig.si(), StreamIdx::Signal(rank)));
        remap.extend(signal_entries);

        // Constraint streams are numbered from 0 in stable IR order.
        let constraint_entries = source_ir
            .constraints()
            .sorted_by(|a, b| Ord::cmp(&a.si(), &b.si()))
            .enumerate()
            .map(|(rank, cs)| (cs.si(), StreamIdx::Constraint(rank)));
        remap.extend(constraint_entries);

        let total_streams = source_ir.signals().count() + source_ir.constraints().count();
        debug_assert_eq!(
            remap.len(),
            total_streams,
            "index remap table must contain exactly one entry per source_ir stream"
        );

        Self {
            source: source_ir,
            remap,
        }
    }

    // Run all lowering passes and assemble the final OORVIR.
    fn condense(&self) -> OORVIR {
        // Pass 1: lower signal streams in stable IR index order.
        let signal_list = self
            .source
            .signals()
            .sorted_by(|a, b| Ord::cmp(&a.si(), &b.si()))
            .map(|sig| {
                let ir_idx = sig.si();
                SignalStream {
                    name: sig.name.clone(),
                    annotation: convert_data_type(&self.source.stream_signature(ir_idx).data_kind),
                    consumers: self
                        .translate_access_list(self.source.in_streams_with_sites(ir_idx)),
                    stream_level: self.source.scheduling_layers(ir_idx),
                    storage_bound: self.source.required_storage_bound(ir_idx),
                    stream_idx: self.remap[&ir_idx],
                }
            })
            .collect::<Vec<_>>();

        assert!(
            signal_list
                .iter()
                .enumerate()
                .all(|(pos, s)| pos == s.stream_idx.in_ix()),
            "signal stream IR indices must form a contiguous range starting at 0"
        );

        // Pass 2: lower constraint streams; retain the IR index alongside each lowered
        // stream so it can be used in the task extraction passes below.
        let paired_constraints: Vec<(StreamIdx, ConstraintStream)> = self
            .source
            .constraints()
            .sorted_by(|a, b| Ord::cmp(&a.si(), &b.si()))
            .map(|cs| {
                let ir_idx = cs.si();
                let lowered = ConstraintStream {
                    name: cs.name(),
                    kind: cs.kind.clone(),
                    annotation: convert_data_type(&self.source.stream_signature(ir_idx).data_kind),
                    start: self.lower_start_info(ir_idx),
                    eval: self.lower_eval_info(ir_idx),
                    end: self.lower_end_info(ir_idx),
                    dependencies: self
                        .translate_access_list(self.source.out_streams_with_sites(ir_idx)),
                    consumers: self
                        .translate_access_list(self.source.in_streams_with_sites(ir_idx)),
                    storage_bound: self.source.required_storage_bound(ir_idx),
                    stream_level: self.source.scheduling_layers(ir_idx),
                    stream_idx: self.remap[&ir_idx],
                    params: self.lower_param_declarations(
                        self.source
                            .output(ir_idx)
                            .expect("constraint stream must exist in IR")
                            .params(),
                        ir_idx,
                    ),
                    level: cs.level.clone(),
                };
                (ir_idx, lowered)
            })
            .collect();

        // Collect the ir constraint streams sorted by their new IR index.
        let constraint_list: Vec<ConstraintStream> = paired_constraints
            .iter()
            .map(|(_, cs)| cs.clone())
            .sorted_by(|a, b| Ord::cmp(&a.stream_idx, &b.stream_idx))
            .collect();

        assert!(
            constraint_list
                .iter()
                .enumerate()
                .all(|(pos, cs)| pos == cs.stream_idx.out_ix()),
            "constraint stream refined_ir indices must form a contiguous range starting at 0"
        );

        // Pass 3: build periodic and event task entries.
        let periodic_tasks: Vec<PeriodicTaskStream> = paired_constraints
            .iter()
            .filter(|(ir_idx, _)| self.source.has_periodic_pacing(*ir_idx))
            .map(|(ir_idx, _)| self.build_periodic_task_entry(*ir_idx))
            .collect();

        let event_tasks: Vec<EventTaskStream> = paired_constraints
            .iter()
            .filter(|(ir_idx, _)| self.source.has_event_pacing(*ir_idx))
            .map(|(ir_idx, _)| self.build_event_task_entry(*ir_idx))
            .collect();

        // Pass 4: collect alarm entries from alarm-kind constraint streams.
        let alarm_list: Vec<Alarm> = constraint_list
            .iter()
            .filter_map(|cs| matches!(&cs.kind, ConstraintKind::Alarm(_)).then_some(cs.stream_idx))
            .enumerate()
            .map(|(alarm_pos, cs_idx)| Alarm {
                alarm_idx: alarm_pos,
                constrain_idx: cs_idx,
            })
            .collect();

        // Pass 5: lower all user-defined function bodies.
        let function_bodies: HashMap<String, Expression> = self
            .source
            .func_bodies()
            .map(|(fn_name, expr_id)| {
                let body = self.lower_expr(self.source.expression(*expr_id));
                (fn_name.clone(), body)
            })
            .collect();
        let object_domains = self.lower_object_domains(&constraint_list);

        OORVIR {
            signals: signal_list,
            constraints: constraint_list,
            object_domains,
            time_task: periodic_tasks,
            event_task: event_tasks,
            alarms: alarm_list,
            func_bodies: function_bodies,
        }
    }

    fn lower_object_domains(&self, constraints: &[ConstraintStream]) -> HashMap<String, StreamIdx> {
        self.source
            .object_domains()
            .filter_map(|(domain, class_name)| {
                let class_suffix = class_name
                    .rsplit("::")
                    .next()
                    .unwrap_or(class_name.as_str());
                constraints
                    .iter()
                    .find(|constraint| {
                        let stream_class = constraint
                            .name
                            .strip_suffix("_params")
                            .unwrap_or(&constraint.name)
                            .rsplit_once("::")
                            .map(|(prefix, _)| prefix)
                            .unwrap_or("");
                        let stream_suffix =
                            stream_class.rsplit("::").next().unwrap_or(stream_class);
                        !constraint.params.is_empty()
                            && constraint.name.ends_with("_params")
                            && (stream_class == class_name || stream_suffix == class_suffix)
                    })
                    .map(|constraint| (domain.clone(), constraint.stream_idx))
            })
            .collect()
    }

    // Build an EventTaskStream for a constraint stream with event-based pacing.
    fn build_event_task_entry(&self, ir_idx: StreamIdx) -> EventTaskStream {
        let pacing = &self.source.stream_signature(ir_idx).execution_rate;
        if let StreamPacingKind::Conditional(ac) = pacing {
            EventTaskStream {
                stream_idx: self.remap[&ir_idx],
                ac: self.map_activation_condition(ac),
            }
        } else {
            unreachable!(
                "build_event_task_entry requires a Conditional pacing; got {:?}",
                pacing
            )
        }
    }

    fn map_activation_condition(
        &self,
        ac: &SourceActivationCondition,
    ) -> RefinedActivationCondition {
        fn build_stream_ac(
            conjunction: &BTreeSet<StreamIdx>,
            remap: &HashMap<StreamIdx, StreamIdx>,
        ) -> RefinedActivationCondition {
            match conjunction.len() {
                1 => {
                    let sole = conjunction
                        .iter()
                        .next()
                        .expect("singleton conjunction must have an element");
                    RefinedActivationCondition::Stream(remap[sole])
                }
                _ => RefinedActivationCondition::Conjunction(
                    conjunction
                        .iter()
                        .map(|idx| RefinedActivationCondition::Stream(remap[idx]))
                        .collect(),
                ),
            }
        }

        match ac {
            SourceActivationCondition::Models(disjuncts) if disjuncts.len() == 1 => {
                let sole_conj = disjuncts
                    .iter()
                    .next()
                    .expect("single-disjunct AC must not be empty");
                build_stream_ac(sole_conj, &self.remap)
            }
            SourceActivationCondition::Models(disjuncts) => {
                let branches = disjuncts
                    .iter()
                    .map(|conj| build_stream_ac(conj, &self.remap))
                    .collect();
                RefinedActivationCondition::Disjunction(branches)
            }
            SourceActivationCondition::True => RefinedActivationCondition::True,
        }
    }

    // Build a PeriodicTaskStream for a constraint stream with clock-based pacing.
    fn build_periodic_task_entry(&self, ir_idx: StreamIdx) -> PeriodicTaskStream {
        let rate = &self.source.stream_signature(ir_idx).execution_rate;
        match rate {
            StreamPacingKind::GlobalClock(freq) => PeriodicTaskStream {
                stream_idx: self.remap[&ir_idx],
                frequency: *freq,
                locality: PacingLocality::Global,
            },
            StreamPacingKind::LocalClock(freq) => PeriodicTaskStream {
                stream_idx: self.remap[&ir_idx],
                frequency: *freq,
                locality: PacingLocality::Local,
            },
            other => unreachable!(
                "build_periodic_task_entry requires a clock pacing; got {:?}",
                other
            ),
        }
    }

    // Map a source_ir StreamPacingKind to the corresponding refined_ir PacingNode.
    fn convert_pacing(&self, rate: StreamPacingKind) -> PacingNode {
        match rate {
            StreamPacingKind::Conditional(ac) => {
                PacingNode::Event(self.map_activation_condition(&ac))
            }
            StreamPacingKind::LocalClock(freq) => PacingNode::LocalTick(freq),
            StreamPacingKind::GlobalClock(freq) => PacingNode::GlobalTick(freq),
            StreamPacingKind::Unconditional => PacingNode::Constant,
            other => unreachable!(
                "unsupported pacing kind reached IR lowering  should have been eliminated by the type checker: {:?}",
                other
            ),
        }
    }

    // Lower the start clause for the constraint stream at `ir_idx`.
    fn lower_start_info(&self, ir_idx: StreamIdx) -> Start {
        let sig = self.source.stream_signature(ir_idx);
        let init_pacing = self.convert_pacing(sig.init_rate);
        let start_cond_expr = self.source.start_cond(ir_idx).map(|e| self.lower_expr(e));
        let start_init_expr = self.source.start_expr(ir_idx).map(|e| self.lower_expr(e));

        Start {
            expression: start_init_expr,
            pacing: init_pacing,
            condition: start_cond_expr,
        }
    }

    // Lower all eval clauses for the constraint stream at `ir_idx`.
    fn lower_eval_info(&self, ir_idx: StreamIdx) -> Eval {
        let all_exprs = self
            .source
            .eval_expr(ir_idx)
            .expect("eval expressions must be present for every constraint stream");
        let all_conds = self
            .source
            .eval_cond(ir_idx)
            .expect("eval conditions must be present for every constraint stream");

        assert_eq!(
            all_exprs.len(),
            all_conds.len(),
            "eval expression count must match condition count for stream {:?}",
            ir_idx
        );

        let clause_list: Vec<EvalDecls> = all_exprs
            .iter()
            .zip(all_conds.iter())
            .enumerate()
            .map(|(pos, (body, guard))| {
                let lowered_body = self.lower_expr(body);
                let lowered_guard = guard.map(|g| self.lower_expr(g));
                let clause_pacing = self.convert_pacing(self.source.eval_pacing_at(ir_idx, pos));

                EvalDecls {
                    pacing: clause_pacing,
                    condition: lowered_guard,
                    expression: lowered_body,
                }
            })
            .collect();

        let stream_pacing =
            self.convert_pacing(self.source.stream_signature(ir_idx).execution_rate);
        Eval {
            decls: clause_list,
            eval_pacing: stream_pacing,
        }
    }

    // Lower the end clause for the constraint stream at `ir_idx`.
    fn lower_end_info(&self, ir_idx: StreamIdx) -> End {
        let (end_cond, end_pacing, end_self_ref) = self
            .source
            .end_cond(ir_idx)
            .map(|cond_expr| {
                // The termination condition references the stream itself when the expression
                // pacing is non-trivial (i.e. conditional, global clock, or local clock).
                let end_self_ref = matches!(
                    self.source.expression_type(cond_expr.id()).init_rate,
                    StreamPacingKind::Conditional(_)
                        | StreamPacingKind::GlobalClock(_)
                        | StreamPacingKind::LocalClock(_)
                );
                let termination_pacing =
                    self.convert_pacing(self.source.stream_signature(ir_idx).termination_rate);
                (
                    Some(self.lower_expr(cond_expr)),
                    termination_pacing,
                    end_self_ref,
                )
            })
            .unwrap_or((None, PacingNode::Constant, false));

        End {
            condition: end_cond,
            pacing: end_pacing,
            has_self_idx: end_self_ref,
        }
    }

    // Lower a source_ir Expression node to its refined_ir counterpart.
    fn lower_expr(&self, expr: &SourceExpression) -> Expression {
        let result_type = convert_data_type(&self.source.expression_type(expr.id()).data_kind);
        let lowered_kind = self.lower_expr_kind(&expr.kind, &result_type);
        Expression {
            kind: lowered_kind,
            annotation: result_type,
        }
    }

    // Lower an ExprVariant body into its refined_ir form.
    fn lower_expr_kind(&self, variant: &SourceExprVariant, result_type: &Type) -> ExprVariant {
        match variant {
            SourceExprVariant::LoadConstant(c) => {
                ExprVariant::LoadConstant(lower_constant_value(c, result_type))
            }
            SourceExprVariant::ArithLog(op, operands) => {
                let rir_op = convert_arith_op(*op);
                let rir_operands: Vec<Expression> =
                    operands.iter().map(|o| self.lower_expr(o)).collect();
                ExprVariant::ArithLog(rir_op, rir_operands)
            }
            SourceExprVariant::StreamAccess(ir_idx, mode, params) => ExprVariant::StreamAccess {
                target: self.remap[ir_idx],
                access_kind: convert_access_kind(*mode),
                parameters: params.iter().map(|p| self.lower_expr(p)).collect(),
            },
            SourceExprVariant::ParameterAccess(ir_idx, param_pos) => {
                ExprVariant::ParameterAccess(self.remap[ir_idx], *param_pos)
            }
            SourceExprVariant::FunctionParameterAccess(_name, _site, position) => {
                ExprVariant::FunctionParameterAccess(*position)
            }
            SourceExprVariant::Ite {
                condition,
                consequence,
                alternative,
            } => ExprVariant::Ite {
                condition: Box::new(self.lower_expr(condition)),
                consequence: Box::new(self.lower_expr(consequence)),
                alternative: Box::new(self.lower_expr(alternative)),
            },
            SourceExprVariant::Tuple(items) => {
                ExprVariant::Tuple(items.iter().map(|item| self.lower_expr(item)).collect())
            }
            SourceExprVariant::TupleAccess(tup, field_pos) => {
                ExprVariant::TupleAccess(Box::new(self.lower_expr(tup)), *field_pos)
            }
            SourceExprVariant::Function(fn_kind) => {
                let FnExprKind { name, args, .. } = fn_kind;
                if name.as_ref() as &str == "cast" {
                    assert_eq!(
                        args.len(),
                        1,
                        "cast expression must have exactly one argument"
                    );
                    ExprVariant::Convert {
                        expr: Box::new(self.lower_expr(&args[0])),
                    }
                } else {
                    let lowered_args: Vec<Expression> =
                        args.iter().map(|a| self.lower_expr(a)).collect();
                    ExprVariant::Function(name.clone(), lowered_args)
                }
            }
            SourceExprVariant::Widen(widen_kind) => {
                let WidenExprKind { expr, .. } = widen_kind;
                ExprVariant::Convert {
                    expr: Box::new(self.lower_expr(expr)),
                }
            }
            SourceExprVariant::Default { expr, default } => ExprVariant::Default {
                expr: Box::new(self.lower_expr(expr)),
                default: Box::new(self.lower_expr(default)),
            },
            SourceExprVariant::QuantifiedVar(ident) => {
                ExprVariant::QuantifiedVar(ident.name.clone())
            }
            SourceExprVariant::Quantified(q, group_a, group_b, body) => {
                let rir_quantifier = match q {
                    crate::oorvir::source::Quantifier::Forall => Quantifier::Forall,
                    crate::oorvir::source::Quantifier::Exists => Quantifier::Exists,
                };
                let names_a: Vec<String> = group_a.iter().map(|id| id.name.clone()).collect();
                let names_b: Vec<String> = group_b.iter().map(|id| id.name.clone()).collect();
                ExprVariant::Quantified {
                    quantifier: rir_quantifier,
                    bindings1: names_a,
                    bindings2: names_b,
                    body: Box::new(self.lower_expr(body)),
                }
            }
        }
    }

    // Remap a list of (source_ir index, access sites) pairs to use refined_ir stream indices.
    fn translate_access_list(
        &self,
        raw: Vec<(StreamIdx, Vec<(AccessSite, SourceAccessMode)>)>,
    ) -> Vec<(StreamIdx, Vec<(AccessSite, AccessMode)>)> {
        raw.into_iter()
            .map(|(ir_idx, site_modes)| {
                let rir_idx = self.remap[&ir_idx];
                let converted: Vec<(AccessSite, AccessMode)> = site_modes
                    .into_iter()
                    .map(|(site, mode)| (site, convert_access_kind(mode)))
                    .collect();
                (rir_idx, converted)
            })
            .collect()
    }

    // Lower a list of source_ir parameter declarations to refined_ir ParamDecl entries.
    fn lower_param_declarations<'a>(
        &self,
        params: impl Iterator<Item = &'a crate::oorvir::source::ParamDecl>,
        ir_idx: StreamIdx,
    ) -> Vec<RefinedParamDecl> {
        params
            .map(|p| RefinedParamDecl {
                name: p.name.clone(),
                annotation: convert_data_type(&self.source.parameter_type_at(ir_idx, p.index())),
                idx: p.index(),
            })
            .collect()
    }
}

// Map a source_ir DataType to the corresponding refined_ir Type.
// All types must be fully resolved before this function is called.
fn convert_data_type(ty: &DataType) -> Type {
    match ty {
        DataType::Bool => Type::Bool,
        DataType::Integer8 => Type::Int(IntTy::Int8),
        DataType::Integer16 => Type::Int(IntTy::Int16),
        DataType::Integer32 => Type::Int(IntTy::Int32),
        DataType::Integer64 => Type::Int(IntTy::Int64),
        DataType::Integer128 => Type::Int(IntTy::Int128),
        DataType::Integer256 => Type::Int(IntTy::Int256),
        DataType::UInteger8 => Type::UInt(UIntTy::UInt8),
        DataType::UInteger16 => Type::UInt(UIntTy::UInt16),
        DataType::UInteger32 => Type::UInt(UIntTy::UInt32),
        DataType::UInteger64 => Type::UInt(UIntTy::UInt64),
        DataType::UInteger128 => Type::UInt(UIntTy::UInt128),
        DataType::UInteger256 => Type::UInt(UIntTy::UInt256),
        DataType::Float32 => Type::Float(FloatTy::Float32),
        DataType::Float64 => Type::Float(FloatTy::Float64),
        DataType::Fixed64_32 => Type::Fixed(FixedTy::Fixed64_32),
        DataType::Fixed32_16 => Type::Fixed(FixedTy::Fixed32_16),
        DataType::Fixed16_8 => Type::Fixed(FixedTy::Fixed16_8),
        DataType::UFixed64_32 => Type::UFixed(FixedTy::Fixed64_32),
        DataType::UFixed32_16 => Type::UFixed(FixedTy::Fixed32_16),
        DataType::UFixed16_8 => Type::UFixed(FixedTy::Fixed16_8),
        DataType::Tuple(inner_types) => {
            let converted: Vec<Type> = inner_types.iter().map(convert_data_type).collect();
            Type::Tuple(converted)
        }
        DataType::TString => Type::String,
        DataType::Byte => Type::Bytes,
        DataType::Option(inner) => Type::Option(Box::new(convert_data_type(inner))),
    }
}

// Emit a compile-time constant, coercing the stored numeric value to the
// declared refined_ir type annotation when necessary.
fn lower_constant_value(constant: &SourceConstant, declared_type: &Type) -> Constant {
    let lit = match constant {
        SourceConstant::Basic(l) | SourceConstant::Inlined(Inlined { lit: l, .. }) => l,
    };

    match (lit, declared_type) {
        (Literal::Str(s), _) => Constant::Str(s.clone()),
        (Literal::Bool(b), _) => Constant::Bool(*b),
        (Literal::UInt(v), Type::Int(_)) => Constant::Int(*v),
        (Literal::UInt(v), Type::UInt(_)) => Constant::UInt(*v as u64),
        (Literal::SInt(v), Type::Int(_)) => Constant::Int(*v as i64),
        (Literal::SInt(v), Type::UInt(_)) => Constant::UInt(*v as u64),
        (Literal::Decimal(d), Type::Float(_)) => {
            Constant::Float(d.to_f64().expect("decimal-to-f64 conversion failed"))
        }
        (Literal::Decimal(d), Type::Fixed(_) | Type::UFixed(_)) => Constant::Decimal(*d),
        _ => unreachable!(
            "constant lowering type mismatch  literal={:?}, declared_type={:?}",
            lit, declared_type
        ),
    }
}

// Translate a source_ir binary/unary operator to the equivalent refined_ir operator.
fn convert_arith_op(op: SourceArithLogOp) -> ArithLogOp {
    match op {
        SourceArithLogOp::Not => ArithLogOp::Not,
        SourceArithLogOp::Neg => ArithLogOp::Neg,
        SourceArithLogOp::Add => ArithLogOp::Add,
        SourceArithLogOp::Sub => ArithLogOp::Sub,
        SourceArithLogOp::Mul => ArithLogOp::Mul,
        SourceArithLogOp::Div => ArithLogOp::Div,
        SourceArithLogOp::Rem => ArithLogOp::Rem,
        SourceArithLogOp::Pow => ArithLogOp::Pow,
        SourceArithLogOp::And => ArithLogOp::And,
        SourceArithLogOp::Or => ArithLogOp::Or,
        SourceArithLogOp::BitXor => ArithLogOp::BitXor,
        SourceArithLogOp::BitAnd => ArithLogOp::BitAnd,
        SourceArithLogOp::BitOr => ArithLogOp::BitOr,
        SourceArithLogOp::BitNot => ArithLogOp::BitNot,
        SourceArithLogOp::Shl => ArithLogOp::Shl,
        SourceArithLogOp::Shr => ArithLogOp::Shr,
        SourceArithLogOp::Eq => ArithLogOp::Eq,
        SourceArithLogOp::Lt => ArithLogOp::Lt,
        SourceArithLogOp::Le => ArithLogOp::Le,
        SourceArithLogOp::Ne => ArithLogOp::Ne,
        SourceArithLogOp::Ge => ArithLogOp::Ge,
        SourceArithLogOp::Gt => ArithLogOp::Gt,
    }
}

// Convert a source_ir stream access mode to its refined_ir counterpart.
// Real-time offset variants must have been eliminated before this point.
fn convert_access_kind(mode: SourceAccessMode) -> AccessMode {
    match mode {
        SourceAccessMode::Strict => AccessMode::Strict,
        SourceAccessMode::Cached => AccessMode::Cached,
        SourceAccessMode::Shift(offset) => {
            let rir_shift = match offset {
                SourceShift::FutureDiscrete(n) => Shift::Future(n),
                SourceShift::PastDiscrete(n) => Shift::Past(n),
                SourceShift::FutureRealTime(_) | SourceShift::PastRealTime(_) => unreachable!(
                    "real-time offset lookups must be rewritten to discrete form before refined_ir lowering"
                ),
            };
            AccessMode::Shift(rir_shift)
        }
        SourceAccessMode::Get => AccessMode::Get,
        SourceAccessMode::Fresh => AccessMode::Fresh,
    }
}
