//! Command-line interface parameter definitions.
//!
//! All CLI arguments are defined using `clap`'s derive macros.  This module
//! also contains the [`From`] conversions that translate parsed CLI values
//! into the core types used by the monitoring pipeline.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{ArgGroup, Args, Parser, ValueEnum};
use oorv_core::runtime::settings::parse_duration_str;

use crate::session::{InputSpec, MetricsLevel, Verbosity, WriteDest};
use crate::stream::feed::CsvOrigin;

// ---------------------------------------------------------------------------
// From conversions — translate CLI values to pipeline types
// ---------------------------------------------------------------------------

impl From<DataSource> for InputSpec {
    fn from(ds: DataSource) -> Self {
        // Route to stdin or file origin depending on which flag was set.
        let origin = if ds.stdin {
            CsvOrigin::Stdin
        } else {
            CsvOrigin::FilePath(
                ds.csv_in
                    .expect("--csv-in is required when --stdin is absent"),
            )
        };
        InputSpec::Csv {
            time_col: ds.csv_time_column,
            origin,
        }
    }
}

impl From<OutputTarget> for WriteDest {
    fn from(tgt: OutputTarget) -> Self {
        // Priority: file > stderr > stdout (stdout is the implicit default).
        if let Some(path) = tgt.output_file {
            WriteDest::File(path)
        } else if tgt.stderr {
            WriteDest::StdErr
        } else {
            WriteDest::StdOut
        }
    }
}

impl From<TimeOrigin> for Option<SystemTime> {
    fn from(origin: TimeOrigin) -> Self {
        // An explicit RFC timestamp takes precedence over a Unix duration.
        origin
            .rfc_ts
            .or_else(|| origin.unix_ts.map(|dur| UNIX_EPOCH + dur))
    }
}

// ---------------------------------------------------------------------------
// InputTimeFmt — recognised time representations for input timestamps
// ---------------------------------------------------------------------------

/// The time representation format used in the input CSV data.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum InputTimeFmt {
    /// Alias: relative-float-secs
    Relative,
    /// Alias: relative-uint-nanos
    RelNanos,
    /// Unsigned nanoseconds elapsed since the monitor start.
    RelativeUintNanos,
    /// Alias: relative-float-secs
    RelativeSecs,
    /// Fractional seconds elapsed since the monitor start (e.g. `5.2`).
    RelativeFloatSecs,
    /// Alias: offset-float-secs
    Offset,
    /// Alias: offset-uint-nanos
    DeltaNanos,
    /// Unsigned nanoseconds since the immediately preceding event.
    OffsetUintNanos,
    /// Alias: offset-float-secs
    OffsetSecs,
    /// Fractional seconds since the immediately preceding event.
    OffsetFloatSecs,
    /// Alias: absolute-unix
    Absolute,
    /// Fractional seconds since the Unix epoch (wall-clock time).
    AbsoluteUnix,
    /// Alias: absolute-rfc3339
    EpochRfc3339,
    /// Wall-clock time in RFC 3339 format.
    AbsoluteRfc3339,
}

// ---------------------------------------------------------------------------
// OutputTimeFmt — recognised time representations for output timestamps
// ---------------------------------------------------------------------------

/// The time representation format used when printing timestamps in the output.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum OutputTimeFmt {
    /// Alias: relative-float-secs
    Relative,
    /// Alias: relative-uint-nanos
    RelNanos,
    /// Unsigned nanoseconds elapsed since the monitor start.
    RelativeUintNanos,
    /// Alias: relative-float-secs
    RelativeSecs,
    /// Fractional seconds elapsed since the monitor start.
    RelativeFloatSecs,
    /// Alias: absolute-unix
    Absolute,
    /// Fractional seconds since the Unix epoch (wall-clock time).
    AbsoluteUnix,
    /// Alias: absolute-rfc3339
    EpochRfc3339,
    /// Wall-clock time in RFC 3339 format.
    AbsoluteRfc3339,
}

// ---------------------------------------------------------------------------
// OutputStyle — output formatting style selector
// ---------------------------------------------------------------------------

/// The formatting style applied to every output line.
#[derive(Clone, Copy, Debug, ValueEnum, Default)]
pub enum OutputStyle {
    /// Structured, line-based log format (the default).
    #[default]
    Logger,
}

// ---------------------------------------------------------------------------
// AppArgs — top-level CLI argument container
// ---------------------------------------------------------------------------

/// Top-level argument structure for the OORV monitor CLI.
///
/// Clap derives the full argument parser from this struct and its
/// flattened member structs.
#[derive(Parser, Debug, Clone)]
pub struct AppArgs {
    /// Path to the OORV specification file to be monitored.
    pub spec_path: PathBuf,

    /// Input source settings (CSV file or standard input).
    #[command(flatten)]
    pub data_source: DataSource,

    /// Destination for monitor output (stdout / stderr / file).
    #[command(flatten)]
    pub output_target: OutputTarget,

    /// Execution mode selection (online real-time or offline replay).
    #[command(flatten)]
    pub run_mode: RunMode,

    /// Optional override of the monitor's start timestamp.
    #[command(flatten)]
    pub time_origin: TimeOrigin,

    /// Level of statistics to compute and display.
    #[arg(short, long, value_enum, default_value_t)]
    pub statistics: MetricsLevel,

    /// Verbosity level for monitor output lines.
    #[arg(short, long, value_enum, default_value_t)]
    pub verbosity: Verbosity,

    /// Time representation format used when printing timestamps in output.
    #[arg(
        short = 'f',
        long,
        value_enum,
        default_value_t = OutputTimeFmt::RelativeFloatSecs
    )]
    pub output_time_format: OutputTimeFmt,

    /// Formatting style applied to every output line.
    #[arg(long, value_enum, default_value_t)]
    pub output_format: OutputStyle,
}

// ---------------------------------------------------------------------------
// TimeOrigin — start-time override arguments
// ---------------------------------------------------------------------------

/// Arguments for overriding the monitor's notion of time zero.
///
/// At most one of the two flags may be supplied at once.
#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Start Time")]
pub struct TimeOrigin {
    /// Start time as a Unix timestamp in `seconds.subseconds` format.
    #[arg(long = "start-time-unix", value_parser = parse_duration_str, group = "start-time")]
    unix_ts: Option<Duration>,

    /// Start time as an RFC 3339 formatted timestamp string.
    #[arg(long = "start-time-rfc3339", value_parser = humantime::parse_rfc3339, group = "start-time")]
    rfc_ts: Option<SystemTime>,
}

// ---------------------------------------------------------------------------
// DataSource — input source arguments
// ---------------------------------------------------------------------------

/// Arguments that select and configure the event data source.
///
/// Exactly one of `--csv-in` or `--stdin` must be provided.
#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Input Source")]
#[command(group(
    ArgGroup::new("monitor_input")
        .required(true)
        .args(&["csv_in", "stdin"])
))]
pub struct DataSource {
    /// Path to a CSV file that provides the stream of input events.
    #[arg(long)]
    pub(crate) csv_in: Option<PathBuf>,

    /// Read input events from standard input.
    #[arg(long)]
    pub(crate) stdin: bool,

    /// Artificial inter-event delay in milliseconds; only valid with --csv-in.
    #[arg(long, requires = "csv_in")]
    pub input_delay: Option<u64>,

    /// CSV column index (1-based) that holds per-event timestamps; only valid with --csv-in.
    #[arg(long, requires = "csv_in")]
    pub(crate) csv_time_column: Option<usize>,
}

// ---------------------------------------------------------------------------
// RunMode — execution mode arguments
// ---------------------------------------------------------------------------

/// Arguments that select between online and offline execution modes.
///
/// Exactly one of `--online` or `--offline <FORMAT>` must be provided.
#[derive(Clone, Copy, Debug, Args)]
#[command(next_help_heading = "Execution Mode")]
#[command(group(
    ArgGroup::new("mode")
        .required(true)
        .args(&["online", "offline"])
))]
pub struct RunMode {
    /// Use wall-clock time for events; requires stdin as the event source.
    #[arg(long, requires = "stdin")]
    pub online: bool,

    /// Replay events using timestamps embedded in the source, in the given format.
    #[arg(long, value_enum, value_name = "TIME FORMAT")]
    pub offline: Option<InputTimeFmt>,
}

// ---------------------------------------------------------------------------
// OutputTarget — output channel arguments
// ---------------------------------------------------------------------------

/// Arguments that select where the monitor writes its output.
///
/// If none are set, stdout is used by default.
#[derive(Clone, Debug, Args)]
#[command(next_help_heading = "Output Channel")]
pub struct OutputTarget {
    /// Send output to standard output (default).
    #[arg(long, group = "output")]
    stdout: bool,

    /// Send output to standard error.
    #[arg(long, group = "output")]
    stderr: bool,

    /// Send output to the given file path.
    #[arg(long, group = "output")]
    output_file: Option<PathBuf>,
}
