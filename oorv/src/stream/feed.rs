use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::stdin;
use std::marker::PhantomData;
use std::path::PathBuf;

use csv::{
    ByteRecord, Reader as CsvReader, ReaderBuilder, Result as CsvResult, StringRecord, Trim,
};
use oorv_core::oorvir::refined::{SignalStream, Type, OORVIR};
use oorv_core::runtime::ingest::{
    AssociatedEventFactory, EventFactory, FieldExtractor, IngestionError, InputMap,
};
use oorv_core::runtime::settings::TimestampCodec;
use oorv_core::runtime::Value;

// Default column names recognised as timestamp columns when no explicit index
// is given via `--csv-time-column`.
const DEFAULT_TIME_COLUMN_NAMES: [&str; 3] = ["time", "ts", "timestamp"];

// Type alias for the timestamp-extraction closure stored inside CsvFeedReader.
type TimestampExtractor<T, E> = Box<dyn Fn(&DataRow) -> Result<T, E>>;

#[allow(missing_debug_implementations)]
pub struct CsvFeedReader<InputTime: TimestampCodec> {
    /// The underlying CSV reader (stdin / file / in-memory).
    backend: CsvBackend,
    /// Column-to-stream mapping built once from the CSV header.
    binding: ColumnBinding,
    /// Closure that extracts a typed timestamp from a data row.
    extract_ts: TimestampExtractor<InputTime::InnerTime, CsvParseError>,
    _time: PhantomData<InputTime>,
}

impl<InputTime: TimestampCodec> CsvFeedReader<InputTime> {
    pub fn create(
        time_col: Option<usize>,
        origin: CsvOrigin,
        ir: &OORVIR,
    ) -> Result<CsvFeedReader<InputTime>, Box<dyn Error>> {
        let mut builder = ReaderBuilder::new();
        builder.trim(Trim::All);

        // Open the appropriate backend based on the requested origin.
        let mut backend = match origin {
            CsvOrigin::Stdin => CsvBackend::Stdin(builder.from_reader(stdin())),
            CsvOrigin::FilePath(path) => CsvBackend::FilePath(builder.from_path(path)?),
            CsvOrigin::InMemory(data) => {
                let bytes = VecDeque::from(data.into_bytes());
                CsvBackend::InMemory(builder.from_reader(bytes))
            }
        };

        // Validate the header against the spec's declared input streams.
        let binding =
            ColumnBinding::map_columns(ir.signals.as_slice(), backend.column_headers()?, time_col)?;

        // Timestamps are required by time-aware input representations.
        if InputTime::provided_by_input() && binding.ts_col.is_none() {
            return Err(Box::from(
                "the selected time format requires a 'time' column in the CSV input",
            ));
        }

        // Build the timestamp-extraction closure once, capturing the column index.
        let extract_ts: TimestampExtractor<InputTime::InnerTime, CsvParseError> =
            match binding.ts_col {
                Some(ts_idx) => Box::new(move |row: &DataRow| {
                    let raw = row
                        .0
                        .get(ts_idx)
                        .expect("ts column was verified during setup");
                    let s = std::str::from_utf8(raw).map_err(|e| {
                        CsvParseError::Value(format!(
                            "timestamp is not valid UTF-8 (column {ts_idx}): {e}"
                        ))
                    })?;
                    InputTime::parse(s).map_err(|e| {
                        CsvParseError::Value(format!(
                            "could not parse timestamp `{s}` in column {ts_idx}: {e}"
                        ))
                    })
                }),
                // No timestamp column — fall back to the zero/default timestamp.
                None => Box::new(|_: &DataRow| Ok(InputTime::parse("").unwrap())),
            };

        Ok(CsvFeedReader {
            backend,
            binding,
            extract_ts,
            _time: PhantomData,
        })
    }
}

impl<InputTime: TimestampCodec> InputFeed<InputTime> for CsvFeedReader<InputTime> {
    type Error = CsvParseError;
    type Record = DataRow;

    /// Return a clone of the column binding for the monitor's event factory.
    fn init_binding(&self) -> Result<ColumnBinding, CsvParseError> {
        Ok(self.binding.clone())
    }

    /// Fetch the next CSV row paired with its extracted timestamp.
    ///
    /// Returns `Ok(None)` when the source is exhausted.
    fn next_record(&mut self) -> Result<Option<(DataRow, InputTime::InnerTime)>, CsvParseError> {
        let mut buf = ByteRecord::new();
        let has_row = self
            .backend
            .advance(&mut buf)
            .map_err(|e| CsvParseError::Validation(format!("error reading CSV row: {e}")))?;

        if !has_row {
            return Ok(None);
        }

        let row = DataRow::from(buf);
        let ts = (self.extract_ts)(&row)?;
        Ok(Some((row, ts)))
    }
}

// ---------------------------------------------------------------------------
// CsvOrigin — selects the CSV data source
// ---------------------------------------------------------------------------

/// Identifies where the CSV byte stream originates.
#[derive(Debug, Clone)]
pub enum CsvOrigin {
    /// Read events from standard input.
    Stdin,
    /// Read events from the file at the given path.
    FilePath(PathBuf),
    /// Read events from an in-memory UTF-8 string (for testing).
    #[allow(dead_code)]
    InMemory(String),
}

#[derive(Debug, Clone)]
pub struct ColumnBinding {
    /// Stream name to zero-based column index.
    col_map: HashMap<String, usize>,
    /// Stream name to expected value type.
    type_map: HashMap<String, Type>,
    /// Zero-based index of the timestamp column, if one was found.
    ts_col: Option<usize>,
}

impl ColumnBinding {
    /// Build a `ColumnBinding` by matching `signals` against the CSV `header`.
    ///
    /// `explicit_time_col` is a 1-based column number from `--csv-time-column`;
    /// if absent, the header is scanned for a well-known timestamp column name.
    fn map_columns(
        signals: &[SignalStream],
        header: &StringRecord,
        explicit_time_col: Option<usize>,
    ) -> Result<ColumnBinding, Box<dyn Error>> {
        // Resolve every declared input stream to its CSV column position.
        let col_map = signals
            .iter()
            .map(|sig| {
                header
                    .iter()
                    .position(|col| col == sig.name)
                    .map(|pos| (sig.name.clone(), pos))
                    .ok_or_else(|| {
                        format!(
                            "error: CSV header has no column for input stream `{}`.",
                            &sig.name
                        )
                    })
            })
            .collect::<Result<HashMap<_, _>, String>>()?;

        let type_map: HashMap<String, Type> = signals
            .iter()
            .map(|sig| (sig.name.clone(), sig.annotation.clone()))
            .collect();

        // Convert 1-based explicit index to 0-based, or scan for a default name.
        let ts_col = explicit_time_col.map(|c| c - 1).or_else(|| {
            header
                .iter()
                .position(|name| DEFAULT_TIME_COLUMN_NAMES.contains(&name.to_lowercase().as_str()))
        });

        Ok(ColumnBinding {
            col_map,
            type_map,
            ts_col,
        })
    }
}

/// Wraps a raw `ByteRecord` and exposes field access via the [`InputMap`] trait.
#[derive(Clone, Debug)]
pub struct DataRow(ByteRecord);

impl From<ByteRecord> for DataRow {
    fn from(rec: ByteRecord) -> Self {
        DataRow(rec)
    }
}

impl InputMap for DataRow {
    type CreationData = ColumnBinding;
    type Error = CsvParseError;

    /// Build a value-getter closure for the named input stream.
    fn extractor_for(
        name: &str,
        binding: Self::CreationData,
    ) -> Result<FieldExtractor<Self, Self::Error>, Self::Error> {
        let col_idx = binding.col_map[name];
        let expected_ty = binding.type_map[name].clone();
        let stream_name = name.to_string();

        Ok(Box::new(move |row: &DataRow| {
            let raw = row
                .0
                .get(col_idx)
                .expect("column index verified during setup");
            Value::parse_bytes(raw, &expected_ty).map_err(|parse_err| {
                CsvParseError::Value(format!(
                    "failed to parse field for stream `{stream_name}`: {parse_err}"
                ))
            })
        }))
    }
}

/// Errors that may occur while reading or parsing a CSV event stream.
#[derive(Debug)]
pub enum CsvParseError {
    /// An underlying I/O error (file not found, permission denied, etc.).
    Io(std::io::Error),
    /// The CSV structure is invalid (missing header, malformed shape, etc.).
    Validation(String),
    /// A field value could not be converted to the expected type.
    Value(String),
}

impl fmt::Display for CsvParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CsvParseError::Io(e) => write!(f, "I/O error: {e}"),
            CsvParseError::Validation(msg) => write!(f, "CSV validation error: {msg}"),
            CsvParseError::Value(msg) => write!(f, "value parse error: {msg}"),
        }
    }
}

impl Error for CsvParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            CsvParseError::Io(e) => Some(e),
            CsvParseError::Validation(_) | CsvParseError::Value(_) => None,
        }
    }
}

impl From<CsvParseError> for IngestionError {
    fn from(err: CsvParseError) -> Self {
        IngestionError::SourceFailure(Box::new(err))
    }
}

/// Wraps the three possible underlying CSV reader types behind a uniform API.
enum CsvBackend {
    Stdin(CsvReader<std::io::Stdin>),
    FilePath(CsvReader<File>),
    InMemory(CsvReader<VecDeque<u8>>),
}

impl CsvBackend {
    /// Read the next byte record into `buf`.
    ///
    /// Returns `Ok(true)` when a row was written; `Ok(false)` at end-of-stream.
    fn advance(&mut self, buf: &mut ByteRecord) -> CsvResult<bool> {
        match self {
            CsvBackend::Stdin(r) => r.read_byte_record(buf),
            CsvBackend::FilePath(r) => r.read_byte_record(buf),
            CsvBackend::InMemory(r) => r.read_byte_record(buf),
        }
    }

    /// Return a reference to the parsed header row.
    fn column_headers(&mut self) -> CsvResult<&StringRecord> {
        match self {
            CsvBackend::Stdin(r) => r.headers(),
            CsvBackend::FilePath(r) => r.headers(),
            CsvBackend::InMemory(r) => r.headers(),
        }
    }
}

// Convenience alias for the poll result returned by `next_record`.
type PollResult<Record, Time, Err> = Result<Option<(Record, Time)>, Err>;

/// The contract that every event-source adapter must satisfy.
///
/// Type parameter `InputTime` determines the timestamp representation expected
/// by the monitor core.
pub trait InputFeed<InputTime: TimestampCodec> {
    /// The event record type produced by this source.
    type Record: AssociatedEventFactory;

    /// Error type returned on I/O or parse failures.
    type Error: Error;

    /// Return the creation data the monitor needs to initialise its event factory.
    ///
    /// Called once, before the first `next_record` invocation.
    fn init_binding(
        &self,
    ) -> Result<
        <<Self::Record as AssociatedEventFactory>::Ingester as EventFactory>::CreationData,
        Self::Error,
    >;

    /// Fetch the next event from the source, blocking until one is available.
    ///
    /// * Returns `Ok(Some((record, timestamp)))` while events remain.
    /// * Returns `Ok(None)` when the source is exhausted.
    fn next_record(&mut self) -> PollResult<Self::Record, InputTime::InnerTime, Self::Error>;
}
