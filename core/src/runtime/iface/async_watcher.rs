//! Multi-threaded (async) stream monitor.
//!
//! [`AsyncMonitor`] runs evaluation in a background thread.

use chrono::{SecondsFormat, Utc};
use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::ops::Not;
use std::rc::Rc;
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use crate::oorvir::refined::{Type, OORVIR};
use crossbeam_channel::{bounded, unbounded, Sender};
pub use crossbeam_channel::{Receiver, RecvError, RecvTimeoutError, TryRecvError};
#[cfg(feature = "serde")]
use serde::Serialize;

use crate::runtime::dispatch::{EventQueue, TaskCoordinator};
use crate::runtime::eval::{EngineSetup, RuntimeEngine};
use crate::runtime::iface::ingest::StreamIngester;
use crate::runtime::iface::watcher::{
    DeltaOutput, EvalOutcome, EvalSnapshot, EvalTracer, OutputFormat,
};
use crate::runtime::settings::{
    LiveMode, OutputTimestamp, RelSeconds, ReplayMode, RunMode, RuntimeSpec, TimestampCodec,
    WallClock,
};
use crate::runtime::Monitor;

// ─── Helper: start a background worker thread ────────────────────────────────

fn start_worker_thread<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    thread::spawn(f)
}

// ─── EvalSource ──────────────────────────────────────────────────────────────

/// Indicates whether an [`AsyncOutcome`] was produced by a timed deadline or
/// an input event.
#[cfg_attr(feature = "serde", derive(Serialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalSource {
    Timed,
    Event,
}

/// Represents the length of a queue used for communication.
#[derive(Debug, Clone, Copy)]
pub enum QueueLength {
    Unbounded,
    Bounded(usize),
}

impl QueueLength {
    fn to_queue<T>(self) -> (Sender<T>, Receiver<T>) {
        match self {
            QueueLength::Unbounded => unbounded(),
            QueueLength::Bounded(cap) => bounded(cap),
        }
    }
}

// ─── AsyncMonitorError ───────────────────────────────────────────────────────

/// Errors that the [`AsyncMonitor`] API can return.
#[derive(Debug)]
pub enum AsyncMonitorError {
    SourceError(Box<dyn Error + Send>),
    ThreadPanic(String),
    ThreadSendError(Box<dyn Any + Send>),
    MultipleStart,
    EventBeforeStart,
}

impl Display for AsyncMonitorError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AsyncMonitorError::SourceError(e) => write!(f, "Event Source error: {}", e),
            AsyncMonitorError::ThreadPanic(reason) => {
                write!(f, "Worker thread hung up: {}", reason)
            }
            AsyncMonitorError::ThreadSendError(msg) => {
                write!(f, "Failed to send message: {:?}", msg)
            }
            AsyncMonitorError::MultipleStart => write!(f, "Multiple start commands sent"),
            AsyncMonitorError::EventBeforeStart => {
                write!(f, "Received an event before a start was called")
            }
        }
    }
}

impl Error for AsyncMonitorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            AsyncMonitorError::SourceError(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

// ─── AsyncOutcome ────────────────────────────────────────────────────────────

/// The outcome of the async monitor.
#[cfg_attr(feature = "serde", derive(Serialize))]
#[derive(Debug, Clone)]
pub struct AsyncOutcome<Verdict: OutputFormat, VerdictTime: OutputTimestamp> {
    pub source: EvalSource,
    pub ts: VerdictTime::InnerTime,
    pub verdict: Verdict,
}

// ─── AsyncMonitor ────────────────────────────────────────────────────────────

/// Multi-threaded stream monitor.
#[allow(missing_debug_implementations)]
pub struct AsyncMonitor<Source, Mode, Verdict = DeltaOutput, VerdictTime = RelSeconds>
where
    Source: StreamIngester,
    Mode: RunMode,
    Verdict: OutputFormat,
    VerdictTime: OutputTimestamp + 'static,
{
    ir: OORVIR,
    worker: Option<JoinHandle<Result<(), AsyncMonitorError>>>,
    input: Sender<WorkItem<Source, Mode::SourceTime>>,
    output: Receiver<AsyncOutcome<Verdict, VerdictTime>>,
}

impl<Source, Mode, Verdict, VerdictTime> AsyncMonitor<Source, Mode, Verdict, VerdictTime>
where
    Source: StreamIngester + 'static,
    Mode: RunMode,
    Verdict: OutputFormat,
    VerdictTime: OutputTimestamp,
{
    fn runner<W: Worker<Source, Mode, Verdict, VerdictTime>>(
        config: RuntimeSpec<Mode, VerdictTime>,
        input_names: HashMap<String, usize>,
        setup_data: Source::CreationData,
        input: Receiver<WorkItem<Source, Mode::SourceTime>>,
        output: Sender<AsyncOutcome<Verdict, VerdictTime>>,
    ) -> Result<(), AsyncMonitorError> {
        let mut worker = W::setup(config, input_names, setup_data, input.clone(), output)?;
        worker.await_launch(&input)?;
        let monitor_start = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        println!("@@MONITOR_START:{}", monitor_start);
        drop(input);
        worker.init()?;
        worker.process()?;
        Ok(())
    }

    fn worker_alive(&mut self) -> Result<(), AsyncMonitorError> {
        if self.worker.is_some() {
            if self.worker.as_ref().unwrap().is_finished() {
                let worker = self.worker.take().unwrap();
                worker
                    .join()
                    .map_err(|e| AsyncMonitorError::ThreadPanic(format!("{:?}", e)))?
            } else {
                Ok(())
            }
        } else {
            Err(AsyncMonitorError::ThreadPanic(
                "Worker thread died.".to_string(),
            ))
        }
    }

    /// Start the evaluation process.
    pub fn launch(&mut self) -> Result<(), AsyncMonitorError> {
        self.worker_alive()?;
        self.input
            .send(WorkItem::Launch)
            .map_err(|msg| AsyncMonitorError::ThreadSendError(Box::new(msg.0)))
    }

    /// Backward-compat alias for [`launch`](AsyncMonitor::launch).
    pub fn start(&mut self) -> Result<(), AsyncMonitorError> {
        self.launch()
    }

    /// Return the [`Receiver`] through which [`AsyncOutcome`] values are delivered.
    pub fn verdict_channel(&self) -> Receiver<AsyncOutcome<Verdict, VerdictTime>> {
        self.output.clone()
    }

    /// Backward-compat alias for [`verdict_channel`](AsyncMonitor::verdict_channel).
    pub fn output_queue(&self) -> Receiver<AsyncOutcome<Verdict, VerdictTime>> {
        self.verdict_channel()
    }

    /// Submit one input event for evaluation.
    pub fn submit_event(
        &mut self,
        ev: Source::Record,
        ts: <Mode::SourceTime as TimestampCodec>::InnerTime,
    ) -> Result<(), AsyncMonitorError> {
        self.worker_alive()?;
        self.input
            .send(WorkItem::Input(ev, ts))
            .map_err(|msg| AsyncMonitorError::ThreadSendError(Box::new(msg.0)))
    }

    /// Shut down the monitor.
    pub fn shutdown(self) -> Result<(), AsyncMonitorError> {
        let AsyncMonitor { worker, input, .. } = self;
        drop(input);
        if let Some(worker) = worker {
            let res = worker
                .join()
                .map_err(|e| AsyncMonitorError::ThreadPanic(format!("{:?}", e)))?;
            let monitor_end = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
            println!("@@MONITOR_END:{}", monitor_end);
            res
        } else {
            Ok(())
        }
    }

    /// Backward-compat alias for [`shutdown`](AsyncMonitor::shutdown).
    pub fn end(self) -> Result<(), AsyncMonitorError> {
        self.shutdown()
    }

    pub fn ir(&self) -> &OORVIR {
        &self.ir
    }

    pub fn input_name(&self, id: usize) -> &str {
        self.ir.signals[id].name.as_str()
    }

    pub fn name_for_input(&self, id: usize) -> &str {
        self.input_name(id)
    }

    pub fn output_name(&self, id: usize) -> &str {
        self.ir.constraints[id].name.as_str()
    }

    pub fn name_for_output(&self, id: usize) -> &str {
        self.output_name(id)
    }

    pub fn constraint_stream_index(&self, id: usize) -> usize {
        self.ir.alarms[id].constrain_idx.out_ix()
    }

    pub fn input_count(&self) -> usize {
        self.ir.signals.len()
    }
    pub fn number_of_input_streams(&self) -> usize {
        self.input_count()
    }
    pub fn output_count(&self) -> usize {
        self.ir.constraints.len()
    }
    pub fn number_of_output_streams(&self) -> usize {
        self.output_count()
    }
    pub fn constraint_count(&self) -> usize {
        self.ir.alarms.len()
    }
    pub fn input_type(&self, id: usize) -> &Type {
        &self.ir.signals[id].annotation
    }
    pub fn type_of_input(&self, id: usize) -> &Type {
        self.input_type(id)
    }
    pub fn output_type(&self, id: usize) -> &Type {
        &self.ir.constraints[id].annotation
    }
    pub fn type_of_output(&self, id: usize) -> &Type {
        self.output_type(id)
    }

    pub fn output_period(&self, id: usize) -> Option<Duration> {
        self.ir
            .time_task
            .iter()
            .find(|td| td.stream_idx.out_ix() == id)
            .map(|td| td.period_as_duration())
    }

    pub fn extend_rate_of_output(&self, id: usize) -> Option<Duration> {
        self.output_period(id)
    }
}

impl<Source, SourceTime, Verdict, VerdictTime>
    AsyncMonitor<Source, ReplayMode<SourceTime>, Verdict, VerdictTime>
where
    Source: StreamIngester + 'static,
    SourceTime: TimestampCodec,
    Verdict: OutputFormat,
    VerdictTime: OutputTimestamp,
{
    pub fn with_capacity(
        config: RuntimeSpec<ReplayMode<SourceTime>, VerdictTime>,
        setup_data: Source::CreationData,
        input_queue_bound: QueueLength,
        output_queue_bound: QueueLength,
    ) -> AsyncMonitor<Source, ReplayMode<SourceTime>, Verdict, VerdictTime> {
        let config_clone = config.clone();

        let input_map = config
            .ir
            .signals
            .iter()
            .map(|i| (i.name.clone(), i.stream_idx.in_ix()))
            .collect();

        let (input_send, input_rcv) = input_queue_bound.to_queue();
        let (output_send, output_rcv) = output_queue_bound.to_queue();

        let worker = start_worker_thread(move || {
            Self::runner::<OfflineWorker<Source, SourceTime, Verdict, VerdictTime>>(
                config_clone,
                input_map,
                setup_data,
                input_rcv,
                output_send,
            )
        });

        AsyncMonitor {
            ir: config.ir,
            worker: Some(worker),
            input: input_send,
            output: output_rcv,
        }
    }

    pub fn bounded_setup(
        config: RuntimeSpec<ReplayMode<SourceTime>, VerdictTime>,
        setup_data: Source::CreationData,
        input_queue_bound: QueueLength,
        output_queue_bound: QueueLength,
    ) -> AsyncMonitor<Source, ReplayMode<SourceTime>, Verdict, VerdictTime> {
        Self::with_capacity(config, setup_data, input_queue_bound, output_queue_bound)
    }

    pub fn initialize(
        config: RuntimeSpec<ReplayMode<SourceTime>, VerdictTime>,
        setup_data: Source::CreationData,
    ) -> AsyncMonitor<Source, ReplayMode<SourceTime>, Verdict, VerdictTime> {
        Self::with_capacity(
            config,
            setup_data,
            QueueLength::Unbounded,
            QueueLength::Unbounded,
        )
    }
}

impl<Source, Verdict, VerdictTime> AsyncMonitor<Source, LiveMode, Verdict, VerdictTime>
where
    Source: StreamIngester + 'static,
    Verdict: OutputFormat,
    VerdictTime: OutputTimestamp,
{
    pub fn with_capacity(
        config: RuntimeSpec<LiveMode, VerdictTime>,
        setup_data: Source::CreationData,
        input_queue_bound: QueueLength,
        output_queue_bound: QueueLength,
    ) -> AsyncMonitor<Source, LiveMode, Verdict, VerdictTime> {
        let config_clone = config.clone();

        let input_map = config
            .ir
            .signals
            .iter()
            .map(|i| (i.name.clone(), i.stream_idx.in_ix()))
            .collect();

        let (input_send, input_rcv) = input_queue_bound.to_queue();
        let (output_send, output_rcv) = output_queue_bound.to_queue();

        let worker = start_worker_thread(move || {
            Self::runner::<OnlineWorker<Source, Verdict, VerdictTime>>(
                config_clone,
                input_map,
                setup_data,
                input_rcv,
                output_send,
            )
        });

        AsyncMonitor {
            ir: config.ir,
            worker: Some(worker),
            input: input_send,
            output: output_rcv,
        }
    }

    pub fn bounded_setup(
        config: RuntimeSpec<LiveMode, VerdictTime>,
        setup_data: Source::CreationData,
        input_queue_bound: QueueLength,
        output_queue_bound: QueueLength,
    ) -> AsyncMonitor<Source, LiveMode, Verdict, VerdictTime> {
        Self::with_capacity(config, setup_data, input_queue_bound, output_queue_bound)
    }

    pub fn initialize(
        config: RuntimeSpec<LiveMode, VerdictTime>,
        setup_data: Source::CreationData,
    ) -> AsyncMonitor<Source, LiveMode, Verdict, VerdictTime> {
        Self::with_capacity(
            config,
            setup_data,
            QueueLength::Unbounded,
            QueueLength::Unbounded,
        )
    }
}

// ─── WorkItem ────────────────────────────────────────────────────────────────

enum WorkItem<Source: StreamIngester, SourceTime: TimestampCodec> {
    Launch,
    Input(Source::Record, SourceTime::InnerTime),
}

// ─── Worker trait ────────────────────────────────────────────────────────────

trait Worker<Source, Mode, Verdict, VerdictTime>: Sized
where
    Source: StreamIngester,
    Mode: RunMode,
    Verdict: OutputFormat,
    VerdictTime: OutputTimestamp + 'static,
{
    fn setup(
        config: RuntimeSpec<Mode, VerdictTime>,
        input_names: HashMap<String, usize>,
        setup_data: Source::CreationData,
        input: Receiver<WorkItem<Source, Mode::SourceTime>>,
        output: Sender<AsyncOutcome<Verdict, VerdictTime>>,
    ) -> Result<Self, AsyncMonitorError>;

    fn await_launch(
        &mut self,
        input: &Receiver<WorkItem<Source, Mode::SourceTime>>,
    ) -> Result<(), AsyncMonitorError> {
        match input.recv() {
            Ok(WorkItem::Launch) => Ok(()),
            Ok(WorkItem::Input(_, _)) => Err(AsyncMonitorError::EventBeforeStart),
            Err(_) => Ok(()),
        }
    }

    fn init(&mut self) -> Result<(), AsyncMonitorError>;

    fn process(&mut self) -> Result<(), AsyncMonitorError>;

    fn try_send(
        output: &Sender<AsyncOutcome<Verdict, VerdictTime>>,
        verdict: Option<AsyncOutcome<Verdict, VerdictTime>>,
    ) -> Result<(), AsyncMonitorError> {
        if let Some(verdict) = verdict {
            output
                .send(verdict)
                .map_err(|e| AsyncMonitorError::ThreadSendError(Box::new(e.0)))
        } else {
            Ok(())
        }
    }
}

// ─── OnlineWorker ────────────────────────────────────────────────────────────

struct OnlineWorker<Source, Verdict, VerdictTime>
where
    Source: StreamIngester,
    Verdict: OutputFormat,
    VerdictTime: OutputTimestamp + 'static,
{
    source: Source,
    source_time: WallClock,
    output_time: Option<VerdictTime>,
    start_time: Option<SystemTime>,
    schedule_manager: TaskCoordinator,
    evaluator: RuntimeEngine,
    input: Receiver<WorkItem<Source, WallClock>>,
    output: Sender<AsyncOutcome<Verdict, VerdictTime>>,
}

impl<Source: StreamIngester, Verdict: OutputFormat, VerdictTime: OutputTimestamp>
    Worker<Source, LiveMode, Verdict, VerdictTime> for OnlineWorker<Source, Verdict, VerdictTime>
{
    fn setup(
        config: RuntimeSpec<LiveMode, VerdictTime>,
        input_names: HashMap<String, usize>,
        setup_data: Source::CreationData,
        input: Receiver<WorkItem<Source, WallClock>>,
        output: Sender<AsyncOutcome<Verdict, VerdictTime>>,
    ) -> Result<Self, AsyncMonitorError> {
        let source_time = config.mode.clock().clone();
        let source = Source::build(input_names, setup_data)
            .map_err(|e| AsyncMonitorError::SourceError(Box::new(e)))?;

        let dyn_schedule = Rc::new(RefCell::new(EventQueue::new()));
        let eval_data = EngineSetup::build(config.ir.clone(), dyn_schedule.clone());
        let schedule_manager = TaskCoordinator::build(config.ir.clone(), dyn_schedule)
            .expect("Error computing schedule for time-driven streams");
        let evaluator = eval_data.into_engine();

        Ok(OnlineWorker {
            source,
            source_time,
            output_time: None,
            start_time: config.start_time,
            schedule_manager,
            evaluator,
            input,
            output,
        })
    }

    fn init(&mut self) -> Result<(), AsyncMonitorError> {
        let st = self.source_time.init_anchor(self.start_time);
        let mut ot = VerdictTime::default();
        ot.adopt_anchor(st);
        self.output_time.replace(ot);
        Ok(())
    }

    fn process(&mut self) -> Result<(), AsyncMonitorError> {
        let output_time = self
            .output_time
            .as_mut()
            .expect("Init to be executed before process");
        let mut durations_ms: Vec<f64> = Vec::new();
        let mut first_single_start: Option<SystemTime> = None;
        let mut last_single_end: Option<SystemTime> = None;
        loop {
            let next_deadline = self.schedule_manager.earliest_due();
            let item = if let Some(due) = next_deadline {
                let now = self.source_time.decode(());
                let wait_time = if due <= now {
                    Duration::ZERO
                } else {
                    due - now
                };
                self.input.recv_timeout(wait_time)
            } else {
                self.input
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected)
            };
            let verdict = match item {
                Ok(WorkItem::Input(e, ts)) => {
                    let start_sys = SystemTime::now();
                    let single_start = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
                    if first_single_start.is_none() {
                        first_single_start = Some(start_sys);
                    }
                    let mut tracer = Verdict::Tracing::default();
                    tracer.parse_begin();
                    let e = self
                        .source
                        .ingest(e)
                        .map_err(|e| AsyncMonitorError::SourceError(Box::new(e)))?;
                    tracer.parse_end();
                    let ts = self.source_time.decode(ts);
                    tracer.eval_begin();
                    self.evaluator.process_event(&e, ts, &mut tracer);
                    tracer.eval_end();

                    let verdict =
                        Verdict::from_snapshot_traced(EvalSnapshot::from(&self.evaluator), tracer);
                    let out_ts = output_time.encode(ts);
                    let end_sys = SystemTime::now();
                    let single_end = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
                    last_single_end = Some(end_sys);
                    let dur = end_sys.duration_since(start_sys).unwrap_or_default();
                    let dur_ns = dur.as_nanos();
                    let dur_ms = (dur.as_secs_f64()) * 1000.0;
                    durations_ms.push(dur_ms);
                    println!("@@TIME_PROC START:{} END:{} DURATION_MS:{:.3} DURATION_NS:{} KIND:Event TS_OUT:{:?}", single_start, single_end, dur_ms, dur_ns, out_ts);
                    verdict.is_empty().not().then_some(AsyncOutcome {
                        source: EvalSource::Event,
                        ts: out_ts,
                        verdict,
                    })
                }
                Err(RecvTimeoutError::Timeout) => {
                    let start_sys = SystemTime::now();
                    let single_start = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
                    let mut tracer = Verdict::Tracing::default();
                    tracer.eval_begin();
                    let due = next_deadline.expect("timeout to only happen for a deadline.");

                    let deadline = self.schedule_manager.collect_due_work(due);
                    self.evaluator
                        .process_timed_tasks(deadline, due, &mut tracer);
                    tracer.eval_end();

                    let verdict =
                        Verdict::from_snapshot_traced(EvalSnapshot::from(&self.evaluator), tracer);
                    let out_ts = output_time.encode(due);
                    let end_sys = SystemTime::now();
                    let single_end = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
                    last_single_end = Some(end_sys);
                    let dur = end_sys.duration_since(start_sys).unwrap_or_default();
                    let dur_ns = dur.as_nanos();
                    let dur_ms = (dur.as_secs_f64()) * 1000.0;
                    durations_ms.push(dur_ms);
                    println!("@@TIME_PROC START:{} END:{} DURATION_MS:{:.3} DURATION_NS:{} KIND:Timed TS_OUT:{:?}", single_start, single_end, dur_ms, dur_ns, out_ts);
                    verdict.is_empty().not().then_some(AsyncOutcome {
                        source: EvalSource::Timed,
                        ts: out_ts,
                        verdict,
                    })
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let count = durations_ms.len();
                    let avg = if count > 0 {
                        durations_ms.iter().sum::<f64>() / (count as f64)
                    } else {
                        0.0
                    };
                    let (duration_s, throughput) =
                        if let (Some(first), Some(last)) = (first_single_start, last_single_end) {
                            let dur = last
                                .duration_since(first)
                                .map(|d| d.as_secs_f64())
                                .unwrap_or(0.0);
                            if dur > 0.0 {
                                (dur, (count as f64) / dur)
                            } else {
                                (0.0, 0.0)
                            }
                        } else {
                            (0.0, 0.0)
                        };
                    println!("@@TIME_PROC_AVG COUNT:{} AVG_MS:{:.3}", count, avg);
                    println!(
                        "@@TIME_THROUGHPUT COUNT:{} DURATION_S:{:.6} EVENTS_PER_S:{:.3}",
                        count, duration_s, throughput
                    );
                    return Ok(());
                }
                Ok(WorkItem::Launch) => {
                    return Err(AsyncMonitorError::MultipleStart);
                }
            };

            Self::try_send(&self.output, verdict)?;
        }
    }
}

// ─── OfflineWorker ───────────────────────────────────────────────────────────

struct OfflineWorker<Source, SourceTime, Verdict, VerdictTime>
where
    Source: StreamIngester,
    SourceTime: TimestampCodec,
    Verdict: OutputFormat,
    VerdictTime: OutputTimestamp + 'static,
{
    config: RuntimeSpec<ReplayMode<SourceTime>, VerdictTime>,
    setup_data: Source::CreationData,
    monitor: Option<Monitor<Source, ReplayMode<SourceTime>, Verdict, VerdictTime>>,
    input: Receiver<WorkItem<Source, SourceTime>>,
    output: Sender<AsyncOutcome<Verdict, VerdictTime>>,
}

impl<
        Source: StreamIngester,
        SourceTime: TimestampCodec,
        Verdict: OutputFormat,
        VerdictTime: OutputTimestamp,
    > Worker<Source, ReplayMode<SourceTime>, Verdict, VerdictTime>
    for OfflineWorker<Source, SourceTime, Verdict, VerdictTime>
{
    fn setup(
        config: RuntimeSpec<ReplayMode<SourceTime>, VerdictTime>,
        _input_names: HashMap<String, usize>,
        setup_data: Source::CreationData,
        input: Receiver<WorkItem<Source, SourceTime>>,
        output: Sender<AsyncOutcome<Verdict, VerdictTime>>,
    ) -> Result<Self, AsyncMonitorError> {
        Ok(OfflineWorker {
            config,
            setup_data,
            monitor: None,
            input,
            output,
        })
    }

    fn init(&mut self) -> Result<(), AsyncMonitorError> {
        let monitor: Monitor<Source, ReplayMode<SourceTime>, Verdict, VerdictTime> =
            Monitor::initialize(self.config.clone(), self.setup_data.clone())
                .map_err(|e| AsyncMonitorError::SourceError(Box::new(e)))?;
        self.monitor.replace(monitor);
        Ok(())
    }

    fn process(&mut self) -> Result<(), AsyncMonitorError> {
        let monitor = self
            .monitor
            .as_mut()
            .expect("Init to be called before process");
        let mut durations_ms: Vec<f64> = Vec::new();
        let mut first_single_start: Option<SystemTime> = None;
        let mut last_single_end: Option<SystemTime> = None;
        let mut last_event = None;
        let mut done = false;
        while !done {
            match self.input.recv() {
                Ok(WorkItem::Input(e, ts)) => {
                    last_event.replace(ts.clone());
                    let start_sys = SystemTime::now();
                    if first_single_start.is_none() {
                        first_single_start = Some(start_sys);
                    }
                    let EvalOutcome { timed, event, ts } = monitor
                        .handle_event(e, ts)
                        .map_err(|e| AsyncMonitorError::SourceError(Box::new(e)))?;
                    for (ts, v) in timed {
                        let outcome = AsyncOutcome {
                            source: EvalSource::Timed,
                            ts,
                            verdict: v,
                        };
                        Self::try_send(&self.output, Some(outcome))?;
                    }
                    if !event.is_empty() {
                        let outcome = AsyncOutcome {
                            source: EvalSource::Event,
                            ts: ts.clone(),
                            verdict: event,
                        };
                        Self::try_send(&self.output, Some(outcome))?;
                    }
                    let end_sys = SystemTime::now();
                    last_single_end = Some(end_sys);
                    let dur = end_sys.duration_since(start_sys).unwrap_or_default();
                    let dur_ms = (dur.as_secs_f64()) * 1000.0;
                    durations_ms.push(dur_ms);
                }
                Err(_) => {
                    done = true;
                    if let Some(last_event) = last_event.as_ref() {
                        let timed = monitor.advance_clock(last_event.clone());
                        for (ts, v) in timed {
                            let outcome = AsyncOutcome {
                                source: EvalSource::Timed,
                                ts,
                                verdict: v,
                            };
                            Self::try_send(&self.output, Some(outcome))?;
                        }
                    }
                }
                Ok(WorkItem::Launch) => {
                    return Err(AsyncMonitorError::MultipleStart);
                }
            }
        }
        let count = durations_ms.len();
        let avg = if count > 0 {
            durations_ms.iter().sum::<f64>() / (count as f64)
        } else {
            0.0
        };
        let (duration_s, throughput) =
            if let (Some(first), Some(last)) = (first_single_start, last_single_end) {
                let dur = last
                    .duration_since(first)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                if dur > 0.0 {
                    (dur, (count as f64) / dur)
                } else {
                    (0.0, 0.0)
                }
            } else {
                (0.0, 0.0)
            };
        println!("@@TIME_PROC_AVG COUNT:{} AVG_MS:{:.3}", count, avg);
        println!(
            "@@TIME_THROUGHPUT COUNT:{} DURATION_S:{:.6} EVENTS_PER_S:{:.3}",
            count, duration_s, throughput
        );
        Ok(())
    }
}
