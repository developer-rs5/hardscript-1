use hs_compiler::{compile_to_cpp, fmt, frontend, Diag};
use hs_pm::cache::Cache;
use hs_pm::install::{self, InstallConfig};
use hs_pm::manifest::{default_manifest, Manifest};
use hs_pm::templates;
use hs_pm::workspace::Workspace;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = env!("CARGO_PKG_VERSION");

// The runtime headers are embedded into the CLI binary at build time so
// `hard` works without a checkout of the compiler repository. Each part is
// written next to `hs_runtime.hpp` in the build directory; the umbrella header
// includes them by name.
const RUNTIME_FILES: &[(&str, &str)] = &[
    (
        "hs_runtime_value.hpp",
        include_str!("../../runtime/hs_runtime_value.hpp"),
    ),
    (
        "hs_runtime_io.hpp",
        include_str!("../../runtime/hs_runtime_io.hpp"),
    ),
    (
        "hs_runtime_crypto.hpp",
        include_str!("../../runtime/hs_runtime_crypto.hpp"),
    ),
    (
        "hs_runtime_arena.hpp",
        include_str!("../../runtime/hs_runtime_arena.hpp"),
    ),
    (
        "hs_runtime_http.hpp",
        include_str!("../../runtime/hs_runtime_http.hpp"),
    ),
    (
        "hs_runtime_sched.hpp",
        include_str!("../../runtime/hs_runtime_sched.hpp"),
    ),
    (
        "hs_runtime_postgres.hpp",
        include_str!("../../runtime/hs_runtime_postgres.hpp"),
    ),
    (
        "hs_runtime_sqlite.hpp",
        include_str!("../../runtime/hs_runtime_sqlite.hpp"),
    ),
    (
        "hs_runtime_validation.hpp",
        include_str!("../../runtime/hs_runtime_validation.hpp"),
    ),
    (
        "hs_runtime_auth.hpp",
        include_str!("../../runtime/hs_runtime_auth.hpp"),
    ),
    (
        "hs_runtime_orm.hpp",
        include_str!("../../runtime/hs_runtime_orm.hpp"),
    ),
    (
        "hs_runtime_util.hpp",
        include_str!("../../runtime/hs_runtime_util.hpp"),
    ),
    ("hs_runtime.hpp", include_str!("../../runtime/hs_runtime.hpp")),
];

fn write_runtime(dir: &Path) {
    for (name, content) in RUNTIME_FILES {
        write(&dir.join(name), content);
    }
}

fn runtime_bytes() -> usize {
    RUNTIME_FILES.iter().map(|(_, c)| c.len()).sum()
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        help();
        return;
    }
    let (cmd, rest) = args.split_first().unwrap();
    match cmd.as_str() {
        "new" => cmd_new(rest),
        "init" => cmd_init(rest),
        "build" => cmd_build(rest),
        "run" => cmd_run(rest),
        "test" => cmd_test(rest),
        "fmt" => cmd_fmt(rest),
        "docs" => cmd_docs(rest),
        "doctor" => cmd_doctor(rest),
        "add" => cmd_add(rest),
        "remove" | "rm" => cmd_remove(rest),
        "install" => cmd_install(rest),
        "update" => cmd_update(rest),
        "list" => cmd_list(rest),
        "outdated" => cmd_outdated(rest),
        "search" => cmd_search(rest),
        "info" => cmd_info(rest),
        "cache" => cmd_cache(rest),
        "workspace" => cmd_workspace(rest),
        "report" => cmd_report(rest),
        "bench" => cmd_bench(rest),
        "hir" => cmd_hir(rest),
        "opt" => cmd_opt(rest),
        "errors" => cmd_errors(rest),
        "--version" | "-V" => println!("hard {VERSION}"),
        "--help" | "-h" | "help" => help(),
        other => {
            eprintln!("hard: unknown command '{other}'");
            eprintln!("run `hard help` for usage.");
            std::process::exit(2);
        }
    }
}

fn help() {
    println!(
        "hard {VERSION} — the HardScript compiler & toolchain\n\
         \n\
         USAGE:\n\
         \x20 hard new <name>              Create a project (or named template)\n\
         \x20 hard init                    Create a manifest in the current dir\n\
         \x20 hard build [file]            Compile to a native executable\n\
         \x20 hard run   [file] [args..]   Build and run the server\n\
         \x20 hard test  [file]            Build and run the test suite\n\
         \x20 hard fmt   [file]            Reformat a source file in place\n\
         \x20 hard docs  [file]            Generate API.md for a source file\n\
         \x20 hard add <pkg>[@req]          Add a dependency and install it\n\
         \x20 hard remove <pkg>            Remove a dependency\n\
         \x20 hard install                 Resolve and lock all dependencies\n\
         \x20 hard update [pkg]            Update locked packages to newest match\n\
         \x20 hard list                    Show installed packages\n\
         \x20 hard outdated                Show packages with newer releases\n\
         \x20 hard search <q>              Search the registry\n\
         \x20 hard info <pkg>              Show package metadata\n\
         \x20 hard cache <info|verify|clean>  Inspect the shared package cache\n\
         \x20 hard workspace <list|build|test>  Run across workspace members\n\
         \x20 hard report                  Write ecosystem reports to reports/\n\
         \x20 hard doctor                  Check the toolchain (g++, runtime)\n\
         \x20 hard bench [file]            Release-build and report timings\n\
         \x20 hard hir   [file]            Print the lowered HIR (debugging)\n\
         \x20 hard opt   [file]            Optimize and show before/after (debugging)\n\
         \x20 hard errors                   List the diagnostic catalog (--markdown)\n\
         \x20 hard help                    Show this help\n\
         \n\
         Files default to main.hard in the current directory.\n\
         \n\
         PM: hard.toml declares [dependencies]; hard.lock pins the resolution;\n\
         cache lives in $HARD_HOME/cache (default ~/.hard/cache)."
    );
}

fn find_target(rest: &[String]) -> (PathBuf, Vec<String>) {
    // first argument ending in .hard is the target; the rest are passed on.
    if let Some((_, t)) = rest.iter().find(|a| a.ends_with(".hard")).map(|a| (0, a.clone())) {
        let mut rem = rest.to_vec();
        rem.retain(|a| a != &t);
        (PathBuf::from(t), rem)
    } else {
        (PathBuf::from("main.hard"), rest.to_vec())
    }
}

fn cmd_new(args: &[String]) {
    let name = match args.first() {
        Some(n) => n.trim().to_string(),
        None => {
            eprintln!("hard new: missing project name");
            std::process::exit(2);
        }
    };
    if name.is_empty() || name == "." || name == ".." {
        eprintln!("hard new: invalid project name '{name}'");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&name);
    if dir.exists() {
        eprintln!("hard new: '{name}' already exists");
        std::process::exit(2);
    }

    // A name that matches an official template scaffolds the template.
    if let Some(tpl) = templates::find(&name) {
        match templates::scaffold(&tpl, &name, &dir) {
            Ok(()) => {
                println!("Created {name}/ from template '{name}'");
                println!("\nNext:\n  cd {name}\n  hard run");
                return;
            }
            Err(e) => die(&e),
        }
    }

    std::fs::create_dir_all(dir.join("runtime")).unwrap_or_else(|e| die(&e.to_string()));
    let main = format!(
        "bring http\n\
         \n\
         app @3000\n\
         \n\
         GET \"/\" :: {{\n\
         \x20   <- {{ hello: \"{name}\" }}\n\
         }}\n\
         \n\
         test \"hello\" {{\n\
         \x20   res <- GET \"/\"\n\
         \x20   expect res.status == 200\n\
         }}\n\
         \n"
    );
    write(&dir.join("main.hard"), &main);
    let toml = format!(
        "name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         description = \"A HardScript application\"\n\
         \n\
         [modules]\n"
    );
    write(&dir.join("hard.toml"), &toml);
    write_runtime(&dir.join("runtime"));
    write(&dir.join(".gitignore"), ".hard/\n*.o\n");
    println!("Created {name}/");
    println!("\nNext:\n  cd {name}\n  hard run");
}

/// `hard init` — scaffold a manifest (and default source) into the current
/// directory without creating a subdirectory.
fn cmd_init(args: &[String]) {
    if args.iter().any(|a| a == "--force") && !Path::new("hard.toml").exists() {
        // nothing to force; fall through
    }
    if Path::new("hard.toml").exists() {
        eprintln!("hard init: hard.toml already exists (use --force to overwrite)");
        std::process::exit(2);
    }
    let dir_name = env::current_dir()
        .ok()
        .and_then(|d| d.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "app".to_string());
    let manifest = default_manifest(&dir_name);
    write(Path::new("hard.toml"), &manifest.render());
    if !Path::new("main.hard").exists() {
        write(
            Path::new("main.hard"),
            "bring http\n\napp @3000\n\nGET \"/\" :: {\n    <- { hello: \"world\" }\n}\n\ntest \"hello\" {\n    res <- GET \"/\"\n    expect res.status == 200\n}\n\n",
        );
    }
    println!("initialized {} with hard.toml", dir_name);
}

fn cmd_build(rest: &[String]) {
    let (jobs, rest) = jobs_arg(rest);
    let (policy, rest) = match warning_policy_arg(&rest) {
        Ok(p) => p,
        Err(e) => die(&e),
    };
    let (target, _) = find_target(&rest);
    if std::env::var("HARD_ESCAPE_REPORT").is_ok() {
        match report_escape(target.as_path()) {
            Ok(lines) => {
                for l in lines {
                    println!("{l}");
                }
            }
            Err(diags) => report(&diags),
        }
    } else {
        let opts = build_options(false, jobs);
        match incremental_build(&target, &opts, &policy) {
            Ok(lines) => {
                for l in lines {
                    println!("{l}");
                }
            }
            Err(diags) => report(&diags),
        }
    }
}

/// Extract `--warnings <spec>` / `--deny <spec>` (and `=` forms) from the
/// argument list. Last value wins for each flag. Returns the policy plus the
/// remaining arguments.
fn warning_policy_arg(rest: &[String]) -> Result<(hs_compiler::warn::WarningPolicy, Vec<String>), String> {
    let mut enabled = None;
    let mut deny = None;
    let mut out = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        if a == "--warnings" || a == "--deny" {
            let flag = a.as_str();
            let val = rest.get(i + 1).ok_or_else(|| format!("{flag} needs a value"))?;
            if flag == "--warnings" {
                enabled = Some(val.clone());
            } else {
                deny = Some(val.clone());
            }
            i += 2;
            continue;
        }
        if let Some(v) = a.strip_prefix("--warnings=") {
            enabled = Some(v.to_string());
            i += 1;
            continue;
        }
        if let Some(v) = a.strip_prefix("--deny=") {
            deny = Some(v.to_string());
            i += 1;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    let policy = hs_compiler::warn::parse_policy(enabled.as_deref(), deny.as_deref())?;
    Ok((policy, out))
}

/// Extract `-j N` / `--jobs N` from the argument list (parallel front-end
/// workers; unlimited on the native stage which stays single, whole-program).
fn jobs_arg(rest: &[String]) -> (usize, Vec<String>) {
    let mut jobs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut out = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        if a == "-j" || a == "--jobs" {
            if let Some(v) = rest.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                jobs = v.max(1);
                i += 2;
                continue;
            }
        }
        if let Some(v) = a.strip_prefix("--jobs=") {
            if let Ok(n) = v.parse::<usize>() {
                jobs = n.max(1);
                i += 1;
                continue;
            }
        }
        out.push(a.clone());
        i += 1;
    }
    (jobs, out)
}

/// Environment stamp inputs shared by the incremental pipeline and `doctor`.
fn runtime_fingerprint() -> String {
    let mut acc = String::new();
    for (name, content) in RUNTIME_FILES {
        acc.push_str(name);
        acc.push('\n');
        acc.push_str(content);
        acc.push('\n');
    }
    hs_compiler::sha256::hex(acc.as_bytes())
}

fn build_options(release: bool, jobs: usize) -> hs_compiler::build::BuildOptions {
    let flags = if release {
        release_flags()
    } else {
        vec!["-O2".to_string()]
    };
    hs_compiler::build::BuildOptions {
        compiler: VERSION.to_string(),
        runtime_sha: runtime_fingerprint(),
        platform: hs_compiler::build::platform(),
        flags,
        jobs,
        release,
    }
}

/// Incremental compile: staged pipeline → g++ → `.hard/build.json`.
///
/// Returns the deterministic `built <path>` line plus a cache summary line
/// (stage timings only when `HARD_TIMED` is set, so summary output stays
/// deterministic).
fn incremental_build(
    target: &Path,
    opts: &hs_compiler::build::BuildOptions,
    policy: &hs_compiler::warn::WarningPolicy,
) -> Result<Vec<String>, Vec<Diag>> {
    let pp = PathBuf::from(target);
    let mut plan = hs_compiler::build::plan(&pp, opts, true)?;

    // Static warnings (M3.4.4): printed to stderr so the deterministic
    // stdout build summary is untouched. `--deny` escalates to a hard fail.
    let warnings = std::mem::take(&mut plan.warnings);
    let (shown, promoted) = policy.filter(warnings);
    if !promoted.is_empty() {
        // The promoted set is a subset of `shown`; the `Err` path re-renders
        // it with its hard-error label, so don't double-print here.
        return Err(promoted);
    }
    if !shown.is_empty() {
        let color = hs_compiler::diagnostics::detect_color(hs_compiler::diagnostics::ColorMode::Auto);
        eprint!("{}", hs_compiler::diagnostics::render_v2(&shown, color));
    }

    let mut lines = vec![format!("built {}", plan.bin_path.display())];

    if plan.warm_eligible {
        let n = plan.graph.order.len();
        if std::env::var("HARD_TIMED").is_ok() {
            lines.push(format!(
                "cache: {n} hit, 0 miss, {n} skipped, 0 compiled (warm)"
            ));
        } else {
            lines.push(format!("cache: {n} hit, 0 miss, {n} skipped, 0 compiled"));
        }
        return Ok(lines);
    }

    let cpp = plan.cpp.clone().unwrap();
    let build_dir = plan.project_root.join(".hard");
    std::fs::create_dir_all(&build_dir).unwrap_or_else(|e| die(&e.to_string()));
    write_runtime(&build_dir);
    write(&plan.cpp_path, &cpp);

    let t_native = std::time::Instant::now();
    let mut cmd = std::process::Command::new("g++");
    cmd.arg("-std=c++17").arg("-pthread").arg("-I").arg(&build_dir);
    for f in &opts.flags {
        cmd.arg(f);
    }
    cmd.arg(&plan.cpp_path).arg("-o").arg(&plan.bin_path);
    let out = cmd
        .output()
        .unwrap_or_else(|e| die(&format!("could not run g++: {e}")));
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(vec![Diag::new_nospan(
            hs_compiler::ErrorKind::Codegen,
            format!("g++ failed:\n{msg}"),
        )
        .with_code(hs_compiler::catalog::NATIVE_COMPILE_FAILED)]);
    }
    let native_ms = t_native.elapsed().as_secs_f64() * 1000.0;

    let m = hs_compiler::build::final_manifest(&plan, native_ms, true);
    m.save(&plan.project_root)
        .unwrap_or_else(|e| die(&e.to_string()));

    let c = &m.cache;
    lines.push(format!(
        "cache: {} hit, {} miss, {} skipped, {} compiled",
        c.hits, c.misses, c.skipped, c.compiled
    ));
    if std::env::var("HARD_TIMED").is_ok() {
        let d = m.timings.discover_ms;
        let p = m.timings.parse_ms;
        let me = m.timings.merge_ms;
        let o = m.timings.optimize_ms;
        let tc = m.timings.typecheck_ms;
        let cg = m.timings.codegen_ms;
        let na = m.timings.native_ms;
        let to = m.timings.total_ms;
        lines.push(format!(
            "time: discover={d:.1}ms parse={p:.1}ms merge={me:.1}ms opt={o:.1}ms typecheck={tc:.1}ms codegen={cg:.1}ms native={na:.1}ms total={to:.1}ms"
        ));
    }
    Ok(lines)
}

/// Escape-analysis report mode (`HARD_ESCAPE_REPORT=1`): prints one line per
/// classified local binding plus a summary, instead of the build banner.
/// The report is derived from the same pipeline as the generated C++ but
/// never alters it.
fn report_escape(target: &Path) -> Result<Vec<String>, Vec<Diag>> {
    let src = match std::fs::read_to_string(target) {
        Ok(s) => s,
        Err(e) => {
            return Err(vec![Diag::new_nospan(
                hs_compiler::ErrorKind::Codegen,
                format!("cannot read {}: {e}", target.display()),
            )
            .with_code(hs_compiler::catalog::IO_ERROR)])
        }
    };
    let rep = hs_compiler::escape_report(&src, target.to_str().unwrap_or("").to_string())?;
    Ok(rep.to_lines())
}

fn cmd_run(rest: &[String]) {
    let (target, passthrough) = find_target(rest);
    let (_, bin) = match compile(target.as_path()) {
        Ok(p) => p,
        Err(diags) => {
            report(&diags);
            return;
        }
    };
    let code = exec(&bin, &passthrough);
    std::process::exit(code);
}

fn cmd_test(rest: &[String]) {
    let (target, passthrough) = find_target(rest);
    let (_, bin) = match compile(target.as_path()) {
        Ok(p) => p,
        Err(diags) => {
            report(&diags);
            return;
        }
    };
    let mut all = passthrough.clone();
    all.push("--test".to_string());
    let code = exec(&bin, &all);
    std::process::exit(code);
}

fn cmd_fmt(rest: &[String]) {
    let check = rest.iter().any(|a| a == "--check");
    let (target, _) = find_target(rest);
    let src = match std::fs::read_to_string(&target) {
        Ok(s) => s,
        Err(e) => die(&format!("cannot read {}: {e}", target.display())),
    };
    match frontend(&src, target.to_str().unwrap_or("").to_string()) {
        Ok(prog) => {
            let out = fmt::format(&prog);
            if check {
                if out == src {
                    println!("{} is formatted correctly", target.display());
                } else {
                    eprintln!("{} is not formatted (run `hard fmt`)", target.display());
                    std::process::exit(1);
                }
            } else {
                std::fs::write(&target, out).unwrap_or_else(|_| die("cannot write formatted file"));
                println!("formatted {}", target.display());
            }
        }
        Err(diags) => report(&diags),
    }
}

fn cmd_docs(rest: &[String]) {
    let (target, _) = find_target(rest);
    let src = match std::fs::read_to_string(&target) {
        Ok(s) => s,
        Err(e) => die(&format!("cannot read {}: {e}", target.display())),
    };
    match frontend(&src, target.to_str().unwrap_or("").to_string()) {
        Ok(prog) => {
            let md = hs_compiler::docs::render_markdown(&prog);
            write(&PathBuf::from("API.md"), &md);
            println!("wrote API.md");
        }
        Err(diags) => report(&diags),
    }
}

fn cmd_doctor(args: &[String]) {
    let graph_flag = args.iter().any(|a| a == "--graph");
    let deps_flag = args.iter().any(|a| a == "--deps");

    if graph_flag || deps_flag {
        let (target, _) = find_target(args);
        if !target.exists() {
            die(&format!("cannot read {}: {:?} (run `hard doctor` without --graph/--deps in a project?)", target.display(), target.exists()));
        }
        match hs_compiler::graph::discover(&target) {
            Ok(g) => {
                println!("{}", if graph_flag { g.to_json() } else { g.to_deps() });
            }
            Err(diags) => report(&diags),
        }
        return;
    }

    let mut ok = true;
    println!("hard doctor — {VERSION}");
    for tool in ["g++", "gcc", "make"] {
        match Command::new(tool).arg("--version").output() {
            Ok(o) => {
                let first = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or(tool)
                    .to_string();
                println!("  ok {tool}: {first}");
            }
            Err(_) => {
                println!("  missing {tool}");
                ok = false;
            }
        }
    }
    println!(
        "  runtime header: embedded ({} bytes)",
        runtime_bytes()
    );

    // Incremental-build diagnostics (only when this directory has built).
    let manifest = hs_compiler::manifest::BuildManifest::load(Path::new("."));
    let store = hs_compiler::cache::EntryStore::new(Path::new("."));
    let has_cache = store.count() > 0 || manifest.is_some();
    if has_cache {
        println!("  compiler version: {VERSION}");
        println!("  runtime version: {}", &runtime_fingerprint()[..16]);
        println!("  cache entries: {}", store.count());
        println!("  cache size: {}", human_bytes(store.size_bytes()));
        if let Some(m) = &manifest {
            println!("  cache hits: {}", m.cache.hits);
            println!("  cache misses: {}", m.cache.misses);
            println!("  cache skipped: {}", m.cache.skipped);
            println!("  cache compiled: {}", m.cache.compiled);
            if m.native_skipped {
                println!("  last build: warm (native skipped)");
            } else {
                println!("  last build: recompiled");
            }
        }
    }
    if !ok {
        std::process::exit(1);
    }
}

fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.2} MB", n as f64 / (1024.0 * 1024.0))
    }
}

fn cmd_add(args: &[String]) {
    let spec = match args.first() {
        Some(m) => m.clone(),
        None => {
            eprintln!("hard add: missing package name");
            std::process::exit(2);
        }
    };
    let (name, req) = install::parse_add_spec(&spec);
    if name.is_empty() {
        eprintln!("hard add: missing package name");
        std::process::exit(2);
    }
    let req = req.unwrap_or_else(|| "^1.0".to_string());

    let project_root = PathBuf::from(".");
    let (mut manifest, created) = load_manifest_or_default(&project_root);
    if created {
        write(&project_root.join("hard.toml"), &manifest.render());
    }
    manifest.dependencies.insert(name.clone(), req.clone());
    write(&project_root.join("hard.toml"), &manifest.render());

    let cfg = base_config_for(manifest, project_root.clone());
    let report = install::install(&cfg);
    println!("added {name} = \"{req}\" to hard.toml");
    if report.succeeded() {
        for (pname, pver) in &report.fetched {
            println!("  installed {pname}@{pver}");
        }
        for (pname, pver) in &report.reused {
            println!("  reused {pname}@{pver} (cached)");
        }
        if report.lock_written {
            println!("  wrote {}", report.lock_path.display());
        }
        println!("  graph:");
        print!("{}", report.graph);
    } else {
        eprintln!("warning: dependency recorded but not installed:");
        for e in &report.errors {
            eprintln!("  {e}");
        }
    }
}

fn cmd_remove(args: &[String]) {
    let name = match args.first() {
        Some(m) => m.clone(),
        None => {
            eprintln!("hard remove: missing package name");
            std::process::exit(2);
        }
    };
    let project_root = PathBuf::from(".");
    let (mut manifest, created) = load_manifest_or_default(&project_root);
    if created {
        write(&project_root.join("hard.toml"), &manifest.render());
    }
    let had = manifest.dependencies.remove(&name).is_some()
        || manifest.dev_dependencies.remove(&name).is_some();
    write(&project_root.join("hard.toml"), &manifest.render());
    if had {
        println!("removed {name}");
        let cfg = base_config_for(manifest, project_root.clone());
        let report = install::install(&cfg);
        if report.lock_written {
            println!("  re-locked {}", report.lock_path.display());
        }
        for e in &report.errors {
            eprintln!("  {e}");
        }
    } else {
        println!("{name} was not a dependency");
    }
}

fn cmd_install(args: &[String]) {
    let offline = args.iter().any(|a| a == "--offline" || a == "-o");
    let frozen = args.iter().any(|a| a == "--frozen" || a == "-F");
    let project_root = PathBuf::from(".");
    let (manifest, created) = load_manifest_or_default(&project_root);
    if created {
        write(&project_root.join("hard.toml"), &manifest.render());
        println!("wrote hard.toml");
    }
    let mut cfg = base_config_for(manifest, project_root);
    cfg.offline = offline;
    cfg.frozen = frozen;
    let report = install::install(&cfg);
    if report.succeeded() {
        for (pname, pver) in &report.fetched {
            println!("installed {pname}@{pver}");
        }
        for (pname, pver) in &report.reused {
            println!("reused {pname}@{pver} (cached)");
        }
        if report.lock_written {
            println!("locked -> {}", report.lock_path.display());
        }
        print!("{}", report.graph);
    } else {
        for e in &report.errors {
            eprintln!("error: {e}");
        }
        std::process::exit(1);
    }
}

fn cmd_update(_args: &[String]) {
    let project_root = PathBuf::from(".");
    let (manifest, created) = load_manifest_or_default(&project_root);
    if created {
        write(&project_root.join("hard.toml"), &manifest.render());
    }
    let cfg = base_config_for(manifest, project_root);
    let report = install::install(&cfg);
    if report.succeeded() {
        println!("updated to newest matching versions");
        print!("{}", report.graph);
    } else {
        for e in &report.errors {
            eprintln!("error: {e}");
        }
        std::process::exit(1);
    }
}

fn cmd_list(_args: &[String]) {
    let project_root = PathBuf::from(".");
    let (manifest, created) = load_manifest_or_default(&project_root);
    if created {
        write(&project_root.join("hard.toml"), &manifest.render());
    }
    let cfg = base_config_for(manifest, project_root);
    for l in install::list_out(cfg) {
        println!("{l}");
    }
}

fn cmd_outdated(args: &[String]) {
    let offline = args.iter().any(|a| a == "--offline" || a == "-o");
    let project_root = PathBuf::from(".");
    let (manifest, created) = load_manifest_or_default(&project_root);
    if created {
        write(&project_root.join("hard.toml"), &manifest.render());
    }
    let mut cfg = base_config_for(manifest, project_root);
    cfg.offline = offline;
    for l in install::outdated(cfg) {
        println!("{l}");
    }
}

fn cmd_search(args: &[String]) {
    let query = match args.first() {
        Some(q) => q.clone(),
        None => {
            eprintln!("hard search: missing query");
            std::process::exit(2);
        }
    };
    let registry = hs_pm::registry::Registry::new(hs_pm::registry::RegistryConfig::resolve(None, false));
    match registry.search(&query) {
        Ok(results) => {
            if results.is_empty() {
                println!("no packages matching '{query}'");
                return;
            }
            for r in results {
                let version = r
                    .version
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string());
                match &r.description {
                    Some(d) => println!("{} {version}  {d}", r.name),
                    None => println!("{} {version}", r.name),
                }
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_info(args: &[String]) {
    let name = match args.first() {
        Some(n) => n.clone(),
        None => {
            eprintln!("hard info: missing package name");
            std::process::exit(2);
        }
    };
    let registry = hs_pm::registry::Registry::new(hs_pm::registry::RegistryConfig::resolve(None, false));
    match registry.metadata(&name) {
        Ok(meta) => {
            println!("{name}");
            for v in &meta.versions {
                let deps: Vec<String> = v
                    .dependencies
                    .iter()
                    .map(|(k, r)| format!("{k}@{r}"))
                    .collect();
                let deps = if deps.is_empty() {
                    String::from("(no dependencies)")
                } else {
                    deps.join(", ")
                };
                match &v.description {
                    Some(d) => println!("  {}  {d}  deps: {deps}", v.version),
                    None => println!("  {}  deps: {deps}", v.version),
                }
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_cache(args: &[String]) {
    let sub = args.first().map(String::as_str).unwrap_or("info");
    match sub {
        "info" | "stats" => {
            let cache = Cache::new();
            let (count, bytes) = cache.stats();
            println!("cache root: {}", cache.root.display());
            println!("packages: {count}");
            println!("size: {}", human_bytes(bytes));
        }
        "verify" => {
            let cache = Cache::new();
            let bad = cache.verify_all();
            if bad.is_empty() {
                println!("cache ok ({} packages verified)", count_packages(&cache));
            } else {
                for e in &bad {
                    eprintln!("corrupt: {e}");
                }
                std::process::exit(1);
            }
        }
        "clean" => {
            let cache = Cache::new();
            match cache.clean() {
                Ok(()) => println!("cache cleaned"),
                Err(e) => die(&format!("cache clean failed: {e}")),
            }
        }
        other => {
            eprintln!("hard cache: unknown subcommand '{other}' (use info|verify|clean)");
            std::process::exit(2);
        }
    }
}

fn count_packages(cache: &Cache) -> usize {
    let (n, _) = cache.stats();
    n
}

fn cmd_workspace(args: &[String]) {
    let verb = args.first().map(String::as_str).unwrap_or("list");
    let log = hs_pm::tui::Logger::new(false);
    let ws = match Workspace::discover(&PathBuf::from("."), &log) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    match verb {
        "list" => match &ws {
            Some(ws) => {
                for l in ws.list_lines() {
                    println!("{l}");
                }
            }
            None => {
                println!("not in a workspace");
            }
        },
        "test" | "build" | "run" => match &ws {
            Some(ws) => {
                println!("workspace {} ({} member{})", ws.root.display(), ws.len(), if ws.len() == 1 { "" } else { "s" });
                let log2 = hs_pm::tui::Logger::new(true);
                for l in ws.run_members(verb, &log2) {
                    println!("{l}");
                }
            }
            None => {
                eprintln!("not in a workspace");
                std::process::exit(1);
            }
        },
        other => {
            eprintln!("hard workspace: unknown subcommand '{other}' (use list|build|test)");
            std::process::exit(2);
        }
    }
}

fn cmd_report(args: &[String]) {
    let dir = args.first().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("reports"));
    let cache = Cache::new();
    match hs_pm::report::write_all(&cache, &dir) {
        Ok(written) => {
            println!("wrote {}/", dir.display());
            for r in &written {
                println!("  {r}");
            }
        }
        Err(e) => die(&e),
    }
}

/// Load `hard.toml` from `project_root`, or build a fresh default manifest.
/// Returns `(manifest, created)`.
fn load_manifest_or_default(project_root: &Path) -> (Manifest, bool) {
    let path = project_root.join("hard.toml");
    match Manifest::load(&path) {
        Ok(Some(m)) => (m, false),
        Ok(None) => {
            let dir_name = project_root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("app");
            (default_manifest(dir_name), true)
        }
        Err(errs) => {
            for e in &errs {
                eprintln!("error: {}: {}", e.key, e.message);
            }
            std::process::exit(1);
        }
    }
}

fn base_config_for(manifest: Manifest, project_root: PathBuf) -> InstallConfig {
    install::base_config(manifest, project_root, false, false, "hard", VERSION)
}

fn cmd_hir(rest: &[String]) {
    let (target, _) = find_target(rest);
    let src = match std::fs::read_to_string(&target) {
        Ok(s) => s,
        Err(e) => die(&format!("cannot read {}: {e}", target.display())),
    };
    match hs_compiler::hir_string(&src, target.to_str().unwrap_or("").to_string()) {
        Ok(hir) => print!("{hir}"),
        Err(diags) => report(&diags),
    }
}

fn cmd_opt(rest: &[String]) {
    let (target, _) = find_target(rest);
    let src = match std::fs::read_to_string(&target) {
        Ok(s) => s,
        Err(e) => die(&format!("cannot read {}: {e}", target.display())),
    };
    match hs_compiler::opt_string(&src, target.to_str().unwrap_or("").to_string()) {
        Ok(out) => print!("{out}"),
        Err(diags) => report(&diags),
    }
}

/// `hard errors` — list the diagnostic catalog. `hard errors --markdown`
/// emits the same catalog as a generated Markdown section for `docs/errors.md`
/// (stable output; regenerate instead of editing by hand).
fn cmd_errors(rest: &[String]) {
    let markdown = rest.iter().any(|a| a == "--markdown" || a == "-m");
    let cat = hs_compiler::catalog::catalog();
    if markdown {
        println!("<!-- Generated by `hard errors --markdown`. Do not edit by hand. -->\n");
        println!("The catalog is the source of truth for every compiler diagnostic in\nHardScript v2. Codes are grouped by reserved ranges:\n");
        for c in cat {
            println!("### `{code}` — {name}\n", code = hs_compiler::catalog::format(c.number), name = c.name);
            println!("{meaning}\n", meaning = c.meaning);
            println!("Example:\n\n```hardscript\n{}\n```\n", c.example);
            println!("Common causes:\n");
            for cause in c.causes {
                println!("- {cause}");
            }
            println!("\nFixes:\n");
            for fix in c.fixes {
                println!("- {fix}");
            }
            println!();
        }
        return;
    }
    println!("hard {VERSION} diagnostic catalog ({} codes):\n", cat.len());
    let mut prev_range = None;
    for c in cat {
        let range = c.number / 100;
        if prev_range != Some(range) {
            prev_range = Some(range);
            println!("  -- {range:02}x range --");
        }
        println!(
            "  {code}  {name:<30} {meaning}",
            code = hs_compiler::catalog::format(c.number),
            name = c.name,
            meaning = c.meaning
        );
    }
}

fn cmd_bench(rest: &[String]) {
    let (target, _) = find_target(rest);
    let start = std::time::Instant::now();
    let (build_dir, bin) = match compile_release(target.as_path()) {
        Ok(p) => p,
        Err(diags) => {
            report(&diags);
            std::process::exit(1);
        }
    };
    let meta = std::fs::metadata(&bin).unwrap();
    println!(
        "release build in {:.2}s, binary {} bytes -> {}",
        start.elapsed().as_secs_f64(),
        meta.len(),
        bin.display()
    );
    let _ = build_dir;
}

// ---------------------------------------------------------------------------
// compilation
// ---------------------------------------------------------------------------

fn compile(target: &Path) -> Result<(PathBuf, PathBuf), Vec<Diag>> {
    compile_impl(target, &["-O2"], false)
}

// MS1.7: release builds get -O3 plus link-time optimization, native CPU
// tuning and hidden visibility. All overridable via HS_CXXFLAGS (appended)
// and HS_NO_NATIVE=1 (drop -march=native for portability). The flags list is
// collected up front so build-report / bench numbers stay deterministic.
fn release_flags() -> Vec<String> {
    let mut f = vec!["-O3".to_string(), "-DNDEBUG".to_string(), "-flto".to_string()];
    if std::env::var("HS_NO_NATIVE").is_err() {
        f.push("-march=native".to_string());
        f.push("-mtune=native".to_string());
    }
    f.push("-fvisibility=hidden".to_string());
    if let Ok(extra) = std::env::var("HS_CXXFLAGS") {
        for part in extra.split_whitespace() {
            if !part.is_empty() {
                f.push(part.to_string());
            }
        }
    }
    f
}

fn compile_release(target: &Path) -> Result<(PathBuf, PathBuf), Vec<Diag>> {
    let flags = release_flags();
    let flag_refs: Vec<&str> = flags.iter().map(String::as_str).collect();
    compile_impl(target, &flag_refs, true)
}

fn compile_impl(
    target: &Path,
    flags: &[&str],
    release: bool,
) -> Result<(PathBuf, PathBuf), Vec<Diag>> {
    let src = match std::fs::read_to_string(target) {
        Ok(s) => s,
        Err(e) => return Err(vec![Diag::new_nospan(hs_compiler::ErrorKind::Codegen,
            format!("cannot read {}: {e}", target.display()))
            .with_code(hs_compiler::catalog::IO_ERROR)]),
    };
    let name = target
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("app")
        .to_string();

    let cpp = match compile_to_cpp(&src, target.to_str().unwrap_or("").to_string()) {
        Ok(c) => c,
        Err(diags) => return Err(diags),
    };

    let build_dir = PathBuf::from(".hard");
    std::fs::create_dir_all(&build_dir).unwrap_or_else(|e| die(&e.to_string()));
    write_runtime(&build_dir);
    write(&build_dir.join(format!("{name}.cpp")), &cpp);

    let bin = if release {
        build_dir.join(format!("{name}.release"))
    } else {
        build_dir.join(name.clone())
    };

    let mut cmd = Command::new("g++");
    cmd.arg("-std=c++17")
        .arg("-pthread")
        .arg("-I")
        .arg(&build_dir)
        .args(flags)
        .arg(build_dir.join(format!("{name}.cpp")))
        .arg("-o")
        .arg(&bin);
    let out = cmd.output().unwrap_or_else(|e| die(&format!("could not run g++: {e}")));
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(vec![Diag::new_nospan(
            hs_compiler::ErrorKind::Codegen,
            format!("g++ failed:\n{msg}"),
        )
        .with_code(hs_compiler::catalog::NATIVE_COMPILE_FAILED)]);
    }
    Ok((build_dir, bin))
}

fn exec(bin: &Path, args: &[String]) -> i32 {
    eprintln!("running {}", bin.display());
    let status = Command::new(bin).args(args).status();
    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("hard: cannot run {}: {e}", bin.display());
            1
        }
    }
}

fn write(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap_or_else(|e| die(&format!("cannot write {}: {e}", path.display())));
}

fn report(diags: &[Diag]) {
    eprint!("{}", hs_compiler::diagnostics::render_error(diags));
    std::process::exit(1);
}

fn die(msg: &str) -> ! {
    eprintln!("hard: {msg}");
    std::process::exit(1);
}