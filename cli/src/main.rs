use hs_compiler::{compile_to_cpp, fmt, frontend, Diag};
use hs_pm::cache::Cache;
use hs_pm::install::{self, InstallConfig};
use hs_pm::registry::{Registry, RegistryConfig};
use hs_pm::manifest::{default_manifest, Manifest};
use hs_pm::templates;
use hs_pm::workspace::Workspace;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

mod dockerfile;
mod migrate;

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
        "hs_runtime_pgsql.hpp",
        include_str!("../../runtime/hs_runtime_pgsql.hpp"),
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
        "hs_runtime_migrate.hpp",
        include_str!("../../runtime/hs_runtime_migrate.hpp"),
    ),
    (
        "hs_runtime_cache.hpp",
        include_str!("../../runtime/hs_runtime_cache.hpp"),
    ),
    (
        "hs_runtime_queue.hpp",
        include_str!("../../runtime/hs_runtime_queue.hpp"),
    ),
    (
        "hs_runtime_schedule.hpp",
        include_str!("../../runtime/hs_runtime_schedule.hpp"),
    ),
    (
        "hs_runtime_session.hpp",
        include_str!("../../runtime/hs_runtime_session.hpp"),
    ),
    (
        "hs_runtime_ratelimit.hpp",
        include_str!("../../runtime/hs_runtime_ratelimit.hpp"),
    ),
    (
        "hs_runtime_email.hpp",
        include_str!("../../runtime/hs_runtime_email.hpp"),
    ),
    (
        "hs_runtime_metrics.hpp",
        include_str!("../../runtime/hs_runtime_metrics.hpp"),
    ),
    (
        "hs_runtime_cluster.hpp",
        include_str!("../../runtime/hs_runtime_cluster.hpp"),
    ),
    (
        "hs_runtime_util.hpp",
        include_str!("../../runtime/hs_runtime_util.hpp"),
    ),
    ("hs_runtime.hpp", include_str!("../../runtime/hs_runtime.hpp")),
];

pub(crate) fn write_runtime(dir: &Path) {
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
    // `hard <cmd> --help` must never do work: without this, `hard publish
    // --help` would try to publish the current directory.
    if matches!(cmd.as_str(), "--help" | "-h" | "help") || rest.iter().any(|a| a == "--help" || a == "-h") {
        if let Some(text) = subcommand_help(cmd) {
            print!("{text}");
            return;
        }
        help();
        return;
    }
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
        "publish" => cmd_publish(rest),
        "yank" => cmd_yank(rest),
        "download" => cmd_download(rest),
        "login" => cmd_login(rest),
        "register" => cmd_register(rest),
        "logout" => cmd_logout(rest),
        "whoami" => cmd_whoami(rest),
        "token" => cmd_token(rest),
        "verify" => cmd_verify(rest),
        "keys" => cmd_keys(rest),
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
        "migrate" => migrate::cmd_migrate(rest),
        "seed" => migrate::cmd_seed(rest),
        "--version" | "-V" => println!("hard {VERSION}"),
        other => {
            eprintln!("hard: unknown command '{other}'");
            eprintln!("run `hard help` for usage.");
            std::process::exit(2);
        }
    }
}

/// Per-subcommand usage, for `hard <cmd> --help`.
fn subcommand_help(cmd: &str) -> Option<String> {
    let text = match cmd {
        "search" => {
            "hard search — search the registry\n\n\
             USAGE:\n\
             \x20 hard search <text> [--tag <tag>]... [--prefix <p>] [--owner <o>]\n\
             \x20 hard search --tag <tag> [--limit <n>] [--offset <n>]\n\n\
             OPTIONS:\n\
             \x20 --tag <tag>       only packages carrying this tag (repeatable)\n\
             \x20 --prefix <p>      only packages whose name starts with <p>\n\
             \x20 --owner <o>      only packages owned by <o>\n\
             \x20 --limit <n>       at most <n> results (1-200, default 20)\n\
             \x20 --offset <n>      skip the first <n> results\n\
             \x20 --json            print results as JSON\n\
             \x20 --explain         also print each result's score\n\
             \x20 --offline, -o     search the local cache instead of the network\n\n\
             The registry is asked first; if it cannot be reached the cached\n\
             index answers instead, and the summary says so.\n"
        }
        "publish" => {
            "hard publish — build a .hspkg and publish it\n\n\
             USAGE:\n\
             \x20 hard publish [--dry-run] [--token <t>] [--registry <url>]\n\n\
             OPTIONS:\n\
             \x20 --dry-run       validate and describe the package, upload nothing\n\
             \x20 --token <t>     publish with <t> instead of the stored token\n\
             \x20 --registry <url>  publish to <url>\n"
        }
        "download" => {
            "hard download — fetch a package archive\n\n\
             USAGE:\n\
             \x20 hard download <pkg>[@<ver>] [--output <path>] [--force]\n\n\
             A version that is already in the cache is not downloaded again\n\
             unless --force is given.\n"
        }
        "yank" => {
            "hard yank — retract a published version\n\n\
             USAGE:\n\
             \x20 hard yank <pkg>[@<ver>]\n\n\
             Yanking needs a token with the 'yank' scope.\n"
        }
        "add" => {
            "hard add — add a dependency and install it\n\n\
             USAGE:\n\
             \x20 hard add <pkg>[@<req>] [--dev]\n"
        }
        "install" => {
            "hard install — resolve and lock every dependency\n\n\
             USAGE:\n\
             \x20 hard install [--offline] [--frozen]\n"
        }
        "login" => "hard login — store a registry token\n\n\
             USAGE:\n             \x20 hard login [--user <name>] [--token <t>] [--registry <url>]\n",
        "register" => "hard register — create a registry account\n\n\
             USAGE:\n             \x20 hard register --user <name> [--email <e>]\n",
        "token" => "hard token — manage personal access tokens\n\n\
             USAGE:\n             \x20 hard token create [--name <n>] [--scope <s>]...\n             \x20 hard token list\n             \x20 hard token revoke <id>\n",
        "cache" => "hard cache — inspect and clean the package cache\n\n             USAGE:
             \x20 hard cache [info|clean|verify]\n",
        "info" => "hard info — show a package's registry metadata\n\n             USAGE:
             \x20 hard info <pkg> [--json]\n",
        _ => return None,
    };
    Some(text.to_string())
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
         \x20 hard publish                  Build a .hspkg and publish it to the registry\n\
         \x20 hard yank <pkg>[@<ver>]       Retract a published version\n\
         \x20 hard download <pkg>[@<ver>]    Download a .hspkg archive to disk\n\
         \x20 hard register --user <name>    Create a registry account\n\
         \x20 hard login [--user <name>]     Log in to a registry and store a token\n\
         \x20 hard logout                    Revoke and forget the stored token\n\
         \x20 hard whoami                     Show who the stored token belongs to\n\
         \x20 hard token <create|list|revoke>  Manage personal access tokens\n\
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
          \x20 hard migrate <diff|up|down|status> [--dialect <name>] [--database <target>]\n\
          \x20 hard seed   [file]            Run seed files against the database\n\
          \x20 hard help                    Show this help\n\
         \n\
         Files default to main.hard in the current directory.\n\
         \n\
         PM: hard.toml declares [dependencies]; hard.lock pins the resolution;\n\
         cache lives in $HARD_HOME/cache (default ~/.hard/cache)."
    );
}

pub(crate) fn find_target(rest: &[String]) -> (PathBuf, Vec<String>) {
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
         [database]\n\
         dialect = \"sqlite\"\n\
         path = \"{name}.db\"\n\
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
    if rest.iter().any(|a| a == "--docker") {
        return cmd_build_docker(&rest);
    }
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

/// `hard build --docker`: write a Dockerfile for this project.
///
/// The image is built from what the compiler already produced -- the generated
/// translation unit and the runtime headers -- so the builder stage is a C++
/// compiler and nothing else, and the runtime stage is plain Alpine.
fn cmd_build_docker(rest: &[String]) {
    let (target, _) = find_target(rest);
    let print_only = rest.iter().any(|a| a == "--print");
    let out = PathBuf::from("Dockerfile");

    // The project needs a build first: the Dockerfile compiles what the
    // compiler emits, so a Dockerfile for a project that has never been built
    // would be a Dockerfile that cannot work.
    let opts = build_options(false, 1);
    match incremental_build(&target, &opts, &hs_compiler::warn::WarningPolicy::default()) {
        Ok(_) => {}
        Err(diags) => report(&diags),
    }
    let cpp_name = target
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "main".to_string());

    let mut cfg = dockerfile::DockerConfig {
        app_name: std::env::current_dir()
            .ok()
            .and_then(|d| d.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "app".to_string()),
        cpp_name: cpp_name.clone(),
        ..dockerfile::DockerConfig::default()
    };
    // A manifest, when there is one, names the image and the port.
    if let Ok(Some(man)) = Manifest::load(Path::new("hard.toml")) {
        if !man.name.is_empty() {
            cfg.app_name = man.name.clone();
        }
        if let Some(port) = man.server.port {
            cfg.port = port;
        }
    }
    // A health path only makes sense if the program has one. A project using
    // the metrics module gets /healthz from the runtime; anything else is
    // probed at the TCP level rather than answering a 404 as "healthy".
    if let Ok(src) = std::fs::read_to_string(&target) {
        if let Ok(prog) = frontend(&src, target.to_string_lossy().into_owned()) {
            if !program_has_healthz(&prog) {
                cfg.health_path = None;
            }
        }
    }
    if std::env::var("HS_ALPINE").ok().filter(|v| !v.is_empty()).is_some() {
        let image = std::env::var("HS_ALPINE").unwrap();
        cfg.builder_image = image.clone();
        cfg.runtime_image = image;
    }
    if std::env::var("HS_DOCKER_HEALTH_PATH").ok().filter(|v| !v.is_empty()).is_some() {
        cfg.health_path = Some(std::env::var("HS_DOCKER_HEALTH_PATH").unwrap());
    }

    let dockerfile = dockerfile::render_dockerfile(&cfg);
    if print_only {
        print!("{dockerfile}");
        return;
    }
    write(&out, &dockerfile);
    // Without this the build context is the whole project, Rust build cache
    // included, and a small image takes a long time to arrive.
    write(&PathBuf::from(".dockerignore"), &dockerfile::render_dockerignore(&cfg));
    println!("wrote {}", out.display());
    println!("wrote .dockerignore");
    println!("build: docker build -t {} .", cfg.image_tag());
    println!("run:   docker run --rm -p {}:{} {}", cfg.port, cfg.port, cfg.image_tag());
}

/// Whether a program answers `GET /healthz`. Only the metrics module installs
/// that route, and a health check that requests it from a program without it
/// would report a perfectly healthy server as unhealthy.
fn program_has_healthz(prog: &hs_compiler::ast::Program) -> bool {
    use hs_compiler::ast::*;
    for st in &prog.stmts {
        if let Stmt::Route(r) = st {
            if r.method == "GET" && r.path == "/healthz" {
                return true;
            }
        }
    }
    false
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

    // Registry security: which key signs, and do we trust it? This is a
    // machine-wide fact, so it is reported whether or not this directory has
    // ever been built. `--no-registry` skips the network round trip.
    if !args.iter().any(|a| a == "--no-registry") {
        println!();
        let registry = Registry::new(RegistryConfig::resolve(None, false));
        let store = hs_pm::verify::TrustStore::load_default();
        let diag = hs_pm::verify::diagnose(&registry, &store);
        print!("{}", diag.render());
        if !diag.warnings().is_empty() {
            ok = false;
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

    let cfg = base_config_for(manifest, project_root.clone(), args);
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

/// The bearer token a registry command should present.
///
/// Precedence: `--token`, then `$HARD_TOKEN`, then the stored credentials
/// for that registry. This is the one place that decision is made, so
/// publish, yank, download and the auth commands cannot disagree.
fn registry_token(args: &[String], registry_url: &str) -> Option<String> {
    if let Some(t) = flag_value(args, "--token") {
        if !t.trim().is_empty() {
            return Some(t.trim().to_string());
        }
    }
    if let Ok(t) = std::env::var("HARD_TOKEN") {
        if !t.trim().is_empty() {
            return Some(t.trim().to_string());
        }
    }
    let store = hs_pm::credentials::CredentialStore::open();
    let t = store.token_for(registry_url);
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Build an auth client for the registry the command should use.
fn auth_client(args: &[String]) -> hs_pm::auth::Auth {
    let registry_override = flag_value(args, "--registry");
    let registry = Registry::new(RegistryConfig::resolve(registry_override.as_deref(), false));
    let mut store = hs_pm::credentials::CredentialStore::open();
    // `--token` (and $HARD_TOKEN) win over the stored credentials, which is
    // what a one-off command or a CI job wants.
    if let Some(t) = flag_value(args, "--token") {
        if !t.trim().is_empty() {
            if let Err(e) = store.add_token(&registry.config.url, t.trim()) {
                die(&e);
            }
        }
    }
    hs_pm::auth::Auth::new(registry, store)
}

/// `hard register` — create an account on a registry.
fn cmd_register(args: &[String]) {
    let user = flag_value(args, "--user")
        .or_else(|| flag_value(args, "-u"))
        .or_else(|| std::env::var("HARD_USER").ok())
        .unwrap_or_default();
    if user.trim().is_empty() {
        eprintln!("hard register: missing user name (--user <name>)");
        std::process::exit(2);
    }
    let password = match flag_value(args, "--password") {
        Some(p) => p,
        None => match std::env::var("HARD_PASSWORD") {
            Ok(p) if !p.is_empty() => p,
            _ => {
                eprintln!("hard register: no password supplied (--password, or HARD_PASSWORD)");
                std::process::exit(2);
            }
        },
    };
    let email = flag_value(args, "--email").or_else(|| std::env::var("HARD_EMAIL").ok());
    let auth = auth_client(args);
    match auth.register(&user, &password, email.as_deref()) {
        Ok(msg) => {
            println!("{msg}");
            println!("now run: hard login --user {user}");
        }
        Err(e) => die(&e),
    }
}

/// `hard login` — exchange credentials for a token and store it.
fn cmd_login(args: &[String]) {
    let user = flag_value(args, "--user")
        .or_else(|| flag_value(args, "-u"))
        .or_else(|| std::env::var("HARD_USER").ok())
        .unwrap_or_default();
    let auth = auth_client(args);
    if let Some(t) = flag_value(args, "--token") {
        // `hard login --token <t>` adopts a token obtained elsewhere
        if t.trim().is_empty() {
            die("hard login: --token needs a value");
        }
        match auth.adopt(if user.is_empty() { "unknown" } else { &user }, t.trim()) {
            Ok(()) => println!("stored a token for {} (as {})", auth.url(), if user.is_empty() { "unknown" } else { &user }),
            Err(e) => die(&e),
        }
        return;
    }
    if user.trim().is_empty() {
        eprintln!("hard login: missing user name (--user <name>, or set HARD_USER)");
        eprintln!("usage: hard login --user ada          (prompts for the password)");
        eprintln!("       hard login --user ada --password secret");
        std::process::exit(2);
    }
    let password = match flag_value(args, "--password") {
        Some(p) => p,
        None => match std::env::var("HARD_PASSWORD") {
            Ok(p) if !p.is_empty() => p,
            _ => {
                eprintln!("hard login: no password supplied");
                eprintln!("set HARD_PASSWORD, or pass --password (a shell prompt is not read here)");
                std::process::exit(2);
            }
        },
    };
    match auth.login(&user, &password) {
        Ok(login) => {
            println!("logged in to {} as {}", auth.url(), login.user);
            if !login.scopes.is_empty() {
                println!("  scopes: {}", login.scopes.join(", "));
            }
            println!("  token stored in {}", auth.store.path().display());
        }
        Err(e) => die(&e),
    }
}

/// `hard logout` — revoke the token and forget it.
fn cmd_logout(args: &[String]) {
    let all = args.iter().any(|a| a == "--all");
    let auth = auth_client(args);
    if all {
        let mut store = hs_pm::credentials::CredentialStore::open();
        match store.logout_all() {
            Ok(n) => {
                if n == 0 {
                    println!("not logged in to any registry");
                } else {
                    println!("logged out of {n} registry/registries");
                }
            }
            Err(e) => die(&e),
        }
        return;
    }
    match auth.logout() {
        Ok(true) => println!("logged out of {}", auth.url()),
        Ok(false) => println!("not logged in to {}", auth.url()),
        Err(e) => die(&e),
    }
}

/// `hard whoami` — who the stored token belongs to.
fn cmd_whoami(args: &[String]) {
    let auth = auth_client(args);
    match auth.whoami() {
        Ok(w) => {
            println!("registry {}", auth.url());
            println!("  user:   {}", w.user);
            println!("  token:  {}", w.token_id);
            println!("  label:  {}", w.name);
            println!("  scopes: {}", w.scopes.join(", "));
            match w.last_used_at {
                Some(at) => println!("  last used: {at}"),
                None => println!("  last used: never"),
            }
        }
        Err(e) => die(&e),
    }
}

/// `hard token create|list|revoke` — personal access tokens.
fn cmd_token(args: &[String]) {
    let verb = args.first().map(String::as_str).unwrap_or("list");
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    let auth = auth_client(args);
    match verb {
        "create" => {
            let name = rest
                .iter()
                .find(|a| !a.starts_with("--"))
                .cloned()
                .unwrap_or_else(|| "default".to_string());
            let scopes = hs_pm::credentials::parse_scopes(&repeated_flags(&rest, "--scope"))
                .unwrap_or_else(|e| die(&e));
            match auth.create_token(&name, &scopes) {
                Ok((info, plaintext)) => {
                    println!("created token {} ({})", info.id, info.name);
                    if info.scopes.is_empty() {
                        println!("  scopes: (registry default)");
                    } else {
                        println!("  scopes: {}", info.scopes.join(", "));
                    }
                    println!("  token: {plaintext}");
                    println!("  this is the only time it is shown; store it now");
                }
                Err(e) => die(&e),
            }
        }
        "list" | "ls" => match auth.list_tokens() {
            Ok(list) => {
                if list.is_empty() {
                    println!("no tokens for {}", auth.url());
                    return;
                }
                for t in list {
                    let used = match t.last_used_at {
                        Some(at) => at.to_string(),
                        None => "never".to_string(),
                    };
                    let state = if t.revoked { " (revoked)" } else { "" };
                    println!(
                        "{}  {:<16}  {:<12}  last used {}{}",
                        t.id, t.name, t.scopes.join(","), used, state
                    );
                }
            }
            Err(e) => die(&e),
        },
        "revoke" | "rm" => {
            let id = rest
                .iter()
                .find(|a| !a.starts_with("--"))
                .cloned()
                .or_else(|| flag_value(&rest, "--id"))
                .unwrap_or_default();
            if id.is_empty() {
                eprintln!("hard token revoke: missing token id");
                eprintln!("usage: hard token revoke <tok_...>");
                std::process::exit(2);
            }
            match auth.revoke_token(&id) {
                Ok(t) => println!("revoked {} ({})", t.id, t.name),
                Err(e) => die(&e),
            }
        }
        other => {
            eprintln!("hard token: unknown subcommand '{other}'");
            eprintln!("usage: hard token <create [name] [--scope s]> | list | revoke <id>");
            std::process::exit(2);
        }
    }
}

/// `hard download <pkg>[@<ver>]` — save a package's archive to disk.
fn cmd_download(args: &[String]) {
    let out_dir = flag_value(args, "--out").map(PathBuf::from);
    let offline = args.iter().any(|a| a == "--offline" || a == "-o");
    let spec = match args.iter().find(|a| !a.starts_with("--")) {
        Some(s) => s.clone(),
        None => {
            eprintln!("hard download: missing package name");
            std::process::exit(2);
        }
    };
    let (name, version) = install::parse_add_spec(&spec);
    let registry_override = flag_value(args, "--registry");
    let registry = Registry::new(RegistryConfig::resolve(registry_override.as_deref(), offline));
    let meta = match registry.metadata(&name) {
        Ok(m) => m,
        Err(e) => die(&format!("{e}")),
    };
    let version = match version {
        Some(v) => hs_pm::semver::Version::parse(&v)
            .unwrap_or_else(|e| die(&format!("bad version '{v}': {e}"))),
        None => match meta.versions.iter().max_by_key(|v| v.version.clone()) {
            Some(v) => v.version.clone(),
            None => die(&format!("{name} has no published versions")),
        },
    };
    let integrity = meta
        .versions
        .iter()
        .find(|v| v.version == version)
        .and_then(|v| v.integrity.clone());
    let out_dir = out_dir.unwrap_or_else(|| PathBuf::from("."));
    let downloader = hs_pm::download::Downloader::new(
        registry,
        Cache::new(),
        hs_pm::download::DownloadConfig {
            offline,
            parallel: 1,
            verbose: true,
            max_bytes: 0,
        },
    );
    match downloader.save_to(&name, &version, integrity.as_deref(), &out_dir) {
        Ok(f) => {
            println!("wrote {}", out_dir.join(format!("{name}-{version}.hspkg")).display());
            println!("  {}", f.summary());
        }
        Err(e) => die(&e.message),
    }
}

/// `hard publish` — build a `.hspkg` from the project and upload it.
fn cmd_publish(args: &[String]) {
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let allow_existing = args.iter().any(|a| a == "--allow-existing");
    let version_override = flag_value(args, "--version");
    let channel = flag_value(args, "--channel");
    let tags = repeated_flags(args, "--tag");
    let keywords = repeated_flags(args, "--keyword");
    let registry_override = flag_value(args, "--registry");
    let token = registry_token(args, &RegistryConfig::resolve(
        registry_override.as_deref(),
        false,
    )
    .url);

    let project_root = PathBuf::from(".");
    let (manifest, created) = load_manifest_or_default(&project_root);
    if created {
        // A directory with no hard.toml still has something publishable;
        // scaffold the manifest so the published archive describes itself.
        write(&project_root.join("hard.toml"), &manifest.render());
    }
    let options = hs_pm::publish::PublishOptions {
        tags,
        keywords,
        channel,
        dry_run,
        allow_existing,
        version: version_override,
        registry: registry_override.clone(),
        token: token.clone(),
    };
    let (plan, archive) = match hs_pm::publish::plan(&project_root, &manifest, &options) {
        Ok(v) => v,
        Err(e) => die(&e.message),
    };

    let env_registry = std::env::var("HARD_REGISTRY")
        .ok()
        .filter(|u| !u.trim().is_empty());
    if dry_run && options.registry.is_none() && env_registry.is_none() {
        // Nothing to talk to: the plan is still useful on its own.
        print!("{}", hs_pm::publish::render_plan(&plan));
        println!("dry run: no registry contacted (set registry = \"...\" or HARD_REGISTRY to check remotely)");
        return;
    }

    let registry = Registry::new(RegistryConfig::resolve(
        options.registry.as_deref().or(manifest.registry.as_deref()),
        false,
    ));
    println!("publishing {} {} to {}", plan.name, plan.version, registry.config.url);
    if !dry_run && !allow_existing {
        match hs_pm::publish::version_exists(&registry, &plan.name, &plan.version) {
            Ok(true) => {
                eprintln!(
                    "hard publish: {}@{} is already on {}",
                    plan.name, plan.version, registry.config.url
                );
                eprintln!("bump the version in hard.toml, pass --version, or use --allow-existing to see the conflict");
                std::process::exit(1);
            }
            Ok(false) => {}
            Err(e) => eprintln!("warning: {e}"),
        }
    }
    let result = if dry_run {
        hs_pm::publish::dry_run(&registry, &plan, &archive, token.as_deref())
    } else {
        hs_pm::publish::publish(&registry, &plan, &archive, token.as_deref())
    };
    match result {
        Ok(report) => {
            print!("{}", hs_pm::publish::render_plan(&report.plan));
            if let Some(i) = &report.registry_integrity {
                if *i != report.plan.integrity {
                    eprintln!(
                        "warning: the registry recorded integrity {i}, the local archive hashes to {}",
                        report.plan.integrity
                    );
                }
            }
            if let Some(fp) = &report.fingerprint {
                println!("recorded fingerprint {fp}");
            }
            if let Some(sig) = &report.signature {
                let kid = report.key_id.clone().unwrap_or_default();
                println!("signed by {kid} ({sig})");
            } else {
                println!("signature  (registry did not sign this publish)");
            }
            if dry_run {
                println!("dry run: the registry validated the publish without storing it");
            } else {
                println!("published {}@{}", plan.name, plan.version);
            }
        }
        Err(e) => die(&e.message),
    }
}

/// `hard yank <pkg>[@<version>]` — retract a published version.
fn cmd_yank(args: &[String]) {
    let unyank = args.iter().any(|a| a == "--unyank");
    let spec = match args.iter().find(|a| !a.starts_with("--")) {
        Some(s) => s.clone(),
        None => {
            eprintln!("hard yank: missing package name");
            std::process::exit(2);
        }
    };
    let (name, version) = install::parse_add_spec(&spec);
    let version = match version {
        Some(v) => v,
        None => {
            eprintln!("hard yank: specify a version, e.g. `hard yank jwt@1.0.0`");
            std::process::exit(2);
        }
    };
    let registry_override = flag_value(args, "--registry");
    let registry = Registry::new(RegistryConfig::resolve(registry_override.as_deref(), false));
    let token = registry_token(args, &registry.config.url);
    let path = if unyank {
        "/api/unyank"
    } else {
        "/api/yank"
    };
    let body = format!(
        "{{\"name\":\"{name}\",\"version\":\"{version}\",\"yanked\":{}}}",
        if unyank { "false" } else { "true" }
    );
    match registry.post_json(path, &body, token.as_deref()) {
        Ok(resp) if resp.is_success() => {
            println!(
                "{} {name}@{version}",
                if unyank { "restored" } else { "yanked" }
            );
        }
        Ok(resp) => {
            eprintln!(
                "hard yank: {}",
                Registry::error_message(&resp)
            );
            std::process::exit(1);
        }
        Err(e) => die(&format!("cannot reach the registry: {e}")),
    }
}

/// The value of `--name <value>` or `--name=<value>`.
/// `hard verify <target>` — check a package's signature.
///
/// The target is either a local `.hspkg` file (verified against its `.sig`
/// sidecar) or a `name`/`name@version` on the registry.
fn cmd_verify(args: &[String]) {
    use hs_pm::verify;
    let Some(target) = args.iter().find(|a| !a.starts_with('-')) else {
        eprintln!("hard verify: missing a package or .hspkg file");
        eprintln!("usage: hard verify <name>[@<ver>] | <file.hspkg> [--verify=<mode>] [--json] [--write-sidecar]");
        std::process::exit(2);
    };
    let json_out = args.iter().any(|a| a == "--json");
    let policy = verify_policy(args).unwrap_or_default();
    let verifier = hs_pm::verify::Verifier::with_store(policy, verify::TrustStore::load_default());
    let registry = Registry::new(RegistryConfig::resolve(flag_value(args, "--registry").as_deref(), false));

    // A path on disk is verified offline against its sidecar.
    let as_path = PathBuf::from(target);
    let (outcome, record) = if as_path.exists() || target.ends_with(".hspkg") {
        verify::verify_archive_file(&verifier, &as_path)
    } else {
        let (name, version) = match target.split_once('@') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (target.clone(), None),
        };
        let version = match version {
            Some(v) => v,
            None => {
                let meta = match registry.metadata(&name) {
                    Ok(m) => m,
                    Err(e) => die(&format!("cannot read metadata for '{name}': {e}")),
                };
                meta.versions
                    .iter()
                    .map(|v| v.version.clone())
                    .max()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| die(&format!("'{name}' has no published versions")))
            }
        };
        (verifier.verify_remote(&registry, &name, &version), None)
    };

    if let Some(p) = flag_value(args, "--write-sidecar") {
        let Some(rec) = &record else {
            die("--write-sidecar needs a local .hspkg with a known signature");
        };
        match verify::write_sidecar(rec, Path::new(&p)) {
            Ok(path) => println!("wrote {}", path.display()),
            Err(e) => die(&e),
        }
    }

    if json_out {
        println!("{}", outcome.to_json().to_string());
    } else {
        println!("{}", outcome.render());
    }
    // `hard verify` was asked to check, so a package that does not check out
    // is a failed command. The policy only decides what is *installed*.
    if policy != hs_pm::verify::VerifyPolicy::Off && !outcome.ok() {
        std::process::exit(1);
    }
}

/// `hard keys <list|add|remove|trust|export>` — the local trust store.
fn cmd_keys(args: &[String]) {
    use hs_pm::verify::{self, TrustedKey};
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    let path = flag_value(args, "--file")
        .map(PathBuf::from)
        .unwrap_or_else(verify::TrustStore::default_path);
    let mut store = verify::TrustStore::load(path.clone());
    let json_out = rest.iter().any(|a| a == "--json");

    match sub {
        "list" => {
            if json_out {
                println!("{}", store.to_json().to_string());
                return;
            }
            if store.is_empty() {
                println!("no keys recorded ({} does not exist yet)", path.display());
                println!("add one with: hard keys add <registry-url> | --key <hex> [--trust]");
                return;
            }
            for k in &store.keys {
                println!(
                    "{}  {}{}{}",
                    k.key_id,
                    if k.trusted { "trusted" } else { "seen   " },
                    match &k.packages {
                        Some(p) if !p.is_empty() => format!("  [{}]", p.join(", ")),
                        _ => String::new(),
                    },
                    if k.label.is_empty() {
                        String::new()
                    } else {
                        format!("  {}", k.label)
                    }
                );
            }
            println!(
                "{} of {} trusted ({} mode)",
                store.trusted_count(),
                store.len(),
                if store.trusted_count() == 0 {
                    "nothing will pass --verify=strict"
                } else {
                    "strict verification will accept these"
                }
            );
        }
        "add" => {
            let hex_key = flag_value(&rest, "--key");
            let label = flag_value(&rest, "--label").unwrap_or_default();
            let trust = flag_value(&rest, "--trust").is_some() || rest.iter().any(|a| a == "--trust");
            let for_pkg = flag_value(&rest, "--for");
            let key = match hex_key {
                Some(hex) => {
                    if !verify::is_valid_public_key(&hex) {
                        die(&format!("'{hex}' is not a valid ed25519 public key"));
                    }
                    let id = match verify::key_id_of(&hex) {
                        Some(id) => id,
                        None => die("cannot derive a key id from that key"),
                    };
                    TrustedKey {
                        key_id: id,
                        public_key: hex,
                        label: if label.is_empty() { "added by hand".to_string() } else { label },
                        trusted: trust,
                        packages: for_pkg.clone().map(|p| vec![p]),
                    }
                }
                None => {
                    // No key given: read it from a registry.
                    let url = rest
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .cloned()
                        .or_else(|| flag_value(&rest, "--registry"))
                        .unwrap_or_else(|| die("hard keys add: pass a registry URL or --key <hex>"));
                    let reg = Registry::new(RegistryConfig::resolve(Some(&url), false));
                    let k = match verify::RegistryKey::fetch(&reg) {
                        Ok(k) => k,
                        Err(e) => die(&format!("cannot read the signing key of {url}: {e}")),
                    };
                    if !verify::is_valid_public_key(&k.public_key) {
                        die("the registry published an invalid public key");
                    }
                    println!(
                        "{} signs with {} ({}{})",
                        url,
                        k.key_id,
                        if k.test_key { "TEST KEY, " } else { "" },
                        if trust { "pinning now" } else { "recorded, not pinned" }
                    );
                    if k.test_key && trust && !rest.iter().any(|a| a == "--allow-test-key") {
                        println!("(a test key is reproducible by anyone; pass --allow-test-key to pin it anyway)");
                    }
                    TrustedKey {
                        key_id: k.key_id,
                        public_key: k.public_key,
                        label: if label.is_empty() { url } else { label },
                        trusted: trust,
                        packages: for_pkg.clone().map(|p| vec![p]),
                    }
                }
            };
            let id = key.key_id.clone();
            let fresh = store.add(key);
            store.save().unwrap_or_else(|e| die(&e));
            println!("{} key {id}", if fresh { "recorded" } else { "updated" });
        }
        "trust" => {
            let Some(id) = rest.iter().find(|a| !a.starts_with('-')) else {
                die("hard keys trust: missing a key id");
            };
            store.trust(id).unwrap_or_else(|e| die(&e));
            store.save().unwrap_or_else(|e| die(&e));
            println!("trusted {id}");
        }
        "remove" | "rm" => {
            let Some(id) = rest.iter().find(|a| !a.starts_with('-')) else {
                die("hard keys remove: missing a key id");
            };
            if !store.remove(id) {
                die(&format!("no key with id '{id}'"));
            }
            store.save().unwrap_or_else(|e| die(&e));
            println!("removed {id}");
        }
        "export" => {
            if json_out {
                println!("{}", store.to_json().to_string());
            } else {
                print!("{}", store.render());
            }
        }
        other => {
            eprintln!("hard keys: unknown subcommand '{other}'");
            eprintln!("usage: hard keys <list|add|remove|trust|export>");
            std::process::exit(2);
        }
    }
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            return args.get(i + 1).cloned();
        }
        if let Some(v) = args[i].strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
        i += 1;
    }
    None
}

/// Every value of a repeatable `--name <value>` flag, in order.
fn repeated_flags(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            if let Some(v) = args.get(i + 1) {
                out.push(v.clone());
            }
            i += 2;
            continue;
        }
        if let Some(v) = args[i].strip_prefix(&format!("{name}=")) {
            out.push(v.to_string());
        }
        i += 1;
    }
    out
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
        let cfg = base_config_for(manifest, project_root.clone(), args);
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
    let mut cfg = base_config_for(manifest, project_root, args);
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
        if !report.downloads.fetched.is_empty() {
            println!("{}", report.downloads.summary());
        }
        if report.lock_written {
            println!("locked -> {}", report.lock_path.display());
        }
        print_report_verification(&report);
        print!("{}", report.graph);
    } else {
        for e in &report.errors {
            eprintln!("error: {e}");
        }
        std::process::exit(1);
    }
}

/// Print what signature checking found: one line per problem, then a count.
fn print_report_verification(report: &install::InstallReport) {
    for w in &report.verification_warnings {
        eprintln!("warning: {w}");
    }
    if report.verify_policy == hs_pm::verify::VerifyPolicy::Off
        || report.verification.is_empty()
    {
        return;
    }
    if report.verified == report.verification.len() {
        println!(
            "verified {} package signature(s)",
            report.verification.len()
        );
    } else {
        println!(
            "verified {}/{} package signature(s) ({})",
            report.verified,
            report.verification.len(),
            report.verify_policy
        );
    }
}

fn cmd_update(args: &[String]) {
    let project_root = PathBuf::from(".");
    let (manifest, created) = load_manifest_or_default(&project_root);
    if created {
        write(&project_root.join("hard.toml"), &manifest.render());
    }
    let mut cfg = base_config_for(manifest, project_root, args);
    // `update` is the one install that must not prefer locked versions.
    cfg.update = true;
    let report = install::install(&cfg);
    if report.succeeded() {
        println!("updated to newest matching versions");
        print_report_verification(&report);
        print!("{}", report.graph);
    } else {
        for e in &report.errors {
            eprintln!("error: {e}");
        }
        std::process::exit(1);
    }
}

fn cmd_list(args: &[String]) {
    let project_root = PathBuf::from(".");
    let (manifest, created) = load_manifest_or_default(&project_root);
    if created {
        write(&project_root.join("hard.toml"), &manifest.render());
    }
    let cfg = base_config_for(manifest, project_root, args);
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
    let mut cfg = base_config_for(manifest, project_root, args);
    cfg.offline = offline;
    for l in install::outdated(cfg) {
        println!("{l}");
    }
}

fn cmd_search(args: &[String]) {
    let text = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_default();
    let json_out = args.iter().any(|a| a == "--json");
    let tags = repeated_flags(args, "--tag");
    let offline = args.iter().any(|a| a == "--offline" || a == "-o");
    let mut query = hs_pm::search::SearchQuery::new(&text).with_tags(&tags);
    if let Some(p) = flag_value(args, "--prefix") {
        query = query.with_prefix(&p);
    }
    if let Some(o) = flag_value(args, "--owner") {
        query = query.with_owner(&o);
    }
    if let Some(l) = flag_value(args, "--limit") {
        query = query.with_limit(l.parse().unwrap_or(20));
    }
    if let Some(o) = flag_value(args, "--offset") {
        query.offset = o.parse().unwrap_or(0);
    }
    if query.is_empty() {
        eprintln!("hard search: missing query");
        eprintln!("usage: hard search <text> [--tag <t>] [--limit <n>] [--offset <n>]");
        std::process::exit(2);
    }
    let registry = Registry::new(RegistryConfig::resolve(
        flag_value(args, "--registry").as_deref(),
        offline,
    ));
    let cache = Cache::new();
    let results = hs_pm::search::search_with_fallback(&registry, &cache, &query);
    if let Some(e) = &results.error {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    if results.hits.is_empty() {
        println!("no packages matching '{text}'");
        if !tags.is_empty() {
            println!("(tags: {})", tags.join(", "));
        }
        if results.degraded {
            println!("(the registry could not be reached: nothing in the local cache)");
        }
        return;
    }
    if json_out {
        let items: Vec<hs_compiler::json::Json> = results
            .hits
            .iter()
            .map(|h| {
                hs_compiler::json::Json::obj(vec![
                    ("name", hs_compiler::json::Json::str(&h.name)),
                    (
                        "version",
                        match &h.version {
                            Some(v) => hs_compiler::json::Json::str(v),
                            None => hs_compiler::json::Json::Null,
                        },
                    ),
                    ("description", hs_compiler::json::Json::str(h.description.clone().unwrap_or_default())),
                    ("license", hs_compiler::json::Json::str(h.license.clone().unwrap_or_default())),
                    ("downloads", hs_compiler::json::Json::num(h.downloads as i64)),
                    (
                        "tags",
                        hs_compiler::json::Json::arr(
                            h.tags.iter().map(hs_compiler::json::Json::str).collect(),
                        ),
                    ),
                    (
                        "owner",
                        hs_compiler::json::Json::str(h.owner.clone().unwrap_or_default()),
                    ),
                    ("score", hs_compiler::json::Json::num(h.score)),
                ])
            })
            .collect();
        let j = hs_compiler::json::Json::obj(vec![
            ("query", hs_compiler::json::Json::str(&results.query)),
            (
                "origin",
                hs_compiler::json::Json::str(
                    results.origin.map(|o| o.as_str()).unwrap_or("none"),
                ),
            ),
            ("total", hs_compiler::json::Json::num(results.total as i64)),
            ("results", hs_compiler::json::Json::arr(items)),
        ]);
        println!("{}", j.to_string());
        return;
    }
    print!("{}", hs_pm::search::render_table(&results));
    if args.iter().any(|a| a == "--explain") {
        for h in &results.hits {
            println!("  {} score {}", h.name, h.score);
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
            let staged = cache.staged_count();
            if staged > 0 {
                println!("partial downloads: {staged} (resume with `hard install`)");
            }
            // list every package with its cached versions, sorted
            let mut names: Vec<String> = std::fs::read_dir(cache.root.join("packages"))
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.path().is_dir())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            for name in names {
                let versions: Vec<String> = cache
                    .cached_versions(&name)
                    .iter()
                    .map(|v| v.to_string())
                    .collect();
                if versions.is_empty() {
                    println!("  {name} (no complete archive)");
                } else {
                    println!("  {name} {}", versions.join(", "));
                }
            }
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

/// The signature policy for a command: `--verify=<mode>`, else `HARD_VERIFY`.
///
/// A bad value is fatal. Someone who typed `--verify=strict` must never end up
/// running a `warn` install because the mode name was misspelled.
fn verify_policy(args: &[String]) -> Option<hs_pm::verify::VerifyPolicy> {
    let raw = flag_value(args, "--verify")
        .or_else(|| args.iter().find_map(|a| a.strip_prefix("--verify=").map(String::from)));
    match hs_pm::verify::VerifyPolicy::from_flags(raw.as_deref()) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("hard: {e}");
            std::process::exit(2);
        }
    }
}

/// The shared install configuration, with the signature policy resolved.
fn base_config_for(manifest: Manifest, project_root: PathBuf, args: &[String]) -> InstallConfig {
    let mut cfg = install::base_config(
        manifest,
        project_root,
        false,
        false,
        "hard",
        VERSION,
        verify_policy(args),
    );
    cfg.offline = args.iter().any(|a| a == "--offline" || a == "-o");
    cfg
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

pub(crate) fn write(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap_or_else(|e| die(&format!("cannot write {}: {e}", path.display())));
}

pub(crate) fn report(diags: &[Diag]) {
    eprint!("{}", hs_compiler::diagnostics::render_error(diags));
    std::process::exit(1);
}

pub(crate) fn die(msg: &str) -> ! {
    eprintln!("hard: {msg}");
    std::process::exit(1);
}