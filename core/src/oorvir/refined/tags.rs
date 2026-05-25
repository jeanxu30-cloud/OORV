// Tag parser and validators for verbosity and debug annotations used by backends for logging.

use std::collections::{HashMap, HashSet};

use crate::diagnostic::{Diagnostic, OORVError};
use crate::oorvir::source::{ConstraintKind, StreamIdx};

#[derive(Debug, Copy, Clone)]
// Tag validator that resolves the verbosity level annotated on each stream.
pub struct VerbosityParser;

// Verbosity level attached to a stream via annotation.
#[derive(Debug, Clone, Copy)]
pub enum StreamVerbosity {
    // Tagged as verbosity level `streams`.
    Streams,
    // Tagged as verbosity level `outputs`.
    Outputs,
    // Tagged as verbosity level `public`.
    Public,
    // Tagged as verbosity level `warning`.
    Warnings,
    // Tagged as verbosity level `violation`.
    Violations,
}

impl TagParser for VerbosityParser {
    type GlobalTags = ();
    type LocalTags = StreamVerbosity;

    fn parse_global(
        &self,
        _global_tags: &HashMap<String, Option<String>>,
        _ir: &OORVIR,
    ) -> Result<Self::GlobalTags, OORVError> {
        Ok(())
    }

    fn parse_local(
        &self,
        sr: StreamIdx,
        tags: &HashMap<String, Option<String>>,
        refined_ir: &OORVIR,
    ) -> Result<Self::LocalTags, OORVError> {
        let verbosity_entries = tags
            .iter()
            .filter_map(|(key, value)| {
                let resolved = match (key.as_str(), value) {
                    ("verbosity", Some(level_name)) => Some(match level_name.as_str() {
                        "streams" => Ok(StreamVerbosity::Streams),
                        "outputs" => Ok(StreamVerbosity::Outputs),
                        "public" => Ok(StreamVerbosity::Public),
                        "warnings" => Ok(StreamVerbosity::Warnings),
                        "violations" => Ok(StreamVerbosity::Violations),
                        other => Err(Diagnostic::error(&format!(
                            "Annotated unexpected verbosity {other} on stream {}",
                            refined_ir.resolve_stream(sr).name()
                        ))),
                    }),
                    ("verbosity", None) => Some(Err(Diagnostic::error(&format!(
                        "Missing verbosity value on annotation on stream {}",
                        refined_ir.resolve_stream(sr).name()
                    )))),
                    ("warning", None) => Some(Ok(StreamVerbosity::Warnings)),
                    ("warning", Some(_)) => panic!(),
                    ("violation", None) => Some(Ok(StreamVerbosity::Violations)),
                    ("violation", Some(_)) => panic!(),
                    ("public", None) => Some(Ok(StreamVerbosity::Public)),
                    ("public", Some(_)) => panic!(),
                    (_, _) => None,
                };
                resolved
            })
            .collect::<Result<Vec<_>, _>>()?;
        match verbosity_entries.len() {
            0 => match sr {
                StreamIdx::Signal(_) => Ok(StreamVerbosity::Streams),
                StreamIdx::Constraint(_) => match refined_ir.constraint(sr).kind {
                    ConstraintKind::Output(_) => Ok(StreamVerbosity::Outputs),
                    ConstraintKind::Alarm(_) => Ok(StreamVerbosity::Violations),
                },
            },
            1 => Ok(verbosity_entries[0]),
            2.. => Err(Diagnostic::error(&format!(
                "Specified multiple verbosities on stream {}",
                refined_ir.resolve_stream(sr).name()
            ))
            .into()),
        }
    }
}

impl TagValidator for VerbosityParser {
    fn supported_tags<'a>(&self, _ir: &'a OORVIR) -> (HashSet<&'a str>, HashSet<&'a str>) {
        (
            HashSet::new(),
            vec!["verbosity", "warning", "violation", "public"]
                .into_iter()
                .collect(),
        )
    }
}

#[derive(Debug, Copy, Clone)]
// Tag validator that resolves the debug annotation on each stream.
pub struct DebugParser;

impl TagParser for DebugParser {
    type GlobalTags = ();
    type LocalTags = bool;

    fn parse_global(
        &self,
        _global_tags: &HashMap<String, Option<String>>,
        _ir: &OORVIR,
    ) -> Result<Self::GlobalTags, OORVError> {
        Ok(())
    }

    fn parse_local(
        &self,
        sr: StreamIdx,
        tags: &HashMap<String, Option<String>>,
        refined_ir: &OORVIR,
    ) -> Result<Self::LocalTags, OORVError> {
        match tags.get("debug") {
            Some(None) => Ok(true),
            None => Ok(false),
            Some(Some(_)) => Err(Diagnostic::error(&format!(
                "The debug tag on stream {} received an unexpected value",
                refined_ir.resolve_stream(sr).name()
            ))
            .into()),
        }
    }
}

impl TagValidator for DebugParser {
    fn supported_tags<'a>(&self, _ir: &'a OORVIR) -> (HashSet<&'a str>, HashSet<&'a str>) {
        (HashSet::new(), vec!["debug"].into_iter().collect())
    }
}

use super::OORVIR;

// Represents a parser that handles a subset of tags annotated in the specification.
pub trait TagParser: TagValidator {
    // The type produced when parsing global tags from a specification.
    type GlobalTags;
    // The type produced when parsing stream-local tags.
    type LocalTags;

    // Parse the global tag map and produce the corresponding GlobalTags value.
    fn parse_global(
        &self,
        global_tags: &HashMap<String, Option<String>>,
        refined_ir: &OORVIR,
    ) -> Result<Self::GlobalTags, OORVError>;

    // Parse the tag map attached to stream `sr` and produce the corresponding LocalTags value.
    fn parse_local(
        &self,
        sr: StreamIdx,
        tags: &HashMap<String, Option<String>>,
        refined_ir: &OORVIR,
    ) -> Result<Self::LocalTags, OORVError>;
}

// Declares which tag keys are recognised by a TagParser; all other keys are silently ignored.
pub trait TagValidator {
    // Returns the set of supported global and local tag keys for this parser.
    fn supported_tags<'a>(&self, refined_ir: &'a OORVIR) -> (HashSet<&'a str>, HashSet<&'a str>);
}
