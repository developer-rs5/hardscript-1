//! Workspace support: detect `workspace` members in a `hard.toml` and run
//! `hard` operations across every member deterministically.

use crate::manifest::{parse, Manifest, ManifestMode};
use crate::tui::Logger;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A workspace discovered from a project root.
#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub members: Vec<Member>,
}

/// A single workspace member project.
#[derive(Debug)]
pub struct Member {
    pub path: PathBuf,
    pub manifest: Manifest,
}

impl Workspace {
    /// Discover a workspace starting from `dir`. If `dir` (or an ancestor)
    /// has a `hard.toml` with a `workspace` list, returns the workspace with
    /// every member loaded. Otherwise returns `None` (standalone project).
    pub fn discover(dir: &Path, log: &Logger) -> Result<Option<Workspace>, String> {
        let mut cursor = Some(dir);
        while let Some(d) = cursor {
            let manifest_path = d.join("hard.toml");
            if manifest_path.is_file() {
                let toml = std::fs::read_to_string(&manifest_path)
                    .map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
                let man = match parse(&toml, ManifestMode::Lenient) {
                    Ok(res) => res.manifest,
                    Err(e) => {
                        let mut msg = format!(
                            "invalid hard.toml at {}:",
                            manifest_path.display()
                        );
                        for er in e {
                            msg.push_str(&format!("\n  {}", er.message));
                        }
                        return Err(msg);
                    }
                };
                if !man.workspace.is_empty() {
                    log.note(&format!("workspace root: {}", d.display()));
                    let mut members = Vec::new();
                    for rel in &man.workspace {
                        let member_dir = d.join(rel);
                        let member_toml = member_dir.join("hard.toml");
                        if !member_toml.is_file() {
                            return Err(format!(
                                "workspace member '{}' has no hard.toml (missing at {})",
                                rel,
                                member_toml.display()
                            ));
                        }
                        let src = std::fs::read_to_string(&member_toml)
                            .map_err(|e| format!("read {}: {e}", member_toml.display()))?;
                        let mman = match parse(&src, ManifestMode::Lenient) {
                            Ok(res) => res.manifest,
                            Err(e) => {
                                let mut msg = format!(
                                    "invalid hard.toml at {}:",
                                    member_toml.display()
                                );
                                for er in e {
                                    msg.push_str(&format!("\n  {}", er.message));
                                }
                                return Err(msg);
                            }
                        };
                        members.push(Member {
                            path: member_dir,
                            manifest: mman,
                        });
                    }
                    members.sort_by(|a, b| a.path.cmp(&b.path));
                    return Ok(Some(Workspace {
                        root: d.to_path_buf(),
                        members,
                    }));
                }
                // A hard.toml that is not a workspace root stops upward search.
                return Ok(None);
            }
            cursor = d.parent();
        }
        Ok(None)
    }

    /// Load a workspace from its root; error if it is not a workspace root.
    pub fn load(dir: &Path, log: &Logger) -> Result<Workspace, String> {
        match Workspace::discover(dir, log)? {
            Some(w) => Ok(w),
            None => Err(format!(
                "'{}' is not inside a workspace (a hard.toml with a `workspace` list)",
                dir.display()
            )),
        }
    }

    /// Absolute path of member `name`.
    pub fn member_dir(&self, name: &str) -> Option<PathBuf> {
        self.members
            .iter()
            .find(|m| m.manifest.name == name)
            .map(|m| m.path.clone())
    }

    /// The number of members.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Render a stable, multi-line listing of the workspace.
    pub fn list_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(format!("workspace {} ({} member{})", self.root.display(), self.members.len(), if self.members.len() == 1 { "" } else { "s" }));
        for m in &self.members {
            out.push(format!("  {}  {}", m.manifest.name, m.path.display()));
        }
        out
    }

    /// Render a stable, multi-line listing of members plus their version.
    pub fn list_versions_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push("name\tversion\tpath".to_string());
        for m in &self.members {
            out.push(format!(
                "{}\t{}\t{}",
                m.manifest.name,
                m.manifest.version,
                m.path.display()
            ));
        }
        out
    }

    /// Run `hard run|build|test` in every member, in member order.
    /// Returns a log of line output. Aborts (returns Err) only when the hard
    /// binary itself cannot be located.
    pub fn run_members(&self, verb: &str, log: &Logger) -> Vec<String> {
        let hard_path = hard_binary();
        let mut out = Vec::new();
        for m in &self.members {
            out.push(format!("[{}/{}] {} {}", m.manifest.name, verb, m.path.display(), ""));
            let status = match &hard_path {
                Some(bin) => Command::new(bin).arg(verb).current_dir(&m.path).status(),
                None => {
                    log.warn("hard binary not found on PATH (running from source); skipping execution");
                    continue;
                }
            };
            match status {
                Ok(s) if s.success() => out.push(format!("  {}: ok", m.manifest.name)),
                Ok(s) => out.push(format!("  {}: FAILED ({})", m.manifest.name, s)),
                Err(e) => out.push(format!("  {}: error ({e})", m.manifest.name)),
            }
        }
        out
    }
}

/// Turn a `hard` invocation into reproducible one-line output for scripts.
pub fn run_hard_lines(args: &[&str]) -> Result<Vec<String>, String> {
    let bin = hard_binary().ok_or("hard binary not found on PATH")?;
    let out = Command::new(&bin)
        .args(args)
        .output()
        .map_err(|e| format!("spawn hard: {e}"))?;
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect();
    if !out.stderr.is_empty() {
        for l in String::from_utf8_lossy(&out.stderr).lines() {
            if !l.trim().is_empty() {
                lines.push(l.to_string());
            }
        }
    }
    lines.sort();
    Ok(lines)
}

/// Locate the `hard` binary: env `HARD_BIN` first, else `hard` on PATH.
/// Returns `None` when building/using the CLI from source is expected.
pub fn hard_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HARD_BIN") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join("hard");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// Format a member summary for the `workspace list` output.
pub fn list_lines_for(dir: &Path, log: &Logger) -> Result<Vec<String>, String> {
    match Workspace::discover(dir, log)? {
        Some(w) => Ok(w.list_lines()),
        None => {
            log.note("no workspace found; showing current project only");
            Ok(vec![format!("  (not a workspace) {}", dir.display())])
        }
    }
}