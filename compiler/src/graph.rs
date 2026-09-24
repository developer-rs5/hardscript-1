//! Module dependency graph.
//!
//! Every local `.hard` file in a project is a node; `bring "./path"` imports
//! become edges. The graph discovers the transitive closure from a root
//! module, reports missing files, deduplicates imports and detects cycles, and
//! produces a deterministic topological order (dependencies first) used by the
//! incremental builder.
//!
//! Builtin runtime modules (`bring http`, `bring std.crypto`) are recorded on
//! each node but never form edges — they are provided by the runtime, not by
//! another `.hard` file.

use crate::ast::Module;
use crate::error::{Diag, ErrorKind};
use crate::json::Json;
use crate::parser::scan_imports;
use crate::sha256;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Guards against pathological import chains overflowing the stack during the
/// recursive DFS at discovery time.
const MAX_MODULE_DEPTH: usize = 256;

fn module_name(m: &Module) -> &'static str {
    match m {
        Module::Http => "http",
        Module::Postgres => "postgres",
        Module::WebSocket => "websocket",
        Module::Crypto => "crypto",
        Module::Json => "json",
        Module::Fs => "fs",
        Module::Jwt => "jwt",
        Module::Env => "env",
        Module::Runtime => "runtime",
        Module::Time => "time",
    }
}

/// One `.hard` module in the graph.
#[derive(Debug, Clone)]
pub struct ModuleNode {
    /// Stable deterministic id: the discovery (DFS) index, root = 0.
    pub id: usize,
    /// Path relative to the project root, `/`-separated (`main.hard`,
    /// `models/user.hard`).
    pub rel: String,
    /// Absolute path on disk.
    pub path: PathBuf,
    /// SHA-256 of the file contents (hex).
    pub source_sha256: String,
    /// Builtin runtime modules imported by this file.
    pub builtins: Vec<String>,
    /// Local import strings exactly as written (`./models/user`).
    pub imports: Vec<String>,
    /// Resolved local dependency ids.
    pub deps: Vec<usize>,
    /// Position in the topological order (`order`).
    pub order: usize,
}

/// Discovery state for one node while the DFS is in flight.
const VISITING: u8 = 1;
const DONE: u8 = 2;

/// The fully-resolved dependency graph for a project.
#[derive(Debug, Clone)]
pub struct ModuleGraph {
    pub root: usize,
    /// Directory containing the root module; all relative paths resolve here.
    pub project_root: PathBuf,
    pub nodes: Vec<ModuleNode>,
    /// Deterministic topological order — dependencies first. Node ids.
    pub order: Vec<usize>,
}

impl ModuleGraph {
    /// Resolve a node id to its relative path, or `"?"`.
    pub fn rel_of(&self, id: usize) -> &str {
        self.nodes.get(id).map(|n| n.rel.as_str()).unwrap_or("?")
    }

    /// Dump the graph as canonical JSON ({@link to_json}).
    pub fn to_json(&self) -> String {
        Json::obj(vec![
            ("root", Json::str(self.rel_of(self.root))),
            ("count", Json::num(self.nodes.len() as i64)),
            ("modules", Json::arr(self.nodes.iter().map(node_json).collect())),
        ])
        .to_string()
    }

    /// Render the graph as a compact adjacency listing for `doctor --deps`.
    /// Deterministic: iterates nodes in discovery (id) order.
    pub fn to_deps(&self) -> String {
        let mut out = String::new();
        for n in &self.nodes {
            let deps: Vec<String> = n
                .deps
                .iter()
                .map(|d| self.rel_of(*d).to_string())
                .collect();
            let builtins = n.builtins.join(", ");
            out.push_str(&n.rel);
            if deps.is_empty() && builtins.is_empty() {
                out.push_str(" -> (no imports)\n");
            } else {
                out.push_str(" -> ");
                out.push_str(&deps.join(", "));
                if !builtins.is_empty() {
                    if !deps.is_empty() {
                        out.push_str(", ");
                    }
                    out.push_str(&builtins);
                }
                out.push('\n');
            }
        }
        out
    }
}

fn node_json(n: &ModuleNode) -> Json {
    Json::obj(vec![
        ("id", Json::num(n.id as i64)),
        ("path", Json::str(n.rel.clone())),
        ("sha256", Json::str(n.source_sha256.clone())),
        ("deps", Json::arr(n.deps.iter().map(|d| Json::num(*d as i64)).collect())),
        ("imports", Json::arr(n.imports.iter().map(|s| Json::str(s.clone())).collect())),
        ("builtins", Json::arr(n.builtins.iter().map(|s| Json::str(s.clone())).collect())),
        ("order", Json::num(n.order as i64)),
    ])
}

/// Normalize a `.hard` module path written in a `bring` into a project-root
/// relative key. Leading `./` is trimmed, the extension is added if missing.
pub fn resolve_import(import: &str) -> String {
    let mut p = import.replace('\\', "/");
    while let Some(stripped) = p.strip_prefix("./") {
        p = stripped.to_string();
    }
    if !p.ends_with(".hard") {
        p.push_str(".hard");
    }
    p
}

struct Discovery {
    project_root: PathBuf,
    nodes: Vec<ModuleNode>,
    index_by_key: HashMap<String, usize>,
    state: Vec<u8>,
    stack: Vec<usize>,
    depth: usize,
    order: Vec<usize>,
}

/// Discover the transitive module graph rooted at `root_file`.
///
/// Always resolves relative to `root_file`'s directory, so the same project
/// produces the same graph regardless of the working directory the compiler
/// is invoked from. Errors are returned as module diagnostics (missing files,
/// cycles, malformed paths).
pub fn discover(root_file: &Path) -> Result<ModuleGraph, Vec<Diag>> {
    let root = root_file.canonicalize().unwrap_or_else(|_| root_file.to_path_buf());
    let project_root = root
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let mut d = Discovery {
        project_root: project_root.clone(),
        nodes: Vec::new(),
        index_by_key: HashMap::new(),
        state: Vec::new(),
        stack: Vec::new(),
        depth: 0,
        order: Vec::new(),
    };
    let root_rel = root
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("main.hard")
        .to_string();
    d.visit(&root_rel, 1)?;
    Ok(ModuleGraph {
        root: 0,
        project_root,
        nodes: d.nodes,
        order: d.order,
    })
}

impl Discovery {
    /// DFS visit of one module (by project-root-relative path). Returns its
    /// node id on success, or a module diagnostic on a missing file/cycle.
    fn visit(&mut self, rel: &str, line: usize) -> Result<usize, Vec<Diag>> {
        let key = resolve_import(rel);
        if let Some(id) = self.index_by_key.get(&key) {
            // Already seen. On the current stack it is a cycle.
            if self.state[*id] == VISITING {
                return Err(vec![self.cycle_diag(*id)]);
            }
            return Ok(*id);
        }

        if self.depth >= MAX_MODULE_DEPTH {
            return Err(vec![Diag::new(
                ErrorKind::Module,
                format!("import chain too deep (limit {MAX_MODULE_DEPTH}) at '{key}'"),
                crate::token::Span::new(line, 0),
                "Break the import chain into a flatter dependency graph.",
            )
            .with_code(crate::catalog::IMPORT_CHAIN_TOO_DEEP)]);
        }

        let abs = self.project_root.join(&key);
        let contents = match std::fs::read_to_string(&abs) {
            Ok(c) => c,
            Err(_) => {
                return Err(vec![Diag::new(
                    ErrorKind::Module,
                    format!("module '{}' not found", rel.trim_end_matches(".hard")),
                    crate::token::Span::new(line, 0),
                    format!("Looked for {} (relative to {}).", abs.display(), self.project_root.display()),
                )
                .with_location(self.project_root.join(key.clone()).display().to_string())
                .with_code(crate::catalog::MODULE_NOT_FOUND)])
            }
        };

        let scan = scan_imports(&contents);
        let id = self.nodes.len();
        let builtins: Vec<String> = scan.builtins.iter().map(module_name).map(String::from).collect();
        let node = ModuleNode {
            id,
            rel: key.clone(),
            path: abs.clone(),
            source_sha256: sha256::hex(contents.as_bytes()),
            builtins,
            imports: scan.paths.clone(),
            deps: Vec::new(),
            order: 0,
        };
        self.index_by_key.insert(key.clone(), id);
        self.nodes.push(node);
        self.state.push(VISITING);
        self.stack.push(id);
        self.depth += 1;

        // Recurse into local imports in source order (deterministic).
        let deps: Vec<usize> = {
            let mut dep_ids = Vec::new();
            for imp in &scan.paths {
                let child = match self.visit(imp, line) {
                    Ok(c) => c,
                    Err(mut ds) => {
                        // Attach the importing file to each message so cycles
                        // and missing modules name their origin.
                        for d in ds.iter_mut() {
                            if d.location.is_none() {
                                d.location = Some(abs.display().to_string());
                            }
                        }
                        self.depth -= 1;
                        self.state[id] = DONE;
                        self.stack.pop();
                        return Err(ds);
                    }
                };
                if !dep_ids.contains(&child) {
                    dep_ids.push(child);
                }
            }
            dep_ids
        };

        self.depth -= 1;
        self.state[id] = DONE;
        self.stack.pop();
        self.nodes[id].deps = deps;
        self.nodes[id].order = self.order.len();
        self.order.push(id);
        Ok(id)
    }

    fn cycle_diag(&self, repeated: usize) -> Diag {
        // The stack holds the path from root to the node that re-entered
        // `repeated`; slice from the first occurrence of `repeated`.
        let start = self
            .stack
            .iter()
            .position(|&id| id == repeated)
            .unwrap_or(0);
        let seg: Vec<&str> = self.stack[start..]
            .iter()
            .map(|&id| self.nodes[id].rel.as_str())
            .collect();
        let mut flow = seg.join(" -> ");
        flow.push_str(" -> ");
        flow.push_str(seg.first().map(|s| *s).unwrap_or("?"));
        Diag::new(
            ErrorKind::Module,
            format!("import cycle detected: {flow}"),
            crate::token::Span::new(1, 0),
            "Remove the circular `bring` between these modules (e.g. move the shared code into a third module).",
        )
        .with_code(crate::catalog::IMPORT_CYCLE)
    }
}

/// Count how many distinct local modules are imported (transitively) by the
/// root module — excludes root itself and builtin modules.
pub fn imported_count(g: &ModuleGraph) -> usize {
    g.order.len().saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Write the given `(rel, content)` modules into a scratch dir and return
    /// the path of the root file (as given by the caller, e.g. `main.hard`).
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Fixture {
            let dir = std::env::temp_dir().join(format!(
                "hs-graph-test-{}-{}",
                std::process::id(),
                unique()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            for (rel, content) in files {
                let p = dir.join(rel);
                if let Some(parent) = p.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(&p, content).unwrap();
            }
            Fixture { dir }
        }
        fn file(&self, rel: &str) -> PathBuf {
            self.dir.join(rel)
        }
    }

    fn unique() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        format!("{n}")
    }

    #[test]
    fn single_module() {
        let f = Fixture::new(&[("main.hard", "bring http\n\nGET \"/\" :: { <- \"ok\" }\n")]);
        let g = discover(&f.file("main.hard")).unwrap();
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.rel_of(0), "main.hard");
        assert_eq!(g.nodes[0].builtins, vec!["http"]);
        assert_eq!(g.order, vec![0]);
    }

    #[test]
    fn nested_imports_resolve_in_order() {
        let f = Fixture::new(&[
            ("main.hard", "bring \"./models/user\"\n"),
            ("models/user.hard", "bring std.json\n"),
        ]);
        let g = discover(&f.file("main.hard")).unwrap();
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.rel_of(1), "models/user.hard");
        // Dependencies come first in topological order.
        assert_eq!(g.order, vec![1, 0]);
        assert_eq!(g.nodes[1].builtins, vec!["json"]);
    }

    #[test]
    fn cycle_detection_reports_flow() {
        let f = Fixture::new(&[
            ("main.hard", "bring \"./a\"\n"),
            ("a.hard", "bring \"./b\"\n"),
            ("b.hard", "bring \"./a\"\n"),
        ]);
        let err = discover(&f.file("main.hard")).unwrap_err();
        let text = crate::error::render_all(&err);
        assert!(text.contains("import cycle detected"), "got: {text}");
        assert!(text.contains("a.hard"), "got: {text}");
        assert!(text.contains("b.hard"), "got: {text}");
    }

    #[test]
    fn missing_module_is_an_error() {
        let f = Fixture::new(&[("main.hard", "bring \"./nope\"\n")]);
        let err = discover(&f.file("main.hard")).unwrap_err();
        let text = crate::error::render_all(&err);
        assert!(text.contains("not found"), "got: {text}");
    }

    #[test]
    fn duplicate_import_is_deduplicated() {
        let f = Fixture::new(&[
            ("main.hard", "bring \"./a\"\nbring \"./a\"\n"),
            ("a.hard", ""),
        ]);
        let g = discover(&f.file("main.hard")).unwrap();
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.nodes[0].deps.len(), 1);
        assert_eq!(g.nodes[0].imports.len(), 2); // both kept as written
    }

    #[test]
    fn diamond_dependency_loads_once() {
        let f = Fixture::new(&[
            ("main.hard", "bring \"./a\"\nbring \"./b\"\n"),
            ("a.hard", "bring \"./shared\"\n"),
            ("b.hard", "bring \"./shared\"\n"),
            ("shared.hard", ""),
        ]);
        let g = discover(&f.file("main.hard")).unwrap();
        assert_eq!(g.nodes.len(), 4);
        // shared appears before its dependents in topo order.
        let sidx = g.nodes.iter().position(|n| n.rel == "shared.hard").unwrap();
        let a = g.nodes.iter().position(|n| n.rel == "a.hard").unwrap();
        let b = g.nodes.iter().position(|n| n.rel == "b.hard").unwrap();
        let mainsidx = g.nodes.iter().position(|n| n.rel == "main.hard").unwrap();
        assert!(g.nodes[sidx].order < g.nodes[a].order);
        assert!(g.nodes[sidx].order < g.nodes[b].order);
        assert!(g.nodes[a].order < g.nodes[mainsidx].order);
        assert!(g.nodes[b].order < g.nodes[mainsidx].order);
    }

    #[test]
    fn json_is_deterministic() {
        let f = Fixture::new(&[
            ("main.hard", "bring \"./a\"\n"),
            ("a.hard", "bring std.crypto\n"),
        ]);
        let g = discover(&f.file("main.hard")).unwrap();
        let json1 = g.to_json();
        let json2 = g.to_json();
        assert_eq!(json1, json2);
        assert!(json1.contains("sha256"));
        assert!(json1.contains("std.crypto") || json1.contains("crypto"));
    }

    #[test]
    fn std_spelling_is_a_builtin_not_a_node() {
        let f = Fixture::new(&[("main.hard", "bring std.crypto\n")]);
        let g = discover(&f.file("main.hard")).unwrap();
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].builtins, vec!["crypto"]);
    }
}