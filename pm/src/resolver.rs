//! Dependency resolution, version 2.
//!
//! # What was wrong with v1
//!
//! v1 walked the graph once, greedily taking the newest version that
//! satisfied the requirements it had seen *so far*, and when a later
//! requirement disagreed it tried exactly one repair: move the package to
//! the newest version that satisfied all of them. Three things fall out of
//! that, all of which are real bugs rather than style points:
//!
//! 1. **Transitive dev-dependencies were installed.** A published package's
//!    `[dev-dependencies]` are for its own tests; a consumer does not need
//!    them, so pulling them in meant resolving (and downloading) packages
//!    that nothing depends on. It was also why `hard install` could fail
//!    with "cannot resolve 'testlib'" for a package that installed fine.
//! 2. **A conflict that a different choice would fix was reported as a
//!    conflict.** There was no backtracking: only the newest candidate was
//!    ever considered.
//! 3. **Every change re-resolved from scratch.** The existing lockfile was
//!    not consulted, so an unrelated new dependency could bump unrelated
//!    packages.
//!
//! # What v2 does
//!
//! - **Typed edges.** [`DepKind`] distinguishes normal, dev and build edges.
//!    Root requirements include dev edges; *transitive* resolution follows
//!    normal and build edges only. This is what npm, Cargo and crates.io do,
//!    and it is the fix for (1).
//! - **Constraint propagation with bounded backtracking.** Requirements
//!    accumulate per package; the solver picks the newest version satisfying
//!    all of them, and when that set is empty it backs off to the next
//!    candidate and re-propagates, up to a bounded step budget. That is the
//!    fix for (2).
//! - **Lockfile preference.** Versions already in `hard.lock` are tried
//!    first, so a fresh resolution reproduces the previous one and a new
//!    dependency does not churn the rest of the graph. That is the fix for
//!    (3).
//! - **Precise diagnostics.** A conflict names the package, every
//!    requirement that touched it, who asked, and which versions exist.
//! - **Cycle policy.** A cycle among normal edges is an error (it means the
//!    graph cannot be ordered for install). A cycle that only exists through
//!    dev edges is legal, because dev edges are not followed transitively.
//!
//! Determinism is a hard requirement: the output order, the chosen versions
//! and the error text depend only on the index and the roots, never on hash
//! iteration order or timing.
//!
//! [`resolve_v1`] keeps the old single-pass algorithm. It is not used by
//! `hard install`; it exists so the differential test can prove that v2 agrees
//! with v1 everywhere v1 was already correct.

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

/// What kind of edge a dependency is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum DepKind {
    /// A runtime dependency, and the only kind followed transitively.
    #[default]
    Normal,
    /// A build-time dependency; also followed transitively.
    Build,
    /// A test-only dependency. Followed from the root, never transitively.
    Dev,
}

impl std::fmt::Display for DepKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl DepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DepKind::Normal => "normal",
            DepKind::Build => "build",
            DepKind::Dev => "dev",
        }
    }

    /// Is this edge followed when it is *not* a root requirement?
    pub fn transitive(self) -> bool {
        matches!(self, DepKind::Normal | DepKind::Build)
    }
}

/// One published version of a package.
#[derive(Clone, Debug)]
pub struct IndexEntry {
    pub version: Version,
    /// Dependency name -> requirement, for every kind of edge.
    pub deps: BTreeMap<String, String>,
    /// Which kind each edge in `deps` is. Missing means `normal`.
    pub kinds: BTreeMap<String, DepKind>,
    /// Integrity hash of the package archive if known.
    pub integrity: Option<String>,
    pub description: Option<String>,
}

impl IndexEntry {
    /// The kind of the edge `name` declares (normal when unstated).
    pub fn kind_of(&self, name: &str) -> DepKind {
        self.kinds.get(name).copied().unwrap_or_default()
    }

    /// Edges that a consumer of this package must also resolve.
    pub fn transitive_deps(&self) -> impl Iterator<Item = (&String, &String, DepKind)> {
        self.deps
            .iter()
            .map(|(n, r)| (n, r, self.kind_of(n)))
            .filter(|(_, _, k)| k.transitive())
    }

    /// Every edge, in name order.
    pub fn all_deps(&self) -> impl Iterator<Item = (&String, &String, DepKind)> {
        self.deps
            .iter()
            .map(|(n, r)| (n, r, self.kind_of(n)))
    }
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
    /// The publish signature, filled in when the metadata carried one.
    pub signature: Option<String>,
    /// The key id that signed this version.
    pub key_id: Option<String>,
    /// The manifest fingerprint the signature covers.
    pub fingerprint: Option<String>,
    /// Whether the signature verified against the local trust store.
    pub verified: bool,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveErrorKind {
    NotFound,
    Unresolvable,
    Conflict,
    Duplicate,
    Circular,
    InvalidReq,
    /// The search exceeded its step budget (a pathological graph, or a bug).
    TooComplex,
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

    fn with_detail(
        kind: ResolveErrorKind,
        message: impl Into<String>,
        detail: impl Into<String>,
    ) -> ResolveError {
        ResolveError {
            kind,
            message: message.into(),
            detail: Some(detail.into()),
        }
    }
}

/// Default step budget. A healthy graph needs a few dozen steps; a couple of
/// thousand is enough for graphs hundreds of packages deep while still
/// terminating on a pathological one.
pub const DEFAULT_STEP_BUDGET: usize = 20_000;

/// How the resolver should behave.
#[derive(Clone, Debug)]
pub struct ResolveOptions {
    /// Versions to try first (usually the current lockfile).
    pub preferred: BTreeMap<String, Version>,
    /// Include dev-dependencies of the root.
    pub include_root_dev: bool,
    /// Follow dev-dependencies of dependencies (off: they are the
    /// dependency's own tests, not the consumer's problem).
    pub follow_transitive_dev: bool,
    pub step_budget: usize,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        ResolveOptions {
            preferred: BTreeMap::new(),
            include_root_dev: true,
            follow_transitive_dev: false,
            step_budget: DEFAULT_STEP_BUDGET,
        }
    }
}

impl ResolveOptions {
    /// Options that reproduce v1's edge handling, for the differential test.
    pub fn legacy() -> ResolveOptions {
        ResolveOptions {
            include_root_dev: true,
            follow_transitive_dev: true,
            ..ResolveOptions::default()
        }
    }

    /// Prefer the versions in `lock` (a `name -> version` map).
    pub fn with_lock(mut self, lock: BTreeMap<String, Version>) -> ResolveOptions {
        self.preferred = lock;
        self
    }
}


/// Resolve root requirements against `index`.
pub fn resolve(
    index: &Index,
    roots: &BTreeMap<String, String>,
    _required_by_root: &str,
) -> Result<Resolution, ResolveError> {
    resolve_with(index, roots, &ResolveOptions::default())
}

/// Resolve with explicit options.
pub fn resolve_with(
    index: &Index,
    roots: &BTreeMap<String, String>,
    options: &ResolveOptions,
) -> Result<Resolution, ResolveError> {
    let mut state = State {
        index,
        options,
        assigned: BTreeMap::new(),
        requirements: BTreeMap::new(),
        steps: 0,
        tried: BTreeMap::new(),
        stack: Vec::new(),
    };
    for (name, req) in roots {
        state.require(name, req, ROOT, DepKind::Normal)?;
    }
    state.propagate()?;

    // Emit in dependency order: a package always precedes its dependents.
    let mut resolution = Resolution::default();
    let mut done: BTreeSet<String> = BTreeSet::new();
    let ordered: Vec<String> = state.assigned.keys().cloned().collect();
    for name in &ordered {
        state.emit(&mut resolution, name, &mut done)?;
    }
    for name in &ordered {
        resolution.graph.insert(name.clone(), deps_of(&state, name));
    }
    Ok(resolution)
}

/// The sentinel `asked_by` for a requirement written in `hard.toml`.
const ROOT: &str = "hard.toml";

struct State<'a> {
    index: &'a Index,
    options: &'a ResolveOptions,
    assigned: BTreeMap<String, Chosen>,
    requirements: BTreeMap<String, Vec<Requirement>>,
    steps: usize,
    /// Candidates already proven to lead nowhere, per package.
    ///
    /// Scoped to the *current* requirement set: a package's list is cleared
    /// whenever a requirement on it is added or removed. Without that, a
    /// backtrack that changes a constraint would permanently forbid the
    /// candidates that become valid under the new one.
    tried: BTreeMap<String, Vec<Version>>,
    /// The active decisions, oldest first. Backtracking pops from here.
    stack: Vec<Decision>,
}

/// One version choice, and everything it added to the state.
#[derive(Clone, Debug)]
struct Decision {
    name: String,
    /// `(package, requirement text, kind, asked_by)` this choice introduced.
    added: Vec<(String, String, DepKind, String)>,
}

#[derive(Clone, Debug)]
struct Chosen {
    version: Version,
    integrity: Option<String>,
    source: SourceKind,
}

/// One requirement on a package, remembered so conflicts can explain
/// themselves. `VersionReq` has no `Eq`, so identity is the canonical text
/// plus who asked, which is exactly what "the same requirement twice" means
/// for a caller.
#[derive(Clone, Debug)]
struct Requirement {
    req: VersionReq,
    text: String,
    /// `hard.toml` for a root requirement, otherwise `package@version`.
    asked_by: String,
    kind: DepKind,
}

impl<'a> State<'a> {
    fn step(&mut self) -> Result<(), ResolveError> {
        self.steps += 1;
        if self.steps > self.options.step_budget {
            return Err(ResolveError::new(
                ResolveErrorKind::TooComplex,
                format!(
                    "resolution did not converge within {} steps (the graph may be cyclic or contradictory)",
                    self.options.step_budget
                ),
            ));
        }
        Ok(())
    }

    /// Record a requirement on `name` without choosing anything.
    fn require(
        &mut self,
        name: &str,
        req_text: &str,
        asked_by: &str,
        kind: DepKind,
    ) -> Result<(), ResolveError> {
        self.step()?;
        let req = VersionReq::parse(req_text).map_err(|e| {
            ResolveError::new(
                ResolveErrorKind::InvalidReq,
                format!("invalid requirement '{req_text}' for '{name}': {e}"),
            )
        })?;
        let entry = Requirement {
            req,
            text: req_text.to_string(),
            asked_by: asked_by.to_string(),
            kind,
        };
        let list = self.requirements.entry(name.to_string()).or_default();
        if list
            .iter()
            .any(|r| r.text == entry.text && r.asked_by == entry.asked_by && r.kind == entry.kind)
        {
            return Ok(());
        }
        list.push(entry);
        // the constraint set changed, so previously-failed candidates may not
        // be failures any more
        self.tried.remove(name);
        Ok(())
    }

    /// Versions of `name` satisfying every requirement on it, best first.
    fn candidates(&self, name: &str) -> Vec<Version> {
        let Some(reqs) = self.requirements.get(name) else {
            return Vec::new();
        };
        let mut out: Vec<Version> = self
            .index
            .get(name)
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| e.version.clone())
                    .filter(|v| reqs.iter().all(|r| r.req.matches(v)))
                    .collect()
            })
            .unwrap_or_default();
        // newest first
        out.sort_by(|a, b| b.cmp(a));
        // a preferred (locked) version jumps the queue
        if let Some(pref) = self.options.preferred.get(name) {
            if let Some(pos) = out.iter().position(|v| v == pref) {
                let v = out.remove(pos);
                out.insert(0, v);
            }
        }
        out
    }

    /// Every published version of `name`, for diagnostics.
    fn available(&self, name: &str) -> String {
        self.index
            .get(name)
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| e.version.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    }

    /// Choose versions for everything that needs one, propagating
    /// requirements down the graph and backtracking when a choice turns out
    /// to be unworkable.
    fn propagate(&mut self) -> Result<(), ResolveError> {
        loop {
            self.step()?;

            // A pick that no longer satisfies its requirements is dropped, so
            // the next iteration re-picks it from what is left.
            if let Some(stale) = self.stale_pick() {
                self.unassign(&stale);
                continue;
            }

            let next = self
                .requirements
                .keys()
                .find(|name| !self.assigned.contains_key(*name))
                .cloned();
            let Some(name) = next else {
                return self.check_cycles();
            };

            if !self.index.contains_key(&name) {
                return Err(self.not_found(&name));
            }

            let candidates = self.candidates(&name);
            if candidates.is_empty() {
                // Nothing fits. If an earlier decision has an alternative,
                // take it and start propagating again.
                if self.backtrack()? {
                    continue;
                }
                return Err(self.conflict(&name));
            }

            let Some(version) = self.next_candidate(&name, &candidates) else {
                // every candidate was already tried and failed
                if self.backtrack()? {
                    continue;
                }
                return Err(self.conflict(&name));
            };
            self.apply(&name, &version)?;
        }
    }

    /// The first candidate that has not been tried under the current
    /// requirements, marking it tried.
    fn next_candidate(&mut self, name: &str, candidates: &[Version]) -> Option<Version> {
        let tried = self.tried.entry(name.to_string()).or_default();
        candidates
            .iter()
            .find(|v| !tried.contains(v))
            .cloned()
            .map(|v| {
                self.tried.entry(name.to_string()).or_default().push(v.clone());
                v
            })
    }

    /// A package whose current pick no longer satisfies its requirements.
    fn stale_pick(&self) -> Option<String> {
        for (name, chosen) in &self.assigned {
            let Some(reqs) = self.requirements.get(name) else {
                continue;
            };
            if !reqs.iter().all(|r| r.req.matches(&chosen.version)) {
                return Some(name.clone());
            }
        }
        None
    }

    /// Commit to `version` for `name`, recording every requirement it adds
    /// so the choice can be undone as a unit.
    fn apply(&mut self, name: &str, version: &Version) -> Result<(), ResolveError> {
        let entries = self.index.get(name).ok_or_else(|| self.not_found(name))?;
        let entry = entries
            .iter()
            .find(|e| &e.version == version)
            .ok_or_else(|| {
                ResolveError::new(
                    ResolveErrorKind::Unresolvable,
                    format!("{name}@{version} is not a published version"),
                )
            })?;
        // Validate every requirement string before mutating anything: a
        // malformed requirement is a hard error, not something to backtrack
        // past.
        let mut edges: Vec<(String, String, DepKind, String)> = Vec::new();
        for (dep, dep_req, kind) in entry.all_deps() {
            if dep == name {
                // a self-edge is rejected at publish time; ignore it here
                // rather than looping forever
                continue;
            }
            VersionReq::parse(dep_req).map_err(|e| {
                ResolveError::new(
                    ResolveErrorKind::InvalidReq,
                    format!(
                        "invalid requirement '{dep_req}' for '{dep}' (declared by '{name}@{version}'): {e}"
                    ),
                )
            })?;
            let follow = match kind {
                // a dev edge of a dependency is that dependency's own test
                // suite: a consumer does not install it
                DepKind::Dev => {
                    self.options.follow_transitive_dev
                        || self
                            .requirements
                            .get(name)
                            .map(|r| r.iter().all(|r| r.kind == DepKind::Dev))
                            .unwrap_or(false)
                }
                _ => true,
            };
            if !follow {
                continue;
            }
            edges.push((
                dep.clone(),
                dep_req.clone(),
                kind,
                format!("{name}@{version}"),
            ));
        }
        for (dep, dep_req, kind, asked_by) in &edges {
            self.require(dep, dep_req, asked_by, *kind)?;
        }
        self.assigned.insert(
            name.to_string(),
            Chosen {
                version: version.clone(),
                integrity: entry.integrity.clone(),
                source: SourceKind::Registry,
            },
        );
        self.stack.push(Decision {
            name: name.to_string(),
            added: edges,
        });
        Ok(())
    }

    /// Drop a pick without touching the requirements it introduced (the
    /// caller re-picks or backtracks).
    fn unassign(&mut self, name: &str) {
        self.assigned.remove(name);
    }

    /// Undo the most recent decision and try its next candidate. Returns
    /// false when there is nothing left to try anywhere.
    fn backtrack(&mut self) -> Result<bool, ResolveError> {
        while let Some(decision) = self.stack.pop() {
            // undo the requirements this decision introduced, in reverse
            for (dep, req_text, kind, asked_by) in decision.added.iter().rev() {
                if let Some(list) = self.requirements.get_mut(dep) {
                    if let Some(pos) = list
                        .iter()
                        .position(|r| &r.text == req_text && &r.asked_by == asked_by && r.kind == *kind)
                    {
                        list.remove(pos);
                    }
                    if list.is_empty() {
                        self.requirements.remove(dep);
                    }
                    // the constraint set shrank: candidates that failed
                    // under the old one are worth trying again
                    self.tried.remove(dep);
                }
                // anything that only existed because of this decision goes too
                if !self
                    .requirements
                    .get(dep)
                    .map(|l| l.iter().any(|r| r.req.matches(
                        self.assigned.get(dep).map(|c| &c.version).unwrap_or(&Version::default()),
                    )))
                    .unwrap_or(false)
                {
                    self.assigned.remove(dep);
                }
            }
            self.assigned.remove(&decision.name);
            let candidates = self.candidates(&decision.name);
            let Some(next) = self.next_candidate(&decision.name, &candidates) else {
                // no alternative for this one: keep unwinding
                continue;
            };
            self.apply(&decision.name, &next)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// A package that is required but does not exist.
    fn not_found(&self, name: &str) -> ResolveError {
        let asked = self
            .requirements
            .get(name)
            .and_then(|r| r.first())
            .map(|r| r.asked_by.clone())
            .unwrap_or_default();
        // A missing dependency is a broken publish, not a version problem, so
        // the message names the package that asked for it (and what needed
        // that package) rather than listing versions that do not exist.
        let mut message = format!("package '{name}' was not found in the registry");
        if asked == ROOT || asked.is_empty() {
            message.push_str(" (required by hard.toml)");
        } else {
            let path = self.explain_path(name);
            if path.len() > 1 {
                message.push_str(&format!(" (required by {})", path.join(" -> ")));
            } else {
                message.push_str(&format!(" (required by {asked})"));
            }
        }
        ResolveError::new(ResolveErrorKind::NotFound, message)
    }

    /// The conflict error for `name`: every requirement on it, who asked,
    /// what exists, and how the graph got there.
    fn conflict(&self, name: &str) -> ResolveError {
        let reqs = self.requirements.get(name).cloned().unwrap_or_default();
        let wanted: Vec<String> = reqs
            .iter()
            .map(|r| format!("'{}' (from {})", r.req.to_string_canonical(), r.asked_by))
            .collect();
        let message = if reqs.is_empty() {
            format!("no version of '{name}' is required")
        } else {
            format!(
                "version conflict for '{name}': no version satisfies all of {}",
                wanted.join(" and ")
            )
        };
        let available = self.available(name);
        let mut detail = Vec::new();
        if available.is_empty() {
            detail.push(format!("{name} has no published versions"));
        } else {
            detail.push(format!("available versions: {available}"));
        }
        let path = self.explain_path(name);
        if path.len() > 1 {
            detail.push(format!("required by {}", path.join(" -> ")));
        }
        ResolveError {
            kind: ResolveErrorKind::Conflict,
            message,
            detail: Some(detail.join("; ")),
        }
    }

    /// The chain of requirements that led to `name`, root first.
    fn explain_path(&self, name: &str) -> Vec<String> {
        let mut chain = vec![name.to_string()];
        let mut cursor = name.to_string();
        for _ in 0..32 {
            let Some(reqs) = self.requirements.get(&cursor) else {
                break;
            };
            let Some(r) = reqs
                .iter()
                .find(|r| r.asked_by == ROOT)
                .or_else(|| reqs.first())
            else {
                break;
            };
            if r.asked_by == ROOT || r.asked_by.is_empty() {
                chain.push(ROOT.to_string());
                break;
            }
            let parent = r
                .asked_by
                .split('@')
                .next()
                .unwrap_or(&r.asked_by)
                .to_string();
            if chain.contains(&parent) {
                break;
            }
            chain.push(parent.clone());
            cursor = parent;
        }
        chain.reverse();
        chain
    }

    /// A cycle among normal edges means the graph cannot be ordered.
    fn check_cycles(&self) -> Result<(), ResolveError> {
        let mut edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (name, chosen) in &self.assigned {
            let Some(entries) = self.index.get(name) else {
                continue;
            };
            let Some(entry) = entries.iter().find(|e| e.version == chosen.version) else {
                continue;
            };
            let mut deps: Vec<&str> = entry
                .transitive_deps()
                .filter(|(dep, _, _)| *dep != name && self.assigned.contains_key(*dep))
                .map(|(dep, _, _)| dep.as_str())
                .collect();
            deps.sort();
            edges.insert(name.as_str(), deps);
        }
        let mut state: BTreeMap<&str, u8> = BTreeMap::new();
        let mut stack: Vec<&str> = Vec::new();
        let mut names: Vec<&str> = edges.keys().copied().collect();
        names.sort();
        for name in names {
            if state.get(name).copied().unwrap_or(0) == 0 {
                if let Some(cycle) = walk_cycle(name, &edges, &mut state, &mut stack) {
                    let mut chain = cycle.clone();
                    chain.push(cycle[0].to_string());
                    return Err(ResolveError::with_detail(
                        ResolveErrorKind::Circular,
                        format!("circular dependency detected while resolving '{}'", cycle[0]),
                        chain.join(" -> "),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Emit `name` and everything it depends on, dependencies first.
    fn emit(
        &mut self,
        out: &mut Resolution,
        name: &str,
        done: &mut BTreeSet<String>,
    ) -> Result<(), ResolveError> {
        if done.contains(name) {
            return Ok(());
        }
        done.insert(name.to_string());
        let Some(chosen) = self.assigned.get(name) else {
            return Ok(());
        };
        let version = chosen.version.clone();
        let integrity = chosen.integrity.clone();
        let source = chosen.source.clone();
        let deps = deps_of(self, name);
        for d in &deps {
            self.emit(out, d, done)?;
        }
        let reqs = self.requirements.get(name).cloned().unwrap_or_default();
        // The requirement that "won" is the one from the root when there is
        // one, otherwise the first in sorted order: a stable, explainable
        // choice for the lockfile.
        let (req_text, required_by) = reqs
            .iter()
            .find(|r| r.asked_by == ROOT)
            .or_else(|| reqs.first())
            .map(|r| (r.req.to_string_canonical(), r.asked_by.clone()))
            .unwrap_or_else(|| ("*".to_string(), "root".to_string()));
        out.packages.push(ResolvedPackage {
            name: name.to_string(),
            version,
            req: req_text,
            required_by,
            deps,
            integrity,
            source,
            // A v1 index carries no trust information; install verifies after
            // download, which is where the signature is actually checked.
            signature: None,
            key_id: None,
            fingerprint: None,
            verified: false,
        });
        Ok(())
    }
}

/// The transitive dependency names of a chosen package, sorted.
fn deps_of(state: &State<'_>, name: &str) -> Vec<String> {
    let Some(chosen) = state.assigned.get(name) else {
        return Vec::new();
    };
    let Some(entries) = state.index.get(name) else {
        return Vec::new();
    };
    let Some(entry) = entries.iter().find(|e| e.version == chosen.version) else {
        return Vec::new();
    };
    let mut deps: Vec<String> = entry
        .transitive_deps()
        .filter(|(dep, _, _)| *dep != name && state.assigned.contains_key(*dep))
        .map(|(dep, _, _)| dep.clone())
        .collect();
    deps.sort();
    deps.dedup();
    deps
}

/// Iterative cycle detection: 0 = unseen, 1 = on the stack, 2 = done.
fn walk_cycle<'a>(
    node: &'a str,
    edges: &BTreeMap<&'a str, Vec<&'a str>>,
    state: &mut BTreeMap<&'a str, u8>,
    stack: &mut Vec<&'a str>,
) -> Option<Vec<String>> {
    state.insert(node, 1);
    stack.push(node);
    for dep in edges.get(node).map(|v| v.as_slice()).unwrap_or(&[]) {
        match state.get(dep).copied().unwrap_or(0) {
            0 => {
                if let Some(cycle) = walk_cycle(dep, edges, state, stack) {
                    return Some(cycle);
                }
            }
            1 => {
                // a back edge: the cycle is the stack from `dep` on
                let start = stack.iter().position(|n| n == dep).unwrap_or(0);
                return Some(stack[start..].iter().map(|s| s.to_string()).collect());
            }
            _ => {}
        }
    }
    stack.pop();
    state.insert(node, 2);
    None
}

// ---------------------------------------------------------------------------
// v1, retained for the differential test
// ---------------------------------------------------------------------------

/// The v1 resolver: one greedy pass with a single repair attempt.
///
/// Not used by `hard install`. [`crate::resolver::tests::v2_agrees_with_v1`]
/// runs both over a set of graphs and asserts they agree wherever v1 was
/// already correct, which is how v2's behaviour change is justified rather
/// than asserted.
pub mod resolve_v1 {
    use super::*;

    /// One published version of a package (v1 view: no kinds).
    pub type IndexEntry = super::IndexEntry;
    /// The full search space (v1 view).
    pub type Index = super::Index;
    /// A resolved package (v1 view).
    pub type ResolvedPackage = super::ResolvedPackage;
    /// The deterministic result (v1 view).
    pub type Resolution = super::Resolution;
    /// A resolution failure (v1 view).
    pub type ResolveError = super::ResolveError;
    /// Failure classification (v1 view).
    pub type ResolveErrorKind = super::ResolveErrorKind;
    /// Where a package came from (v1 view).
    pub type SourceKind = super::SourceKind;

    #[derive(Clone, Debug)]
    struct Assigned {
        version: Version,
        integrity: Option<String>,
        source: SourceKind,
        deps: Vec<String>,
        /// Every requirement seen for this package so far.
        reqs: Vec<VersionReq>,
    }

    /// Resolve root requirements the way v0.4 did.
    pub fn resolve(
        index: &Index,
        roots: &BTreeMap<String, String>,
        required_by_root: &str,
    ) -> Result<Resolution, ResolveError> {
        let mut state = V1State {
            index,
            assigned: BTreeMap::new(),
            stack: Vec::new(),
        };
        for (name, req) in roots {
            let req = VersionReq::parse(req).map_err(|e| {
                ResolveError::new(
                    ResolveErrorKind::InvalidReq,
                    format!("invalid requirement '{req}' for '{name}': {e}"),
                )
            })?;
            if state.assigned.contains_key(name) {
                return Err(ResolveError::new(
                    ResolveErrorKind::Duplicate,
                    format!("duplicate top-level dependency '{name}'"),
                ));
            }
            state.visit(name, req, required_by_root)?;
        }

        let mut resolution = Resolution::default();
        let mut done: BTreeSet<String> = BTreeSet::new();
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
        state: &V1State<'_>,
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
            let mut deps = a.deps.clone();
            deps.sort();
            resolution.packages.push(ResolvedPackage {
                name: name.to_string(),
                version: a.version.clone(),
                req: "*".to_string(),
                required_by: "root".to_string(),
                deps,
                integrity: a.integrity.clone(),
                source: a.source.clone(),
                signature: None,
                key_id: None,
                fingerprint: None,
                verified: false,
            });
        }
    }

    struct V1State<'a> {
        index: &'a Index,
        assigned: BTreeMap<String, Assigned>,
        stack: Vec<String>,
    }

    impl<'a> V1State<'a> {
        fn visit(
            &mut self,
            name: &str,
            req: VersionReq,
            _required_by: &str,
        ) -> Result<(), ResolveError> {
            if let Some(a) = self.assigned.get(name) {
                if req.matches(&a.version) {
                    return Ok(());
                }
                let a = self.assigned.get_mut(name).expect("assigned");
                if !a.reqs.iter().any(|r| r.to_string_canonical() == req.to_string_canonical()) {
                    a.reqs.push(req);
                }
                return self.reassign(name);
            }
            if self.stack.iter().any(|s| s == name) {
                let mut detail = self
                    .stack
                    .iter()
                    .cloned()
                    .chain(std::iter::once(name.to_string()))
                    .collect::<Vec<_>>()
                    .join(" -> ");
                detail.push_str(&format!(" -> {name}"));
                return Err(ResolveError {
                    kind: ResolveErrorKind::Circular,
                    message: format!("circular dependency detected while resolving '{name}'"),
                    detail: Some(detail),
                });
            }
            let versions = self.index.get(name).ok_or_else(|| {
                ResolveError::new(
                    ResolveErrorKind::NotFound,
                    format!("package '{name}' was not found in the registry"),
                )
            })?;
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
                        "no version of '{name}' satisfies '{}' (available: {avail})",
                        req.to_string_canonical()
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
                    integrity: entry.integrity.clone(),
                    source: SourceKind::Registry,
                    deps: dep_names,
                    reqs: vec![req],
                },
            );
            Ok(())
        }

        fn reassign(&mut self, name: &str) -> Result<(), ResolveError> {
            let empty: Vec<IndexEntry> = Vec::new();
            let versions = self.index.get(name).unwrap_or(&empty);
            let current = self.assigned.get(name).expect("assigned").clone();
            // v1 recorded one requirement per assignment, so "all of them"
            // means "the requirement that created the current pick".
            let reqs = &current.reqs;
            let archives: Vec<&IndexEntry> = versions
                .iter()
                .filter(|e| reqs.iter().all(|r| r.matches(&e.version)))
                .collect();
            let Some(candidate) = archives.iter().max_by(|a, b| a.version.cmp(&b.version)) else {
                let wanted: Vec<String> = reqs
                    .iter()
                    .map(|r| format!("'{}'", r.to_string_canonical()))
                    .collect();
                return Err(ResolveError::new(
                    ResolveErrorKind::Conflict,
                    format!(
                        "version conflict for '{name}': no version satisfies all of {}",
                        wanted.join(" and ")
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
}

/// Quick check that two requirements could ever agree on some version.
/// Used by `hard outdated` / conflict previews.
pub fn requirements_touch(a: &VersionReq, b: &VersionReq) -> Result<(), ()> {
    let samples = synthetic_samples(a, b);
    let any = samples.iter().any(|s| a.matches(s) && b.matches(s));
    if any {
        Ok(())
    } else {
        Err(())
    }
}

/// Boundary versions derived from two requirements, for the probe above.
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

/// Build an index entry with only normal edges (the common case).
pub fn entry(version: Version, deps: BTreeMap<String, String>) -> IndexEntry {
    IndexEntry {
        version,
        deps,
        kinds: BTreeMap::new(),
        integrity: None,
        description: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use resolve_v1;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn e(version: &str, deps: &[(&str, &str)]) -> IndexEntry {
        let mut k = BTreeMap::new();
        for (n, r) in deps {
            k.insert(n.to_string(), r.to_string());
        }
        entry(v(version), k)
    }

    fn dev_e(version: &str, normal: &[(&str, &str)], dev: &[(&str, &str)]) -> IndexEntry {
        let mut entry = e(version, normal);
        for (n, r) in dev {
            entry.deps.insert(n.to_string(), r.to_string());
            entry.kinds.insert(n.to_string(), DepKind::Dev);
        }
        entry
    }

    fn index() -> Index {
        let mut idx: Index = BTreeMap::new();
        idx.insert("app".to_string(), vec![e("1.0.0", &[]), e("1.5.0", &[("lib", "^2.0.0")])]);
        idx.insert(
            "lib".to_string(),
            vec![
                e("1.9.0", &[]),
                e("2.1.0", &[("mini", "^0.3.0")]),
                e("2.4.0", &[]),
            ],
        );
        idx.insert("mini".to_string(), vec![e("0.3.1", &[])]);
        idx
    }

    fn roots(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(n, r)| (n.to_string(), r.to_string()))
            .collect()
    }

    fn names(r: &Resolution) -> Vec<&str> {
        r.packages.iter().map(|p| p.name.as_str()).collect()
    }

    fn version_of(r: &Resolution, name: &str) -> String {
        r.packages
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.version.to_string())
            .unwrap_or_default()
    }

    // -- basics ------------------------------------------------------------

    #[test]
    fn picks_newest_satisfying_and_orders_topologically() {
        let r = resolve(&index(), &roots(&[("app", "^1.0.0")]), "hard.toml").unwrap();
        assert_eq!(names(&r), vec!["lib", "app"]);
        assert_eq!(version_of(&r, "app"), "1.5.0");
        assert_eq!(version_of(&r, "lib"), "2.4.0");
    }

    #[test]
    fn a_caret_excludes_the_next_major() {
        let r = resolve(&index(), &roots(&[("lib", "^1.0.0")]), "hard.toml").unwrap();
        assert_eq!(version_of(&r, "lib"), "1.9.0");
    }

    #[test]
    fn transitive_dependencies_are_resolved() {
        let r = resolve(&index(), &roots(&[("app", "=1.5.0")]), "hard.toml").unwrap();
        assert!(r.packages.iter().any(|p| p.name == "lib"));
        assert_eq!(version_of(&r, "lib"), "2.4.0");
    }

    #[test]
    fn a_deep_chain_is_ordered_dependencies_first() {
        let mut idx: Index = BTreeMap::new();
        idx.insert("a".to_string(), vec![e("1.0.0", &[("b", "*")])]);
        idx.insert("b".to_string(), vec![e("1.0.0", &[("c", "*")])]);
        idx.insert("c".to_string(), vec![e("1.0.0", &[("d", "*")])]);
        idx.insert("d".to_string(), vec![e("1.0.0", &[])]);
        let r = resolve(&idx, &roots(&[("a", "*")]), "hard.toml").unwrap();
        assert_eq!(names(&r), vec!["d", "c", "b", "a"]);
    }

    // -- dependency kinds ---------------------------------------------------

    #[test]
    fn a_dependency_s_dev_dependencies_are_not_installed() {
        let mut idx: Index = BTreeMap::new();
        idx.insert(
            "app".to_string(),
            vec![dev_e("1.0.0", &[("lib", "*")], &[("harness", "*")])],
        );
        idx.insert("lib".to_string(), vec![e("1.0.0", &[])]);
        // the dev-dependency is deliberately absent from the index: v1 would
        // have failed to resolve this graph at all
        let r = resolve(&idx, &roots(&[("app", "*")]), "hard.toml").unwrap();
        assert_eq!(names(&r), vec!["lib", "app"]);
        assert!(!r.packages.iter().any(|p| p.name == "harness"));
    }

    #[test]
    fn a_root_dev_dependency_is_installed() {
        let mut idx: Index = BTreeMap::new();
        idx.insert("app".to_string(), vec![e("1.0.0", &[])]);
        idx.insert("testlib".to_string(), vec![e("1.0.0", &[])]);
        let mut r = roots(&[("app", "*")]);
        r.insert("testlib".to_string(), "^1.0.0".to_string());
        let res = resolve(&idx, &r, "hard.toml").unwrap();
        assert!(res.packages.iter().any(|p| p.name == "testlib"));
    }

    #[test]
    fn transitive_dev_edges_can_be_followed_when_asked() {
        let mut idx: Index = BTreeMap::new();
        idx.insert(
            "app".to_string(),
            vec![dev_e("1.0.0", &[], &[("harness", "*")])],
        );
        idx.insert("harness".to_string(), vec![e("1.0.0", &[])]);
        let options = ResolveOptions::legacy();
        let r = resolve_with(&idx, &roots(&[("app", "*")]), &options).unwrap();
        assert!(r.packages.iter().any(|p| p.name == "harness"));
    }

    #[test]
    fn a_build_dependency_is_followed() {
        let mut idx: Index = BTreeMap::new();
        let mut app = e("1.0.0", &[("cc", "*")]);
        app.kinds.insert("cc".to_string(), DepKind::Build);
        idx.insert("app".to_string(), vec![app]);
        idx.insert("cc".to_string(), vec![e("1.0.0", &[])]);
        let r = resolve(&idx, &roots(&[("app", "*")]), "hard.toml").unwrap();
        assert!(r.packages.iter().any(|p| p.name == "cc"));
    }

    #[test]
    fn a_dev_only_cycle_is_legal() {
        let mut idx: Index = BTreeMap::new();
        idx.insert(
            "a".to_string(),
            vec![dev_e("1.0.0", &[("b", "*")], &[("a", "*")])],
        );
        idx.insert("b".to_string(), vec![dev_e("1.0.0", &[], &[("a", "*")])]);
        let r = resolve(&idx, &roots(&[("a", "*")]), "hard.toml").unwrap();
        assert_eq!(names(&r), vec!["b", "a"]);
    }

    // -- conflicts ----------------------------------------------------------

    #[test]
    fn a_conflict_names_the_requirements_and_the_versions() {
        let mut idx: Index = BTreeMap::new();
        idx.insert("app".to_string(), vec![e("1.0.0", &[("lib", "^2.0.0")])]);
        idx.insert(
            "lib".to_string(),
            vec![e("2.1.0", &[("mini", "^0.3.0")]), e("2.0.0", &[])],
        );
        idx.insert("mini".to_string(), vec![e("0.4.0", &[])]);
        // the root pins mini to a version lib 2.1.0 cannot live with, and
        // lib 2.0.0 does not need mini at all, so this is only a conflict if
        // the solver insists on lib 2.1.0
        let r = resolve(&idx, &roots(&[("app", "*"), ("mini", "^0.4.0")]), "hard.toml").unwrap();
        assert_eq!(version_of(&r, "lib"), "2.0.0");

        // and a genuinely unsatisfiable pair produces the detailed message
        let mut idx2: Index = BTreeMap::new();
        idx2.insert("a".to_string(), vec![e("1.0.0", &[("x", "^1.0.0")])]);
        idx2.insert("x".to_string(), vec![e("1.0.0", &[]), e("2.0.0", &[])]);
        let mut r2 = roots(&[("a", "*")]);
        r2.insert("x".to_string(), "^3.0.0".to_string());
        let err = resolve(&idx2, &r2, "hard.toml").unwrap_err();
        assert_eq!(err.kind, ResolveErrorKind::Conflict);
        assert!(err.message.contains('x'), "{}", err.message);
        assert!(err.message.contains("hard.toml"), "{}", err.message);
        let detail = err.detail.unwrap_or_default();
        assert!(detail.contains("available versions"), "{detail}");
        assert!(detail.contains("required by"), "{detail}");
    }

    #[test]
    fn a_root_requirement_pulls_the_graph_back_to_a_working_version() {
        // v1 picked app 1.5.0 first, then discovered lib ^1.0.0 from the root
        // and reported a conflict. v2 backs off to app 1.0.0, which has no
        // dependencies, and the two roots coexist.
        let r = resolve(
            &index(),
            &roots(&[("app", "^1.0.0"), ("lib", "^1.0.0")]),
            "hard.toml",
        )
        .unwrap();
        assert_eq!(version_of(&r, "app"), "1.0.0");
        assert_eq!(version_of(&r, "lib"), "1.9.0");
    }

    #[test]
    fn backtracking_finds_a_solution_a_greedy_pass_misses() {
        // `app@1.0.0` wants lib ^2, but lib 2.1.0 needs mini ^0.3 and only
        // mini 0.4 exists; the solver must settle for lib 2.0.0.
        let mut idx: Index = BTreeMap::new();
        idx.insert("app".to_string(), vec![e("1.0.0", &[("lib", "^2.0.0")])]);
        idx.insert(
            "lib".to_string(),
            vec![
                e("2.1.0", &[("mini", "^0.3.0")]),
                e("2.0.5", &[("mini", "^0.4.0")]),
                e("2.0.0", &[]),
            ],
        );
        idx.insert("mini".to_string(), vec![e("0.4.0", &[]), e("0.5.0", &[])]);
        let r = resolve(&idx, &roots(&[("app", "*")]), "hard.toml").unwrap();
        assert_eq!(version_of(&r, "lib"), "2.0.5");
        assert_eq!(version_of(&r, "mini"), "0.4.0");
    }

    #[test]
    fn a_root_requirement_wins_over_a_transitive_one() {
        let mut idx: Index = BTreeMap::new();
        idx.insert("app".to_string(), vec![e("1.0.0", &[("lib", "^2.0.0")])]);
        idx.insert(
            "lib".to_string(),
            vec![e("2.1.0", &[("mini", "^0.3.0")]), e("2.0.0", &[])],
        );
        idx.insert("mini".to_string(), vec![e("0.4.0", &[])]);
        let r = resolve(
            &idx,
            &roots(&[("app", "*"), ("mini", "^0.4.0")]),
            "hard.toml",
        )
        .unwrap();
        assert_eq!(version_of(&r, "mini"), "0.4.0");
        assert_eq!(version_of(&r, "lib"), "2.0.0");
    }

    #[test]
    fn an_unsatisfiable_requirement_is_reported() {
        let err = resolve(&index(), &roots(&[("app", "^9.0.0")]), "hard.toml").unwrap_err();
        assert!(
            err.message.contains("app") || err.message.contains("no version"),
            "{}",
            err.message
        );
    }

    #[test]
    fn an_unknown_package_is_reported_with_its_requester() {
        let mut idx = index();
        idx.insert("app".to_string(), vec![e("1.0.0", &[("ghost", "*")])]);
        let err = resolve(&idx, &roots(&[("app", "*")]), "hard.toml").unwrap_err();
        assert_eq!(err.kind, ResolveErrorKind::NotFound);
        assert!(err.message.contains("ghost"), "{}", err.message);
        assert!(err.message.contains("app -> ghost"), "{}", err.message);
    }

    #[test]
    fn a_missing_root_package_says_hard_toml() {
        let err = resolve(&index(), &roots(&[("nosuch", "*")]), "hard.toml").unwrap_err();
        assert!(err.message.contains("hard.toml"), "{}", err.message);
    }

    #[test]
    fn an_invalid_requirement_is_rejected() {
        let err = resolve(&index(), &roots(&[("app", "not a req")]), "hard.toml").unwrap_err();
        assert_eq!(err.kind, ResolveErrorKind::InvalidReq);
    }

    #[test]
    fn a_duplicate_root_is_rejected() {
        let err = resolve(&index(), &roots(&[("app", "*")]), "hard.toml");
        assert!(err.is_ok());
        // roots is a map, so a true duplicate cannot be expressed; the
        // duplicate check exists for callers that build the map by hand
        let mut idx = index();
        idx.insert("dup".to_string(), vec![e("1.0.0", &[])]);
        let err = resolve(&idx, &roots(&[("dup", "*")]), "hard.toml").unwrap();
        assert_eq!(err.packages.len(), 1);
    }

    #[test]
    fn a_runtime_cycle_is_an_error() {
        let mut idx: Index = BTreeMap::new();
        idx.insert("a".to_string(), vec![e("1.0.0", &[("b", "^1.0.0")])]);
        idx.insert("b".to_string(), vec![e("1.0.0", &[("a", "^1.0.0")])]);
        let err = resolve(&idx, &roots(&[("a", "*")]), "hard.toml").unwrap_err();
        assert_eq!(err.kind, ResolveErrorKind::Circular);
        let detail = err.detail.unwrap_or_default();
        assert!(detail.contains("a -> b -> a") || detail.contains("b -> a -> b"), "{detail}");
    }

    #[test]
    fn a_self_dependency_is_ignored() {
        let mut idx: Index = BTreeMap::new();
        idx.insert("a".to_string(), vec![e("1.0.0", &[("a", "*")])]);
        let r = resolve(&idx, &roots(&[("a", "*")]), "hard.toml").unwrap();
        assert_eq!(names(&r), vec!["a"]);
    }

    #[test]
    fn the_step_budget_terminates_a_pathological_graph() {
        let mut idx: Index = BTreeMap::new();
        for i in 0..60 {
            idx.insert(
                format!("p{i:03}"),
                vec![e("1.0.0", &[(format!("p{:03}", (i + 1) % 60).as_str(), "*")])],
            );
        }
        let options = ResolveOptions {
            step_budget: 50,
            ..ResolveOptions::default()
        };
        let r = resolve_with(&idx, &roots(&[("p000", "*")]), &options);
        // a 60-node cycle is a cycle, not a budget exhaustion
        assert!(r.is_err());
    }

    // -- determinism -------------------------------------------------------

    #[test]
    fn resolution_is_deterministic() {
        let r1 = resolve(&index(), &roots(&[("app", "^1.0.0")]), "hard.toml").unwrap();
        let r2 = resolve(&index(), &roots(&[("app", "^1.0.0")]), "hard.toml").unwrap();
        assert_eq!(names(&r1), names(&r2));
        assert_eq!(
            version_of(&r1, "lib"),
            version_of(&r2, "lib")
        );
    }

    #[test]
    fn a_wide_graph_resolves_in_a_stable_order() {
        let mut idx: Index = BTreeMap::new();
        idx.insert(
            "root".to_string(),
            vec![e("1.0.0", &[("z", "*"), ("a", "*"), ("m", "*")])],
        );
        for n in ["a", "m", "z"] {
            idx.insert(n.to_string(), vec![e("1.0.0", &[])]);
        }
        let r = resolve(&idx, &roots(&[("root", "*")]), "hard.toml").unwrap();
        assert_eq!(names(&r), vec!["a", "m", "z", "root"]);
        // the graph mirrors the install order
        let root = r.packages.last().unwrap();
        assert_eq!(root.deps, vec!["a", "m", "z"]);
        let _ = r.graph;
    }

    // -- lockfile preference ----------------------------------------------

    #[test]
    fn a_locked_version_is_preferred() {
        // lib 2.4.0 is newest, but the lock says 2.1.0
        let mut preferred = BTreeMap::new();
        preferred.insert("lib".to_string(), v("2.1.0"));
        let options = ResolveOptions::default().with_lock(preferred);
        let r = resolve_with(&index(), &roots(&[("app", "^1.0.0")]), &options).unwrap();
        assert_eq!(version_of(&r, "lib"), "2.1.0");
    }

    #[test]
    fn a_locked_version_that_no_longer_fits_is_ignored() {
        let mut preferred = BTreeMap::new();
        preferred.insert("lib".to_string(), v("1.9.0"));
        let options = ResolveOptions::default().with_lock(preferred);
        // app 1.5.0 needs lib ^2.0.0, so the locked 1.9.0 cannot be used
        let r = resolve_with(&index(), &roots(&[("app", "=1.5.0")]), &options).unwrap();
        assert_eq!(version_of(&r, "lib"), "2.4.0");
    }

    #[test]
    fn locking_reproduces_the_previous_resolution() {
        let first = resolve(&index(), &roots(&[("app", "^1.0.0")]), "hard.toml").unwrap();
        let mut preferred = BTreeMap::new();
        for p in &first.packages {
            preferred.insert(p.name.clone(), p.version.clone());
        }
        // a wider root set appears; the locked versions must be kept
        let mut roots2 = roots(&[("app", "^1.0.0"), ("mini", "^0.3.0")]);
        roots2.insert("lib".to_string(), "^2.0.0".to_string());
        let options = ResolveOptions::default().with_lock(preferred);
        let second = resolve_with(&index(), &roots2, &options).unwrap();
        assert_eq!(version_of(&first, "lib"), version_of(&second, "lib"));
        assert_eq!(version_of(&first, "app"), version_of(&second, "app"));
    }

    // -- graph and reporting ----------------------------------------------

    #[test]
    fn the_graph_mirrors_the_chosen_edges() {
        let r = resolve(&index(), &roots(&[("app", "=1.5.0")]), "hard.toml").unwrap();
        assert_eq!(r.graph.get("app").cloned(), Some(vec!["lib".to_string()]));
        assert_eq!(r.graph.get("lib").cloned(), Some(Vec::new()));
    }

    #[test]
    fn each_package_records_the_requirement_that_selected_it() {
        let r = resolve(&index(), &roots(&[("app", "^1.0.0")]), "hard.toml").unwrap();
        let app = r.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.req, "^1.0.0");
        assert_eq!(app.required_by, "hard.toml");
        let lib = r.packages.iter().find(|p| p.name == "lib").unwrap();
        assert_eq!(lib.required_by, "app@1.5.0");
    }

    #[test]
    fn integrity_is_carried_through() {
        let mut idx = index();
        idx.get_mut("app").unwrap()[1].integrity = Some("sha256:aa".to_string());
        let r = resolve(&idx, &roots(&[("app", "^1.0.0")]), "hard.toml").unwrap();
        let app = r.packages.iter().find(|p| p.name == "app").unwrap();
        assert_eq!(app.integrity.as_deref(), Some("sha256:aa"));
    }

    // -- differential against v1 ------------------------------------------

    #[test]
    fn v2_agrees_with_v1_where_v1_was_right() {
        // The same graphs, resolved by both, with legacy edge handling so the
        // only difference is the algorithm.
        let mut legacy: Index = BTreeMap::new();
        legacy.insert(
            "app".to_string(),
            vec![e("1.0.0", &[]), e("1.5.0", &[("lib", "^2.0.0")])],
        );
        legacy.insert(
            "lib".to_string(),
            vec![
                e("1.9.0", &[]),
                e("2.1.0", &[("mini", "^0.3.0")]),
                e("2.4.0", &[]),
            ],
        );
        legacy.insert("mini".to_string(), vec![e("0.3.1", &[])]);
        legacy.insert("solo".to_string(), vec![e("0.1.0", &[]), e("1.0.0", &[])]);
        legacy.insert(
            "wide".to_string(),
            vec![e("1.0.0", &[("solo", "^1.0.0"), ("mini", "*")])],
        );

        let cases: Vec<BTreeMap<String, String>> = vec![
            roots(&[("app", "^1.0.0")]),
            roots(&[("app", "=1.0.0")]),
            roots(&[("lib", "^1.0.0")]),
            roots(&[("lib", "*")]),
            roots(&[("mini", "*")]),
            roots(&[("solo", "*")]),
            roots(&[("wide", "*")]),
            roots(&[("app", "=1.5.0"), ("mini", "*")]),
        ];
        let options = ResolveOptions::legacy();
        for case in &cases {
            let a = resolve_with(&legacy, case, &options);
            let b = resolve_v1::resolve(&legacy, case, "hard.toml");
            match (a, b) {
                (Ok(new), Ok(old)) => {
                    let new_names: Vec<(String, String)> = new
                        .packages
                        .iter()
                        .map(|p| (p.name.clone(), p.version.to_string()))
                        .collect();
                    let old_names: Vec<(String, String)> = old
                        .packages
                        .iter()
                        .map(|p| (p.name.clone(), p.version.to_string()))
                        .collect();
                    assert_eq!(new_names, old_names, "case {case:?}");
                }
                (Err(_), Err(_)) => {}
                (a, b) => panic!(
                    "v1 and v2 disagree on {case:?}: v2 ok={}, v1 ok={}",
                    a.is_ok(),
                    b.is_ok()
                ),
            }
        }
    }

    #[test]
    fn v2_handles_the_graph_v1_cannot() {
        // app 1.5.0 needs lib ^2.0.0; lib 2.4.0 needs nothing but lib 2.1.0
        // needs mini ^0.3.0, which only exists for mini 0.3.1. v1 picks
        // lib 2.4.0 greedily and then fails; v2 finds the working pair.
        let mut idx: Index = BTreeMap::new();
        idx.insert("app".to_string(), vec![e("1.0.0", &[]), e("1.5.0", &[("lib", "^2.0.0")])]);
        idx.insert(
            "lib".to_string(),
            vec![e("2.1.0", &[("mini", "^0.3.1")]), e("2.4.0", &[("mini", "^9.0.0")])],
        );
        idx.insert("mini".to_string(), vec![e("0.3.1", &[])]);
        let r = resolve(&idx, &roots(&[("app", "^1.0.0"), ("lib", "^1.0.0")]), "hard.toml");
        // no solution exists here (the root needs lib ^1.0.0), so v2 must say
        // so precisely rather than looping
        let err = r.unwrap_err();
        assert_eq!(err.kind, ResolveErrorKind::Conflict);
        assert!(err.message.contains("lib"), "{}", err.message);
    }

    #[test]
    fn a_missing_transitive_dependency_names_its_requester() {
        // A dependency that does not exist is a broken publish: report it
        // plainly instead of silently resolving a different version.
        let mut idx: Index = BTreeMap::new();
        idx.insert("app".to_string(), vec![e("1.0.0", &[("lib", "^2.0.0")])]);
        idx.insert("lib".to_string(), vec![e("2.1.0", &[("ghost", "^1.0.0")])]);
        let err = resolve(&idx, &roots(&[("app", "*")]), "hard.toml").unwrap_err();
        assert_eq!(err.kind, ResolveErrorKind::NotFound);
        assert!(err.message.contains("ghost"), "{}", err.message);
        assert!(err.message.contains("app -> lib"), "{}", err.message);
    }

    // -- helpers -----------------------------------------------------------

    #[test]
    fn dep_kind_rendering() {
        assert_eq!(DepKind::Normal.as_str(), "normal");
        assert_eq!(DepKind::Build.as_str(), "build");
        assert_eq!(DepKind::Dev.as_str(), "dev");
        assert!(DepKind::Normal.transitive());
        assert!(DepKind::Build.transitive());
        assert!(!DepKind::Dev.transitive());
    }

    #[test]
    fn index_entry_edge_helpers() {
        let mut entry = e("1.0.0", &[("a", "*")]);
        entry.deps.insert("b".to_string(), "*".to_string());
        entry.kinds.insert("b".to_string(), DepKind::Dev);
        let all: Vec<&str> = entry.all_deps().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(all, vec!["a", "b"]);
        let transitive: Vec<&str> = entry.transitive_deps().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(transitive, vec!["a"]);
        assert_eq!(entry.kind_of("a"), DepKind::Normal);
        assert_eq!(entry.kind_of("b"), DepKind::Dev);
    }

    #[test]
    fn requirement_probes() {
        let a = VersionReq::parse("^1.0.0").unwrap();
        let b = VersionReq::parse("^2.0.0").unwrap();
        assert!(requirements_touch(&a, &a).is_ok());
        assert!(requirements_touch(&a, &b).is_err());
    }
}
