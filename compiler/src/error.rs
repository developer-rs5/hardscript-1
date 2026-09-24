use crate::catalog;
use crate::token::Span;
use std::fmt;

/// A secondary span attached to a primary diagnostic (Diagnostics V2). Used
/// for "declared here", "first defined here", and similar two-place notes.
/// Each related span may point into a different source file; the renderer
/// loads that file on demand.
#[derive(Debug, Clone)]
pub struct Related {
    pub location: Option<String>,
    pub span: Option<Span>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Lex,
    Parse,
    Type,
    Codegen,
    Fmt,
    /// Module-graph errors: missing modules, import cycles, bad paths.
    Module,
}

impl ErrorKind {
    pub fn name(&self) -> &'static str {
        match self {
            ErrorKind::Lex => "Lex Error",
            ErrorKind::Parse => "Syntax Error",
            ErrorKind::Type => "Type Error",
            ErrorKind::Codegen => "Compile Error",
            ErrorKind::Fmt => "Format Error",
            ErrorKind::Module => "Module Error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Diag {
    pub kind: ErrorKind,
    pub message: String,
    pub span: Option<Span>,
    pub location: Option<String>,
    pub suggestion: Option<String>,
    pub notes: Vec<String>,
    pub expected: Option<String>,
    pub received: Option<String>,
    /// Diagnostic catalog code (see [`crate::catalog`]). Every diagnostic
    /// carries one; the constructor seeds a default scoped to [`ErrorKind`]
    /// and call sites override it with their granular code via `with_code`.
    pub code: u16,
    /// A short "how to fix" line (rendered as a `help:` footer).
    pub help: Option<String>,
    /// Secondary spans shown under the primary location.
    pub related: Vec<Related>,
}

/// `true` when a catalog code belongs to the warning range (HS2000..HS2099).
/// The severity drives both the label (`error` vs `warning`) and the render
/// color; it is derived from the code so the catalog stays the single source
/// of truth.
pub fn is_warning(code: u16) -> bool {
    (2000..=2099).contains(&code)
}

impl Diag {
    pub fn new(kind: ErrorKind, message: impl Into<String>, span: Span, suggestion: impl Into<String>) -> Diag {
        Diag {
            kind,
            message: message.into(),
            span: Some(span),
            location: None,
            suggestion: Some(suggestion.into()),
            notes: Vec::new(),
            expected: None,
            received: None,
            code: catalog::default_for(kind),
            help: None,
            related: Vec::new(),
        }
    }

    pub fn new_nospan(kind: ErrorKind, message: impl Into<String>) -> Diag {
        Diag {
            kind,
            message: message.into(),
            span: None,
            location: None,
            suggestion: None,
            notes: Vec::new(),
            expected: None,
            received: None,
            code: catalog::default_for(kind),
            help: None,
            related: Vec::new(),
        }
    }

    /// Assign the granular catalog code for this diagnostic.
    pub fn with_code(mut self, code: u16) -> Diag {
        debug_assert!(catalog::lookup(code).is_some(), "unknown diagnostic code HS{code:04}");
        self.code = code;
        self
    }

    /// Attach a short "how to fix" line shown as a `help:` footer.
    pub fn with_help(mut self, h: impl Into<String>) -> Diag {
        self.help = Some(h.into());
        self
    }

    /// Attach a secondary span (declaration site, first definition, ...).
    pub fn with_related(mut self, location: impl Into<String>, span: Span, label: impl Into<String>) -> Diag {
        self.related.push(Related {
            location: Some(location.into()),
            span: Some(span),
            label: Some(label.into()),
        });
        self
    }

    pub fn with_location(mut self, loc: impl Into<String>) -> Diag {
        self.location = Some(loc.into());
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Diag {
        self.notes.push(note.into());
        self
    }

    /// What the compiler wanted here (e.g. `an identifier`, `Int`).
    pub fn with_expected(mut self, e: impl Into<String>) -> Diag {
        self.expected = Some(e.into());
        self
    }

    /// What was actually found instead (e.g. the offending token).
    pub fn with_received(mut self, r: impl Into<String>) -> Diag {
        self.received = Some(r.into());
        self
    }

    /// Renders a human-friendly, multi-line diagnostic like:
    ///
    ///   Syntax Error [HS0002]
    ///   Expected:   an identifier
    ///   Received:   `}
    ///   Location:   routes/users.hard:18
    ///   Message:    expected an identifier, found }
    ///   Suggestion: Provide a name after the keyword.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("{} [HS{:04}]", self.kind.name(), self.code));
        out.push('\n');
        if let Some(exp) = &self.expected {
            out.push_str("Expected:   ");
            out.push_str(exp);
            out.push('\n');
        }
        if let Some(rec) = &self.received {
            out.push_str("Received:   ");
            out.push_str(rec);
            out.push('\n');
        }
        if let Some(loc) = &self.location {
            out.push_str("Location:   ");
            out.push_str(loc);
            out.push('\n');
        } else if let Some(sp) = &self.span {
            out.push_str("Location:   ");
            out.push_str(&format!("{}:{}", sp.line, sp.col));
            out.push('\n');
        }
        out.push_str("Message:    ");
        out.push_str(&self.message);
        out.push('\n');
        if let Some(s) = &self.suggestion {
            out.push_str("Suggestion: ");
            out.push_str(s);
            out.push('\n');
        }
        for n in &self.notes {
            out.push_str("Note:       ");
            out.push_str(n);
            out.push('\n');
        }
        if let Some(h) = &self.help {
            out.push_str("Help:       ");
            out.push_str(h);
            out.push('\n');
        }
        out
    }
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.render())
    }
}

impl std::error::Error for Diag {}

pub type Result<T> = std::result::Result<T, Vec<Diag>>;

pub fn render_all(diags: &[Diag]) -> String {
    diags.iter().map(|d| d.render()).collect::<Vec<_>>().join("\n")
}