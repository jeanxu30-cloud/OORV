use std::cell::RefCell;
use std::fmt::{self, Display};
use std::io::{self, Write};
use std::sync::Arc;

use codespan_reporting::diagnostic::{Diagnostic as CsDiag, Label, Severity as CsSeverity};
use codespan_reporting::files::SimpleFile;
use codespan_reporting::term;
use codespan_reporting::term::termcolor::{ColorChoice, StandardStream};

// ── Severity ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

// ── Source file ──────────────────────────────────────────────────────────────

/// A source file (name + content) used for annotated error output.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub name: String,
    pub text: String,
}

// ── Source context thread-local ──────────────────────────────────────────────

thread_local! {
    static CURRENT_SOURCE_CTX: RefCell<Option<Arc<SourceFile>>> = RefCell::new(None);
}

/// Set the thread-local source context (called from the parser before parsing begins).
pub fn set_source_context(name: &str, text: &str) {
    let src = Arc::new(SourceFile {
        name: name.to_string(),
        text: text.to_string(),
    });
    CURRENT_SOURCE_CTX.with(|c| *c.borrow_mut() = Some(src));
}

/// Clear the thread-local source context (called after parsing is complete).
pub fn clear_source_context() {
    CURRENT_SOURCE_CTX.with(|c| *c.borrow_mut() = None);
}

/// Get a clone of the current thread-local source context Arc, if any.
pub fn current_source_arc() -> Option<Arc<SourceFile>> {
    CURRENT_SOURCE_CTX.with(|c| c.borrow().clone())
}

// ── SpanLabel ────────────────────────────────────────────────────────────────

/// A byte-range label attached to a source span.
#[derive(Debug, Clone)]
pub struct SpanLabel {
    pub start: usize,
    pub end: usize,
    pub message: String,
    pub primary: bool,
}

// ── Diagnostic ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Short human-readable summary (was `message` in older versions).
    pub title: String,
    pub labels: Vec<SpanLabel>,
    pub notes: Vec<String>,
    /// When present, span labels are rendered against this source text.
    pub source: Option<Arc<SourceFile>>,
}

impl Diagnostic {
    pub fn error(msg: &str) -> Self {
        Diagnostic {
            severity: Severity::Error,
            title: msg.to_string(),
            labels: Vec::new(),
            notes: Vec::new(),
            source: None,
        }
    }

    pub fn warning(msg: &str) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            title: msg.to_string(),
            labels: Vec::new(),
            notes: Vec::new(),
            source: None,
        }
    }

    pub fn info(msg: &str) -> Self {
        Diagnostic {
            severity: Severity::Info,
            title: msg.to_string(),
            labels: Vec::new(),
            notes: Vec::new(),
            source: None,
        }
    }

    // Builder: attach a SourceSpan with an optional label text.
    pub fn add_span_with_label<L: AsRef<str>>(
        mut self,
        span: crate::oorvast::SourceSpan,
        label: Option<L>,
        primary: bool,
    ) -> Self {
        use crate::oorvast::SourceSpan;
        let label_msg = label.map(|l| l.as_ref().to_string()).unwrap_or_default();
        match span {
            SourceSpan::Direct { start, end } | SourceSpan::Indirect { start, end } => {
                self.labels.push(SpanLabel {
                    start,
                    end,
                    message: label_msg,
                    primary,
                });
            }
            SourceSpan::Unknown => {
                if !label_msg.is_empty() {
                    self.title.push_str(&format!(" ({})", label_msg));
                }
            }
        }
        self
    }

    // Builder: same as `add_span_with_label` but skips when span is `None`.
    pub fn maybe_add_span_with_label<L: AsRef<str>>(
        self,
        span: Option<crate::oorvast::SourceSpan>,
        label: Option<L>,
        primary: bool,
    ) -> Self {
        match span {
            Some(s) => self.add_span_with_label(s, label, primary),
            None => self,
        }
    }

    // Builder: attach a precise byte-range span for rich rendering.
    pub fn add_span_range<L: Into<String>>(
        mut self,
        start: usize,
        end: usize,
        label: Option<L>,
        primary: bool,
    ) -> Self {
        self.labels.push(SpanLabel {
            start,
            end,
            message: label.map(|l| l.into()).unwrap_or_default(),
            primary,
        });
        self
    }

    // Builder: append a free-form note.
    pub fn add_note(mut self, note: &str) -> Self {
        self.notes.push(note.to_string());
        self
    }

    // Builder: attach source context used for span rendering.
    pub fn with_source(mut self, src: Arc<SourceFile>) -> Self {
        self.source = Some(src);
        self
    }

    /// Builder: try to attach source context from the thread-local set by the parser.
    /// No-op if source is already set or thread-local is empty.
    pub fn try_attach_source(self) -> Self {
        if self.source.is_none() {
            if let Some(src) = current_source_arc() {
                return self.with_source(src);
            }
        }
        self
    }

    /// Emit the diagnostic to stderr.
    /// Uses codespan-reporting to render annotated source when a source file
    /// and at least one span label are present; falls back to plain text.
    /// Automatically uses the thread-local source context if none was explicitly attached.
    pub fn emit(&self) {
        // Lazily resolve source from thread-local if not explicitly set.
        let source = self.source.clone().or_else(current_source_arc);
        if let Some(src) = &source {
            if !self.labels.is_empty() {
                let file = SimpleFile::new(&src.name, &src.text);
                let cs_sev = match self.severity {
                    Severity::Error => CsSeverity::Error,
                    Severity::Warning => CsSeverity::Warning,
                    Severity::Info => CsSeverity::Note,
                };
                let mut cs_labels: Vec<Label<()>> = Vec::new();
                for sl in &self.labels {
                    let label = if sl.primary {
                        Label::primary((), sl.start..sl.end)
                    } else {
                        Label::secondary((), sl.start..sl.end)
                    };
                    cs_labels.push(if sl.message.is_empty() {
                        label
                    } else {
                        label.with_message(&sl.message)
                    });
                }
                let diag: CsDiag<()> = CsDiag::new(cs_sev)
                    .with_message(&self.title)
                    .with_labels(cs_labels)
                    .with_notes(self.notes.clone());
                let writer = StandardStream::stderr(ColorChoice::Auto);
                let config = term::Config::default();
                let _ = term::emit(&mut writer.lock(), &config, &file, &diag);
                return;
            }
        }
        // Plain-text fallback.
        self.emit_plain();
    }

    fn emit_plain(&self) {
        let no_color = std::env::var("NO_COLOR").is_ok();
        let (plain, colored) = match self.severity {
            Severity::Error => ("error", "\x1b[1;31merror\x1b[0m"),
            Severity::Warning => ("warning", "\x1b[33mwarning\x1b[0m"),
            Severity::Info => ("info", "\x1b[34minfo\x1b[0m"),
        };
        let prefix = if no_color { plain } else { colored };
        let _ = writeln!(io::stderr(), "{}: {}", prefix, self.title);
        for note in &self.notes {
            let _ = writeln!(io::stderr(), "  = note: {}", note);
        }
    }
}

// ── OORVError ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct OORVError(pub Vec<Diagnostic>);

impl From<Diagnostic> for OORVError {
    fn from(d: Diagnostic) -> Self {
        OORVError(vec![d])
    }
}

impl From<Vec<Diagnostic>> for OORVError {
    fn from(v: Vec<Diagnostic>) -> Self {
        OORVError(v)
    }
}

impl Display for OORVError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, d) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            let sev = match d.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Info => "info",
            };
            write!(f, "{}: {}", sev, d.title)?;
        }
        Ok(())
    }
}

impl std::error::Error for OORVError {}

impl OORVError {
    pub fn new() -> Self {
        OORVError(Vec::new())
    }

    pub fn combine<A, B, F>(a: A, b: B, _f: F) -> OORVError
    where
        A: Into<OORVError>,
        B: Into<OORVError>,
        F: FnOnce((), ()),
    {
        let mut a = a.into();
        let b = b.into();
        a.0.extend(b.0);
        a
    }

    pub fn boxed(self) -> Box<dyn std::error::Error> {
        for d in &self.0 {
            d.emit();
        }
        Box::new(self)
    }

    pub fn push(&mut self, d: Diagnostic) {
        self.0.push(d);
    }
    pub fn add(&mut self, d: Diagnostic) {
        self.push(d);
    }
    pub fn join(&mut self, other: OORVError) {
        self.0.extend(other.0);
    }
}

impl FromIterator<Diagnostic> for OORVError {
    fn from_iter<I: IntoIterator<Item = Diagnostic>>(iter: I) -> Self {
        OORVError(iter.into_iter().collect())
    }
}

impl IntoIterator for OORVError {
    type Item = Diagnostic;
    type IntoIter = std::vec::IntoIter<Diagnostic>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a OORVError {
    type Item = &'a Diagnostic;
    type IntoIter = std::slice::Iter<'a, Diagnostic>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl From<Result<(), OORVError>> for OORVError {
    fn from(r: Result<(), OORVError>) -> Self {
        match r {
            Ok(_) => OORVError::new(),
            Err(e) => e,
        }
    }
}

impl From<OORVError> for Result<(), OORVError> {
    fn from(e: OORVError) -> Self {
        // Only escalate to an Err result when at least one diagnostic has
        // Error severity; warnings and info-level entries do not fail the pass.
        if e.0.iter().any(|d| matches!(d.severity, Severity::Error)) {
            Err(e)
        } else {
            Ok(())
        }
    }
}
