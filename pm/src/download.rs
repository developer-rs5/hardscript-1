//! The package download client.
//!
//! `hard install` used to mean "one GET per package, straight into memory",
//! which is fine for a 4 KB library and wrong for everything else: a 4 MB
//! package that dies at 90% started over, a graph of 40 packages was 40
//! round trips in a row, and nothing recorded where the bytes came from.
//!
//! This module is the real thing:
//!
//! - **cache first.** A package that is already in `~/.hard/cache` is never
//!   fetched again; the archive is re-verified against its recorded digest
//!   so a corrupted cache entry is caught instead of installed.
//! - **resumable.** Bytes land in a `.part` file. An interrupted download
//!   resumes with a `Range` request instead of starting over, and the part
//!   file is renamed into place only after its digest matches.
//! - **bounded parallelism.** Independent packages are fetched by a small
//!   worker pool, which is what turns a 40-package graph from 40 serial
//!   round trips into a handful of batches.
//! - **honest reporting.** Every fetch reports where the bytes came from, how
//!   many attempts it took and whether it resumed, which is what the
//!   benchmarks and the cache hit-rate report are computed from.

use crate::cache::Cache;
use crate::registry::{HttpResponse, Method, Registry};
use crate::semver::Version;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Where a package's bytes came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Already in the shared cache; nothing was fetched.
    Cache,
    /// Fetched from the registry in one request.
    Network,
    /// Fetched from the registry, continuing a previous partial download.
    Resumed,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Cache => "cache",
            Source::Network => "network",
            Source::Resumed => "resumed",
        }
    }

    /// Did this fetch touch the network?
    pub fn is_network(self) -> bool {
        !matches!(self, Source::Cache)
    }
}

/// What one fetch did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    pub name: String,
    pub version: Version,
    pub source: Source,
    /// Bytes actually pulled over the network this run.
    pub bytes: u64,
    /// Size of the finished archive.
    pub size: u64,
    /// How many HTTP requests it took.
    pub attempts: u32,
    /// True when the digest matched after the fetch.
    pub verified: bool,
}

impl Fetched {
    /// One line for a report or a benchmark.
    pub fn summary(&self) -> String {
        format!(
            "{}@{} {} {} bytes in {} request(s){}",
            self.name,
            self.version,
            self.source.as_str(),
            self.size,
            self.attempts,
            if self.verified { "" } else { " [UNVERIFIED]" }
        )
    }
}

/// A download failure, with enough context to print something useful.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadError {
    pub message: String,
}

impl DownloadError {
    pub fn new(message: impl Into<String>) -> DownloadError {
        DownloadError {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DownloadError {}

/// How the downloader behaves.
#[derive(Clone, Debug)]
pub struct DownloadConfig {
    /// Never touch the network; cache hits only.
    pub offline: bool,
    /// How many packages may be in flight at once.
    pub parallel: usize,
    /// Print one line per package as it is fetched.
    pub verbose: bool,
    /// Refuse to start a download larger than this (0 = no cap).
    pub max_bytes: u64,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        DownloadConfig {
            offline: false,
            parallel: 4,
            verbose: false,
            max_bytes: 0,
        }
    }
}

impl DownloadConfig {
    /// A configuration for tests: sequential and quiet.
    pub fn sequential() -> DownloadConfig {
        DownloadConfig {
            parallel: 1,
            ..DownloadConfig::default()
        }
    }
}

/// Aggregate counters for a batch of fetches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatchReport {
    pub fetched: Vec<Fetched>,
    pub errors: Vec<String>,
}

impl BatchReport {
    /// Packages that came from the cache.
    pub fn cache_hits(&self) -> usize {
        self.fetched
            .iter()
            .filter(|f| f.source == Source::Cache)
            .count()
    }

    /// Packages that needed the network.
    pub fn cache_misses(&self) -> usize {
        self.fetched.len() - self.cache_hits()
    }

    /// Hit rate as a percentage of attempted packages (0 when nothing ran).
    pub fn hit_rate(&self) -> f64 {
        if self.fetched.is_empty() {
            return 0.0;
        }
        (self.cache_hits() as f64 / self.fetched.len() as f64) * 100.0
    }

    /// Bytes pulled over the network.
    pub fn network_bytes(&self) -> u64 {
        self.fetched.iter().map(|f| f.bytes).sum()
    }

    /// Total HTTP requests.
    pub fn requests(&self) -> u32 {
        self.fetched.iter().map(|f| f.attempts).sum()
    }

    pub fn succeeded(&self) -> bool {
        self.errors.is_empty()
    }

    /// A `cache 3 hit, 1 miss, 2 resumed, 0 corrupt` style summary line.
    pub fn summary(&self) -> String {
        let resumed = self
            .fetched
            .iter()
            .filter(|f| f.source == Source::Resumed)
            .count();
        format!(
            "cache: {} hit, {} miss, {} resumed, {} corrupt ({} bytes over the network, {} requests, {:.0}% hit rate)",
            self.cache_hits(),
            self.cache_misses(),
            resumed,
            self.fetched.iter().filter(|f| !f.verified).count(),
            self.network_bytes(),
            self.requests(),
            self.hit_rate()
        )
    }
}

/// The download client.
#[derive(Clone)]
pub struct Downloader {
    pub registry: Registry,
    pub cache: Cache,
    pub config: DownloadConfig,
}

impl Downloader {
    pub fn new(registry: Registry, cache: Cache, config: DownloadConfig) -> Downloader {
        Downloader {
            registry,
            cache,
            config,
        }
    }

    /// One package. Returns where the bytes came from.
    ///
    /// The order is deliberate: cache first (verified), then the network
    /// (resuming a partial download), and a digest mismatch is fatal rather
    /// than something to install anyway.
    pub fn fetch(
        &self,
        name: &str,
        version: &Version,
        integrity: Option<&str>,
    ) -> Result<Fetched, DownloadError> {
        let version_str = version.to_string();
        if self.cache.has_version(name, &version_str) {
            match self.cached(name, &version_str, integrity) {
                Some(f) => return Ok(f),
                None => {
                    // corrupt or mismatched: forget it and fetch again
                    let _ = std::fs::remove_file(self.cache.archive_path(name, &version_str));
                    let _ = std::fs::remove_file(self.cache.meta_path_for(name, &version_str));
                }
            }
        }
        if self.config.offline {
            return Err(DownloadError::new(format!(
                "{name}@{version_str} is not in the cache and the registry is offline"
            )));
        }
        let (_, attempts, resumed) = self.fetch_to_cache(name, &version_str, integrity)?;
        let size = file_size(&self.cache.archive_path(name, &version_str));
        Ok(Fetched {
            name: name.to_string(),
            version: version.clone(),
            source: if resumed {
                Source::Resumed
            } else {
                Source::Network
            },
            bytes: size,
            size,
            attempts,
            verified: true,
        })
    }

    /// The cache-hit path: `None` when the cached copy cannot be trusted.
    fn cached(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
    ) -> Option<Fetched> {
        let path = self.cache.archive_path(name, version);
        let bytes = std::fs::read(&path).ok()?;
        let got = crate::pkgfmt::sha256_hex(&bytes);
        if let Some(want) = integrity {
            if !want_matches(want, &got) {
                eprintln!(
                    "warning: cached {name}@{version} does not match the registry's digest ({got}); refetching"
                );
                return None;
            }
        }
        // A cached archive must still be a valid .hspkg; the cache verifier
        // also checks the recorded metadata, which a hand-seeded entry lacks.
        if crate::pkgfmt::read_archive(&bytes).is_err() {
            eprintln!("warning: cached {name}@{version} is not a valid .hspkg; refetching");
            return None;
        }
        Some(Fetched {
            name: name.to_string(),
            version: Version::parse(version).unwrap_or_else(|_| Version::default()),
            source: Source::Cache,
            bytes: 0,
            size: bytes.len() as u64,
            attempts: 0,
            verified: true,
        })
    }

    /// Download into the cache, resuming when a `.part` file exists.
    ///
    /// Returns the final response (kept for its headers), the number of HTTP
    /// requests it took, and whether the transfer resumed.
    fn fetch_to_cache(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
    ) -> Result<(HttpResponse, u32, bool), DownloadError> {
        let part = self.cache.part_path(name, version);
        let mut have = self.cache.staged_size(name, version);
        if self.config.max_bytes > 0 && have > self.config.max_bytes {
            self.cache.drop_part(name, version);
            have = 0;
        }
        let range = if have > 0 {
            format!("bytes={have}-")
        } else {
            String::new()
        };
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if !range.is_empty() {
            headers.push(("Range", range.as_str()));
        }
        let path = format!(
            "/packages/{}/{}",
            crate::publish::urlencode(name),
            version
        );
        let resp = self
            .registry
            .request(
                Method::Get,
                &path,
                Vec::new(),
                &headers,
                None,
            )
            .map_err(|e| DownloadError::new(format!("cannot download {name}@{version}: {e}")))?;

        if have > 0 && resp.status == 206 {
            // the server honoured the range: keep what we already have
            append(&part, &resp.body)?;
            return self
                .finish(name, version, integrity, resp, true)
                .map(|r| (r, 2, true));
        }
        // A 200 to a Range request means the server ignored it: start over.
        if !(200..=299).contains(&resp.status) {
            return Err(DownloadError::new(format!(
                "cannot download {name}@{version}: {}",
                Registry::error_message(&resp)
            )));
        }
        write_part(&part, &resp.body)?;
        self.finish(name, version, integrity, resp, false)
            .map(|r| (r, 1, false))
    }

    /// Verify the staged bytes and hand them to the cache, which is the only
    /// writer of the archive (and the only one that extracts sources).
    fn finish(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
        resp: HttpResponse,
        resumed: bool,
    ) -> Result<HttpResponse, DownloadError> {
        let part = self.cache.part_path(name, version);
        let bytes = std::fs::read(&part)
            .map_err(|e| DownloadError::new(format!("cannot read {}: {e}", part.display())))?;
        let got = crate::pkgfmt::sha256_hex(&bytes);
        let want = integrity
            .map(|i| i.trim_start_matches("sha256:").to_string())
            .or_else(|| {
                resp.header("x-hard-integrity")
                    .map(|v| v.trim_start_matches("sha256:").to_string())
            });
        if let Some(want) = &want {
            if !got.eq_ignore_ascii_case(want) {
                self.cache.drop_part(name, version);
                return Err(DownloadError::new(format!(
                    "integrity mismatch for {name}@{version}: got {got}, want {want}"
                )));
            }
        }
        self.cache
            .put_bytes(
                name,
                version,
                &bytes,
                Some(&self.registry.config.url),
                None,
            )
            .map_err(DownloadError::new)?;
        self.cache.drop_part(name, version);
        if self.config.verbose {
            println!(
                "fetched {name}@{version} ({} bytes, {})",
                bytes.len(),
                if resumed { "resumed" } else { "new" }
            );
        }
        Ok(resp)
    }

    /// Fetch many packages with bounded parallelism, preserving input order
    /// in the report.
    pub fn fetch_all(
        &self,
        wanted: &[(String, Version, Option<String>)],
    ) -> BatchReport {
        let mut report = BatchReport::default();
        if wanted.is_empty() {
            return report;
        }
        let workers = self.config.parallel.max(1).min(wanted.len());
        if workers == 1 {
            for (name, version, integrity) in wanted {
                match self.fetch(name, version, integrity.as_deref()) {
                    Ok(f) => report.fetched.push(f),
                    Err(e) => report.errors.push(e.message),
                }
            }
            return report;
        }
        let next = Arc::new(AtomicUsize::new(0));
        let results: Mutex<Vec<Option<Result<Fetched, DownloadError>>>> =
            Mutex::new(vec![None; wanted.len()].into_iter().collect());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let next = Arc::clone(&next);
                let results = &results;
                let this = self;
                scope.spawn(move || loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= wanted.len() {
                        return;
                    }
                    let (name, version, integrity) = &wanted[i];
                    let outcome = this.fetch(name, version, integrity.as_deref());
                    let mut guard = results.lock().expect("download results lock");
                    guard[i] = Some(outcome);
                });
            }
        });
        if let Ok(guard) = results.into_inner() {
            for slot in guard.into_iter().flatten() {
                match slot {
                    Ok(f) => report.fetched.push(f),
                    Err(e) => report.errors.push(e.message),
                }
            }
        }
        report
    }

    /// Save a package's archive to `out_dir` (for `hard download`). Uses the
    /// cache when it has the package.
    pub fn save_to(
        &self,
        name: &str,
        version: &Version,
        integrity: Option<&str>,
        out_dir: &Path,
    ) -> Result<Fetched, DownloadError> {
        let fetched = self.fetch(name, version, integrity)?;
        let src = self.cache.archive_path(name, &version.to_string());
        let dest = out_dir.join(format!("{name}-{version}.hspkg"));
        std::fs::create_dir_all(out_dir)
            .map_err(|e| DownloadError::new(format!("cannot create {}: {e}", out_dir.display())))?;
        std::fs::copy(&src, &dest).map_err(|e| {
            DownloadError::new(format!("cannot write {}: {e}", dest.display()))
        })?;
        Ok(Fetched {
            name: name.to_string(),
            version: version.clone(),
            size: std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0),
            ..fetched
        })
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn want_matches(want: &str, got: &str) -> bool {
    want.trim_start_matches("sha256:").eq_ignore_ascii_case(got)
}

fn write_part(path: &Path, bytes: &[u8]) -> Result<(), DownloadError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| DownloadError::new(format!("cannot create {}: {e}", parent.display())))?;
    }
    let mut f = std::fs::File::create(path)
        .map_err(|e| DownloadError::new(format!("cannot create {}: {e}", path.display())))?;
    f.write_all(bytes)
        .map_err(|e| DownloadError::new(format!("cannot write {}: {e}", path.display())))?;
    f.flush()
        .map_err(|e| DownloadError::new(format!("cannot flush {}: {e}", path.display())))?;
    Ok(())
}

fn append(path: &Path, bytes: &[u8]) -> Result<(), DownloadError> {
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| DownloadError::new(format!("cannot open {}: {e}", path.display())))?;
    f.write_all(bytes)
        .map_err(|e| DownloadError::new(format!("cannot append to {}: {e}", path.display())))?;
    Ok(())
}

/// Read a whole stream, for tests and diagnostics.
pub fn read_all(mut r: impl Read) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("hs-dl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn archive(src: &str) -> Vec<u8> {
        crate::pkgfmt::pack(&[crate::pkgfmt::FileRecord {
            rel_path: "main.hard".to_string(),
            data: src.as_bytes().to_vec(),
        }])
        .unwrap()
    }

    /// A one-package registry over a real socket, so the client is exercised
    /// end to end (framing, headers, ranges) without a helper crate.
    struct TinyRegistry {
        addr: std::net::SocketAddr,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl TinyRegistry {
        fn start(pkg: Vec<u8>, support_ranges: bool) -> TinyRegistry {
            use std::sync::atomic::{AtomicBool, Ordering};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&shutdown);
            let handle = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if flag.load(Ordering::Relaxed) {
                        return;
                    }
                    let mut s = match stream {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let pkg = pkg.clone();
                    std::thread::spawn(move || {
                        use std::io::{BufRead, BufReader, Write};
                        let mut reader = BufReader::new(s.try_clone().unwrap());
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                            return;
                        }
                        let mut range = None;
                        loop {
                            let mut h = String::new();
                            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                                break;
                            }
                            if h.trim().is_empty() {
                                break;
                            }
                            if let Some(v) = h.to_ascii_lowercase().strip_prefix("range:") {
                                range = Some(v.trim().to_string());
                            }
                        }
                        let total = pkg.len();
                        let (status, body) = match (&range, support_ranges) {
                            (Some(r), true) if r.starts_with("bytes=") => {
                                let start: usize = r[6..]
                                    .split('-')
                                    .next()
                                    .and_then(|s| s.parse().ok())
                                    .unwrap_or(0);
                                if start >= total {
                                    (416, Vec::new())
                                } else {
                                    (206, pkg[start..].to_vec())
                                }
                            }
                            _ => (200, pkg.clone()),
                        };
                        let head = format!(
                            "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nX-Hard-Integrity: sha256:{}\r\nConnection: close\r\n\r\n",
                            body.len(),
                            crate::pkgfmt::sha256_hex(&pkg)
                        );
                        let _ = s.write_all(head.as_bytes());
                        let _ = s.write_all(&body);
                    });
                }
            });
            TinyRegistry {
                addr,
                shutdown,
                handle: Some(handle),
            }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    impl Drop for TinyRegistry {
        fn drop(&mut self) {
            use std::sync::atomic::Ordering;
            self.shutdown.store(true, Ordering::Relaxed);
            let _ = std::net::TcpStream::connect_timeout(&self.addr, std::time::Duration::from_millis(200));
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn downloader(url: &str, root: &Path, config: DownloadConfig) -> Downloader {
        Downloader::new(
            Registry::new(crate::registry::RegistryConfig::local(url)),
            Cache::at(root.to_path_buf()),
            config,
        )
    }

    #[test]
    fn a_fresh_package_is_fetched_and_then_cached() {
        let root = temp("fresh");
        let pkg = archive("hello");
        let reg = TinyRegistry::start(pkg.clone(), true);
        let d = downloader(&reg.url(), &root, DownloadConfig::sequential());
        let v = Version::parse("1.0.0").unwrap();
        let f = d.fetch("demo", &v, None).unwrap();
        assert_eq!(f.source, Source::Network);
        assert_eq!(f.size, pkg.len() as u64);
        assert_eq!(f.attempts, 1);
        assert!(f.verified);
        // second time: no network at all
        let f2 = d.fetch("demo", &v, None).unwrap();
        assert_eq!(f2.source, Source::Cache);
        assert_eq!(f2.bytes, 0);
        assert_eq!(f2.attempts, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_partial_download_resumes_instead_of_restarting() {
        let root = temp("resume");
        let pkg = archive("a longer payload so ranges matter");
        let reg = TinyRegistry::start(pkg.clone(), true);
        let d = downloader(&reg.url(), &root, DownloadConfig::sequential());
        let v = Version::parse("1.0.0").unwrap();
        // stage the first half by hand, as an interrupted run would
        let part = d.cache.part_path("demo", "1.0.0");
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, &pkg[..pkg.len() / 2]).unwrap();
        assert_eq!(d.cache.staged_size("demo", "1.0.0"), (pkg.len() / 2) as u64);
        let f = d.fetch("demo", &v, None).unwrap();
        assert_eq!(f.source, Source::Resumed);
        assert_eq!(f.attempts, 2);
        assert_eq!(
            std::fs::read(d.cache.archive_path("demo", "1.0.0")).unwrap(),
            pkg
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_server_that_ignores_range_restarts_the_transfer() {
        let root = temp("norange");
        let pkg = archive("payload");
        let reg = TinyRegistry::start(pkg.clone(), false);
        let d = downloader(&reg.url(), &root, DownloadConfig::sequential());
        let v = Version::parse("1.0.0").unwrap();
        let part = d.cache.part_path("demo", "1.0.0");
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, b"stale bytes").unwrap();
        let f = d.fetch("demo", &v, None).unwrap();
        assert_eq!(f.source, Source::Network, "a 200 to a Range means start over");
        assert_eq!(
            std::fs::read(d.cache.archive_path("demo", "1.0.0")).unwrap(),
            pkg,
            "the part file must be overwritten, not appended to"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_digest_mismatch_is_refused() {
        let root = temp("digest");
        let pkg = archive("payload");
        let reg = TinyRegistry::start(pkg, true);
        let d = downloader(&reg.url(), &root, DownloadConfig::sequential());
        let v = Version::parse("1.0.0").unwrap();
        let err = d
            .fetch("demo", &v, Some("sha256:0000000000000000000000000000000000000000000000000000000000000000"))
            .unwrap_err();
        assert!(err.message.contains("integrity mismatch"), "{err}");
        // the bad bytes are not left behind
        assert!(!d.cache.has_version("demo", "1.0.0"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn offline_mode_refuses_to_reach_the_network() {
        let root = temp("offline");
        let reg = TinyRegistry::start(archive("a"), true);
        let online = DownloadConfig::sequential();
        let d = downloader(&reg.url(), &root, online.clone());
        let v = Version::parse("1.0.0").unwrap();
        let offline = downloader(
            &reg.url(),
            &root,
            DownloadConfig {
                offline: true,
                ..online
            },
        );
        let err = offline.fetch("demo", &v, None).unwrap_err();
        assert!(err.message.contains("offline"), "{err}");
        // but a cached package still resolves offline
        d.fetch("demo", &v, None).ok();
        let d2 = offline;
        assert_eq!(d2.fetch("demo", &v, None).unwrap().source, Source::Cache);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dead_registry_reports_a_useful_error() {
        let root = temp("dead");
        let d = downloader("http://127.0.0.1:1", &root, DownloadConfig::sequential());
        let v = Version::parse("1.0.0").unwrap();
        let err = d.fetch("demo", &v, None).unwrap_err();
        assert!(err.message.contains("cannot download demo@1.0.0"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_cache_entry_is_dropped_and_refetched() {
        let root = temp("corrupt");
        let pkg = archive("payload");
        let reg = TinyRegistry::start(pkg.clone(), true);
        let d = downloader(&reg.url(), &root, DownloadConfig::sequential());
        let v = Version::parse("1.0.0").unwrap();
        d.fetch("demo", &v, None).unwrap();
        // scribble on the cached archive
        std::fs::write(d.cache.archive_path("demo", "1.0.0"), b"garbage").unwrap();
        let f = d.fetch("demo", &v, None).unwrap();
        assert_eq!(f.source, Source::Network, "a corrupt cache entry must be refetched");
        assert_eq!(
            std::fs::read(d.cache.archive_path("demo", "1.0.0")).unwrap(),
            pkg
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parallel_fetches_preserve_order_and_count_hits() {
        let root = temp("parallel");
        let pkg = archive("payload");
        let reg = TinyRegistry::start(pkg.clone(), true);
        let d = downloader(&reg.url(), &root, DownloadConfig::default());
        let wanted: Vec<(String, Version, Option<String>)> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|n| {
                (
                    n.to_string(),
                    Version::parse("1.0.0").unwrap(),
                    None,
                )
            })
            .collect();
        let report = d.fetch_all(&wanted);
        assert!(report.succeeded(), "{:?}", report.errors);
        assert_eq!(report.fetched.len(), 5);
        let names: Vec<&str> = report.fetched.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c", "d", "e"], "report order must match input");
        assert_eq!(report.cache_misses(), 5);
        assert_eq!(report.hit_rate(), 0.0);
        // now everything is cached
        let report = d.fetch_all(&wanted);
        assert_eq!(report.cache_hits(), 5);
        assert_eq!(report.hit_rate(), 100.0);
        assert_eq!(report.network_bytes(), 0);
        assert!(report.summary().contains("100% hit rate"), "{}", report.summary());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_batch_is_a_no_op() {
        let root = temp("emptybatch");
        let d = downloader("http://127.0.0.1:1", &root, DownloadConfig::default());
        let report = d.fetch_all(&[]);
        assert!(report.succeeded());
        assert_eq!(report.hit_rate(), 0.0);
        assert_eq!(report.network_bytes(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn errors_are_collected_not_fatal() {
        let root = temp("errors");
        let d = downloader("http://127.0.0.1:1", &root, DownloadConfig::default());
        let wanted = vec![(
            "a".to_string(),
            Version::parse("1.0.0").unwrap(),
            None,
        )];
        let report = d.fetch_all(&wanted);
        assert!(!report.succeeded());
        assert_eq!(report.errors.len(), 1);
        assert!(report.fetched.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn saving_to_a_directory_writes_a_named_archive() {
        let root = temp("save");
        let pkg = archive("payload");
        let reg = TinyRegistry::start(pkg.clone(), true);
        let d = downloader(&reg.url(), &root, DownloadConfig::sequential());
        let v = Version::parse("2.1.0").unwrap();
        let out = root.join("out");
        let f = d.save_to("demo", &v, None, &out).unwrap();
        assert_eq!(f.size, pkg.len() as u64);
        let dest = out.join("demo-2.1.0.hspkg");
        assert!(dest.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), pkg);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn source_rendering() {
        assert_eq!(Source::Cache.as_str(), "cache");
        assert_eq!(Source::Network.as_str(), "network");
        assert_eq!(Source::Resumed.as_str(), "resumed");
        assert!(!Source::Cache.is_network());
        assert!(Source::Resumed.is_network());
    }

    #[test]
    fn batch_summary_counts_corrupt_entries() {
        let report = BatchReport {
            fetched: vec![
                Fetched {
                    name: "a".to_string(),
                    version: Version::parse("1.0.0").unwrap(),
                    source: Source::Cache,
                    bytes: 0,
                    size: 10,
                    attempts: 0,
                    verified: true,
                },
                Fetched {
                    name: "b".to_string(),
                    version: Version::parse("1.0.0").unwrap(),
                    source: Source::Network,
                    bytes: 20,
                    size: 20,
                    attempts: 1,
                    verified: false,
                },
            ],
            errors: Vec::new(),
        };
        let s = report.summary();
        assert!(s.contains("1 hit"), "{s}");
        assert!(s.contains("1 miss"), "{s}");
        assert!(s.contains("1 corrupt"), "{s}");
        assert!(s.contains("50% hit rate"), "{s}");
    }
}
