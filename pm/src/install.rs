//! Install orchestration: `hard install/add/remove/update/list/outdated`.
//!
//! This is the entry point the CLI calls. It owns the "fetch or reuse from
//! cache -> resolve -> lock -> project install" pipeline shared by every
//! package-manager command.

use crate::cache::Cache;
use crate::lockfile::{Lockfile, LOCKFILE_SCHEMA};
use crate::manifest::Manifest;
use crate::registry::{Registry, RegistryConfig};
use crate::resolver::{self, Index, IndexEntry, ResolveError, Resolution, ResolvedPackage, SourceKind};
use crate::semver::{Version, VersionReq};
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
}

impl InstallReport {
    pub fn succeeded(&self) -> bool {
        self.errors.is_empty()
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
            for dep in e.deps.keys() {
                if !seen.contains(dep) {
                    queue.push(dep.clone());
                }
            }
        }
        index.insert(name.clone(), entries);
    }
    Ok(index)
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
                    if let Some(d) = v.get("dependencies").and_then(|x| x.as_arr()) {
                        for item in d {
                            if let (Some(dn), Some(dr)) = (
                                item.get("name").and_then(|x| x.as_str()),
                                item.get("req").and_then(|x| x.as_str()),
                            ) {
                                deps.insert(dn.to_string(), dr.to_string());
                            }
                        }
                    }
                    out.push(IndexEntry {
                        version: ver,
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

    let resolution: Resolution = if !cfg.offline || lock.packages.is_empty() {
        match fresh_resolve(cfg) {
            Ok(r) => r,
            Err(errors) => {
                report.errors = errors;
                return report;
            }
        }
    } else if let Some(r) = locked_resolution(&cfg.manifest, &lock) {
        r
    } else {
        match fresh_resolve(cfg) {
            Ok(r) => r,
            Err(errors) => {
                report.errors = errors;
                return report;
            }
        }
    };

    // Materialize: ensure every resolved package is present (cache + project).
    for p in &resolution.packages {
        match ensure_package(cfg, p) {
            Ok(fetched) => {
                if fetched {
                    report.fetched.push((p.name.clone(), p.version.clone()));
                } else {
                    report.reused.push((p.name.clone(), p.version.clone()));
                }
            }
            Err(e) => {
                report.errors.push(e);
            }
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
fn fresh_resolve(cfg: &InstallConfig) -> Result<Resolution, Vec<String>> {
    let index = build_index(cfg)?;
    let mut roots: BTreeMap<String, String> = BTreeMap::new();
    roots.extend(cfg.manifest.dependencies.clone());
    roots.extend(cfg.manifest.dev_dependencies.clone());
    resolver::resolve(&index, &roots, "hard.toml").map_err(resolver_errors)
}

fn resolver_errors(e: ResolveError) -> Vec<String> {
    let mut v = vec![e.message];
    if let Some(d) = e.detail {
        v.push(d);
    }
    v
}

/// Make sure a resolved package is present in the cache and the project's
/// `.hard/packages/`. Returns true when it was downloaded this run.
fn ensure_package(cfg: &InstallConfig, p: &ResolvedPackage) -> Result<bool, String> {
    if p.source == SourceKind::Workspace {
        return Ok(false);
    }
    let version_str = p.version.to_string();
    let cached = cfg.cache.has(&p.name, &p.version);
    if cached {
        cfg.cache
            .verify_version(&p.name, &version_str)
            .map_err(|e| format!("cached {}@{version_str}: {e}", p.name))?;
    } else {
        if cfg.offline || cfg.frozen {
            return Err(format!(
                "{}@{} is not in the cache and the registry is offline",
                p.name, version_str
            ));
        }
        crate::tui::progress::start(&format!("fetch {}@{version_str}", p.name));
        let archive = cfg
            .registry
            .download(&p.name, &p.version, p.integrity.as_deref())
            .map_err(|e| format!("cannot download {}@{version_str}: {e}", p.name))?;
        cfg.cache
            .put(
                &p.name,
                &p.version,
                &archive,
                Some(&cfg.registry.config.url),
                None,
            )
            .map_err(|e| format!("cannot cache {}@{version_str}: {e}", p.name))?;
        crate::tui::progress::finish(&format!("fetch {}@{version_str}", p.name), true);
        return link_package(cfg, p);
    }

    link_package(cfg, p)
}

/// Copy a cached package's extracted source into `.hard/packages/<name>` and
/// update the marker metadata.
fn link_package(cfg: &InstallConfig, p: &ResolvedPackage) -> Result<bool, String> {
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
    Ok(false)
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
) -> InstallConfig {
    let reg_cfg = RegistryConfig::resolve(manifest.registry.as_deref(), offline);
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
    }
}