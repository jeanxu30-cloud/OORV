//! # The OORV Runtime Monitor
//! The OORV runtime monitor is a library used to evaluate OORV specifications.
//! It is designed such that it easily integrates into your setup through highly configurable APIs.
//!
//! ## Features
//! - `queued-api` (Default): By default the library features a queued API that uses threads. If your target architecture doesn't support threads, consider disabling this feature through "default-features = false"
//! - `serde`: Enables Serde Serialization and Deserialization support for API interface structs.
//!
//! ## Usage
//! The main entrypoint of the application is the [crate::runtime::settings::RuntimeSpec].
//! It features multiple methods to configure the interpreter for your needs.
//! From there you can create a [Monitor] or [crate::runtime::iface::async_watcher::AsyncMonitor].
//! The main interaction points of the library.

#![forbid(unused_must_use)] // disallow discarding errors
#![warn(
    missing_debug_implementations,
    missing_copy_implementations,
    trivial_casts,
    trivial_numeric_casts,
    unsafe_code,
    unstable_features,
    unused_import_braces,
    unused_qualifications
)]

pub use crate::oorvir::refined as refined_ir;
pub use itertools::izip;

#[cfg(feature = "queued-api")]
pub use crate::runtime::iface::async_watcher;
#[cfg(feature = "queued-api")]
pub use crate::runtime::iface::async_watcher::AsyncMonitor;
pub use crate::runtime::iface::emitter;
pub use crate::runtime::iface::ingest;
pub use crate::runtime::iface::ingest::FieldExtractor;
pub use crate::runtime::iface::watcher;
pub use crate::runtime::iface::watcher::Monitor;
pub use crate::runtime::store::{Value, ValueConvertError};

mod dispatch;
mod eval;
pub mod iface;
pub mod settings;
mod store;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A helper trait to conditionally require a `serde::Serialize` as a trait bound when the `serde` feature is activated.
#[cfg(feature = "serde")]
pub trait CondSerialize: Serialize {}

#[cfg(not(feature = "serde"))]
/// A helper trait to conditionally require a `serde::Serialize` as a trait bound when the `serde` feature is activated.
pub trait CondSerialize {}

#[cfg(feature = "serde")]
impl<T: Serialize> CondSerialize for T {}
#[cfg(not(feature = "serde"))]
impl<T> CondSerialize for T {}

#[cfg(feature = "serde")]
/// A helper trait to conditionally require a `serde::Deserialize` as a trait bound when the `serde` feature is activated.
pub trait CondDeserialize: for<'a> Deserialize<'a> {}

#[cfg(not(feature = "serde"))]
/// A helper trait to conditionally require a `serde::Deserialize` as a trait bound when the `serde` feature is activated.
pub trait CondDeserialize {}

#[cfg(feature = "serde")]
impl<T: for<'a> Deserialize<'a>> CondDeserialize for T {}
#[cfg(not(feature = "serde"))]
impl<T> CondDeserialize for T {}
