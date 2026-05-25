use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io::{self, Write};
use std::marker::PhantomData;

use oorv_core::oorvir::refined::tags::StreamVerbosity;
use oorv_core::oorvir::refined::{StreamIdx, OORVIR};
use oorv_core::runtime::emitter::{BuildableFormatter, OutputFormatter};
use oorv_core::runtime::settings::OutputTimestamp;
use oorv_core::runtime::watcher::{Change, FullDeltaOutput, InstanceKey};
use oorv_core::runtime::Value;
use termcolor::{Ansi, Color, ColorSpec, NoColor, WriteColor};

use super::channel::{StreamAnnotations, WriteChannel};

#[derive(Debug)]
pub struct EventPrinter<OutputTime: OutputTimestamp, W: ColorWriter<Vec<u8>>> {
    output_time: OutputTime,
    verbosity: Verbosity,
    stream_names: HashMap<StreamIdx, String>,
    constraint_ids: HashMap<usize, usize>,
    stream_verbosity: HashMap<StreamIdx, Verbosity>,
    _writer: PhantomData<W>,
}

impl<OutputTime: OutputTimestamp, W: ColorWriter<Vec<u8>>> EventPrinter<OutputTime, W> {
    /// Build a formatter using default verbosity annotations derived from the IR.
    pub fn new(verbosity: Verbosity, ir: &OORVIR) -> Result<Self, String> {
        let annotations = StreamAnnotations::new(ir).map_err(|e| e.to_string())?;
        Self::new_with_annotations(verbosity, ir, annotations)
    }

    /// Build a formatter with explicit per-stream verbosity annotations.
    pub fn new_with_annotations(
        verbosity: Verbosity,
        ir: &OORVIR,
        annotations: StreamAnnotations,
    ) -> Result<Self, String> {
        // Collect stream names for display.
        let stream_names = ir
            .iter_streams()
            .map(|s| (s, ir.resolve_stream(s).name().to_owned()))
            .collect();

        // Map each constraint's output index to its alarm index.
        let constraint_ids = ir
            .alarms
            .iter()
            .map(|t| (t.constrain_idx.out_ix(), t.alarm_idx))
            .collect();

        // Resolve per-stream verbosity from annotations and level tags.
        let stream_verbosity = ir
            .iter_streams()
            .map(|sr| {
                let v = match sr {
                    StreamIdx::Constraint(_) => {
                        let level_tag = ir.constraint(sr).level.as_str();
                        if !level_tag.is_empty() {
                            match level_tag.to_lowercase().as_str() {
                                "info" => Verbosity::Info,
                                "alert" => Verbosity::Alert,
                                "violation" => Verbosity::Violation,
                                "violations" => Verbosity::Violations,
                                _ => Verbosity::from(annotations.verbosity(sr)),
                            }
                        } else {
                            Verbosity::from(annotations.verbosity(sr))
                        }
                    }
                    _ => Verbosity::from(annotations.verbosity(sr)),
                };
                Ok((sr, v))
            })
            .collect::<Result<_, String>>()?;

        Ok(Self {
            output_time: OutputTime::default(),
            verbosity,
            stream_names,
            constraint_ids,
            stream_verbosity,
            _writer: PhantomData,
        })
    }

    /// Wrap this formatter in a [`WriteChannel`] that streams output to `writer`.
    pub fn into_sink<OW: Write>(
        self,
        writer: OW,
    ) -> WriteChannel<OW, Self, FullDeltaOutput, OutputTime, String> {
        WriteChannel::new(writer, self)
    }

    /// Format a parameter list as `<p1, p2>` or an empty string.
    fn fmt_params(params: InstanceKey) -> String {
        match params {
            Some(ps) if !ps.is_empty() => {
                let inner = ps
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("<{inner}>")
            }
            _ => String::new(),
        }
    }

    /// Format a value, quoting string literals with double-quotes.
    fn fmt_val(value: Value) -> String {
        match value {
            Value::Str(s) => format!("\"{s}\""),
            other => other.to_string(),
        }
    }

    /// Write one formatted line when `level <= self.verbosity`.
    fn write_record<F>(&self, out: &mut W, level: Verbosity, ts: &str, body: F) -> io::Result<()>
    where
        F: FnOnce(&mut W) -> io::Result<()>,
    {
        if level <= self.verbosity {
            write!(out, "@{ts} ")?;
            out.set_color(ColorSpec::default().set_fg(Some(level.into())))?;
            body(out)?;
            writeln!(out)?;
            out.reset()?;
        }
        Ok(())
    }

    /// Write a Debug-level lifecycle event (stream start / end).
    fn write_lifecycle<F>(&self, out: &mut W, ts: &str, body: F) -> io::Result<()>
    where
        F: FnOnce(&mut W) -> io::Result<()>,
    {
        self.write_record(out, Verbosity::Debug, ts, body)
    }
}

impl<OutputTime: OutputTimestamp, W: ColorWriter<Vec<u8>>>
    OutputFormatter<FullDeltaOutput, OutputTime> for EventPrinter<OutputTime, W>
{
    type Error = io::Error;
    type Record = String;

    /// Convert one evaluation cycle's verdict into a batch of formatted lines.
    fn format_output(
        &mut self,
        verdict: FullDeltaOutput,
        ts: OutputTime::InnerTime,
    ) -> Result<Self::Record, Self::Error> {
        if self.verbosity == Verbosity::Silent {
            return Ok(String::new());
        }

        let FullDeltaOutput {
            signals,
            outputs,
            constraints: _,
        } = verdict;
        let ts_str = self.output_time.format(ts);
        let mut out = W::for_writer(Vec::new());

        // Emit one line per changed signal value.
        for (idx, val) in signals {
            let sr = StreamIdx::Signal(idx);
            let name = self.stream_names[&sr].clone();
            let lvl = self.stream_verbosity[&sr];
            let fval = Self::fmt_val(val);
            self.write_record(&mut out, lvl, &ts_str, move |w| {
                write!(w, "|   signal   | {name} :: value = {fval}")
            })?;
        }

        // Emit lines for every output / constraint change event.
        for (out_idx, changes) in outputs {
            let sr = StreamIdx::Constraint(out_idx);
            let lvl = self.stream_verbosity[&sr];
            let is_alarm = self.constraint_ids.contains_key(&out_idx);
            let kind_tag = if is_alarm { "constraint" } else { "  output  " };
            let label = if is_alarm {
                format!("#{}", self.constraint_ids[&out_idx])
            } else {
                self.stream_names[&sr].clone()
            };

            for change in changes {
                match change {
                    Change::Activate(params) => {
                        let tag = kind_tag.to_owned();
                        let lbl = label.clone();
                        let ps = Self::fmt_params(Some(params));
                        self.write_lifecycle(&mut out, &ts_str, move |w| {
                            write!(w, "| {tag} | {lbl}{ps} :: start")
                        })?;
                    }
                    Change::Update(params, val) => {
                        let tag = kind_tag.to_owned();
                        let lbl = label.clone();
                        let ps = Self::fmt_params(params);
                        let fval = Self::fmt_val(val);
                        self.write_record(&mut out, lvl, &ts_str, move |w| {
                            write!(w, "| {tag} | {lbl}{ps} :: value = {fval}")
                        })?;
                    }
                    Change::Deactivate(params) => {
                        let tag = kind_tag.to_owned();
                        let lbl = label.clone();
                        let ps = Self::fmt_params(Some(params));
                        self.write_lifecycle(&mut out, &ts_str, move |w| {
                            write!(w, "| {tag} | {lbl}{ps} :: end")
                        })?;
                    }
                }
            }
        }

        out.flush()?;
        String::from_utf8(out.into_inner()).map_err(io::Error::other)
    }
}

impl<OutputTime: OutputTimestamp, W: ColorWriter<Vec<u8>>>
    BuildableFormatter<FullDeltaOutput, OutputTime> for EventPrinter<OutputTime, W>
{
    type CreationData = Verbosity;
    type CreationError = String;

    fn new(ir: &OORVIR, data: Self::CreationData) -> Result<Self, Self::CreationError> {
        Self::new(data, ir)
    }
}

/// Controls which stream events are emitted to the output.
///
/// Variants are ordered from least to most verbose; a record is printed only
/// when its assigned stream level is at most the configured verbosity.
#[derive(PartialEq, Ord, PartialOrd, Eq, Debug, Clone, Copy)]
pub enum Verbosity {
    /// Suppress all output.
    Silent,
    /// High-severity violation (stream-specific).
    Violation,
    /// Constraint violations only.
    Violations,
    /// Alert-level message (stream-specific).
    Alert,
    /// Violations and warnings.
    Warnings,
    /// Informational stream message (stream-specific).
    Info,
    /// Public output streams only.
    Public,
    /// All output and constraint streams.
    Outputs,
    /// All streams, including signals.
    Streams,
    /// Include start/end lifecycle events and fine-grained debug information.
    Debug,
}

impl From<StreamVerbosity> for Verbosity {
    fn from(v: StreamVerbosity) -> Self {
        match v {
            StreamVerbosity::Streams => Verbosity::Streams,
            StreamVerbosity::Outputs => Verbosity::Outputs,
            StreamVerbosity::Public => Verbosity::Public,
            StreamVerbosity::Warnings => Verbosity::Warnings,
            StreamVerbosity::Violations => Verbosity::Violations,
        }
    }
}

impl Display for Verbosity {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Verbosity::Silent => "silent",
            Verbosity::Violation => "violation",
            Verbosity::Violations => "violation",
            Verbosity::Alert => "alert",
            Verbosity::Warnings => "warning",
            Verbosity::Public => "public",
            Verbosity::Info => "info",
            Verbosity::Outputs => "outputs",
            Verbosity::Streams => "stream",
            Verbosity::Debug => "debug",
        };
        write!(f, "{label}")
    }
}

impl From<Verbosity> for Color {
    fn from(v: Verbosity) -> Self {
        match v {
            Verbosity::Silent => unreachable!("silent has no associated colour"),
            Verbosity::Violation => Color::Ansi256(1), // dark red
            Verbosity::Violations => Color::Ansi256(1), // dark red
            Verbosity::Alert => Color::Ansi256(3),     // dark yellow
            Verbosity::Warnings => Color::Ansi256(3),  // dark yellow
            Verbosity::Info => Color::Ansi256(2),      // dark green
            Verbosity::Public => Color::Ansi256(4),    // dark blue
            Verbosity::Outputs => Color::Ansi256(4),   // dark blue
            Verbosity::Streams => Color::Ansi256(5),   // dark magenta
            Verbosity::Debug => Color::Ansi256(8),     // dark grey
        }
    }
}

/// Extends `std::io::Write` with ANSI colour support.
///
/// Two built-in implementations are provided:
/// - [`Ansi`]    — enables ANSI escape-code colour output
/// - [`NoColor`] — strips all colour codes (plain text)
pub trait ColorWriter<W: Write>: WriteColor {
    /// Wrap `write` in a new colour-capable adapter.
    fn for_writer(write: W) -> Self;

    /// Consume this adapter and return the inner writer.
    fn into_inner(self) -> W;
}

impl<W: Write> ColorWriter<W> for Ansi<W> {
    fn for_writer(write: W) -> Self {
        Ansi::new(write)
    }
    fn into_inner(self) -> W {
        Ansi::into_inner(self)
    }
}

impl<W: Write> ColorWriter<W> for NoColor<W> {
    fn for_writer(write: W) -> Self {
        NoColor::new(write)
    }
    fn into_inner(self) -> W {
        NoColor::into_inner(self)
    }
}
