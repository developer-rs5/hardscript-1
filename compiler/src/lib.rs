//! hs_compiler — the HardScript compiler front end.
//!
//! Pipeline: source text -> [lexer] -> [parser] -> [typecheck] -> [codegen]
//! (C++ emitted against the single-header `hs_runtime.hpp`) -> `g++` ->
//! native executable.
//!
//! The high-level driver to use is [`frontend`] (lex + parse) or
//! [`compile_to_cpp`] (parse + typecheck + codegen).

pub mod ast;
pub mod astser;
pub mod build;
pub mod cache;
pub mod codegen;
pub mod diagnostics;
pub mod docs;
pub mod escape;
pub mod error;
pub mod fmt;
pub mod graph;
pub mod hir;
pub mod json;
pub mod lexer;
pub mod manifest;
pub mod optimizer;
pub mod optimize;
pub mod parser;
pub mod sha256;
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

/// Lower a source program into HIR. Errors surface as diagnostics from the
/// front end (lex/parse); lowering itself is total for any parsed program.
pub fn lower_hir(src: &str, path: impl Into<String>) -> Result<hir::HirProgram, Vec<Diag>> {
    let path = path.into();
    let prog = frontend(src, path)?;
    Ok(hir::lower(&prog))
}

/// Render the HIR for a source file (frontend + lowering + pretty printer).
/// This is the output of `hard hir` and the snapshot fixtures.
pub fn hir_string(src: &str, path: impl Into<String>) -> Result<String, Vec<Diag>> {
    Ok(lower_hir(src, path)?.render())
}

/// Optimize a source program to a fixpoint and render before / after trees plus
/// per-pass statistics. This is the output of `hard opt`; the optimizer is
/// pure and never affects codegen.
pub fn opt_string(src: &str, path: impl Into<String>) -> Result<String, Vec<Diag>> {
    let mut p = lower_hir(src, path)?;
    let before = p.render();
    let stats = optimize::run(&mut p);
    let after = p.render();
    Ok(format!(
        "=== before ===\n{before}\n=== after ===\n{after}\n=== optimizer ===\n{}",
        stats.summary()
    ))
}

/// Run the escape analysis over a source program (frontend + optimizer, the
/// same AST shape codegen would see) and return its report. Pure — never
/// changes emitted C++ — exposed for tooling such as `hard build` report mode.
pub fn escape_report(src: &str, path: impl Into<String>) -> Result<escape::EscapeReport, Vec<Diag>> {
    let path = path.into();
    let mut prog = frontend(src, path.clone())?;
    let _ = optimizer::run(&mut prog);
    Ok(escape::analyze(&prog))
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