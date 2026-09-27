//! Storage abstraction for the registry.
//!
//! [`Store`] is the only surface the HTTP layer talks to. v0.9 ships exactly
//! one implementation, [`crate::sqlite::SqliteStore`], but every method is
//! written in terms of [`crate::model`] types and plain parameters so a
//! PostgreSQL backend (the deployment target — SQLite is a single-node
//! convenience) can be added without touching the API, the CLI or the
//! tests. Nothing in the router may depend on SQL.
//!
//! Implementations must be safe to share across threads: the HTTP server is
//! thread-per-connection, so a store is typically wrapped in an `Arc` and
//! every method takes `&self`.

use crate::model::{Change, ChangeKind, Package, PackageVersion, Scope, Stats, Token, User};

/// Anything a backend can fail with. Deliberately coarse: the HTTP layer maps
/// this onto status codes, and a PostgreSQL backend should not need to leak
/// its own error taxonomy into the API.
#[derive(Clone, Debug)]
pub enum StoreError {
    /// The record already exists and the operation refused to overwrite it.
    Conflict(String),
    /// The record does not exist.
    NotFound(String),
    /// The backend rejected the request (bad column, constraint violation).
    Invalid(String),
    /// The backend itself failed.
    Internal(String),
}

impl StoreError {
    pub fn conflict(msg: impl Into<String>) -> StoreError {
        StoreError::Conflict(msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> StoreError {
        StoreError::NotFound(msg.into())
    }
    pub fn invalid(msg: impl Into<String>) -> StoreError {
        StoreError::Invalid(msg.into())
    }
    pub fn internal(msg: impl Into<String>) -> StoreError {
        StoreError::Internal(msg.into())
    }

    /// HTTP status the API should answer with.
    pub fn status(&self) -> u16 {
        match self {
            StoreError::Conflict(_) => 409,
            StoreError::NotFound(_) => 404,
            StoreError::Invalid(_) => 400,
            StoreError::Internal(_) => 500,
        }
    }

    /// Stable machine-readable error code for JSON error bodies.
    pub fn code(&self) -> &'static str {
        match self {
            StoreError::Conflict(_) => "conflict",
            StoreError::NotFound(_) => "not_found",
            StoreError::Invalid(_) => "invalid_request",
            StoreError::Internal(_) => "internal_error",
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Conflict(m) => write!(f, "{m}"),
            StoreError::NotFound(m) => write!(f, "{m}"),
            StoreError::Invalid(m) => write!(f, "{m}"),
            StoreError::Internal(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Result alias used across the storage layer.
pub type StoreResult<T> = Result<T, StoreError>;

/// A publish request as the store receives it (archive bytes are stored by
/// the caller; the store keeps metadata only).
#[derive(Clone, Debug)]
pub struct NewVersion {
    pub name: String,
    pub version: String,
    pub deps: Vec<crate::model::Dep>,
    pub integrity: String,
    pub fingerprint: String,
    pub signature: Option<String>,
    pub key_id: Option<String>,
    pub size: u64,
    pub file_count: usize,
    pub files: Vec<String>,
    pub channel: Option<String>,
}

/// A publish request for the package-level record.
#[derive(Clone, Debug, Default)]
pub struct NewPackage {
    pub name: String,
    /// Set only when the package is created; an existing package keeps its
    /// owner.
    pub owner: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub documentation: Option<String>,
    pub keywords: Vec<String>,
    pub tags: Vec<String>,
}

/// The registry's persistence contract.
///
/// Ordering guarantees: every list method returns rows sorted by a stable key
/// (package name, or version) so the JSON the API serves is byte-stable.
pub trait Store: Send + Sync {
    /// A short backend identifier (`sqlite`, `postgres`, ...), reported by
    /// `GET /` so operators can tell nodes apart.
    fn backend(&self) -> &'static str;

    // -- packages ----------------------------------------------------------

    /// Insert a package, or update its descriptive fields if it already
    /// exists. Returns the stored record. An existing package keeps its
    /// original `owner`.
    fn upsert_package(&self, pkg: &NewPackage) -> StoreResult<Package>;

    /// Fetch one package by exact name.
    fn package(&self, name: &str) -> StoreResult<Option<Package>>;

    /// Every package, sorted by name. `None` prefix = all.
    fn packages(&self, prefix: Option<&str>) -> StoreResult<Vec<Package>>;

    // -- versions ----------------------------------------------------------

    /// Insert a version plus its dependency edges. Refuses to overwrite an
    /// existing version (that is what re-publishing means: a new version).
    fn add_version(&self, v: &NewVersion) -> StoreResult<PackageVersion>;

    /// Fetch one version, or `None`.
    fn version(&self, name: &str, version: &str) -> StoreResult<Option<PackageVersion>>;

    /// Every version of a package, ascending.
    fn versions(&self, name: &str) -> StoreResult<Vec<PackageVersion>>;

    /// The newest non-yanked version of a package.
    fn latest(&self, name: &str) -> StoreResult<Option<PackageVersion>>;

    /// Set the yank flag on a version. Returns the updated record.
    fn set_yanked(&self, name: &str, version: &str, yanked: bool) -> StoreResult<PackageVersion>;

    /// Count a download of one version (package counter is bumped too).
    fn record_download(&self, name: &str, version: &str) -> StoreResult<()>;

    // -- change log --------------------------------------------------------

    /// Append a change-log entry, returning its sequence number.
    fn append_change(&self, name: &str, version: &str, kind: ChangeKind) -> StoreResult<i64>;

    /// Changes with `seq > since`, oldest first, capped at `limit`.
    fn changes_since(&self, since: i64, limit: usize) -> StoreResult<Vec<Change>>;

    /// The newest change-log sequence number (0 when empty).
    fn latest_seq(&self) -> StoreResult<i64>;

    // -- accounts and tokens -----------------------------------------------

    /// Create a user. Fails with [`StoreError::Conflict`] when taken.
    fn create_user(&self, user: &User) -> StoreResult<User>;

    /// Look up a user by exact (already normalized) name.
    fn user(&self, name: &str) -> StoreResult<Option<User>>;

    /// Every user, sorted by name.
    fn users(&self) -> StoreResult<Vec<User>>;

    /// Store a token. The plaintext is never persisted.
    fn create_token(&self, token: &Token) -> StoreResult<Token>;

    /// Find a token by its hashed plaintext. Used on every authenticated
    /// request; revokes are honoured here (revoked tokens never match).
    fn find_token(&self, token_hash: &str) -> StoreResult<Option<Token>>;

    /// Every token of one user, newest first.
    fn tokens(&self, user: &str) -> StoreResult<Vec<Token>>;

    /// Revoke a token owned by `user`. Fails when the token is unknown or
    /// belongs to somebody else.
    fn revoke_token(&self, user: &str, id: &str) -> StoreResult<Token>;

    /// Stamp `last_used_at` on a token (best effort; failures are ignored).
    fn touch_token(&self, id: &str, at: i64) -> StoreResult<()> {
        let _ = (id, at);
        Ok(())
    }

    // -- introspection -----------------------------------------------------

    /// Aggregate counters for `GET /stats`.
    fn stats(&self) -> StoreResult<Stats>;

    /// Number of users holding at least one unrevoked token, by scope set.
    /// Default implementation returns an empty map.
    fn scope_histogram(&self) -> StoreResult<std::collections::BTreeMap<String, usize>> {
        Ok(Default::default())
    }
}

/// Helper shared by implementations: filter a version list down to those
/// matching a requirement string, newest first.
pub fn matching<'a>(versions: &'a [PackageVersion], req: &str) -> Vec<&'a PackageVersion> {
    let mut out: Vec<&PackageVersion> = versions.iter().filter(|v| v.matches(req)).collect();
    out.sort_by(|a, b| b.version.cmp(&a.version));
    out
}

/// Helper: does a scope list contain `want` (honouring `admin`)?
pub fn has_scope(scopes: &[Scope], want: Scope) -> bool {
    scopes.iter().any(|s| s.implies(want))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(name: &str, version: &str, yanked: bool) -> PackageVersion {
        PackageVersion {
            name: name.to_string(),
            version: hs_pm::semver::Version::parse(version).unwrap(),
            deps: Vec::new(),
            integrity: "sha256:00".to_string(),
            fingerprint: "sha256:00".to_string(),
            signature: None,
            key_id: None,
            size: 0,
            file_count: 0,
            files: Vec::new(),
            channel: None,
            yanked,
            downloads: 0,
            published_at: 0,
        }
    }

    #[test]
    fn matching_filters_and_sorts_newest_first() {
        let all = vec![v("a", "1.0.0", false), v("a", "2.0.0", false), v("a", "2.1.0", true)];
        let hits = matching(&all, "^1.0.0");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].version.to_string(), "1.0.0");
        let all_hits = matching(&all, "*");
        assert_eq!(all_hits.len(), 3);
        assert_eq!(all_hits[0].version.to_string(), "2.1.0");
        assert!(matching(&all, "^9.0.0").is_empty());
    }

    #[test]
    fn scope_checks_honour_admin() {
        assert!(has_scope(&[Scope::Admin], Scope::Publish));
        assert!(has_scope(&[Scope::Publish], Scope::Publish));
        assert!(!has_scope(&[Scope::Read], Scope::Publish));
    }
}
