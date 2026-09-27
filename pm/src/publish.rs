//! `hard publish`: turn a project directory into a `.hspkg` and upload it.
//!
//! The flow mirrors what every package registry expects:
//!
//! 1. read `hard.toml` for the coordinates and the dependency edges;
//! 2. collect the files that belong in the package (source, manifest,
//!    docs — never build output, caches or VCS directories);
//! 3. pack them into a deterministic `.hspkg` archive;
//! 4. optionally check the plan without sending anything (`--dry-run`);
//! 5. upload the archive with the metadata as `X-Hard-*` headers, so the
//!    server never has to base64-inflate the bytes.
//!
//! Determinism matters twice over: the same tree always produces the same
//! archive bytes (and therefore the same SHA-256), and the integrity the
//! client computes locally is the integrity the registry records.

use crate::manifest::Manifest;
use crate::pkgfmt::{self, FileRecord};
use crate::registry::{HttpResponse, Method, Registry};
use crate::semver::Version;
use std::path::{Path, PathBuf};

/// Directories never included in a package.
pub const EXCLUDED_DIRS: &[&str] = &[
    ".hard",
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "__pycache__",
    "dist",
    "build",
];

/// File names never included in a package.
///
/// The secret-shaped entries matter as much as the build artifacts: publishing
/// a `.env` or a credentials file is how a token ends up in a public
/// registry, and no amount of `.gitignore` protects against `hard publish`
/// walking the tree itself.
pub const EXCLUDED_FILES: &[&str] = &[
    "hard.lock",
    ".DS_Store",
    "Cargo.lock",
    ".env",
    ".envrc",
    "credentials.toml",
    "registry.key",
    "id_rsa",
    "id_ed25519",
];

/// Suffixes never included in a package.
pub const EXCLUDED_SUFFIXES: &[&str] = &[
    ".hspkg", ".o", ".obj", ".exe", ".release", ".bin", ".key", ".pem", ".p12", ".pfx", ".keystore",
];

/// Largest package the client will build or upload.
pub const MAX_ARCHIVE_BYTES: usize = 16 * 1024 * 1024;

/// Everything `hard publish` decided to send, before it sends it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PublishPlan {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub documentation: Option<String>,
    pub keywords: Vec<String>,
    pub tags: Vec<String>,
    /// `name@req` edges, sorted.
    pub dependencies: Vec<String>,
    pub channel: Option<String>,
    /// Files that will go into the archive, sorted.
    pub files: Vec<String>,
    /// Archive size in bytes.
    pub size: u64,
    /// `sha256:<hex>` of the archive.
    pub integrity: String,
}

/// The outcome of a publish.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PublishReport {
    pub plan: PublishPlan,
    /// The registry's response body, verbatim.
    pub response: String,
    /// What the registry reported as the recorded integrity.
    pub registry_integrity: Option<String>,
    /// What the registry reported as the manifest fingerprint.
    pub fingerprint: Option<String>,
    pub signature: Option<String>,
    pub key_id: Option<String>,
    pub status: u16,
}

/// Why a publish could not be prepared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishError {
    pub message: String,
}

impl PublishError {
    fn new(message: impl Into<String>) -> PublishError {
        PublishError {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PublishError {}

/// Options that change what gets published.
#[derive(Clone, Debug, Default)]
pub struct PublishOptions {
    /// Extra tags, appended to the manifest's.
    pub tags: Vec<String>,
    /// Extra keywords.
    pub keywords: Vec<String>,
    /// Release channel (`stable`, `beta`, ...).
    pub channel: Option<String>,
    /// Do not talk to the registry at all.
    pub dry_run: bool,
    /// Bypass the "this version already exists" pre-check.
    pub allow_existing: bool,
    /// Override the manifest version.
    pub version: Option<String>,
    /// Registry override.
    pub registry: Option<String>,
    /// Bearer token.
    pub token: Option<String>,
}

/// Build the plan (and the archive bytes) for a project directory.
pub fn plan(
    project_root: &Path,
    manifest: &Manifest,
    options: &PublishOptions,
) -> Result<(PublishPlan, Vec<u8>), PublishError> {
    if manifest.name.is_empty() {
        return Err(PublishError::new("hard.toml has no package name"));
    }
    let version = options
        .version
        .clone()
        .unwrap_or_else(|| manifest.version.clone());
    if version.is_empty() {
        return Err(PublishError::new("hard.toml has no version to publish"));
    }
    Version::parse(&version)
        .map_err(|e| PublishError::new(format!("version '{version}' is not valid: {e}")))?;

    let files = collect_files(project_root)?;
    if files.is_empty() {
        return Err(PublishError::new(
            "nothing to publish: the project has no publishable files (is hard.toml the only file?)",
        ));
    }
    let records: Vec<FileRecord> = files
        .iter()
        .map(|(path, data)| FileRecord {
            rel_path: path.clone(),
            data: data.clone(),
        })
        .collect();
    let total: u64 = records.iter().map(|r| r.data.len() as u64).sum();
    if total > MAX_ARCHIVE_BYTES as u64 {
        return Err(PublishError::new(format!(
            "package would be {total} bytes, over the {MAX_ARCHIVE_BYTES} byte limit"
        )));
    }
    let archive = pkgfmt::pack(&records)
        .map_err(|e| PublishError::new(format!("cannot pack the package: {e}")))?;

    let mut dependencies: Vec<String> = manifest
        .dependencies
        .iter()
        .map(|(n, r)| format!("{n}@{r}"))
        .collect();
    for (n, r) in &manifest.dev_dependencies {
        dependencies.push(format!("dev:{n}@{r}"));
    }
    dependencies.sort();
    dependencies.dedup();

    let plan = PublishPlan {
        name: manifest.name.clone(),
        version,
        description: manifest.description.clone(),
        license: manifest.license.clone(),
        homepage: manifest.package.homepage.clone(),
        repository: manifest.package.repository.clone(),
        documentation: manifest.package.documentation.clone(),
        keywords: merge_lists(&manifest.package.keywords, &options.keywords),
        tags: merge_lists(&manifest.package.tags, &options.tags),
        dependencies,
        channel: options
            .channel
            .clone()
            .or_else(|| manifest.package.publish_as.clone()),
        files: files.iter().map(|(p, _)| p.clone()).collect(),
        size: archive.len() as u64,
        integrity: format!("sha256:{}", pkgfmt::sha256_hex(&archive)),
    };
    Ok((plan, archive))
}

fn merge_lists(base: &[String], extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = base.to_vec();
    for e in extra {
        let t = e.trim().to_ascii_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out.sort();
    out
}

/// Collect every publishable file under `root`, sorted, with the manifest
/// first so the archive is stable and self-describing.
pub fn collect_files(root: &Path) -> Result<Vec<(String, Vec<u8>)>, PublishError> {
    let mut out = Vec::new();
    walk(root, root, &mut out, 0)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    // hard.toml first: deterministic and what a reader expects to find
    if let Some(pos) = out.iter().position(|(p, _)| p == "hard.toml") {
        let m = out.remove(pos);
        out.insert(0, m);
    }
    Ok(out)
}

fn walk(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, Vec<u8>)>,
    depth: usize,
) -> Result<(), PublishError> {
    if depth > 16 {
        return Err(PublishError::new("package tree is nested too deeply"));
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|e| PublishError::new(format!("cannot read {}: {e}", dir.display())))?;
    let mut names: Vec<(PathBuf, bool)> = Vec::new();
    for e in entries.flatten() {
        let ft = match e.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        names.push((e.path(), ft.is_dir()));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, is_dir) in names {
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if is_dir {
            if EXCLUDED_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, out, depth + 1)?;
            continue;
        }
        if EXCLUDED_FILES.contains(&name.as_str()) {
            continue;
        }
        if EXCLUDED_SUFFIXES.iter().any(|s| name.ends_with(s)) {
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel = rel
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        let data = std::fs::read(&path)
            .map_err(|e| PublishError::new(format!("cannot read {}: {e}", path.display())))?;
        out.push((rel, data));
    }
    Ok(())
}

/// Is a path inside the package (i.e. would `hard publish` send it)?
pub fn is_publishable(rel: &str) -> bool {
    let mut parts = rel.split('/');
    let Some(name) = parts.next() else {
        return false;
    };
    if EXCLUDED_DIRS.contains(&name) {
        return false;
    }
    while let Some(p) = parts.next() {
        if EXCLUDED_DIRS.contains(&p) {
            return false;
        }
    }
    let base = rel.rsplit('/').next().unwrap_or(rel);
    !EXCLUDED_FILES.contains(&base)
        && !EXCLUDED_SUFFIXES.iter().any(|s| base.ends_with(s))
}

/// Ask the registry whether `name@version` already exists.
///
/// This uses `HEAD` on purpose: the download route counts downloads, and a
/// pre-publish conflict check must not inflate anybody's statistics.
pub fn version_exists(
    registry: &Registry,
    name: &str,
    version: &str,
) -> Result<bool, PublishError> {
    let path = format!("/packages/{}/{}", urlencode(name), urlencode(version));
    match registry.request(Method::Head, &path, Vec::new(), &[], None) {
        Ok(resp) => Ok(resp.is_success()),
        Err(e) => Err(PublishError::new(format!("cannot reach the registry: {e}"))),
    }
}

/// The `X-Hard-*` header set describing a plan.
pub fn headers_for(plan: &PublishPlan) -> Vec<(String, String)> {
    let mut h = vec![
        ("X-Hard-Package".to_string(), plan.name.clone()),
        ("X-Hard-Version".to_string(), plan.version.clone()),
        (
            "Content-Type".to_string(),
            "application/vnd.hardscript.package".to_string(),
        ),
    ];
    let mut push_opt = |k: &str, v: &Option<String>| {
        if let Some(v) = v {
            h.push((k.to_string(), v.clone()));
        }
    };
    push_opt("X-Hard-Description", &plan.description);
    push_opt("X-Hard-License", &plan.license);
    push_opt("X-Hard-Homepage", &plan.homepage);
    push_opt("X-Hard-Repository", &plan.repository);
    push_opt("X-Hard-Documentation", &plan.documentation);
    push_opt("X-Hard-Channel", &plan.channel);
    if !plan.tags.is_empty() {
        h.push(("X-Hard-Tags".to_string(), plan.tags.join(", ")));
    }
    if !plan.keywords.is_empty() {
        h.push(("X-Hard-Keywords".to_string(), plan.keywords.join(", ")));
    }
    if !plan.dependencies.is_empty() {
        h.push((
            "X-Hard-Dependencies".to_string(),
            plan.dependencies.join(", "),
        ));
    }
    h
}

/// Publish a prepared plan.
pub fn publish(
    registry: &Registry,
    plan: &PublishPlan,
    archive: &[u8],
    token: Option<&str>,
) -> Result<PublishReport, PublishError> {
    let headers = headers_for(plan);
    let refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let resp: HttpResponse = registry
        .request(Method::Post, "/api/publish", archive.to_vec(), &refs, token)
        .map_err(|e| PublishError::new(format!("cannot publish to the registry: {e}")))?;
    let mut report = PublishReport {
        plan: plan.clone(),
        response: resp.text(),
        status: resp.status,
        ..PublishReport::default()
    };
    if !resp.is_success() {
        return Err(PublishError::new(format!(
            "the registry rejected the publish: {}",
            Registry::error_message(&resp)
        )));
    }
    if let Some(j) = resp.json() {
        report.registry_integrity = j
            .get("integrity")
            .and_then(|v| v.as_str())
            .map(String::from);
        report.fingerprint = j
            .get("fingerprint")
            .and_then(|v| v.as_str())
            .map(String::from);
        report.signature = j
            .get("signature")
            .and_then(|v| v.as_str())
            .map(String::from);
        report.key_id = j
            .get("key_id")
            .and_then(|v| v.as_str())
            .map(String::from);
    }
    Ok(report)
}

/// Ask the registry to validate a plan without storing anything.
pub fn dry_run(
    registry: &Registry,
    plan: &PublishPlan,
    archive: &[u8],
    token: Option<&str>,
) -> Result<PublishReport, PublishError> {
    let mut headers = headers_for(plan);
    headers.push(("X-Hard-Dry-Run".to_string(), "1".to_string()));
    let refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let resp = registry
        .request(Method::Post, "/api/publish", archive.to_vec(), &refs, token)
        .map_err(|e| PublishError::new(format!("cannot reach the registry: {e}")))?;
    let report = PublishReport {
        plan: plan.clone(),
        response: resp.text(),
        status: resp.status,
        registry_integrity: resp
            .json()
            .and_then(|j| j.get("integrity").and_then(|v| v.as_str()).map(String::from)),
        fingerprint: resp
            .json()
            .and_then(|j| j.get("fingerprint").and_then(|v| v.as_str()).map(String::from)),
        ..PublishReport::default()
    };
    if !resp.is_success() {
        return Err(PublishError::new(format!(
            "the registry rejected the publish: {}",
            Registry::error_message(&resp)
        )));
    }
    Ok(report)
}

/// A human-readable summary of a plan, printed by `hard publish`.
pub fn render_plan(plan: &PublishPlan) -> String {
    let mut s = String::new();
    s.push_str(&format!("package  {} {}\n", plan.name, plan.version));
    if let Some(d) = &plan.description {
        s.push_str(&format!("about    {d}\n"));
    }
    if let Some(l) = &plan.license {
        s.push_str(&format!("license  {l}\n"));
    }
    if !plan.tags.is_empty() {
        s.push_str(&format!("tags     {}\n", plan.tags.join(", ")));
    }
    if !plan.keywords.is_empty() {
        s.push_str(&format!("keywords {}\n", plan.keywords.join(", ")));
    }
    if plan.dependencies.is_empty() {
        s.push_str("deps     (none)\n");
    } else {
        s.push_str(&format!("deps     {}\n", plan.dependencies.join(", ")));
    }
    if let Some(c) = &plan.channel {
        s.push_str(&format!("channel  {c}\n"));
    }
    s.push_str(&format!("files    {}\n", plan.files.len()));
    for f in &plan.files {
        s.push_str(&format!("  {f}\n"));
    }
    s.push_str(&format!("size     {} bytes\n", plan.size));
    s.push_str(&format!("integrity {}\n", plan.integrity));
    s
}

/// Percent-encode a path segment.
pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::default_manifest;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("hs-publish-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn demo_project(tag: &str) -> (PathBuf, Manifest) {
        let dir = scratch(tag);
        write(
            &dir.join("hard.toml"),
            "name = \"demo\"\nversion = \"1.2.0\"\ndescription = \"a demo\"\nlicense = \"MIT\"\n\n[dependencies]\nbase64 = \"^0.4.0\"\n",
        );
        write(&dir.join("main.hard"), "app @3000\n");
        write(&dir.join("src/lib.hard"), "calc x() => Int { <- 1 }\n");
        write(&dir.join("hard.lock"), "schema = \"hard-lock/v1\"\n");
        write(&dir.join(".hard/build.json"), "{}");
        write(&dir.join("target/debug/app"), "binary");
        write(&dir.join("README.md"), "# demo\n");
        let mut m = default_manifest("demo");
        m.name = "demo".to_string();
        m.version = "1.2.0".to_string();
        m.description = Some("a demo".to_string());
        m.license = Some("MIT".to_string());
        m.dependencies.insert("base64".to_string(), "^0.4.0".to_string());
        (dir, m)
    }

    #[test]
    fn plan_includes_sources_and_manifest_only() {
        let (dir, m) = demo_project("plan");
        let (p, archive) = plan(&dir, &m, &PublishOptions::default()).unwrap();
        assert_eq!(p.name, "demo");
        assert_eq!(p.version, "1.2.0");
        assert_eq!(
            p.files,
            vec!["hard.toml", "README.md", "main.hard", "src/lib.hard"]
        );
        assert!(p.files.contains(&"hard.toml".to_string()), "manifest first");
        assert!(!p.files.iter().any(|f| f.starts_with(".hard/")));
        assert!(!p.files.iter().any(|f| f.starts_with("target/")));
        assert!(!p.files.contains(&"hard.lock".to_string()));
        assert_eq!(p.size, archive.len() as u64);
        assert!(p.integrity.starts_with("sha256:"));
        assert_eq!(p.dependencies, vec!["base64@^0.4.0".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_is_deterministic() {
        let (dir, m) = demo_project("determinism");
        let (a, bytes_a) = plan(&dir, &m, &PublishOptions::default()).unwrap();
        let (b, bytes_b) = plan(&dir, &m, &PublishOptions::default()).unwrap();
        assert_eq!(a, b);
        assert_eq!(bytes_a, bytes_b, "same tree, same bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_dependencies_are_marked_dev() {
        let (dir, mut m) = demo_project("devdeps");
        m.dev_dependencies.insert("testlib".to_string(), "^1.0.0".to_string());
        let (p, _) = plan(&dir, &m, &PublishOptions::default()).unwrap();
        assert!(p.dependencies.contains(&"dev:testlib@^1.0.0".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn options_add_tags_keywords_and_version_override() {
        let (dir, m) = demo_project("options");
        let opts = PublishOptions {
            tags: vec!["web".to_string(), "auth".to_string()],
            keywords: vec!["Jwt".to_string()],
            channel: Some("beta".to_string()),
            version: Some("2.0.0".to_string()),
            ..PublishOptions::default()
        };
        let (p, _) = plan(&dir, &m, &opts).unwrap();
        assert_eq!(p.version, "2.0.0");
        assert_eq!(p.tags, vec!["auth", "web"]);
        assert_eq!(p.keywords, vec!["jwt"]);
        assert_eq!(p.channel.as_deref(), Some("beta"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_project_with_no_files_is_rejected() {
        let dir = scratch("empty");
        let mut m = default_manifest("empty");
        m.name = "empty".to_string();
        let err = plan(&dir, &m, &PublishOptions::default()).unwrap_err();
        assert!(err.message.contains("nothing to publish"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_version_is_rejected_before_packing() {
        let (dir, mut m) = demo_project("badversion");
        m.version = "one.two".to_string();
        let err = plan(&dir, &m, &PublishOptions::default()).unwrap_err();
        assert!(err.message.contains("not valid"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unnamed_project_is_rejected() {
        let (dir, mut m) = demo_project("noname");
        m.name = String::new();
        let err = plan(&dir, &m, &PublishOptions::default()).unwrap_err();
        assert!(err.message.contains("no package name"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn headers_describe_the_plan() {
        let (dir, m) = demo_project("headers");
        let opts = PublishOptions {
            tags: vec!["web".to_string()],
            channel: Some("stable".to_string()),
            ..PublishOptions::default()
        };
        let (p, _) = plan(&dir, &m, &opts).unwrap();
        let h = headers_for(&p);
        let get = |k: &str| {
            h.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("X-Hard-Package"), "demo");
        assert_eq!(get("X-Hard-Version"), "1.2.0");
        assert_eq!(get("X-Hard-License"), "MIT");
        assert_eq!(get("X-Hard-Tags"), "web");
        assert_eq!(get("X-Hard-Channel"), "stable");
        assert_eq!(get("X-Hard-Dependencies"), "base64@^0.4.0");
        assert!(get("Content-Type").contains("hardscript.package"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rendered_plan_is_readable() {
        let (dir, m) = demo_project("render");
        let (p, _) = plan(&dir, &m, &PublishOptions::default()).unwrap();
        let text = render_plan(&p);
        assert!(text.contains("package  demo 1.2.0"));
        assert!(text.contains("integrity sha256:"));
        assert!(text.contains("  src/lib.hard"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exclusion_rules_are_path_aware() {
        assert!(is_publishable("src/main.hard"));
        assert!(!is_publishable("hard.lock"));
        assert!(!is_publishable(".hard/build.json"));
        assert!(!is_publishable("target/debug/app"));
        assert!(!is_publishable("nested/target/x"));
        assert!(!is_publishable("x.hspkg"));
    }

    #[test]
    fn secrets_are_never_publishable() {
        // A token that reaches a public registry cannot be taken back, so the
        // walk refuses secret-shaped files wherever they are.
        for secret in [
            ".env",
            ".envrc",
            "credentials.toml",
            "registry.key",
            "server.key",
            "cert.pem",
            "id_rsa",
            "id_ed25519",
            "keystore.p12",
            "nested/deep/.env",
        ] {
            assert!(!is_publishable(secret), "'{secret}' must not be published");
        }
    }

    #[test]
    fn a_credentials_file_in_the_project_is_skipped() {
        let (dir, m) = demo_project("secret");
        // HARD_HOME inside the project is unusual, but the walk must still
        // refuse to ship the token store
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::fs::write(
            dir.join("home/credentials.toml"),
            "[registry.\"http://x\"]\ntoken = \"hspat_secret\"\n",
        )
        .unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();
        let (p, archive) = plan(&dir, &m, &PublishOptions::default()).unwrap();
        assert!(!p.files.iter().any(|f| f.contains("credentials.toml")), "{:?}", p.files);
        assert!(!p.files.iter().any(|f| f.ends_with(".env")), "{:?}", p.files);
        // and the bytes really are not in the archive
        let text = String::from_utf8_lossy(&archive);
        assert!(!text.contains("hspat_secret"), "the token leaked into the archive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn urlencoding_matches_the_client() {
        assert_eq!(urlencode("jwt"), "jwt");
        assert_eq!(urlencode("acme/http"), "acme%2Fhttp");
        assert_eq!(urlencode("a-b_c.d"), "a-b_c.d");
    }
}
