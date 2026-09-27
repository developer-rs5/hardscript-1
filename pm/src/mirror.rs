//! Mirrors, health checks and metadata sync.
//!
//! A registry that is down should not be a project that cannot build. A
//! manifest can name mirrors, and every read — metadata, search, download,
//! signature — is attempted against the default registry first and then each
//! mirror in priority order until one answers.
//!
//! ```toml
//! [registry]
//! default = "https://registry.hardscript.org"
//!
//! [[registry.mirror]]
//! url = "https://registry.eu.hardscript.org"
//! priority = 1
//! ```
//!
//! Three properties make the fallback honest rather than magical:
//!
//! - **A failure is remembered, not retried blindly.** A mirror that refused
//!   the connection is skipped for [`COOLDOWN`]; a mirror that answered with
//!   404 was asked a real question and is asked again next time.
//! - **Provenance is reported.** [`MirrorSet::succeeded_by`] says which
//!   registry actually answered, so a lockfile or an install report can name
//!   it instead of pretending everything came from the default.
//! - **Metadata sync is incremental.** Mirrors expose a change feed, so
//!   [`MirrorSet::sync`] fetches what changed since the last run rather than
//!   the whole index, and can seed a cold cache from a manifest listing.

use crate::cache::Cache;
use crate::manifest::{Manifest, MirrorSettings, RegistrySettings};
use crate::registry::{HttpResponse, Method, Registry, RegistryConfig, RegistryError};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a mirror that could not be reached is left alone.
pub const COOLDOWN: Duration = Duration::from_secs(30);

/// What `GET /health` said about one registry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Health {
    pub url: String,
    pub reachable: bool,
    /// Round-trip time of the health request, in milliseconds.
    pub latency_ms: u64,
    /// The registry's change sequence, which mirrors use to sync.
    pub seq: i64,
    /// Package count, when the registry reports one.
    pub packages: Option<i64>,
    /// The reason it is unreachable, when it is.
    pub error: Option<String>,
    /// True when the registry signed with its built-in test key.
    pub test_key: bool,
}

impl Health {
    pub fn is_healthy(&self) -> bool {
        self.reachable
    }

    /// One line for `hard registry health`.
    pub fn render(&self) -> String {
        if self.reachable {
            let mut s = format!(
                "{:<40} ok    {:>6} ms  seq {}",
                self.url, self.latency_ms, self.seq
            );
            if let Some(p) = self.packages {
                s.push_str(&format!("  {p} packages"));
            }
            if self.test_key {
                s.push_str("  [test key]");
            }
            s
        } else {
            format!(
                "{:<40} FAIL  {}",
                self.url,
                self.error.clone().unwrap_or_else(|| "unreachable".to_string())
            )
        }
    }
}

impl std::fmt::Display for Health {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render())
    }
}

/// Probe one registry.
pub fn probe(config: &RegistryConfig) -> Health {
    let mut h = Health {
        url: config.url.clone(),
        ..Health::default()
    };
    let registry = Registry::new(config.clone());
    let start = Instant::now();
    match registry.request(Method::Get, "/health", Vec::new(), &[], None) {
        Ok(resp) => {
            h.latency_ms = start.elapsed().as_millis() as u64;
            if resp.is_success() {
                h.reachable = true;
                if let Some(j) = resp.json() {
                    h.seq = j.get("seq").and_then(|v| v.as_num()).unwrap_or(0) as i64;
                    h.packages = j.get("packages").and_then(|v| v.as_num()).map(|n| n as i64);
                    h.test_key = j.get("test_key") == Some(&hs_compiler::json::Json::Bool(true));
                }
            } else {
                h.error = Some(Registry::error_message(&resp));
            }
        }
        Err(e) => {
            h.latency_ms = start.elapsed().as_millis() as u64;
            h.error = Some(e.message);
        }
    }
    h
}

/// Probe every registry in a set, in order, without falling back.
pub fn probe_all(set: &MirrorSet) -> Vec<Health> {
    set.configs().iter().map(probe).collect()
}

/// One registry in the set, with the bookkeeping that makes fallback cheap.
#[derive(Clone, Debug)]
struct Entry {
    config: RegistryConfig,
    /// The default registry is never skipped, even if it fails.
    primary: bool,
    disabled: bool,
    /// When this entry last refused a connection.
    failed_at: Option<Instant>,
    /// The error from that failure, kept for `hard registry list`.
    last_error: Option<String>,
}

/// The ordered set of registries a command may talk to.
#[derive(Clone, Debug)]
pub struct MirrorSet {
    entries: Vec<Entry>,
    /// The URL that answered the most recent successful request.
    served_by: Option<String>,
    /// Every URL that failed during that request, with the reason.
    failed: Vec<(String, String)>,
    /// When the set was built. Kept so a set that lives a long time (a REPL-ish
    /// command chain) does not treat a very old failure as recent.
    built_at: Instant,
}

impl MirrorSet {
    /// Build from a manifest's `[registry]` table plus the environment.
    ///
    /// Precedence matches [`RegistryConfig::resolve`]: the manifest's
    /// `default` wins, `HARD_REGISTRY` is the fallback for a manifest that
    /// names none, and the built-in registry is the last resort. Mirrors come
    /// from the manifest either way.
    pub fn from_manifest(manifest: Option<&Manifest>, offline: bool) -> MirrorSet {
        let settings = manifest.map(|m| m.registries.clone()).unwrap_or_default();
        MirrorSet::from_settings(&settings, offline)
    }

    /// Build from explicit settings.
    pub fn from_settings(settings: &RegistrySettings, offline: bool) -> MirrorSet {
        let default = settings
            .default
            .clone()
            .filter(|u| !u.trim().is_empty())
            .or_else(|| std::env::var("HARD_REGISTRY").ok().filter(|u| !u.trim().is_empty()))
            .unwrap_or_else(|| crate::registry::DEFAULT_REGISTRY.to_string());
        let mut cfg = RegistryConfig::resolve(Some(&default), offline);
        cfg.url = default.trim_end_matches('/').to_string();
        let mut entries = vec![Entry {
            config: cfg,
            primary: true,
            disabled: false,
            failed_at: None,
            last_error: None,
        }];
        let mut mirrors: Vec<(&MirrorSettings, usize)> = settings.mirrors.iter().zip(0..).collect();
        mirrors.sort_by_key(|(m, i)| (m.priority.unwrap_or(i64::MAX), *i));
        for (m, _) in mirrors {
            if m.url.trim().is_empty() {
                continue;
            }
            let mut c = RegistryConfig::resolve(Some(&m.url), offline);
            c.url = m.url.trim_end_matches('/').to_string();
            if entries.iter().any(|e| e.config.url == c.url) {
                // The default listed as a mirror is not a second copy of it.
                continue;
            }
            entries.push(Entry {
                config: c,
                primary: false,
                disabled: m.disabled,
                failed_at: None,
                last_error: None,
            });
        }
        MirrorSet {
            entries,
            served_by: None,
            failed: Vec::new(),
            built_at: Instant::now(),
        }
    }

    /// Add mirrors to an existing set, skipping any URL already in it.
    pub fn add_mirrors(&mut self, mirrors: Vec<crate::registry::RegistryConfig>) {
        for m in mirrors {
            let url = m.url.trim_end_matches('/').to_string();
            if url.is_empty() || self.entries.iter().any(|e| e.config.url == url) {
                continue;
            }
            self.entries.push(Entry {
                config: crate::registry::RegistryConfig {
                    url,
                    ..m
                },
                primary: false,
                disabled: false,
                failed_at: None,
                last_error: None,
            });
        }
    }

    /// A set with a single registry: what most tests and scripts want.
    pub fn single(config: RegistryConfig) -> MirrorSet {
        MirrorSet {
            entries: vec![Entry {
                primary: true,
                config,
                disabled: false,
                failed_at: None,
                last_error: None,
            }],
            served_by: None,
            failed: Vec::new(),
            built_at: Instant::now(),
        }
    }

    /// Every configured registry, primary first.
    pub fn configs(&self) -> Vec<RegistryConfig> {
        self.entries.iter().map(|e| e.config.clone()).collect()
    }

    /// The registry a command should try first.
    pub fn primary(&self) -> RegistryConfig {
        self.entries[0].config.clone()
    }

    pub fn urls(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.config.url.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.entries.iter().filter(|e| !e.disabled).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many mirrors (not counting the default) are usable.
    pub fn mirror_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| !e.primary && !e.disabled)
            .count()
    }

    /// The registry that answered the last successful request.
    pub fn served_by(&self) -> Option<&str> {
        self.served_by.as_deref()
    }

    /// `(url, reason)` for every registry that failed the last request.
    pub fn failed(&self) -> &[(String, String)] {
        &self.failed
    }

    /// A one-line note naming the mirror that answered, when it was not the
    /// default. Empty when the default answered, or when nothing did.
    /// How long this set has existed, for diagnostics.
    pub fn age(&self) -> Duration {
        self.built_at.elapsed()
    }

    pub fn provenance(&self) -> String {
        match (&self.served_by, &self.entries[0].config.url) {
            (Some(served), primary) if served != primary => {
                format!("served by mirror {served}")
            }
            _ => String::new(),
        }
    }

    /// The last error recorded for a URL.
    pub fn last_error(&self, url: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.config.url == url)
            .and_then(|e| e.last_error.as_deref())
    }

    /// Run `f` against each registry until one returns `Ok`.
    ///
    /// A `4xx` that is not 408/429 is *not* a reason to try the next mirror:
    /// the registry understood the question and answered "no such package", and
    /// a mirror would answer the same. Only transport failures and 5xx fall
    /// through, so a missing package fails fast instead of hammering mirrors.
    pub fn try_each<T, F>(&mut self, mut f: F) -> Result<T, RegistryError>
    where
        F: FnMut(&Registry) -> Result<T, RegistryError>,
    {
        self.served_by = None;
        self.failed.clear();
        let now = Instant::now();
        let mut last: Option<RegistryError> = None;
        let mut tried = 0usize;
        for i in 0..self.entries.len() {
            if self.entries[i].disabled {
                continue;
            }
            let cooling = self.entries[i]
                .failed_at
                .map(|t| now.duration_since(t) < COOLDOWN)
                .unwrap_or(false);
            if cooling && !self.entries[i].primary {
                self.failed.push((
                    self.entries[i].config.url.clone(),
                    "skipped: still in the failure cooldown".to_string(),
                ));
                continue;
            }
            tried += 1;
            let registry = Registry::new(self.entries[i].config.clone());
            match f(&registry) {
                Ok(v) => {
                    self.entries[i].failed_at = None;
                    self.entries[i].last_error = None;
                    self.served_by = Some(self.entries[i].config.url.clone());
                    return Ok(v);
                }
                Err(e) => {
                    let url = self.entries[i].config.url.clone();
                    if is_mirror_worthy(&e) {
                        self.entries[i].failed_at = Some(now);
                        self.entries[i].last_error = Some(e.message.clone());
                        self.failed.push((url, e.message.clone()));
                    } else {
                        // A real answer from a reachable registry: report it
                        // as-is rather than papering over it with a mirror.
                        self.served_by = Some(url);
                    }
                    last = Some(e);
                }
            }
        }
        if last.is_none() && tried == 0 {
            last = Some(RegistryError::other(
                "no registry is usable: every mirror is disabled or cooling down",
            ));
        }
        Err(last.unwrap_or_else(|| RegistryError::other("no registry answered")))
    }

    /// `GET` a path, falling back across mirrors.
    pub fn get(&mut self, path: &str) -> Result<HttpResponse, RegistryError> {
        self.try_each(|r| r.request(Method::Get, path, Vec::new(), &[], None))
    }

    /// Package metadata, from the first registry that has it.
    pub fn metadata(&mut self, name: &str) -> Result<crate::registry::PackageMeta, RegistryError> {
        self.try_each(|r| r.metadata(name))
    }

    /// Search, falling back across mirrors.
    pub fn search(&mut self, query: &crate::search::SearchQuery) -> crate::search::SearchResults {
        let mut out = crate::search::SearchResults {
            query: query.text.clone(),
            ..Default::default()
        };
        match self.try_each(|r| r.request(Method::Get, &query.path(), Vec::new(), &[], None)) {
            Ok(resp) => {
                if !resp.is_success() {
                    out.error = Some(Registry::error_message(&resp));
                    return out;
                }
                out.origin = Some(crate::search::Origin::Registry);
                // Reuse the client's parser so a mirror and the primary produce
                // identical results.
                if let Some(j) = resp.json() {
                    out.total = j.get("total").and_then(|v| v.as_num()).unwrap_or(0) as usize;
                    if let Some(hs_compiler::json::Json::Arr(items)) = j.get("results") {
                        for it in items {
                            let s = |k: &str| {
                                it.get(k)
                                    .and_then(|v| v.as_str())
                                    .map(String::from)
                            };
                            out.hits.push(crate::search::SearchHit {
                                name: s("name").unwrap_or_default(),
                                version: s("version"),
                                description: s("description"),
                                license: s("license"),
                                downloads: it
                                    .get("downloads")
                                    .and_then(|v| v.as_num())
                                    .unwrap_or(0)
                                    .max(0) as u64,
                                tags: Vec::new(),
                                keywords: Vec::new(),
                                owner: s("owner").filter(|o| !o.is_empty()),
                                score: it.get("score").and_then(|v| v.as_num()).unwrap_or(0),
                            });
                        }
                    }
                }
            }
            Err(e) => out.error = Some(e.message),
        }
        out
    }

    /// One version's signature record, from the first registry that has it.
    pub fn signature(
        &mut self,
        name: &str,
        version: &str,
    ) -> Result<crate::verify::SignatureRecord, RegistryError> {
        let path = format!("/packages/{name}/{version}/signature");
        let resp = self.get(&path)?;
        if !resp.is_success() {
            return Err(RegistryError::other(Registry::error_message(&resp)));
        }
        crate::verify::signature_from_response(&resp)
    }
}

/// Search every registry in turn, falling back to the cached index.
///
/// This is the mirror-aware form of [`crate::search::search_with_fallback`]:
/// the same degraded answer, whichever registry was being asked.
pub fn search_with_fallback(
    set: &mut MirrorSet,
    cache: &Cache,
    query: &crate::search::SearchQuery,
) -> crate::search::SearchResults {
    let live = set.search(query);
    crate::search::fallback_to_cache(live, cache, query)
}

/// Should a failure be retried against the next registry?
fn is_mirror_worthy(e: &RegistryError) -> bool {
    // A 404 (or any other definitive "no") is an answer. Only transport
    // failures, timeouts, 5xx and rate limits justify trying a mirror.
    match e.status {
        Some(code) => code >= 500 || code == 408 || code == 429,
        None => true,
    }
}

// -- metadata sync ---------------------------------------------------------

/// Where the sync state lives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncState {
    /// Last change sequence applied, per registry URL.
    pub seq: BTreeMap<String, i64>,
    /// When the last successful sync happened, per registry URL.
    pub synced_at: BTreeMap<String, i64>,
}

impl SyncState {
    pub fn path() -> PathBuf {
        crate::cache::hard_home().join("mirror-sync.toml")
    }

    pub fn load(path: &Path) -> SyncState {
        let Some(text) = std::fs::read_to_string(path).ok() else {
            return SyncState::default();
        };
        let Ok(doc) = crate::toml::parse(&text) else {
            return SyncState::default();
        };
        let mut out = SyncState::default();
        for t in doc.tables(&["sync", "registry"]) {
            let Some(url) = t.get("url").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(seq) = t.get("seq").and_then(|v| v.as_int()) {
                out.seq.insert(url.to_string(), seq);
            }
            if let Some(at) = t.get("synced_at").and_then(|v| v.as_int()) {
                out.synced_at.insert(url.to_string(), at);
            }
        }
        out
    }

    pub fn render(&self) -> String {
        let mut s = String::from("# mirror sync state (managed by `hard registry sync`)\n");
        for (url, seq) in &self.seq {
            s.push_str("\n[[sync.registry]]\n");
            s.push_str(&format!("url = \"{url}\"\n"));
            s.push_str(&format!("seq = {seq}\n"));
            if let Some(at) = self.synced_at.get(url) {
                s.push_str(&format!("synced_at = {at}\n"));
            }
        }
        s
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(path, self.render())
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

/// What a sync run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// The registry that was synced.
    pub url: String,
    /// The sequence before and after.
    pub from_seq: i64,
    pub to_seq: i64,
    /// Names touched by the change feed.
    pub changed: Vec<String>,
    /// Names seen in a full manifest sync.
    pub listed: usize,
    /// Index documents written into the cache.
    pub indexed: usize,
    /// Change-feed pages fetched.
    pub requests: usize,
    /// Milliseconds spent.
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

impl SyncReport {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }

    pub fn render(&self) -> String {
        if let Some(e) = &self.error {
            return format!("sync {}: {e}", self.url);
        }
        let mut s = format!(
            "synced {} in {} ms (seq {} -> {}, {} request(s))",
            self.url, self.elapsed_ms, self.from_seq, self.to_seq, self.requests
        );
        if !self.changed.is_empty() {
            s.push_str(&format!(
                "\n  changed: {}",
                self.changed.join(", ")
            ));
        }
        if self.listed > 0 {
            s.push_str(&format!("\n  listed {} package version(s)", self.listed));
        }
        if self.indexed > 0 {
            s.push_str(&format!("\n  wrote {} cache index document(s)", self.indexed));
        }
        s
    }
}

/// Refresh the cached index from a registry's mirror feeds.
///
/// `full` seeds from `/api/mirror/manifest` (a cold cache, or a mirror whose
/// sequence we do not have); otherwise the change feed is replayed from the
/// last sequence we applied. Index documents are written under
/// `$HARD_HOME/cache/index/`, which is exactly where `hard search --offline`
/// and `--offline` installs read them from.
pub fn sync(config: &RegistryConfig, cache: &Cache, state: &mut SyncState, full: bool) -> SyncReport {
    let start = Instant::now();
    let registry = Registry::new(config.clone());
    let mut report = SyncReport {
        url: config.url.clone(),
        from_seq: state.seq.get(&config.url).copied().unwrap_or(0),
        ..SyncReport::default()
    };

    if full || report.from_seq == 0 {
        match registry.request(Method::Get, "/api/mirror/manifest", Vec::new(), &[], None) {
            Ok(resp) if resp.is_success() => {
                report.requests += 1;
                if let Some(j) = resp.json() {
                    report.listed = j.get("count").and_then(|v| v.as_num()).unwrap_or(0) as usize;
                    report.to_seq = j.get("seq").and_then(|v| v.as_num()).unwrap_or(0) as i64;
                    if let Some(hs_compiler::json::Json::Arr(versions)) = j.get("versions") {
                        let mut names: Vec<String> = Vec::new();
                        for v in versions {
                            let Some(name) = v.get("name").and_then(|x| x.as_str()) else {
                                continue;
                            };
                            let name = name.to_string();
                            if names.last().map(String::as_str) == Some(name.as_str()) {
                                continue;
                            }
                            names.push(name);
                        }
                        for name in &names {
                            // A fresh document per package: the registry's own
                            // metadata, so offline installs see what is really
                            // published rather than a guess.
                            if let Ok(r) =
                                registry.request(Method::Get, &format!("/packages/{name}"), Vec::new(), &[], None)
                            {
                                if r.is_success() {
                                    if cache
                                        .write_index(name, &String::from_utf8_lossy(&r.body))
                                        .is_ok()
                                    {
                                        report.indexed += 1;
                                    }
                                }
                                report.requests += 1;
                            }
                        }
                    }
                }
            }
            Ok(resp) => {
                report.error = Some(Registry::error_message(&resp));
            }
            Err(e) => report.error = Some(e.message),
        }
    }

    if report.error.is_none() && !full {
        // Replay changes since the last sequence, following `seq` until the
        // feed stops advancing.
        let mut since = report.from_seq;
        for _ in 0..64 {
            let path = format!("/api/mirror/changes?since={since}&limit=200");
            let resp = match registry.request(Method::Get, &path, Vec::new(), &[], None) {
                Ok(r) => r,
                Err(e) => {
                    report.error = Some(e.message);
                    break;
                }
            };
            report.requests += 1;
            if !resp.is_success() {
                report.error = Some(Registry::error_message(&resp));
                break;
            }
            let Some(j) = resp.json() else {
                report.error = Some("the change feed was not JSON".to_string());
                break;
            };
            let next = j.get("seq").and_then(|v| v.as_num()).unwrap_or(since) as i64;
            if let Some(hs_compiler::json::Json::Arr(changes)) = j.get("changes") {
                for c in changes {
                    let Some(name) = c.get("name").and_then(|x| x.as_str()) else {
                        continue;
                    };
                    let name = name.to_string();
                    if !report.changed.contains(&name) {
                        report.changed.push(name.clone());
                    }
                    if let Ok(r) =
                        registry.request(Method::Get, &format!("/packages/{name}"), Vec::new(), &[], None)
                    {
                        if r.is_success() {
                            if cache.write_index(&name, &String::from_utf8_lossy(&r.body)).is_ok() {
                                report.indexed += 1;
                            }
                        }
                        report.requests += 1;
                    }
                }
            }
            if next <= since {
                report.to_seq = since.max(next);
                break;
            }
            since = next;
            report.to_seq = since;
        }
        if report.to_seq == 0 {
            report.to_seq = since;
        }
    }

    report.elapsed_ms = start.elapsed().as_millis() as u64;
    if report.ok() {
        state.seq.insert(config.url.clone(), report.to_seq);
        state
            .synced_at
            .insert(config.url.clone(), unix_now());
    }
    report
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// -- mirror verification ---------------------------------------------------

/// What comparing a mirror against the primary found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MirrorCheck {
    pub mirror: String,
    pub checked: usize,
    /// Packages the mirror is missing.
    pub missing: Vec<String>,
    /// Packages whose version list differs from the primary's.
    pub divergent: Vec<String>,
    /// Packages whose signature did not verify against the trust store.
    pub unverified: Vec<String>,
    /// Signatures that verified.
    pub verified: usize,
    pub error: Option<String>,
}

impl MirrorCheck {
    /// Does the mirror look like a faithful copy?
    pub fn is_clean(&self) -> bool {
        self.error.is_none()
            && self.missing.is_empty()
            && self.divergent.is_empty()
            && self.unverified.is_empty()
    }

    pub fn render(&self) -> String {
        let mut s = format!(
            "mirror {}: {} package(s) checked, {} verified",
            self.mirror, self.checked, self.verified
        );
        if let Some(e) = &self.error {
            s.push_str(&format!("\n  error: {e}"));
        }
        if !self.missing.is_empty() {
            s.push_str(&format!("\n  missing: {}", self.missing.join(", ")));
        }
        if !self.divergent.is_empty() {
            s.push_str(&format!(
                "\n  divergent versions: {}",
                self.divergent.join(", ")
            ));
        }
        if !self.unverified.is_empty() {
            s.push_str(&format!(
                "\n  unverified signatures: {}",
                self.unverified.join(", ")
            ));
        }
        if self.is_clean() {
            s.push_str("\n  clean");
        }
        s
    }
}

/// Compare a mirror's contents with the primary's.
///
/// "Verify" here means three things, all of them cheap enough to run by hand:
/// the mirror has the packages the primary has, their version lists agree, and
/// every signature it serves verifies against the local trust store. A mirror
/// that quietly serves a different version list is the failure mode worth
/// catching, because resolution would then depend on which host answered.
pub fn verify_mirror(
    primary: &RegistryConfig,
    mirror: &RegistryConfig,
    verifier: &crate::verify::Verifier,
    limit: usize,
) -> MirrorCheck {
    let mut out = MirrorCheck {
        mirror: mirror.url.clone(),
        ..MirrorCheck::default()
    };
    let p = Registry::new(primary.clone());
    let m = Registry::new(mirror.clone());
    let listing = match p.request(Method::Get, "/api/mirror/manifest", Vec::new(), &[], None) {
        Ok(r) if r.is_success() => r,
        Ok(r) => {
            out.error = Some(Registry::error_message(&r));
            return out;
        }
        Err(e) => {
            out.error = Some(e.message);
            return out;
        }
    };
    let Some(j) = listing.json() else {
        out.error = Some("the primary's manifest feed was not JSON".to_string());
        return out;
    };
    let mut names: Vec<String> = Vec::new();
    if let Some(hs_compiler::json::Json::Arr(versions)) = j.get("versions") {
        for v in versions {
            if let Some(n) = v.get("name").and_then(|x| x.as_str()) {
                if !names.iter().any(|x| x == n) {
                    names.push(n.to_string());
                }
            }
        }
    }
    names.sort();
    for name in names.iter().take(limit.max(1)) {
        out.checked += 1;
        let theirs = m.request(Method::Get, &format!("/packages/{name}"), Vec::new(), &[], None);
        let theirs = match theirs {
            Ok(r) if r.is_success() => r,
            Ok(_) | Err(_) => {
                out.missing.push(name.clone());
                continue;
            }
        };
        let mine = p.request(Method::Get, &format!("/packages/{name}"), Vec::new(), &[], None);
        if let Ok(r) = mine {
            if r.is_success() {
                if let (Some(a), Some(b)) = (r.json(), theirs.json()) {
                    if versions_of(&a) != versions_of(&b) {
                        out.divergent.push(name.clone());
                    }
                }
            }
        }
        // Signatures: the mirror must serve the same ones the primary does.
        if let Some(v) = theirs.json().as_ref().and_then(newest_version) {
            if let Ok(resp) =
                m.request(Method::Get, &format!("/packages/{name}/{v}/signature"), Vec::new(), &[], None)
            {
                if resp.is_success() {
                    if let Ok(rec) = crate::verify::signature_from_response(&resp) {
                        if verifier.verify(&rec).ok() {
                            out.verified += 1;
                        } else {
                            out.unverified.push(name.clone());
                        }
                    }
                }
            }
        }
    }
    out
}

fn versions_of(j: &hs_compiler::json::Json) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(hs_compiler::json::Json::Arr(vs)) = j.get("versions") {
        for v in vs {
            if let Some(s) = v.get("version").and_then(|x| x.as_str()) {
                out.push(s.to_string());
            }
        }
    }
    out.sort();
    out
}

fn newest_version(j: &hs_compiler::json::Json) -> Option<String> {
    let mut best: Option<crate::semver::Version> = None;
    if let Some(hs_compiler::json::Json::Arr(vs)) = j.get("versions") {
        for v in vs {
            let Some(text) = v.get("version").and_then(|x| x.as_str()) else {
                continue;
            };
            if v.get("yanked") == Some(&hs_compiler::json::Json::Bool(true)) {
                continue;
            }
            if let Ok(parsed) = crate::semver::Version::parse(text) {
                best = Some(match best {
                    Some(b) if b >= parsed => b,
                    _ => parsed,
                });
            }
        }
    }
    best.map(|v| v.to_string())
}

// -- cache verification and repair -----------------------------------------

/// Every cache entry whose bytes do not match what the registry said.
///
/// The cache knows its own layout, so it does the reporting; this is the
/// mirror-facing name for it.
pub fn audit_cache(cache: &Cache) -> Vec<crate::cache::CacheIssue> {
    cache.audit()
}

/// Remove every cache entry that fails its digest check.
pub fn repair_cache(cache: &Cache) -> Result<Vec<crate::cache::CacheIssue>, String> {
    cache.repair()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::RegistryConfig;

    fn cfg(url: &str) -> RegistryConfig {
        RegistryConfig::local(url)
    }

    fn settings(default: &str, mirrors: &[(&str, Option<i64>)]) -> RegistrySettings {
        RegistrySettings {
            default: Some(default.to_string()),
            mirrors: mirrors
                .iter()
                .map(|(u, p)| MirrorSettings {
                    url: u.to_string(),
                    priority: *p,
                    name: None,
                    disabled: false,
                })
                .collect(),
        }
    }

    fn dead() -> RegistryConfig {
        // Port 1 is reserved and refuses instantly.
        cfg("http://127.0.0.1:1")
    }

    #[test]
    fn a_set_puts_the_default_first() {
        let set = MirrorSet::from_settings(&settings("https://primary", &[("https://eu", Some(1))]), false);
        assert_eq!(set.urls(), vec!["https://primary", "https://eu"]);
        assert_eq!(set.primary().url, "https://primary");
        assert_eq!(set.mirror_count(), 1);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn mirrors_are_ordered_by_priority() {
        let set = MirrorSet::from_settings(
            &settings(
                "https://primary",
                &[("https://slow", Some(9)), ("https://fast", Some(1)), ("https://mid", Some(5))],
            ),
            false,
        );
        assert_eq!(
            set.urls(),
            vec!["https://primary", "https://fast", "https://mid", "https://slow"]
        );
    }

    #[test]
    fn a_trailing_slash_is_normalised_away() {
        let set = MirrorSet::from_settings(&settings("https://primary/", &[("https://eu/", None)]), false);
        assert_eq!(set.urls(), vec!["https://primary", "https://eu"]);
    }

    #[test]
    fn the_default_listed_as_a_mirror_is_not_duplicated() {
        let set = MirrorSet::from_settings(&settings("https://primary", &[("https://primary", None)]), false);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn the_manifest_default_wins_and_the_environment_is_the_fallback() {
        std::env::set_var("HARD_REGISTRY", "https://from-env");
        let with_default = MirrorSet::from_settings(
            &settings("https://primary", &[("https://eu", None)]),
            false,
        );
        assert_eq!(with_default.primary().url, "https://primary");
        assert_eq!(with_default.mirror_count(), 1);
        let no_default = MirrorSet::from_settings(
            &RegistrySettings {
                default: None,
                mirrors: vec![MirrorSettings {
                    url: "https://eu".to_string(),
                    priority: None,
                    name: None,
                    disabled: false,
                }],
            },
            false,
        );
        assert_eq!(no_default.primary().url, "https://from-env");
        std::env::remove_var("HARD_REGISTRY");
    }

    #[test]
    fn a_disabled_mirror_is_never_contacted() {
        let mut s = settings("http://127.0.0.1:1", &[("http://127.0.0.1:2", None), ("http://127.0.0.1:3", None)]);
        s.mirrors[1].disabled = true;
        let mut set = MirrorSet::from_settings(&s, false);
        let err = set.get("/health").unwrap_err();
        assert!(!err.message.is_empty());
        let urls: Vec<&str> = set.failed().iter().map(|(u, _)| u.as_str()).collect();
        assert!(urls.contains(&"http://127.0.0.1:1"), "{urls:?}");
        assert!(urls.contains(&"http://127.0.0.1:2"), "{urls:?}");
        assert!(!urls.contains(&"http://127.0.0.1:3"), "a disabled mirror is skipped: {urls:?}");
        assert_eq!(set.len(), 2, "a disabled mirror is not counted");
    }

    #[test]
    fn an_offline_set_refuses_to_build_a_request() {
        let set = MirrorSet::from_settings(&settings("https://primary", &[]), true);
        assert!(set.primary().offline);
    }

    #[test]
    fn a_single_set_has_no_mirrors() {
        let set = MirrorSet::single(cfg("http://127.0.0.1:1"));
        assert_eq!(set.len(), 1);
        assert_eq!(set.mirror_count(), 0);
        assert!(set.served_by().is_none());
        assert!(set.provenance().is_empty());
    }

    #[test]
    fn errors_carry_the_status_that_produced_them() {
        let e = RegistryError::with_status("nope", 404);
        assert_eq!(e.status, Some(404));
        assert_eq!(RegistryError::other("boom").status, None);
    }

    #[test]
    fn a_transport_failure_is_mirror_worthy() {
        assert!(is_mirror_worthy(&RegistryError::other("connection refused")));
        let mut e = RegistryError::other("boom");
        e.status = Some(503);
        assert!(is_mirror_worthy(&e));
        e.status = Some(429);
        assert!(is_mirror_worthy(&e), "a rate limit is worth another host");
        e.status = Some(404);
        assert!(!is_mirror_worthy(&e), "404 is an answer");
        e.status = Some(400);
        assert!(!is_mirror_worthy(&e));
    }

    #[test]
    fn a_set_reports_its_age() {
        let set = MirrorSet::single(cfg("http://127.0.0.1:1"));
        assert!(set.age() < Duration::from_secs(5), "{:?}", set.age());
    }

    #[test]
    fn provenance_names_the_mirror_that_answered() {
        let mut set = MirrorSet::from_settings(&settings("http://127.0.0.1:1", &[]), false);
        set.served_by = Some("http://127.0.0.1:9".to_string());
        assert!(set.provenance().contains("127.0.0.1:9"), "{}", set.provenance());
        set.served_by = Some("http://127.0.0.1:1".to_string());
        assert!(set.provenance().is_empty());
        set.served_by = None;
        assert!(set.provenance().is_empty());
    }

    #[test]
    fn health_of_a_dead_registry_reports_the_reason() {
        let h = probe(&dead());
        assert!(!h.is_healthy());
        assert!(h.error.is_some());
        assert!(h.render().contains("FAIL"), "{}", h.render());
        assert!(h.to_string().contains("FAIL"));
    }

    #[test]
    fn a_failed_mirror_is_remembered() {
        let mut set = MirrorSet::from_settings(&settings("http://127.0.0.1:1", &[]), false);
        let _ = set.get("/health");
        assert!(set.last_error("http://127.0.0.1:1").is_some());
        assert!(set.served_by().is_none());
    }

    #[test]
    fn every_registry_unreachable_is_an_error_not_a_panic() {
        let mut set = MirrorSet::from_settings(
            &settings("http://127.0.0.1:1", &[("http://127.0.0.1:2", None)]),
            false,
        );
        let err = set.get("/health").unwrap_err();
        assert!(!err.message.is_empty());
        assert_eq!(set.failed().len(), 2, "both failures are reported");
    }

    #[test]
    fn a_search_through_a_dead_set_falls_back_to_the_cache() {
        let dir = std::env::temp_dir().join(format!("hs-mirror-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = Cache::at(dir.clone());
        let j = hs_compiler::json::Json::obj(vec![
            ("name", hs_compiler::json::Json::str("jwt")),
            ("description", hs_compiler::json::Json::str("tokens")),
            ("latest", hs_compiler::json::Json::str("1.0.0")),
            ("tags", hs_compiler::json::Json::arr(vec![hs_compiler::json::Json::str("web")])),
        ]);
        cache.write_index("jwt", &j.to_string()).unwrap();
        let mut set = MirrorSet::from_settings(&settings("http://127.0.0.1:1", &[]), false);
        let r = search_with_fallback(&mut set, &cache, &crate::search::SearchQuery::new("jwt"));
        assert!(r.error.is_none(), "{:?}", r.error);
        assert!(r.degraded);
        assert_eq!(r.names(), vec!["jwt"]);
        // and the failure is remembered
        assert!(set.last_error("http://127.0.0.1:1").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_search_through_a_dead_set_reports_an_error() {
        let mut set = MirrorSet::from_settings(&settings("http://127.0.0.1:1", &[]), false);
        let r = set.search(&crate::search::SearchQuery::new("jwt"));
        assert!(r.error.is_some());
        assert!(r.hits.is_empty());
    }

    #[test]
    fn sync_state_round_trips() {
        let dir = std::env::temp_dir().join(format!("hs-mirror-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("mirror-sync.toml");
        let mut st = SyncState::default();
        assert!(SyncState::load(&path).seq.is_empty(), "a missing file is empty state");
        st.seq.insert("https://a".to_string(), 42);
        st.synced_at.insert("https://a".to_string(), 1_700_000_000);
        st.save(&path).unwrap();
        let back = SyncState::load(&path);
        assert_eq!(back, st);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_sync_state_does_not_crash() {
        let dir = std::env::temp_dir().join(format!("hs-mirror-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mirror-sync.toml");
        std::fs::write(&path, "this is not toml [").unwrap();
        assert!(SyncState::load(&path).seq.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sync_against_a_dead_registry_reports_the_error() {
        let cache = Cache::at(
            std::env::temp_dir().join(format!("hs-mirror-cache-{}", std::process::id())),
        );
        let mut state = SyncState::default();
        let report = sync(&dead(), &cache, &mut state, true);
        assert!(!report.ok());
        assert!(report.error.is_some());
        assert!(report.render().contains("sync"));
        assert!(state.seq.is_empty(), "a failed sync records no progress");
    }

    #[test]
    fn version_extraction_and_ordering() {
        let j = hs_compiler::json::parse(
            r#"{"versions":[{"version":"1.0.0"},{"version":"1.10.0"},{"version":"1.2.0","yanked":true}]}"#,
        )
        .unwrap();
        assert_eq!(versions_of(&j), vec!["1.0.0", "1.10.0", "1.2.0"]);
        assert_eq!(newest_version(&j).as_deref(), Some("1.10.0"));
        let empty = hs_compiler::json::parse("{}").unwrap();
        assert_eq!(newest_version(&empty), None);
    }

    #[test]
    fn a_clean_mirror_check_says_so() {
        let c = MirrorCheck {
            mirror: "https://eu".to_string(),
            checked: 2,
            verified: 2,
            ..MirrorCheck::default()
        };
        assert!(c.is_clean());
        assert!(c.render().contains("clean"));
    }

    #[test]
    fn a_dirty_mirror_check_lists_every_problem() {
        let c = MirrorCheck {
            mirror: "https://eu".to_string(),
            checked: 3,
            missing: vec!["a".to_string()],
            divergent: vec!["b".to_string()],
            unverified: vec!["c".to_string()],
            verified: 0,
            error: None,
        };
        assert!(!c.is_clean());
        let r = c.render();
        assert!(r.contains("missing: a"), "{r}");
        assert!(r.contains("divergent versions: b"), "{r}");
        assert!(r.contains("unverified signatures: c"), "{r}");
    }

    #[test]
    fn a_mirror_check_against_a_dead_primary_reports_the_error() {
        let v = crate::verify::Verifier::new(crate::verify::VerifyPolicy::Off);
        let c = verify_mirror(&dead(), &dead(), &v, 5);
        assert!(!c.is_clean());
        assert!(c.error.is_some());
    }

    #[test]
    fn mirrors_can_be_added_to_a_single_set() {
        let mut set = MirrorSet::single(cfg("http://127.0.0.1:1"));
        set.add_mirrors(vec![cfg("http://127.0.0.1:2"), cfg("http://127.0.0.1:1/")]);
        assert_eq!(set.urls(), vec!["http://127.0.0.1:1", "http://127.0.0.1:2"]);
        assert_eq!(set.mirror_count(), 1);
    }

    #[test]
    fn an_audit_of_an_empty_cache_finds_nothing() {
        let cache = Cache::at(
            std::env::temp_dir().join(format!("hs-mirror-empty-{}", std::process::id())),
        );
        assert!(audit_cache(&cache).is_empty());
        assert!(repair_cache(&cache).unwrap().is_empty());
    }
}
