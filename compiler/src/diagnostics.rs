//! Source-aware diagnostics rendering.
//!
//! [`Diag`] carries a span (line + column into the source `path`). This module
//! produces the classic compiler-code-frame alongside the message:
//!
//! ```text
//!  12 |     res <- GET "/" { }
//!     |            ^^^
//! ```
//!
//! `frame` returns `None` when the diagnostic has no span or the source does
//! not contain the referenced line (e.g. end-of-file diagnostics).

use crate::error::Diag;

/// Render a source code frame for a diagnostic, if possible.
pub fn frame(d: &Diag, src: &str) -> Option<String> {
    let sp = d.span?;
    let line = src.lines().nth(sp.line.checked_sub(1)?)?;
    let mut out = String::new();

    let gutter = sp.line.to_string().len().max(1);
    let pad = " ".repeat(gutter);
    let col = sp.col.saturating_sub(1);

    // Cap the caret so it points at something reasonable for the display.
    let caret_col = col.min(line.chars().count().max(1) - 1);
    let caret = " ".repeat(caret_col) + "^";

    out.push_str(&format!("{pad} |\n"));
    out.push_str(&format!("{} | {line}\n", sp.line));
    out.push_str(&format!("{pad} | {caret}\n"));
    Some(out)
}

/// Render a code frame for every diagnostic in a list that has both a span
/// and a resolvable source line.
pub fn frames_all(diags: &[Diag], src: &str) -> String {
    let mut out = String::new();
    for d in diags {
        if let Some(f) = frame(d, src) {
            out.push_str(&f);
            out.push('\n');
        }
    }
    out
}