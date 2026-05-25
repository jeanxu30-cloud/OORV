//! Output formatting traits and helpers.
//!
//! [`OutputFormatter`] is the core trait converting monitor output into
//! user-visible records.  Use [`StructOutputFormatter`] for types that
//! implement [`FromStreamValues`].

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::marker::PhantomData;

use crate::oorvir::refined::OORVIR;
use crate::oorvir::refined::{Stream, StreamIdx};

use crate::runtime::iface::watcher::{Change, FullDeltaOutput, OutputFormat, SnapshotOutput};
use crate::runtime::settings::{OutputTimestamp, TimestampCast};
use crate::runtime::{Value, ValueConvertError};

// ─── Error types ─────────────────────────────────────────────────────────────

/// Error that can occur when constructing a type from stream values.
#[derive(Debug)]
pub enum ConversionError {
    ValueConversion(ValueConvertError),
    ExpectedValue {
        stream_name: String,
    },
    InvalidHashMap {
        stream_name: String,
        expected_num_params: usize,
        got_number_params: usize,
    },
    StreamKindMismatch,
}

impl Display for ConversionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversionError::ValueConversion(v) => write!(f, "{}", v),
            ConversionError::ExpectedValue { stream_name } => {
                write!(
                    f,
                    "The value for stream {} was expected to exist but was not present in the monitor verdict.",
                    stream_name
                )
            }
            ConversionError::InvalidHashMap {
                stream_name,
                expected_num_params,
                got_number_params,
            } => {
                write!(
                    f,
                    "Mismatch in the number of parameters of stream {}\nExpected {} parameters, but got {}",
                    stream_name, expected_num_params, got_number_params
                )
            }
            ConversionError::StreamKindMismatch => {
                write!(
                    f,
                    "Expected a parameterized stream but got a non-parameterized stream or vice-versa."
                )
            }
        }
    }
}

impl Error for ConversionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ConversionError::ValueConversion(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ValueConvertError> for ConversionError {
    fn from(value: ValueConvertError) -> Self {
        Self::ValueConversion(value)
    }
}

/// Error returned when constructing a [`StructOutputFormatter`] fails.
#[derive(Debug)]
pub enum StructFormatterError {
    UnknownStream(String),
    ValueError(ConversionError),
}

impl Display for StructFormatterError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            StructFormatterError::UnknownStream(field) => {
                write!(f, "No stream found for struct field: {}", field)
            }
            StructFormatterError::ValueError(ve) => write!(f, "{}", ve),
        }
    }
}

impl Error for StructFormatterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            StructFormatterError::UnknownStream(_) => None,
            StructFormatterError::ValueError(ve) => Some(ve),
        }
    }
}

impl From<ConversionError> for StructFormatterError {
    fn from(value: ConversionError) -> Self {
        Self::ValueError(value)
    }
}

// ─── StreamState ─────────────────────────────────────────────────────────────

/// State of one stream in an output cycle.
#[derive(Debug, Clone)]
pub enum StreamState {
    Stream(Option<Value>),
    Instances(HashMap<Vec<Value>, Value>),
}

// ─── Core traits ─────────────────────────────────────────────────────────────

/// Converts raw monitor output into a user-visible record.
pub trait OutputFormatter<MonitorOutput: OutputFormat, OutputTime: OutputTimestamp> {
    type Record;
    type Error: Error + 'static;

    fn format_output(
        &mut self,
        rec: MonitorOutput,
        ts: OutputTime::InnerTime,
    ) -> Result<Self::Record, Self::Error>;
}

/// Extends [`OutputFormatter`] with a construction method.
pub trait BuildableFormatter<MonitorOutput: OutputFormat, OutputTime: OutputTimestamp>:
    OutputFormatter<MonitorOutput, OutputTime> + Sized
{
    type CreationData;
    type CreationError;

    fn new(ir: &OORVIR, data: Self::CreationData) -> Result<Self, Self::CreationError>;
}

/// Annotates `Self` with a [`BuildableFormatter`] associated type.
pub trait HasFormatter<MonitorOutput: OutputFormat, OutputTime: OutputTimestamp> {
    type Formatter: BuildableFormatter<MonitorOutput, OutputTime>;
}

/// Trait for types that can be constructed from a snapshot of stream values.
pub trait FromStreamValues: Sized {
    type OutputTime;

    fn streams() -> Vec<String>;

    fn construct(ts: Self::OutputTime, data: Vec<StreamState>) -> Result<Self, ConversionError>;
}

impl<V, ExpectedTime, MonitorTime> HasFormatter<SnapshotOutput, MonitorTime> for V
where
    V: FromStreamValues<OutputTime = ExpectedTime>,
    MonitorTime: TimestampCast<ExpectedTime>,
{
    type Formatter = StructOutputFormatter<V>;
}

impl<V, ExpectedTime, MonitorTime> HasFormatter<FullDeltaOutput, MonitorTime> for V
where
    V: FromStreamValues<OutputTime = ExpectedTime>,
    MonitorTime: TimestampCast<ExpectedTime>,
{
    type Formatter = StructOutputFormatter<V>;
}

// ─── StructOutputFormatter ───────────────────────────────────────────────────

/// An [`OutputFormatter`] for types implementing [`FromStreamValues`].
#[derive(Debug, Clone)]
pub struct StructOutputFormatter<V: FromStreamValues> {
    map: Vec<StreamIdx>,
    map_inv: HashMap<StreamIdx, usize>,
    parameterized_streams: HashSet<usize>,
    inner: PhantomData<V>,
}

impl<V: FromStreamValues> StructOutputFormatter<V> {
    pub fn new(ir: &OORVIR) -> Result<Self, StructFormatterError> {
        let map: Vec<StreamIdx> = V::streams()
            .iter()
            .map(|name| {
                ir.stream_by_name(name)
                    .map(|s| s.stream_idx())
                    .or_else(|| {
                        name.starts_with("constrain_")
                            .then(|| name.split_once('_'))
                            .flatten()
                            .and_then(|(_, trg_idx)| trg_idx.parse::<usize>().ok())
                            .and_then(|trg_idx| ir.alarms.get(trg_idx).map(|trg| trg.constrain_idx))
                    })
                    .ok_or_else(|| StructFormatterError::UnknownStream(name.to_string()))
            })
            .collect::<Result<_, _>>()?;
        let map_inv = map.iter().enumerate().map(|(idx, sr)| (*sr, idx)).collect();
        let parameterized_streams = ir
            .constraints
            .iter()
            .filter(|os| os.is_parameter())
            .map(|o| o.stream_idx.out_ix())
            .collect();
        Ok(Self {
            map,
            map_inv,
            parameterized_streams,
            inner: Default::default(),
        })
    }
}

impl<O, I, V> OutputFormatter<SnapshotOutput, O> for StructOutputFormatter<V>
where
    V: FromStreamValues<OutputTime = I>,
    O: OutputTimestamp + TimestampCast<I>,
{
    type Error = StructFormatterError;
    type Record = V;

    fn format_output(
        &mut self,
        rec: SnapshotOutput,
        ts: O::InnerTime,
    ) -> Result<Self::Record, Self::Error> {
        let values: Vec<StreamState> = self
            .map
            .iter()
            .map(|sr| match sr {
                StreamIdx::Signal(i) => StreamState::Stream(rec.signals[*i].clone()),
                StreamIdx::Constraint(o) if !self.parameterized_streams.contains(o) => {
                    StreamState::Stream(rec.output[*o][0].1.clone())
                }
                StreamIdx::Constraint(o) => StreamState::Instances(
                    rec.output[*o]
                        .iter()
                        .filter(|(_, value)| value.is_some())
                        .map(|(inst, val)| (inst.clone().unwrap(), val.clone().unwrap()))
                        .collect(),
                ),
            })
            .collect();
        let time = O::cast(ts);
        Ok(V::construct(time, values)?)
    }
}

impl<O, I, V> BuildableFormatter<SnapshotOutput, O> for StructOutputFormatter<V>
where
    V: FromStreamValues<OutputTime = I>,
    O: OutputTimestamp + TimestampCast<I>,
{
    type CreationData = ();
    type CreationError = StructFormatterError;

    fn new(ir: &OORVIR, _data: Self::CreationData) -> Result<Self, Self::CreationError> {
        Self::new(ir)
    }
}

impl<O, I, V> OutputFormatter<FullDeltaOutput, O> for StructOutputFormatter<V>
where
    V: FromStreamValues<OutputTime = I>,
    O: OutputTimestamp + TimestampCast<I>,
{
    type Error = StructFormatterError;
    type Record = V;

    fn format_output(
        &mut self,
        rec: FullDeltaOutput,
        ts: O::InnerTime,
    ) -> Result<Self::Record, Self::Error> {
        let mut values: Vec<StreamState> = self
            .map
            .iter()
            .map(|sr| {
                if sr.is_output() && self.parameterized_streams.contains(&sr.out_ix()) {
                    StreamState::Instances(HashMap::new())
                } else {
                    StreamState::Stream(None)
                }
            })
            .collect();

        for (ir, v) in rec.signals {
            if let Some(idx) = self.map_inv.get(&StreamIdx::Signal(ir)) {
                values[*idx] = StreamState::Stream(Some(v));
            }
        }
        for (or, changes) in rec.outputs {
            if let Some(idx) = self.map_inv.get(&StreamIdx::Constraint(or)) {
                if self.parameterized_streams.contains(&or) {
                    let StreamState::Instances(res) = &mut values[*idx] else {
                        unreachable!("Mapping did not work!");
                    };
                    for change in changes {
                        if let Change::Update(p, v) = change {
                            res.insert(p.unwrap(), v);
                        }
                    }
                } else {
                    let value = changes.into_iter().find_map(|change| {
                        if let Change::Update(_, v) = change {
                            Some(v)
                        } else {
                            None
                        }
                    });
                    values[*idx] = StreamState::Stream(value);
                }
            }
        }
        let time = O::cast(ts);
        Ok(V::construct(time, values)?)
    }
}

impl<O, I, V> BuildableFormatter<FullDeltaOutput, O> for StructOutputFormatter<V>
where
    V: FromStreamValues<OutputTime = I>,
    O: OutputTimestamp + TimestampCast<I>,
{
    type CreationData = ();
    type CreationError = StructFormatterError;

    fn new(ir: &OORVIR, _data: Self::CreationData) -> Result<Self, Self::CreationError> {
        Self::new(ir)
    }
}
