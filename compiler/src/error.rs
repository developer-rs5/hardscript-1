use crate::token::Span;
use std::fmt;

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
        }
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
    ///   Type Error
    ///   Expected:   Int
    ///   Received:   Text
    ///   Location:   routes/users.hard:18
    ///   Suggestion: Convert using Int(value)
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(self.kind.name());
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