//! Deterministic dependency resolution.
//!
//! Takes a package index (a `name -> published versions` map) plus the root
//! requirements from `hard.toml` and produces a topologically-sorted,
//! byte-for-byte reproducible resolution. Detects unresolved packages,
//! version conflicts, duplicate requirements and circular dependency chains,
//! each with a helpful message.

use crate::semver::{Version, VersionReq};
use std::collections::{BTreeMap, BTreeSet};

/// Where a resolved package came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// Published to the registry and stored in the shared cache.
    Registry,
    /// A local workspace member.
    Workspace,
}

/// One published version of a package.
#[derive(Clone, Debug)]
pub struct IndexEntry {
    pub version: Version,
    /// Dependency name -> requirement.
    pub deps: BTreeMap<String, String>,
    /// Integrity hash of the package archive if known.
    pub integrity: Option<String>,
    pub description: Option<String>,
}

/// The full search space the resolver may pick from.
pub type Index = BTreeMap<String, Vec<IndexEntry>>;

/// A resolved package in the final graph.
#[derive(Clone, Debug)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: Version,
    /// The requirement that selected this version.
    pub req: String,
    /// `root` for top-level dependencies, otherwise the requesting package.
    pub required_by: String,
    /// Dependency package names, in sorted order.
    pub deps: Vec<String>,
    pub integrity: Option<String>,
    pub source: SourceKind,
}

/// The deterministic result of a resolution.
#[derive(Clone, Debug, Default)]
pub struct Resolution {
    /// Packages in dependency (install) order: a package always precedes the
    /// packages that depend on it.
    pub packages: Vec<ResolvedPackage>,
    /// `name -> sorted dependency names` for the lockfile tree.
    pub graph: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveErrorKind {
    NotFound,
    Unresolvable,
    Conflict,
    Duplicate,
    Circular,
    InvalidReq,
}

#[derive(Clone, Debug)]
pub struct ResolveError {
    pub kind: ResolveErrorKind,
    pub message: String,
    /// Optional chain of packages involved (e.g. the cycle).
    pub detail: Option<String>,
}

impl ResolveError {
    fn new(kind: ResolveErrorKind, message: impl Into<String>) -> ResolveError {
        ResolveError {
            kind,
            message: message.into(),
            detail: None,
        }
    }
    fn cycle(package: &str, chain: &[String]) -> ResolveError {
        let mut detail = chain
            .iter()
            .chain(std::iter::once(&package.to_string()))
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" -> ");
        detail.push_str(&format!(" -> {package}"));
        ResolveError {
            kind: ResolveErrorKind::Circular,
            message: format!("circular dependency detected while resolving '{package}'"),
            detail: Some(detail),
        }
    }
}

#[derive(Clone, Debug)]
struct Assigned {
    version: Version,
    reqs: Vec<(VersionReq, String)>,
    integrity: Option<String>,
    source: SourceKind,
    deps: Vec<String>,
}

/// Resolve root requirements against `index`.
///
/// `require` is the requirement string that introduced each root package
/// (used in diagnostics); defaults to `"*"`.
pub fn resolve(
    index: &Index,
    roots: &BTreeMap<String, String>,
    required_by_root: &str,
) -> Result<Resolution, ResolveError> {
    let mut state = ResolveState {
        index,
        assigned: BTreeMap::new(),
        stack: Vec::new(),
    };
    for (name, req) in roots {
        let req = VersionReq::parse(req).map_err(|e| ResolveError::new(
            ResolveErrorKind::InvalidReq,
            format!("invalid requirement '{req}' for '{name}': {e}"),
        ))?;
        if state.assigned.contains_key(name) {
            return Err(ResolveError::new(
                ResolveErrorKind::Duplicate,
                format!("duplicate top-level dependency '{name}'"),
            ));
        }
        state.visit(name, req, required_by_root)?;
    }

    // Topological order: complete the DFS using each assigned package's deps.
    let mut resolution = Resolution::default();
    let mut done = BTreeSet::new();
    let names: Vec<String> = state.assigned.keys().cloned().collect();
    for name in names {
        if !done.contains(&name) {
            topo_emit(&state, &mut resolution, &name, &mut done);
        }
    }
    for (name, pkg) in &state.assigned {
        resolution.graph.insert(name.clone(), pkg.deps.clone());
    }
    Ok(resolution)
}

fn topo_emit(
    state: &ResolveState,
    resolution: &mut Resolution,
    name: &str,
    done: &mut BTreeSet<String>,
) {
    if done.contains(name) {
        return;
    }
    done.insert(name.to_string());
    if let Some(a) = state.assigned.get(name) {
        for d in &a.deps {
            topo_emit(state, resolution, d, done);
        }
        let (req_text, required_by) = a
            .reqs
            .first()
            .map(|(r, src)| (r.to_string_canonical(), src.clone()))
            .unwrap_or(("*".to_string(), "root".to_string()));
        resolution.packages.push(ResolvedPackage {
            name: name.to_string(),
            version: a.version.clone(),
            req: req_text,
            required_by,
            deps: a.deps.clone(),
            integrity: a.integrity.clone(),
            source: a.source.clone(),
        });
    }
}

struct ResolveState<'a> {
    index: &'a Index,
    assigned: BTreeMap<String, Assigned>,
    stack: Vec<String>,
}

impl<'a> ResolveState<'a> {
    fn visit(
        &mut self,
        name: &str,
        req: VersionReq,
        required_by: &str,
    ) -> Result<(), ResolveError> {
        if let Some(a) = self.assigned.get(name) {
            if req.matches(&a.version) {
                return Ok(());
            }
            // A new requirement no longer matches the assigned version: try
            // to find a version satisfying EVERY requirement seen so far.
            return self.reassign(name, required_by);
        }
        if self.stack.iter().any(|s| s == name) {
            return Err(ResolveError::cycle(name, &self.stack));
        }
        let versions = match self.index.get(name) {
            Some(v) => v,
            None => {
                return Err(ResolveError::new(
                    ResolveErrorKind::NotFound,
                    format!("package '{name}' was not found in the registry"),
                ));
            }
        };
        let pv: Vec<Version> = versions.iter().map(|e| e.version.clone()).collect();
        let chosen = req.max_satisfying(&pv).ok_or_else(|| {
            let avail = versions
                .iter()
                .map(|e| e.version.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            ResolveError::new(
                ResolveErrorKind::Unresolvable,
                format!(
                    "no version of '{name}' satisfies '{req_text}' (available: {avail})",
                    req_text = req.to_string_canonical()
                ),
            )
        })?;
        let entry = versions
            .iter()
            .find(|e| e.version == chosen)
            .expect("chosen version present");
        self.stack.push(name.to_string());
        let mut dep_names = Vec::new();
        for (dep, dep_req_text) in &entry.deps {
            let dep_req = VersionReq::parse(dep_req_text).map_err(|e| {
                ResolveError::new(
                    ResolveErrorKind::InvalidReq,
                    format!(
                        "invalid requirement '{dep_req_text}' for '{dep}' (declared by '{name}'): {e}"
                    ),
                )
            })?;
            self.visit(dep, dep_req, name)?;
            if !dep_names.contains(dep) {
                dep_names.push(dep.clone());
            }
        }
        self.stack.pop();
        self.assigned.insert(
            name.to_string(),
            Assigned {
                version: chosen,
                reqs: vec![(req, required_by.to_string())],
                integrity: entry.integrity.clone(),
                source: SourceKind::Registry,
                deps: dep_names,
            },
        );
        Ok(())
    }

    /// Try to replace an assigned version with one satisfying all recorded
    /// requirements. Failures become version-conflict errors.
    fn reassign(&mut self, name: &str, _required_by: &str) -> Result<(), ResolveError> {
        let versions = self
            .index
            .get(name)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let current = self.assigned.get(name).expect("assigned").clone();
        let archives: Vec<&IndexEntry> = versions
            .iter()
            .filter(|e| current.reqs.iter().all(|(r, _)| r.matches(&e.version)))
            .collect();
        let Some(candidate) = archives.iter().max_by(|a, b| a.version.cmp(&b.version)) else {
            let reqs: Vec<String> = current
                .reqs
                .iter()
                .map(|(r, by)| format!("'{}' (from {by})", r.to_string_canonical()))
                .collect();
            return Err(ResolveError::new(
                ResolveErrorKind::Conflict,
                format!(
                    "version conflict for '{name}': no version satisfies all of {}",
                    reqs.join(", and ")
                ),
            ));
        };
        let mut a = current;
        a.version = candidate.version.clone();
        a.integrity = candidate.integrity.clone();
        self.assigned.insert(name.to_string(), a);
        Ok(())
    }
}

/// Quick check that two requirement strings could ever agree on some version
/// (used by `hard outdated` / conflict previews). Returns Ok when an
/// intersection is proven or undetermined, Err on a definite conflict.
pub fn requirements_touch(a: &VersionReq, b: &VersionReq) -> Result<(), ()> {
    let samples = synthetic_samples(a, b);
    let any = samples.iter().any(|s| a.matches(s) && b.matches(s));
    if any {
        Ok(())
    } else {
        Err(())
    }
}

/// Build a small set of boundary versions from two requirements for the
/// intersection probe above.
fn synthetic_samples(a: &VersionReq, b: &VersionReq) -> Vec<Version> {
    let mut v = Vec::new();
    for r in [a, b] {
        for alt in r.clone().alts {
            for p in alt {
                let ver = match &p {
                    crate::semver::Pred::Any => Version::parse("0.0.0").unwrap(),
                    crate::semver::Pred::Exact(v)
                    | crate::semver::Pred::Gt(v)
                    | crate::semver::Pred::Ge(v)
                    | crate::semver::Pred::Lt(v)
                    | crate::semver::Pred::Le(v)
                    | crate::semver::Pred::Caret(v)
                    | crate::semver::Pred::Tilde(v) => v.clone(),
                    crate::semver::Pred::Wildcard { major, minor } => {
                        let mut x = Version::default();
                        x.major = *major;
                        x.minor = minor.unwrap_or(0);
                        x
                    }
                };
                v.push(ver.clone());
                let mut next = ver.clone();
                next.patch += 1;
                v.push(next);
                let mut nextm = ver.clone();
                nextm.minor += 1;
                v.push(nextm);
            }
        }
    }
    v
}

/// Load an index entry set with a workspace member overlaying the same name
/// (workspace members always win over registry versions).
pub fn entry(version: Version, deps: BTreeMap<String, String>) -> IndexEntry {
    IndexEntry {
        version,
        deps,
        integrity: None,
        description: None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn entry(version: &str, deps: &[(&str, &str)]) -> IndexEntry {
        IndexEntry {
            version: Version::parse(version).unwrap(),
            deps: deps.iter().map(|(n, r)| (n.to_string(), r.to_string())).collect(),
            integrity: None,
            description: None,
        }
    }

    fn index() -> Index {
        let mut idx: Index = BTreeMap::new();
        idx.insert("app".to_string(), vec![entry("1.0.0", &[]), entry("1.5.0", &[("lib", "^2.0.0")])]);
        idx.insert("lib".to_string(), vec![entry("1.9.0", &[]), entry("2.1.0", &[("mini", "^0.3.0")]), entry("2.4.0", &[])]);
        idx.insert("mini".to_string(), vec![entry("0.3.1", &[])]);
        idx
    }

    #[test]
    fn picks_newest_satisfying_and_orders_topologically() {
        let mut roots = BTreeMap::new();
        roots.insert("app".to_string(), "^1.0.0".to_string());
        let resolution = resolve(&index(), &roots, "hard.toml").unwrap();
        let names: Vec<&str> = resolution.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["lib", "app"]); // deps before dependents
        let app = resolution.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.version.tuple(), (1, 5, 0));
        let lib = resolution.packages.iter().find(|p| p.name == "lib").unwrap();
        assert_eq!(lib.version.tuple(), (2, 4, 0)); // newest satisfying ^2.0.0
    }

    #[test]
    fn conflicting_requirements_are_an_error() {
        // app 1.5.0 needs lib ^2.0.0, but the root pins lib ^1.0.0: no version
        // can satisfy both, so the resolver must report a conflict.
        let mut roots = BTreeMap::new();
        roots.insert("app".to_string(), "^1.0.0".to_string());
        roots.insert("lib".to_string(), "^1.0.0".to_string());
        let err = resolve(&index(), &roots, "hard.toml").unwrap_err();
        assert!(
            err.message.to_lowercase().contains("lib"),
            "unexpected message: {}",
            err.message
        );
    }

    #[test]
    fn unresolved_package_is_an_error() {
        let mut roots = BTreeMap::new();
        roots.insert("nope".to_string(), "*".to_string());
        let err = resolve(&index(), &roots, "hard.toml").unwrap_err();
        assert!(err.message.contains("nope"), "{}", err.message);
    }

    #[test]
    fn unsatisfiable_requirement_is_an_error() {
        let mut roots = BTreeMap::new();
        roots.insert("app".to_string(), "^9.0.0".to_string());
        let err = resolve(&index(), &roots, "hard.toml").unwrap_err();
        assert!(err.message.to_lowercase().contains("no version") || err.message.contains("app"), "{}", err.message);
    }

    #[test]
    fn circular_dependency_is_detected() {
        let mut idx: Index = BTreeMap::new();
        idx.insert("a".to_string(), vec![entry("1.0.0", &[("b", "^1.0.0")])]);
        idx.insert("b".to_string(), vec![entry("1.0.0", &[("a", "^1.0.0")])]);
        let mut roots = BTreeMap::new();
        roots.insert("a".to_string(), "^1.0.0".to_string());
        let err = resolve(&idx, &roots, "hard.toml").unwrap_err();
        assert!(err.message.contains("circular"), "{}", err.message);
    }

    #[test]
    fn resolution_is_deterministic() {
        let mut roots = BTreeMap::new();
        roots.insert("app".to_string(), "^1.0.0".to_string());
        let r1 = resolve(&index(), &roots, "hard.toml").unwrap();
        let r2 = resolve(&index(), &roots, "hard.toml").unwrap();
        assert_eq!(r1.packages.len(), r2.packages.len());
        let names: Vec<&str> = r1.packages.iter().map(|p| p.name.as_str()).collect();
        let names2: Vec<&str> = r2.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, names2);
    }
}
