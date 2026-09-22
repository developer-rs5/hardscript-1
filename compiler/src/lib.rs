//! hs_compiler — the HardScript compiler front end.
//!
//! Pipeline: source text -> [lexer] -> [parser] -> [typecheck] -> [codegen]
//! (C++ emitted against the single-header `hs_runtime.hpp`) -> `g++` ->
//! native executable.
//!
//! The high-level driver to use is [`frontend`] (lex + parse) or
//! [`compile_to_cpp`] (parse + typecheck + codegen).

pub mod ast;
pub mod codegen;
pub mod diagnostics;
pub mod docs;
pub mod error;
pub mod fmt;
pub mod lexer;
pub mod optimizer;
pub mod parser;
pub mod token;
pub mod typecheck;

pub use error::{render_all, Diag, ErrorKind};
pub use lexer::Lexer;
pub use parser::Parser;
pub use token::{Span, Tok, Token};

use crate::ast::Program;

/// Lex a HardScript source string.
///
/// Returns `(Some(tokens), [])` on success, or `(None, diags)` when the
/// source does not tokenize cleanly.
pub fn lex(src: &str) -> (Option<Vec<Token>>, Vec<Diag>) {
    match Lexer::new(src).tokenize() {
        Ok(t) => (Some(t), Vec::new()),
        Err(d) => (None, d),
    }
}

/// Parse a token stream into a [`Program`].
pub fn parse(toks: Vec<Token>) -> Result<Program, Vec<Diag>> {
    Parser::new(toks).parse_program()
}

/// Lex + parse a source string. The [`Program`] receives `path` so
/// diagnostics and codegen can report locations against it.
pub fn frontend(src: &str, path: impl Into<String>) -> Result<Program, Vec<Diag>> {
    let path = path.into();
    let (toks, diags) = lex(src);
    let Some(toks) = toks else {
        return Err(diags
            .into_iter()
            .map(|d| d.with_location(path.clone()))
            .collect());
    };
    match parse(toks) {
        Ok(mut p) => {
            p.path = path;
            Ok(p)
        }
        Err(ds) => Err(ds
            .into_iter()
            .map(|d| d.with_location(path.clone()))
            .collect()),
    }
}

/// Full pipeline: parse -> typecheck -> codegen. Returns the generated C++
/// source on success, or the collected diagnostics on failure.
pub fn compile_to_cpp(src: &str, path: impl Into<String>) -> Result<String, Vec<Diag>> {
    let path = path.into();
    let mut prog = frontend(src, path.clone())?;

    // AST optimization (safe, semantics-preserving; see optimizer module).
    let _optimized = optimizer::run(&mut prog);

    let mut errs = typecheck::check(&prog);

    // Ignore codegen diagnostics that are carried as errors only when severe.
    errs.retain(|d| d.kind == ErrorKind::Type);
    if errs.is_empty() {
        match codegen::generate(&prog) {
            Ok(cpp) => Ok(cpp),
            Err(d) => Err(vcat_err(&path, &prog, d)),
        }
    } else {
        Err(vcat_type(&path, errs))
    }
}

fn vcat_type(path: &str, mut errs: Vec<Diag>) -> Vec<Diag> {
    for d in errs.iter_mut() {
        if d.location.is_none() {
            d.location = Some(path.to_string());
        }
    }
    errs
}

fn vcat_err(path: &str, prog: &Program, mut errs: Vec<Diag>) -> Vec<Diag> {
    let _ = prog;
    for d in errs.iter_mut() {
        if d.location.is_none() {
            d.location = Some(path.to_string());
        }
    }
    errs
}

/// Render diagnostics to a multi-line string (see [`render_all`]).
pub fn explain(diags: &[Diag]) -> String {
    render_all(diags)
}