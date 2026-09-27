//! `hard migrate` and `hard seed`: schema migrations for the native ORM.
//!
//! The flow is deliberately boring. `migrate diff` compares the models in the
//! project against the models embedded in the newest migration file, using the
//! same compiler that builds the application, and writes the difference as a
//! versioned file with an up side and a down side generated together.
//! `migrate up`, `down`, and `status`, and `seed`, shell out to a small C++
//! helper built from the embedded runtime, so the code that runs migrations is
//! the same code the fixtures test rather than a second implementation.

use crate::{die, find_target, report, write, write_runtime};
use hs_compiler::Diag as CDiag;
use hs_compiler::orm::{self, Dialect, Schema};
use hs_compiler::{catalog, frontend, Diag, ErrorKind};
use hs_pm::manifest::{parse as parse_manifest, ManifestMode};

/// Like `report`, but usable in expression position: it never returns.
fn fail_report(diags: &[CDiag]) -> ! {
    report(diags);
    std::process::exit(1);
}

/// A migration file on disk: its version, name, and full text.
struct MigrationFile {
    version: u32,
    path: std::path::PathBuf,
    text: String,
}

pub fn cmd_migrate(args: &[String]) {
    let (sub, rest) = match args.split_first() {
        Some(x) => x,
        None => {
            eprintln!("hard migrate: missing subcommand (diff, up, down, status)");
            std::process::exit(2);
        }
    };
    match sub.as_str() {
        "diff" => cmd_diff(rest),
        "up" => cmd_up(rest),
        "down" => cmd_down(rest),
        "status" => cmd_status(rest),
        other => {
            eprintln!("hard migrate: unknown subcommand '{other}' (diff, up, down, status)");
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

fn load_manifest() -> hs_pm::manifest::Manifest {
    match std::fs::read_to_string("hard.toml") {
        Ok(src) => match parse_manifest(&src, ManifestMode::Lenient) {
            Ok(res) => {
                for w in &res.warnings {
                    eprintln!("hard: warning: {}: {}", w.key, w.message);
                }
                res.manifest
            }
            Err(errs) => {
                for e in &errs {
                    eprintln!("hard: hard.toml: {}: {}", e.key, e.message);
                }
                std::process::exit(1);
            }
        },
        Err(_) => hs_pm::manifest::Manifest::default(),
    }
}

/// `--flag value` and `--flag=value`, removed from the args. Last one wins.
fn take_flag(args: &[String], name: &str) -> (Option<String>, Vec<String>) {
    let mut value = None;
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == name {
            match args.get(i + 1) {
                Some(v) => {
                    value = Some(v.clone());
                    i += 2;
                    continue;
                }
                None => {
                    eprintln!("hard: {name} needs a value");
                    std::process::exit(2);
                }
            }
        }
        if let Some(v) = a.strip_prefix(&format!("{name}=")) {
            value = Some(v.to_string());
            i += 1;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    (value, out)
}

fn no_dialect(name: Option<&str>) -> Diag {
    let mut msg = String::from("no database dialect configured");
    if let Some(n) = name {
        msg.push_str(&format!(": unknown dialect '{n}'"));
    }
    Diag::new_nospan(
        ErrorKind::Codegen,
        format!(
            "{msg}\nset dialect = \"sqlite\" or dialect = \"postgres\" in the manifest's [database] section, or pass --dialect <name>"
        ),
    )
    .with_code(catalog::ORM_NO_DIALECT)
}

fn parse_dialect(name: &str) -> Option<Dialect> {
    match name.to_lowercase().as_str() {
        "sqlite" | "sqlite3" => Some(Dialect::Sqlite),
        "postgres" | "postgresql" | "pg" => Some(Dialect::Postgres),
        _ => None,
    }
}

fn resolve_dialect(args: &[String]) -> (Dialect, Vec<String>) {
    let (flag, rest) = take_flag(args, "--dialect");
    if let Some(n) = flag {
        return match parse_dialect(&n) {
            Some(d) => (d, rest),
            None => fail_report(&[no_dialect(Some(&n))]),
        };
    }
    let manifest = load_manifest();
    match manifest.database.dialect {
        Some(n) => match parse_dialect(&n) {
            Some(d) => (d, rest),
            None => fail_report(&[no_dialect(Some(&n))]),
        },
        None => fail_report(&[no_dialect(None)]),
    }
}

fn resolve_target(args: &[String], dialect: Dialect) -> (String, Vec<String>) {
    let (flag, rest) = take_flag(args, "--database");
    if let Some(t) = flag {
        return (t, rest);
    }
    let manifest = load_manifest();
    match dialect {
        Dialect::Sqlite => (
            manifest.database.path.unwrap_or_else(|| "app.db".to_string()),
            rest,
        ),
        Dialect::Postgres => match manifest.database.url {
            Some(u) => (u, rest),
            None => {
                eprintln!(
                    "hard: no PostgreSQL connection configured\nset url = \"postgresql://...\" in the manifest's [database] section, or pass --database <url>"
                );
                std::process::exit(1);
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Models and schemas
// ---------------------------------------------------------------------------

/// The models of the project as source text, one block per model, in file
/// order. Each block is sliced out of the file starting at the model's own
/// span, so what gets embedded is what the programmer wrote, byte for byte.
fn model_sources(src: &str, path: &str) -> Result<Vec<String>, Vec<Diag>> {
    let prog = frontend(src, path)?;
    let mut models = prog.model_defs();
    models.sort_by_key(|m| (m.span.line, m.span.col));
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    for m in &models {
        out.push(extract_block(&lines, m.span.line, m.span.col));
    }
    Ok(out)
}

/// Slice one top-level block starting at (`line`, `col`), 1-based, by matching
/// the first bracket it opens. Strings and comments are skipped so a brace in
/// a default value does not end the block early.
fn extract_block(lines: &[&str], line: usize, col: usize) -> String {
    // Byte offset of the model's first character. A model's span points at its
    // name rather than its `model` keyword, so the scan starts at the
    // beginning of the line: a model is a top-level item, and the keyword is
    // on that line. Columns count characters; models are overwhelmingly ASCII,
    // and a multibyte character only shifts where the scan starts, never
    // whether the brackets match.
    let mut line_start = 0;
    let mut span_off = 0;
    for (i, l) in lines.iter().enumerate() {
        if i + 1 == line {
            let mut chars = l.chars();
            span_off = line_start;
            for _ in 1..col {
                if let Some(c) = chars.next() {
                    span_off += c.len_utf8();
                }
            }
            break;
        }
        line_start += l.len() + 1;
    }
    let text = lines.join("\n");
    // Prefer the line start when it opens a model declaration; fall back to
    // the span itself when the line holds something else first.
    let mut off = span_off;
    if text[line_start..].trim_start().starts_with("model ") {
        off = line_start;
    }
    let bytes = text.as_bytes();
    let mut i = off;
    // The `model Name ...` head ends at the first bracket.
    while i < bytes.len() && !matches!(bytes[i], b'{' | b'[' | b'(') {
        i += 1;
    }
    if i >= bytes.len() {
        return text[off..].to_string();
    }
    let (open, close) = (bytes[i], match bytes[i] {
        b'{' => b'}',
        b'[' => b']',
        _ => b')',
    });
    let _ = open;
    let mut depth = 0;
    let mut in_str = None;
    let mut in_line = false;
    let mut in_block = false;
    while i < bytes.len() {
        let c = bytes[i];
        let n = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
        if in_line {
            if c == b'\n' {
                in_line = false;
            }
            i += 1;
            continue;
        }
        if in_block {
            if c == b'*' && n == b'/' {
                in_block = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if let Some(q) = in_str {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                if n == q {
                    i += 2;
                    continue;
                }
                in_str = None;
            }
            i += 1;
            continue;
        }
        if c == b'-' && n == b'-' {
            in_line = true;
            i += 2;
            continue;
        }
        if c == b'/' && n == b'*' {
            in_block = true;
            i += 2;
            continue;
        }
        if c == b'\'' || c == b'"' {
            in_str = Some(c);
            i += 1;
            continue;
        }
        if c == b'{' || c == b'[' || c == b'(' {
            depth += 1;
        } else if c == b'}' || c == b']' || c == b')' {
            depth -= 1;
            if depth == 0 {
                i += 1;
                break;
            }
        }
        let _ = close;
        i += 1;
    }
    text[off..i].trim().to_string()
}

fn build_schema(models_src: &str) -> Schema {
    let prog = match frontend(models_src, "models.hard") {
        Ok(p) => p,
        Err(diags) => fail_report(&diags),
    };
    let models = prog.model_defs();
    match orm::build(&models, orm::BuildOpts::lenient()) {
        Ok(s) => s,
        Err(diags) => fail_report(&diags),
    }
}

fn schema_of_project(target: &std::path::Path) -> (Schema, Vec<String>) {
    let src = std::fs::read_to_string(target)
        .unwrap_or_else(|e| die(&format!("cannot read {}: {e}", target.display())));
    let blocks = match model_sources(&src, &target.to_string_lossy()) {
        Ok(b) => b,
        Err(diags) => fail_report(&diags),
    };
    let schema = build_schema(&blocks.join("\n\n"));
    (schema, blocks)
}

// ---------------------------------------------------------------------------
// Migration files
// ---------------------------------------------------------------------------

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("migrations")
}

fn list_migrations() -> Vec<MigrationFile> {
    let dir = migrations_dir();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        let mut parts = name.splitn(2, '_');
        let digits = parts.next().unwrap_or("");
        if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let version: u32 = digits.parse().unwrap_or(0);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = parts.next();
        out.push(MigrationFile { version, path, text });
    }
    out.sort_by_key(|m| m.version);
    out
}

/// The models a migration file was generated from, and the fingerprint they
/// had. A file whose models no longer match its fingerprint was hand-edited
/// after the fact, and diffing against it would silently drop the edit.
fn embedded_models(text: &str, path: &std::path::Path) -> (String, String) {
    let mut fingerprint = String::new();
    let mut models = Vec::new();
    let mut in_models = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(fp) = t.strip_prefix("-- hardscript:fingerprint ") {
            fingerprint = fp.trim().to_string();
            continue;
        }
        if t == "-- hardscript:models" {
            in_models = true;
            continue;
        }
        if t == "-- hardscript:end" {
            in_models = false;
            continue;
        }
        if in_models {
            // A line that is exactly `--` is a separator the writer put between
            // blocks, or an empty comment in the source. Either way it carries
            // no meaning, and leaving it in would hand the parser a line that
            // is not a model.
            if t == "--" {
                continue;
            }
            models.push(t.strip_prefix("-- ").unwrap_or(t));
        }
    }
    if models.is_empty() && !fingerprint.is_empty() {
        // A file from before models were embedded, or one edited down: either
        // way there is nothing trustworthy to diff against.
        let msg = format!(
            "migration conflict: {} has no embedded models, so a diff cannot know what it was generated from; \
             write new migrations by hand, or regenerate history",
            path.display()
        );
        fail_report(&[Diag::new_nospan(ErrorKind::Codegen, msg).with_code(catalog::MIGRATION_CONFLICT)]);
    }
    (fingerprint, models.join("\n"))
}

/// A fingerprint spans lines, and a migration header is one line: backslashes
/// go first and newlines become two characters, the same escaping the database
/// helper uses on its way out.
fn escape_fingerprint(fp: &str) -> String {
    fp.replace('\\', "\\\\").replace('\n', "\\n")
}

fn write_migration(version: u32, name: &str, dialect: Dialect, fingerprint: &str, blocks: &[String],
                   changes: &[orm::Change]) -> std::path::PathBuf {
    let mut out = String::new();
    out.push_str(&format!("-- hardscript:migration {version:04}\n"));
    out.push_str(&format!("-- hardscript:fingerprint {}\n", escape_fingerprint(fingerprint)));
    out.push_str("-- hardscript:models\n");
    for block in blocks {
        for line in block.lines() {
            if line.trim().is_empty() {
                out.push_str("--\n");
            } else {
                out.push_str(&format!("-- {line}\n"));
            }
        }
        out.push_str("--\n");
    }
    out.push_str("-- hardscript:end\n");
    out.push_str("-- +migrate Up\n");
    for c in changes {
        out.push_str(&format!("-- {}\n", c.summary()));
        for sql in c.up_sql(dialect) {
            out.push_str(sql.trim());
            out.push_str(";\n");
        }
    }
    out.push_str("-- +migrate Down\n");
    for c in changes.iter().rev() {
        out.push_str(&format!("-- {}\n", c.summary()));
        for sql in c.down_sql(dialect) {
            out.push_str(sql.trim());
            out.push_str(";\n");
        }
    }
    let filename = format!("{version:04}_{name}.sql");
    let path = migrations_dir().join(&filename);
    std::fs::create_dir_all(migrations_dir()).unwrap_or_else(|e| die(&e.to_string()));
    write(&path, &out);
    path
}

// ---------------------------------------------------------------------------
// `migrate diff`
// ---------------------------------------------------------------------------

fn cmd_diff(args: &[String]) {
    let (name_flag, args) = take_flag(args, "--name");
    let (dialect, args) = resolve_dialect(&args);
    let (target, _) = find_target(&args);
    let (new_schema, blocks) = schema_of_project(&target);
    let fingerprint = new_schema.fingerprint(dialect);

    let existing = list_migrations();
    let old_schema = match existing.last() {
        None => Schema { tables: Vec::new(), relations: Vec::new() },
        Some(m) => {
            let (fp, models) = embedded_models(&m.text, &m.path);
            let old = build_schema(&models);
            if !fp.is_empty() && fp != escape_fingerprint(&old.fingerprint(dialect)) {
                let msg = format!(
                    "migration conflict: {} was edited after it was generated: its models no longer match \
                     its recorded fingerprint; restore the file or reconcile it by hand before diffing",
                    m.path.display()
                );
                fail_report(&[Diag::new_nospan(ErrorKind::Codegen, msg)
                    .with_code(catalog::MIGRATION_CONFLICT)]);
            }
            old
        }
    };

    let changes = orm::diff(&old_schema, &new_schema, dialect);
    if changes.is_empty() {
        println!("migrate diff: no changes; the models match the last migration");
        return;
    }
    let next = existing.last().map(|m| m.version + 1).unwrap_or(1);
    if next > 9999 {
        die("migrate diff: migration versions exhausted (past 9999)");
    }
    let raw_name = name_flag.unwrap_or_else(|| "migration".to_string());
    let name: String = raw_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    let name = if name.is_empty() { "migration".to_string() } else { name };
    let path = write_migration(next, &name, dialect, &fingerprint, &blocks, &changes);
    println!("migrate diff: wrote {} ({} change{}):", path.display(), changes.len(), if changes.len() == 1 { "" } else { "s" });
    for c in &changes {
        println!("  - {}", c.summary());
    }
}

// ---------------------------------------------------------------------------
// The database helper: a C++ program that runs one command
// ---------------------------------------------------------------------------

const HELPER_SRC: &str = r#"// Generated by `hard migrate`: one command against one database.
#include "hs_runtime.hpp"
#include <fstream>
#include <iostream>
#include <memory>
#include <sstream>

static std::string read_file(const std::string& path) {
    std::ifstream f(path, std::ios::binary);
    if (!f) throw std::runtime_error("cannot read " + path);
    std::ostringstream out;
    out << f.rdbuf();
    return out.str();
}

static void fail(const std::string& msg) {
    std::cerr << "error: " << msg << "\n";
    std::exit(1);
}

// One line out of many: a fingerprint spans lines, and this protocol speaks in
// lines, so backslashes go first and newlines become two characters.
static std::string one_line(const std::string& s) {
    std::string out;
    for (char c : s) {
        if (c == '\\') out += "\\\\";
        else if (c == '\n') out += "\\n";
        else out += c;
    }
    return out;
}

int main(int argc, char** argv) {
    // helper <cmd> <sqlite|postgres> <target> [steps] <files...>
    if (argc < 4) fail("usage: helper <up|down|status|seed> <dialect> <target> [args] <files...>");
    std::string cmd = argv[1];
    std::string dialect = argv[2];
    std::string target = argv[3];
    try {
        hs::DbBackend* db = nullptr;
        std::unique_ptr<hs::SqliteDb> sqlite;
        std::unique_ptr<hs::PgDb> pg;
        if (dialect == "sqlite") {
            sqlite.reset(new hs::SqliteDb(target));
            db = sqlite.get();
        } else if (dialect == "postgres") {
            pg.reset(new hs::PgDb(target));
            db = pg.get();
        } else {
            fail("unknown dialect " + dialect);
        }
        if (cmd == "status") {
            std::vector<std::pair<std::string, std::string>> files;
            for (int i = 4; i < argc; i++) files.push_back({argv[i], read_file(argv[i])});
            hs::MigrationStatus st = hs::migration_status(db, files);
            for (auto& a : st.applied) std::cout << "A " << a.version << " " << a.name << "\n";
            for (auto& m : st.pending) std::cout << "P " << m.version << " " << m.name << "\n";
            for (auto& v : st.missing) std::cout << "M " << v << "\n";
            if (!st.conflict.empty()) std::cout << "C " << st.conflict << "\n";
            if (!st.applied.empty()) std::cout << "F " << one_line(st.applied.back().fingerprint) << "\n";
            return 0;
        }
        if (cmd == "up") {
            std::vector<std::pair<std::string, std::string>> files;
            for (int i = 4; i < argc; i++) files.push_back({argv[i], read_file(argv[i])});
            auto done = hs::migrate_up(db, files);
            std::cout << "U";
            for (auto& v : done) std::cout << " " << v;
            std::cout << "\n";
            return 0;
        }
        if (cmd == "down") {
            if (argc < 6) fail("usage: helper down <dialect> <target> <steps> <files...>");
            int steps = std::atoi(argv[4]);
            std::vector<std::pair<std::string, std::string>> files;
            for (int i = 5; i < argc; i++) files.push_back({argv[i], read_file(argv[i])});
            auto undone = hs::migrate_down(db, files, steps);
            std::cout << "D";
            for (auto& v : undone) std::cout << " " << v;
            std::cout << "\n";
            return 0;
        }
        if (cmd == "seed") {
            if (argc != 5) fail("usage: helper seed <dialect> <target> <file>");
            int n = hs::run_seed(db, argv[4], read_file(argv[4]));
            std::cout << "S " << n << "\n";
            return 0;
        }
        fail("unknown command " + cmd);
    } catch (const std::exception& e) {
        fail(e.what());
    }
    return 1;
}
"#;

/// Compile the helper once per invocation and run it. The helper is built from
/// the embedded runtime, so it always matches the compiler that generated it.
fn run_helper(cmd: &str, dialect: Dialect, target: &str, extra: &[String], files: &[String]) -> String {
    let bin_path = build_helper();
    let dialect_name = match dialect {
        Dialect::Sqlite => "sqlite",
        Dialect::Postgres => "postgres",
    };
    let mut command = std::process::Command::new(&bin_path);
    command.arg(cmd).arg(dialect_name).arg(target);
    for e in extra {
        command.arg(e);
    }
    for f in files {
        command.arg(f);
    }
    let run = command.output().unwrap_or_else(|e| die(&format!("could not run the migration helper: {e}")));
    if !run.status.success() {
        let err = String::from_utf8_lossy(&run.stderr);
        // The helper prints `error: <message>`; the CLI adds the catalog code.
        return format!("HELPER-FAILED:{}", err.trim());
    }
    String::from_utf8_lossy(&run.stdout).to_string()
}

/// Compile the migration helper into `.hard/` and return its path.
///
/// Public because `hard deploy` ships this exact binary to a server: the
/// migration code that runs in production is then the code that was compiled
/// here, from the same embedded runtime, rather than a second implementation
/// that only exists on the deploy path.
///
/// Cached on the fingerprint of the helper source and the runtime headers, the
/// same stamp the incremental build uses. Without it every `hard migrate up` and
/// every database deploy pays a three-second C++ compile to produce a binary
/// that has not changed since the last one -- measured at 3.2s in
/// `qa/bench_deploy.sh` before this existed, against 5ms after.
pub fn build_helper() -> std::path::PathBuf {
    let build_dir = std::path::PathBuf::from(".hard");
    let bin_path = build_dir.join("migrate-helper");
    let stamp = build_dir.join("migrate-helper.sha");
    let fingerprint = hs_compiler::sha256::hex(
        format!("{HELPER_SRC}\n{}", crate::runtime_fingerprint()).as_bytes(),
    );
    if let Ok(previous) = std::fs::read_to_string(&stamp) {
        if previous.trim() == fingerprint && bin_path.is_file() {
            return bin_path;
        }
    }
    std::fs::create_dir_all(&build_dir).unwrap_or_else(|e| die(&e.to_string()));
    write_runtime(&build_dir);
    let src_path = build_dir.join("migrate-helper.cpp");
    write(&src_path, HELPER_SRC);
    let out = std::process::Command::new("g++")
        .arg("-std=c++17")
        .arg("-O1")
        .arg("-pthread")
        .arg("-I")
        .arg(&build_dir)
        .arg(&src_path)
        .arg("-o")
        .arg(&bin_path)
        .arg("-ldl")
        .output()
        .unwrap_or_else(|e| die(&format!("could not run g++: {e}")));
    if !out.status.success() {
        die(&format!("could not build the migration helper:\n{}", String::from_utf8_lossy(&out.stderr)));
    }
    write(&stamp, &format!("{fingerprint}\n"));
    bin_path
}

fn helper_error(out: &str) -> String {
    out.strip_prefix("HELPER-FAILED:error: ")
        .or_else(|| out.strip_prefix("HELPER-FAILED:"))
        .unwrap_or(out)
        .to_string()
}

fn migration_files_as_args() -> Vec<String> {
    list_migrations().iter().map(|m| m.path.to_string_lossy().into_owned()).collect()
}

// ---------------------------------------------------------------------------
// `migrate up|down|status`
// ---------------------------------------------------------------------------

fn cmd_up(args: &[String]) {
    let (dialect, args) = resolve_dialect(args);
    let (target, _) = resolve_target(&args, dialect);
    let files = migration_files_as_args();
    let out = run_helper("up", dialect, &target, &[], &files);
    if let Some(err) = out.strip_prefix("HELPER-FAILED:") {
        let msg = helper_error(out.as_str());
        let _ = err;
        if msg.contains("conflict") {
            fail_report(&[Diag::new_nospan(ErrorKind::Codegen, msg).with_code(catalog::MIGRATION_CONFLICT)]);
        }
        die(&msg);
    }
    let applied: Vec<&str> = out.trim().strip_prefix("U").unwrap_or("").split_whitespace().collect();
    if applied.is_empty() {
        println!("migrate up: already up to date");
    } else {
        println!("migrate up: applied {}", applied.join(", "));
    }
}

fn cmd_down(args: &[String]) {
    let (steps_flag, args) = take_flag(args, "--steps");
    let (dialect, args) = resolve_dialect(&args);
    let (target, args) = resolve_target(&args, dialect);
    let steps = match steps_flag.or_else(|| args.first().cloned()) {
        Some(s) => s.parse::<i32>().unwrap_or_else(|_| {
            eprintln!("hard migrate down: steps must be a number, got '{s}'");
            std::process::exit(2);
        }),
        None => 1,
    };
    let files = migration_files_as_args();
    let out = run_helper("down", dialect, &target, &[steps.to_string()], &files);
    if out.starts_with("HELPER-FAILED:") {
        let msg = helper_error(&out);
        if msg.contains("conflict") {
            fail_report(&[Diag::new_nospan(ErrorKind::Codegen, msg).with_code(catalog::MIGRATION_CONFLICT)]);
        }
        die(&msg);
    }
    let undone: Vec<&str> = out.trim().strip_prefix("D").unwrap_or("").split_whitespace().collect();
    if undone.is_empty() {
        println!("migrate down: nothing applied");
    } else {
        println!("migrate down: rolled back {}", undone.join(", "));
    }
}

fn cmd_status(args: &[String]) {
    let (dialect, args) = resolve_dialect(args);
    let (target, _) = resolve_target(&args, dialect);
    let files = migration_files_as_args();
    let out = run_helper("status", dialect, &target, &[], &files);
    if out.starts_with("HELPER-FAILED:") {
        die(&helper_error(&out));
    }
    let mut applied = Vec::new();
    let mut pending = Vec::new();
    let mut conflict = None;
    let mut fingerprint = String::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("A ") {
            applied.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("P ") {
            pending.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("C ") {
            conflict = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("F ") {
            fingerprint = rest.to_string();
        }
    }
    println!("migrate status: {} applied, {} pending", applied.len(), pending.len());
    for a in &applied {
        println!("  applied {a}");
    }
    for p in &pending {
        println!("  pending {p}");
    }
    if let Some(c) = conflict {
        report(&[Diag::new_nospan(ErrorKind::Codegen, c).with_code(catalog::MIGRATION_CONFLICT)]);
    }
    // Drift: the models moved on without a migration. The fingerprint the last
    // migration recorded is what the database should match; anything else
    // means `migrate diff` has work to do.
    let (target_file, _) = find_target(&args);
    let (schema, _) = schema_of_project(&target_file);
    let current = escape_fingerprint(&schema.fingerprint(dialect));
    if !pending.is_empty() {
        println!("migrate status: run `hard migrate up` to apply");
    } else if !applied.is_empty() && fingerprint != current {
        println!("migrate status: the models changed since the last migration; run `hard migrate diff`");
    } else {
        println!("migrate status: up to date");
    }
}

// ---------------------------------------------------------------------------
// `hard seed`
// ---------------------------------------------------------------------------

pub fn cmd_seed(args: &[String]) {
    let (dialect, args) = resolve_dialect(args);
    let (target, args) = resolve_target(&args, dialect);
    let (file_flag, args) = take_flag(&args, "--file");
    let files: Vec<String> = if let Some(f) = file_flag.or_else(|| args.first().cloned()) {
        vec![f]
    } else {
        let mut seeds = Vec::new();
        if let Ok(entries) = std::fs::read_dir("seeds") {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("sql") {
                    seeds.push(path.to_string_lossy().into_owned());
                }
            }
        }
        seeds.sort();
        if seeds.is_empty() {
            eprintln!("hard seed: no seed files (pass a file, or put .sql files in seeds/)");
            std::process::exit(2);
        }
        seeds
    };
    let mut total = 0;
    for file in &files {
        let out = run_helper("seed", dialect, &target, &[], &[file.clone()]);
    if out.starts_with("HELPER-FAILED:") {
        let msg = helper_error(&out);
        fail_report(&[Diag::new_nospan(ErrorKind::Codegen, msg).with_code(catalog::SEED_FAILED)]);
    }
        total += out.trim().strip_prefix("S ").unwrap_or("0").trim().parse::<i32>().unwrap_or(0);
    }
    if files.len() == 1 {
        println!("seed: ran {total} statements from {}", files[0]);
    } else {
        println!("seed: ran {total} statements from {} files", files.len());
    }
}
