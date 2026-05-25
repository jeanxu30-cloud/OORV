use std::convert::Infallible;
use std::io::Write;
use std::marker::PhantomData;
use std::time::{Duration, Instant};

use crossterm::cursor::MoveToPreviousLine;
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};
use oorv_core::oorvir::refined::OORVIR;
use oorv_core::runtime::emitter::{BuildableFormatter, OutputFormatter};
use oorv_core::runtime::settings::OutputTimestamp;
use oorv_core::runtime::watcher::{EvalTracer, FullDeltaOutput, TracedOutput};

use super::channel::DataSink;

/// Records wall-clock timestamps bracketing the parse and evaluation phases
/// of a single monitoring cycle, enabling per-cycle latency measurements.
#[derive(Debug, Clone, Copy, Default)]
pub struct CycleWatch {
    input_begin: Option<Instant>,
    input_finish: Option<Instant>,
    eval_begin: Option<Instant>,
    eval_finish: Option<Instant>,
}

impl CycleWatch {
    /// Total wall-clock time spent in the evaluation phase for this cycle.
    pub fn cycle_elapsed(&self) -> Duration {
        self.eval_finish
            .unwrap()
            .duration_since(self.eval_begin.unwrap())
    }

    /// Wall-clock time spent parsing the input event for this cycle, or `None`
    /// when the cycle was timer-triggered (no input event was parsed).
    pub fn input_elapsed(&self) -> Option<Duration> {
        self.input_finish
            .and_then(|fin| self.input_begin.map(|beg| fin.duration_since(beg)))
    }
}

impl EvalTracer for CycleWatch {
    fn parse_begin(&mut self) {
        self.input_begin.replace(Instant::now());
    }
    fn parse_end(&mut self) {
        self.input_finish.replace(Instant::now());
    }
    fn eval_begin(&mut self) {
        self.eval_begin.replace(Instant::now());
    }
    fn eval_end(&mut self) {
        self.eval_finish.replace(Instant::now());
    }
}

/// Accumulates raw counters across evaluation cycles.
///
/// After each cycle, call [`record_cycle`] with the associated [`CycleWatch`].
/// Use [`snapshot`] to obtain a point-in-time [`MetricsSnapshot`].
#[derive(Debug, Clone)]
pub struct MetricsAccumulator {
    cycle_count: u128,
    event_count: u128,
    eval_duration: Duration,
    parse_duration: Duration,
    constraint_hits: Vec<u64>,
}

impl MetricsAccumulator {
    /// Create a new accumulator for a spec that contains `num_constraints` alarms.
    pub fn new(num_constraints: usize) -> Self {
        Self {
            cycle_count: 0,
            event_count: 0,
            eval_duration: Duration::default(),
            parse_duration: Duration::default(),
            constraint_hits: vec![0; num_constraints],
        }
    }

    /// Build a snapshot from the current counter values.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let cycles_per_sec = (self.cycle_count > 0).then(|| {
            (self.cycle_count * Duration::from_secs(1).as_nanos()) / self.eval_duration.as_nanos()
        });
        let nanos_per_cycle =
            (self.cycle_count > 0).then(|| self.eval_duration.as_nanos() / self.cycle_count);

        MetricsSnapshot {
            cycles_per_sec,
            nanos_per_cycle,
            cycle_count: self.cycle_count,
            event_count: self.event_count,
            constraint_hits: self.constraint_hits.clone(),
            eval_duration: self.eval_duration,
            parse_duration: self.parse_duration,
        }
    }

    /// Incorporate the timing data from one completed evaluation cycle.
    pub(crate) fn record_cycle(&mut self, watch: CycleWatch) {
        self.eval_duration += watch.cycle_elapsed();
        if let Some(parse_dur) = watch.input_elapsed() {
            self.event_count += 1;
            self.parse_duration += parse_dur;
        }
        self.cycle_count += 1;
    }

    /// Increment the hit counter for the given constraint (alarm) index.
    fn record_constraint(&mut self, constraint_idx: usize) {
        self.constraint_hits[constraint_idx] += 1;
    }
}

/// An immutable snapshot of accumulated statistics at one point in time.
#[derive(Clone, Debug)]
pub struct MetricsSnapshot {
    /// Estimated evaluation cycles per second (None until at least one cycle).
    pub cycles_per_sec: Option<u128>,
    /// Average nanoseconds per cycle (None until at least one cycle).
    pub nanos_per_cycle: Option<u128>,
    /// Total evaluation cycles completed so far.
    pub cycle_count: u128,
    /// Total input events accepted so far.
    pub event_count: u128,
    /// Per-constraint hit counts indexed by alarm index.
    pub constraint_hits: Vec<u64>,
    /// Cumulative wall-time spent on evaluation.
    #[allow(dead_code)]
    pub eval_duration: Duration,
    /// Cumulative wall-time spent on input parsing.
    pub parse_duration: Duration,
}

impl<OutputTime: OutputTimestamp>
    OutputFormatter<TracedOutput<CycleWatch, FullDeltaOutput>, OutputTime> for MetricsAccumulator
{
    type Error = Infallible;
    type Record = MetricsSnapshot;

    fn format_output(
        &mut self,
        traced: TracedOutput<CycleWatch, FullDeltaOutput>,
        _ts: OutputTime::InnerTime,
    ) -> Result<Self::Record, Self::Error> {
        let TracedOutput { tracer, verdict } = traced;
        self.record_cycle(tracer);
        for constraint_idx in verdict.constraints {
            self.record_constraint(constraint_idx);
        }
        Ok(self.snapshot())
    }
}

impl<OutputTime: OutputTimestamp>
    BuildableFormatter<TracedOutput<CycleWatch, FullDeltaOutput>, OutputTime>
    for MetricsAccumulator
{
    type CreationData = usize;
    type CreationError = Infallible;

    fn new(_ir: &OORVIR, data: Self::CreationData) -> Result<Self, Self::CreationError> {
        Ok(Self::new(data))
    }
}

/// A [`DataSink`] that renders a continuously-updated statistics overlay
/// in the terminal while the monitor is running.
///
/// Each accepted verdict refreshes the overlay in-place.  When the run
/// finishes, [`finalize`] prints a permanent summary.
#[derive(Debug)]
pub struct StatsSink<W: Write, O: OutputTimestamp> {
    spinner_chars: [char; 4],
    spinner_pos: usize,
    term_width: u16,
    last_refresh: Instant,
    writer: W,
    accumulator: MetricsAccumulator,
    live_snap: MetricsSnapshot,
    display_snap: MetricsSnapshot,
    _output_time: PhantomData<O>,
}

impl<W: Write, O: OutputTimestamp> DataSink<TracedOutput<CycleWatch, FullDeltaOutput>, O>
    for StatsSink<W, O>
{
    type Error = Infallible;
    type Factory = MetricsAccumulator;
    type Return = ();

    fn flush_record(&mut self, snap: MetricsSnapshot) -> Result<Self::Return, Self::Error> {
        self.live_snap = snap;
        self.tick_progress();
        Ok(())
    }

    fn factory(&mut self) -> &mut Self::Factory {
        &mut self.accumulator
    }
}

impl<W: Write, O: OutputTimestamp> StatsSink<W, O> {
    /// Initialise a new sink that writes statistics output to `writer`.
    ///
    /// `num_constraints` must equal the number of alarm streams in the spec.
    pub fn new(num_constraints: usize, writer: W) -> Self {
        let accumulator = MetricsAccumulator::new(num_constraints);
        let initial = accumulator.snapshot();
        Self {
            spinner_chars: [' ', ' ', ' ', ' '],
            spinner_pos: 0,
            term_width: crossterm::terminal::size().map(|v| v.0).unwrap_or(70),
            last_refresh: Instant::now(),
            writer,
            live_snap: initial.clone(),
            display_snap: initial,
            accumulator,
            _output_time: PhantomData,
        }
    }

    /// Advance the spinner and refresh the cached snapshot if enough time has passed.
    fn advance_display(&mut self) {
        self.display_snap = self.live_snap.clone();
        self.spinner_pos = (self.spinner_pos + 1) % self.spinner_chars.len();
        self.last_refresh = Instant::now();
    }

    fn spinner_char(&self) -> char {
        self.spinner_chars[self.spinner_pos]
    }

    /// Redraw the live statistics overlay (3 terminal lines).
    pub fn tick_progress(&mut self) {
        if self.last_refresh.elapsed() >= Duration::from_millis(250) {
            self.advance_display();
        }
        writeln!(self.writer, "{}", " ".repeat(self.term_width as usize)).unwrap_or(());
        self.render_cycle_line(self.spinner_char());
        self.render_constraint_line(true);
    }

    /// Print the final, permanent statistics summary.
    pub fn finalize(&mut self) {
        self.display_snap = self.live_snap.clone();
        writeln!(self.writer, "{}", " ".repeat(self.term_width as usize)).unwrap_or(());
        self.render_cycle_line(' ');
        self.render_event_line();
        self.render_constraint_line(false);
    }

    /// Erase the overlay lines printed by the previous [`tick_progress`] call.
    pub fn erase_progress(&mut self) {
        execute!(
            self.writer,
            MoveToPreviousLine(3u16),
            Clear(ClearType::FromCursorDown)
        )
        .unwrap_or(());
    }

    fn render_cycle_line(&mut self, spin: char) {
        if self.display_snap.cycle_count > 0 {
            writeln!(
                self.writer,
                "{spin} {} cycles  |  {} cyc/s  |  {} ns/cyc",
                self.display_snap.cycle_count,
                self.display_snap.cycles_per_sec.unwrap(),
                self.display_snap.nanos_per_cycle.unwrap(),
            )
            .unwrap_or(());
        } else {
            writeln!(
                self.writer,
                "{spin} {} cycles",
                self.display_snap.cycle_count
            )
            .unwrap_or(());
        }
    }

    fn render_event_line(&mut self) {
        if self.display_snap.event_count > 0 {
            writeln!(
                self.writer,
                "  {} input events parsed in {:.3}s  |  {} ns/event avg",
                self.display_snap.event_count,
                self.display_snap.parse_duration.as_secs_f64(),
                self.display_snap.nanos_per_cycle.unwrap_or(0),
            )
            .unwrap_or(());
        }
    }

    fn render_constraint_line(&mut self, brief: bool) {
        let total: u64 = self.display_snap.constraint_hits.iter().sum();
        if brief {
            writeln!(self.writer, "  {total} constraint hits").unwrap_or(());
        } else {
            writeln!(self.writer, "  {total} constraint hits total").unwrap_or(());
            writeln!(self.writer, "  Breakdown:").unwrap_or(());
            for (idx, hits) in self.display_snap.constraint_hits.iter().enumerate() {
                writeln!(self.writer, "   [#{idx}]: {hits}").unwrap_or(());
            }
        }
    }
}
