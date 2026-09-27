//! Archive storage on disk.
//!
//! The registry keeps the bytes it serves in a content-addressed-ish layout
//! underneath its data directory, keyed by package name and version:
//!
//! ```text
//! <data>/archives/<name>/<name>-<version>.hspkg
//! ```
//!
//! Metadata lives in the store; bytes live here. A publish writes the
//! archive, verifies the digest, and only then commits the row, so a crash
//! can leave an orphan file (harmless, swept by [`ArchiveStore::sweep`]) but
//! never a row pointing at missing bytes.

use hs_pm::pkgfmt;
use crate::security;
use std::path::{Path, PathBuf};

/// Maximum accepted archive size.
pub const MAX_ARCHIVE_BYTES: usize = crate::model::MAX_ARCHIVE_BYTES;

/// One archive's on-disk facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredArchive {
    pub integrity: String,
    pub fingerprint: String,
    pub size: u64,
    pub file_count: usize,
    pub files: Vec<String>,
}

/// Why an archive was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArchiveError {
    TooLarge(usize),
    Malformed(String),
    UnsafePath(String),
    NoFiles,
    Io(String),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArchiveError::TooLarge(n) => write!(
                f,
                "archive is {n} bytes, over the {MAX_ARCHIVE_BYTES} byte limit"
            ),
            ArchiveError::Malformed(m) => write!(f, "archive is not a valid .hspkg: {m}"),
            ArchiveError::UnsafePath(p) => write!(f, "archive contains an unsafe path: {p}"),
            ArchiveError::NoFiles => write!(f, "archive contains no files"),
            ArchiveError::Io(m) => write!(f, "{m}"),
        }
    }
}

impl ArchiveError {
    pub fn status(&self) -> u16 {
        match self {
            ArchiveError::TooLarge(_) => 413,
            _ => 400,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            ArchiveError::TooLarge(_) => "package_too_large",
            ArchiveError::Malformed(_) => "malformed_package",
            ArchiveError::UnsafePath(_) => "unsafe_package",
            ArchiveError::NoFiles => "empty_package",
            ArchiveError::Io(_) => "io_error",
        }
    }
}

/// Result alias for archive operations.
pub type ArchiveResult<T> = Result<T, ArchiveError>;

/// The on-disk archive area of one registry.
#[derive(Clone, Debug)]
pub struct ArchiveStore {
    root: PathBuf,
    max_bytes: usize,
}

impl ArchiveStore {
    /// Use `<data>/archives` as the storage root.
    pub fn new(data_dir: &Path) -> ArchiveStore {
        ArchiveStore {
            root: data_dir.join("archives"),
            max_bytes: MAX_ARCHIVE_BYTES,
        }
    }

    /// Use an explicit root (tests point it at a temp dir).
    pub fn at(root: impl Into<PathBuf>) -> ArchiveStore {
        ArchiveStore {
            root: root.into(),
            max_bytes: MAX_ARCHIVE_BYTES,
        }
    }

    /// Override the size cap.
    pub fn with_max_bytes(mut self, n: usize) -> ArchiveStore {
        self.max_bytes = n;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Where a package version's archive lives.
    pub fn path(&self, name: &str, version: &str) -> PathBuf {
        let safe = sanitize(name);
        self.root
            .join(&safe)
            .join(format!("{safe}-{version}.hspkg"))
    }

    /// Is the archive present on disk?
    pub fn has(&self, name: &str, version: &str) -> bool {
        self.path(name, version).exists()
    }

    /// Read an archive's bytes.
    pub fn read(&self, name: &str, version: &str) -> ArchiveResult<Vec<u8>> {
        std::fs::read(self.path(name, version))
            .map_err(|e| ArchiveError::Io(format!("cannot read {name}@{version}: {e}")))
    }

    /// Validate an archive and write it, returning its recorded facts.
    ///
    /// Validation covers the size cap, the `.hspkg` structure, path safety
    /// and the empty-archive case, in that order, so a hostile upload cannot
    /// make the registry do expensive work before it rejects it.
    pub fn put(
        &self,
        name: &str,
        version: &str,
        bytes: &[u8],
        fingerprint: &str,
    ) -> ArchiveResult<StoredArchive> {
        if bytes.len() > self.max_bytes {
            return Err(ArchiveError::TooLarge(bytes.len()));
        }
        let files = pkgfmt::read_archive(bytes)
            .map_err(|e| ArchiveError::Malformed(e.message.clone()))?;
        if files.is_empty() {
            return Err(ArchiveError::NoFiles);
        }
        for f in &files {
            if f.rel_path.is_empty() || f.rel_path.contains("..") || f.rel_path.starts_with('/') {
                return Err(ArchiveError::UnsafePath(f.rel_path.clone()));
            }
        }
        let path = self.path(name, version);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ArchiveError::Io(format!("cannot create archive dir: {e}")))?;
        }
        std::fs::write(&path, bytes)
            .map_err(|e| ArchiveError::Io(format!("cannot write archive: {e}")))?;
        Ok(StoredArchive {
            integrity: format!("sha256:{}", security::sha256_hex(bytes)),
            fingerprint: fingerprint.to_string(),
            size: bytes.len() as u64,
            file_count: files.len(),
            files: files.into_iter().map(|f| f.rel_path).collect(),
        })
    }

    /// Delete one archive. Missing files are not an error.
    pub fn remove(&self, name: &str, version: &str) -> ArchiveResult<()> {
        let path = self.path(name, version);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ArchiveError::Io(format!("cannot remove archive: {e}"))),
        }
    }

    /// Remove every archive file no version row refers to. Returns the
    /// number of files removed.
    pub fn sweep(&self, referenced: &dyn Fn(&str, &str) -> bool) -> ArchiveResult<usize> {
        let mut removed = 0usize;
        for (name, version) in self.entries()? {
            if !referenced(&name, &version) {
                self.remove(&name, &version)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Every `(name, version)` pair present on disk, sorted.
    pub fn entries(&self) -> ArchiveResult<Vec<(String, String)>> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Ok(out);
        };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let Some(dir_name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            for f in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                let fname = f.file_name().to_string_lossy().into_owned();
                if let Some(rest) = fname.strip_prefix(&format!("{dir_name}-")) {
                    if let Some(v) = rest.strip_suffix(".hspkg") {
                        out.push((dir_name.to_string(), v.to_string()));
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Total bytes and file count across the archive store.
    pub fn usage(&self) -> (usize, u64) {
        let mut count = 0usize;
        let mut bytes = 0u64;
        for (name, version) in self.entries().unwrap_or_default() {
            if let Ok(m) = std::fs::metadata(self.path(&name, &version)) {
                count += 1;
                bytes += m.len();
            }
        }
        (count, bytes)
    }

    /// Read a byte range of an archive (used by ranged downloads).
    pub fn read_range(&self, name: &str, version: &str, start: u64, end: u64) -> ArchiveResult<Vec<u8>> {
        let bytes = self.read(name, version)?;
        let start = start.min(bytes.len() as u64) as usize;
        let end = (end as usize).min(bytes.len());
        if start > end {
            return Ok(Vec::new());
        }
        Ok(bytes[start..end].to_vec())
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_pm::pkgfmt::FileRecord;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("hs-archives-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn archive() -> Vec<u8> {
        pkgfmt::pack(&[
            FileRecord {
                rel_path: "hard.toml".to_string(),
                data: b"name = \"x\"\n".to_vec(),
            },
            FileRecord {
                rel_path: "src/main.hard".to_string(),
                data: b"<- { ok: true }".to_vec(),
            },
        ])
        .unwrap()
    }

    #[test]
    fn put_records_facts_and_read_back_matches() {
        let root = tmp("put");
        let s = ArchiveStore::at(&root);
        let bytes = archive();
        let facts = s.put("demo", "1.0.0", &bytes, "sha256:fp").unwrap();
        assert_eq!(facts.size, bytes.len() as u64);
        assert_eq!(facts.file_count, 2);
        assert_eq!(facts.files, vec!["hard.toml", "src/main.hard"]);
        assert!(facts.integrity.starts_with("sha256:"));
        assert!(s.has("demo", "1.0.0"));
        assert_eq!(s.read("demo", "1.0.0").unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_malformed_and_oversized() {
        let root = tmp("reject");
        let s = ArchiveStore::at(&root);
        assert!(matches!(
            s.put("demo", "1.0.0", b"not an archive", "f"),
            Err(ArchiveError::Malformed(_))
        ));
        let small = ArchiveStore::at(&root).with_max_bytes(4);
        assert!(matches!(
            small.put("demo", "1.0.0", &archive(), "f"),
            Err(ArchiveError::TooLarge(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_empty_archive() {
        let root = tmp("empty");
        let s = ArchiveStore::at(&root);
        let bytes = pkgfmt::pack(&[]).unwrap();
        assert_eq!(s.put("demo", "1.0.0", &bytes, "f"), Err(ArchiveError::NoFiles));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_traversal_archives() {
        let root = tmp("traverse");
        let s = ArchiveStore::at(&root);
        let bytes = pkgfmt::pack(&[FileRecord {
            rel_path: "../evil.txt".to_string(),
            data: b"x".to_vec(),
        }])
        .unwrap();
        // hs-pm's reader already refuses '..', so this is caught as malformed.
        assert!(s.put("demo", "1.0.0", &bytes, "f").is_err());
        assert!(!root.parent().unwrap().join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn entries_usage_and_sweep() {
        let root = tmp("sweep");
        let s = ArchiveStore::at(&root);
        s.put("a", "1.0.0", &archive(), "f").unwrap();
        s.put("b", "2.0.0", &archive(), "f").unwrap();
        assert_eq!(s.entries().unwrap().len(), 2);
        let (count, bytes) = s.usage();
        assert_eq!(count, 2);
        assert!(bytes > 0);
        let removed = s.sweep(&|n, v| n == "a" && v == "1.0.0").unwrap();
        assert_eq!(removed, 1);
        assert_eq!(s.entries().unwrap(), vec![("a".to_string(), "1.0.0".to_string())]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ranged_reads() {
        let root = tmp("range");
        let s = ArchiveStore::at(&root);
        let bytes = archive();
        s.put("demo", "1.0.0", &bytes, "f").unwrap();
        assert_eq!(s.read_range("demo", "1.0.0", 0, 3).unwrap(), bytes[0..3].to_vec());
        // clamped past the end
        assert_eq!(
            s.read_range("demo", "1.0.0", 0, 9999).unwrap().len(),
            bytes.len()
        );
        assert!(s.read_range("demo", "1.0.0", 5, 1).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_is_idempotent() {
        let root = tmp("remove");
        let s = ArchiveStore::at(&root);
        s.put("demo", "1.0.0", &archive(), "f").unwrap();
        s.remove("demo", "1.0.0").unwrap();
        s.remove("demo", "1.0.0").unwrap();
        assert!(!s.has("demo", "1.0.0"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
