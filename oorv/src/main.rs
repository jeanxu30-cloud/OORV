use std::error::Error;
use std::fs::File;
use std::io::{stderr, stdout, BufWriter};
use std::marker::PhantomData;
use std::time::{Duration, SystemTime};

use clap::Parser;
use oorv_core::compile_refined_file;
use oorv_core::oorvir::refined::OORVIR;
use oorv_core::runtime::settings::{
    DeltaNanos, DeltaSeconds, EpochRfc3339, EpochSeconds, FixedStep, RelNanos, RelSeconds,
    WallClock,
};
use oorv_core::runtime::settings::{LiveMode, ReplayMode, RunMode as ExecRunMode};
use termcolor::{Ansi, NoColor};

use crate::cli::params::{AppArgs, InputTimeFmt, OutputStyle, OutputTimeFmt, RunMode};
use crate::session::{InputSpec, MetricsLevel, MonitorSession, WriteDest};
use crate::stream::feed::CsvFeedReader;
use crate::stream::printer::EventPrinter;
use crate::stream::tracker::StatsSink;
use crate::stream::StreamAnnotations;

mod cli;
mod session;
mod stream;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn Error>> {
    // clap exits automatically on `--help` or invalid arguments.
    let args = AppArgs::parse();

    // Compile the specification and derive ancillary metadata.
    let (refined_ir, annotations, input_spec) = build_pipeline(&args)?;

    // Dispatch into the correct fully-typed execution path.
    launch_monitor(refined_ir, annotations, input_spec, &args)
}

// ---------------------------------------------------------------------------
// Macro dispatch chain
// ---------------------------------------------------------------------------

/// Layer 1 — map an `InputTimeFmt` variant to a concrete `TimeRepresentation`.
macro_rules! resolve_input_time {
    ($fmt:expr, $out_fmt:expr, $ir:expr, $spec:expr,
     $stats:expr, $verb:expr, $dest:expr, $mode:ty, $t0:expr, $style:expr, $ann:expr) => {
        match $fmt {
            InputTimeFmt::RelNanos | InputTimeFmt::RelativeUintNanos => {
                resolve_output_time!(
                    RelNanos::default(),
                    $out_fmt,
                    $ir,
                    $spec,
                    $stats,
                    $verb,
                    $dest,
                    $mode,
                    $t0,
                    $style,
                    $ann
                )
            }
            InputTimeFmt::Relative
            | InputTimeFmt::RelativeSecs
            | InputTimeFmt::RelativeFloatSecs => {
                resolve_output_time!(
                    RelSeconds::default(),
                    $out_fmt,
                    $ir,
                    $spec,
                    $stats,
                    $verb,
                    $dest,
                    $mode,
                    $t0,
                    $style,
                    $ann
                )
            }
            InputTimeFmt::DeltaNanos | InputTimeFmt::OffsetUintNanos => {
                resolve_output_time!(
                    DeltaNanos::default(),
                    $out_fmt,
                    $ir,
                    $spec,
                    $stats,
                    $verb,
                    $dest,
                    $mode,
                    $t0,
                    $style,
                    $ann
                )
            }
            InputTimeFmt::Offset | InputTimeFmt::OffsetSecs | InputTimeFmt::OffsetFloatSecs => {
                resolve_output_time!(
                    DeltaSeconds::default(),
                    $out_fmt,
                    $ir,
                    $spec,
                    $stats,
                    $verb,
                    $dest,
                    $mode,
                    $t0,
                    $style,
                    $ann
                )
            }
            InputTimeFmt::Absolute | InputTimeFmt::AbsoluteUnix => {
                resolve_output_time!(
                    EpochSeconds::default(),
                    $out_fmt,
                    $ir,
                    $spec,
                    $stats,
                    $verb,
                    $dest,
                    $mode,
                    $t0,
                    $style,
                    $ann
                )
            }
            InputTimeFmt::EpochRfc3339 | InputTimeFmt::AbsoluteRfc3339 => {
                resolve_output_time!(
                    EpochRfc3339::default(),
                    $out_fmt,
                    $ir,
                    $spec,
                    $stats,
                    $verb,
                    $dest,
                    $mode,
                    $t0,
                    $style,
                    $ann
                )
            }
        }
    };
}

/// Layer 2 — map an `OutputTimeFmt` variant to a concrete `OutputTimeRepresentation`.
macro_rules! resolve_output_time {
    ($it:expr, $fmt:expr, $ir:expr, $spec:expr,
     $stats:expr, $verb:expr, $dest:expr, $mode:ty, $t0:expr, $style:expr, $ann:expr) => {
        match $fmt {
            OutputTimeFmt::RelNanos | OutputTimeFmt::RelativeUintNanos => {
                resolve_source!(
                    $it, RelNanos, $ir, $spec, $stats, $verb, $dest, $mode, $t0, $style, $ann
                )
            }
            OutputTimeFmt::Relative
            | OutputTimeFmt::RelativeSecs
            | OutputTimeFmt::RelativeFloatSecs => {
                resolve_source!(
                    $it, RelSeconds, $ir, $spec, $stats, $verb, $dest, $mode, $t0, $style, $ann
                )
            }
            OutputTimeFmt::Absolute | OutputTimeFmt::AbsoluteUnix => {
                resolve_source!(
                    $it,
                    EpochSeconds,
                    $ir,
                    $spec,
                    $stats,
                    $verb,
                    $dest,
                    $mode,
                    $t0,
                    $style,
                    $ann
                )
            }
            OutputTimeFmt::EpochRfc3339 | OutputTimeFmt::AbsoluteRfc3339 => {
                resolve_source!(
                    $it,
                    EpochRfc3339,
                    $ir,
                    $spec,
                    $stats,
                    $verb,
                    $dest,
                    $mode,
                    $t0,
                    $style,
                    $ann
                )
            }
        }
    };
}

/// Layer 3 — construct the concrete event source from an `InputSpec`.
macro_rules! resolve_source {
    ($it:expr, $ot:ty, $ir:expr, $spec:expr,
     $stats:expr, $verb:expr, $dest:expr, $mode:ty, $t0:expr, $style:expr, $ann:expr) => {
        match $spec {
            InputSpec::Csv { time_col, origin } => {
                let feed: CsvFeedReader<_> = CsvFeedReader::create(time_col, origin, &$ir)?;
                resolve_writer!($it, $ot, $ir, feed, $stats, $verb, $dest, $mode, $t0, $style, $ann)
            }
        }
    };
}

/// Layer 4 — open the output writer for the selected `WriteDest`.
macro_rules! resolve_writer {
    ($it:expr, $ot:ty, $ir:expr, $feed:expr, $stats:expr,
     $verb:expr, $dest:expr, $mode:ty, $t0:expr, $style:expr, $ann:expr) => {
        match $dest {
            WriteDest::StdOut => {
                resolve_color!(
                    $it,
                    $ot,
                    $ir,
                    $feed,
                    $stats,
                    $verb,
                    stdout(),
                    $mode,
                    $t0,
                    $style,
                    atty::is(atty::Stream::Stdout),
                    $ann
                )
            }
            WriteDest::StdErr => {
                resolve_color!(
                    $it,
                    $ot,
                    $ir,
                    $feed,
                    $stats,
                    $verb,
                    stderr(),
                    $mode,
                    $t0,
                    $style,
                    atty::is(atty::Stream::Stderr),
                    $ann
                )
            }
            WriteDest::File(path) => {
                let file = File::create(path.as_path()).expect("unable to create output file");
                let writer = BufWriter::new(file);
                resolve_color!(
                    $it, $ot, $ir, $feed, $stats, $verb, writer, $mode, $t0, $style, false, $ann
                )
            }
        }
    };
}

/// Layer 5 — wrap the writer in an ANSI colour adapter or a plain no-colour wrapper.
macro_rules! resolve_color {
    ($it:expr, $ot:ty, $ir:expr, $feed:expr, $stats:expr, $verb:expr, $writer:expr,
     $mode:ty, $t0:expr, $style:expr, $colored:expr, $ann:expr) => {{
        match $style {
            OutputStyle::Logger if $colored => {
                let sink = EventPrinter::<_, Ansi<_>>::new_with_annotations(
                    $verb.try_into()?,
                    &$ir,
                    $ann,
                )?
                .into_sink($writer);
                assemble_session!($it, $ot, $ir, $feed, $stats, $mode, $t0, sink)
            }
            OutputStyle::Logger => {
                let sink = EventPrinter::<_, NoColor<_>>::new_with_annotations(
                    $verb.try_into()?,
                    &$ir,
                    $ann,
                )?
                .into_sink($writer);
                assemble_session!($it, $ot, $ir, $feed, $stats, $mode, $t0, sink)
            }
        }
    }};
}

/// Layer 6 — assemble the fully-typed `MonitorSession< >` and start the run.
macro_rules! assemble_session {
    ($it:expr, $ot:ty, $ir:expr, $feed:expr, $stats:expr, $mode:ty, $t0:expr, $sink:expr) => {{
        let stats_sink = match $stats {
            MetricsLevel::All => Some(StatsSink::new($ir.alarms.len(), stderr())),
            MetricsLevel::None => None,
        };
        MonitorSession {
            ir: $ir,
            source: $feed,
            mode: <$mode as ExecRunMode>::from_clock($it),
            output_time_repr: PhantomData::<$ot>::default(),
            start_time: $t0,
            verdict_sink: $sink,
            stats_sink,
        }
        .run()
        .map(|_| ())
    }};
}

// ---------------------------------------------------------------------------
// Run dispatch
// ---------------------------------------------------------------------------

/// Resolve the correct fully-typed runtime path and run the monitor.
fn launch_monitor(
    ir: OORVIR,
    annotations: StreamAnnotations,
    input_spec: InputSpec,
    args: &AppArgs,
) -> Result<(), Box<dyn Error>> {
    let dest = WriteDest::from(args.output_target.clone());
    let verbosity = args.verbosity;
    let out_fmt = args.output_time_format;
    let style = args.output_format;
    let stats = args.statistics;
    let t0: Option<SystemTime> = args.time_origin.clone().into();

    match args.run_mode {
        // Real-time mode: use the wall-clock as the timestamp source.
        RunMode { online: true, .. } => {
            resolve_output_time!(
                WallClock::default(),
                out_fmt,
                ir,
                input_spec,
                stats,
                verbosity,
                dest,
                LiveMode,
                t0,
                style,
                annotations
            )?;
        }

        // Offline with artificial inter-event delay.
        RunMode {
            offline: Some(_), ..
        } if args.data_source.input_delay.is_some() => {
            let delay_ms = args.data_source.input_delay.unwrap();
            resolve_output_time!(
                FixedStep::with_step(Duration::from_millis(delay_ms)),
                out_fmt,
                ir,
                input_spec,
                stats,
                verbosity,
                dest,
                ReplayMode<_>,
                t0,
                style,
                annotations
            )?;
        }

        // Offline replay: timestamps are provided by the input source.
        RunMode {
            offline: Some(time_fmt),
            ..
        } => {
            resolve_input_time!(
                time_fmt,
                out_fmt,
                ir,
                input_spec,
                stats,
                verbosity,
                dest,
                ReplayMode<_>,
                t0,
                style,
                annotations
            )?;
        }

        _ => unreachable!("clap guarantees exactly one of --online / --offline is present"),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Spec compilation pipeline
// ---------------------------------------------------------------------------

/// Load and compile the specification referenced by `args`.
fn build_pipeline(
    args: &AppArgs,
) -> Result<(OORVIR, StreamAnnotations, InputSpec), Box<dyn Error>> {
    let path = &args.spec_path;

    // Stage 1: source file -> shared core refined IR.
    let refined_ir = compile_refined_file(path).map_err(|e| e.boxed())?;

    // Stage 2: derive per-stream verbosity annotations.
    let annotations = StreamAnnotations::new(&refined_ir).map_err(|e| e.boxed())?;

    // Stage 3: resolve the event source from CLI arguments.
    let input_spec = InputSpec::from(args.data_source.clone());

    Ok((refined_ir, annotations, input_spec))
}
