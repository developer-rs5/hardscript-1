//! Domain model for the HardScript package registry.
//!
//! Everything the REST API can express lives here: packages, versions,
//! dependency edges, users and personal access tokens. The model is
//! deliberately plain data (no database or HTTP types) so the same structs
//! are shared by the SQLite store, the mirror replicator, the JSON views and
//! the tests.

use hs_pm::semver::Version;
use std::collections::BTreeMap;

/// Largest archive the registry accepts, in bytes (16 MiB default).
pub const MAX_ARCHIVE_BYTES: usize = 16 * 1024 * 1024;
/// Largest number of versions one package may have.
pub const MAX_VERSIONS_PER_PACKAGE: usize = 10_000;
/// Longest accepted package name.
pub const MAX_NAME_LEN: usize = 128;
/// Longest accepted description / license / homepage string.
pub const MAX_TEXT_LEN: usize = 512;
/// Longest accepted tag or keyword.
pub const MAX_TAG_LEN: usize = 64;

/// Which dependency edge a version declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DepKind {
    /// A runtime dependency (`[dependencies]`).
    Normal,
    /// A development-only dependency (`[dev-dependencies]`).
    Dev,
    /// A build-time dependency (`[build-dependencies]`).
    Build,
}

impl DepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DepKind::Normal => "normal",
            DepKind::Dev => "dev",
            DepKind::Build => "build",
        }
    }

    pub fn parse(s: &str) -> DepKind {
        match s {
            "dev" => DepKind::Dev,
            "build" => DepKind::Build,
            _ => DepKind::Normal,
        }
    }
}

/// A single dependency edge `name = req` of one published version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dep {
    pub name: String,
    pub req: String,
    pub kind: DepKind,
}

impl Dep {
    pub fn normal(name: impl Into<String>, req: impl Into<String>) -> Dep {
        Dep {
            name: name.into(),
            req: req.into(),
            kind: DepKind::Normal,
        }
    }
}

/// The package-level record: everything shared by every version.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub description: Option<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub documentation: Option<String>,
    pub keywords: Vec<String>,
    pub tags: Vec<String>,
    /// Total downloads across every version.
    pub downloads: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Package {
    pub fn new(name: impl Into<String>) -> Package {
        let now = now_secs();
        Package {
            name: name.into(),
            created_at: now,
            updated_at: now,
            ..Package::default()
        }
    }
}

/// One published version of a package, including its archive digest,
/// manifest fingerprint and (when the registry signs publishes) signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageVersion {
    pub name: String,
    pub version: Version,
    pub deps: Vec<Dep>,
    /// `sha256:<hex>` of the archive bytes as served.
    pub integrity: String,
    /// Canonical manifest fingerprint (`sha256:<hex>`); see
    /// [`crate::fingerprint`].
    pub fingerprint: String,
    /// Base64 Ed25519 signature over the signing payload, when signed.
    pub signature: Option<String>,
    /// Key id of the signing key (`k:<16 hex>`).
    pub key_id: Option<String>,
    /// Archive size in bytes.
    pub size: u64,
    /// Number of files inside the archive.
    pub file_count: usize,
    /// Sorted list of paths inside the archive.
    pub files: Vec<String>,
    /// Optional release channel tag (`stable`, `beta`, `nightly`, ...).
    pub channel: Option<String>,
    /// Whether the version was retracted (`hard yank`).
    pub yanked: bool,
    pub downloads: u64,
    pub published_at: i64,
}

impl PackageVersion {
    /// Does `req` (a semver requirement string) select this version?
    pub fn matches(&self, req: &str) -> bool {
        match hs_pm::semver::VersionReq::parse(req) {
            Ok(r) => r.matches(&self.version),
            Err(_) => false,
        }
    }
}

/// A registry account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub name: String,
    /// PBKDF2-HMAC-SHA256 hash, hex encoded.
    pub password_hash: String,
    /// PBKDF2 salt, hex encoded.
    pub salt: String,
    /// PBKDF2 iteration count.
    pub iterations: u32,
    pub email: Option<String>,
    pub created_at: i64,
}

/// The permissions a token can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Read metadata, search and download.
    Read,
    /// Publish new versions.
    Publish,
    /// Yank / unyank versions.
    Yank,
    /// Create and revoke tokens.
    Token,
    /// Read the audit log.
    Admin,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Publish => "publish",
            Scope::Yank => "yank",
            Scope::Token => "token",
            Scope::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Scope> {
        match s.trim() {
            "read" => Some(Scope::Read),
            "publish" => Some(Scope::Publish),
            "yank" => Some(Scope::Yank),
            "token" => Some(Scope::Token),
            "admin" => Some(Scope::Admin),
            _ => None,
        }
    }

    /// Every scope, in canonical order.
    pub fn all() -> &'static [Scope] {
        &[Scope::Read, Scope::Publish, Scope::Yank, Scope::Token, Scope::Admin]
    }

    /// The scopes a freshly minted token gets.
    pub fn defaults() -> Vec<Scope> {
        vec![Scope::Read, Scope::Publish, Scope::Yank]
    }

    /// Every scope, including `admin`, as strings.
    pub fn all_names() -> Vec<&'static str> {
        Scope::all().iter().map(|s| s.as_str()).collect()
    }

    /// Render a scope list in canonical (sorted) order.
    pub fn render(scopes: &[Scope]) -> Vec<&'static str> {
        let mut v: Vec<Scope> = scopes.to_vec();
        v.sort();
        v.dedup();
        v.iter().map(|s| s.as_str()).collect()
    }

    /// Parse a scope list from strings, rejecting unknown names.
    pub fn parse_list(names: &[String]) -> Result<Vec<Scope>, String> {
        let mut out = Vec::new();
        for n in names {
            match Scope::parse(n) {
                Some(s) => out.push(s),
                None => {
                    return Err(format!(
                        "unknown scope '{n}' (valid: {})",
                        Scope::all_names().join(", ")
                    ))
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Does this scope imply `other`?
    pub fn implies(self, other: Scope) -> bool {
        if self == Scope::Admin {
            return true;
        }
        self == other
    }
}

/// A personal access token. Only the hash is stored; the plaintext is shown
/// once at creation time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// Stable public identifier (`tok_<16 hex>`).
    pub id: String,
    pub user: String,
    /// Human label, e.g. `ci-laptop`.
    pub name: String,
    /// SHA-256 of the token plaintext, hex encoded.
    pub token_hash: String,
    pub scopes: Vec<Scope>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked: bool,
}

/// A login session, minted by `POST /auth/login` and exchanged for a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub token: String,
    pub user: String,
    pub created_at: i64,
    pub expires_at: i64,
}

impl Session {
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }
}

/// One recorded state change, used by mirror replication and the audit log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// Monotonic sequence number; mirrors replicate from a watermark.
    pub seq: i64,
    pub package: String,
    pub version: String,
    pub kind: ChangeKind,
    pub at: i64,
}

/// What happened to a version at `seq`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Published,
    Yanked,
    Unyanked,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Published => "published",
            ChangeKind::Yanked => "yanked",
            ChangeKind::Unyanked => "unyanked",
        }
    }

    pub fn parse(s: &str) -> Option<ChangeKind> {
        match s {
            "published" => Some(ChangeKind::Published),
            "yanked" => Some(ChangeKind::Yanked),
            "unyanked" => Some(ChangeKind::Unyanked),
            _ => None,
        }
    }
}

/// Aggregate counters exposed by `GET /stats`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub packages: usize,
    pub versions: usize,
    pub yanked: usize,
    pub users: usize,
    pub tokens: usize,
    pub signed: usize,
    pub downloads: u64,
    pub archive_bytes: u64,
    /// Highest change-log sequence number.
    pub seq: i64,
}

/// A validation failure for a publish request or a registry record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

impl ValidationError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> ValidationError {
        ValidationError {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

/// Result alias for validation.
pub type Validated<T> = Result<T, Vec<ValidationError>>;

/// Package names: lowercase, `[a-z0-9_-]`, must start with a letter or `_`,
/// and may contain a single `@scope/` prefix. Mirrors npm/crates naming
/// rules so packages port across ecosystems.
pub fn is_valid_package_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return false;
    }
    let bare = match name.split_once('/') {
        Some((scope, rest)) => {
            if scope.is_empty() || rest.is_empty() || rest.contains('/') {
                return false;
            }
            rest
        }
        None => name,
    };
    let first = bare.chars().next().unwrap();
    if !(first.is_ascii_lowercase() || first == '_' || first.is_ascii_digit()) {
        return false;
    }
    bare.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Normalize a user name: trim, lowercase, reject anything exotic.
pub fn normalize_user(name: &str) -> Result<String, ValidationError> {
    let t = name.trim().to_ascii_lowercase();
    if t.is_empty() || t.len() > MAX_NAME_LEN {
        return Err(ValidationError::new(
            "user",
            "user name must be 1-128 characters",
        ));
    }
    let first = t.chars().next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit() || first == '_') {
        return Err(ValidationError::new(
            "user",
            "user names start with a lowercase letter, digit or '_'",
        ));
    }
    if !t
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(ValidationError::new(
            "user",
            "user names may only contain lowercase letters, digits, '_' and '-'",
        ));
    }
    Ok(t)
}

/// Sort helper: dependency maps rendered in a stable order.
pub fn sorted_deps(deps: &[Dep]) -> Vec<(String, String)> {
    let mut m: BTreeMap<String, String> = BTreeMap::new();
    for d in deps {
        m.entry(d.name.clone()).or_insert_with(|| d.req.clone());
    }
    m.into_iter().collect()
}

/// Seconds since the Unix epoch (registry timestamps).
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Truncate a text field to [`MAX_TEXT_LEN`], trimming trailing spaces.
pub fn clamp_text(s: &str) -> String {
    let t = s.trim();
    if t.chars().count() <= MAX_TEXT_LEN {
        return t.to_string();
    }
    t.chars().take(MAX_TEXT_LEN).collect()
}

/// Normalize a tag/keyword: lowercase, spaces to `-`, deduped and sorted.
pub fn normalize_tags(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in raw {
        for piece in t.split([',', ' ', '\t']) {
            let p = piece
                .trim()
                .to_ascii_lowercase()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                .collect::<String>();
            let p = p.trim_matches('-').to_string();
            if p.is_empty() || p.chars().count() > MAX_TAG_LEN {
                continue;
            }
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_crate_style_names() {
        for n in ["jwt", "hard-toml", "my_pkg", "v2", "acme/http"] {
            assert!(is_valid_package_name(n), "expected '{n}' to be valid");
        }
    }

    #[test]
    fn rejects_bad_names() {
        for n in ["", "Jwt", "-lead", "has space", "a/b/c", "UPPER", "sym!"] {
            assert!(!is_valid_package_name(n), "expected '{n}' to be invalid");
        }
        let long = "a".repeat(MAX_NAME_LEN + 1);
        assert!(!is_valid_package_name(&long));
    }

    #[test]
    fn user_names_are_normalized() {
        assert_eq!(normalize_user("  Ada ").unwrap(), "ada");
        assert_eq!(normalize_user("Bob99").unwrap(), "bob99");
        assert!(normalize_user("").is_err());
        assert!(normalize_user("Ada!").is_err());
    }

    #[test]
    fn scope_rendering_is_sorted_and_deduped() {
        let s = vec![Scope::Publish, Scope::Read, Scope::Publish];
        assert_eq!(Scope::render(&s), vec!["read", "publish"]);
        let parsed = Scope::parse_list(&["yank".to_string(), "read".to_string()]).unwrap();
        assert_eq!(Scope::render(&parsed), vec!["read", "yank"]);
        assert!(Scope::parse_list(&["nope".to_string()]).is_err());
    }

    #[test]
    fn admin_implies_everything() {
        for s in Scope::all() {
            assert!(Scope::Admin.implies(*s));
        }
        assert!(!Scope::Read.implies(Scope::Publish));
    }

    #[test]
    fn tags_are_normalized_and_sorted() {
        let raw = vec!["Web Server".to_string(), "http".to_string(), "http".to_string()];
        // separators split, and the result is deduped and sorted
        assert_eq!(normalize_tags(&raw), vec!["http", "server", "web"]);
        assert_eq!(normalize_tags(&["web server".to_string()]), vec!["server", "web"]);
        assert!(normalize_tags(&["!!!".to_string()]).is_empty());
    }

    #[test]
    fn text_is_clamped() {
        let long = "x".repeat(MAX_TEXT_LEN + 50);
        assert_eq!(clamp_text(&long).chars().count(), MAX_TEXT_LEN);
        assert_eq!(clamp_text("  hi  "), "hi");
    }
}
