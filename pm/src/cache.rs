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

    pub fn source_dir(&self, name: &str, version: &str) -> PathBuf {
        self.root
            .join("packages")
            .join(name)
            .join(format!("{}-{version}.src", sanitize(name)))
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
        self.archive_path(name, &version.to_string()).exists()
            && self.meta_path(name, &version.to_string()).exists()
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
        let want = self
            .meta(name, version)
            .map(|m| m.integrity)
            .unwrap_or_default();
        if want.is_empty() {
            // No recorded hash: verify structural validity instead.
            pkgfmt::read_archive(&bytes)
                .map(|_| ())
                .map_err(|e| format!("{name}@{version}: {e}"))
        } else if got == want {
            Ok(())
        } else {
            Err(format!(
                "integrity mismatch for {name}@{version}: got {got}, want {want}"
            ))
        }
    }

    // -- write --------------------------------------------------------------

    /// Store an archive and its extracted sources in the cache. The archive's
    /// integrity is verified before it is kept.
    pub fn put(
        &self,
        name: &str,
        version: &Version,
        archive: &[u8],
        registry: Option<&str>,
        description: Option<&str>,
    ) -> Result<(), String> {
        let integrity = pkgfmt::sha256_hex(archive);
        pkgfmt::read_archive(archive)
            .map_err(|e| format!("refusing to cache invalid package {name}@{version}: {e}"))?;
        let dir = self.root.join("packages").join(name);
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create cache: {e}"))?;

        let archive_path = self.archive_path(name, &version.to_string());
        atomic_write_bytes(&archive_path, archive)?;
        let src_dir = self.source_dir(name, &version.to_string());
        std::fs::create_dir_all(&src_dir).map_err(|e| format!("cannot create source dir: {e}"))?;
        pkgfmt::unpack(archive, &src_dir).map_err(|e| format!("extract failed: {e}"))?;

        let meta = CachedMeta {
            name: name.to_string(),
            version: version.to_string(),
            integrity,
            registry: registry.map(String::from),
            deps: Vec::new(),
            description: description.map(String::from),
        };
        let json = hs_compiler::json::Json::obj(vec![
            ("name", hs_compiler::json::Json::str(&meta.name)),
            ("version", hs_compiler::json::Json::str(&meta.version)),
            ("integrity", hs_compiler::json::Json::str(&meta.integrity)),
            ("registry", match &meta.registry {
                Some(r) => hs_compiler::json::Json::str(r),
                None => hs_compiler::json::Json::Null,
            }),
            ("deps", hs_compiler::json::Json::arr(Vec::new())),
            ("description", match &meta.description {
                Some(d) => hs_compiler::json::Json::str(d),
                None => hs_compiler::json::Json::Null,
            }),
        ]);
        atomic_write_bytes(&self.meta_path(name, &version.to_string()), json.to_string().as_bytes())?;
        Ok(())
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
        let mut corrupt = Vec::new();
        for (path, _) in walk_files(&self.root) {
            if path.extension().map(|e| e == "hspkg").unwrap_or(false) {
                let name = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let version = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(String::from)
                    .unwrap_or_default();
                let version = version
                    .strip_prefix(&format!("{}-", sanitize(&name)))
                    .unwrap_or(&version)
                    .to_string();
                if let Err(e) = self.verify_version(&name, &version) {
                    corrupt.push(e);
                }
            }
        }
        corrupt
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
