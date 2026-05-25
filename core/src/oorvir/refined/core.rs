use std::collections::HashMap;
use std::convert::TryInto;
use std::time::Duration;

use crate::oorvir::source::{
    AccessSite, ConstraintKind, DataType, LayerIndex, StorageRequirement, StreamIdx, StreamLayer,
};
use num::traits::Inv;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize, Serializer};
use uom::si::rational64::{Frequency as UOM_Frequency, Time as UOM_Time};
use uom::si::time::nanosecond;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OORVIR {
    pub signals: Vec<SignalStream>,
    pub constraints: Vec<ConstraintStream>,
    pub object_domains: HashMap<String, StreamIdx>,
    pub alarms: Vec<Alarm>,
    pub time_task: Vec<PeriodicTaskStream>,
    pub event_task: Vec<EventTaskStream>,
    pub func_bodies: HashMap<String, Expression>,
}

impl Default for OORVIR {
    // Produce an empty IR with no streams, alarms, or function bodies.
    fn default() -> Self {
        let empty_signals = Vec::new();
        let empty_constraints = Vec::new();
        let empty_alarms = Vec::new();
        let empty_time = Vec::new();
        let empty_events = Vec::new();
        let empty_funcs = HashMap::new();
        let empty_domains = HashMap::new();
        OORVIR {
            signals: empty_signals,
            constraints: empty_constraints,
            object_domains: empty_domains,
            alarms: empty_alarms,
            time_task: empty_time,
            event_task: empty_events,
            func_bodies: empty_funcs,
        }
    }
}

pub trait Stream {
    fn start_layer(&self) -> LayerIndex;
    fn eval_layer(&self) -> LayerIndex;
    fn name(&self) -> &str;
    fn annotation(&self) -> &Type;
    fn is_signal(&self) -> bool;
    fn is_parameter(&self) -> bool;
    fn is_start(&self) -> bool;
    fn is_end(&self) -> bool;
    fn is_filter(&self) -> bool;
    fn required_storage_bound(&self) -> StorageRequirement;
    fn stream_idx(&self) -> StreamIdx;
    fn consumers(&self) -> &Accesses;
}

// The concrete value type of a stream or expression in refined_ir.
// Pacing information is encoded separately in PeriodicTaskStream and EventTaskStream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Type {
    // Two-valued logical type.
    Bool,
    // Fixed-width signed integer.
    Int(IntTy),
    // Fixed-width unsigned integer.
    UInt(UIntTy),
    // IEEE 754 floating-point number.
    Float(FloatTy),
    // Signed fixed-point number.
    Fixed(FixedTy),
    // Unsigned fixed-point number.
    UFixed(FixedTy),
    // Unicode text string.
    String,
    // Raw byte sequence.
    Bytes,
    // Heterogeneous tuple; length determined by the inner vec.
    Tuple(Vec<Type>),
    // Nullable wrapper around an inner type.
    Option(Box<Type>),
    // Monomorphized function type carrying argument and return types.
    Function {
        // Resolved argument types.
        args: Vec<Type>,
        // Resolved return type.
        ret: Box<Type>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PacingNode {
    GlobalTick(UOM_Frequency),
    LocalTick(UOM_Frequency),
    Event(ActivationCondition),
    Constant,
}

// Discriminates the bit-width of a signed integer type.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntTy {
    Int8,
    Int16,
    Int32,
    Int64,
    Int128,
    Int256,
}

// Discriminates the bit-width of an unsigned integer type.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UIntTy {
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    UInt128,
    UInt256,
}

// Discriminates between 32-bit and 64-bit floating-point representations.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FloatTy {
    Float32,
    Float64,
}

// Discriminates fixed-point formats; notation is TotalBits_IntegerBits.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FixedTy {
    Fixed64_32,
    Fixed32_16,
    Fixed16_8,
}

impl From<DataType> for Type {
    // Convert a source_ir DataType to its refined_ir Type counterpart.
    // Unsupported kinds (e.g. raw numeric literals) must be resolved before this point.
    fn from(src: DataType) -> Type {
        use DataType as D;
        match src {
            D::Integer8 => Type::Int(IntTy::Int8),
            D::Integer16 => Type::Int(IntTy::Int16),
            D::Integer32 => Type::Int(IntTy::Int32),
            D::Integer64 => Type::Int(IntTy::Int64),
            D::UInteger8 => Type::UInt(UIntTy::UInt8),
            D::UInteger16 => Type::UInt(UIntTy::UInt16),
            D::UInteger32 => Type::UInt(UIntTy::UInt32),
            D::UInteger64 => Type::UInt(UIntTy::UInt64),
            D::Float32 => Type::Float(FloatTy::Float32),
            D::Float64 => Type::Float(FloatTy::Float64),
            D::Tuple(elems) => Type::Tuple(elems.into_iter().map(Type::from).collect()),
            D::TString => Type::String,
            D::Byte => Type::Bytes,
            D::Option(inner) => Type::Option(Box::new(Type::from(*inner))),
            other => unreachable!(
                "DataType {:?} cannot be converted to refined_ir Type ?must be resolved earlier",
                other
            ),
        }
    }
}

type Accesses = Vec<(StreamIdx, Vec<(AccessSite, AccessMode)>)>;

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct SignalStream {
    pub name: String,
    pub annotation: Type,
    pub consumers: Accesses,
    pub stream_level: StreamLayer,
    pub storage_bound: StorageRequirement,
    pub stream_idx: StreamIdx,
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct ConstraintStream {
    pub name: String,
    pub kind: ConstraintKind,
    pub annotation: Type,
    pub start: Start,
    pub eval: Eval,
    pub end: End,
    pub dependencies: Accesses,
    pub consumers: Accesses,
    pub storage_bound: StorageRequirement,
    pub stream_level: StreamLayer,
    pub stream_idx: StreamIdx,
    pub params: Vec<ParamDecl>,
    pub level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Copy)]
pub struct Alarm {
    pub constrain_idx: StreamIdx,
    pub alarm_idx: usize,
}

impl ConstraintStream {
    // Returns true when this constraint stream represents an alarm output.
    fn is_constrain(&self) -> bool {
        let kind_ref = &self.kind;
        matches!(kind_ref, ConstraintKind::Alarm(_))
    }
}

// Carries the initialization expression, pacing, and optional guard condition for a stream's start clause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Start {
    pub expression: Option<Expression>,
    pub pacing: PacingNode,
    pub condition: Option<Expression>,
}

impl Default for Start {
    // Build a Start with no initializer expression, no condition, and a constant pacing.
    fn default() -> Self {
        let no_expr = None;
        let no_cond = None;
        Start {
            expression: no_expr,
            pacing: PacingNode::Constant,
            condition: no_cond,
        }
    }
}

// Carries the optional termination condition and pacing for a stream's end clause.
// `has_self_idx` indicates that the condition references the stream's own current instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct End {
    pub condition: Option<Expression>,
    pub pacing: PacingNode,
    pub has_self_idx: bool,
}

impl Default for End {
    // Build an End with no termination condition and a constant pacing.
    fn default() -> Self {
        End {
            condition: None,
            pacing: PacingNode::Constant,
            has_self_idx: false,
        }
    }
}

// Collected eval clauses plus a stream-level aggregated pacing node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Eval {
    pub decls: Vec<EvalDecls>,
    pub eval_pacing: PacingNode,
}

// A single eval clause: one expression, an optional guard condition, and the clause's pacing node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalDecls {
    pub condition: Option<Expression>,
    pub expression: Expression,
    pub pacing: PacingNode,
}

// Describes a single named parameter of a parameterized output stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamDecl {
    pub name: String,
    pub annotation: Type,
    pub idx: usize,
}

// Associates a constraint stream with a periodic clock: stores the stream index,
// evaluation frequency, and whether the clock is relative to a global or local origin.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
pub struct PeriodicTaskStream {
    // Index of the associated constraint stream.
    pub stream_idx: StreamIdx,
    // The rate at which the stream is evaluated.
    pub frequency: UOM_Frequency,
    // Whether the frequency is measured from a global or instance-local clock origin.
    pub locality: PacingLocality,
}

// Determines whether periodic timing is measured from a shared global clock or
// from a per-instance local start time.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum PacingLocality {
    // Timing reference is the global monotonic clock.
    Global,
    // Timing reference is the local start time of the stream instance.
    Local,
}

impl PeriodicTaskStream {
    // Computes the reciprocal of the stored frequency to obtain the evaluation period.
    pub fn evaluation_period(&self) -> UOM_Time {
        let freq_hz = self.frequency.get::<uom::si::frequency::hertz>();
        UOM_Time::new::<uom::si::time::second>(freq_hz.inv())
    }

    // Returns the stored evaluation frequency directly.
    pub fn eval_frequency(&self) -> UOM_Frequency {
        self.frequency
    }

    // Converts the evaluation period to a standard-library Duration (nanosecond precision).
    pub fn period_as_duration(&self) -> Duration {
        let period_ns = self
            .evaluation_period()
            .get::<nanosecond>()
            .to_integer()
            .try_into()
            .expect("period nanosecond value overflows u64");
        Duration::from_nanos(period_ns)
    }
}

// Associates a constraint stream with an event-based activation condition.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct EventTaskStream {
    // Index of the associated constraint stream.
    pub stream_idx: StreamIdx,
    // Condition that must hold for the stream to be evaluated at a given event.
    pub ac: ActivationCondition,
}

// Describes when an event-driven stream or start condition should fire.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub enum ActivationCondition {
    // Fires only when every listed sub-condition is satisfied simultaneously.
    Conjunction(Vec<Self>),
    // Fires when at least one listed sub-condition is satisfied.
    Disjunction(Vec<Self>),
    // Fires when the identified stream is updated in the current evaluation step.
    Stream(StreamIdx),
    // Unconditionally satisfied; fires at every event.
    True,
}

// A typed expression node: a variant describing the computation kind plus the result type.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct Expression {
    // The computation kind encoded by this expression node.
    pub kind: ExprVariant,
    // The refined_ir type of the value produced by this expression.
    pub annotation: Type,
}

// Enumerates all expression forms that can appear in refined_ir.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub enum ExprVariant {
    LoadConstant(Constant),
    ArithLog(ArithLogOp, Vec<Expression>),
    StreamAccess {
        target: StreamIdx,
        parameters: Vec<Expression>,
        access_kind: AccessMode,
    },
    ParameterAccess(StreamIdx, usize),
    FunctionParameterAccess(usize),
    Ite {
        condition: Box<Expression>,
        consequence: Box<Expression>,
        alternative: Box<Expression>,
    },
    Tuple(Vec<Expression>),
    TupleAccess(Box<Expression>, usize),
    Function(String, Vec<Expression>),
    Convert {
        expr: Box<Expression>,
    },
    Default {
        expr: Box<Expression>,
        default: Box<Expression>,
    },
    Quantified {
        quantifier: Quantifier,
        bindings1: Vec<String>,
        bindings2: Vec<String>,
        body: Box<Expression>,
    },
    QuantifiedVar(String),
}

// Universal (Forall) and existential (Exists) quantifiers for quantified stream expressions.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub enum Quantifier {
    Forall,
    Exists,
}

// A compile-time constant value.
// Note: the stored numeric type may be wider than the declared stream type,
// e.g. Constant::UInt may represent a UInt8 stream value.
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub enum Constant {
    #[allow(missing_docs)]
    Str(String),
    #[allow(missing_docs)]
    Bool(bool),
    #[allow(missing_docs)]
    UInt(u64),
    #[allow(missing_docs)]
    Int(i64),
    #[allow(missing_docs)]
    Float(f64),
    #[allow(missing_docs)]
    Decimal(Decimal),
}

// All arithmetic and logical operators supported in refined_ir expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

// Selects which instances participate in an aggregation operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InstanceSelection {
    // Only instances updated in the current cycle.
    Fresh,
    // Every known instance, regardless of update status.
    All,
    // Updated instances satisfying the given filter condition.
    FilteredFresh {
        // Lambda bindings for the filter expression.
        parameters: Vec<ParamDecl>,
        // Guard expression each candidate instance must satisfy.
        cond: Box<Expression>,
    },
    // All instances satisfying the given filter condition.
    FilteredAll {
        // Lambda bindings for the filter expression.
        parameters: Vec<ParamDecl>,
        // Guard expression each candidate instance must satisfy.
        cond: Box<Expression>,
    },
}

impl InstanceSelection {
    // Returns the filter expression for filtered variants, or None for unfiltered selections.
    pub fn filter_condition(&self) -> Option<&Expression> {
        match self {
            InstanceSelection::Fresh | InstanceSelection::All => None,
            InstanceSelection::FilteredFresh { cond, .. }
            | InstanceSelection::FilteredAll { cond, .. } => Some(cond.as_ref()),
        }
    }

    // Returns the lambda parameter list for filtered variants, or None for unfiltered selections.
    pub fn filter_params(&self) -> Option<&Vec<ParamDecl>> {
        match self {
            InstanceSelection::Fresh | InstanceSelection::All => None,
            InstanceSelection::FilteredFresh { parameters, .. }
            | InstanceSelection::FilteredAll { parameters, .. } => Some(parameters),
        }
    }
}

// Aggregation operations applicable over a set of stream instances.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash, Serialize, Deserialize)]
pub enum InstanceOperation {
    Count,
    Min,
    Max,
    Sum,
    Product,
    Average,
    Conjunction,
    Disjunction,
    Variance,
    Covariance,
    StandardDeviation,
    NthPercentile(u8),
}

// ─────────────────────────────── Stream trait implementations ───────────────────────
impl Stream for ConstraintStream {
    // Evaluation layer of the start clause.
    fn start_layer(&self) -> LayerIndex {
        self.stream_level.start_layer()
    }

    // Evaluation layer of the eval clause.
    fn eval_layer(&self) -> LayerIndex {
        self.stream_level.eval_layer()
    }

    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn annotation(&self) -> &Type {
        &self.annotation
    }

    // Constraint streams are not signal streams.
    fn is_signal(&self) -> bool {
        false
    }

    // A constraint stream is parameterized when it has a start expression.
    fn is_parameter(&self) -> bool {
        self.start.expression.is_some()
    }

    // A stream has start behavior when any part of the start clause is non-trivial.
    fn is_start(&self) -> bool {
        let has_expr = self.start.expression.is_some();
        let has_cond = self.start.condition.is_some();
        let has_pacing = self.start.pacing != PacingNode::Constant;
        has_expr || has_cond || has_pacing
    }

    // A stream has end behavior when a termination condition is present.
    fn is_end(&self) -> bool {
        self.end.condition.is_some()
    }

    // A stream is filtered when at least one eval clause carries a guard condition.
    fn is_filter(&self) -> bool {
        self.eval
            .decls
            .iter()
            .any(|clause| clause.condition.is_some())
    }

    fn required_storage_bound(&self) -> StorageRequirement {
        self.storage_bound
    }

    fn stream_idx(&self) -> StreamIdx {
        self.stream_idx
    }

    fn consumers(&self) -> &Accesses {
        &self.consumers
    }
}

impl Stream for SignalStream {
    fn start_layer(&self) -> LayerIndex {
        self.stream_level.start_layer()
    }

    fn eval_layer(&self) -> LayerIndex {
        self.stream_level.eval_layer()
    }

    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn annotation(&self) -> &Type {
        &self.annotation
    }

    // Signal streams are always signal streams.
    fn is_signal(&self) -> bool {
        true
    }

    // Signal streams carry no parameterization.
    fn is_parameter(&self) -> bool {
        false
    }

    // Signal streams have no start clause.
    fn is_start(&self) -> bool {
        false
    }

    // Signal streams have no end clause.
    fn is_end(&self) -> bool {
        false
    }

    // Signal streams carry no filter.
    fn is_filter(&self) -> bool {
        false
    }

    fn required_storage_bound(&self) -> StorageRequirement {
        self.storage_bound
    }

    fn stream_idx(&self) -> StreamIdx {
        self.stream_idx
    }

    fn consumers(&self) -> &Accesses {
        &self.consumers
    }
}

// ─────────────────────────────── OORVIR query methods ───────────────────────────────
impl OORVIR {
    // Returns an iterator of raw indices into the signal stream slice.
    pub fn signal_indices(&self) -> impl Iterator<Item = usize> {
        0..self.signals.len()
    }

    // Returns an iterator of raw indices into the constraint stream slice.
    pub fn constraint_indices(&self) -> impl Iterator<Item = usize> {
        0..self.constraints.len()
    }

    // Provides exclusive access to the signal stream identified by `idx`.
    // Panics when `idx` is a Constraint variant.
    pub fn signal_mut(&mut self, idx: StreamIdx) -> &mut SignalStream {
        match idx {
            StreamIdx::Signal(pos) => &mut self.signals[pos],
            StreamIdx::Constraint(_) => {
                panic!("signal_mut called with a Constraint index ?expected a Signal index")
            }
        }
    }

    // Provides shared access to the signal stream identified by `idx`.
    // Panics when `idx` is a Constraint variant.
    pub fn signal(&self, idx: StreamIdx) -> &SignalStream {
        match idx {
            StreamIdx::Signal(pos) => &self.signals[pos],
            StreamIdx::Constraint(_) => {
                panic!("signal called with a Constraint index ?expected a Signal index")
            }
        }
    }

    // Provides exclusive access to the constraint stream identified by `idx`.
    // Panics when `idx` is a Signal variant.
    pub fn constraint_mut(&mut self, idx: StreamIdx) -> &mut ConstraintStream {
        match idx {
            StreamIdx::Signal(_) => {
                panic!("constraint_mut called with a Signal index ?expected a Constraint index")
            }
            StreamIdx::Constraint(pos) => &mut self.constraints[pos],
        }
    }

    // Provides shared access to the constraint stream identified by `idx`.
    // Panics when `idx` is a Signal variant.
    pub fn constraint(&self, idx: StreamIdx) -> &ConstraintStream {
        match idx {
            StreamIdx::Signal(_) => {
                panic!("constraint called with a Signal index ?expected a Constraint index")
            }
            StreamIdx::Constraint(pos) => &self.constraints[pos],
        }
    }

    // Returns a type-erased reference to the stream at `idx` (signal or constraint).
    pub fn resolve_stream(&self, idx: StreamIdx) -> &dyn Stream {
        match idx {
            StreamIdx::Signal(pos) => &self.signals[pos],
            StreamIdx::Constraint(pos) => &self.constraints[pos],
        }
    }

    // Produces a chained iterator of all StreamIdx values (signals first, then constraints).
    pub fn iter_streams(&self) -> impl Iterator<Item = StreamIdx> {
        let signal_range = self.signal_indices().map(StreamIdx::Signal);
        let constraint_range = self.constraint_indices().map(StreamIdx::Constraint);
        signal_range.chain(constraint_range)
    }

    // Collects references to every constraint stream that represents an alarm output.
    pub fn iter_alarms(&self) -> Vec<&ConstraintStream> {
        self.alarms
            .iter()
            .map(|entry| self.constraint(entry.constrain_idx))
            .collect()
    }

    // Collects references to every constraint stream driven by event-based pacing.
    pub fn event_driven_constraints(&self) -> Vec<&ConstraintStream> {
        self.event_task
            .iter()
            .map(|entry| self.constraint(entry.stream_idx))
            .collect()
    }

    // Returns true when the specification contains at least one periodically-scheduled feature.
    // This includes periodic constraint streams and period-based start conditions.
    pub fn has_periodic_streams(&self) -> bool {
        let has_time_tasks = !self.time_task.is_empty();
        let has_periodic_starts = self.constraints.iter().any(|cs| {
            matches!(
                cs.start.pacing,
                PacingNode::GlobalTick(_) | PacingNode::LocalTick(_)
            )
        });
        has_time_tasks || has_periodic_starts
    }

    // Collects references to every constraint stream driven by periodic (clock-based) pacing.
    pub fn periodic_constraints(&self) -> Vec<&ConstraintStream> {
        self.time_task
            .iter()
            .map(|entry| self.constraint(entry.stream_idx))
            .collect()
    }

    // Returns the activation condition for the given event-driven stream, or None if it is periodic.
    pub fn activation_condition(&self, target: StreamIdx) -> Option<&ActivationCondition> {
        self.event_task
            .iter()
            .find(|entry| entry.stream_idx == target)
            .map(|entry| &entry.ac)
    }

    // Builds the per-layer execution order for event-reactive streams and event-based start tasks.
    // Returns an empty vec when no event-driven features are present.
    pub fn event_schedule_layers(&self) -> Vec<Vec<Task>> {
        // Gather constraint streams that have event-based start pacing.
        let event_starts = self
            .constraints
            .iter()
            .filter(|cs| matches!(cs.start.pacing, PacingNode::Event(_)))
            .peekable();

        // `@always` lowers to Constant pacing, but operationally it is an
        // every-event rule rather than a compile-time constant.  Include such
        // streams in the event schedule and let the runtime gate accept them.
        let always_eval = self.constraints.iter().filter(|cs| {
            matches!(cs.eval.eval_pacing, PacingNode::Constant)
                && matches!(cs.kind, ConstraintKind::Alarm(_))
        });

        // Map event-reactive streams to (layer, task) pairs.
        let eval_with_layers = self
            .event_task
            .iter()
            .map(|et| {
                let layer: usize = self.constraint(et.stream_idx).eval_layer().into();
                (layer, Task::Evaluate(et.stream_idx.out_ix()))
            })
            .chain(always_eval.map(|cs| {
                let layer: usize = cs.eval_layer().into();
                (layer, Task::Evaluate(cs.stream_idx.out_ix()))
            }));

        let start_with_layers = event_starts.map(|cs| {
            (
                cs.start_layer().inner(),
                Task::Start(cs.stream_idx.out_ix()),
            )
        });

        let all_tasks: Vec<(usize, Task)> = eval_with_layers.chain(start_with_layers).collect();
        if all_tasks.is_empty() {
            return Vec::new();
        }

        // Determine the highest layer index across all tasks.
        let ceiling = all_tasks
            .iter()
            .map(|(layer, _)| *layer)
            .max()
            .expect("task list is non-empty at this point");

        // Build the output by gathering tasks for each layer 0..=ceiling,
        // discarding empty layers to produce a compact representation.
        let mut grouped: Vec<Vec<Task>> = Vec::new();
        for depth in 0..=ceiling {
            let bucket: Vec<Task> = all_tasks
                .iter()
                .filter_map(|(layer, task)| if *layer == depth { Some(*task) } else { None })
                .collect();
            if !bucket.is_empty() {
                grouped.push(bucket);
            }
        }
        grouped
    }

    // Attempts to derive a Schedule for all periodically-driven streams.
    // Returns Err when the schedule would exceed 10^7 deadlines.
    pub fn build_schedule(&self) -> std::result::Result<Schedule, String> {
        Schedule::derive_from_ir(self)
    }

    // Wraps `target` in a printer that borrows from this IR to resolve stream names.
    pub fn wrapped_display<'a, T>(&'a self, target: &'a T) -> OorvIrPrinter<'a, T> {
        OorvIrPrinter::wrap(self, target)
    }

    // Returns a dependency flow graph derived from this specification.
    pub fn flow_graph(&self) -> FlowGraph<'_> {
        FlowGraph::from_ir(self)
    }

    // Looks up a signal stream by its declared name.
    pub fn signal_by_name(&self, name: &str) -> Option<&SignalStream> {
        self.signals.iter().find(|s| s.name == name)
    }

    // Looks up a constraint stream by its declared name.
    pub fn constraint_by_name(&self, name: &str) -> Option<&ConstraintStream> {
        self.constraints.iter().find(|cs| cs.name == name)
    }

    // Looks up any stream (signal or constraint) by its declared name.
    // Returns a type-erased reference, or None when no match exists.
    pub fn stream_by_name(&self, name: &str) -> Option<&dyn Stream> {
        if let Some(sig) = self.signal_by_name(name) {
            let sig_ref: &dyn Stream = sig;
            return Some(sig_ref);
        }
        self.constraint_by_name(name).map(|cs| {
            let cs_ref: &dyn Stream = cs;
            cs_ref
        })
    }
}

// ─────────────────────────────── Type utility methods ───────────────────────────────
impl Type {
    // Determines how many bytes this type occupies when stored.
    // Returns None for function types, which have no direct memory footprint.
    // Panics for Option, String, and Bytes whose sizes are not statically known.
    pub fn byte_width(&self) -> Option<ValSize> {
        match self {
            Type::Bool => Some(ValSize(1)),
            Type::Int(IntTy::Int8) | Type::UInt(UIntTy::UInt8) => Some(ValSize(1)),
            Type::Int(IntTy::Int16) | Type::UInt(UIntTy::UInt16) => Some(ValSize(2)),
            Type::Int(IntTy::Int32) | Type::UInt(UIntTy::UInt32) => Some(ValSize(4)),
            Type::Int(IntTy::Int64) | Type::UInt(UIntTy::UInt64) => Some(ValSize(8)),
            Type::Int(IntTy::Int128) | Type::UInt(UIntTy::UInt128) => Some(ValSize(16)),
            Type::Int(IntTy::Int256) | Type::UInt(UIntTy::UInt256) => Some(ValSize(32)),
            Type::Float(FloatTy::Float32) => Some(ValSize(4)),
            Type::Float(FloatTy::Float64) => Some(ValSize(8)),
            Type::Fixed(FixedTy::Fixed64_32) | Type::UFixed(FixedTy::Fixed64_32) => {
                Some(ValSize(64))
            }
            Type::Fixed(FixedTy::Fixed32_16) | Type::UFixed(FixedTy::Fixed32_16) => {
                Some(ValSize(32))
            }
            Type::Fixed(FixedTy::Fixed16_8) | Type::UFixed(FixedTy::Fixed16_8) => Some(ValSize(16)),
            Type::Tuple(elems) => {
                let total: u32 = elems
                    .iter()
                    .map(|elem| {
                        Type::byte_width(elem)
                            .expect("tuple element must have known size")
                            .0
                    })
                    .sum();
                Some(ValSize(total))
            }
            Type::Option(_) => unimplemented!("byte_width is not defined for Option types"),
            Type::String | Type::Bytes => {
                unimplemented!("byte_width is not defined for unsized string/byte types")
            }
            Type::Function { .. } => None,
        }
    }
}

// Byte size of a typed value; used to compute storage requirements.
// The inner u32 must be large enough to hold the total for compound tuple types.
#[derive(Debug, Clone, Copy)]
pub struct ValSize(pub u32);

impl From<u8> for ValSize {
    // Widen a byte-sized value into the u32 backing field.
    fn from(val: u8) -> ValSize {
        ValSize(u32::from(val))
    }
}

impl std::ops::Add for ValSize {
    type Output = ValSize;

    // Combine two sizes by summing their byte counts.
    fn add(self, rhs: ValSize) -> ValSize {
        ValSize(self.0 + rhs.0)
    }
}

// How a stream value is accessed from a dependent stream.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize, Hash)]
pub enum AccessMode {
    // Synchronous access: the dependency must be evaluated in the same step.
    Strict,
    // Sample-and-hold: use the most recently computed value.
    Cached,
    // Offset-based lookup relative to the current timestamp.
    Shift(Shift),
    // Optional access that returns None when the stream has no current value.
    Get,
    // Freshness check: true if the target received a new value this step.
    Fresh,
}

// Direction and magnitude of a stream offset lookup.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize, Hash)]
pub enum Shift {
    // Look ahead by the given number of time steps.
    Future(u32),
    // Look back by the given number of time steps.
    Past(u32),
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
            (Past(_), Future(_)) => Ordering::Less,
            (Future(_), Past(_)) => Ordering::Greater,
            (Future(a), Future(b)) => a.cmp(b),
            (Past(a), Past(b)) => b.cmp(a),
        }
    }
}

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::Display;
use std::io::BufWriter;

use dot::{LabelText, Style};
use itertools::Itertools;
use serde_json::{json, to_string_pretty};

// Visual and JSON representation of the data-flow and pacing dependencies in an OORVIR specification.
#[derive(Debug, Clone)]
pub struct FlowGraph<'a> {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    infos: HashMap<Node, NodeInformation<'a>>,
}

impl<'a> FlowGraph<'a> {
    pub(super) fn from_ir(ir: &'a OORVIR) -> Self {
        let stream_nodes = ir
            .signals
            .iter()
            .map(|sig| sig.stream_idx)
            .chain(
                ir.constraints
                    .iter()
                    .filter(|cs| !cs.is_constrain())
                    .map(|cs| cs.stream_idx),
            )
            .map(Node::Stream);

        let alarm_nodes = ir.alarms.iter().map(|a| Node::Constrain(a.alarm_idx));

        let node_list: Vec<_> = stream_nodes.chain(alarm_nodes).collect();

        let edge_list = collect_flow_edges(ir);

        let info_map = node_list
            .iter()
            .map(|node| (*node, gather_node_metadata(ir, *node)))
            .collect();

        Self {
            nodes: node_list,
            edges: edge_list,
            infos: info_map,
        }
    }

    // Render the dependency graph in Graphviz DOT format.
    pub fn to_dot(&self) -> String {
        let res = Vec::new();
        let mut res_writer = BufWriter::new(res);
        dot::render(self, &mut res_writer).unwrap();
        String::from_utf8(res_writer.into_inner().unwrap()).unwrap()
    }

    // Render the dependency graph as a pretty-printed JSON string.
    pub fn to_json(&self) -> String {
        let infos = self
            .infos
            .iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect::<HashMap<_, _>>();

        let json_value = json!({
            "edges": self.edges,
            "nodes": infos
        });

        to_string_pretty(&json_value).unwrap()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Node {
    Stream(StreamIdx),
    Constrain(usize),
}

impl From<StreamIdx> for Node {
    fn from(s: StreamIdx) -> Self {
        Node::Stream(s)
    }
}

impl Display for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Node::Stream(StreamIdx::Signal(i)) => write!(f, "sig_{i}"),
            Node::Stream(StreamIdx::Constraint(i)) => write!(f, "cs_{i}"),
            Node::Constrain(i) => write!(f, "alarm_{i}"),
        }
    }
}

impl Serialize for Node {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.to_string().as_str())
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct Edge {
    from: Node,
    with: EdgeType,
    to: Node,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type")]
enum EdgeType {
    Access {
        kind: AccessMode,
        #[serde(rename = "AccessSite")]
        access_site: AccessSite,
    },
    // Edge created by the start pacing of a stream instance.
    StartEdge,
    Eval,
}

impl Display for EdgeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            EdgeType::Access {
                kind: AccessMode::Strict,
                ..
            } => "Strict".into(),
            EdgeType::Access {
                kind: AccessMode::Cached,
                ..
            } => "Cached".into(),
            EdgeType::Access {
                kind: AccessMode::Shift(o),
                ..
            } => format!("Shift({o})"),
            EdgeType::StartEdge => "Start".into(),
            EdgeType::Eval => "Eval".into(),
            EdgeType::Access {
                kind: AccessMode::Get,
                ..
            } => "Get".into(),
            EdgeType::Access {
                kind: AccessMode::Fresh,
                ..
            } => "Fresh".into(),
        };

        write!(f, "{s}")
    }
}

#[derive(Serialize, Debug, Clone)]
#[serde(untagged)]
enum NodeInformation<'a> {
    Input {
        stream_idx: StreamIdx,
        stream_name: &'a str,
        storage_bound: u32,
        value_ty: String,
    },

    Output {
        stream_idx: StreamIdx,
        is_constrain: bool,
        stream_name: &'a str,
        eval_layer: usize,
        storage_bound: u32,
        pacing_ty: String,
        // Pacing string for the start clause of the stream instance.
        start_pacing_ty: String,
        value_ty: String,
    },
}

fn gather_node_metadata<'a>(ir: &'a OORVIR, node: Node) -> NodeInformation<'a> {
    match node {
        Node::Stream(idx) => gather_stream_metadata(ir, idx),
        Node::Constrain(alarm_pos) => {
            gather_stream_metadata(ir, ir.alarms[alarm_pos].constrain_idx)
        }
    }
}

fn gather_stream_metadata<'a>(ir: &'a OORVIR, idx: StreamIdx) -> NodeInformation<'a> {
    let stream_ref = ir.resolve_stream(idx);

    let stream_name = stream_ref.name();
    let eval_layer_val: usize = stream_ref.eval_layer().into();
    let storage_bound_val = stream_ref.required_storage_bound().unwrap_bound();
    let value_type = stream_ref.annotation();
    let value_str = value_type.to_string();

    match idx {
        StreamIdx::Signal(_) => NodeInformation::Input {
            stream_idx: idx,
            stream_name,
            storage_bound: storage_bound_val,
            value_ty: value_str,
        },
        StreamIdx::Constraint(_) => {
            let cs = ir.constraint(idx);
            let eval_pacing_str = ir.wrapped_display(&cs.eval.eval_pacing).to_string();
            let start_pacing_str = ir.wrapped_display(&cs.start.pacing).to_string();

            NodeInformation::Output {
                stream_idx: idx,
                is_constrain: cs.is_constrain(),
                stream_name,
                eval_layer: eval_layer_val,
                storage_bound: storage_bound_val,
                pacing_ty: eval_pacing_str,
                start_pacing_ty: start_pacing_str,
                value_ty: value_str,
            }
        }
    }
}

fn collect_flow_edges(ir: &OORVIR) -> Vec<Edge> {
    let signal_accesses = ir
        .signals
        .iter()
        .map(|sig| (sig.stream_idx, &sig.consumers));
    let constraint_accesses = ir
        .constraints
        .iter()
        .map(|cs| (cs.stream_idx, &cs.consumers));
    let all_accesses = signal_accesses.chain(constraint_accesses);
    let constraint_to_alarm: &HashMap<_, _> = &(ir
        .alarms
        .iter()
        .map(|a| (a.constrain_idx, a.alarm_idx))
        .collect());

    let access_edges = all_accesses.flat_map(|(source_ref, accesses)| {
        let source_node = constraint_to_alarm
            .get(&source_ref)
            .map(|alarm_pos| Node::Constrain(*alarm_pos))
            .unwrap_or_else(|| Node::Stream(source_ref));
        accesses.iter().flat_map(move |(target_ref, access_kinds)| {
            let target_node = constraint_to_alarm
                .get(target_ref)
                .map(|alarm_pos| Node::Constrain(*alarm_pos))
                .unwrap_or_else(|| Node::Stream(*target_ref));
            access_kinds
                .iter()
                .flat_map(move |&(site, kind)| match kind {
                    AccessMode::Fresh
                    | AccessMode::Get
                    | AccessMode::Cached
                    | AccessMode::Shift(_)
                    | AccessMode::Strict => {
                        vec![Edge {
                            from: target_node,
                            with: EdgeType::Access {
                                access_site: site,
                                kind,
                            },
                            to: source_node,
                        }]
                    }
                })
        })
    });

    // Edges originating from the start pacing of each constraint stream.
    let start_edges = ir.constraints.iter().flat_map(|cs| {
        let source_node = constraint_to_alarm
            .get(&cs.stream_idx)
            .map(|alarm_pos| Node::Constrain(*alarm_pos))
            .unwrap_or_else(|| Node::Stream(cs.stream_idx));
        match &cs.start.pacing {
            PacingNode::Event(ac) => ac_stream_indices(ac)
                .into_iter()
                .map(|dep_idx| Edge {
                    from: source_node,
                    with: EdgeType::StartEdge,
                    to: Node::Stream(dep_idx),
                })
                .collect(),
            PacingNode::LocalTick(_) | PacingNode::GlobalTick(_) | PacingNode::Constant => {
                vec![]
            }
        }
    });

    let ac_edges = ir.constraints.iter().flat_map(|cs| {
        let source_node = constraint_to_alarm
            .get(&cs.stream_idx)
            .map(|alarm_pos| Node::Constrain(*alarm_pos))
            .unwrap_or_else(|| Node::Stream(cs.stream_idx));
        match &cs.eval.eval_pacing {
            PacingNode::Event(ac) => ac_stream_indices(ac)
                .into_iter()
                .map(|dep_idx| Edge {
                    from: source_node,
                    with: EdgeType::Eval,
                    to: Node::Stream(dep_idx),
                })
                .collect(),
            PacingNode::LocalTick(_) | PacingNode::GlobalTick(_) | PacingNode::Constant => {
                vec![]
            }
        }
    });

    access_edges.chain(start_edges).chain(ac_edges).collect()
}

// Collect all stream indices referenced by an ActivationCondition (recursive inner helper).
fn ac_to_stream_refs_inner(ac: &ActivationCondition) -> Vec<StreamIdx> {
    match ac {
        ActivationCondition::Disjunction(xs) | ActivationCondition::Conjunction(xs) => {
            xs.iter().flat_map(ac_stream_indices).collect()
        }
        ActivationCondition::Stream(s) => vec![*s],
        ActivationCondition::True => vec![],
    }
}

// Collect all unique stream indices referenced by an ActivationCondition.
fn ac_stream_indices(ac: &ActivationCondition) -> Vec<StreamIdx> {
    let mut indices = ac_to_stream_refs_inner(ac);
    indices.sort();
    indices.dedup();
    indices
}

impl<'a> dot::Labeller<'a, Node, Edge> for FlowGraph<'a> {
    fn graph_id(&'a self) -> dot::Id<'a> {
        dot::Id::new("access_graph").unwrap()
    }

    fn node_id(&'a self, n: &Node) -> dot::Id<'a> {
        let id = n.to_string();
        dot::Id::new(id).unwrap()
    }

    fn node_label<'b>(&'b self, n: &Node) -> LabelText<'b> {
        let infos = self.infos.get(n).unwrap();

        let label_text = match infos {
            NodeInformation::Input {
                stream_name,
                storage_bound,
                value_ty,
                stream_idx: _,
            } => {
                format!("<b>{stream_name}</b> :: {value_ty}<br/>[buf: {storage_bound}]")
            }
            NodeInformation::Output {
                stream_name,
                is_constrain: _,
                eval_layer,
                storage_bound,
                pacing_ty,
                start_pacing_ty,
                value_ty,
                stream_idx: _,
            } => {
                format!(
                    "<b>{stream_name}</b> :: {value_ty}<br/>\
eval: {pacing_ty}<br/>\
begin: {start_pacing_ty}<br/>\
[buf: {storage_bound}, layer: {eval_layer}]"
                )
            }
        };

        LabelText::HtmlStr(label_text.into())
    }

    fn edge_label<'b>(&'b self, edge: &Edge) -> LabelText<'b> {
        LabelText::LabelStr(edge.with.to_string().into())
    }

    fn edge_style(&self, edge: &Edge) -> Style {
        match &edge.with {
            EdgeType::Access {
                kind,
                access_site: _,
            } => match kind {
                AccessMode::Get | AccessMode::Fresh | AccessMode::Cached => Style::Dashed,
                AccessMode::Strict | AccessMode::Shift(_) => Style::None,
            },
            EdgeType::StartEdge | EdgeType::Eval => Style::Dotted,
        }
    }

    fn node_shape(&self, node: &Node) -> Option<LabelText<'_>> {
        let shape_str = match node {
            Node::Stream(StreamIdx::Signal(_)) => "rectangle",
            Node::Stream(StreamIdx::Constraint(_)) => "oval",
            Node::Constrain(_) => "hexagon",
        };

        Some(LabelText::LabelStr(shape_str.into()))
    }

    fn edge_end_arrow(&'a self, _e: &Edge) -> dot::Arrow {
        dot::Arrow::none()
    }

    fn edge_start_arrow(&'a self, _e: &Edge) -> dot::Arrow {
        dot::Arrow::normal()
    }
}

impl<'a> dot::GraphWalk<'a, Node, Edge> for FlowGraph<'a> {
    fn nodes(&'a self) -> dot::Nodes<'a, Node> {
        Cow::Borrowed(&self.nodes)
    }

    fn edges(&'a self) -> dot::Edges<'a, Edge> {
        // all the sync and offset edges
        let ac_accesses = self
            .edges
            .iter()
            .filter(|edge| {
                matches!(
                    edge.with,
                    EdgeType::Access {
                        kind: AccessMode::Strict,
                        ..
                    } | EdgeType::Access {
                        kind: AccessMode::Shift(_),
                        ..
                    }
                )
            })
            .map(|edge| (&edge.from, &edge.to))
            .collect::<HashSet<_>>();

        let edges = self
            .edges
            .iter()
            // remove edges that have the same access kind but different origins, because
            // the AccessSite is not displayed in the dot-representation
            .unique_by(|edge| {
                (
                    edge.from,
                    edge.to,
                    match edge.with {
                        EdgeType::Access {
                            kind,
                            access_site: _,
                        } => Some(kind),
                        EdgeType::StartEdge | EdgeType::Eval => None,
                    },
                )
            })
            // In the DOT format, eval edges are suppressed when the same dependency
            // is already covered by a strict or offset access edge.
            .filter(|edge| match edge.with {
                EdgeType::Access { .. } | EdgeType::StartEdge => true,
                EdgeType::Eval => !ac_accesses.contains(&(&edge.from, &edge.to)),
            })
            .cloned()
            .collect();
        Cow::Owned(edges)
    }

    fn source(&self, e: &Edge) -> Node {
        // because we add the arrows the wrong way round (see edge style)
        e.to
    }

    fn target(&self, e: &Edge) -> Node {
        // because we add the arrows the wrong way round (see edge style)
        e.from
    }
}

use std::fmt::{Formatter, Result as FmtResult};

impl Display for Constant {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Constant::Bool(b) => write!(f, "Bool({b})"),
            Constant::UInt(u) => write!(f, "{u}u"),
            Constant::Int(i) => write!(f, "{i}i"),
            Constant::Float(fl) => write!(f, "{fl}f"),
            Constant::Str(s) => write!(f, "'{s}'"),
            Constant::Decimal(i) => write!(f, "dec({i})"),
        }
    }
}

impl Display for ArithLogOp {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        use ArithLogOp::*;
        match self {
            Not => write!(f, "!"),
            Neg => write!(f, "~"),
            Add => write!(f, "+"),
            Sub => write!(f, "-"),
            Mul => write!(f, "*"),
            Div => write!(f, "/"),
            Rem => write!(f, "%"),
            Pow => write!(f, "^"),
            And => write!(f, "&&"),
            Or => write!(f, "||"),
            Eq => write!(f, "=="),
            Lt => write!(f, "<"),
            Le => write!(f, "<="),
            Ne => write!(f, "!="),
            Ge => write!(f, ">="),
            Gt => write!(f, ">"),
            BitNot => write!(f, "~"),
            BitAnd => write!(f, "&"),
            BitOr => write!(f, "|"),
            BitXor => write!(f, "^"),
            Shl => write!(f, "<<"),
            Shr => write!(f, ">>"),
        }
    }
}

impl Display for Type {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Type::Float(_) => write!(
                f,
                "f{}",
                self.byte_width().expect("Floats are sized.").0 * 8
            ),
            Type::UInt(_) => write!(f, "u{}", self.byte_width().expect("UInts are sized.").0 * 8),
            Type::Int(_) => write!(f, "i{}", self.byte_width().expect("Ints are sized.").0 * 8),
            Type::Fixed(ty) => write!(f, "Fixed{ty}"),
            Type::UFixed(ty) => write!(f, "UFixed{ty}"),
            Type::Function { args, ret } => {
                write_enclosed_list(f, args, "(", &format!(") -> {ret}"), ",")
            }
            Type::Tuple(elems) => write_enclosed_list(f, elems, "(", ")", ","),
            Type::String => write!(f, "String"),
            Type::Bytes => write!(f, "Bytes"),
            Type::Option(inner) => write!(f, "Option<{inner}>"),
            Type::Bool => write!(f, "Bool"),
        }
    }
}

impl Display for IntTy {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            IntTy::Int8 => write!(f, "8"),
            IntTy::Int16 => write!(f, "16"),
            IntTy::Int32 => write!(f, "32"),
            IntTy::Int64 => write!(f, "64"),
            IntTy::Int128 => write!(f, "128"),
            IntTy::Int256 => write!(f, "256"),
        }
    }
}

impl Display for UIntTy {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            UIntTy::UInt8 => write!(f, "8"),
            UIntTy::UInt16 => write!(f, "16"),
            UIntTy::UInt32 => write!(f, "32"),
            UIntTy::UInt64 => write!(f, "64"),
            UIntTy::UInt128 => write!(f, "128"),
            UIntTy::UInt256 => write!(f, "256"),
        }
    }
}

impl Display for FloatTy {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            FloatTy::Float32 => write!(f, "32"),
            FloatTy::Float64 => write!(f, "64"),
        }
    }
}

impl Display for FixedTy {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            FixedTy::Fixed64_32 => write!(f, "64_32"),
            FixedTy::Fixed32_16 => write!(f, "32_16"),
            FixedTy::Fixed16_8 => write!(f, "16_8"),
        }
    }
}

// Write the joined slice `v` enclosed by `pref` and `suff`, using `join` as separator.
pub(crate) fn write_enclosed_list<T: Display>(
    f: &mut Formatter<'_>,
    v: &[T],
    pref: &str,
    suff: &str,
    join: &str,
) -> FmtResult {
    write!(f, "{pref}")?;
    if let Some(e) = v.first() {
        write!(f, "{e}")?;
        for b in &v[1..] {
            write!(f, "{join}{b}")?;
        }
    }
    write!(f, "{suff}")?;
    Ok(())
}

impl Display for Shift {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Shift::Past(u) => write!(f, "{u}"),
            _ => unimplemented!(),
        }
    }
}

// A lightweight wrapper around OORVIR providing a [Display] implementation for inner type `T`.
#[derive(Debug, Clone, Copy)]
pub struct OorvIrPrinter<'a, T> {
    refined_ir: &'a OORVIR,
    inner: &'a T,
}

impl<'a, T> OorvIrPrinter<'a, T> {
    pub(crate) fn wrap(refined_ir: &'a OORVIR, target: &'a T) -> Self {
        OorvIrPrinter {
            refined_ir,
            inner: target,
        }
    }
}

impl<T: Display> Display for OorvIrPrinter<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        self.inner.fmt(f)
    }
}

impl Display for OorvIrPrinter<'_, ActivationCondition> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self.inner {
            ActivationCondition::Conjunction(s) => {
                let rs = s
                    .iter()
                    .map(|ac| OorvIrPrinter::wrap(self.refined_ir, ac).to_string())
                    .join(&ArithLogOp::And.to_string());
                write!(f, "{rs}")
            }
            ActivationCondition::Disjunction(s) => {
                let rs = s
                    .iter()
                    .map(|ac| OorvIrPrinter::wrap(self.refined_ir, ac).to_string())
                    .join(&ArithLogOp::Or.to_string());
                write!(f, "{rs}")
            }
            ActivationCondition::Stream(s) => {
                write!(f, "{}", self.refined_ir.resolve_stream(*s).name())
            }
            ActivationCondition::True => write!(f, "true"),
        }
    }
}

impl Display for OorvIrPrinter<'_, PacingNode> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self.inner {
            PacingNode::GlobalTick(freq) => {
                let s = freq
                    .into_format_args(
                        uom::si::frequency::hertz,
                        uom::fmt::DisplayStyle::Abbreviation,
                    )
                    .to_string();
                write!(f, "@global/{}Hz", &s[..s.len() - 3])
            }
            PacingNode::LocalTick(freq) => {
                let s = freq
                    .into_format_args(
                        uom::si::frequency::hertz,
                        uom::fmt::DisplayStyle::Abbreviation,
                    )
                    .to_string();
                write!(f, "@local/{}Hz", &s[..s.len() - 3])
            }
            PacingNode::Event(ac) => OorvIrPrinter::wrap(self.refined_ir, ac).fmt(f),
            PacingNode::Constant => write!(f, "@always"),
        }
    }
}

type Associativity = bool;

fn precedence_level(op: &ArithLogOp) -> (u32, Associativity) {
    // https://en.cppreference.com/w/c/language/operator_precedence
    let precedence = match op {
        ArithLogOp::Not | ArithLogOp::BitNot | ArithLogOp::Neg => 2,

        ArithLogOp::Mul | ArithLogOp::Rem | ArithLogOp::Pow | ArithLogOp::Div => 3,

        ArithLogOp::Add | ArithLogOp::Sub => 4,

        ArithLogOp::Shl | ArithLogOp::Shr => 5,

        ArithLogOp::Lt | ArithLogOp::Le | ArithLogOp::Ge | ArithLogOp::Gt => 6,

        ArithLogOp::Eq | ArithLogOp::Ne => 7,

        ArithLogOp::BitAnd => 8,
        ArithLogOp::BitXor => 9,
        ArithLogOp::BitOr => 10,
        ArithLogOp::And => 11,
        ArithLogOp::Or => 12,
    };

    let associativity = !matches!(op, ArithLogOp::Div | ArithLogOp::Sub);

    (precedence, associativity)
}

pub(crate) fn display_expression(
    refined_ir: &OORVIR,
    expr: &Expression,
    current_level: u32,
) -> String {
    match &expr.kind {
        ExprVariant::LoadConstant(c) => c.to_string(),
        ExprVariant::ArithLog(op, exprs) => {
            let (op_level, associative) = precedence_level(op);
            let display_exprs = exprs
                .iter()
                .map(|expr| display_expression(refined_ir, expr, op_level))
                .collect::<Vec<_>>();
            let display = match display_exprs.len() {
                1 => format!("{}{}", op, display_exprs[0]),
                2 => format!("{} {} {}", display_exprs[0], op, display_exprs[1]),
                _ => unreachable!(),
            };
            if (associative && current_level < op_level
                || !associative && current_level <= op_level)
                && current_level != 0
            {
                format!("({display})")
            } else {
                display
            }
        }
        ExprVariant::StreamAccess {
            target,
            parameters,
            access_kind,
        } => {
            let stream_name = refined_ir.resolve_stream(*target).name();
            let target_name = if !parameters.is_empty() {
                let parameter_list = parameters
                    .iter()
                    .map(|parameter| display_expression(refined_ir, parameter, 0))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{stream_name}({parameter_list})")
            } else {
                stream_name.into()
            };

            match access_kind {
                AccessMode::Strict => target_name,
                AccessMode::Cached => format!("{target_name}.last()"),
                AccessMode::Shift(o) => format!("{target_name}[-{o}]"),
                AccessMode::Get => format!("{target_name}.opt()"),
                AccessMode::Fresh => format!("{target_name}.new()"),
            }
        }
        ExprVariant::ParameterAccess(sref, parameter) => refined_ir.constraint(*sref).params
            [*parameter]
            .name
            .to_string(),
        ExprVariant::FunctionParameterAccess(idx) => format!("param_{}", idx),
        ExprVariant::Ite {
            condition,
            consequence,
            alternative,
        } => {
            let display_condition = display_expression(refined_ir, condition, 0);
            let display_consequence = display_expression(refined_ir, consequence, 0);
            let display_alternative = display_expression(refined_ir, alternative, 0);
            format!("if {display_condition} then {display_consequence} else {display_alternative}")
        }
        ExprVariant::Tuple(exprs) => {
            let display_exprs = exprs
                .iter()
                .map(|expr| display_expression(refined_ir, expr, 0))
                .collect::<Vec<_>>()
                .join(", ");
            format!("({display_exprs})")
        }
        ExprVariant::TupleAccess(expr, i) => {
            let display_expr = display_expression(refined_ir, expr, 20);
            format!("{display_expr}.{i}")
        }
        ExprVariant::QuantifiedVar(name) => name.clone(),
        ExprVariant::Function(name, args) => {
            let display_args = args
                .iter()
                .map(|arg| display_expression(refined_ir, arg, 0))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{name}({display_args})")
        }
        ExprVariant::Convert { expr: inner_expr } => {
            let inner_display = display_expression(refined_ir, inner_expr, 0);
            format!("({inner_display} as {})", expr.annotation)
        }
        ExprVariant::Default { expr, default } => {
            let display_expr = display_expression(refined_ir, expr, 0);
            let display_default = display_expression(refined_ir, default, 0);
            format!("{display_expr}.defaults(to: {display_default})")
        }
        ExprVariant::Quantified {
            quantifier,
            bindings1,
            bindings2,
            body,
        } => {
            let q = match quantifier {
                crate::oorvir::refined::Quantifier::Forall => "forall",
                crate::oorvir::refined::Quantifier::Exists => "exists",
            };
            let binds_str1 = format!("[{}]", bindings1.join(", "));
            let binds_str2 = format!("[{}]", bindings2.join(", "));
            let body_str = display_expression(refined_ir, body, 0);
            format!("{q} {binds_str1} {binds_str2}: {body_str}")
        }
    }
}

impl Display for OorvIrPrinter<'_, InstanceSelection> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match &self.inner {
            InstanceSelection::Fresh => write!(f, "fresh"),
            InstanceSelection::All => write!(f, "all"),
            InstanceSelection::FilteredFresh { parameters, cond } => {
                let parameters = parameters
                    .iter()
                    .map(|p| format!("{}: {}", &p.name, p.annotation))
                    .join(", ");
                let cond = display_expression(self.refined_ir, cond, 0);
                write!(f, "fresh(where: ({parameters}) => {cond})")
            }
            InstanceSelection::FilteredAll { parameters, cond } => {
                let parameters = parameters
                    .iter()
                    .map(|p| format!("{}: {}", &p.name, p.annotation))
                    .join(", ");
                let cond = display_expression(self.refined_ir, cond, 0);
                write!(f, "all(where: ({parameters}) => {cond})")
            }
        }
    }
}

impl Display for OorvIrPrinter<'_, Expression> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{}", display_expression(self.refined_ir, self.inner, 0))
    }
}

impl Display for SignalStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let name = &self.name;
        let annotation = &self.annotation;
        write!(f, "signal {name} :: {annotation}")
    }
}

impl Display for OorvIrPrinter<'_, ConstraintStream> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let ConstraintStream {
            name: _,
            annotation,
            start,
            eval,
            end,
            params,
            kind,
            ..
        } = self.inner;

        let display_parameters = if !params.is_empty() {
            let parameter_list = params
                .iter()
                .map(|parameter| format!("{} : {}", parameter.name, parameter.annotation))
                .join(", ");
            format!("({parameter_list})")
        } else {
            "".into()
        };

        match kind {
            ConstraintKind::Output(name) => {
                write!(f, "output {name}{display_parameters} : {annotation}")?
            }
            ConstraintKind::Alarm(_) => write!(f, "constrain{display_parameters}")?,
        }

        if start.expression.is_some()
            || start.condition.is_some()
            || start.pacing != PacingNode::Constant
        {
            let display_pacing = OorvIrPrinter::wrap(self.refined_ir, &start.pacing).to_string();
            write!(f, "\n  start @{display_pacing}")?;
            if let Some(start_expr) = &start.expression {
                let display_start_expr = display_expression(self.refined_ir, start_expr, 0);
                write!(f, " with {display_start_expr}")?;
            }
            if let Some(start_condition) = &start.condition {
                let display_start_condition =
                    display_expression(self.refined_ir, start_condition, 0);
                write!(f, " when {display_start_condition}")?;
            }
        }

        for clause in &eval.decls {
            let display_pacing =
                OorvIrPrinter::wrap(self.refined_ir, &eval.eval_pacing).to_string();
            write!(f, "\n  eval @{display_pacing} ")?;
            if let Some(eval_condition) = &clause.condition {
                let display_eval_condition = display_expression(self.refined_ir, eval_condition, 0);
                write!(f, "when {display_eval_condition} ")?;
            }
            let display_eval_expr = display_expression(self.refined_ir, &clause.expression, 0);
            write!(f, "with {display_eval_expr}")?;
        }

        if let Some(end_condition) = &end.condition {
            let display_pacing = OorvIrPrinter::wrap(self.refined_ir, &end.pacing).to_string();
            let display_end_condition = display_expression(self.refined_ir, end_condition, 0);
            write!(f, "\n  end @{display_pacing} when {display_end_condition}")?;
        }

        Ok(())
    }
}

impl Display for OorvIrPrinter<'_, Alarm> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let cs = self.refined_ir.constraint(self.inner.constrain_idx);
        OorvIrPrinter::wrap(self.refined_ir, cs).fmt(f)
    }
}

impl Display for OORVIR {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        self.signals.iter().try_for_each(|sig| {
            OorvIrPrinter::wrap(self, sig).fmt(f)?;
            write!(f, "\n\n")
        })?;

        self.constraints.iter().try_for_each(|cs| {
            OorvIrPrinter::wrap(self, cs).fmt(f)?;
            write!(f, "\n\n")
        })?;
        Ok(())
    }
}

use num::rational::Rational64 as Rational;
use num::{One, ToPrimitive};
use std::ops::Not;
use uom::si::time::second;

// Enumerates the periodic tasks that must be executed at each schedule deadline.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum Task {
    // Evaluate the stream at the given constraint index.
    Evaluate(usize),
    // Initialise a new stream instance at the given constraint index.
    Start(usize),
    // Evaluate the termination condition at the given constraint index.
    End(usize),
}

// A single point in the periodic schedule within one hyper-period.
// All deadlines tile the hyper-period; `pause` is the gap to the next deadline.
#[derive(Debug, Clone)]
pub struct Deadline {
    // Time gap between this deadline and the next.
    pub pause: Duration,
    // Tasks due at this deadline.
    pub due: Vec<Task>,
}

// The full periodic schedule: a sequence of deadlines that tile one hyper-period
// and are meant to repeat indefinitely.
#[derive(Debug, Clone)]
pub struct Schedule {
    // Least common multiple of all stream periods; None if no periodic streams exist.
    pub hyper_period: Option<Duration>,

    // Ordered sequence of deadlines within a single hyper-period.
    // Each entry carries the time offset from the previous deadline and the tasks due at that point.
    pub deadlines: Vec<Deadline>,
}

impl Schedule {
    // Compute the full periodic schedule for the given refined_ir.
    // Returns an error if the schedule would require 10^7 or more deadlines.
    pub(crate) fn derive_from_ir(ir: &OORVIR) -> std::result::Result<Schedule, String> {
        let stream_periods = ir
            .time_task
            .iter()
            .filter(|tds| tds.locality == PacingLocality::Global)
            .map(|tds| tds.evaluation_period());
        let periodic_starts = ir.constraints.iter().filter_map(|cs| {
            if let PacingNode::GlobalTick(freq) = &cs.start.pacing {
                Some(UOM_Time::new::<second>(
                    freq.get::<uom::si::frequency::hertz>().inv(),
                ))
            } else {
                None
            }
        });
        let end_periods = ir.constraints.iter().filter_map(|cs| {
            if let PacingNode::GlobalTick(freq) = &cs.end.pacing {
                Some(UOM_Time::new::<second>(
                    freq.get::<uom::si::frequency::hertz>().inv(),
                ))
            } else {
                None
            }
        });
        let periods: Vec<UOM_Time> = stream_periods
            .chain(periodic_starts)
            .chain(end_periods)
            .collect();
        if periods.is_empty() {
            return Ok(Schedule {
                hyper_period: None,
                deadlines: vec![],
            });
        }
        let gcd_period = Self::compute_gcd_interval(&periods);
        let lcm_period = Self::compute_lcm_period(&periods);

        let slot_grid = Self::create_deadline_slots(ir, gcd_period, lcm_period)?;
        let slot_grid = Self::expand_periodic_slots(&slot_grid);
        let mut deadline_list = Self::merge_into_deadlines(gcd_period, slot_grid);
        Self::order_deadline_tasks(ir, &mut deadline_list);

        let hyper_period_duration = Duration::from_nanos(
            lcm_period
                .get::<nanosecond>()
                .to_integer()
                .to_u64()
                .unwrap(),
        );
        Ok(Schedule {
            hyper_period: Some(hyper_period_duration),
            deadlines: deadline_list,
        })
    }

    // Compute the GCD of all stream periods in nanoseconds (maximum safe polling interval).
    fn compute_gcd_interval(rates: &[UOM_Time]) -> UOM_Time {
        assert!(!rates.is_empty());
        let ns_rates: Vec<Rational> = rates.iter().map(|r| r.get::<nanosecond>()).collect();
        let gcd_val = math::gcd_all_rational(&ns_rates);
        UOM_Time::new::<nanosecond>(gcd_val)
    }

    // Compute the LCM of all stream periods in nanoseconds (hyper-period length).
    fn compute_lcm_period(rates: &[UOM_Time]) -> UOM_Time {
        assert!(!rates.is_empty());
        let ns_rates: Vec<Rational> = rates.iter().map(|r| r.get::<nanosecond>()).collect();
        let lcm_val = math::lcm_all_rational(&ns_rates);
        // Hyper-period must be a whole number of nanoseconds.
        let lcm_val = math::lcm_rational(lcm_val, Rational::one());
        UOM_Time::new::<nanosecond>(lcm_val)
    }

    // Propagate tasks in the slot grid so that a task at slot `i` also appears in every
    fn expand_periodic_slots(steps: &[Vec<Task>]) -> Vec<Vec<Task>> {
        // For each slot at index `ix` containing tasks, propagate them to every
        // slot at index k*(ix+1)-1 for k = 2, 3, ... that is still within bounds.
        let mut res = vec![Vec::new(); steps.len()];
        for (ix, streams) in steps.iter().enumerate() {
            if !streams.is_empty() {
                let mut k = 1;
                while let Some(target) = res.get_mut(k * (ix + 1) - 1) {
                    target.extend(streams);
                    k += 1;
                }
            }
        }
        res
    }

    // Produce the raw slot grid: one slot per GCD interval up to the hyper-period.
    fn create_deadline_slots(
        ir: &OORVIR,
        gcd: UOM_Time,
        hyper_period: UOM_Time,
    ) -> std::result::Result<Vec<Vec<Task>>, String> {
        let num_steps = hyper_period.get::<second>() / gcd.get::<second>();
        assert!(num_steps.is_integer());
        let num_steps = num_steps.to_integer() as usize;
        if num_steps >= 10_000_000 {
            return Err("stream frequencies are too incompatible to generate schedule".to_string());
        }
        let mut slot_grid = vec![Vec::new(); num_steps];
        // Fill evaluation slots for global-clock streams.
        for s in ir
            .time_task
            .iter()
            .filter(|tds| tds.locality == PacingLocality::Global)
        {
            let slot_pos = s.evaluation_period().get::<second>() / gcd.get::<second>();
            assert!(slot_pos.is_integer());
            let slot_pos = slot_pos.to_integer() as usize - 1;
            slot_grid[slot_pos].push(Task::Evaluate(s.stream_idx.out_ix()));
        }
        // Fill start slots for constraints with a global-clock start pacing.
        let periodic_starts = ir
            .constraints
            .iter()
            .filter_map(|cs| match &cs.start.pacing {
                PacingNode::GlobalTick(freq) => Some((
                    cs.stream_idx.out_ix(),
                    UOM_Time::new::<second>(freq.get::<uom::si::frequency::hertz>().inv()),
                )),
                _ => None,
            });
        for (out_idx, period) in periodic_starts {
            let slot_pos = period.get::<second>() / gcd.get::<second>();
            assert!(slot_pos.is_integer());
            let slot_pos = slot_pos.to_integer() as usize - 1;
            slot_grid[slot_pos].push(Task::Start(out_idx));
        }
        // Fill end slots for constraints with a global-clock end pacing (no self-reference).
        let periodic_ends = ir.constraints.iter().filter_map(|cs| {
            if let PacingNode::GlobalTick(freq) = &cs.end.pacing {
                cs.end.has_self_idx.not().then(|| {
                    (
                        cs.stream_idx.out_ix(),
                        UOM_Time::new::<second>(freq.get::<uom::si::frequency::hertz>().inv()),
                    )
                })
            } else {
                None
            }
        });
        for (out_idx, period) in periodic_ends {
            let slot_pos = period.get::<second>() / gcd.get::<second>();
            assert!(slot_pos.is_integer());
            let slot_pos = slot_pos.to_integer() as usize - 1;
            slot_grid[slot_pos].push(Task::End(out_idx));
        }
        Ok(slot_grid)
    }

    // Collapse the filled slot grid into a list of Deadlines, merging empty slots into
    // their successor's `pause` value. The last slot must not be empty.
    fn merge_into_deadlines(gcd: UOM_Time, slot_grid: Vec<Vec<Task>>) -> Vec<Deadline> {
        let mut gap_count = 0i64;
        let mut deadline_list: Vec<Deadline> = vec![];
        for slot in slot_grid.iter() {
            if slot.is_empty() {
                gap_count += 1;
                continue;
            }
            let pause_ns = gcd.get::<nanosecond>() * (gap_count + 1);
            let pause_dur = Duration::from_nanos(pause_ns.to_integer() as u64);
            gap_count = 0i64;
            deadline_list.push(Deadline {
                pause: pause_dur,
                due: slot.clone(),
            });
        }
        // The final slot must never be empty ?the last deadline must close the hyper-period.
        assert!(gap_count == 0i64);
        deadline_list
    }

    fn order_deadline_tasks(ir: &OORVIR, deadlines: &mut Vec<Deadline>) {
        for dl in deadlines {
            dl.due.sort_by_key(|task| match task {
                Task::Evaluate(idx) => ir.constraints[*idx].eval_layer().inner(),
                Task::Start(idx) => ir.constraints[*idx].start_layer().inner(),
                Task::End(_) => usize::MAX,
            });
        }
    }
}
mod math {
    use num::integer::{gcd as num_gcd, lcm as num_lcm};
    use num::rational::Rational64 as Rational;

    // GCD of two rational numbers.
    pub(crate) fn gcd_rational(a: Rational, b: Rational) -> Rational {
        let numer = num_gcd(*a.numer(), *b.numer());
        let denom = num_lcm(*a.denom(), *b.denom());
        Rational::new(numer, denom)
    }

    // LCM of two rational numbers.
    pub(crate) fn lcm_rational(a: Rational, b: Rational) -> Rational {
        let numer = num_lcm(*a.numer(), *b.numer());
        let denom = num_gcd(*a.denom(), *b.denom());
        Rational::new(numer, denom)
    }

    // GCD of a non-empty slice of rational numbers.
    pub(crate) fn gcd_all_rational(v: &[Rational]) -> Rational {
        assert!(!v.is_empty());
        v.iter().fold(v[0], |acc, &r| gcd_rational(acc, r))
    }

    // LCM of a non-empty slice of rational numbers.
    pub(crate) fn lcm_all_rational(v: &[Rational]) -> Rational {
        assert!(!v.is_empty());
        v.iter().fold(v[0], |acc, &r| lcm_rational(acc, r))
    }
}
