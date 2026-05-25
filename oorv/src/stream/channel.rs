use std::collections::HashMap;
use std::error::Error;
use std::io::Write;
use std::marker::PhantomData;

use oorv_core::diagnostic::OORVError;
use oorv_core::oorvir::refined::tags::StreamVerbosity;
use oorv_core::oorvir::refined::OORVIR;
use oorv_core::oorvir::source::StreamIdx;
use oorv_core::runtime::emitter::OutputFormatter;
use oorv_core::runtime::settings::{OutputTimestamp, TimestampCodec};
use oorv_core::runtime::watcher::OutputFormat;

/// Captures the verbosity level for every stream in the specification,
/// derived from the spec's tag annotations.
#[derive(Debug, Clone)]
pub struct StreamAnnotations {
    stream_verbosity: HashMap<StreamIdx, StreamVerbosity>,
}

impl StreamAnnotations {
    /// Parse the specification's verbosity and debug tags and build the map.
    pub fn new(ir: &OORVIR) -> Result<StreamAnnotations, OORVError> {
        let stream_verbosity = ir
            .iter_streams()
            .map(|sr| {
                let level = match sr {
                    StreamIdx::Signal(_) => StreamVerbosity::Streams,
                    StreamIdx::Constraint(_) => match ir.constraint(sr).kind {
                        oorv_core::oorvir::refined::ConstraintKind::Output(_) => {
                            StreamVerbosity::Outputs
                        }
                        oorv_core::oorvir::refined::ConstraintKind::Alarm(_) => {
                            StreamVerbosity::Violations
                        }
                    },
                };
                (sr, level)
            })
            .collect();

        Ok(Self { stream_verbosity })
    }

    /// Look up the resolved verbosity level for the given stream index.
    pub fn verbosity(&self, sr: StreamIdx) -> StreamVerbosity {
        *self.stream_verbosity.get(&sr).unwrap()
    }
}

/// Covers both factory conversion failures and underlying write failures.
#[derive(Debug)]
pub enum SinkError<SE: Error + 'static, FE: Error + 'static> {
    /// The underlying writer or sink returned an error.
    Sink(SE),
    /// The factory failed to convert the monitor output.
    Factory(FE),
}

impl<SE: Error, FE: Error> std::fmt::Display for SinkError<SE, FE> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinkError::Sink(e) => write!(f, "sink error: {}", e),
            SinkError::Factory(e) => write!(f, "factory error: {}", e),
        }
    }
}

impl<SE: Error, FE: Error> Error for SinkError<SE, FE> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            SinkError::Sink(e) => Some(e),
            SinkError::Factory(e) => Some(e),
        }
    }
}

/// Core output trait.  Every output plugin implements `DataSink` to receive
/// evaluated monitor records and deliver them to a destination.
pub trait DataSink<V: OutputFormat, T: OutputTimestamp> {
    /// Error that may arise while delivering a record.
    type Error: Error + 'static;
    /// Value returned on successful delivery.
    type Return;
    /// Factory that converts raw monitor output into `Self::Factory::Record`.
    type Factory: OutputFormatter<V, T>;

    /// Deliver a single timed or event-triggered verdict.
    fn accept(
        &mut self,
        ts: <T as TimestampCodec>::InnerTime,
        verdict: V,
    ) -> Result<Self::Return, SinkError<Self::Error, <Self::Factory as OutputFormatter<V, T>>::Error>>
    {
        let record = self
            .factory()
            .format_output(verdict, ts)
            .map_err(SinkError::Factory)?;
        self.flush_record(record).map_err(SinkError::Sink)
    }

    /// Write one already-converted record to the underlying destination.
    fn flush_record(
        &mut self,
        record: <Self::Factory as OutputFormatter<V, T>>::Record,
    ) -> Result<Self::Return, Self::Error>;

    /// Return a mutable reference to the inner factory.
    fn factory(&mut self) -> &mut Self::Factory;
}

/// A [`DataSink`] that serialises records to bytes and writes them to any
/// `std::io::Write` implementor (stdout, a file, an in-memory buffer, etc.).
#[derive(Debug)]
pub struct WriteChannel<
    W: Write,
    Factory: OutputFormatter<Output, Time, Record = Record>,
    Output: OutputFormat,
    Time: OutputTimestamp,
    Record: Into<Vec<u8>>,
> {
    factory: Factory,
    writer: W,
    _output: PhantomData<Output>,
    _time: PhantomData<Time>,
}

impl<
        W: Write,
        Factory: OutputFormatter<Output, Time, Record = Record>,
        Output: OutputFormat,
        Time: OutputTimestamp,
        Record: Into<Vec<u8>>,
    > WriteChannel<W, Factory, Output, Time, Record>
{
    /// Construct a `WriteChannel` that uses `factory` to convert records and
    /// `writer` to receive the serialised bytes.
    pub fn new(writer: W, factory: Factory) -> Self {
        Self {
            factory,
            writer,
            _output: PhantomData,
            _time: PhantomData,
        }
    }
}

impl<
        W: Write,
        Factory: OutputFormatter<Output, Time, Record = Record>,
        Output: OutputFormat,
        Time: OutputTimestamp,
        Record: Into<Vec<u8>>,
    > DataSink<Output, Time> for WriteChannel<W, Factory, Output, Time, Record>
{
    type Error = std::io::Error;
    type Factory = Factory;
    type Return = ();

    fn flush_record(&mut self, record: Record) -> Result<Self::Return, Self::Error> {
        let bytes: Vec<u8> = record.into();
        self.writer.write_all(&bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    fn factory(&mut self) -> &mut Self::Factory {
        &mut self.factory
    }
}
