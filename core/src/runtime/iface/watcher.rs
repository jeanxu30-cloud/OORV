use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Duration;

use crate::oorvir::refined::{Type, OORVIR};
use itertools::Itertools;

use crate::runtime::dispatch::{EventQueue, TaskCoordinator};
use crate::runtime::eval::{EngineSetup, RuntimeEngine};
use crate::runtime::iface::ingest::{IngestionError, StreamIngester};
use crate::runtime::settings::{OutputTimestamp, RelSeconds, RunMode, RuntimeSpec, TimestampCodec};
use crate::runtime::store::Value;
use crate::runtime::CondSerialize;

pub type Event = Vec<Value>;

// ─── Change ──────────────────────────────────────────────────────────────────

/// Describes one observable state transition on a stream.
#[cfg_attr(feature = "serde", derive(Serialize))]
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub enum Change {
    /// A new stream instance was activated with the given parameters.
    Activate(Vec<Value>),
    /// An existing instance received a new value.
    Update(InstanceKey, Value),
    /// An instance was deactivated.
    Deactivate(Vec<Value>),
}

impl Display for Change {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Change::Activate(params) => write!(f, "Activate<{}>", params.iter().join(", ")),
            Change::Deactivate(params) => write!(f, "Deactivate<{}>", params.iter().join(", ")),
            Change::Update(key, value) => match key {
                Some(params) => write!(f, "Instance<{}> = {}", params.iter().join(", "), value),
                None => write!(f, "Value = {}", value),
            },
        }
    }
}

// ─── Built-in Output Formats ─────────────────────────────────────────────────

/// Incremental output: only the streams that changed this cycle.
pub type DeltaOutput = Vec<(usize, Vec<Change>)>;

impl OutputFormat for DeltaOutput {
    type Tracing = NullTracer;

    fn from_snapshot(data: EvalSnapshot) -> Self {
        data.engine.snapshot_output_changes()
    }

    fn is_empty(&self) -> bool {
        Vec::is_empty(self)
    }
}

/// Combined incremental output grouping changed signals, outputs and
/// constraint violations into separate collections.
#[cfg_attr(feature = "serde", derive(Serialize))]
#[derive(Debug, Clone)]
pub struct FullDeltaOutput {
    /// Input streams that received a new value this cycle.
    pub signals: Vec<(usize, Value)>,
    /// Output streams with their [`Change`] list.
    pub outputs: Vec<(usize, Vec<Change>)>,
    /// Indices of constraints that were violated this cycle.
    pub constraints: Vec<usize>,
}

impl OutputFormat for FullDeltaOutput {
    type Tracing = NullTracer;

    fn from_snapshot(data: EvalSnapshot) -> Self {
        let signals = data.engine.snapshot_new_inputs();
        let outputs = data.engine.snapshot_output_changes();
        let constraints = data.engine.snapshot_active_constraints();
        Self {
            signals,
            outputs,
            constraints,
        }
    }

    fn is_empty(&self) -> bool {
        self.signals.is_empty() && self.outputs.is_empty() && self.constraints.is_empty()
    }
}

/// Full snapshot output: the current value of every input and output stream.
#[cfg_attr(feature = "serde", derive(Serialize))]
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SnapshotOutput {
    /// Current value of each input stream (indexed by stream position).
    pub signals: Vec<Option<Value>>,

    /// Current instances of each output stream.
    pub output: Vec<Vec<StreamInstance>>,
}

impl OutputFormat for SnapshotOutput {
    type Tracing = NullTracer;

    fn from_snapshot(data: EvalSnapshot) -> Self {
        SnapshotOutput {
            signals: data.engine.snapshot_all_inputs(),
            output: data.engine.snapshot_all_outputs(),
        }
    }

    fn is_empty(&self) -> bool {
        false
    }
}

/// Alert output: violated constraints with their formatted message strings.
pub type ConstraintAlerts = Vec<(usize, InstanceKey, String)>;

impl OutputFormat for ConstraintAlerts {
    type Tracing = NullTracer;

    fn from_snapshot(data: EvalSnapshot) -> Self {
        data.engine.snapshot_alarm_messages()
    }

    fn is_empty(&self) -> bool {
        Vec::is_empty(self)
    }
}

// ─── EvalTracer ──────────────────────────────────────────────────────────────

/// Optional hooks invoked around evaluation phases for profiling or debugging.
pub trait EvalTracer: Default + Clone + Debug + Send + 'static {
    fn parse_begin(&mut self) {}
    fn parse_end(&mut self) {}
    fn eval_begin(&mut self) {}
    fn eval_end(&mut self) {}
    fn activation_begin(&mut self, _output: usize) {}
    fn activation_end(&mut self, _output: usize) {}
    fn instance_begin(&mut self, _output: usize, _instance: &[Value]) {}
    fn instance_end(&mut self, _output: usize, _instance: &[Value]) {}
    fn deactivation_begin(&mut self, _output: usize, _instance: &[Value]) {}
    fn deactivation_end(&mut self, _output: usize, _instance: &[Value]) {}
}

/// A no-op tracer used as the default when tracing is not required.
#[cfg_attr(feature = "serde", derive(Serialize))]
#[derive(Debug, Clone, Copy, Default)]
pub struct NullTracer {}

impl EvalTracer for NullTracer {}

/// Bundles a user-chosen [`EvalTracer`] together with any [`OutputFormat`].
#[cfg_attr(feature = "serde", derive(Serialize))]
#[derive(Debug, Clone)]
pub struct TracedOutput<T: EvalTracer, V: OutputFormat> {
    #[cfg_attr(feature = "serde", serde(skip))]
    pub tracer: T,
    pub verdict: V,
}

impl<T: EvalTracer, V: OutputFormat<Tracing = NullTracer>> OutputFormat for TracedOutput<T, V> {
    type Tracing = T;

    fn from_snapshot(data: EvalSnapshot) -> Self {
        Self {
            tracer: T::default(),
            verdict: V::from_snapshot(data),
        }
    }

    fn from_snapshot_traced(data: EvalSnapshot, tracing: Self::Tracing) -> Self {
        Self {
            tracer: tracing,
            verdict: V::from_snapshot(data),
        }
    }

    fn is_empty(&self) -> bool {
        V::is_empty(&self.verdict)
    }
}

// ─── OutputFormat ────────────────────────────────────────────────────────────

/// Converts a raw evaluator snapshot into a user-visible representation.
pub trait OutputFormat: Clone + Debug + Send + CondSerialize + 'static {
    type Tracing: EvalTracer;

    fn from_snapshot(data: EvalSnapshot) -> Self;

    fn from_snapshot_traced(data: EvalSnapshot, _tracing: Self::Tracing) -> Self {
        Self::from_snapshot(data)
    }

    fn is_empty(&self) -> bool;
}

// ─── Instance Key / Stream Instance ──────────────────────────────────────────

/// Parameter values identifying a stream instance.
pub type InstanceKey = Option<Vec<Value>>;

/// A stream instance: `(key, current value)`.
pub type StreamInstance = (InstanceKey, Option<Value>);

// ─── EvalOutcome / EvalSnapshot ──────────────────────────────────────────────

/// The combined result of one evaluation cycle.
#[cfg_attr(feature = "serde", derive(Serialize))]
#[derive(Debug, Clone)]
pub struct EvalOutcome<V: OutputFormat, VerdictTime: OutputTimestamp> {
    pub timed: Vec<(VerdictTime::InnerTime, V)>,
    pub event: V,
    pub ts: VerdictTime::InnerTime,
}

/// A short-lived reference to the evaluator state after a cycle completes.
#[allow(missing_debug_implementations)]
#[derive(Copy, Clone)]
pub struct EvalSnapshot<'a> {
    pub(crate) engine: &'a RuntimeEngine,
}

impl<'a> From<&'a RuntimeEngine> for EvalSnapshot<'a> {
    fn from(engine: &'a RuntimeEngine) -> Self {
        EvalSnapshot { engine }
    }
}

// ─── Monitor ─────────────────────────────────────────────────────────────────

/// Single-threaded stream monitor.
#[allow(missing_debug_implementations)]
pub struct Monitor<Source, Mode, Verdict = DeltaOutput, VerdictTime = RelSeconds>
where
    Source: StreamIngester,
    Mode: RunMode,
    Verdict: OutputFormat,
    VerdictTime: OutputTimestamp + 'static,
{
    ir: OORVIR,
    engine: RuntimeEngine,
    schedule_manager: TaskCoordinator,
    source: Source,
    source_time: Mode::SourceTime,
    output_time: VerdictTime,
    phantom: PhantomData<Verdict>,
}

// ─── Internal helpers ────────────────────────────────────────────────────────

impl<Source, Mode, Verdict, VerdictTime> Monitor<Source, Mode, Verdict, VerdictTime>
where
    Source: StreamIngester,
    Mode: RunMode,
    Verdict: OutputFormat,
    VerdictTime: OutputTimestamp,
{
    /// Construct a monitor from a fully resolved configuration.
    pub fn initialize(
        config: RuntimeSpec<Mode, VerdictTime>,
        setup_data: Source::CreationData,
    ) -> Result<Monitor<Source, Mode, Verdict, VerdictTime>, IngestionError> {
        let event_queue = Rc::new(RefCell::new(EventQueue::new()));
        let mut source_time = config.mode.clock().clone();
        let mut output_time = VerdictTime::default();

        let start = source_time.init_anchor(config.start_time);
        output_time.adopt_anchor(start);

        let input_map: HashMap<String, usize> = config
            .ir
            .signals
            .iter()
            .map(|s| (s.name.clone(), s.stream_idx.in_ix()))
            .collect();

        let eval_data = EngineSetup::build(config.ir.clone(), event_queue.clone());
        let schedule_mgr = TaskCoordinator::build(config.ir.clone(), event_queue)
            .expect("failed to build schedule for time-driven streams");

        Ok(Monitor {
            ir: config.ir,
            engine: eval_data.into_engine(),
            schedule_manager: schedule_mgr,
            source: Source::build(input_map, setup_data)?,
            source_time,
            output_time,
            phantom: PhantomData,
        })
    }

    fn flush_deadlines(&mut self, ts: Duration, inclusive: bool) -> Vec<(Duration, Verdict)> {
        let mut timed: Vec<(Duration, Verdict)> = Vec::new();
        while let Some(due) = self.schedule_manager.earliest_due() {
            if due > ts || (inclusive && due == ts) {
                break;
            }
            let mut tracer = Verdict::Tracing::default();
            tracer.eval_begin();
            let tasks = self.schedule_manager.collect_due_work(ts);
            self.engine.process_timed_tasks(tasks, due, &mut tracer);
            tracer.eval_end();
            timed.push((
                due,
                Verdict::from_snapshot_traced(EvalSnapshot::from(&self.engine), tracer),
            ));
        }
        timed
    }
}

// ─── Public Monitor API ──────────────────────────────────────────────────────

impl<Source, Mode, Verdict, VerdictTime> Monitor<Source, Mode, Verdict, VerdictTime>
where
    Source: StreamIngester,
    Mode: RunMode,
    Verdict: OutputFormat,
    VerdictTime: OutputTimestamp,
{
    /// Process one input event and return the resulting [`EvalOutcome`].
    pub fn handle_event(
        &mut self,
        rec: Source::Record,
        ts: <Mode::SourceTime as TimestampCodec>::InnerTime,
    ) -> Result<EvalOutcome<Verdict, VerdictTime>, IngestionError> {
        let mut tracer = Verdict::Tracing::default();

        tracer.parse_begin();
        let ev = self.source.ingest(rec)?;
        tracer.parse_end();

        let ts = self.source_time.decode(ts);

        let timed = if self.ir.has_periodic_streams() {
            self.flush_deadlines(ts, true)
        } else {
            vec![]
        };

        tracer.eval_begin();
        self.engine.process_event(ev.as_slice(), ts, &mut tracer);
        tracer.eval_end();

        let event_output = Verdict::from_snapshot_traced(EvalSnapshot::from(&self.engine), tracer);

        let timed = timed
            .into_iter()
            .map(|(t, v)| (self.output_time.encode(t), v))
            .collect();

        Ok(EvalOutcome {
            timed,
            event: event_output,
            ts: self.output_time.encode(ts),
        })
    }

    /// Advance the monitor's clock to `ts`.
    pub fn advance_clock(
        &mut self,
        ts: <Mode::SourceTime as TimestampCodec>::InnerTime,
    ) -> Vec<(VerdictTime::InnerTime, Verdict)> {
        let ts = self.source_time.decode(ts);
        let timed = if self.ir.has_periodic_streams() {
            self.flush_deadlines(ts, false)
        } else {
            vec![]
        };
        timed
            .into_iter()
            .map(|(t, v)| (self.output_time.encode(t), v))
            .collect()
    }

    pub fn ir(&self) -> &OORVIR {
        &self.ir
    }

    pub fn input_name(&self, id: usize) -> &str {
        self.ir.signals[id].name.as_str()
    }

    pub fn output_name(&self, id: usize) -> &str {
        self.ir.constraints[id].name.as_str()
    }

    pub fn constraint_stream_index(&self, id: usize) -> usize {
        self.ir.alarms[id].constrain_idx.out_ix()
    }

    pub fn input_count(&self) -> usize {
        self.ir.signals.len()
    }

    pub fn output_count(&self) -> usize {
        self.ir.constraints.len()
    }

    pub fn constraint_count(&self) -> usize {
        self.ir.alarms.len()
    }

    pub fn input_type(&self, id: usize) -> &Type {
        &self.ir.signals[id].annotation
    }

    pub fn output_type(&self, id: usize) -> &Type {
        &self.ir.constraints[id].annotation
    }

    pub fn output_period(&self, id: usize) -> Option<Duration> {
        self.ir
            .time_task
            .iter()
            .find(|td| td.stream_idx.out_ix() == id)
            .map(|td| td.period_as_duration())
    }

    pub fn reformat<T: OutputFormat>(self) -> Monitor<Source, Mode, T, VerdictTime> {
        let Monitor {
            ir,
            engine,
            schedule_manager,
            source_time,
            source,
            output_time,
            phantom: _,
        } = self;
        Monitor {
            ir,
            engine,
            schedule_manager,
            source_time,
            source,
            output_time,
            phantom: PhantomData,
        }
    }

    pub fn with_verdict_representation<T: OutputFormat>(
        self,
    ) -> Monitor<Source, Mode, T, VerdictTime> {
        self.reformat()
    }
}
