use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::marker::PhantomData;

use itertools::Itertools;

use crate::runtime::iface::watcher::Event;
use crate::runtime::{CondDeserialize, CondSerialize, Value, ValueConvertError};

// ─── FieldMappedIngester ─────────────────────────────────────────────────────

/// An ingester backed by a [`FieldMapper`] implementation.
#[allow(missing_debug_implementations)]
pub struct FieldMappedIngester<Inner: FieldMapper> {
    extractors: Vec<FieldExtractor<Inner, Inner::Error>>,
}

/// Auto-impl: every type that implements [`FieldMapper`] gets [`HasIngester`]
/// pointing to [`FieldMappedIngester`].
impl<M: FieldMapper> HasIngester for M {
    type Ingester = FieldMappedIngester<M>;
}

impl<Inner: FieldMapper> StreamIngester for FieldMappedIngester<Inner> {
    type CreationData = Inner::CreationData;
    type Error = Inner::Error;
    type Record = Inner;

    fn try_build(
        map: HashMap<String, usize>,
        data: Self::CreationData,
    ) -> Result<(Self, Vec<String>), IngestionError> {
        let mut extractors: Vec<Option<FieldExtractor<Inner, Inner::Error>>> =
            (0..map.len()).map(|_| None).collect();
        let mut claimed = Vec::with_capacity(map.len());
        for (name, pos) in map {
            match Inner::extractor_for(name.as_str(), data.clone()) {
                Ok(extractor) => {
                    extractors[pos] = Some(extractor);
                    claimed.push(name);
                }
                Err(e) => {
                    let ie: IngestionError = e.into();
                    if matches!(ie, IngestionError::UnknownStream(_)) {
                        extractors[pos] = Some(Box::new(|_| Ok(Value::None)));
                    } else {
                        return Err(ie);
                    }
                }
            }
        }
        let extractors = extractors.into_iter().map(Option::unwrap).collect();
        Ok((Self { extractors }, claimed))
    }

    fn ingest(&self, record: Inner) -> Result<Event, IngestionError> {
        self.extractors
            .iter()
            .map(|f| f(&record).map_err(Into::into))
            .collect()
    }
}

// ─── FixedArrayIngester ──────────────────────────────────────────────────────

/// Wraps a conversion error produced by [`FixedArrayIngester`].
#[derive(Debug, Clone, Copy)]
pub struct FixedArrayError<I: Error + Send + 'static>(I);

impl<I: Error + Send + 'static> Display for FixedArrayError<I> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl<I: Error + Send + 'static> Error for FixedArrayError<I> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

impl<I: Error + Send + 'static> From<FixedArrayError<I>> for IngestionError {
    fn from(e: FixedArrayError<I>) -> Self {
        Self::SourceFailure(Box::new(e.0))
    }
}

/// An ingester for types convertible to a fixed-size value array `[Value; N]`.
#[derive(Debug, Clone)]
pub struct FixedArrayIngester<
    const N: usize,
    I: Error + Send + 'static,
    E: TryInto<[Value; N], Error = I> + CondSerialize + CondDeserialize,
> {
    total_inputs: usize,
    _phantom: PhantomData<E>,
}

impl<
        const N: usize,
        I: Error + Send + 'static,
        E: TryInto<[Value; N], Error = I> + Send + CondSerialize + CondDeserialize,
    > StreamIngester for FixedArrayIngester<N, I, E>
{
    type CreationData = ();
    type Error = FixedArrayError<I>;
    type Record = E;

    fn try_build(
        map: HashMap<String, usize>,
        _data: Self::CreationData,
    ) -> Result<(Self, Vec<String>), IngestionError> {
        let total_inputs = map.len();
        let claimed: Vec<_> = map
            .into_iter()
            .sorted_by_key(|(_, idx)| *idx)
            .map(|(name, _)| name)
            .take(N)
            .collect();
        Ok((
            FixedArrayIngester {
                total_inputs,
                _phantom: PhantomData,
            },
            claimed,
        ))
    }

    fn ingest(&self, record: Self::Record) -> Result<Event, IngestionError> {
        let arr = record.try_into().map_err(FixedArrayError)?;
        let mut values = Vec::from(arr);
        values.resize(self.total_inputs, Value::None);
        Ok(values)
    }
}

// ─── VectorIngester ──────────────────────────────────────────────────────────

/// Error variants for [`VectorIngester`].
#[derive(Debug, Clone, Copy)]
pub enum VectorIngesterError<I: Error + Send + 'static> {
    /// The underlying conversion to `Vec<Value>` failed.
    ConversionFailure(I),
    /// The produced vector length does not match the declared size.
    LengthMismatch {
        #[allow(missing_docs)]
        expected: usize,
        #[allow(missing_docs)]
        got: usize,
    },
}

impl<I: Error + Send + 'static> Display for VectorIngesterError<I> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            VectorIngesterError::ConversionFailure(e) => Display::fmt(e, f),
            VectorIngesterError::LengthMismatch { expected, got } => {
                write!(
                    f,
                    "Event vector length mismatch (expected {expected}, got {got})"
                )
            }
        }
    }
}

impl<I: Error + Send + 'static> Error for VectorIngesterError<I> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            VectorIngesterError::ConversionFailure(e) => Some(e),
            VectorIngesterError::LengthMismatch { .. } => None,
        }
    }
}

impl<I: Error + Send + 'static> From<VectorIngesterError<I>> for IngestionError {
    fn from(e: VectorIngesterError<I>) -> Self {
        Self::SourceFailure(Box::new(e))
    }
}

/// An ingester for types convertible to `Vec<Value>`.
#[derive(Debug, Clone)]
pub struct VectorIngester<
    I: Error + Send + 'static,
    E: TryInto<Vec<Value>, Error = I> + CondSerialize + CondDeserialize,
> {
    total_inputs: usize,
    declared_length: usize,
    _phantom: PhantomData<E>,
}

impl<
        I: Error + Send + 'static,
        E: TryInto<Vec<Value>, Error = I> + Send + CondSerialize + CondDeserialize,
    > StreamIngester for VectorIngester<I, E>
{
    type CreationData = usize;
    type Error = VectorIngesterError<I>;
    type Record = E;

    fn try_build(
        map: HashMap<String, usize>,
        declared_length: Self::CreationData,
    ) -> Result<(Self, Vec<String>), IngestionError> {
        let total_inputs = map.len();
        let claimed: Vec<_> = map
            .into_iter()
            .sorted_by_key(|(_, idx)| *idx)
            .map(|(name, _)| name)
            .take(declared_length)
            .collect();
        Ok((
            Self {
                total_inputs,
                declared_length,
                _phantom: PhantomData,
            },
            claimed,
        ))
    }

    fn ingest(&self, record: Self::Record) -> Result<Event, IngestionError> {
        let mut values: Vec<_> = record
            .try_into()
            .map_err(VectorIngesterError::ConversionFailure)?;
        if values.len() != self.declared_length {
            return Err(VectorIngesterError::<I>::LengthMismatch {
                expected: self.declared_length,
                got: values.len(),
            }
            .into());
        }
        values.resize(self.total_inputs, Value::None);
        Ok(values)
    }
}

impl HasIngester for Vec<Value> {
    type Ingester = VectorIngester<Infallible, Vec<Value>>;
}

// ─── NullIngester ────────────────────────────────────────────────────────────

/// A no-op ingester that always produces an all-`None` event.
#[derive(Debug, Copy, Clone)]
pub struct NullIngester<T: Send>(usize, PhantomData<T>);

/// A marker type representing an event in which no input stream receives a value.
#[derive(Debug, Copy, Clone, Default)]
pub struct EmptyRecord;

impl<T: Send> StreamIngester for NullIngester<T> {
    type CreationData = ();
    type Error = Infallible;
    type Record = T;

    fn try_build(
        map: HashMap<String, usize>,
        _data: Self::CreationData,
    ) -> Result<(Self, Vec<String>), IngestionError> {
        Ok((Self(map.len(), PhantomData), vec![]))
    }

    fn ingest(&self, _record: Self::Record) -> Result<Event, IngestionError> {
        Ok(vec![Value::None; self.0])
    }
}

impl HasIngester for EmptyRecord {
    type Ingester = NullIngester<EmptyRecord>;
}

// ─── IngestionError ──────────────────────────────────────────────────────────

/// Errors produced by [`StreamIngester`] implementations.
#[derive(Debug)]
pub enum IngestionError {
    /// The provided stream name(s) are not recognised by this ingester.
    UnknownStream(Vec<String>),
    /// A field value could not be converted to the expected [`Value`] variant.
    UnsupportedValue(ValueConvertError),
    /// The input matched an enum variant that was explicitly excluded during
    /// ingester derivation.
    IgnoredVariant(String),
    /// An unclassified I/O or parsing error from the underlying source.
    SourceFailure(Box<dyn Error + Send + 'static>),
}

impl Display for IngestionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestionError::UnknownStream(names) => write!(
                f,
                "The following input stream(s) are not served by this ingester: {}",
                names.join(", ")
            ),
            IngestionError::UnsupportedValue(v) => {
                write!(f, "Value type {:?} is not supported by the interpreter.", v)
            }
            IngestionError::IgnoredVariant(v) => {
                write!(f, "Received an explicitly ignored input variant: {}.", v)
            }
            IngestionError::SourceFailure(e) => write!(f, "Input source error: {}.", e),
        }
    }
}

impl Error for IngestionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            IngestionError::SourceFailure(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl From<ValueConvertError> for IngestionError {
    fn from(v: ValueConvertError) -> Self {
        IngestionError::UnsupportedValue(v)
    }
}

impl From<Infallible> for IngestionError {
    fn from(_: Infallible) -> Self {
        unreachable!()
    }
}

// ─── Core Traits ─────────────────────────────────────────────────────────────

/// A heap-allocated closure that projects a `&From` reference to a [`Value`].
pub type FieldExtractor<From, E> = Box<dyn Fn(&From) -> Result<Value, E>>;

/// A factory that converts records of type [`Self::Record`] into internal
/// [`Event`] vectors consumed by the monitor evaluator.
pub trait StreamIngester: Sized {
    /// The external record type accepted by this ingester.
    type Record: Send;

    /// Error produced when ingestion fails (e.g. type mismatch, unknown stream).
    type Error: Into<IngestionError> + Send + 'static;

    /// Arbitrary data supplied at construction time (e.g. file paths, stream counts).
    type CreationData: Clone + Send;

    /// Build a complete ingester from a `name → position` mapping.
    fn build(
        map: HashMap<String, usize>,
        data: Self::CreationData,
    ) -> Result<Self, IngestionError> {
        let all: HashSet<_> = map.keys().cloned().collect();
        let (ingester, claimed) = Self::try_build(map, data)?;
        let claimed_set: HashSet<_> = claimed.into_iter().collect();
        let unclaimed: Vec<_> = all.difference(&claimed_set).cloned().collect();
        if !unclaimed.is_empty() {
            Err(IngestionError::UnknownStream(unclaimed))
        } else {
            Ok(ingester)
        }
    }

    /// Build an ingester that handles a subset of the streams in `map`.
    fn try_build(
        map: HashMap<String, usize>,
        data: Self::CreationData,
    ) -> Result<(Self, Vec<String>), IngestionError>;

    /// Convert a single record into an event ready for the evaluator.
    fn ingest(&self, record: Self::Record) -> Result<Event, IngestionError>;
}

/// Provides per-field extraction functions for mapping a custom struct into
/// stream values.
pub trait FieldMapper: Send {
    /// Arbitrary data forwarded to [`extractor_for`] during construction.
    type CreationData: Clone + Send;

    /// Per-field error type.
    type Error: Into<IngestionError> + Send + 'static;

    /// Return a boxed closure that extracts the stream value named `name`.
    fn extractor_for(
        name: &str,
        data: Self::CreationData,
    ) -> Result<FieldExtractor<Self, Self::Error>, Self::Error>;
}

/// Annotates a type with the [`StreamIngester`] that accepts it as a record.
pub trait HasIngester {
    /// The associated ingester type.
    type Ingester: StreamIngester<Record = Self> + Sized;
}

/// Public re-export of [`FieldMapper`]: marks a record type as exposing a field mapper.
pub use self::FieldMapper as InputMap;
/// Public re-export of [`HasIngester`]: associates a record type with its ingester.
pub use self::HasIngester as AssociatedEventFactory;
/// Public re-export of [`StreamIngester`]: the trait implemented by all ingesters.
pub use self::StreamIngester as EventFactory;
