//! Source-aware diagnostics rendering (Diagnostics V2).
//!
//! [`Diag`] carries a primary span (line + column into its `location`) and an
//! optional list of [`Related`] spans that may point into other files. This
//! module renders the classic rustc-style code frame for every span:
//!
//! ```text
//! error[HS0104]: `user_id` is not defined
//!    --> auth.hard:18:12
//!     |
//!  18 |     <- user_id
//!     |        ^^^^^^^
//!     |
//!     = expected: a defined name or module function
//!     = received: `user_id`
//! help: Define `user_id <- ...` before use.
//! ```
//!
//! Output is byte-deterministic for a given input, `NO_COLOR` / `HARD_COLOR`
//! select the color mode, and long source lines are truncated around the
//! caret (a single diagnostic must never flood the terminal).

use crate::error::{is_warning, Diag};
use std::io::IsTerminal;

/// Long source lines (deep nesting!) are truncated around the caret so a
/// single diagnostic cannot flood the terminal with a 100 kB line.
const MAX_LINE_CHARS: usize = 200;
const TRUNC_MARGIN: usize = 40;

// ANSI SGR sequences. Kept as constants so the mappings are the single
// source of truth for color output.
const RED: &str = "\x1b[1;31m";
const YELLOW: &str = "\x1b[1;33m";
const BLUE: &str = "\x1b[1;34m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Color mode. `render_error` resolves `Auto` once from the environment and
/// the stderr terminal state:
///   * any non-empty `NO_COLOR` disables color,
///   * `HARD_COLOR=1|0` forces it on or off,
///   * otherwise color is used iff stderr is a TTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

/// Resolve the effective color flag from the environment and stderr.
pub fn detect_color(mode: ColorMode) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            if let Ok(v) = std::env::var("NO_COLOR") {
                if !v.is_empty() {
                    return false;
                }
            }
            match std::env::var("HARD_COLOR").as_deref() {
                Ok("1") | Ok("true") | Ok("always") => return true,
                Ok("0") | Ok("false") | Ok("never") => return false,
                _ => {}
            }
            std::io::stderr().is_terminal()
        }
    }
}

/// Severity word + caret color for a catalog code. A promoted diagnostic
/// (escalated by `--deny`) renders as a hard error.
fn severity(d: &Diag) -> (&'static str, &'static str) {
    if d.promote {
        ("error", RED)
    } else if is_warning(d.code) {
        ("warning", YELLOW)
    } else {
        ("error", RED)
    }
}

/// Render a list of diagnostics to a deterministic string, choosing color
/// automatically. This is the entry point used by the CLI's `report`.
pub fn render_error(diags: &[Diag]) -> String {
    render_v2(diags, detect_color(ColorMode::Auto))
}

/// Render with an explicit color flag (used by tests and tooling).
pub fn render_v2(diags: &[Diag], color: bool) -> String {
    let mut out = String::new();
    for (i, d) in diags.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        render_one(d, color, &mut out);
    }
    out
}

fn paint(color: bool, code: &str, body: &str) -> String {
    if color {
        format!("{code}{body}{RESET}")
    } else {
        body.to_string()
    }
}

/// The first line of a diagnostic: `error[HS0104]: message`.
fn header_line(d: &Diag, color: bool, out: &mut String) {
    let (word, caret_color) = severity(d);
    let severity = paint(color, caret_color, word);
    let code = if color {
        format!("{DIM}[{}]{RESET}", crate::catalog::format(d.code))
    } else {
        format!("[{}]", crate::catalog::format(d.code))
    };
    out.push_str(&format!("{severity}{code}: {}\n", d.message));
}

/// Load the source lines for a location (path + line). Returns `None` when
/// the file cannot be read (module errors and I/O failures routinely have no
/// readable primary source).
fn source_line(location: &Option<String>, line: usize) -> Option<String> {
    let path = location.as_ref()?;
    let src = std::fs::read_to_string(path).ok()?;
    src.lines().nth(line.checked_sub(1)?).map(|l| l.to_string())
}

/// Width of the gutter (number of digits) across every line referenced by
/// the primary and related spans of this diagnostic.
fn gutter_width(d: &Diag) -> usize {
    let mut w = 1;
    if let Some(sp) = d.span {
        w = w.max(sp.line.to_string().len());
    }
    for r in &d.related {
        if let Some(sp) = &r.span {
            w = w.max(sp.line.to_string().len());
        }
    }
    w
}

/// How wide the caret should be. Reads the source at the span column and,
/// when the token there is a word, underlines the whole identifier; otherwise
/// a single caret under the position.
fn caret_len(line: &str, col: usize) -> usize {
    let mut chars = line.chars().skip(col.saturating_sub(1)).peekable();
    match chars.peek() {
        Some(c) if c.is_alphanumeric() || *c == '_' => {
            let mut n = 0;
            for c in chars {
                if c.is_alphanumeric() || c == '_' {
                    n += 1;
                } else {
                    break;
                }
            }
            n.max(1)
        }
        _ => 1,
    }
}

/// One code frame: `--> path:line:col` + the source line + caret underline.
/// The `-->` header always prints (the location is meaningful even when the
/// source is gone); the source lines only render when the file is readable.
fn frame(location: &Option<String>, span: crate::token::Span, w: usize, color: bool, d: &Diag, out: &mut String) {
    let left_pad = "  ";
    let path = location.as_deref().unwrap_or("<unknown>");
    out.push_str(&format!("   --> {path}:{}:{}\n", span.line, span.col));

    let Some(line) = source_line(location, span.line) else {
        return;
    };
    let caret_color = if color { severity(d).1 } else { "" };

    out.push_str(&format!("{left_pad}{:>w$} |\n", ""));
    if line.chars().count() <= MAX_LINE_CHARS {
        let cl = caret_len(&line, span.col);
        let caret_col = (span.col.saturating_sub(1)).min(line.chars().count().max(1) - 1);
        let caret = if color {
            format!("{caret_color}{}{RESET}", "^".repeat(cl))
        } else {
            "^".repeat(cl)
        };
        out.push_str(&format!("{left_pad}{:>w$} | {line}\n", span.line));
        out.push_str(&format!("{left_pad}{:>w$} | {}{}\n", "", " ".repeat(caret_col), caret));
    } else {
        let caret_col = span.col.saturating_sub(1);
        let lo = caret_col.saturating_sub(TRUNC_MARGIN);
        let chars: Vec<char> = line.chars().collect();
        let hi = (caret_col + TRUNC_MARGIN).min(chars.len());
        let snippet: String = chars[lo..hi].iter().collect();
        let lead = if lo > 0 { "… " } else { "" };
        let trail = if hi < chars.len() { " …" } else { "" };
        let rel = caret_col - lo;
        let caret = if color {
            format!("{caret_color}^{RESET}")
        } else {
            "^".to_string()
        };
        out.push_str(&format!("{left_pad}{:>w$} | {lead}{snippet}{trail}\n", span.line));
        out.push_str(&format!(
            "{left_pad}{:>w$} | {}{}\n",
            "",
            " ".repeat(lead.len() + rel),
            caret
        ));
    }
    out.push_str(&format!("{left_pad}{:>w$} |\n", ""));
}

fn render_one(d: &Diag, color: bool, out: &mut String) {
    header_line(d, color, out);

    let w = gutter_width(d);
    if d.span.is_some() {
        frame(&d.location, d.span.unwrap(), w, color, d, out);
    }

    // every note / expected / received line is rendered at the frame depth
    let indent = "    ";
    // M3.4.5: always give the reader one line of catalog context, followed by
    // any concrete notes the diagnostic already carries.
    if let Some(def) = crate::catalog::lookup(d.code) {
        out.push_str(&format!("{indent}= {}\n", def.meaning));
    }
    for n in &d.notes {
        out.push_str(&format!("{indent}= {n}\n"));
    }
    if let Some(e) = &d.expected {
        out.push_str(&format!("{indent}= expected: {e}\n"));
    }
    if let Some(r) = &d.received {
        out.push_str(&format!("{indent}= received: {r}\n"));
    }

    // related spans ("first declared here", cross-file declarations, ...)
    for r in &d.related {
        if let Some(label) = &r.label {
            out.push_str(&format!("{indent}= {label}\n"));
        }
        if r.span.is_some() {
            frame(&r.location, r.span.unwrap(), w, color, d, out);
        }
    }

    // help / suggestion footer, falling back to the catalog's first fix when
    // the site carries neither (e.g. internal new_nospan diagnostics).
    let help = d
        .help
        .clone()
        .or_else(|| {
            d.suggestion
                .as_ref()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.clone())
        })
        .or_else(|| {
            crate::catalog::lookup(d.code).and_then(|def| def.fixes.first().map(|f| f.to_string()))
        });
    if let Some(h) = help {
        let label = paint(color, BLUE, "help: ");
        out.push_str(&format!("{label}{h}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog as cat;
    use crate::error::{Diag, ErrorKind, Related};
    use crate::token::Span;

    fn diag() -> Diag {
        Diag::new(ErrorKind::Type, "`x` is not defined", Span::new(3, 5), "Define it.")
            .with_code(cat::UNDEFINED_VARIABLE)
            .with_expected("a defined name")
            .with_received("`x`")
    }

    #[test]
    fn renders_header_with_code() {
        let out = render_v2(&[diag()], false);
        assert!(
            out.starts_with("error[HS0104]: `x` is not defined\n"),
            "got: {out}"
        );
    }

    #[test]
    fn renders_frame_with_relocation() {
        let path = std::env::temp_dir().join("hs_diag_frame.hard");
        std::fs::write(&path, "one\ntwo\nthis_is_user_id <- 1;\nfour\n").unwrap();
        let mut d = diag();
        d.location = Some(path.to_string_lossy().to_string());
        let out = render_v2(&[d], false);
        let loc = path.to_string_lossy();
        assert!(out.contains(&format!("--> {loc}:3:5")), "got: {out}");
        assert!(out.contains("^^^"), "identifier underlined, got: {out}");
        assert!(out.contains("= expected: a defined name"), "got: {out}");
        assert!(out.contains("help: Define it."), "got: {out}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn no_span_still_prints_header_and_help() {
        let out = render_v2(&[diag().with_code(cat::NATIVE_COMPILE_FAILED)], false);
        assert!(out.starts_with("error[HS0502]: `x` is not defined\n"), "got: {out}");
    }

    #[test]
    fn warning_severity_from_code() {
        let d = diag().with_code(cat::W_UNUSED_VARIABLE);
        let out = render_v2(&[d], false);
        assert!(out.starts_with("warning[HS2001]: `x` is not defined\n"), "got: {out}");
    }

    #[test]
    fn catalog_meaning_notes_and_help_fallback() {
        // M3.4.5: every diagnostic gets one line of catalog context plus an
        // actionable help footer — from the site's help, its suggestion, or
        // the catalog's first documented fix as a last resort.
        let out = render_v2(&[diag()], false);
        let meaning = cat::lookup(cat::UNDEFINED_VARIABLE).unwrap().meaning;
        assert!(
            out.contains(&format!("= {meaning}\n")),
            "catalog meaning note missing:\n{out}"
        );
        let bare = Diag::new_nospan(ErrorKind::Codegen, "g++ failed".to_string())
            .with_code(cat::NATIVE_COMPILE_FAILED);
        let out = render_v2(&[bare], false);
        assert!(out.contains("error[HS0502]: g++ failed"), "got: {out}");
        assert!(
            out.contains("help: "),
            "catalog fix fallback should provide help for nospan diags:\n{out}"
        );
    }

    #[test]
    fn no_color_stays_plain_and_deterministic() {
        let a = render_v2(&[diag()], false);
        let b = render_v2(&[diag()], false);
        assert_eq!(a, b);
        assert!(!a.contains('\x1b'));
    }

    #[test]
    fn color_mode_is_forced() {
        assert!(detect_color(ColorMode::Always));
        assert!(!detect_color(ColorMode::Never));
    }

    #[test]
    fn related_span_renders_label_and_frame() {
        let path = std::env::temp_dir().join("hs_diag_rel.hard");
        std::fs::write(&path, "model User = users [\n  id => Int,\n]\n").unwrap();
        let mut d = diag();
        d.code = cat::UNDEFINED_VARIABLE;
        d.location = Some(path.to_string_lossy().to_string());
        d.related.push(Related {
            location: Some(path.to_string_lossy().to_string()),
            span: Some(Span::new(2, 3)),
            label: Some("field `id` declared here".to_string()),
        });
        let out = render_v2(&[d], false);
        assert!(out.contains("= field `id` declared here"), "got: {out}");
        assert!(out.matches("   --> ").count() >= 2, "two frames expected, got: {out}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn related_span_same_line_aligns_gutter_width() {
        let mut d = diag();
        d.related.push(Related {
            location: None,
            span: Some(Span::new(12, 1)),
            label: None,
        });
        assert_eq!(gutter_width(&d), 2);
    }
}