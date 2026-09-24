use hs_compiler::{compile_to_cpp, fmt, frontend, render_all, Diag};
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
        "build" => cmd_build(rest),
        "run" => cmd_run(rest),
        "test" => cmd_test(rest),
        "fmt" => cmd_fmt(rest),
        "docs" => cmd_docs(rest),
        "doctor" => cmd_doctor(rest),
        "add" => cmd_add(rest),
        "bench" => cmd_bench(rest),
        "hir" => cmd_hir(rest),
        "opt" => cmd_opt(rest),
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
         \x20 hard new <name>              Create a new HardScript project\n\
         \x20 hard build [file]            Compile to a native executable\n\
         \x20 hard run   [file] [args..]   Build and run the server\n\
         \x20 hard test  [file]            Build and run the test suite\n\
         \x20 hard fmt   [file]            Reformat a source file in place\n\
         \x20 hard docs  [file]            Generate API.md for a source file\n\
         \x20 hard add <module>            Add a module reference to hard.toml\n\
         \x20 hard doctor                  Check the toolchain (g++, runtime)\n\
         \x20 hard bench [file]            Release-build and report timings\n\
         \x20 hard hir   [file]            Print the lowered HIR (debugging)\n\
         \x20 hard opt   [file]            Optimize and show before/after (debugging)\n\
         \x20 hard help                    Show this help\n\
         \n\
         Files default to main.hard in the current directory."
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
        Some(n) => n.clone(),
        None => {
            eprintln!("hard new: missing project name");
            std::process::exit(2);
        }
    };
    let dir = PathBuf::from(&name);
    if dir.exists() {
        eprintln!("hard new: '{name}' already exists");
        std::process::exit(2);
    }
    std::fs::create_dir_all(dir.join("runtime")).unwrap_or_else(|e| die(&e.to_string()));
    let main = format!(
        "bring http\n\
         \n\
         app @3000\n\
         \n\
         GET \"/\" :: {{\n\
         \x20   <- {{ \"hello\": \"{name}\" }}\n\
         }}\n\
         \n\
         test \"hello\" {{\n\
         \x20   res <- GET \"/\" {{ }}\n\
         \x20   expect res.status == 200\n\
         }}\n"
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

fn cmd_build(rest: &[String]) {
    let (target, _) = find_target(rest);
    match compile(target.as_path()) {
        Ok((_, bin)) => {
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
                println!("built {}", bin.display());
            }
        }
        Err(diags) => report(&diags),
    }
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
            )])
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

fn cmd_doctor(_args: &[String]) {
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
    if !ok {
        std::process::exit(1);
    }
}

fn cmd_add(args: &[String]) {
    let mod_name = match args.first() {
        Some(m) => m.clone(),
        None => {
            eprintln!("hard add: missing module name");
            std::process::exit(2);
        }
    };
    let toml_path = PathBuf::from("hard.toml");
    let mut content = if let Ok(s) = std::fs::read_to_string(&toml_path) {
        s
    } else {
        String::from("name = \"app\"\n")
    };
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!("{mod_name} = \"latest\"\n"));
    write(&toml_path, &content);
    println!("added module '{mod_name}' to hard.toml");
    println!("note: the registry is not live yet; the module list is informational.");
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
            format!("cannot read {}: {e}", target.display()))]),
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
        )]);
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
    let src = diags
        .iter()
        .filter_map(|d| d.location.as_ref())
        .find_map(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    eprint!("{}", render_all(diags));
    if !src.is_empty() {
        eprint!("{}", hs_compiler::diagnostics::frames_all(diags, &src));
    }
    std::process::exit(1);
}

fn die(msg: &str) -> ! {
    eprintln!("hard: {msg}");
    std::process::exit(1);
}