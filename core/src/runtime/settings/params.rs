//! Runtime parameters: timing strategies and execution modes.
//!
//! This module is the merged result of the original `configuration/config.rs`
//! and `configuration/time.rs`.  It defines the [`RunMode`] trait, the two
//! concrete run modes ([`LiveMode`] and [`ReplayMode`]), [`RuntimeSpec`] and
//! [`Monitorinitialize`], as well as every timestamp codec available in the system.

use std::fmt::Debug;
use std::marker::PhantomData;
use std::ops::Sub;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

#[cfg(not(feature = "serde"))]
use humantime::Rfc3339Timestamp;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::oorvir::refined::OORVIR;

use crate::runtime::iface::ingest::{EventFactory, IngestionError};
use crate::runtime::iface::watcher::{DeltaOutput, OutputFormat};
#[cfg(feature = "queued-api")]
use crate::runtime::AsyncMonitor;
use crate::runtime::Monitor;
use crate::runtime::{CondDeserialize, CondSerialize};

// ─── Helper macros ───────────────────────────────────────────────────────────

macro_rules! impl_cast_string {
    ($codec:ty) => {
        impl TimestampCast<String> for $codec {
            fn cast(from: <Self as TimestampCodec>::InnerTime) -> String {
                <$codec>::default().format(from)
            }
        }
    };
}

macro_rules! impl_cast_unit {
    ($codec:ty) => {
        impl TimestampCast<()> for $codec {
            fn cast(_from: <Self as TimestampCodec>::InnerTime) -> () {}
        }
    };
}

macro_rules! impl_cast_duration {
    ($codec:ty) => {
        impl TimestampCast<Duration> for $codec {
            fn cast(from: <Self as TimestampCodec>::InnerTime) -> Duration {
                <$codec>::default().decode(from)
            }
        }
    };
}

// ─── Constants ───────────────────────────────────────────────────────────────

const NANOS_PER_SECOND: u64 = 1_000_000_000;

// ─── Core types ──────────────────────────────────────────────────────────────

/// A shared, mutable anchor point in wall-clock time used to compute relative timestamps.
pub(crate) type SharedAnchor = Arc<RwLock<Option<SystemTime>>>;

// ─── RunMode ─────────────────────────────────────────────────────────────────

/// Describes how the monitor obtains event timestamps.
///
/// Implement this trait to define a custom timing strategy, or use one of the
/// two built-in implementations: [`LiveMode`] and [`ReplayMode`].
pub trait RunMode: Default {
    /// The [`TimestampCodec`] used to interpret (or generate) timestamps.
    type SourceTime: TimestampCodec;

    /// Construct a new `RunMode` from an already-initialised codec instance.
    fn from_clock(clock: Self::SourceTime) -> Self;

    /// Borrow the active codec.
    fn clock(&self) -> &Self::SourceTime;
}

// ─── LiveMode ─────────────────────────────────────────────────────────────────

/// Online execution: the monitor reads wall-clock time for every event.
///
/// Timestamps in the input are ignored; [`WallClock`] measures elapsed time
/// from the moment the monitor is created.
#[derive(Debug, Clone, Default)]
pub struct LiveMode {
    clock: WallClock,
}

impl RunMode for LiveMode {
    type SourceTime = WallClock;

    fn from_clock(clock: Self::SourceTime) -> Self {
        Self { clock }
    }

    fn clock(&self) -> &Self::SourceTime {
        &self.clock
    }
}

// ─── ReplayMode ───────────────────────────────────────────────────────────────

/// Offline (replay) execution: timestamps are supplied by the event source.
///
/// The type parameter `InputTime` selects the [`TimestampCodec`] used to
/// interpret timestamps found in the input stream.
#[derive(Debug, Copy, Clone, Default)]
pub struct ReplayMode<InputTime: TimestampCodec> {
    clock: InputTime,
}

impl<InputTime: TimestampCodec> RunMode for ReplayMode<InputTime> {
    type SourceTime = InputTime;

    fn from_clock(clock: Self::SourceTime) -> Self {
        Self { clock }
    }

    fn clock(&self) -> &Self::SourceTime {
        &self.clock
    }
}

// ─── RuntimeSpec ──────────────────────────────────────────────────────────────

/// Combines an OORV specification with all parameters needed to run the monitor.
#[derive(Clone, Debug)]
pub struct RuntimeSpec<Mode: RunMode, OutTime: OutputTimestamp> {
    /// The compiled specification.
    pub ir: OORVIR,
    /// Timing strategy (live or replay).
    pub mode: Mode,
    /// Phantom marker for the output timestamp type.
    pub output_time_representation: PhantomData<OutTime>,
    /// Optional wall-clock reference used to align the first event.
    pub start_time: Option<SystemTime>,
}

impl RuntimeSpec<ReplayMode<RelSeconds>, RelSeconds> {
    /// Construct a minimal spec suitable for unit tests and debugging.
    pub fn for_testing(ir: OORVIR) -> Self {
        RuntimeSpec {
            ir,
            mode: ReplayMode::default(),
            output_time_representation: PhantomData,
            start_time: None,
        }
    }
}

impl<Mode: RunMode, OutTime: OutputTimestamp> RuntimeSpec<Mode, OutTime> {
    /// Construct a spec configured for direct API use (replay mode, relative-float output).
    pub fn for_api(ir: OORVIR) -> RuntimeSpec<ReplayMode<RelSeconds>, OutTime> {
        RuntimeSpec {
            ir,
            mode: ReplayMode::default(),
            output_time_representation: PhantomData,
            start_time: None,
        }
    }

    /// Directly construct a [`Monitor`] from this spec, passing `data` to the input source.
    pub fn into_monitor<Source: EventFactory, Verdict: OutputFormat>(
        self,
        data: Source::CreationData,
    ) -> Result<Monitor<Source, Mode, Verdict, OutTime>, IngestionError> {
        Monitor::initialize(self, data)
    }
}

// ─── Monitorinitialize ─────────────────────────────────────────────────────────────

/// A fully-typed configuration ready to produce a [`Monitor`] or [`AsyncMonitor`].
#[derive(Debug, Clone)]
pub struct Monitorinitialize<Source, Mode, Verdict = DeltaOutput, VerdictTime = RelSeconds>
where
    Source: EventFactory,
    Mode: RunMode,
    Verdict: OutputFormat,
    VerdictTime: OutputTimestamp,
{
    spec: RuntimeSpec<Mode, VerdictTime>,
    input: PhantomData<Source>,
    verdict: PhantomData<Verdict>,
}

impl<
        Source: EventFactory + 'static,
        Mode: RunMode,
        Verdict: OutputFormat,
        VerdictTime: OutputTimestamp,
    > Monitorinitialize<Source, Mode, Verdict, VerdictTime>
{
    /// Wrap a [`RuntimeSpec`] in a `Monitorinitialize` for the given source and verdict types.
    pub fn from_spec(spec: RuntimeSpec<Mode, VerdictTime>) -> Self {
        Self {
            spec,
            input: PhantomData,
            verdict: PhantomData,
        }
    }

    /// Borrow the underlying [`RuntimeSpec`].
    pub fn spec(&self) -> &RuntimeSpec<Mode, VerdictTime> {
        &self.spec
    }

    /// Build a [`Monitor`], providing `data` to initialise the event source.
    pub fn build_with(
        self,
        data: Source::CreationData,
    ) -> Result<Monitor<Source, Mode, Verdict, VerdictTime>, IngestionError> {
        Monitor::initialize(self.spec, data)
    }

    /// Build a [`Monitor`] when the event source requires no initialisation data.
    pub fn build(self) -> Result<Monitor<Source, Mode, Verdict, VerdictTime>, IngestionError>
    where
        Source: EventFactory<CreationData = ()>,
    {
        Monitor::initialize(self.spec, ())
    }
}

#[cfg(feature = "queued-api")]
impl<
        Source: EventFactory + 'static,
        InputTime: TimestampCodec,
        Verdict: OutputFormat,
        VerdictTime: OutputTimestamp,
    > Monitorinitialize<Source, ReplayMode<InputTime>, Verdict, VerdictTime>
{
    /// Build a [`AsyncMonitor`], providing `data` to initialise the event source.
    pub fn async_build_with(
        self,
        data: Source::CreationData,
    ) -> AsyncMonitor<Source, ReplayMode<InputTime>, Verdict, VerdictTime> {
        <AsyncMonitor<Source, ReplayMode<InputTime>, Verdict, VerdictTime>>::initialize(
            self.spec, data,
        )
    }

    /// Build a [`AsyncMonitor`] when the event source requires no initialisation data.
    pub fn async_build(self) -> AsyncMonitor<Source, ReplayMode<InputTime>, Verdict, VerdictTime>
    where
        Source: EventFactory<CreationData = ()>,
    {
        <AsyncMonitor<Source, ReplayMode<InputTime>, Verdict, VerdictTime>>::initialize(
            self.spec,
            (),
        )
    }
}

#[cfg(feature = "queued-api")]
impl<Source: EventFactory + 'static, Verdict: OutputFormat, VerdictTime: OutputTimestamp>
    Monitorinitialize<Source, LiveMode, Verdict, VerdictTime>
{
    /// Build a [`AsyncMonitor`], providing `data` to initialise the event source.
    pub fn async_build_with(
        self,
        data: Source::CreationData,
    ) -> AsyncMonitor<Source, LiveMode, Verdict, VerdictTime> {
        <AsyncMonitor<Source, LiveMode, Verdict, VerdictTime>>::initialize(self.spec, data)
    }

    /// Build a [`AsyncMonitor`] when the event source requires no initialisation data.
    pub fn async_build(self) -> AsyncMonitor<Source, LiveMode, Verdict, VerdictTime>
    where
        Source: EventFactory<CreationData = ()>,
    {
        <AsyncMonitor<Source, LiveMode, Verdict, VerdictTime>>::initialize(self.spec, ())
    }
}

// ─── parse_duration_str ───────────────────────────────────────────────────────

/// Parses a decimal string of the form `"{secs}.{subsecs}"` into a [`Duration`].
pub fn parse_duration_str(s: &str) -> Result<Duration, String> {
    let num = Decimal::from_str(s).map_err(|e| e.to_string())?;
    let nanos = (num.fract() * Decimal::from(NANOS_PER_SECOND))
        .to_u32()
        .ok_or_else(|| "nanosecond value out of range".to_string())?;
    let secs = num
        .trunc()
        .to_u64()
        .ok_or_else(|| "second value out of range".to_string())?;
    Ok(Duration::new(secs, nanos))
}

// ─── TimestampCodec ───────────────────────────────────────────────────────────

/// Core interface that every time format must implement.
pub trait TimestampCodec:
    Clone + Send + Default + CondSerialize + CondDeserialize + 'static
{
    /// The raw type as seen in the event source (e.g. `Duration`, `u64`, `()`).
    type InnerTime: Debug + Clone + Send + CondSerialize + CondDeserialize;

    /// Whether timestamps are present in the input at all.
    fn provided_by_input() -> bool {
        true
    }

    /// Convert a raw value from the event source into monitor-internal time.
    fn decode(&mut self, inner: Self::InnerTime) -> Duration;

    /// Convert monitor-internal time back to the raw representation.
    fn encode(&self, ts: Duration) -> Self::InnerTime;

    /// Render the raw representation as a human-readable string.
    fn format(&self, ts: Self::InnerTime) -> String;

    /// Attempt to parse a raw value from a string slice.
    fn parse(s: &'_ str) -> Result<Self::InnerTime, String>;

    /// Return a default anchor if the codec can supply one automatically.
    fn default_anchor() -> Option<SystemTime> {
        Some(SystemTime::now())
    }

    /// Initialise the anchor and return a handle to the shared state.
    fn init_anchor(&mut self, anchor: Option<SystemTime>) -> SharedAnchor {
        Arc::new(RwLock::new(anchor.or_else(Self::default_anchor)))
    }

    /// Adopt an already-initialised anchor shared with another codec instance.
    fn adopt_anchor(&mut self, _anchor: SharedAnchor) {}
}

// ─── OutputTimestamp ─────────────────────────────────────────────────────────

/// Marker for [`TimestampCodec`] implementations usable in output verdicts.
pub trait OutputTimestamp: TimestampCodec {}

// ─── TimestampCast ────────────────────────────────────────────────────────────

/// Generic conversion from a codec's `InnerTime` to an arbitrary target type `T`.
pub trait TimestampCast<T>: OutputTimestamp {
    fn cast(from: <Self as TimestampCodec>::InnerTime) -> T;
}

impl<O: OutputTimestamp> TimestampCast<O::InnerTime> for O {
    fn cast(from: <Self as TimestampCodec>::InnerTime) -> O::InnerTime {
        from
    }
}

// ─── RelNanos ─────────────────────────────────────────────────────────────────

/// Relative timestamps expressed as unsigned nanoseconds from a fixed anchor.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Copy, Clone, Default)]
pub struct RelNanos {}

impl TimestampCodec for RelNanos {
    type InnerTime = u64;

    fn decode(&mut self, nanos: Self::InnerTime) -> Duration {
        Duration::from_nanos(nanos)
    }

    fn encode(&self, ts: Duration) -> Self::InnerTime {
        ts.as_nanos() as u64
    }

    fn format(&self, ts: Self::InnerTime) -> String {
        ts.to_string()
    }

    fn parse(s: &'_ str) -> Result<u64, String> {
        u64::from_str(s).map_err(|e| e.to_string())
    }
}
impl OutputTimestamp for RelNanos {}

impl TimestampCast<f64> for RelNanos {
    fn cast(from: Self::InnerTime) -> f64 {
        from as f64
    }
}
impl_cast_string!(RelNanos);
impl_cast_duration!(RelNanos);
impl_cast_unit!(RelNanos);

// ─── RelSeconds ───────────────────────────────────────────────────────────────

/// Relative timestamps as decimal seconds (e.g. `"5.200000000"`).
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Copy, Clone, Default)]
pub struct RelSeconds {}

impl TimestampCodec for RelSeconds {
    type InnerTime = Duration;

    fn decode(&mut self, ts: Self::InnerTime) -> Duration {
        ts
    }

    fn encode(&self, ts: Duration) -> Self::InnerTime {
        ts
    }

    fn format(&self, ts: Self::InnerTime) -> String {
        format!("{}.{:09}", ts.as_secs(), ts.subsec_nanos())
    }

    fn parse(s: &str) -> Result<Duration, String> {
        parse_duration_str(s)
    }
}
impl OutputTimestamp for RelSeconds {}

impl_cast_string!(RelSeconds);
impl_cast_unit!(RelSeconds);
impl TimestampCast<f64> for RelSeconds {
    fn cast(from: Self::InnerTime) -> f64 {
        from.as_secs_f64()
    }
}

// ─── DeltaNanos ───────────────────────────────────────────────────────────────

/// Delta (incremental) timestamps as unsigned nanoseconds.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Copy, Clone, Default)]
pub struct DeltaNanos {
    accumulated: Duration,
    prev: Duration,
}

impl TimestampCodec for DeltaNanos {
    type InnerTime = u64;

    fn decode(&mut self, raw: Self::InnerTime) -> Duration {
        self.prev = self.accumulated;
        self.accumulated += Duration::from_nanos(raw);
        self.accumulated
    }

    fn encode(&self, ts: Duration) -> Self::InnerTime {
        ts.sub(self.prev).as_nanos() as u64
    }

    fn format(&self, ts: Self::InnerTime) -> String {
        ts.to_string()
    }

    fn parse(s: &'_ str) -> Result<u64, String> {
        u64::from_str(s).map_err(|e| e.to_string())
    }
}

// ─── DeltaSeconds ─────────────────────────────────────────────────────────────

/// Delta (incremental) timestamps as decimal seconds.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Copy, Clone, Default)]
pub struct DeltaSeconds {
    accumulated: Duration,
    prev: Duration,
}

impl TimestampCodec for DeltaSeconds {
    type InnerTime = Duration;

    fn decode(&mut self, gap: Duration) -> Duration {
        self.prev = self.accumulated;
        self.accumulated += gap;
        self.accumulated
    }

    fn encode(&self, ts: Duration) -> Self::InnerTime {
        ts - self.prev
    }

    fn format(&self, ts: Self::InnerTime) -> String {
        format!("{}.{:09}", ts.as_secs(), ts.subsec_nanos())
    }

    fn parse(s: &str) -> Result<Duration, String> {
        parse_duration_str(s)
    }
}

// ─── EpochSeconds ─────────────────────────────────────────────────────────────

/// Absolute timestamps as decimal seconds since the Unix epoch.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Default)]
pub struct EpochSeconds {
    anchor: SharedAnchor,
}

impl TimestampCodec for EpochSeconds {
    type InnerTime = Duration;

    fn decode(&mut self, since_epoch: Duration) -> Duration {
        let wall = SystemTime::UNIX_EPOCH + since_epoch;
        let guard = *self.anchor.read().unwrap();
        if let Some(a) = guard {
            wall.duration_since(a)
                .expect("non-monotonic timestamp detected in EpochSeconds")
        } else {
            *self.anchor.write().unwrap() = Some(wall);
            Duration::ZERO
        }
    }

    fn encode(&self, ts: Duration) -> Self::InnerTime {
        let wall = self.anchor.read().unwrap().unwrap() + ts;
        wall.duration_since(SystemTime::UNIX_EPOCH)
            .expect("non-monotonic timestamp in EpochSeconds::encode")
    }

    fn format(&self, ts: Duration) -> String {
        format!("{}.{:09}", ts.as_secs(), ts.subsec_nanos())
    }

    fn parse(s: &str) -> Result<Duration, String> {
        parse_duration_str(s)
    }

    fn init_anchor(&mut self, anchor: Option<SystemTime>) -> SharedAnchor {
        self.anchor = Arc::new(RwLock::new(anchor));
        self.anchor.clone()
    }

    fn adopt_anchor(&mut self, anchor: SharedAnchor) {
        self.anchor = anchor;
    }

    fn default_anchor() -> Option<SystemTime> {
        None
    }
}
impl OutputTimestamp for EpochSeconds {}

impl_cast_string!(EpochSeconds);
impl_cast_unit!(EpochSeconds);
impl TimestampCast<f64> for EpochSeconds {
    fn cast(from: Self::InnerTime) -> f64 {
        from.as_secs_f64()
    }
}

// ─── EpochRfc3339 ─────────────────────────────────────────────────────────────

/// Absolute timestamps in RFC 3339 format.
///
/// Only available when the `serde` feature is **disabled**.
#[cfg(not(feature = "serde"))]
#[derive(Debug, Clone, Default)]
pub struct EpochRfc3339 {
    anchor: SharedAnchor,
}

#[cfg(not(feature = "serde"))]
impl TimestampCodec for EpochRfc3339 {
    type InnerTime = Rfc3339Timestamp;

    fn decode(&mut self, rfc: Self::InnerTime) -> Duration {
        let wall = rfc.get_ref();
        let guard = *self.anchor.read().unwrap();
        if let Some(a) = guard {
            wall.duration_since(a)
                .expect("non-monotonic timestamp in EpochRfc3339")
        } else {
            *self.anchor.write().unwrap() = Some(*wall);
            Duration::ZERO
        }
    }

    fn encode(&self, ts: Duration) -> Self::InnerTime {
        let wall = self.anchor.read().unwrap().unwrap() + ts;
        humantime::format_rfc3339(wall)
    }

    fn format(&self, ts: Self::InnerTime) -> String {
        ts.to_string()
    }

    fn parse(s: &'_ str) -> Result<Self::InnerTime, String> {
        let t = humantime::parse_rfc3339(s).map_err(|e| e.to_string())?;
        Ok(humantime::format_rfc3339(t))
    }

    fn init_anchor(&mut self, anchor: Option<SystemTime>) -> SharedAnchor {
        self.anchor = Arc::new(RwLock::new(anchor));
        self.anchor.clone()
    }

    fn adopt_anchor(&mut self, anchor: SharedAnchor) {
        self.anchor = anchor;
    }

    fn default_anchor() -> Option<SystemTime> {
        None
    }
}
#[cfg(not(feature = "serde"))]
impl OutputTimestamp for EpochRfc3339 {}

#[cfg(not(feature = "serde"))]
impl_cast_string!(EpochRfc3339);

#[cfg(not(feature = "serde"))]
impl_cast_unit!(EpochRfc3339);

#[cfg(not(feature = "serde"))]
impl TimestampCast<f64> for EpochRfc3339 {
    fn cast(from: Self::InnerTime) -> f64 {
        from.get_ref()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }
}

// ─── FixedStep ────────────────────────────────────────────────────────────────

/// A synthetic clock that advances by a fixed duration on every event.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, Default)]
pub struct FixedStep {
    current: Duration,
    step: Duration,
}

impl FixedStep {
    /// Construct a `FixedStep` codec with the given step interval.
    pub fn with_step(step: Duration) -> Self {
        FixedStep {
            current: Duration::ZERO,
            step,
        }
    }
}

impl TimestampCodec for FixedStep {
    type InnerTime = ();

    fn provided_by_input() -> bool {
        false
    }

    fn decode(&mut self, _ignored: Self::InnerTime) -> Duration {
        self.current += self.step;
        self.current
    }

    fn encode(&self, _ts: Duration) -> Self::InnerTime {}

    fn format(&self, _ts: Self::InnerTime) -> String {
        format!(
            "{}.{:09}",
            self.current.as_secs(),
            self.current.subsec_nanos()
        )
    }

    fn parse(_s: &str) -> Result<(), String> {
        Ok(())
    }
}

// ─── WallClock ────────────────────────────────────────────────────────────────

/// Real-time clock: timestamps are taken from the system clock, not the input.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Default)]
pub struct WallClock {
    last: Duration,
    anchor: SharedAnchor,
}

impl TimestampCodec for WallClock {
    type InnerTime = ();

    fn provided_by_input() -> bool {
        false
    }

    fn decode(&mut self, _ignored: Self::InnerTime) -> Duration {
        let now = SystemTime::now();
        let guard = *self.anchor.read().unwrap();
        self.last = if let Some(a) = guard {
            now.duration_since(a)
                .expect("system clock moved backwards in WallClock")
        } else {
            *self.anchor.write().unwrap() = Some(now);
            Duration::ZERO
        };
        self.last
    }

    fn encode(&self, _ts: Duration) -> Self::InnerTime {}

    fn format(&self, _ts: Self::InnerTime) -> String {
        let wall = self.anchor.read().unwrap().unwrap() + self.last;
        humantime::format_rfc3339(wall).to_string()
    }

    fn parse(_s: &str) -> Result<(), String> {
        Ok(())
    }

    fn init_anchor(&mut self, anchor: Option<SystemTime>) -> SharedAnchor {
        self.anchor = Arc::new(RwLock::new(anchor.or_else(Self::default_anchor)));
        self.anchor.clone()
    }

    fn adopt_anchor(&mut self, anchor: SharedAnchor) {
        self.anchor = anchor;
    }
}
