//! Semantic versioning (SemVer 2.0.0) with a dependency-free implementation.
//!
//! Provides [`Version`] (parsing, precedence), [`VersionReq`] (requirement
//! strings: exact, caret `^`, tilde `~`, wildcard `x`/`*`, and comparator
//! sets joined by spaces/commas/`||`), deterministic ordering and the
//! pre-release compatibility rule used by the resolver.

use std::cmp::Ordering;
use std::fmt;

/// A parsed semantic version `major.minor.patch[-pre][+build]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Pre-release identifiers, in order (`["alpha", "1"]` for `-alpha.1`).
    pub pre: Vec<String>,
    /// Build metadata identifiers (ignored for precedence).
    pub build: Vec<String>,
}

impl Version {
    /// Parse a full `major.minor.patch` version string. Fails on any trailing
    /// garbage, missing components, or invalid identifiers.
    pub fn parse(s: &str) -> Result<Version, String> {
        let s = s.trim();
        let (core, pre, build) = split_pre_build(s);
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 {
            return Err(format!("'{}' is not a full X.Y.Z version", s));
        }
        let major = parse_num(parts[0]).ok_or_else(|| format!("invalid major in '{s}'"))?;
        let minor = parse_num(parts[1]).ok_or_else(|| format!("invalid minor in '{s}'"))?;
        let patch = parse_num(parts[2]).ok_or_else(|| format!("invalid patch in '{s}'"))?;
        let pre = parse_ids(pre, "pre-release")?;
        let build = parse_ids(build, "build")?;
        Ok(Version {
            major,
            minor,
            patch,
            pre,
            build,
        })
    }

    /// Lenient parse: `1`, `1.2` and `1.2.3` are all accepted (missing parts
    /// default to 0), matching the relaxed requirements users write in a
    /// manifest. Pre-release/build suffixes follow strict SemVer rules.
    pub fn parse_lenient(s: &str) -> Result<Version, String> {
        let s = s.trim();
        let (core, pre, build) = split_pre_build(s);
        let parts: Vec<&str> = core.split('.').collect();
        if parts.is_empty() || parts.len() > 3 {
            return Err(format!("'{}' is not a valid version", s));
        }
        let mut vals = [0u64; 3];
        for (i, p) in parts.iter().take(3).enumerate() {
            vals[i] = parse_num(p).ok_or_else(|| format!("invalid version component '{p}' in '{s}'"))?;
        }
        let pre = parse_ids(pre, "pre-release")?;
        let build = parse_ids(build, "build")?;
        Ok(Version {
            major: vals[0],
            minor: vals[1],
            patch: vals[2],
            pre,
            build,
        })
    }

    /// True when this is a stable release (no pre-release identifiers).
    pub fn is_stable(&self) -> bool {
        self.pre.is_empty()
    }

    /// The `major.minor.patch` tuple for pre-release compatibility checks.
    pub fn tuple(&self) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch)
    }
}

fn split_pre_build(s: &str) -> (&str, Option<&str>, Option<&str>) {
    let (no_build, build) = match s.find('+') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let (core, pre) = match no_build.find('-') {
        Some(i) => (&no_build[..i], Some(&no_build[i + 1..])),
        None => (no_build, None),
    };
    (core, pre, build)
}

fn parse_ids(s: Option<&str>, what: &str) -> Result<Vec<String>, String> {
    let Some(s) = s else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for id in s.split('.') {
        if id.is_empty() {
            return Err(format!("empty {what} identifier in '{s}'"));
        }
        let numeric = id.bytes().all(|b| b.is_ascii_digit());
        if numeric && id.len() > 1 && id.starts_with('0') {
            return Err(format!("{what} identifier '{id}' has a leading zero"));
        }
        if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(format!("invalid {what} identifier '{id}'"));
        }
        out.push(id.to_string());
    }
    Ok(out)
}

fn parse_num(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse().ok()
}

/// SemVer 2.0.0 precedence. Build metadata never participates.
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Version) -> Ordering {
        self.tuple().cmp(&other.tuple()).then_with(|| {
            // A release outranks its own pre-releases: 1.0.0-rc.1 < 1.0.0.
            // compare_ids alone would rank the shorter, empty list lower.
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => compare_ids(&self.pre, &other.pre),
            }
        })
    }
}

/// `[a]` < `[a.1]`; numeric identifiers sort below alphanumeric ones;
/// identifiers compare numerically when both are numeric.
fn compare_ids(a: &[String], b: &[String]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let xn = x.bytes().all(|c| c.is_ascii_digit());
        let yn = y.bytes().all(|c| c.is_ascii_digit());
        let ord = match (xn, yn) {
            (true, true) => {
                let (xv, yv) = (x.parse::<u64>().unwrap_or(0), y.parse::<u64>().unwrap_or(0));
                xv.cmp(&yv)
            }
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        if !self.build.is_empty() {
            write!(f, "+{}", self.build.join("."))?;
        }
        Ok(())
    }
}

type Ctor = fn(Version) -> Pred;

const CTORS: &[(&str, Ctor)] = &[
    ("^", Pred::Caret as Ctor),
    ("~", Pred::Tilde as Ctor),
    (">=", Pred::Ge as Ctor),
    ("<=", Pred::Le as Ctor),
    (">", Pred::Gt as Ctor),
    ("<", Pred::Lt as Ctor),
    ("=", Pred::Exact as Ctor),
];

/// A single constraint such as `^1.2.3`, `~0.4`, `1.x` or `>=1.0`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pred {
    Any,
    Exact(Version),
    Gt(Version),
    Ge(Version),
    Lt(Version),
    Le(Version),
    Caret(Version),
    Tilde(Version),
    Wildcard { major: u64, minor: Option<u64> },
}

/// A dependency requirement such as `^1.2.3`, `~0.4`, `1.x`, `*` or a
/// comparator set `>=1.0, <2.0`. Alternatives are unioned (`a || b`).
#[derive(Clone, Debug, Default)]
pub struct VersionReq {
    pub(crate) alts: Vec<Vec<Pred>>,
}

/// Is `s` made only of the components before a wildcard? Returns the wildcard
/// predicate when `s` is a bare wildcard core like `1`, `1.x` or `1.2.*`.
fn bare_wildcard(s: &str) -> Option<Pred> {
    let core = split_pre_build(s).0;
    let parts: Vec<&str> = core.split('.').collect();
    if parts.iter().all(|p| *p == "x" || *p == "X" || *p == "*") {
        return Some(Pred::Any);
    }
    if parts.iter().any(|p| *p == "x" || *p == "X" || *p == "*") {
        // wildcard must be the final component
        let pos = parts
            .iter()
            .position(|p| *p == "x" || *p == "X" || *p == "*")
            .unwrap();
        if parts[pos + 1..].iter().any(|p| *p != "x" && *p != "X" && *p != "*") {
            return None; // let the caller surface a parse error
        }
        let major = parts[0].parse::<u64>().ok()?;
        let minor = if pos >= 1 {
            Some(parts[1].parse::<u64>().ok()?)
        } else {
            None
        };
        if pos > 1 {
            return None;
        }
        return Some(Pred::Wildcard { major, minor });
    }
    None
}

fn parse_comp(s: &str, req_total: &str) -> Result<Pred, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err(format!("empty requirement in '{req_total}'"));
    }
    for (prefix, ctor) in CTORS {
        if let Some(rest) = s.strip_prefix(prefix) {
            if let Some(w) = bare_wildcard(rest) {
                // `^1.x` behaves like `^1` (upper bound = next major), `~1.x`
                // behaves like `^1` too, `>=1.x`/`<1.x` are non-sensical.
                match w {
                    Pred::Any => return Ok(ctor(Version::default())),
                    Pred::Wildcard { major, minor } => {
                        if *prefix == ">" || *prefix == "<" || *prefix == ">=" || *prefix == "<=" ||
                           *prefix == "=" {
                            return Err(format!(
                                "wildcard '{}' is not allowed with '{}'",
                                rest, prefix
                            ));
                        }
                        let mut v = Version::default();
                        v.major = major;
                        v.minor = minor.unwrap_or(0);
                        return Ok(ctor(v));
                    }
                    _ => unreachable!(),
                }
            }
            let v = Version::parse_lenient(rest)
                .map_err(|e| format!("invalid version in '{s}': {e}"))?;
            // Lenient pre-counts: `^1.2` expands to `^1.2.0`; a bare prefix
            // like `^1` keeps minor/patch at 0 which caret handles.
            return Ok(ctor(v));
        }
    }
    if let Some(w) = bare_wildcard(s) {
        return Ok(w);
    }
    if let Some(rest) = s.strip_prefix('v') {
        return parse_comp(rest, req_total);
    }
    // A bare version means "this version and newer patch/minor releases"
    // (caret semantics), the least surprising manifest default.
    let v = Version::parse_lenient(s)
        .map_err(|e| format!("invalid requirement '{s}': {e}; in '{req_total}'"))?;
    Ok(Pred::Caret(v))
}

impl VersionReq {
    /// Parse a requirement string. Empty input means `Any` (a bare `*`).
    pub fn parse(s: &str) -> Result<VersionReq, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(VersionReq {
                alts: vec![vec![Pred::Any]],
            });
        }
        let mut alts = Vec::new();
        for part in s.split("||") {
            let mut preds = Vec::new();
            for comp in part.split([',', ' ']) {
                let comp = comp.trim();
                if comp.is_empty() {
                    continue;
                }
                preds.push(parse_comp(comp, s)?);
            }
            if preds.is_empty() {
                return Err(format!("empty alternative in '{s}'"));
            }
            alts.push(preds);
        }
        if alts.is_empty() {
            return Err(format!("empty requirement '{s}'"));
        }
        Ok(VersionReq { alts })
    }

    /// The `major.minor.patch` tuple any comparator in this alternative
    /// mentions with a pre-release, if one exists (SemVer pre-release rule).
    fn pre_baseline(alt: &[Pred]) -> Option<(u64, u64, u64)> {
        for p in alt {
            let v = match p {
                Pred::Any | Pred::Wildcard { .. } => continue,
                Pred::Exact(v) | Pred::Gt(v) | Pred::Ge(v) | Pred::Lt(v) | Pred::Le(v)
                | Pred::Caret(v) | Pred::Tilde(v) => v,
            };
            if !v.pre.is_empty() {
                return Some(v.tuple());
            }
        }
        None
    }

    /// Does `v` satisfy any alternative in this requirement?
    pub fn matches(&self, v: &Version) -> bool {
        self.alts.iter().any(|alt| {
            let pre_ok =
                v.pre.is_empty() || Self::pre_baseline(alt).map(|t| t == v.tuple()).unwrap_or(false);
            pre_ok && alt.iter().all(|p| pred_matches(p, v))
        })
    }

    /// Pick the highest matching version from a (possibly unsorted) list.
    pub fn max_satisfying(&self, versions: &[Version]) -> Option<Version> {
        versions.iter().filter(|v| self.matches(v)).max().cloned()
    }

    /// True when at least one version in `versions` satisfies this request.
    pub fn satisfiable_by(&self, versions: &[Version]) -> bool {
        versions.iter().any(|v| self.matches(v))
    }

    /// Render the requirement in canonical form.
    pub fn to_string_canonical(&self) -> String {
        let alts: Vec<String> = self
            .alts
            .iter()
            .map(|alt| {
                if alt.len() == 1 {
                    pred_str(&alt[0])
                } else {
                    alt.iter().map(pred_str).collect::<Vec<_>>().join(", ")
                }
            })
            .collect();
        alts.join(" || ")
    }
}

fn pred_matches(p: &Pred, v: &Version) -> bool {
    match p {
        Pred::Any => true,
        Pred::Exact(e) => cmp_exact(v, e),
        Pred::Ge(e) => v >= e,
        Pred::Gt(e) => v > e,
        Pred::Le(e) => v <= e,
        Pred::Lt(e) => v < e,
        Pred::Caret(e) => caret_range(e)
            .map(|(lo, hi)| v >= &lo && v < &hi)
            .unwrap_or_else(|| cmp_exact(v, e)),
        Pred::Tilde(e) => tilde_range(e)
            .map(|(lo, hi)| v >= &lo && v < &hi)
            .unwrap_or_else(|| cmp_exact(v, e)),
        Pred::Wildcard { major, minor } => match minor {
            Some(m) => v.major == *major && v.minor == *m,
            None => v.major == *major,
        },
    }
}

/// `v` equals `e` ignoring build metadata only (pre is significant).
fn cmp_exact(v: &Version, e: &Version) -> bool {
    v.major == e.major && v.minor == e.minor && v.patch == e.patch && v.pre == e.pre
}

/// Caret range for a `^major.minor.patch` requirement. Mirrors the
/// well-known cargo behavior for 0.x releases; returns None only for the
/// degenerate `^0.0.0` (matches nothing but itself).
fn caret_range(e: &Version) -> Option<(Version, Version)> {
    let mut lo = e.clone();
    lo.build = Vec::new();
    let mut hi = lo.clone();
    hi.pre = Vec::new();
    hi.build = Vec::new();
    if e.major > 0 {
        hi.major += 1;
        hi.minor = 0;
        hi.patch = 0;
    } else if e.minor > 0 {
        hi.minor += 1;
        hi.patch = 0;
    } else {
        hi.patch += 1;
    }
    Some((lo, hi))
}

/// Tilde range: `~M.m.p` allows only patch releases (`<M.(m+1)`),
/// `~M.m` and `~M` lift the upper bound to the next minor/major.
fn tilde_range(e: &Version) -> Option<(Version, Version)> {
    let mut lo = e.clone();
    lo.build = Vec::new();
    let mut hi = lo.clone();
    hi.pre = Vec::new();
    hi.build = Vec::new();
    hi.minor += 1;
    hi.patch = 0;
    Some((lo, hi))
}

fn pred_str(p: &Pred) -> String {
    match p {
        Pred::Any => "*".to_string(),
        Pred::Exact(v) => v.to_string(),
        Pred::Ge(v) => format!(">={v}"),
        Pred::Gt(v) => format!(">{v}"),
        Pred::Le(v) => format!("<={v}"),
        Pred::Lt(v) => format!("<{v}"),
        Pred::Caret(v) => format!("^{v}"),
        Pred::Tilde(v) => format!("~{v}"),
        Pred::Wildcard { major, minor } => match minor {
            Some(m) => format!("{major}.{m}.x"),
            None => format!("{major}.x"),
        },
    }
}

impl fmt::Display for VersionReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_canonical())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_and_compare() {
        let a = Version::parse("1.2.3").unwrap();
        let b = Version::parse("1.2.4").unwrap();
        let c = Version::parse("2.0.0").unwrap();
        assert!(a < b);
        assert!(b < c);
        assert_eq!(Version::parse_lenient("1.2.3").unwrap(), a);
        assert_eq!(Version::parse("1.2.3-alpha.1").unwrap().tuple(), (1, 2, 3));
        assert!(a.is_stable());
        assert!(!Version::parse("1.2.3-beta").unwrap().is_stable());
    }

    #[test]
    fn req_caret_semantics() {
        let caret = VersionReq::parse("^1.2.0").unwrap();
        assert!(caret.matches(&Version::parse("1.2.0").unwrap()));
        assert!(caret.matches(&Version::parse("1.9.9").unwrap()));
        assert!(!caret.matches(&Version::parse("2.0.0").unwrap()));
        assert!(!caret.matches(&Version::parse("1.1.9").unwrap()));

        // leftmost non-zero component is locked
        let zero_minor = VersionReq::parse("^0.2.1").unwrap();
        assert!(zero_minor.matches(&Version::parse("0.2.9").unwrap()));
        assert!(!zero_minor.matches(&Version::parse("0.3.0").unwrap()));

        let zero_patch = VersionReq::parse("^0.0.3").unwrap();
        assert!(zero_patch.matches(&Version::parse("0.0.3").unwrap()));
        assert!(!zero_patch.matches(&Version::parse("0.0.4").unwrap()));
    }

    #[test]
    fn req_wildcards_and_ranges() {
        // a bare version expands to a caret (this version + patch/minor)
        let caret_bare = VersionReq::parse("1.2").unwrap();
        assert!(caret_bare.matches(&Version::parse("1.2.9").unwrap()));
        assert!(!caret_bare.matches(&Version::parse("2.0.0").unwrap()));

        let range = VersionReq::parse(">=1.0.0 <2.0.0").unwrap();
        assert!(range.matches(&Version::parse("1.8.1").unwrap()));
        assert!(!range.matches(&Version::parse("2.0.0").unwrap()));

        let star = VersionReq::parse("*").unwrap();
        assert!(star.matches(&Version::parse("0.0.1").unwrap()));
        assert!(star.matches(&Version::parse("99.0.0").unwrap()));

        let alternates = VersionReq::parse("^1.0.0 || ^2.0.0").unwrap();
        assert!(alternates.matches(&Version::parse("1.5.0").unwrap()));
        assert!(alternates.matches(&Version::parse("2.4.0").unwrap()));
        assert!(!alternates.matches(&Version::parse("3.0.0").unwrap()));
    }

    #[test]
    fn max_satisfying_selects_highest() {
        let req = VersionReq::parse("^1.0.0").unwrap();
        let versions = vec![
            Version::parse("1.0.0").unwrap(),
            Version::parse("1.2.0").unwrap(),
            Version::parse("1.2.5").unwrap(),
            Version::parse("2.0.0").unwrap(),
        ];
        assert_eq!(req.max_satisfying(&versions).unwrap().tuple(), (1, 2, 5));
        let nothing = VersionReq::parse("^3.0.0").unwrap();
        assert!(nothing.max_satisfying(&versions).is_none());
    }

    #[test]
    fn parse_lenient_accepts_common_shorthands() {
        assert_eq!(Version::parse_lenient("1").unwrap().tuple(), (1, 0, 0));
        assert_eq!(Version::parse_lenient("1.2").unwrap().tuple(), (1, 2, 0));
        assert_eq!(Version::parse_lenient("1.2.3-alpha.1").unwrap().tuple(), (1, 2, 3));
        assert!(Version::parse_lenient("nope").is_err());
    }

    #[test]
    fn sort_is_highest_first() {
        let mut vs = vec![
            Version::parse("1.0.0").unwrap(),
            Version::parse("1.0.2").unwrap(),
            Version::parse("1.0.10").unwrap(),
        ];
        vs.sort();
        assert_eq!(vs.last().unwrap().tuple(), (1, 0, 10)); // numeric, not lexicographic
    }
}
