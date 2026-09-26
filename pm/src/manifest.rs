//! `hard.toml` project manifest: model, parsing, validation, rendering.
//!
//! The manifest is the single source of truth for a HardScript project. It
//! is parsed with the project's own TOML reader ([crate::toml]) so that
//! diagnostics can point at exact lines/columns with a suggestion.

use crate::semver::{Version, VersionReq};
use crate::toml::{self, TomlValue};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The format version of `hard.toml` this toolchain understands.
pub const MANIFEST_SCHEMA: u32 = 1;
/// The default compiler edition a manifest declares.
pub const DEFAULT_EDITION: &str = "2027";

/// A warning about an unrecognized key (the manifest remains usable).
#[derive(Clone, Debug)]
pub struct ManifestWarning {
    pub key: String,
    pub message: String,
    pub suggestion: Option<String>,
}

/// A hard error in the manifest.
#[derive(Clone, Debug)]
pub struct ManifestError {
    pub key: String,
    pub message: String,
}

/// Validated `hard.toml`.
#[derive(Clone, Debug, Default)]
pub struct Manifest {
    pub schema: u32,
    pub name: String,
    pub version: String,
    pub edition: String,
    pub description: Option<String>,
    pub authors: Vec<String>,
    pub license: Option<String>,
    pub registry: Option<String>,
    pub compiler: CompilerConfig,
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub dependencies: BTreeMap<String, String>,
    pub dev_dependencies: BTreeMap<String, String>,
    pub workspace: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CompilerConfig {
    pub opt: u32,
    pub warnings: bool,
    pub jobs: usize,
}

impl Default for CompilerConfig {
    fn default() -> Self {
        CompilerConfig {
            opt: 3,
            warnings: true,
            jobs: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub port: Option<u16>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig { port: None }
    }
}

/// Which database the ORM and the migration commands talk to.
///
/// `dialect` is `"sqlite"` or `"postgres"`. A SQLite database is a file named
/// by `path`; a PostgreSQL database is named by `url`, either a
/// `postgresql://` URL or `key=value` pairs.
#[derive(Clone, Debug, Default)]
pub struct DatabaseConfig {
    pub dialect: Option<String>,
    pub path: Option<String>,
    pub url: Option<String>,
}

/// Results of parsing a manifest: the model plus any accumulated diagnostics.
#[derive(Clone, Debug, Default)]
pub struct ParseResult {
    pub manifest: Manifest,
    pub warnings: Vec<ManifestWarning>,
}

const KNOWN_TOP: &[&str] = &[
    "schema", "name", "version", "edition", "description", "authors", "license", "registry",
    "compiler", "server", "database", "dependencies", "dev-dependencies", "dev_dependencies", "workspace",
    "modules",
];
const KNOWN_COMPILER: &[&str] = &["opt", "warnings", "jobs"];
const KNOWN_SERVER: &[&str] = &["port", "host"];
const KNOWN_DATABASE: &[&str] = &["dialect", "path", "url"];

/// How detailed diagnostics the caller wants.
#[derive(Clone, Copy, PartialEq)]
pub enum ManifestMode {
    /// Strict: unknown keys are hard errors (used by validation fixtures).
    Strict,
    /// Lenient: unknown keys become warnings (used by init/new).
    Lenient,
}

/// Parse and validate a manifest from its source text.
pub fn parse(src: &str, mode: ManifestMode) -> Result<ParseResult, Vec<ManifestError>> {
    let doc = match toml::parse(src) {
        Ok(d) => d,
        Err(diags) => {
            return Err(diags
                .into_iter()
                .map(|d| ManifestError {
                    key: format!("{}:{}", d.line, d.col),
                    message: d.message,
                })
                .collect());
        }
    };
    let mut res = ParseResult::default();

    let mut errors: Vec<ManifestError> = Vec::new();

    if let Some(v) = doc.get("schema") {
        match v.as_int() {
            Some(1) => res.manifest.schema = 1,
            Some(n) => {
                errors.push(manifest_err(
                    "schema",
                    format!(
                        "this toolchain supports hard.toml schema {MANIFEST_SCHEMA}, found {n}",
                    ),
                ));
            }
            None => errors.push(manifest_err("schema", "the schema key must be an integer")),
        }
    } else {
        res.manifest.schema = MANIFEST_SCHEMA;
    }

    let get_str = |doc: &toml::TomlDoc, key: &str| -> Option<String> {
        doc.get(key).and_then(|v| v.as_str()).map(String::from)
    };

    res.manifest.name = get_str(&doc, "name").unwrap_or_default();
    res.manifest.version = get_str(&doc, "version").unwrap_or_default();
    res.manifest.edition = get_str(&doc, "edition").unwrap_or_else(|| DEFAULT_EDITION.to_string());
    res.manifest.description = get_str(&doc, "description");
    res.manifest.license = get_str(&doc, "license");
    res.manifest.registry = get_str(&doc, "registry");

    if let Some(v) = doc.get("authors") {
        match v {
            TomlValue::Str(s) => res.manifest.authors.push(s.clone()),
            TomlValue::Array(a) => {
                for item in a {
                    if let TomlValue::Str(s) = item {
                        res.manifest.authors.push(s.clone());
                    } else {
                        errors.push(manifest_err("authors", "each author must be a string"));
                    }
                }
            }
            _ => errors.push(manifest_err("authors", "authors must be a string or array of strings")),
        }
    }

    if let Some(t) = doc.table("compiler") {
        for (k, v) in t {
            match k.as_str() {
                "opt" => match v.as_int() {
                    Some(n) if (1..=3).contains(&n) => res.manifest.compiler.opt = n as u32,
                    Some(_) => errors.push(manifest_err("compiler.opt", "'opt' must be between 1 and 3")),
                    None => errors.push(manifest_err("compiler.opt", "'opt' must be an integer")),
                },
                "warnings" => match v.as_bool() {
                    Some(b) => res.manifest.compiler.warnings = b,
                    None => errors.push(manifest_err("compiler.warnings", "'warnings' must be a boolean")),
                },
                "jobs" => match v.as_int() {
                    Some(n) if n >= 1 => res.manifest.compiler.jobs = n as usize,
                    Some(_) => errors.push(manifest_err("compiler.jobs", "'jobs' must be >= 1")),
                    None => errors.push(manifest_err("compiler.jobs", "'jobs' must be an integer")),
                },
                other => {
                    res.warnings.push(ManifestWarning {
                        key: format!("compiler.{other}"),
                        message: format!("unknown compiler setting '{other}'"),
                        suggestion: suggest(other, KNOWN_COMPILER),
                    });
                }
            }
        }
    }

    if let Some(t) = doc.table("server") {
        for (k, v) in t {
            match k.as_str() {
                "port" => match v.as_int() {
                    Some(n) if (1..=65535).contains(&n) => res.manifest.server.port = Some(n as u16),
                    Some(_) => errors.push(manifest_err("server.port", "port must be between 1 and 65535")),
                    None => errors.push(manifest_err("server.port", "'port' must be an integer")),
                },
                "host" => { /* accepted, ignored in v0.4 */ }
                other => {
                    res.warnings.push(ManifestWarning {
                        key: format!("server.{other}"),
                        message: format!("unknown server setting '{other}'"),
                        suggestion: suggest(other, KNOWN_SERVER),
                    });
                }
            }
        }
    }

    if let Some(t) = doc.table("database") {
        for (k, v) in t {
            match k.as_str() {
                "dialect" => match v.as_str() {
                    Some(s) => res.manifest.database.dialect = Some(s.to_string()),
                    None => errors.push(manifest_err("database.dialect", "'dialect' must be a string")),
                },
                "path" => match v.as_str() {
                    Some(s) => res.manifest.database.path = Some(s.to_string()),
                    None => errors.push(manifest_err("database.path", "'path' must be a string")),
                },
                "url" => match v.as_str() {
                    Some(s) => res.manifest.database.url = Some(s.to_string()),
                    None => errors.push(manifest_err("database.url", "'url' must be a string")),
                },
                other => {
                    res.warnings.push(ManifestWarning {
                        key: format!("database.{other}"),
                        message: format!("unknown database setting '{other}'"),
                        suggestion: suggest(other, KNOWN_DATABASE),
                    });
                }
            }
        }
    }

    res.manifest.dependencies = parse_dep_table(&doc, "dependencies", &mut errors, &mut res.warnings);

    // dev-dependencies spelling may be hyphenated or underscore.
    let dev = parse_dep_table(&doc, "dev-dependencies", &mut errors, &mut res.warnings);
    if !dev.is_empty() {
        res.manifest.dev_dependencies = dev;
    } else {
        let dev2 = parse_dep_table(&doc, "dev_dependencies", &mut errors, &mut res.warnings);
        res.manifest.dev_dependencies = dev2;
    }

    if let Some(v) = doc.get("workspace") {
        match v {
            TomlValue::Array(a) => {
                for item in a {
                    match item {
                        TomlValue::Str(s) => res.manifest.workspace.push(s.clone()),
                        _ => errors.push(manifest_err(
                            "workspace",
                            "workspace members must be path strings",
                        )),
                    }
                }
            }
            TomlValue::Table(t) => {
                if let Some(TomlValue::Array(a)) = t.get("members") {
                    for item in a {
                        if let TomlValue::Str(s) = item {
                            res.manifest.workspace.push(s.clone());
                        }
                    }
                } else {
                    errors.push(manifest_err(
                        "workspace",
                        "a [workspace] table needs a 'members' array",
                    ));
                }
            }
            _ => errors.push(manifest_err(
                "workspace",
                "workspace must be an array of member paths like [\"apps/api\"]",
            )),
        }
    }

    // Top-level unknown keys. The legacy `modules` table is accepted so old
    // projects keep working (its entries are informational in v0.4).
    for (k, _v) in doc.root.iter() {
        if !KNOWN_TOP.contains(&k.as_str()) {
            res.warnings.push(ManifestWarning {
                key: k.clone(),
                message: format!("unknown top-level key '{k}'"),
                suggestion: suggest(k, KNOWN_TOP),
            });
        }
    }

    // Required fields.
    if res.manifest.name.is_empty() {
        errors.push(manifest_err("name", "missing required field 'name'"));
    } else if !is_valid_package_name(&res.manifest.name) {
        errors.push(manifest_err(
            "name",
            format!(
                "package name '{}' is invalid: use lowercase letters, digits, '_' or '-'",
                res.manifest.name
            ),
        ));
    }
    if res.manifest.version.is_empty() {
        errors.push(manifest_err("version", "missing required field 'version'"));
    } else if Version::parse(&res.manifest.version).is_err() {
        errors.push(manifest_err(
            "version",
            format!(
                "'{}' is not a valid semantic version (want e.g. \"0.1.0\")",
                res.manifest.version
            ),
        ));
    }
    if res.manifest.edition != DEFAULT_EDITION && res.manifest.edition != "2026" {
        errors.push(manifest_err(
            "edition",
            format!(
                "unknown edition '{}' (supported: 2026, {DEFAULT_EDITION})",
                res.manifest.edition
            ),
        ));
    }

    // Workspace path sanity.
    for m in &res.manifest.workspace {
        if m.is_empty() || m == "." || m == ".." || m.contains("..") {
            errors.push(manifest_err(
                "workspace",
                format!("invalid workspace member path '{m}'"),
            ));
        }
    }

    if mode == ManifestMode::Strict && !res.warnings.is_empty() {
        errors.extend(
            res.warnings
                .iter()
                .map(|w| ManifestError {
                    key: w.key.clone(),
                    message: w.message.clone(),
                }),
        );
        res.warnings.clear();
    }

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(res)
}

fn parse_dep_table(
    doc: &toml::TomlDoc,
    key: &str,
    errors: &mut Vec<ManifestError>,
    _warnings: &mut Vec<ManifestWarning>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(t) = doc.table(key) else { return out };
    for (name, req) in t {
        let text = match req {
            TomlValue::Str(s) => s.clone(),
            TomlValue::Table(_) => {
                errors.push(manifest_err(
                    &format!("{key}.{name}"),
                    "inline dependency tables are not supported yet; use version = \"^1.0\"",
                ));
                continue;
            }
            _ => {
                errors.push(manifest_err(
                    &format!("{key}.{name}"),
                    "dependency versions must be strings like \"^1.0\"",
                ));
                continue;
            }
        };
        if !is_valid_package_name(name) {
            errors.push(manifest_err(
                &format!("{key}.{name}"),
                format!("invalid dependency name '{name}'"),
            ));
            continue;
        }
        if VersionReq::parse(&text).is_err() {
            errors.push(manifest_err(
                &format!("{key}.{name}"),
                format!("invalid version requirement '{text}' for '{name}'"),
            ));
            continue;
        }
        out.insert(name.clone(), text);
    }
    out
}

fn is_valid_package_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 128 {
        return false;
    }
    let first = name.chars().next().unwrap();
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// A tiny edit-distance suggestion helper.
pub fn suggest(input: &str, candidates: &[&str]) -> Option<String> {
    let input = input.to_ascii_lowercase();
    let mut best: Option<(&str, usize)> = None;
    for c in candidates {
        let d = levenshtein(&input, &c.to_ascii_lowercase());
        let n = input.len().max(c.len());
        if d <= 2 && n > 0 {
            match best {
                Some((_, bd)) if bd <= d => {}
                _ => best = Some((c, d)),
            }
        }
    }
    best.map(|(c, _)| format!("did you mean '{c}'?"))
}

fn levenshtein(a: &str, b: &str) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.chars().enumerate() {
            cur.push((prev[j] + usize::from(ca != cb)).min(cur[j] + 1).min(prev[j + 1] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

// ---------------------------------------------------------------------------
// rendering / generation
// ---------------------------------------------------------------------------

impl Manifest {
    /// Load from a `hard.toml` path. Returns None when the file is missing;
    /// propagates parse errors/validation errors as Err.
    pub fn load(path: &Path) -> Result<Option<Manifest>, Vec<ManifestError>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(vec![ManifestError {
                    key: "file".into(),
                    message: format!("cannot read {}: {e}", path.display()),
                }]);
            }
        };
        let res = parse(&String::from_utf8_lossy(&bytes), ManifestMode::Lenient)?;
        Ok(Some(res.manifest))
    }

    /// Render the manifest back to canonical TOML. Deterministic: keys are
    /// emitted in a stable order and the output round-trips through the
    /// parser unchanged.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("schema = {}\n", self.schema));
        s.push_str(&format!("name = {}\n", toml_quote(&self.name)));
        s.push_str(&format!("version = {}\n", toml_quote(&self.version)));
        s.push_str(&format!("edition = {}\n", toml_quote(&self.edition)));
        if let Some(d) = &self.description {
            s.push_str(&format!("description = {}\n", toml_quote(d)));
        }
        if !self.authors.is_empty() {
            let authors: Vec<String> = self.authors.iter().map(|a| toml_quote(a)).collect();
            s.push_str(&format!("authors = [{}]\n", authors.join(", ")));
        }
        if let Some(l) = &self.license {
            s.push_str(&format!("license = {}\n", toml_quote(l)));
        }
        if let Some(r) = &self.registry {
            s.push_str(&format!("registry = {}\n", toml_quote(r)));
        }
        if !self.workspace.is_empty() {
            let members: Vec<String> = self.workspace.iter().map(|m| toml_quote(m)).collect();
            s.push_str(&format!("workspace = [{}]\n", members.join(", ")));
        }
        s.push('\n');

        s.push_str("[compiler]\n");
        s.push_str(&format!("opt = {}\n", self.compiler.opt));
        s.push_str(&format!("warnings = {}\n", self.compiler.warnings));
        if self.compiler.jobs > 0 {
            s.push_str(&format!("jobs = {}\n", self.compiler.jobs));
        }
        s.push('\n');
        s.push_str("[server]\n");
        if let Some(p) = self.server.port {
            s.push_str(&format!("port = {p}\n"));
        }
        s.push('\n');

        if self.database.dialect.is_some() {
            s.push_str("[database]\n");
            if let Some(d) = &self.database.dialect {
                s.push_str(&format!("dialect = {}\n", toml_quote(d)));
            }
            if let Some(p) = &self.database.path {
                s.push_str(&format!("path = {}\n", toml_quote(p)));
            }
            if let Some(u) = &self.database.url {
                s.push_str(&format!("url = {}\n", toml_quote(u)));
            }
            s.push('\n');
        }

        if !self.dependencies.is_empty() || !self.dev_dependencies.is_empty() {
            if !self.dependencies.is_empty() {
                s.push_str("[dependencies]\n");
                for (k, v) in &self.dependencies {
                    s.push_str(&format!("{k} = {}\n", toml_quote(v)));
                }
                s.push('\n');
            }
            if !self.dev_dependencies.is_empty() {
                s.push_str("[dev-dependencies]\n");
                for (k, v) in &self.dev_dependencies {
                    s.push_str(&format!("{k} = {}\n", toml_quote(v)));
                }
                s.push('\n');
            }
        }

        s
    }

    /// Merge another manifest's dependencies into `self`.
    pub fn merge_dependencies(&mut self, other: &Manifest) {
        for (k, v) in &other.dependencies {
            self.dependencies.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
}

fn toml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn manifest_err(key: &str, message: impl Into<String>) -> ManifestError {
    ManifestError {
        key: key.to_string(),
        message: message.into(),
    }
}

/// Default manifest for a project directory name.
pub fn default_manifest(name: &str) -> Manifest {
    let mut m = Manifest::default();
    m.schema = MANIFEST_SCHEMA;
    m.name = sanitize_default_name(name);
    m.version = "0.1.0".to_string();
    m.edition = DEFAULT_EDITION.to_string();
    m.description = Some(format!("The {} HardScript service", sanitize_default_name(name)));
    m.compiler = CompilerConfig {
        opt: 3,
        warnings: true,
        jobs: 0,
    };
    m.server = ServerConfig { port: Some(3000) };
    m
}

fn sanitize_default_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "app".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The project directory a manifest lives in (parent of `hard.toml`).
pub fn project_root(manifest_path: &Path) -> PathBuf {
    manifest_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> &'static str {
        r#"schema = 1
name = "demo"
version = "1.2.0"
edition = "2027"
description = "A demo service"
license = "MIT"
workspace = ["a", "b"]

[compiler]
opt = 2
warnings = false
jobs = 4

[server]
port = 4321

[dependencies]
http = "^1.0.0"
"#
    }

    #[test]
    fn parses_a_full_manifest() {
        let res = parse(sample(), ManifestMode::Strict).expect("sample parses");
        let m = &res.manifest;
        assert_eq!(m.name, "demo");
        assert_eq!(m.version, "1.2.0");
        assert_eq!(m.description.as_deref(), Some("A demo service"));
        assert_eq!(m.compiler.opt, 2);
        assert!(!m.compiler.warnings);
        assert_eq!(m.compiler.jobs, 4);
        assert_eq!(m.server.port, Some(4321));
        assert_eq!(m.dependencies.get("http").map(String::as_str), Some("^1.0.0"));
        assert_eq!(m.workspace, vec!["a", "b"]);
        assert!(res.warnings.is_empty(), "unexpected warnings: {:?}", res.warnings);
    }

    #[test]
    fn a_database_section_names_the_dialect_and_where_it_lives() {
        let src = "schema = 1\nname = \"x\"\nversion = \"0.1.0\"\n\n[database]\ndialect = \"sqlite\"\npath = \"app.db\"\n";
        let res = parse(src, ManifestMode::Strict).expect("database parses");
        assert_eq!(res.manifest.database.dialect.as_deref(), Some("sqlite"));
        assert_eq!(res.manifest.database.path.as_deref(), Some("app.db"));
        assert!(res.manifest.database.url.is_none());
        let back = res.manifest.render();
        assert!(back.contains("[database]"), "rendered:\n{back}");
        assert!(back.contains("[database]"), "rendered:\n{back}");
        let again = parse(&back, ManifestMode::Strict).expect("rendered manifest parses");
        assert_eq!(again.manifest.database.dialect.as_deref(), Some("sqlite"));
    }

    #[test]
    fn an_unknown_database_key_is_a_warning_with_a_suggestion() {
        let src = "schema = 1\nname = \"x\"\nversion = \"0.1.0\"\n\n[database]\ndialects = \"sqlite\"\n";
        let res = parse(src, ManifestMode::Lenient).expect("still parses");
        assert!(res.manifest.database.dialect.is_none());
        assert!(res.warnings.iter().any(|w| w.key == "database.dialects"), "{:?}", res.warnings);
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let bad = r#"schema = 99
name = "x"
"#;
        let errs = parse(bad, ManifestMode::Strict).expect_err("bad schema rejected");
        assert!(errs.iter().any(|e| e.key == "schema"));
    }

    #[test]
    fn strict_mode_rejects_unknown_dependency_shape() {
        let bad = r#"schema = 1
name = "x"

[dependencies]
foo = true
"#;
        let errs = parse(bad, ManifestMode::Strict).expect_err("bad dep rejected");
        assert!(!errs.is_empty());
    }

    #[test]
    fn lenient_mode_accepts_modules_fixture() {
        // cli_matrix fixture: legacy `[modules]` projects must keep working.
        let fixture = r#"name = "hsok"
version = "0.1.2"

[modules]
datetime = "latest"
"#;
        let res = parse(fixture, ManifestMode::Lenient).expect("legacy fixture parses");
        assert_eq!(res.manifest.name, "hsok");
    }

    #[test]
    fn render_roundtrips_the_model() {
        let res = parse(sample(), ManifestMode::Strict).unwrap();
        let rendered = res.manifest.render();
        let reparsed = parse(&rendered, ManifestMode::Strict).expect("rendered manifest re-parses");
        assert_eq!(reparsed.manifest.name, "demo");
        assert_eq!(reparsed.manifest.dependencies, res.manifest.dependencies);
        assert_eq!(reparsed.manifest.compiler.opt, 2);
        assert_eq!(reparsed.manifest.workspace, vec!["a", "b"]);
    }

    #[test]
    fn default_manifest_is_valid() {
        let m = default_manifest("init-test");
        assert_eq!(m.name, "init-test");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.edition, DEFAULT_EDITION);
        assert_eq!(m.schema, MANIFEST_SCHEMA);
        assert!(m.dependencies.is_empty());
        // a default manifest must round-trip through the parser
        parse(&m.render(), ManifestMode::Strict).expect("default renders cleanly");
    }

    #[test]
    fn merge_dependencies_unions_both_sides() {
        let mut base = default_manifest("merge");
        base.dependencies.insert("a".into(), "^1.0.0".into());
        let mut other = default_manifest("other");
        other.dependencies.insert("b".into(), "^2.0.0".into());
        other.dev_dependencies.insert("c".into(), "^0.1.0".into());
        base.merge_dependencies(&other);
        assert_eq!(base.dependencies.len(), 2);
        // merge_dependencies merges only [dependencies], not dev-dependencies
        assert!(base.dev_dependencies.is_empty());
    }
}
