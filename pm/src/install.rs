//! Install orchestration: `hard install/add/remove/update/list/outdated`.
//!
//! This is the entry point the CLI calls. It owns the "fetch or reuse from
//! cache -> resolve -> lock -> project install" pipeline shared by every
//! package-manager command.

use crate::cache::Cache;
use crate::download::{BatchReport, DownloadConfig, Downloader};
use crate::lockfile::{Lockfile, LOCKFILE_SCHEMA};
use crate::manifest::Manifest;
use crate::registry::{Registry, RegistryConfig};
use crate::resolver::{
    self, DepKind, Index, IndexEntry, Resolution, ResolveError, ResolvedPackage, SourceKind,
};
use crate::semver::{Version, VersionReq};
use crate::verify::{self, SignatureRecord, Status, Verifier, VerifyPolicy};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Everything `hard install` needs to know.
#[derive(Clone, Debug)]
pub struct InstallConfig {
    pub manifest: Manifest,
    pub project_root: PathBuf,
    pub offline: bool,
    pub verbose: bool,
    pub toolchain: String,
    pub compiler_version: String,
    pub registry: Registry,
    pub cache: Cache,
    /// Resolve only against the cache; never touch the registry.
    pub frozen: bool,
    /// Ignore the lockfile when choosing versions (`hard update`).
    pub update: bool,
    /// How many packages may download at once.
    pub parallel: usize,
    /// How to treat packages whose signature does not verify.
    ///
    /// `None` means "not specified", which resolves to
    /// [`VerifyPolicy::Warn`]. Off is not the default on purpose: a package
    /// manager that cannot tell you a package is unverified is not much better
    /// than one that does not check at all.
    pub verify: Option<VerifyPolicy>,
}

/// The result of an install/update/remove cycle.
#[derive(Clone, Debug, Default)]
pub struct InstallReport {
    pub resolved: Vec<ResolvedPackage>,
    /// Packages actually downloaded this run.
    pub fetched: Vec<(String, Version)>,
    /// Packages reused from the shared cache.
    pub reused: Vec<(String, Version)>,
    pub lock_path: PathBuf,
    pub lock_written: bool,
    /// ASCII dependency graph (deterministic).
    pub graph: String,
    pub errors: Vec<String>,
    /// Download counters: hits, misses, bytes and requests.
    pub downloads: BatchReport,
    /// One line per package whose signature was checked.
    pub verification: Vec<verify::Outcome>,
    /// Signature checks that were refused but did not stop the install
    /// (the `warn` policy). Printed as warnings, never as errors.
    pub verification_warnings: Vec<String>,
    /// How many packages checked out as verified.
    pub verified: usize,
    /// The policy that was in force.
    pub verify_policy: VerifyPolicy,
}

impl InstallReport {
    pub fn succeeded(&self) -> bool {
        self.errors.is_empty()
    }

    /// Packages that were checked and did not verify.
    pub fn unverified(&self) -> Vec<&verify::Outcome> {
        self.verification
            .iter()
            .filter(|o| !matches!(o.status, verify::Status::Verified | verify::Status::Skipped))
            .collect()
    }
}

/// Build the deterministic ASCII graph for a resolution.
pub fn render_graph(resolution: &Resolution) -> String {
    let mut out = String::new();
    for p in &resolution.packages {
        if p.deps.is_empty() {
            out.push_str(&format!("{} {}\n", p.name, p.version));
        } else {
            out.push_str(&format!("{} {}\n", p.name, p.version));
            for d in &p.deps {
                out.push_str(&format!("  └─ {d}\n"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// index construction
// ---------------------------------------------------------------------------

/// Build a resolver index for every dependency of the manifest (direct +
/// dev), traversing transitive dependencies breadth-first. Uses cached
/// metadata first, then the registry when online.
fn build_index(cfg: &InstallConfig) -> Result<Index, Vec<String>> {
    let mut index: Index = BTreeMap::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = Vec::new();
    queue.extend(cfg.manifest.dependencies.keys().cloned());
    queue.extend(cfg.manifest.dev_dependencies.keys().cloned());

    while let Some(name) = queue.pop() {
        if seen.contains(&name) {
            continue;
        }
        seen.insert(name.clone());
        let cached = cached_index_entries(&cfg.cache, &name);
        let meta = if !cfg.offline && !cfg.frozen {
            match cfg.registry.metadata(&name) {
                Ok(meta) => {
                    // refresh the cache's index copy
                    if let Ok(s) = cfg.registry.get(&format!("/packages/{}", name)) {
                        if (200..=299).contains(&s.status) {
                            let _ = cfg.cache.write_index(&name, &String::from_utf8_lossy(&s.body));
                        }
                    }
                    Some(meta)
                }
                Err(_) if !cached.is_empty() => None,
                Err(e) => return Err(vec![format!("cannot resolve '{name}': {e}")]),
            }
        } else {
            None
        };
        let entries = match meta {
            Some(m) => registry_entries(&name, &m),
            None if !cached.is_empty() => cached,
            None => {
                return Err(vec![format!(
                    "cannot resolve '{name}': not in the cache and the registry is{}",
                    if cfg.offline { " offline" } else { " unavailable" }
                )]);
            }
        };
        for e in &entries {
            // Only follow the edges a consumer of the package must also
            // resolve. A dependency's dev-dependencies belong to its own test
            // suite, and fetching metadata for them used to be why installing
            // a perfectly good package could fail.
            for (dep, _, kind) in e.all_deps() {
                if !kind.transitive() {
                    continue;
                }
                if !seen.contains(dep) {
                    queue.push(dep.clone());
                }
            }
        }
        index.insert(name.clone(), entries);
    }
    Ok(index)
}

/// Registry `kind` strings mapped onto resolver kinds.
fn cached_kinds(kinds: &BTreeMap<String, String>) -> BTreeMap<String, DepKind> {
    kinds
        .iter()
        .map(|(n, k)| {
            (
                n.clone(),
                match k.as_str() {
                    "dev" => DepKind::Dev,
                    "build" => DepKind::Build,
                    _ => DepKind::Normal,
                },
            )
        })
        .collect()
}

fn cached_index_entries(cache: &Cache, name: &str) -> Vec<IndexEntry> {
    let mut out = Vec::new();
    if let Some(index) = cache.read_index(name) {
        if let Some(j) = hs_compiler::json::parse(&index) {
            if let Some(arr) = j.get("versions").and_then(|x| x.as_arr()) {
                for v in arr {
                    let Some(vs) = v.get("version").and_then(|x| x.as_str()) else {
                        continue;
                    };
                    let Ok(ver) = Version::parse(vs) else {
                        continue;
                    };
                    let mut deps = BTreeMap::new();
                    let mut kinds = BTreeMap::new();
                    if let Some(d) = v.get("dependencies").and_then(|x| x.as_arr()) {
                        for item in d {
                            if let (Some(dn), Some(dr)) = (
                                item.get("name").and_then(|x| x.as_str()),
                                item.get("req").and_then(|x| x.as_str()),
                            ) {
                                kinds.insert(
                                    dn.to_string(),
                                    item.get("kind")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("normal")
                                        .to_string(),
                                );
                                deps.insert(dn.to_string(), dr.to_string());
                            }
                        }
                    }
                    out.push(IndexEntry {
                        version: ver,
                        kinds: cached_kinds(&kinds),
                        deps,
                        integrity: v.get("integrity").and_then(|x| x.as_str()).map(String::from),
                        description: v.get("description").and_then(|x| x.as_str()).map(String::from),
                    });
                }
            }
        }
    }
    out
}

fn registry_entries(name: &str, meta: &crate::registry::PackageMeta) -> Vec<IndexEntry> {
    let _ = name;
    meta.versions
        .iter()
        .map(|v| IndexEntry {
            version: v.version.clone(),
            kinds: v.dependency_kinds(),
            deps: v.dependencies.clone(),
            integrity: v.integrity.clone(),
            description: v.description.clone(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// lockfile-driven resolution
// ---------------------------------------------------------------------------

/// Does the lockfile already satisfy the manifest? If so, produces a
/// resolution pinned to the locked versions without any network access.
fn locked_resolution(
    manifest: &Manifest,
    lock: &Lockfile,
) -> Option<Resolution> {
    let mut deps: BTreeMap<String, String> = BTreeMap::new();
    deps.extend(manifest.dependencies.clone());
    deps.extend(manifest.dev_dependencies.clone());

    for (name, req) in &deps {
        let Some(locked) = lock.version_of(name) else {
            return None;
        };
        let req = VersionReq::parse(req).ok()?;
        if !req.matches(locked) {
            return None;
        }
    }

    // Topologically order the locked graph: a package precedes its
    // dependents. Lock entries already form a consistent DAG.
    let mut order: Vec<String> = Vec::new();
    let mut visiting: BTreeSet<String> = BTreeSet::new();
    let mut done: BTreeSet<String> = BTreeSet::new();
    for p in &lock.packages {
        topo_lock(&lock, &p.name, &mut visiting, &mut done, &mut order)?;
    }

    let by_name: BTreeMap<&str, &crate::lockfile::LockedPackage> = lock
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();

    let mut packages = Vec::new();
    for name in order {
        let p = by_name.get(name.as_str())?;
        packages.push(ResolvedPackage {
            name: name.clone(),
            version: p.version.clone(),
            req: manifest
                .dependencies
                .get(&name)
                .or_else(|| manifest.dev_dependencies.get(&name))
                .cloned()
                .unwrap_or_else(|| "*".to_string()),
            required_by: "lockfile".to_string(),
            deps: p.deps.clone(),
            integrity: p.integrity.clone(),
            source: if p.source == "workspace" {
                SourceKind::Workspace
            } else {
                SourceKind::Registry
            },
            // Trust recorded in the lockfile is carried forward, so an install
            // that trusts the lockfile does not silently forget what it knew.
            signature: p.signature.clone(),
            key_id: p.key_id.clone(),
            fingerprint: p.fingerprint.clone(),
            verified: p.verified,
        });
    }
    let graph = lock
        .packages
        .iter()
        .map(|p| (p.name.clone(), p.deps.clone()))
        .collect();
    Some(Resolution { packages, graph })
}

fn topo_lock(
    lock: &Lockfile,
    name: &str,
    visiting: &mut BTreeSet<String>,
    done: &mut BTreeSet<String>,
    order: &mut Vec<String>,
) -> Option<()> {
    if done.contains(name) {
        return Some(());
    }
    if visiting.contains(name) {
        // A cycle in the lock is a hard error surface.
        return None;
    }
    visiting.insert(name.to_string());
    let p = lock.packages.iter().find(|p| p.name == name)?;
    for d in &p.deps {
        topo_lock(lock, d, visiting, done, order)?;
    }
    visiting.remove(name);
    done.insert(name.to_string());
    order.push(name.to_string());
    Some(())
}

// ---------------------------------------------------------------------------
// main entry points
// ---------------------------------------------------------------------------

/// `hard install` — resolve dependencies and materialize `hard.lock`.
pub fn install(cfg: &InstallConfig) -> InstallReport {
    let mut report = InstallReport::default();
    report.lock_path = cfg.project_root.join("hard.lock");

    let existing_lock = Lockfile::parse(&std::fs::read_to_string(&report.lock_path).unwrap_or_default());
    let lock = match existing_lock {
        Ok(lock) => lock,
        Err(_) => Lockfile::parse(&empty_lockfile(cfg)).unwrap_or_else(|_| Lockfile::default()),
    };

    let preferred = locked_versions(&lock);
    let mut resolution: Resolution = if !cfg.offline || lock.packages.is_empty() {
        match fresh_resolve(cfg, preferred) {
            Ok(r) => r,
            Err(errors) => {
                report.errors = errors;
                return report;
            }
        }
    } else if let Some(r) = locked_resolution(&cfg.manifest, &lock) {
        r
    } else {
        match fresh_resolve(cfg, preferred) {
            Ok(r) => r,
            Err(errors) => {
                report.errors = errors;
                return report;
            }
        }
    };

    // Materialize: fetch everything that is not already cached (in
    // parallel, bounded by cfg.parallel), then link it into the project.
    let wanted: Vec<(String, Version, Option<String>)> = resolution
        .packages
        .iter()
        .filter(|p| p.source != SourceKind::Workspace)
        .map(|p| (p.name.clone(), p.version.clone(), p.integrity.clone()))
        .collect();
    let downloader = Downloader::new(
        cfg.registry.clone(),
        cfg.cache.clone(),
        DownloadConfig {
            offline: cfg.offline || cfg.frozen,
            parallel: cfg.parallel,
            verbose: cfg.verbose,
            max_bytes: 0,
        },
    );
    let batch = downloader.fetch_all(&wanted);
    for f in &batch.fetched {
        match f.source {
            crate::download::Source::Cache => report.reused.push((f.name.clone(), f.version.clone())),
            _ => report.fetched.push((f.name.clone(), f.version.clone())),
        }
    }
    // A failed download is a failed install: report every one of them, not
    // just the first, and do not write a lockfile for a partial tree.
    let download_errors = batch.errors.clone();
    report.downloads = batch;
    report.errors.extend(download_errors);
    if !report.errors.is_empty() {
        return report;
    }

    // Verify before anything is linked: an unverified package must never end
    // up in the project's package directory.
    let verifier = verifier_for(cfg);
    report.verify_policy = verifier.policy;
    verify_resolution(cfg, &verifier, &mut report, &mut resolution);
    if !report.errors.is_empty() {
        return report;
    }

    for p in &resolution.packages {
        if let Err(e) = link_package(cfg, p) {
            report.errors.push(e);
        }
    }
    if !report.errors.is_empty() {
        return report;
    }

    let new_lock = Lockfile::from_resolution(
        &resolution,
        &cfg.toolchain,
        &cfg.compiler_version,
        &crate::registry::platform(),
    );
    match std::fs::write(&report.lock_path, new_lock.render()) {
        Ok(()) => report.lock_written = true,
        Err(e) => report.errors.push(format!("cannot write {}: {e}", report.lock_path.display())),
    }
    report.resolved = resolution.packages.clone();
    report.graph = render_graph(&resolution);
    report
}

/// The verifier for an install: the configured policy plus the trust store.
fn verifier_for(cfg: &InstallConfig) -> Verifier {
    let policy = cfg.verify.unwrap_or_default();
    Verifier::with_store(policy, verify::TrustStore::load_default())
}

/// Check every registry package in a resolution, and record the result.
///
/// The signature comes from the registry, the trust decision from the local
/// store, and the archive digest from the bytes the downloader verified. Under
/// `strict` anything that is not `verified` is an error, so the install stops
/// before linking; under `warn` the same findings become warnings and the
/// install continues. `off` costs one no-op per package.
fn verify_resolution(
    cfg: &InstallConfig,
    verifier: &Verifier,
    report: &mut InstallReport,
    resolution: &mut Resolution,
) {
    if verifier.policy == VerifyPolicy::Off {
        for p in &mut resolution.packages {
            if p.source == SourceKind::Workspace {
                continue;
            }
            p.verified = false;
        }
        return;
    }
    let offline = cfg.offline || cfg.frozen;
    for i in 0..resolution.packages.len() {
        if resolution.packages[i].source == SourceKind::Workspace {
            continue;
        }
        let (name, version) = {
            let p = &resolution.packages[i];
            (p.name.clone(), p.version.to_string())
        };
        // Offline installs have no registry to ask, so they fall back to the
        // trust record cached next to the archive. Absence is reported, not
        // silently treated as verified.
        let record = if offline {
            cached_signature(cfg, &name, &version)
        } else {
            SignatureRecord::fetch(&cfg.registry, &name, &version).ok()
        };
        let outcome = match record.as_ref() {
            Some(rec) => {
                let archive = cfg
                    .cache
                    .archive_path(&name, &version);
                match std::fs::read(&archive) {
                    Ok(bytes) if cfg.cache.has_version(&name, &version) => {
                        verifier.verify_downloaded(rec, &bytes)
                    }
                    _ => verifier.verify(rec),
                }
            }
            None => verify::Outcome {
                name: name.clone(),
                version: version.clone(),
                status: Status::Unsigned,
                key_id: String::new(),
                integrity: resolution.packages[i].integrity.clone().unwrap_or_default(),
                payload_hash: String::new(),
                detail: if offline {
                    "offline: no cached signature to check".to_string()
                } else {
                    "the registry has no signature for this version".to_string()
                },
            },
        };
        // Keep the record beside the archive so a later `--offline` install
        // can re-verify without the registry. It is re-checked every time, so
        // a poisoned cache file cannot make anything pass.
        if let Some(rec) = &record {
            save_cached_signature(cfg, rec);
        }
        let entry = &mut resolution.packages[i];
        entry.signature = match &record {
            Some(r) if !r.signature.is_empty() => Some(r.signature.clone()),
            _ => None,
        };
        entry.key_id = match &record {
            Some(r) if !r.key_id.is_empty() => Some(r.key_id.clone()),
            _ => None,
        };
        entry.fingerprint = match &record {
            Some(r) if !r.fingerprint.is_empty() => Some(r.fingerprint.clone()),
            _ => None,
        };
        entry.verified = outcome.ok();
        if outcome.ok() {
            report.verified += 1;
        } else {
            // Strict stops the install; warn records a line the CLI prints.
            // Either way the finding is not swallowed.
            let line = outcome.render();
            if verifier.policy.is_fatal() {
                report.errors.push(line);
            } else {
                report.verification_warnings.push(line);
            }
        }
        report.verification.push(outcome);
    }
    report.verification.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.version.cmp(&b.version))
    });
}

/// Save a signature record in the cache for offline verification.
fn save_cached_signature(cfg: &InstallConfig, record: &verify::SignatureRecord) {
    let dir = cfg.cache.root.join("signatures");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{}@{}.json", record.name, record.version));
    let _ = std::fs::write(path, format!("{}\n", record.to_json().to_string()));
}

/// A signature record saved in the cache by an earlier verified install.
fn cached_signature(cfg: &InstallConfig, name: &str, version: &str) -> Option<SignatureRecord> {
    let path = cfg
        .cache
        .root
        .join("signatures")
        .join(format!("{name}@{version}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    SignatureRecord::from_sidecar(&text).ok()
}

fn empty_lockfile(cfg: &InstallConfig) -> String {
    let l = Lockfile {
        schema: LOCKFILE_SCHEMA.to_string(),
        toolchain: cfg.toolchain.clone(),
        compiler_version: cfg.compiler_version.clone(),
        platform: crate::registry::platform(),
        packages: Vec::new(),
    };
    l.render()
}

/// Resolve from scratch (registry + cache). Errors carry resolver details.
///
/// The previous lockfile is passed to the resolver as a *preference*: locked
/// versions are tried first, so re-resolving an unchanged project
/// reproduces the same lockfile, and adding one dependency does not bump
/// everything else.
fn fresh_resolve(cfg: &InstallConfig, preferred: BTreeMap<String, Version>) -> Result<Resolution, Vec<String>> {
    let index = build_index(cfg)?;
    let mut roots: BTreeMap<String, String> = BTreeMap::new();
    roots.extend(cfg.manifest.dependencies.clone());
    roots.extend(cfg.manifest.dev_dependencies.clone());
    // `hard update` deliberately ignores the lockfile: its whole job is to
    // move off the locked versions.
    let options = if cfg.update {
        resolver::ResolveOptions::default()
    } else {
        resolver::ResolveOptions::default().with_lock(preferred)
    };
    resolver::resolve_with(&index, &roots, &options).map_err(resolver_errors)
}

/// The `name -> version` map an existing lockfile pins.
fn locked_versions(lock: &Lockfile) -> BTreeMap<String, Version> {
    lock.as_map()
}

fn resolver_errors(e: ResolveError) -> Vec<String> {
    let mut v = vec![e.message];
    if let Some(d) = e.detail {
        v.push(d);
    }
    v
}

/// Copy a cached package's extracted source into `.hard/packages/<name>` and
/// update the marker metadata.
fn link_package(cfg: &InstallConfig, p: &ResolvedPackage) -> Result<(), String> {
    let version_str = p.version.to_string();
    let src = cfg.cache.source_dir(&p.name, &version_str);
    let dest = cfg
        .project_root
        .join(".hard")
        .join("packages")
        .join(&p.name);
    if dest.exists() {
        let _ = std::fs::remove_dir_all(&dest);
    }
    // Always materialize the directory, even when the cached source tree is
    // missing: the marker below has to land somewhere, and an empty
    // `.hard/packages/<name>` is a clearer signal than a write error.
    std::fs::create_dir_all(&dest)
        .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    copy_tree(&src, &dest).map_err(|e| {
        format!(
            "cannot install {}@{version_str} into {}: {e}",
            p.name,
            dest.display()
        )
    })?;
    let marker = hs_compiler::json::Json::obj(vec![
        ("name", hs_compiler::json::Json::str(&p.name)),
        ("version", hs_compiler::json::Json::str(&version_str)),
        ("integrity", match &p.integrity {
            Some(i) => hs_compiler::json::Json::str(i),
            None => hs_compiler::json::Json::Null,
        }),
    ]);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create dir: {e}"))?;
    }
    std::fs::write(dest.join(".hs-pkg.json"), marker.to_string())
        .map_err(|e| format!("cannot write marker: {e}"))?;
    Ok(())
}

fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    if !src.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dest)?;
    let mut stack = vec![(src.to_path_buf(), dest.to_path_buf())];
    while let Some((s, d)) = stack.pop() {
        for entry in std::fs::read_dir(&s)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let from = entry.path();
            let to = d.join(entry.file_name());
            if ft.is_dir() {
                std::fs::create_dir_all(&to)?;
                stack.push((from, to));
            } else {
                std::fs::copy(&from, &to)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// add / remove / update / list / outdated / info / search
// ---------------------------------------------------------------------------

/// Split `name@req` (or `name:req`) into its parts.
pub fn parse_add_spec(spec: &str) -> (String, Option<String>) {
    for sep in ['@', ':'] {
        if let Some((n, r)) = spec.split_once(sep) {
            return (n.trim().to_string(), Some(r.trim().to_string()));
        }
    }
    (spec.trim().to_string(), None)
}

/// Re-run install and bind an outcome to a fresh manifest.
pub fn run(cfg: InstallConfig) -> InstallReport {
    install(&cfg)
}

/// Human-readable list of installed packages (deterministic order).
pub fn list_out(cfg: InstallConfig) -> Vec<String> {
    let mut lines = Vec::new();
    let lock = Lockfile::parse(&std::fs::read_to_string(cfg.project_root.join("hard.lock")).unwrap_or_default())
        .unwrap_or_default();
    let mut names: BTreeSet<String> = BTreeSet::new();
    names.extend(cfg.manifest.dependencies.keys().cloned());
    names.extend(cfg.manifest.dev_dependencies.keys().cloned());
    for name in names {
        let locked = lock
            .version_of(&name)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".to_string());
        let requested = cfg
            .manifest
            .dependencies
            .get(&name)
            .or_else(|| cfg.manifest.dev_dependencies.get(&name))
            .cloned()
            .unwrap_or_default();
        let dev = if cfg.manifest.dev_dependencies.contains_key(&name) {
            " (dev)"
        } else {
            ""
        };
        lines.push(format!("{name} {locked} (wants {requested}){dev}"));
    }
    lines
}

/// `hard outdated` — compare locked versions against the newest satisfying
/// version the registry offers.
pub fn outdated(cfg: InstallConfig) -> Vec<String> {
    let mut lines = Vec::new();
    if cfg.offline {
        return vec!["registry is offline; cannot check for updates".to_string()];
    }
    let lock = Lockfile::parse(&std::fs::read_to_string(cfg.project_root.join("hard.lock")).unwrap_or_default())
        .unwrap_or_default();
    let mut names: BTreeSet<String> = BTreeSet::new();
    names.extend(cfg.manifest.dependencies.keys().cloned());
    names.extend(cfg.manifest.dev_dependencies.keys().cloned());
    for name in names {
        let req_text = cfg
            .manifest
            .dependencies
            .get(&name)
            .or_else(|| cfg.manifest.dev_dependencies.get(&name))
            .cloned()
            .unwrap_or_default();
        let Ok(req) = VersionReq::parse(&req_text) else {
            continue;
        };
        match cfg.registry.metadata(&name) {
            Ok(meta) => {
                let versions: Vec<Version> = meta.versions.iter().map(|v| v.version.clone()).collect();
                match req.max_satisfying(&versions) {
                    Some(latest) => {
                        let locked = lock.version_of(&name);
                        if locked != Some(&latest) {
                            match locked {
                                Some(lv) => lines.push(format!(
                                    "{name}: locked {lv}, latest matching {latest} (update with `hard update {name}`)"
                                )),
                                None => lines.push(format!("{name}: not locked, latest {latest}")),
                            }
                        }
                    }
                    None => lines.push(format!("{name}: no version satisfies {req_text}")),
                }
            }
            Err(e) => lines.push(format!("{name}: {e}")),
        }
    }
    if lines.is_empty() {
        lines.push("all dependencies are up to date".to_string());
    }
    lines
}

/// Build an `InstallConfig` fragment shared by the CLI.
pub fn base_config(
    manifest: Manifest,
    project_root: PathBuf,
    offline: bool,
    verbose: bool,
    toolchain: &str,
    compiler_version: &str,
    verify: Option<VerifyPolicy>,
) -> InstallConfig {
    let reg_cfg = RegistryConfig::resolve(manifest.registry.as_deref(), offline);
    // `None` means the caller passed no policy, so `HARD_VERIFY` still applies
    // and, failing that, the default. The CLI resolves --verify itself and
    // exits on a bad value, so nothing is silently downgraded here.
    let parallel = std::env::var("HARD_JOBS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get().min(8))
                .unwrap_or(4)
        });
    InstallConfig {
        manifest,
        project_root,
        offline,
        verbose,
        toolchain: toolchain.to_string(),
        compiler_version: compiler_version.to_string(),
        registry: Registry::new(reg_cfg),
        cache: Cache::new(),
        frozen: false,
        update: false,
        parallel,
        verify,
    }
}