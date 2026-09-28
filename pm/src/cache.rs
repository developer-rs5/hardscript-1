//! The shared package cache at `~/.hard/cache/`.
//!
//! The cache is the single on-disk store shared by every HardScript project
//! on a machine. It holds downloaded package archives, their extracted
//! sources, resolver metadata and integrity hashes. Offline installs resolve
//! entirely from the cache.
//!
//! Layout (all writes are atomic rename-based, so a crash never leaves a
//! half-written package):
//!
//! ```text
//! ~/.hard/
//!   cache/
//!     index/<name>.json                 registry metadata cache
//!     packages/<name>/<version>.hspkg   package archive
//!     packages/<name>/<version>.hspkg.part  download in progress (resumable)
//!     packages/<name>/<version>/        extracted sources
//! ```

use crate::pkgfmt;
use crate::registry::RegistryError;
use crate::semver::Version;
use std::io;
use std::path::{Path, PathBuf};

/// The `~/.hard` directory (overrideable via `HARD_HOME` for tests/containers).
pub fn hard_home() -> PathBuf {
    if let Ok(h) = std::env::var("HARD_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".hard")
}

/// The cache root (`~/.hard/cache/`).
pub fn cache_root() -> PathBuf {
    hard_home().join("cache")
}

/// One cached archive that does not match its recorded digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheIssue {
    pub name: String,
    pub version: String,
    /// Why it failed: a digest mismatch, a missing metadata file, an archive
    /// that is not a valid `.hspkg`.
    pub reason: String,
    /// The archive on disk, so a repair removes exactly this file.
    pub archive: PathBuf,
}

impl std::fmt::Display for CacheIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}: {}", self.name, self.version, self.reason)
    }
}

impl CacheIssue {
    /// The line `hard cache verify` prints.
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// Serialized metadata for one cached version of a package.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CachedMeta {
    pub name: String,
    pub version: String,
    pub integrity: String,
    pub registry: Option<String>,
    pub deps: Vec<String>,
    pub description: Option<String>,
}

/// The cache store.
#[derive(Clone, Debug)]
pub struct Cache {
    pub root: PathBuf,
}

impl Cache {
    pub fn new() -> Cache {
        Cache {
            root: cache_root(),
        }
    }

    pub fn at(root: PathBuf) -> Cache {
        Cache { root }
    }

    // -- path helpers ------------------------------------------------------

    pub fn archive_path(&self, name: &str, version: &str) -> PathBuf {
        self.root
            .join("packages")
            .join(name)
            .join(format!("{}-{version}.hspkg", sanitize(name)))
    }

    /// Where an in-progress download is staged. A `.part` file is only ever
    /// renamed onto the real archive after its digest verifies, so a
    /// half-written download can never be mistaken for a cached package.
    pub fn part_path(&self, name: &str, version: &str) -> PathBuf {
        let safe = sanitize(name);
        self.root
            .join("packages")
            .join(name)
            .join(format!("{safe}-{version}.hspkg.part"))
    }

    /// Bytes already staged for an interrupted download (0 if none).
    pub fn staged_size(&self, name: &str, version: &str) -> u64 {
        std::fs::metadata(self.part_path(name, version))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Move a verified `.part` file into place. Fails if the staged bytes do
    /// not hash to `integrity`.
    pub fn finalize_part(
        &self,
        name: &str,
        version: &str,
        integrity: &str,
    ) -> Result<(), String> {
        let part = self.part_path(name, version);
        let bytes = std::fs::read(&part)
            .map_err(|e| format!("cannot read {}: {e}", part.display()))?;
        let got = pkgfmt::sha256_hex(&bytes);
        let want = integrity.strip_prefix("sha256:").unwrap_or(integrity);
        if got != want {
            let _ = std::fs::remove_file(&part);
            return Err(format!(
                "integrity mismatch for {name}@{version}: got {got}, want {want}"
            ));
        }
        let dest = self.archive_path(name, version);
        std::fs::rename(&part, &dest).map_err(|e| {
            format!(
                "cannot move {} to {}: {e}",
                part.display(),
                dest.display()
            )
        })
    }

    /// Throw away a staged download.
    pub fn drop_part(&self, name: &str, version: &str) {
        let _ = std::fs::remove_file(self.part_path(name, version));
    }

    /// How many versions have an interrupted download.
    pub fn staged_count(&self) -> usize {
        let mut n = 0;
        let Ok(rd) = std::fs::read_dir(self.root.join("packages")) else {
            return 0;
        };
        for e in rd.flatten() {
            let Ok(inner) = std::fs::read_dir(e.path()) else {
                continue;
            };
            for f in inner.flatten() {
                if f.file_name().to_string_lossy().ends_with(".hspkg.part") {
                    n += 1;
                }
            }
        }
        n
    }

    pub fn source_dir(&self, name: &str, version: &str) -> PathBuf {
        self.root
            .join("packages")
            .join(name)
            .join(format!("{}-{version}.src", sanitize(name)))
    }

    /// The metadata sidecar path (public so the downloader can drop a bad
    /// entry together with its archive).
    pub fn meta_path_for(&self, name: &str, version: &str) -> PathBuf {
        self.meta_path(name, version)
    }

    fn meta_path(&self, name: &str, version: &str) -> PathBuf {
        self.root
            .join("packages")
            .join(name)
            .join(format!("{}-{version}.json", sanitize(name)))
    }

    fn index_path(&self, name: &str) -> PathBuf {
        self.root.join("index").join(format!("{}.json", sanitize(name)))
    }

    // -- query --------------------------------------------------------------

    /// Is this exact version present in the cache?
    pub fn has(&self, name: &str, version: &Version) -> bool {
        self.has_version(name, &version.to_string())
    }

    /// Is this exact version present in the cache? (string form)
    pub fn has_version(&self, name: &str, version: &str) -> bool {
        self.archive_path(name, version).exists() && self.meta_path(name, version).exists()
    }

    /// Every cached version of `name`. Deterministic (ascending version order).
    pub fn cached_versions(&self, name: &str) -> Vec<Version> {
        let mut out = Vec::new();
        let dir = self.root.join("packages").join(name);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return out;
        };
        for e in entries.flatten() {
            let file = e.file_name();
            let file = file.to_string_lossy().into_owned();
            if let Some(stripped) = file.strip_suffix(".hspkg") {
                if let Some(v) = stripped.strip_prefix(&format!("{}-", sanitize(name))) {
                    if let Ok(ver) = Version::parse(v) {
                        out.push(ver);
                    }
                }
            }
        }
        out.sort();
        out
    }

    /// Metadata for a cached version.
    pub fn meta(&self, name: &str, version: &str) -> Option<CachedMeta> {
        let bytes = std::fs::read(self.meta_path(name, version)).ok()?;
        let j = hs_compiler::json::parse(&String::from_utf8_lossy(&bytes))?;
        let deps = j
            .get("deps")
            .and_then(|x| x.as_arr())
            .map(|a| a.iter().filter_map(|i| i.as_str().map(String::from)).collect())
            .unwrap_or_default();
        Some(CachedMeta {
            name: j.get("name").and_then(|x| x.as_str()).unwrap_or(name).to_string(),
            version: j.get("version").and_then(|x| x.as_str()).unwrap_or(version).to_string(),
            integrity: j.get("integrity").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            registry: j.get("registry").and_then(|x| x.as_str()).map(String::from),
            deps,
            description: j.get("description").and_then(|x| x.as_str()).map(String::from),
        })
    }

    /// Integrity check for a single cached archive.
    pub fn verify_version(&self, name: &str, version: &str) -> Result<(), String> {
        let path = self.archive_path(name, version);
        if !path.exists() {
            return Err(format!("{name}@{version}: archive missing"));
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("{name}@{version}: {e}"))?;
        let got = pkgfmt::sha256_hex(&bytes);
        // recorded digests may or may not carry the `sha256:` prefix
        let want = self
            .meta(name, version)
            .map(|m| m.integrity)
            .map(|i| i.trim_start_matches("sha256:").to_string())
            .unwrap_or_default();
        if want.is_empty() {
            // No recorded hash: verify structural validity instead.
            pkgfmt::read_archive(&bytes)
                .map(|_| ())
                .map_err(|e| format!("{name}@{version}: {e}"))
        } else if got.eq_ignore_ascii_case(&want) {
            Ok(())
        } else {
            Err(format!(
                "integrity mismatch for {name}@{version}: got {got}, want {want}"
            ))
        }
    }

    // -- write --------------------------------------------------------------

    /// Store archive bytes and their extracted sources in the cache.
    ///
    /// This is the only writer of `packages/<name>/<version>.hspkg`: the
    /// archive is validated, hashed, written atomically and only then
    /// extracted, so a crash can leave a `.tmp` file but never a half-written
    /// package that `has()` would report as present. Returns the recorded
    /// `sha256:<hex>`.
    pub fn put_bytes(
        &self,
        name: &str,
        version: &str,
        bytes: &[u8],
        registry: Option<&str>,
        description: Option<&str>,
    ) -> Result<String, String> {
        let integrity = pkgfmt::sha256_hex(bytes);
        let files = pkgfmt::read_archive(bytes)
            .map_err(|e| format!("refusing to cache invalid package {name}@{version}: {e}"))?;
        let dir = self.root.join("packages").join(name);
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create cache: {e}"))?;

        atomic_write_bytes(&self.archive_path(name, version), bytes)?;
        // an older, larger version's extracted tree must not linger
        let src_dir = self.source_dir(name, version);
        if src_dir.exists() {
            let _ = std::fs::remove_dir_all(&src_dir);
        }
        std::fs::create_dir_all(&src_dir).map_err(|e| format!("cannot create source dir: {e}"))?;
        pkgfmt::unpack(bytes, &src_dir).map_err(|e| format!("extract failed: {e}"))?;

        let rel_paths: Vec<String> = files.iter().map(|f| f.rel_path.clone()).collect();
        let recorded = format!("sha256:{integrity}");
        let meta = CachedMeta {
            name: name.to_string(),
            version: version.to_string(),
            integrity: recorded.clone(),
            registry: registry.map(String::from),
            deps: rel_paths.clone(),
            description: description.map(String::from),
        };
        let json = hs_compiler::json::Json::obj(vec![
            ("name", hs_compiler::json::Json::str(&meta.name)),
            ("version", hs_compiler::json::Json::str(&meta.version)),
            ("integrity", hs_compiler::json::Json::str(&meta.integrity)),
            (
                "registry",
                match &meta.registry {
                    Some(r) => hs_compiler::json::Json::str(r),
                    None => hs_compiler::json::Json::Null,
                },
            ),
            (
                "files",
                hs_compiler::json::Json::arr(
                    rel_paths.iter().map(hs_compiler::json::Json::str).collect(),
                ),
            ),
            (
                "description",
                match &meta.description {
                    Some(d) => hs_compiler::json::Json::str(d),
                    None => hs_compiler::json::Json::Null,
                },
            ),
        ]);
        atomic_write_bytes(
            &self.meta_path(name, version),
            json.to_string().as_bytes(),
        )?;
        Ok(recorded)
    }

    /// Store an archive and its extracted sources in the cache. The archive's
    /// integrity is verified before it is kept.
    #[allow(clippy::too_many_arguments)]
    pub fn put(
        &self,
        name: &str,
        version: &Version,
        archive: &[u8],
        registry: Option<&str>,
        description: Option<&str>,
    ) -> Result<(), String> {
        self.put_bytes(
            name,
            &version.to_string(),
            archive,
            registry,
            description,
        )
        .map(|_| ())
    }

    /// Copy a local directory into the cache as a package archive (used by
    /// tests and by `hard cache add <dir>`).
    pub fn put_local(&self, name: &str, version: &Version, dir: &Path) -> Result<(), String> {
        let archive = pkgfmt::pack_dir(dir).map_err(|e| e.to_string())?;
        self.put(name, version, &archive, Some("local"), Some("local package"))
    }

    /// Read a cached index file (raw registry metadata).
    pub fn read_index(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.index_path(name)).ok()
    }

    pub fn write_index(&self, name: &str, contents: &str) -> Result<(), String> {
        std::fs::create_dir_all(self.root.join("index"))
            .map_err(|e| format!("cannot create index dir: {e}"))?;
        atomic_write_bytes(&self.index_path(name), contents.as_bytes())
    }

    // -- stats --------------------------------------------------------------

    /// (package count, total bytes) across the cache.
    pub fn stats(&self) -> (usize, u64) {
        let mut count = 0usize;
        let mut bytes = 0u64;
        for (path, size) in walk_files(&self.root) {
            let _ = path;
            bytes += size;
            if path.extension().map(|e| e == "hspkg").unwrap_or(false) {
                count += 1;
            }
        }
        (count, bytes)
    }

    /// Verify every cached archive; returns corrupt entries.
    pub fn verify_all(&self) -> Vec<String> {
        self.audit().into_iter().map(|i| i.message()).collect()
    }

    /// Every cached archive that does not match its recorded digest.
    ///
    /// Structured rather than a list of strings, because `hard cache verify
    /// --repair` has to know *which* files to remove; recovering a name and a
    /// version out of a formatted message is how a repair ends up deleting the
    /// wrong path.
    pub fn audit(&self) -> Vec<CacheIssue> {
        let mut out = Vec::new();
        for (path, _) in walk_files(&self.root) {
            if !path.extension().map(|e| e == "hspkg").unwrap_or(false) {
                continue;
            }
            let name = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(String::from)
                .unwrap_or_default();
            let version = stem
                .strip_prefix(&format!("{}-", sanitize(&name)))
                .unwrap_or(&stem)
                .to_string();
            if let Err(reason) = self.verify_version(&name, &version) {
                // `verify_version` prefixes some of its messages with the
                // entry; strip it so `CacheIssue` is not "demo@1.0.0:
                // mismatch for demo@1.0.0".
                let prefix = format!("{name}@{version}: ");
                let reason = reason
                    .strip_prefix(&prefix)
                    .unwrap_or(&reason)
                    .to_string();
                out.push(CacheIssue {
                    name,
                    version,
                    reason,
                    archive: path,
                });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
        out
    }

    /// Delete the archives (and their recorded metadata) that fail the audit.
    ///
    /// A corrupt archive is worse than a missing one: it would install bytes
    /// nobody can account for. Removal is the whole repair — the next install
    /// fetches the package again and verifies it afresh.
    pub fn repair(&self) -> Result<Vec<CacheIssue>, String> {
        let issues = self.audit();
        for issue in &issues {
            if issue.archive.exists() {
                std::fs::remove_file(&issue.archive).map_err(|e| {
                    format!("cannot remove {}: {e}", issue.archive.display())
                })?;
            }
            let meta = self.meta_path_for(&issue.name, &issue.version);
            if meta.exists() {
                let _ = std::fs::remove_file(&meta);
            }
            // The extracted sources go too: leaving them behind would let a
            // later install link the code of a version whose archive was just
            // declared untrustworthy.
            let src = self
                .root
                .join("packages")
                .join(&issue.name)
                .join(format!("{}-{}.src", sanitize(&issue.name), issue.version));
            if src.exists() {
                std::fs::remove_dir_all(&src).map_err(|e| {
                    format!("cannot remove {}: {e}", src.display())
                })?;
            }
            let sig = self.root.join("signatures").join(format!(
                "{}@{}.json",
                issue.name, issue.version
            ));
            if sig.exists() {
                let _ = std::fs::remove_file(&sig);
            }
        }
        Ok(issues)
    }

    /// Remove every cached package (keeps the cache directory itself).
    pub fn clean(&self) -> io::Result<()> {
        if self.root.exists() {
            match std::fs::remove_dir_all(&self.root) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// A short `--json`-style summary line used by reports.
    pub fn summary(&self) -> (usize, u64) {
        self.stats()
    }
}

impl Default for Cache {
    fn default() -> Self {
        Cache::new()
    }
}

/// Persist an index/metadata blob into the summary used for reports.
pub fn cache_error(e: &RegistryError) -> String {
    format!("registry: {e}")
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot rename {}: {e}", path.display()))?;
    Ok(())
}

fn walk_files(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk_files(&p));
        } else if let Ok(m) = e.metadata() {
            out.push((p, m.len()));
        }
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkgfmt::FileRecord;
    use crate::semver::Version;

    fn temp_cache(tag: &str) -> Cache {
        let root = std::env::temp_dir().join(format!("hs-cache-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Cache::at(root)
    }

    fn demo_archive() -> Vec<u8> {
        crate::pkgfmt::pack(&[FileRecord {
            rel_path: "main.hard".to_string(),
            data: b"@handler main".to_vec(),
        }])
        .unwrap()
    }

    #[test]
    fn put_has_verify_roundtrip() {
        let cache = temp_cache("roundtrip");
        let v = Version::parse("1.0.0").unwrap();
        let archive = demo_archive();
        cache.put("demo", &v, &archive, Some("https://registry.example"), Some("a demo")).unwrap();
        assert!(cache.has("demo", &v));
        assert_eq!(cache.cached_versions("demo"), vec![v.clone()]);
        assert!(cache.verify_version("demo", "1.0.0").is_ok());
        // extracted source exists
        assert!(cache.source_dir("demo", "1.0.0").join("main.hard").exists());
        let (count, _) = cache.stats();
        assert_eq!(count, 1);
        let _ = std::fs::remove_dir_all(cache.root);
    }

    #[test]
    fn verify_all_reports_corruption() {
        let cache = temp_cache("verify");
        let v = Version::parse("1.0.0").unwrap();
        cache.put("demo", &v, &demo_archive(), None, None).unwrap();
        // corrupt the archive blob on disk
        let p = cache.archive_path("demo", "1.0.0");
        std::fs::write(&p, b"garbage").unwrap();
        let bad = cache.verify_all();
        assert!(
            bad.iter().any(|msg| msg.contains("demo") && msg.contains("1.0.0")),
            "verify_all did not flag corruption: {:?}",
            bad
        );
        let _ = std::fs::remove_dir_all(cache.root);
    }

    #[test]
    fn clean_removes_everything_and_recreates_on_next_put() {
        let cache = temp_cache("clean");
        let v = Version::parse("1.0.0").unwrap();
        cache.put("demo", &v, &demo_archive(), None, None).unwrap();
        assert!(cache.has("demo", &v));
        cache.clean().unwrap();
        assert!(!cache.has("demo", &v));
        assert!(!cache.root.exists()); // clean() wipes the whole cache root
        // a subsequent put recreates the directories
        cache.put("demo", &v, &demo_archive(), None, None).unwrap();
        assert!(cache.has("demo", &v));
        let _ = std::fs::remove_dir_all(cache.root);
    }

    #[test]
    fn rejects_corrupt_archive_on_put() {
        let cache = temp_cache("badput");
        let v = Version::parse("1.0.0").unwrap();
        let err = cache.put("demo", &v, b"not an archive", None, None).unwrap_err();
        assert!(err.contains("refusing to cache"), "message: {err}");
        let _ = std::fs::remove_dir_all(cache.root);
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    fn temp_cache(tag: &str) -> Cache {
        let dir = std::env::temp_dir().join(format!(
            "hs-cache-audit-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Cache::at(dir)
    }

    fn archive(src: &str) -> Vec<u8> {
        crate::pkgfmt::pack(&[crate::pkgfmt::FileRecord {
            rel_path: "main.hard".to_string(),
            data: src.as_bytes().to_vec(),
        }])
        .unwrap()
    }

    fn seed(cache: &Cache) {
        let v = Version::parse("1.0.0").unwrap();
        let bytes = archive("calc x() => Int { <- 1 }\n");
        cache
            .put("demo", &v, &bytes, None, None)
            .expect("seed the cache");
    }

    #[test]
    fn a_healthy_cache_audits_clean() {
        let cache = temp_cache("clean");
        seed(&cache);
        assert!(cache.audit().is_empty());
        assert!(cache.verify_all().is_empty());
        assert!(cache.repair().unwrap().is_empty());
        assert!(cache.archive_path("demo", "1.0.0").exists(), "repair must not touch a good entry");
    }

    #[test]
    fn an_audit_names_the_entry_and_keeps_the_path() {
        let cache = temp_cache("bad");
        seed(&cache);
        let path = cache.archive_path("demo", "1.0.0");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(b'x');
        std::fs::write(&path, &bytes).unwrap();
        let issues = cache.audit();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].name, "demo");
        assert_eq!(issues[0].version, "1.0.0");
        assert_eq!(issues[0].archive, path);
        assert!(issues[0].reason.contains("mismatch"), "{}", issues[0].reason);
        assert!(issues[0].message().starts_with("demo@1.0.0: "));
    }

    #[test]
    fn a_repair_removes_exactly_the_corrupt_archive() {
        let cache = temp_cache("repair");
        seed(&cache);
        let good = cache.archive_path("good", "1.0.0");
        let v = Version::parse("1.0.0").unwrap();
        cache.put("good", &v, &archive("calc y() => Int { <- 2 }\n"), None, None).unwrap();
        assert!(good.exists());
        let bad = cache.archive_path("demo", "1.0.0");
        let mut bytes = std::fs::read(&bad).unwrap();
        bytes.push(b'x');
        std::fs::write(&bad, &bytes).unwrap();

        let fixed = cache.repair().unwrap();
        assert_eq!(fixed.len(), 1);
        assert_eq!(fixed[0].name, "demo");
        assert!(!bad.exists(), "the corrupt archive is gone");
        assert!(good.exists(), "the healthy archive is untouched");
        assert!(cache.audit().is_empty(), "and the cache is clean again");
        assert!(!cache.meta_path_for("demo", "1.0.0").exists(), "its metadata goes too");
        assert!(
            !cache.root.join("packages/demo/demo-1.0.0.src").exists(),
            "the extracted sources go too"
        );
    }

    #[test]
    fn an_archive_with_no_metadata_is_checked_structurally() {
        // Deliberately lenient: a hand-seeded cache entry has no recorded
        // digest, so the only thing left to check is that it parses.
        let cache = temp_cache("nometa");
        seed(&cache);
        std::fs::remove_file(cache.meta_path_for("demo", "1.0.0")).unwrap();
        assert!(cache.audit().is_empty(), "a valid archive with no digest is kept");

        let path = cache.archive_path("demo", "1.0.0");
        std::fs::write(&path, b"not an archive").unwrap();
        let issues = cache.audit();
        assert_eq!(issues.len(), 1);
        assert!(!issues[0].reason.is_empty());
        assert!(!issues[0].message().contains("demo@1.0.0: demo@1.0.0"), "{}", issues[0].message());
    }

    #[test]
    fn a_repair_is_idempotent() {
        let cache = temp_cache("idem");
        seed(&cache);
        let bad = cache.archive_path("demo", "1.0.0");
        let mut bytes = std::fs::read(&bad).unwrap();
        bytes.push(b'x');
        std::fs::write(&bad, &bytes).unwrap();
        assert_eq!(cache.repair().unwrap().len(), 1);
        assert!(cache.repair().unwrap().is_empty(), "a second repair finds nothing to do");
    }
}
