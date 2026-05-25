use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Not;
use std::rc::Rc;
use std::time::Duration;

use crate::oorvir::refined::{
    ActivationCondition as Activation, Alarm, ConstraintKind, PacingLocality, PacingNode,
    PeriodicTaskStream, Stream, StreamIdx, Task, OORVIR,
};
use bit_set::BitSet;
use num::traits::Inv;
use uom::si::rational64::Time as UOM_Time;
use uom::si::time::nanosecond;

use crate::runtime::dispatch::{EventQueue, WorkItem};
use crate::runtime::eval::compiler::{BoundExpr, Compilable};
use crate::runtime::iface::watcher::{Change, StreamInstance};
use crate::runtime::iface::watcher::{EvalTracer, InstanceKey};
use crate::runtime::store::{DataStore, Value, ValueBuffer};

/// The evaluation context received by every [`BoundExpr`] closure at runtime.
///
/// Provides read access to stream values, freshness sets, the current timestamp,
/// instance parameters, and the user-function registry.
pub(crate) struct EvalFrame<'e> {
    pub(crate) ts: Duration,
    global_store: &'e DataStore,
    active_inputs: &'e BitSet,
    active_outputs: &'e BitSet,
    pub(crate) parameter: &'e [Value],
    pub(crate) lambda_parameter: Option<&'e [Value]>,
    object_domains: &'e HashMap<String, StreamIdx>,
    /// Registry of compiled user-defined functions.
    pub(crate) user_functions: &'e HashMap<String, BoundExpr>,
}

impl EvalFrame<'_> {
    /// Return whether the given stream is parameterized by live object instances.
    pub(crate) fn stream_is_parameterized(&self, stream_ref: StreamIdx) -> bool {
        match stream_ref {
            StreamIdx::Signal(_) => false,
            StreamIdx::Constraint(ix) => self.global_store.constraint_is_parameterized(ix),
        }
    }

    /// Return all instance parameter vectors for the given parameterized stream.
    pub(crate) fn fetch_instance_params(&self, stream_ref: StreamIdx) -> Vec<Vec<Value>> {
        match stream_ref {
            StreamIdx::Signal(_) => Vec::new(),
            StreamIdx::Constraint(ix) if self.stream_is_parameterized(stream_ref) => {
                let mut params: Vec<Vec<Value>> =
                    self.global_store.group(ix).params().cloned().collect();
                params.sort_by_key(|p| p.iter().map(|v| v.to_string()).collect::<Vec<_>>());
                params
            }
            StreamIdx::Constraint(_) => Vec::new(),
        }
    }

    /// Return all active object parameter vectors for a world collection domain.
    pub(crate) fn fetch_domain_params(&self, domain: &str) -> Vec<Vec<Value>> {
        self.object_domains
            .get(domain)
            .map(|stream_ref| self.fetch_instance_params(*stream_ref))
            .unwrap_or_default()
    }

    /// Return the most recently stored value for this stream instance, or `Value::None`.
    pub(crate) fn read_held_value(&self, stream_ref: StreamIdx, params: &[Value]) -> Value {
        match stream_ref {
            StreamIdx::Signal(ix) => self
                .global_store
                .signal(ix)
                .read_at(0)
                .unwrap_or(Value::None),
            StreamIdx::Constraint(ix) => {
                if params.is_empty() {
                    self.global_store
                        .constraint(ix)
                        .read_at(0)
                        .unwrap_or(Value::None)
                } else {
                    self.global_store
                        .group(ix)
                        .slot(params)
                        .and_then(|i| i.read_at(0))
                        .unwrap_or(Value::None)
                }
            }
        }
    }

    /// Same as [`read_held_value`] but asserts that the stream was fresh this cycle (debug builds).
    pub(crate) fn read_held_value_strict(&self, stream_ref: StreamIdx, params: &[Value]) -> Value {
        let slot = match stream_ref {
            StreamIdx::Signal(ix) => {
                debug_assert!(
                    self.active_inputs.contains(ix),
                    "signal {} not fresh in strict access",
                    ix
                );
                self.global_store.signal(ix)
            }
            StreamIdx::Constraint(ix) => {
                debug_assert!(
                    self.active_outputs.contains(ix),
                    "output {} not fresh in strict access",
                    ix
                );
                if params.is_empty() {
                    self.global_store.constraint(ix)
                } else {
                    self.global_store
                        .group(ix)
                        .slot(params)
                        .expect("attempted strict access on a non-existent parameterized instance")
                }
            }
        };
        slot.read_at(0).unwrap_or(Value::None)
    }

    /// Resolve the buffer and freshness flag for a stream instance.
    fn resolve_instance_and_freshness(
        &self,
        stream_ref: StreamIdx,
        params: &[Value],
    ) -> (Option<&ValueBuffer>, bool) {
        match stream_ref {
            StreamIdx::Signal(ix) => (
                Some(self.global_store.signal(ix)),
                self.active_inputs.contains(ix),
            ),
            StreamIdx::Constraint(ix) => {
                if params.is_empty() {
                    (
                        Some(self.global_store.constraint(ix)),
                        self.active_outputs.contains(ix),
                    )
                } else {
                    let group = self.global_store.group(ix);
                    (group.slot(params), group.was_updated(params))
                }
            }
        }
    }

    /// Return `Value::Bool(true)` if the stream instance was updated this cycle.
    pub(crate) fn read_freshness(&self, stream_ref: StreamIdx, params: &[Value]) -> Value {
        let (_, fresh) = self.resolve_instance_and_freshness(stream_ref, params);
        Value::Bool(fresh)
    }

    /// Return the value at `offset` positions back in the history ring-buffer.
    ///
    /// Freshness is taken into account: if the instance was not updated this cycle,
    /// the logical offset is shifted by one to remain consistent with the cycle model.
    pub(crate) fn read_at_offset(
        &self,
        stream_ref: StreamIdx,
        params: &[Value],
        offset: i16,
    ) -> Value {
        let (slot, fresh) = self.resolve_instance_and_freshness(stream_ref, params);
        let slot = slot.expect("target stream instance must exist for offset access");
        let adjusted = if fresh { offset } else { offset + 1 };
        slot.read_at(adjusted).unwrap_or(Value::None)
    }

    /// Return the current-cycle value if the instance was updated, otherwise `Value::None`.
    pub(crate) fn read_current_value(&self, stream_ref: StreamIdx, params: &[Value]) -> Value {
        let (slot, fresh) = self.resolve_instance_and_freshness(stream_ref, params);
        if fresh {
            slot.expect("fresh instance must have a slot")
                .read_at(0)
                .expect("fresh stream must have a value in slot 0")
        } else {
            Value::None
        }
    }

    /// Return `true` when the given pacing gate is satisfied.
    pub(crate) fn gate_is_open(&self, gate: &PacingGate) -> bool {
        gate.is_satisfied(self.active_inputs)
    }

    /// Derive a new frame using `params` as the instance parameter slice.
    pub(crate) fn fork_with_params<'a>(&'a self, params: &'a [Value]) -> EvalFrame<'a> {
        EvalFrame {
            ts: self.ts,
            global_store: self.global_store,
            active_inputs: self.active_inputs,
            active_outputs: self.active_outputs,
            parameter: params,
            lambda_parameter: self.lambda_parameter,
            object_domains: self.object_domains,
            user_functions: self.user_functions,
        }
    }
}

/// Represents when a stream should be evaluated relative to incoming events.
///
/// The `Conjunction` variant enables a fast bitset-subset check for the
/// common case where the condition is a simple conjunction of input freshness
/// requirements.
#[derive(Debug)]
pub(crate) enum PacingGate {
    /// Stream is driven by a periodic clock, not by events.
    TimeDriven,
    /// Stream is always active (unconditional evaluation).
    True,
    /// Stream is active when all listed input indices are fresh.
    Conjunction(BitSet),
    /// General activation condition that requires recursive evaluation.
    General(Activation),
}

impl PacingGate {
    /// Build a [`PacingGate`] from a refined IR [`Activation`] condition.
    pub(crate) fn from_condition(ac: &Activation, n_inputs: usize) -> Self {
        if let Activation::True = ac {
            return PacingGate::True;
        }
        if let Activation::Conjunction(items) = ac {
            assert!(!items.is_empty());
            let indices: Vec<usize> = items
                .iter()
                .flat_map(|a| {
                    if let Activation::Stream(v) = a {
                        Some(v.in_ix())
                    } else {
                        None
                    }
                })
                .collect();
            if indices.len() == items.len() {
                let mut bs = BitSet::with_capacity(n_inputs);
                for i in indices {
                    bs.insert(i);
                }
                return PacingGate::Conjunction(bs);
            }
        }
        PacingGate::General(ac.clone())
    }

    /// Return `true` when this gate is satisfied given the set of currently-fresh inputs.
    pub(crate) fn is_satisfied(&self, active_signals: &BitSet) -> bool {
        match self {
            PacingGate::True => true,
            PacingGate::Conjunction(required) => required.is_subset(active_signals),
            PacingGate::General(ac) => Self::check_condition(ac, active_signals),
            PacingGate::TimeDriven => {
                unreachable!("time-driven gate should not be evaluated as an event gate")
            }
        }
    }

    fn check_condition(ac: &Activation, active_signals: &BitSet) -> bool {
        match ac {
            Activation::Stream(v) => active_signals.contains(v.in_ix()),
            Activation::Conjunction(items) => items
                .iter()
                .all(|a| Self::check_condition(a, active_signals)),
            Activation::Disjunction(items) => items
                .iter()
                .any(|a| Self::check_condition(a, active_signals)),
            Activation::True => unreachable!("True activation should be handled by the fast path"),
        }
    }

    /// Returns `true` if this gate responds to events (as opposed to being time-driven).
    pub(crate) fn is_event_driven(&self) -> bool {
        !matches!(self, PacingGate::TimeDriven)
    }
}

/// All data required to instantiate a [`RuntimeEngine`].
///
/// Constructed from a parsed [`OORVIR`] specification and a shared dynamic
/// schedule.  Call [`EngineSetup::into_engine`] to compile expressions and
/// produce the live engine.
pub(crate) struct EngineSetup {
    eval_layers: Vec<Vec<Task>>,
    stream_gates: Vec<PacingGate>,
    instance_start_gates: Vec<PacingGate>,
    instance_end_gates: Vec<PacingGate>,
    global_store: DataStore,
    active_inputs: BitSet,
    active_outputs: BitSet,
    started_outputs: BitSet,
    ended_outputs: BitSet,
    activated_constraints: BitSet,
    alarms: Vec<Option<Alarm>>,
    time_driven_streams: Vec<Option<PeriodicTaskStream>>,
    ending_streams: Vec<usize>,
    ir: OORVIR,
    dyn_schedule: Rc<RefCell<EventQueue>>,
}

impl EngineSetup {
    /// Build an [`EngineSetup`] from the compiled specification and a shared event queue.
    pub(crate) fn build(ir: OORVIR, dyn_schedule: Rc<RefCell<EventQueue>>) -> Self {
        let eval_layers: Vec<Vec<Task>> = ir.event_schedule_layers();

        let ending_streams = ir
            .constraints
            .iter()
            .filter(|s| s.end.condition.is_some())
            .map(|s| s.stream_idx.out_ix())
            .collect();

        let stream_gates = ir
            .constraints
            .iter()
            .map(|o| match &o.eval.eval_pacing {
                PacingNode::GlobalTick(_) | PacingNode::LocalTick(_) => PacingGate::TimeDriven,
                PacingNode::Event(ac) => PacingGate::from_condition(ac, ir.signals.len()),
                PacingNode::Constant => PacingGate::True,
            })
            .collect();

        let instance_start_gates = ir
            .constraints
            .iter()
            .map(|o| match &o.start.pacing {
                PacingNode::GlobalTick(_) | PacingNode::LocalTick(_) => PacingGate::TimeDriven,
                PacingNode::Event(ac) => PacingGate::from_condition(ac, ir.signals.len()),
                PacingNode::Constant => PacingGate::True,
            })
            .collect();

        let instance_end_gates = ir
            .constraints
            .iter()
            .map(|o| match &o.end.pacing {
                PacingNode::GlobalTick(_) | PacingNode::LocalTick(_) => PacingGate::TimeDriven,
                PacingNode::Event(ac) => PacingGate::from_condition(ac, ir.signals.len()),
                PacingNode::Constant => PacingGate::True,
            })
            .collect();

        let global_store = DataStore::build(&ir);
        let n_signals = ir.signals.len();
        let n_constraints = ir.constraints.len();

        let mut alarms = vec![None; n_constraints];
        for t in &ir.alarms {
            alarms[t.constrain_idx.out_ix()] = Some(*t);
        }

        let mut time_driven_streams = vec![None; n_constraints];
        for t in &ir.time_task {
            time_driven_streams[t.stream_idx.out_ix()] = Some(*t);
        }

        EngineSetup {
            eval_layers,
            stream_gates,
            instance_start_gates,
            instance_end_gates,
            global_store,
            active_inputs: BitSet::with_capacity(n_signals),
            active_outputs: BitSet::with_capacity(n_constraints),
            started_outputs: BitSet::with_capacity(n_constraints),
            ended_outputs: BitSet::with_capacity(n_constraints),
            activated_constraints: BitSet::with_capacity(n_constraints),
            alarms,
            time_driven_streams,
            ending_streams,
            ir,
            dyn_schedule,
        }
    }

    /// Compile all stream expressions and transfer ownership to a [`RuntimeEngine`].
    ///
    /// The setup data is heap-allocated and its lifetime extended via `Box::leak`
    /// so that the static references held by `RuntimeEngine` remain valid.  The
    /// raw pointer stored in `setup_ptr` is used by `Drop` to reclaim the memory.
    pub(crate) fn into_engine(self) -> RuntimeEngine {
        let mut boxed = Box::new(self);
        let setup_ptr: *mut EngineSetup = &mut *boxed;
        let leaked: &'static mut EngineSetup = Box::leak(boxed);

        let user_func_map: HashMap<String, BoundExpr> = leaked
            .ir
            .func_bodies
            .iter()
            .map(|(name, expr)| (name.clone(), expr.clone().lower()))
            .collect();
        let user_func_registry: &'static HashMap<String, BoundExpr> =
            Box::leak(Box::new(user_func_map));

        let stream_evaluators = leaked.ir.constraints.iter().map(|o| {
            let clause_exprs: Vec<BoundExpr> = o.eval.decls.iter().map(|clause| {
                let base_expr = match &clause.condition {
                    None => clause.expression.clone().lower(),
                    Some(guard) => {
                        use crate::oorvir::refined::ExprVariant as EK;
                        match &guard.kind {
                            EK::Quantified { .. } => BoundExpr::guarded_quantified(guard.clone(), clause.expression.clone()),
                            _ => BoundExpr::guarded(guard.clone().lower(), clause.expression.clone().lower()),
                        }
                    }
                };
                if clause.pacing != o.eval.eval_pacing {
                    match &clause.pacing {
                        PacingNode::Event(ac) => BoundExpr::gated(
                            base_expr,
                            PacingGate::from_condition(ac, leaked.ir.signals.len()),
                        ),
                        _ => unreachable!(
                            "mixed pacing of eval clauses is only supported for event-driven streams"
                        ),
                    }
                } else {
                    base_expr
                }
            }).collect();

            if clause_exprs.len() == 1 {
                clause_exprs.into_iter().next().expect("exactly one element")
            } else {
                BoundExpr::first_match(clause_exprs)
            }
        }).collect();

        let start_evaluators = leaked
            .ir
            .constraints
            .iter()
            .map(
                |o| match (o.start.expression.as_ref(), o.start.condition.as_ref()) {
                    (None, None) => BoundExpr::wrap(|_| Value::Tuple(vec![].into_boxed_slice())),
                    (Some(target), None) => target.clone().lower(),
                    (None, Some(cond)) => BoundExpr::guarded(
                        cond.clone().lower(),
                        BoundExpr::wrap(|_| Value::Tuple(vec![].into_boxed_slice())),
                    ),
                    (Some(target), Some(cond)) => {
                        BoundExpr::guarded(cond.clone().lower(), target.clone().lower())
                    }
                },
            )
            .collect();

        let end_evaluators = leaked
            .ir
            .constraints
            .iter()
            .map(|o| {
                o.end
                    .condition
                    .as_ref()
                    .map_or_else(|| BoundExpr::wrap(|_| Value::None), |e| e.clone().lower())
            })
            .collect();

        RuntimeEngine {
            eval_layers: &leaked.eval_layers,
            stream_gates: &leaked.stream_gates,
            instance_start_gates: &leaked.instance_start_gates,
            instance_end_gates: &leaked.instance_end_gates,
            stream_evaluators,
            start_evaluators,
            end_evaluators,
            user_func_registry,
            global_store: &mut leaked.global_store,
            active_inputs: &mut leaked.active_inputs,
            active_outputs: &mut leaked.active_outputs,
            started_outputs: &mut leaked.started_outputs,
            ended_outputs: &mut leaked.ended_outputs,
            activated_constraints: &mut leaked.activated_constraints,
            alarms: &leaked.alarms,
            time_driven_streams: &leaked.time_driven_streams,
            ending_streams: &leaked.ending_streams,
            ir: &leaked.ir,
            dyn_schedule: &leaked.dyn_schedule,
            setup_ptr,
        }
    }
}

/// The live stream evaluation engine produced by [`EngineSetup::into_engine`].
///
/// Drives per-event and time-driven evaluation loops, manages the global
/// value store, and exposes read-only snapshots of the current cycle's results.
#[allow(missing_debug_implementations)]
pub(crate) struct RuntimeEngine {
    eval_layers: &'static [Vec<Task>],
    stream_gates: &'static [PacingGate],
    instance_start_gates: &'static [PacingGate],
    instance_end_gates: &'static [PacingGate],
    stream_evaluators: Vec<BoundExpr>,
    start_evaluators: Vec<BoundExpr>,
    end_evaluators: Vec<BoundExpr>,
    user_func_registry: &'static HashMap<String, BoundExpr>,
    global_store: &'static mut DataStore,
    active_inputs: &'static mut BitSet,
    active_outputs: &'static mut BitSet,
    started_outputs: &'static mut BitSet,
    ended_outputs: &'static mut BitSet,
    activated_constraints: &'static mut BitSet,
    alarms: &'static [Option<Alarm>],
    time_driven_streams: &'static [Option<PeriodicTaskStream>],
    ending_streams: &'static [usize],
    ir: &'static OORVIR,
    dyn_schedule: &'static RefCell<EventQueue>,
    setup_ptr: *mut EngineSetup,
}

impl Drop for RuntimeEngine {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        drop(unsafe { Box::from_raw(self.setup_ptr) });
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl RuntimeEngine {
    /// Process one input event at timestamp `ts`.
    pub(crate) fn process_event(
        &mut self,
        event: &[Value],
        ts: Duration,
        tracer: &mut impl EvalTracer,
    ) {
        self.begin_cycle(ts);
        self.ingest_event_values(event, ts);
        self.run_event_layers(ts, tracer);
    }

    /// Snapshot the output changes produced in the current cycle.
    pub(crate) fn snapshot_output_changes(&self) -> Vec<(usize, Vec<Change>)> {
        self.ir
            .constraints
            .iter()
            .filter_map(|o| {
                let stream = o.stream_idx;
                let out_ix = o.stream_idx.out_ix();
                let changes: Vec<Change> = if o.is_parameter() {
                    let group = self.global_store.group(out_ix);
                    group
                        .added_params()
                        .map(|p| Change::Activate(p.clone()))
                        .chain(group.updated_params().map(|p| {
                            Change::Update(
                                Some(p.clone()),
                                self.read_stream_value(stream, p, 0)
                                    .expect("marked as active"),
                            )
                        }))
                        .chain(
                            group
                                .removed_params()
                                .map(|p| Change::Deactivate(p.clone())),
                        )
                        .collect()
                } else if o.is_start() {
                    let mut res = Vec::new();
                    if self.started_outputs.contains(out_ix) {
                        res.push(Change::Activate(vec![]));
                    }
                    if self.active_outputs.contains(out_ix) {
                        res.push(Change::Update(
                            Some(vec![]),
                            self.read_stream_value(stream, &[], 0)
                                .expect("marked as active"),
                        ));
                    }
                    if self.ended_outputs.contains(out_ix) {
                        res.push(Change::Deactivate(vec![]));
                    }
                    res
                } else if self.active_outputs.contains(out_ix) {
                    vec![Change::Update(
                        None,
                        self.read_stream_value(stream, &[], 0)
                            .expect("marked as active"),
                    )]
                } else {
                    vec![]
                };
                changes.is_empty().not().then(|| (out_ix, changes))
            })
            .collect()
    }

    /// Return all alarm messages generated during the current cycle.
    pub(crate) fn snapshot_alarm_messages(&self) -> Vec<(usize, InstanceKey, String)> {
        self.snapshot_output_changes()
            .into_iter()
            .filter(|(idx, _)| matches!(self.ir.constraints[*idx].kind, ConstraintKind::Alarm(_)))
            .flat_map(|(idx, changes)| {
                changes.into_iter().filter_map(move |ch| match ch {
                    Change::Update(params, Value::Str(msg)) => Some((idx, params, msg.into())),
                    Change::Update(_, _) => unreachable!("alarm values must be strings"),
                    _ => None,
                })
            })
            .collect()
    }

    /// Return the indices of constraints that were activated (violated) this cycle.
    pub(crate) fn snapshot_active_constraints(&self) -> Vec<usize> {
        self.activated_constraints.iter().collect()
    }

    /// Return the input signals that received a new value this cycle.
    pub(crate) fn snapshot_new_inputs(&self) -> Vec<(usize, Value)> {
        self.active_inputs
            .iter()
            .map(|i| {
                (
                    i,
                    self.read_stream_value(StreamIdx::Signal(i), &[], 0)
                        .expect("marked as active"),
                )
            })
            .collect()
    }

    /// Return the current value (or `None`) of every input signal.
    pub(crate) fn snapshot_all_inputs(&self) -> Vec<Option<Value>> {
        self.ir
            .signals
            .iter()
            .map(|s| self.read_stream_value(s.stream_idx, &[], 0))
            .collect()
    }

    /// Return the current instances (parameter key + value) of every output constraint.
    pub(crate) fn snapshot_all_outputs(&self) -> Vec<Vec<StreamInstance>> {
        self.ir
            .constraints
            .iter()
            .map(|elem| {
                if elem.is_parameter() {
                    let ix = elem.stream_idx.out_ix();
                    self.global_store
                        .group(ix)
                        .params()
                        .map(|para| {
                            (
                                Some(para.clone()),
                                self.read_stream_value(elem.stream_idx, para, 0),
                            )
                        })
                        .collect()
                } else if elem.is_start() {
                    vec![(
                        Some(vec![]),
                        self.read_stream_value(elem.stream_idx, &[], 0),
                    )]
                } else {
                    vec![(None, self.read_stream_value(elem.stream_idx, &[], 0))]
                }
            })
            .collect()
    }

    /// Evaluate all time-driven tasks at the given timestamp.
    pub(crate) fn process_timed_tasks(
        &mut self,
        tasks: Vec<WorkItem>,
        ts: Duration,
        tracer: &mut impl EvalTracer,
    ) {
        if tasks.is_empty() {
            return;
        }
        self.begin_cycle(ts);
        for task in tasks {
            match task {
                WorkItem::Compute(idx, params) => {
                    tracer.instance_begin(idx, params.as_slice());
                    self.run_stream_instance(idx, params.as_slice(), ts);
                    tracer.instance_end(idx, params.as_slice());
                }
                WorkItem::ComputeAll(idx) => {
                    self.run_all_instances(idx, ts, tracer);
                }
                WorkItem::Activate(idx) => {
                    tracer.activation_begin(idx);
                    self.activate_instance(idx, ts);
                    tracer.activation_end(idx);
                }
                WorkItem::Deactivate(idx, params) => {
                    tracer.deactivation_begin(idx, params.as_slice());
                    self.deactivate_instance(idx, params.as_slice(), ts);
                    tracer.deactivation_end(idx, params.as_slice());
                }
                WorkItem::DeactivateAll(idx) => {
                    self.deactivate_all_instances(idx, ts, tracer);
                }
            }
        }
    }
}

impl RuntimeEngine {
    fn ingest_event_values(&mut self, event: &[Value], ts: Duration) {
        for (ix, v) in event.iter().enumerate() {
            if !matches!(v, Value::None) {
                self.ingest_single_value(ix, v.clone(), ts);
            }
        }
    }

    fn ingest_single_value(&mut self, signal_ix: usize, v: Value, _ts: Duration) {
        self.global_store.signal_mut(signal_ix).write(v);
        self.active_inputs.insert(signal_ix);
    }

    fn run_event_layers(&mut self, ts: Duration, tracer: &mut impl EvalTracer) {
        for layer in self.eval_layers {
            self.run_event_layer(layer, ts, tracer);
        }
        for &end_idx in self.ending_streams {
            let gate = &self.instance_end_gates[end_idx];
            if gate.is_event_driven() && gate.is_satisfied(self.active_inputs) {
                self.deactivate_all_instances(end_idx, ts, tracer);
            }
        }
    }

    fn run_event_layer(&mut self, tasks: &[Task], ts: Duration, tracer: &mut impl EvalTracer) {
        for task in tasks {
            match task {
                Task::Evaluate(idx) => self.run_event_output(*idx, ts, tracer),
                Task::Start(idx) => self.run_event_start(*idx, ts, tracer),
                Task::End(_) => {
                    unreachable!("instance-end tasks are not placed in evaluation layers")
                }
            }
        }
    }

    fn run_event_output(&mut self, output: usize, ts: Duration, tracer: &mut impl EvalTracer) {
        if self.stream_gates[output].is_satisfied(self.active_inputs) {
            self.run_all_instances(output, ts, tracer);
        }
    }

    fn run_event_start(&mut self, output: usize, ts: Duration, tracer: &mut impl EvalTracer) {
        if self.instance_start_gates[output].is_satisfied(self.active_inputs) {
            tracer.activation_begin(output);
            self.activate_instance(output, ts);
            tracer.activation_end(output);
        }
    }

    fn run_all_instances(&mut self, output: usize, ts: Duration, tracer: &mut impl EvalTracer) {
        if self
            .ir
            .constraint(StreamIdx::Constraint(output))
            .is_parameter()
        {
            let params: Vec<Vec<Value>> =
                self.global_store.group(output).params().cloned().collect();
            for inst in params {
                tracer.instance_begin(output, inst.as_slice());
                self.run_stream_instance(output, inst.as_slice(), ts);
                tracer.instance_end(output, inst.as_slice());
            }
        } else if self.global_store.constraint(output).enabled() {
            tracer.instance_begin(output, &[]);
            self.run_stream_instance(output, &[], ts);
            tracer.instance_end(output, &[]);
        }
    }

    fn run_stream_instance(&mut self, output: usize, params: &[Value], ts: Duration) {
        let expr = self.stream_evaluators[output].clone();
        let frame = self.make_eval_frame(params, ts);
        let result = expr.run(&frame);

        if matches!(result, Value::None) {
            return;
        }

        let is_parameterized = self.ir.constraints[output].is_parameter();
        let slot = if is_parameterized {
            self.global_store
                .group_mut(output)
                .slot_mut(params)
                .expect("attempted to evaluate a non-existent parameterized instance")
        } else {
            self.global_store.constraint_mut(output)
        };
        slot.write(result);
        self.active_outputs.insert(output);

        if let Some(alarm) = self.find_alarm(output) {
            self.activated_constraints.insert(alarm.alarm_idx);
        }
    }

    fn activate_instance(&mut self, output: usize, ts: Duration) {
        let stream = self.ir.constraint(StreamIdx::Constraint(output));
        debug_assert!(
            stream.is_start(),
            "activate_instance called on a non-startable stream"
        );

        let expr = self.start_evaluators[output].clone();
        let empty: Vec<Value> = vec![];
        let frame = self.make_eval_frame(&empty, ts);
        let result = expr.run(&frame);

        let instance_params: Vec<Value> = match result {
            Value::None => return,
            Value::Tuple(p) => p.to_vec(),
            v => vec![v],
        };

        if stream.is_parameter() {
            debug_assert!(!instance_params.is_empty());
            let group = self.global_store.group_mut(output);
            if group.has_slot(instance_params.as_slice()) {
                return;
            }
            group.register(instance_params.as_slice());
        } else {
            debug_assert!(instance_params.is_empty());
            let slot = self.global_store.constraint_mut(output);
            if slot.enabled() {
                return;
            }
            slot.enable();
        }

        if let Some(tds) = self.time_driven_streams[output] {
            let mut sched = (*self.dyn_schedule).borrow_mut();

            if tds.locality == PacingLocality::Local {
                sched.enqueue_compute(
                    output,
                    instance_params.as_slice(),
                    ts,
                    tds.period_as_duration(),
                );
            }

            if let PacingNode::LocalTick(freq) = stream.end.pacing {
                let period = Duration::from_nanos(
                    UOM_Time::new::<uom::si::time::second>(
                        freq.get::<uom::si::frequency::hertz>().inv(),
                    )
                    .get::<nanosecond>()
                    .to_integer()
                    .try_into()
                    .expect("stream period exceeds u64 nanoseconds"),
                );
                sched.enqueue_end(output, instance_params.as_slice(), ts, period);
            }
        }

        self.started_outputs.insert(output);
    }

    fn deactivate_instance(&mut self, output: usize, params: &[Value], ts: Duration) {
        let stream = self.ir.constraint(StreamIdx::Constraint(output));
        let expr = self.end_evaluators[output].clone();
        let frame = self.make_eval_frame(params, ts);

        if !expr.run(&frame).boolean_value() {
            return;
        }

        if stream.is_parameter() {
            self.global_store.group_mut(output).schedule_removal(params);
        }
        self.ended_outputs.insert(output);

        if let Some(tds) = self.time_driven_streams[output] {
            let mut sched = (*self.dyn_schedule).borrow_mut();
            sched.cancel_compute(output, params, tds.period_as_duration());

            if let PacingNode::LocalTick(freq) = stream.end.pacing {
                let period = Duration::from_nanos(
                    UOM_Time::new::<uom::si::time::second>(
                        freq.get::<uom::si::frequency::hertz>().inv(),
                    )
                    .get::<nanosecond>()
                    .to_integer()
                    .try_into()
                    .expect("stream period exceeds u64 nanoseconds"),
                );
                sched.cancel_end(output, params, period);
            }
        }
    }

    fn deactivate_all_instances(
        &mut self,
        output: usize,
        ts: Duration,
        tracer: &mut impl EvalTracer,
    ) {
        if self
            .ir
            .constraint(StreamIdx::Constraint(output))
            .is_parameter()
        {
            let params: Vec<Vec<Value>> =
                self.global_store.group(output).params().cloned().collect();
            for inst in params {
                tracer.deactivation_begin(output, inst.as_slice());
                self.deactivate_instance(output, inst.as_slice(), ts);
                tracer.deactivation_end(output, inst.as_slice());
            }
        } else if self.global_store.constraint(output).enabled() {
            tracer.deactivation_begin(output, &[]);
            self.deactivate_instance(output, &[], ts);
            tracer.deactivation_end(output, &[]);
        }
    }

    fn flush_ended_streams(&mut self) {
        for o in self.ended_outputs.iter() {
            if self.ir.constraint(StreamIdx::Constraint(o)).is_parameter() {
                let _removed = self.global_store.group_mut(o).flush_removed();
            } else {
                self.global_store.constraint_mut(o).disable();
            }
        }
    }

    fn begin_cycle(&mut self, _ts: Duration) {
        self.flush_ended_streams();
        self.active_inputs.clear();
        self.active_outputs.clear();
        self.activated_constraints.clear();
        self.started_outputs.clear();
        self.ended_outputs.clear();
        self.global_store.next_cycle();
    }

    fn find_alarm(&self, output: usize) -> Option<&Alarm> {
        self.alarms[output].as_ref()
    }

    fn read_stream_value(&self, sr: StreamIdx, args: &[Value], offset: i16) -> Option<Value> {
        match sr {
            StreamIdx::Signal(ix) => {
                debug_assert!(args.is_empty());
                self.global_store.signal(ix).read_at(offset)
            }
            StreamIdx::Constraint(ix) => {
                if self.ir.resolve_stream(sr).is_parameter() {
                    debug_assert!(!args.is_empty());
                    self.global_store
                        .group(ix)
                        .slot(args)
                        .and_then(|i| i.read_at(offset))
                } else {
                    self.global_store.constraint(ix).read_at(offset)
                }
            }
        }
    }

    fn make_eval_frame<'a>(&'a mut self, params: &'a [Value], ts: Duration) -> EvalFrame<'a> {
        EvalFrame {
            ts,
            global_store: self.global_store,
            active_inputs: self.active_inputs,
            active_outputs: self.active_outputs,
            parameter: params,
            lambda_parameter: None,
            object_domains: &self.ir.object_domains,
            user_functions: self.user_func_registry,
        }
    }
}
