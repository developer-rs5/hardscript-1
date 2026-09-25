//! Minimal terminal progress output.
//!
//! When stdout is a terminal, long installs render a single-line `\r`-updated
//! spinner; when piped (CI, tests) every step prints one line so output is
//! still deterministic and greppable.

use std::io::IsTerminal;

/// Whether the current process should draw TTY progress.
pub fn is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// A line-oriented step logger (deterministic in non-TTY mode).
#[derive(Clone, Debug)]
pub struct Logger {
    pub verbose: bool,
}

impl Logger {
    pub fn new(verbose: bool) -> Logger {
        Logger { verbose }
    }

    pub fn note(&self, msg: &str) {
        if self.verbose {
            println!("{msg}");
        }
    }

    pub fn warn(&self, msg: &str) {
        eprintln!("warning: {msg}");
    }

    pub fn err(&self, msg: &str) {
        eprintln!("error: {msg}");
    }

    /// A logger that never emits anything.
    pub fn silent() -> Logger {
        Logger { verbose: false }
    }
}

/// Render a progress event. In non-TTY mode `started`/`finished` produce
/// stable lines; ticks are skipped.
pub mod progress {
    use super::is_tty;

    /// Print the start of an operation (always one line when not a TTY).
    pub fn start(label: &str) {
        if !is_tty() {
            println!("{label} ...");
        }
    }

    /// Update the in-place spinner for long operations (TTY only).
    pub fn tick(label: &str, done: usize, total: usize) {
        if is_tty() {
            let pct = if total == 0 {
                0
            } else {
                (done as f64 / total as f64 * 100.0) as usize
            };
            eprint!("\r{label} [{done}/{total}] {pct}%   ");
        }
    }

    /// Complete an operation (always a line when not a TTY).
    pub fn finish(label: &str, ok: bool) {
        if is_tty() {
            eprintln!("\r{label} {}", if ok { "ok" } else { "FAILED" });
        } else {
            println!("{label} {}", if ok { "ok" } else { "failed" });
        }
    }
}