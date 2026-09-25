//! Staged incremental build pipeline.
//!
//! Stage order: Source → Cache → Parse → Merge → Optimize → Typecheck →
//! Codegen → Native → Link. This module drives everything up to the generated
//! C++ plus the build manifest; the CLI invokes `g++` (the Native → Link
//! stages) because the compiler crate deliberately stays toolchain-free and
//! the runtime headers are embedded in the `hard` binary.
//!
//! Reuse rules:
//! - per-module parse results are cached (`EntryStore`) and reused when the
//!   module's source hash and the environment stamp match;
//! - the whole build is skipped entirely (front-end *and* native) when the
//!   previous build manifest matches the current environment + whole-program
//!   hash and the outputs still exist on disk (warm path).

use crate::ast::Program;
use crate::cache::{EnvStamp, EntryData, EntryStore};
use crate::codegen;
use crate::error::{Diag, ErrorKind};
use crate::graph::{discover, ModuleGraph};
use crate::manifest::{BuildManifest, ModuleStatus, Timings};
use crate::optimizer;
use crate::typecheck;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub compiler: String,
    pub runtime_sha: String,
    pub platform: String,
    pub flags: Vec<String>,
    /// Parallel front-end workers. Never changes output bytes.
    pub jobs: usize,
    /// Binary name suffix: `.release` for release builds.
    pub release: bool,
}

impl BuildOptions {
    pub fn env(&self) -> EnvStamp {
        EnvStamp::new(
            self.compiler.clone(),
            self.runtime_sha.clone(),
            self.platform.clone(),
            self.flags.join(" "),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseOutcome {
    /// AST reused from the cache.
    Hit,
    /// Parsed from source this build.
    Parsed,
}

/// Per-module front-end outcome, in topological order.
#[derive(Debug, Clone)]
pub struct ModuleFrontOutcome {
    pub rel: String,
    pub outcome: ParseOutcome,
    pub items_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct StageTimings {
    pub discover_ms: f64,
    pub parse_ms: f64,
    pub merge_ms: f64,
    pub optimize_ms: f64,
    pub typecheck_ms: f64,
    pub codegen_ms: f64,
}

/// Result of planning one build. `cpp` is `None` on a warm path, where the
/// whole front end was skipped because the previous build is still valid.
#[derive(Debug)]
pub struct Plan {
    pub graph: ModuleGraph,
    pub project_root: PathBuf,
    pub env: EnvStamp,
    pub env_fp: String,
    pub merged_hash: String,
    pub cpp_path: PathBuf,
    pub bin_path: PathBuf,
    /// Generated C++ on non-warm paths; `None` when warm-eligible.
    pub cpp: Option<String>,
    pub outcomes: Vec<ModuleFrontOutcome>,
    pub timings: StageTimings,
    /// Static warnings (Diagnostics V2 M3.4.4) from the warning engine.
    /// Empty on a warm path (nothing was re-analyzed).
    pub warnings: Vec<Diag>,
    pub warm_eligible: bool,
    pub prev: Option<BuildManifest>,
    pub flags: Vec<String>,
    pub jobs: usize,
}

pub fn platform() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// Plan a build of `target`. When `warm_ok` is true and the previous manifest
/// proves everything is unchanged, the front end is skipped (`cpp: None`) and
/// the caller may skip `g++` too.
pub fn plan(target: &Path, opts: &BuildOptions, warm_ok: bool) -> Result<Plan, Vec<Diag>> {
    let t_discover = Instant::now();
    let graph = discover(target)?;
    let discover_ms = t_discover.elapsed().as_secs_f64() * 1000.0;

    let project_root = graph.project_root.clone();
    let env = opts.env();
    let env_fp = env.fingerprint();

    let pairs: Vec<(String, String)> = graph
        .order
        .iter()
        .map(|&id| (graph.rel_of(id).to_string(), graph.nodes[id].source_sha256.clone()))
        .collect();
    let merged_hash = env.merged_hash(&pairs);

    let name = target
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("app")
        .to_string();
    let bin_name = if opts.release {
        format!("{name}.release")
    } else {
        name.clone()
    };
    let cpp_path = project_root.join(".hard").join(format!("{name}.cpp"));
    let bin_path = project_root.join(".hard").join(bin_name);

    let prev = BuildManifest::load(&project_root);
    let warm_eligible = prev
        .as_ref()
        .map(|p| p.is_warm(&env, &merged_hash, &cpp_path, &bin_path))
        .unwrap_or(false);

    if warm_eligible && warm_ok {
        let skipped = graph.order.len();
        let mut counts = crate::manifest::CacheCounts::default();
        counts.hits = skipped;
        counts.skipped = skipped;
        let mut modules = Vec::new();
        for &id in &graph.order {
            let n = &graph.nodes[id];
            modules.push(crate::manifest::ModuleEntry {
                rel: n.rel.clone(),
                source_sha: n.source_sha256.clone(),
                deps: n.deps.iter().map(|d| graph.rel_of(*d).to_string()).collect(),
                status: ModuleStatus::Hit,
                items_bytes: 0,
            });
        }
        let cpp = cpp_path.to_string_lossy().into_owned();
        let binary = bin_path.to_string_lossy().into_owned();
        if let Some(mut m) = prev {
            m.created_ms = now_ms();
            m.native_skipped = true;
            m.cache = counts;
            m.modules = modules;
            m.timings = Timings { discover_ms, native_ms: 0.0, total_ms: 0.0, ..Default::default() };
            m.cpp = cpp;
            m.binary = binary;
            let _ = m.save(&project_root);
        }
        return Ok(Plan {
            graph,
            project_root,
            env,
            env_fp,
            merged_hash,
            cpp_path,
            bin_path,
            cpp: None,
            outcomes: Vec::new(),
            timings: StageTimings { discover_ms, ..Default::default() },
            warnings: Vec::new(),
            warm_eligible: true,
            prev: None,
            flags: opts.flags.clone(),
            jobs: opts.jobs,
        });
    }

    let store = EntryStore::new(&project_root);
    let t_parse = Instant::now();

    // Work items in deterministic (topological) order; parallelism only affects
    // how fast they complete, never the merge order below.
    let jobs_list: Vec<(usize, String)> = graph
        .order
        .iter()
        .map(|&id| {
            let n = &graph.nodes[id];
            (id, EntryStore::parse_key(&n.rel, &n.source_sha256, &env_fp))
        })
        .collect();

    let loaded: Vec<Result<(Vec<crate::ast::Stmt>, ParseOutcome, u64), Vec<Diag>>> =
        if opts.jobs > 1 {
            // Worker stacks must be at least as large as the main thread's:
            // the parser's MAX_DEPTH guard (parser.rs) is tuned below the
            // main-thread overflow point (~450 nested parens on a dev build),
            // but rayon's default worker stack (2 MiB on Linux, 512 KiB on
            // macOS) overflows far earlier than the guard trips. Oversized
            // stacks are reserved lazily, so this costs address space only.
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(opts.jobs)
                .stack_size(64 * 1024 * 1024)
                .build()
                .map_err(|e| {
                    vec![Diag::new(
                        ErrorKind::Module,
                        format!("cannot start worker pool: {e}"),
                        crate::token::Span::new(1, 0),
                        "Lower -j or free OS resources.",
                    )
                    .with_code(crate::catalog::INTERNAL_COMPILER)]
                })?;
            pool.install(|| {
                jobs_list
                    .par_iter()
                    .map(|(id, key)| load_module(*id, &store, &graph, key, &env_fp))
                    .collect::<Vec<_>>()
            })
        } else {
            jobs_list
                .iter()
                .map(|(id, key)| load_module(*id, &store, &graph, key, &env_fp))
                .collect()
        };
    let parse_ms = t_parse.elapsed().as_secs_f64() * 1000.0;

    let mut merged: Vec<crate::ast::Stmt> = Vec::new();
    // Per-statement module provenance (project-root-relative) for Diagnostics
    // V2: lets "declared here" related spans point into the declaring file,
    // e.g. `models/user.hard:5:5` instead of the merged root path.
    let mut merged_files: Vec<Option<String>> = Vec::new();
    let mut outcomes = Vec::new();
    let mut modules: Vec<crate::manifest::ModuleEntry> = Vec::new();
    let mut root_path = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "main.hard".to_string());
    let mut errs: Vec<Diag> = Vec::new();
    for (idx, (id, _key)) in jobs_list.iter().enumerate() {
        let n = &graph.nodes[*id];
        match &loaded[idx] {
            Ok((stmts, outcome, bytes)) => {
                let mut stmts = stmts.clone();
                stmts.retain(|s| !matches!(s, crate::ast::Stmt::Import { .. }));
                merged_files.extend(stmts.iter().map(|_| Some(n.rel.clone())));
                merged.append(&mut stmts);
                if *id == graph.root {
                    // Relative, not absolute: diagnostic/expect locations must
                    // stay stable across machines and project directories.
                    root_path = n.rel.clone();
                }
                outcomes.push(ModuleFrontOutcome {
                    rel: n.rel.clone(),
                    outcome: *outcome,
                    items_bytes: *bytes,
                });
                modules.push(crate::manifest::ModuleEntry {
                    rel: n.rel.clone(),
                    source_sha: n.source_sha256.clone(),
                    deps: n.deps.iter().map(|d| graph.rel_of(*d).to_string()).collect(),
                    status: if *outcome == ParseOutcome::Hit {
                        ModuleStatus::Hit
                    } else {
                        ModuleStatus::Miss
                    },
                    items_bytes: *bytes,
                });
            }
            Err(ds) => errs.extend(ds.iter().cloned()),
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }

    let mut prog = Program { stmts: merged, path: root_path };

    // Static warnings (M3.4.4): analyzed on the UN-optimized tree so they
    // reflect what the user wrote (the optimizer legitimately rewrites away
    // bindings and branches the source still references). Emitted regardless
    // of later stage failures; the CLI surfaces them only on successful
    // builds, and `--deny` decides whether any escalate to hard errors.
    let warnings = crate::warn::analyze(&prog, &merged_files);

    let t_merge = Instant::now();
    let merge_ms = t_merge.elapsed().as_secs_f64() * 1000.0;

    let t_opt = Instant::now();
    let _ = optimizer::run(&mut prog);
    let optimize_ms = t_opt.elapsed().as_secs_f64() * 1000.0;

    let t_tc = Instant::now();
    let mut errs = typecheck::check_with(&prog, &merged_files);
    let typecheck_ms = t_tc.elapsed().as_secs_f64() * 1000.0;

    errs.retain(|d| d.kind == ErrorKind::Type);
    if !errs.is_empty() {
        return Err(attach_paths(&prog.path, errs));
    }

    let t_cg = Instant::now();
    let cpp = match codegen::generate(&prog) {
        Ok(c) => c,
        Err(ds) => return Err(attach_paths(&prog.path, ds)),
    };
    let codegen_ms = t_cg.elapsed().as_secs_f64() * 1000.0;

    Ok(Plan {
        graph,
        project_root,
        env,
        env_fp,
        merged_hash,
        cpp_path,
        bin_path,
        cpp: Some(cpp),
        outcomes,
        timings: StageTimings {
            discover_ms,
            parse_ms,
            merge_ms,
            optimize_ms,
            typecheck_ms,
            codegen_ms,
        },
        warnings,
        warm_eligible: false,
        prev,
        flags: opts.flags.clone(),
        jobs: opts.jobs,
    })
}

/// Load one module's parse either from the cache or from source. Safe to call
/// from any number of parallel workers: distinct modules write distinct cache
/// keys, and reads that race with no writes are impossible (a module is either
/// cached or not in any given build).
fn load_module(
    id: usize,
    store: &EntryStore,
    graph: &ModuleGraph,
    key: &str,
    env_fp: &str,
) -> Result<(Vec<crate::ast::Stmt>, ParseOutcome, u64), Vec<Diag>> {
    let n = &graph.nodes[id];
    if let Some(ed) = store.get(key, &n.source_sha256, env_fp) {
        return Ok((ed.stmts, ParseOutcome::Hit, ed.serialized_bytes));
    }
    let src = std::fs::read_to_string(&n.path).map_err(|e| {
        vec![Diag::new(
            ErrorKind::Module,
            format!("cannot read {}: {e}", n.path.display()),
            crate::token::Span::new(1, 0),
            "The module file must exist and be readable.",
        )
        .with_code(crate::catalog::IO_ERROR)]
    })?;
    let prog = crate::frontend(&src, n.path.display().to_string())?;
    let encoded = crate::astser::serialize_stmts(&prog.stmts).unwrap_or_default();
    let ed = EntryData {
        rel: n.rel.clone(),
        source_sha: n.source_sha256.clone(),
        env_fp: env_fp.to_string(),
        stmts: prog.stmts.clone(),
        serialized_bytes: encoded.len() as u64,
        created_ms: now_ms(),
    };
    store
        .put(key, &ed)
        .map_err(|e| {
            vec![Diag::new(
                ErrorKind::Module,
                format!("cannot write build cache: {e}"),
                crate::token::Span::new(1, 0),
                "Check that .hard/cache is writable.",
            )
            .with_code(crate::catalog::IO_ERROR)]
        })?;
    Ok((prog.stmts, ParseOutcome::Parsed, encoded.len() as u64))
}

/// Attach the merged program's file path to any diagnostics lacking one.
fn attach_paths(path: &str, mut errs: Vec<Diag>) -> Vec<Diag> {
    for d in errs.iter_mut() {
        if d.location.is_none() {
            d.location = Some(path.to_string());
        }
    }
    errs
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Build a finalized manifest for a non-warm build. Callers pass the native
/// compile duration (0.0 when the caller decides to skip native compilation,
/// which happens only in tests or when only `cpp` was needed).
pub fn final_manifest(
    plan: &Plan,
    native_ms: f64,
    compiled: bool,
) -> BuildManifest {
    let cpp = plan.cpp_path.to_string_lossy().into_owned();
    let binary = plan.bin_path.to_string_lossy().into_owned();
    let mut counts = crate::manifest::CacheCounts::default();
    for o in &plan.outcomes {
        match o.outcome {
            ParseOutcome::Hit => counts.hits += 1,
            ParseOutcome::Parsed => counts.misses += 1,
        }
    }
    if compiled {
        counts.compiled += 1;
    } else {
        counts.skipped += 1;
    }
    let total_ms = plan.timings.discover_ms
        + plan.timings.parse_ms
        + plan.timings.merge_ms
        + plan.timings.optimize_ms
        + plan.timings.typecheck_ms
        + plan.timings.codegen_ms
        + native_ms;
    let mut modules = Vec::new();
    for (i, &id) in plan.graph.order.iter().enumerate() {
        let n = &plan.graph.nodes[id];
        let o = &plan.outcomes[i];
        modules.push(crate::manifest::ModuleEntry {
            rel: n.rel.clone(),
            source_sha: n.source_sha256.clone(),
            deps: n.deps.iter().map(|d| plan.graph.rel_of(*d).to_string()).collect(),
            status: if o.outcome == ParseOutcome::Hit {
                ModuleStatus::Hit
            } else {
                ModuleStatus::Miss
            },
            items_bytes: o.items_bytes,
        });
    }
    BuildManifest {
        created_ms: now_ms(),
        env_fp: plan.env_fp.clone(),
        merged_hash: plan.merged_hash.clone(),
        root: plan.graph.rel_of(plan.graph.root).to_string(),
        flags: plan.flags.clone(),
        jobs: plan.jobs,
        cpp,
        binary,
        native_skipped: !compiled,
        cache: counts,
        timings: Timings {
            discover_ms: plan.timings.discover_ms,
            parse_ms: plan.timings.parse_ms,
            merge_ms: plan.timings.merge_ms,
            optimize_ms: plan.timings.optimize_ms,
            typecheck_ms: plan.timings.typecheck_ms,
            codegen_ms: plan.timings.codegen_ms,
            native_ms,
            total_ms,
        },
        modules,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        dir: PathBuf,
        bin: PathBuf,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Fixture {
            let dir = std::env::temp_dir().join(format!(
                "hs-build-test-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
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
            Fixture { bin: dir.join(".hard/main"), dir }
        }
        fn main(&self) -> PathBuf {
            self.dir.join("main.hard")
        }
    }

    use std::time::SystemTime;

    fn opts() -> BuildOptions {
        BuildOptions {
            compiler: "0.1.0".to_string(),
            runtime_sha: "rt".to_string(),
            platform: platform(),
            flags: vec!["-O2".to_string()],
            jobs: 1,
            release: false,
        }
    }

    #[test]
    fn cold_single_module_matches_compile_to_cpp() {
        let f = Fixture::new(&[(
            "main.hard",
            "bring http\napp @3031\n\nGET \"/\" :: { <- { hello: \"world\" } }\n",
        )]);
        let p = plan(&f.main(), &opts(), false).unwrap();
        let cpp = p.cpp.unwrap();
        let expected =
            crate::compile_to_cpp(&fs::read_to_string(f.main()).unwrap(), f.main().to_str().unwrap())
                .unwrap();
        assert_eq!(cpp, expected, "cold build must be byte-identical to the whole-program pipeline");
        assert!(!p.warm_eligible);
    }

    #[test]
    fn warm_path_skips_frontend_and_is_deterministic() {
        let f = Fixture::new(&[("main.hard", "GET \"/\" :: { <- 1 }\n")]);
        fs::create_dir_all(f.dir.join(".hard")).unwrap();
        fs::write(&f.dir.join(".hard/main.cpp"), "cpp").unwrap();
        fs::write(&f.bin, "bin").unwrap();
        let p1 = plan(&f.main(), &opts(), false).unwrap();
        let cpp1 = p1.cpp.as_ref().unwrap().clone();
        let m1 = final_manifest(&p1, 10.0, true);
        m1.save(&f.dir).unwrap();

        let p2 = plan(&f.main(), &opts(), true).unwrap();
        assert!(p2.warm_eligible, "second build should warm-hit");
        assert!(p2.cpp.is_none());
        // regenerate whatever the caller would have produced -> still equal
        let p3 = plan(&f.main(), &opts(), false).unwrap();
        assert_eq!(p3.cpp.unwrap(), cpp1, "deterministic across runs");
    }

    #[test]
    fn changed_module_reparses_only_that_module() {
        let f = Fixture::new(&[
            ("main.hard", "bring \"./util\"\nGET \"/u\" :: { <- util_val() }\n"),
            ("util.hard", "calc util_val() => Int { <- 1 }\n"),
        ]);
        let p1 = plan(&f.main(), &opts(), false).unwrap();
        for o in &p1.outcomes {
            assert_eq!(o.outcome, ParseOutcome::Parsed);
        }
        let cpp1 = p1.cpp.unwrap().clone();
        // record the parse-cache by re-planning (cache is now populated)
        let p2 = plan(&f.main(), &opts(), false).unwrap();
        assert_eq!(p2.cpp.unwrap(), cpp1);
        for o in &p2.outcomes {
            assert_eq!(o.outcome, ParseOutcome::Hit, "unchanged modules hit the parse cache");
        }
        // modify one module -> only that module reparses
        fs::write(f.dir.join("util.hard"), "calc util_val() => Int { <- 2 }\n").unwrap();
        let p3 = plan(&f.main(), &opts(), false).unwrap();
        let hit = p3
            .outcomes
            .iter()
            .filter(|o| o.outcome == ParseOutcome::Hit)
            .count();
        let miss = p3
            .outcomes
            .iter()
            .filter(|o| o.outcome == ParseOutcome::Parsed)
            .count();
        assert_eq!(hit, 1, "main.hard unchanged -> parse cache hit");
        assert_eq!(miss, 1, "util.hard changed -> reparse");
        assert_ne!(p3.cpp.unwrap(), cpp1, "cpp reflects the change");
    }

    #[test]
    fn imports_are_merged_deps_first_and_bring_imports_dropped() {
        let f = Fixture::new(&[
            ("main.hard", "bring \"./a\"\nbring \"./b\"\nGET \"/\" :: { <- { a: av(), b: bv() } }\n"),
            ("a.hard", "calc av() => Int { <- 1 }\n"),
            ("b.hard", "bring \"./a\"\ncalc bv() => Int { <- 2 }\n"),
        ]);
        let p = plan(&f.main(), &opts(), false).unwrap();
        // graph order: a only (once), b, main? -> a, b, main
        let rels: Vec<&str> = p.graph.order.iter().map(|&id| p.graph.rel_of(id)).collect();
        assert_eq!(rels[0], "a.hard");
        assert_eq!(rels[1], "b.hard");
        assert_eq!(rels[2], "main.hard");
        // main depends on both a and b
        assert_eq!(p.graph.nodes[p.graph.root].deps.len(), 2);
        let cpp = p.cpp.unwrap();
        assert!(!cpp.contains("Import"), "import stmts never reach codegen");
        assert!(cpp.contains("av") && cpp.contains("bv"));
    }

    #[test]
    fn missing_module_surfaces_module_diagnostic() {
        let f = Fixture::new(&[("main.hard", "bring \"./nope\"\n")]);
        let err = plan(&f.main(), &opts(), false).unwrap_err();
        let text = crate::error::render_all(&err);
        assert!(text.contains("not found"), "got: {text}");
    }

    #[test]
    fn empty_module_round_trip_cache() {
        let f = Fixture::new(&[("main.hard", "bring http\nGET \"/\" :: { <- \"ok\" }\n")]);
        let p1 = plan(&f.main(), &opts(), false).unwrap();
        let p2 = plan(&f.main(), &opts(), false).unwrap();
        assert_eq!(p1.cpp.unwrap(), p2.cpp.unwrap());
    }

    #[test]
    fn parallel_and_serial_are_byte_identical() {
        let mut files: Vec<(String, String)> = Vec::new();
        for i in 0..40 {
            files.push((
                format!("m{i:02}.hard"),
                format!("calc v{i:02}() => Int {{ <- {i} }}\n"),
            ));
        }
        let mut imports = String::new();
        for i in 0..40 {
            imports.push_str(&format!("bring \"./m{i:02}\"\n"));
        }
        files.push(("main.hard".to_string(), format!("{imports}GET \"/\" :: {{ <- {{ n: v00() }} }}\n")));
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let f = Fixture::new(&refs);
        let mut serial = opts();
        serial.jobs = 1;
        let p1 = plan(&f.main(), &serial, false).unwrap();
        let mut parallel = opts();
        parallel.jobs = 8;
        let p2 = plan(&f.main(), &parallel, false).unwrap();
        assert_eq!(p1.cpp.unwrap(), p2.cpp.unwrap(), "parallelism must not change output");
        let rels1: Vec<&str> = p1.outcomes.iter().map(|o| o.rel.as_str()).collect();
        let rels2: Vec<&str> = p2.outcomes.iter().map(|o| o.rel.as_str()).collect();
        assert_eq!(rels1, rels2, "outcome order is deterministic (topological)");
        let sha1 = p1.merged_hash;
        let sha2 = p2.merged_hash;
        assert_eq!(sha1, sha2);
    }

    #[test]
    fn parallel_rebuild_counts_agree_via_manifest() {
        let mut files: Vec<(String, String)> = Vec::new();
        for i in 0..16 {
            files.push((
                format!("m{i:02}.hard"),
                format!("calc v{i:02}() => Int {{ <- {i} }}\n"),
            ));
        }
        let mut imports = String::new();
        for i in 0..16 {
            imports.push_str(&format!("bring \"./m{i:02}\"\n"));
        }
        files.push(("main.hard".to_string(), format!("{imports}GET \"/\" :: {{ <- {{ n: v00() }} }}\n")));
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let f = Fixture::new(&refs);
        let mut o = opts();
        o.jobs = 4;
        let p1 = plan(&f.main(), &o, false).unwrap();
        assert_eq!(p1.outcomes.len(), 17, "all modules present");
        assert!(p1.outcomes.iter().all(|m| m.outcome == ParseOutcome::Parsed));
        let m1 = final_manifest(&p1, 0.0, true);
        m1.save(&f.dir).unwrap();
        fs::write(f.dir.join("m07.hard"), "calc v07() => Int { <- 99 }\n").unwrap();
        let p2 = plan(&f.main(), &o, false).unwrap();
        let hits = p2.outcomes.iter().filter(|m| m.outcome == ParseOutcome::Hit).count();
        let misses = p2.outcomes.iter().filter(|m| m.outcome == ParseOutcome::Parsed).count();
        assert_eq!(hits, 16);
        assert_eq!(misses, 1);
    }
}