use std::error::Error;
use std::io::Write;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use std::time::SystemTime;

use clap::ValueEnum;
use oorv_core::runtime::async_watcher::{AsyncOutcome, Receiver, RecvTimeoutError};
use oorv_core::runtime::ingest::{
    AssociatedEventFactory, EventFactory, FieldMappedIngester, InputMap,
};
use oorv_core::runtime::refined_ir::OORVIR;
use oorv_core::runtime::settings::{LiveMode, ReplayMode, RunMode as ExecRunMode};
use oorv_core::runtime::settings::{OutputTimestamp, TimestampCodec, WallClock};
use oorv_core::runtime::watcher::{FullDeltaOutput, TracedOutput};
use oorv_core::runtime::AsyncMonitor;

use crate::stream::channel::DataSink;
use crate::stream::feed::{CsvOrigin, InputFeed};
use crate::stream::printer;
use crate::stream::tracker::{CycleWatch, StatsSink};

/// Pulls evaluated verdicts from a channel and forwards them to one or two sinks:
/// - `primary_sink`   the human-readable log (always present)
/// - `stats_sink`     optional live statistics display
pub struct OutputRouter<
    OutputTime: OutputTimestamp,
    W: Write,
    PrimarySink: DataSink<FullDeltaOutput, OutputTime, Error: Error + 'static, Return = ()>,
> {
    /// Main verdict.
    primary_sink: PrimarySink,
    /// Optional live-statistics display; `None` when `--statistics none`.
    stats_sink: Option<StatsSink<W, OutputTime>>,
    _output_time: PhantomData<OutputTime>,
}

impl<
        OutputTime: OutputTimestamp,
        W: Write,
        PrimarySink: DataSink<FullDeltaOutput, OutputTime, Error: Error + 'static, Return = ()>,
    > OutputRouter<OutputTime, W, PrimarySink>
{
    /// Construct a new router.
    ///
    /// - `primary_sink`  mandatory log destination
    /// - `stats_sink`    optional statistics display; pass `None` to disable
    pub fn new(primary_sink: PrimarySink, stats_sink: Option<StatsSink<W, OutputTime>>) -> Self {
        OutputRouter {
            primary_sink,
            stats_sink,
            _output_time: PhantomData,
        }
    }

    /// Route a single verdict to every active sink.
    fn process_verdict(
        &mut self,
        qv: AsyncOutcome<TracedOutput<CycleWatch, FullDeltaOutput>, OutputTime>,
    ) {
        self.primary_sink
            .accept(qv.ts.clone(), qv.verdict.verdict.clone())
            .unwrap();

        if let Some(sink) = &mut self.stats_sink {
            sink.accept(qv.ts, qv.verdict).unwrap();
        }
    }

    fn on_pause(&mut self) {
        if let Some(sink) = &mut self.stats_sink {
            sink.tick_progress();
        }
    }

    /// Main output loop.
    ///
    /// Polls the incoming channel with a short timeout so the progress display
    /// can update even when no new verdicts are available.  Exits once the
    /// monitor disconnects the sender end of the channel.
    pub fn run_output_loop(
        mut self,
        input: Receiver<AsyncOutcome<TracedOutput<CycleWatch, FullDeltaOutput>, OutputTime>>,
    ) {
        loop {
            // Clear the previous progress line before potentially printing new output.
            if let Some(sink) = &mut self.stats_sink {
                sink.erase_progress();
            }

            match input.recv_timeout(Duration::from_millis(100)) {
                Ok(qv) => self.process_verdict(qv),
                Err(RecvTimeoutError::Timeout) => self.on_pause(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        // Print the final statistics summary once the monitor has finished.
        if let Some(sink) = &mut self.stats_sink {
            sink.finalize();
        }
    }
}

/// Selects where the monitor writes its formatted output lines.
#[derive(Debug, Clone, Default)]
pub enum WriteDest {
    /// Emit to standard output (default).
    #[default]
    StdOut,
    /// Emit to standard error.
    StdErr,
    /// Emit to a file at the given path (created or truncated on start-up).
    File(PathBuf),
}

/// Controls whether live throughput statistics are computed during a run.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, ValueEnum, Default)]
pub enum MetricsLevel {
    /// No statistics will be computed or displayed.
    #[default]
    None,
    /// All statistics will be computed and displayed in the terminal.
    All,
}

/// Describes where the monitor reads its input events from.
#[derive(Debug, Clone)]
pub enum InputSpec {
    /// Parse events in CSV format from the given origin.
    Csv {
        /// 1-based column index for timestamps, or `None` to scan the header.
        time_col: Option<usize>,
        /// The specific CSV origin (stdin / file / in-memory).
        origin: CsvOrigin,
    },
}

/// The verbosity level that governs which stream events appear in the output.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, ValueEnum, Default)]
pub enum Verbosity {
    /// Suppresses any kind of logging.
    Silent,
    /// A single, very high-severity violation (output-specific level).
    Violation,
    /// An alert-level message (output-specific level).
    Alert,
    /// Informational messages for streams (output-specific level).
    Info,
    /// Only print alarm violations.
    #[clap(alias = "alarm")]
    Violations,
    /// Print alarm violations and warning alarms.
    Warnings,
    /// Print new stream values for public output streams.
    #[default]
    Public,
    /// Print new stream values for outputs and constraints.
    Outputs,
    /// Print new stream values for every stream.
    Streams,
    Debug,
}

impl TryFrom<Verbosity> for printer::Verbosity {
    type Error = String;

    fn try_from(v: Verbosity) -> Result<Self, Self::Error> {
        let mapped = match v {
            Verbosity::Silent => printer::Verbosity::Silent,
            Verbosity::Info => printer::Verbosity::Info,
            Verbosity::Alert => printer::Verbosity::Alert,
            Verbosity::Violation => printer::Verbosity::Violation,
            Verbosity::Warnings => printer::Verbosity::Warnings,
            Verbosity::Violations => printer::Verbosity::Violations,
            Verbosity::Public => printer::Verbosity::Public,
            Verbosity::Outputs => printer::Verbosity::Outputs,
            Verbosity::Streams => printer::Verbosity::Streams,
            Verbosity::Debug => printer::Verbosity::Debug,
        };
        Ok(mapped)
    }
}

pub struct MonitorSession<
    Source: InputFeed<InputTime>,
    Mode: ExecRunMode<SourceTime = InputTime>,
    InputTime: TimestampCodec,
    OutputTime: OutputTimestamp,
    VerdictSink: DataSink<FullDeltaOutput, OutputTime>,
    W: Write,
> {
    /// The compiled specification in refined IR form.
    pub ir: OORVIR,
    /// The event source that feeds rows into the monitor.
    pub source: Source,
    /// The execution mode (replay / live wall-clock).
    pub mode: Mode,
    /// Phantom marker for the selected output timestamp type.
    pub output_time_repr: PhantomData<OutputTime>,
    /// Optional override for the monitor's notion of time zero.
    pub start_time: Option<SystemTime>,
    /// The primary log sink.
    pub verdict_sink: VerdictSink,
    /// Optional live-statistics sink.
    pub stats_sink: Option<StatsSink<W, OutputTime>>,
}

impl<Source: InputFeed<InputTime> + 'static,InputTime: TimestampCodec,OutputTime: OutputTimestamp,VerdictSink: DataSink<FullDeltaOutput,OutputTime,Return = (),Error: Error + 'static,> + Send + 'static,W: Write + Send + 'static,> MonitorSession<Source, ReplayMode<InputTime>, InputTime, OutputTime, VerdictSink, W>
    where Source::Record: InputMap<CreationData = <<Source::Record as AssociatedEventFactory>::Ingester as EventFactory>::CreationData,>,
{
    pub fn run(self) -> Result<(), Box<dyn Error>> {
        use oorv_core::runtime::settings::RuntimeSpec as RuntimeConfig;

        // Unpack all session fields up front to avoid partial-move issues later.
        let MonitorSession {ir: spec_ir,source: mut event_src,mode: exec_mode,output_time_repr: ts_marker,start_time: epoch_override,verdict_sink: log_sink,stats_sink: metrics_sink,} = self;

        // Wire the primary log sink and optional metrics display into a single dispatcher.
        let dispatcher = OutputRouter::new(log_sink, metrics_sink);

        // Assemble the runtime specification consumed by the monitor engine.
        let exec_spec = RuntimeConfig {ir: spec_ir,mode: exec_mode,output_time_representation: ts_marker,start_time: epoch_override,};

        // Build the queued monitor and bind it to the event factory.
        let mut queued_mon = AsyncMonitor::<FieldMappedIngester<Source::Record>,ReplayMode<InputTime>,TracedOutput<CycleWatch, FullDeltaOutput>,OutputTime,>::initialize(exec_spec, event_src.init_binding()?);

        // Launch the output drain thread before any events are processed.
        let verdict_rx = queued_mon.output_queue();
        let drain_thread = thread::spawn(move || dispatcher.run_output_loop(verdict_rx));

        // Replay all recorded events into the monitor.
        queued_mon.start()?;
        while let Some((ev, stamp)) = event_src.next_record()? {
            queued_mon.submit_event(ev, stamp)?;
        }
        queued_mon.end()?;

        // Block until the drain thread has flushed all remaining output.
        drain_thread.join().expect("output drain thread panicked");
        Ok(())
    }
}

impl<Source: InputFeed<WallClock> + 'static,OutputTime: OutputTimestamp,VerdictSink: DataSink<FullDeltaOutput,OutputTime,Return = (),Error: Error + 'static,> + Send + 'static,W: Write + Send + 'static,> MonitorSession<Source, LiveMode, WallClock, OutputTime, VerdictSink, W>
    where Source::Record: InputMap<CreationData = <<Source::Record as AssociatedEventFactory>::Ingester as EventFactory>::CreationData,>,
{
    pub fn run(self) -> Result<(), Box<dyn Error>> {
        use oorv_core::runtime::settings::RuntimeSpec as RuntimeConfig;

        // Unpack all session fields up front to avoid partial-move issues later.
        let MonitorSession {ir: spec_ir,source: mut event_src,mode: exec_mode,output_time_repr: ts_marker,start_time: epoch_override,verdict_sink: log_sink,stats_sink: metrics_sink,} = self;

        // Wire the primary log sink and optional metrics display into a single dispatcher.
        let dispatcher = OutputRouter::new(log_sink, metrics_sink);

        // Assemble the runtime specification consumed by the monitor engine.
        let exec_spec = RuntimeConfig {ir: spec_ir,mode: exec_mode,output_time_representation: ts_marker,start_time: epoch_override,};

        // Build the queued monitor for live wall-clock mode.
        let mut queued_mon = AsyncMonitor::<FieldMappedIngester<Source::Record>,LiveMode,TracedOutput<CycleWatch, FullDeltaOutput>,OutputTime,>::initialize(exec_spec, event_src.init_binding()?);

        // Launch the output drain thread before any events are processed.
        let verdict_rx   = queued_mon.output_queue();
        let drain_thread = thread::spawn(move || dispatcher.run_output_loop(verdict_rx));

        // Feed live events until the source signals end-of-input.
        queued_mon.start()?;
        while let Some((ev, stamp)) = event_src.next_record()? {
            queued_mon.submit_event(ev, stamp)?;
        }
        queued_mon.end()?;

        // Block until the drain thread has flushed all remaining output.
        drain_thread.join().expect("output drain thread panicked");
        Ok(())
    }
}
