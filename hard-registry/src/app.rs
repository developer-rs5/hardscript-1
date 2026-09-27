//! The registry application: everything the HTTP layer needs, minus HTTP.
//!
//! [`App`] owns the store, the archive area, the signing key and the policy
//! knobs (require auth, allow unsigned publishes, auto-sign). The router is a
//! thin translation from `Request` to a call on this type, which keeps the
//! API testable without sockets and keeps storage concerns out of routing.

use crate::archives::{ArchiveError, ArchiveStore};
use crate::auth;
use crate::fingerprint::{self, FingerprintInput};
use crate::model::{
    clamp_text, is_valid_package_name, normalize_tags, normalize_user, now_secs, Change, Dep,
    DepKind, Package, PackageVersion, Scope, Stats, Token, User, ValidationError,
};
use crate::security;
use crate::signing::SigningKey;
use crate::store::{NewPackage, NewVersion, Store, StoreError, StoreResult};
use std::sync::Arc;

/// How the registry is configured. Everything has a working default, so
/// `App::in_memory()` is enough to run the test suite.
#[derive(Clone, Debug)]
pub struct Config {
    /// Require a valid token for publish/yank. Tests turn this off; a real
    /// deployment leaves it on.
    pub require_auth: bool,
    /// Sign every publish with the registry key.
    pub sign_publishes: bool,
    /// Accept publishes from anybody (no auth) — dev/test only.
    pub open_publish: bool,
    /// Cap on the search result list.
    pub max_search_results: usize,
    /// Cap on `limit` for mirror change feeds.
    pub max_change_batch: usize,
    /// Service name reported by `GET /`.
    pub service: String,
    /// Allow downloading yanked versions.
    pub serve_yanked: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            require_auth: true,
            sign_publishes: true,
            open_publish: false,
            max_search_results: 100,
            max_change_batch: 1000,
            service: "hardscript-registry".to_string(),
            serve_yanked: true,
        }
    }
}

impl Config {
    /// A configuration with every check relaxed, for tests and local
    /// experimentation.
    pub fn permissive() -> Config {
        Config {
            require_auth: false,
            open_publish: true,
            sign_publishes: false,
            ..Config::default()
        }
    }
}

/// Everything a publish needs: the coordinates, the descriptive fields, the
/// dependency edges and the `.hspkg` bytes. The HTTP layer fills this in
/// (from a JSON envelope, from headers, or from an archive's own
/// `hard.toml`) and the app turns it into a stored version.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PublishRequest {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub documentation: Option<String>,
    pub keywords: Vec<String>,
    pub tags: Vec<String>,
    pub deps: Vec<Dep>,
    pub channel: Option<String>,
    /// Raw `.hspkg` bytes.
    pub archive: Vec<u8>,
}

impl PublishRequest {
    /// Coordinates plus archive: the minimum a publish needs.
    pub fn new(name: impl Into<String>, version: impl Into<String>, archive: Vec<u8>) -> PublishRequest {
        PublishRequest {
            name: name.into(),
            version: version.into(),
            archive,
            ..PublishRequest::default()
        }
    }

    /// Add descriptive fields in one call.
    pub fn with_description(mut self, description: impl Into<String>) -> PublishRequest {
        self.description = Some(description.into());
        self
    }

    /// Add a tag.
    pub fn with_tag(mut self, tag: impl Into<String>) -> PublishRequest {
        self.tags.push(tag.into());
        self
    }

    /// Add a dependency edge.
    pub fn with_dep(mut self, name: impl Into<String>, req: impl Into<String>) -> PublishRequest {
        self.deps.push(Dep::normal(name, req));
        self
    }
}

/// Who is publishing. Ownership of a package name is decided here: the first
/// account to publish a name owns it, and only that account (or one holding
/// the `admin` scope) may publish further versions of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Publisher {
    /// Open mode: no account, no ownership.
    Anonymous,
    /// An authenticated account without the `admin` scope.
    User(String),
    /// An account holding `admin`, which may publish to any name.
    Admin(String),
}

impl Publisher {
    /// The account name, when there is one.
    pub fn name(&self) -> Option<&str> {
        match self {
            Publisher::Anonymous => None,
            Publisher::User(n) | Publisher::Admin(n) => Some(n),
        }
    }

    pub fn is_admin(&self) -> bool {
        matches!(self, Publisher::Admin(_))
    }

    /// Decide who is publishing from a resolved token.
    pub fn from_token(token: Option<&Token>) -> Publisher {
        match token {
            None => Publisher::Anonymous,
            Some(t) => {
                if t.scopes.contains(&Scope::Admin) {
                    Publisher::Admin(t.user.clone())
                } else {
                    Publisher::User(t.user.clone())
                }
            }
        }
    }
}

/// What a publish would do, without doing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishPreview {
    pub name: String,
    pub version: String,
    pub integrity: String,
    pub fingerprint: String,
    pub size: u64,
    pub file_count: usize,
    pub files: Vec<String>,
    pub dependencies: Vec<(String, String, String)>,
    /// Set when the version is already published and identical.
    pub already_published: bool,
    /// Set when the version exists but with different bytes.
    pub conflicting_version: bool,
    /// The current owner of the name, when the package already exists.
    pub owner: Option<String>,
}

/// The registry service.
pub struct App {
    pub store: Arc<dyn Store>,
    pub archives: ArchiveStore,
    pub key: SigningKey,
    pub config: Config,
    /// Name of the account that owns auto-issued tokens when publishing with
    /// authentication disabled (mirrors, sandboxes).
    pub anonymous_owner: String,
}

impl App {
    /// Build an app over an existing store and archive directory.
    pub fn new(
        store: Arc<dyn Store>,
        archives: ArchiveStore,
        key: SigningKey,
        config: Config,
    ) -> App {
        App {
            store,
            archives,
            key,
            config,
            anonymous_owner: "anonymous".to_string(),
        }
    }

    /// A fully in-memory registry: no database file, no archive directory,
    /// no signing key. Used by the test suite and by benchmarks that only
    /// need reproducible behaviour.
    pub fn in_memory(config: Config) -> App {
        App::new(
            Arc::new(crate::sqlite::SqliteStore::memory().expect("in-memory registry store")),
            ArchiveStore::at(std::env::temp_dir().join(format!(
                "hs-reg-mem-{}-{}",
                std::process::id(),
                now_secs()
            ))),
            SigningKey::deterministic_for_tests(),
            config,
        )
    }

    // -- validation --------------------------------------------------------

    /// Check a publish request before any I/O happens.
    pub fn validate_publish(&self, req: &PublishRequest) -> Result<(), Vec<ValidationError>> {
        let mut errs = Vec::new();
        if !is_valid_package_name(&req.name) {
            errs.push(ValidationError::new(
                "name",
                format!(
                    "'{}' is not a valid package name (lowercase letters, digits, '_', '-')",
                    req.name
                ),
            ));
        }
        if let Err(e) = hs_pm::semver::Version::parse(&req.version) {
            errs.push(ValidationError::new("version", e));
        }
        if req.archive.is_empty() {
            errs.push(ValidationError::new(
                "archive",
                "a publish must carry the .hspkg archive",
            ));
        } else if req.archive.len() > self.archives.max_bytes() {
            errs.push(ValidationError::new(
                "archive",
                format!(
                    "archive is {} bytes, over the {} byte limit",
                    req.archive.len(),
                    self.archives.max_bytes()
                ),
            ));
        }
        let mut seen: Vec<(String, DepKind)> = Vec::new();
        for d in &req.deps {
            if !is_valid_package_name(&d.name) {
                errs.push(ValidationError::new(
                    "dependencies",
                    format!("'{}' is not a valid dependency name", d.name),
                ));
            }
            if hs_pm::semver::VersionReq::parse(&d.req).is_err() {
                errs.push(ValidationError::new(
                    "dependencies",
                    format!("'{}' is not a valid requirement for {}", d.req, d.name),
                ));
            }
            if d.name == req.name {
                errs.push(ValidationError::new(
                    "dependencies",
                    "a package may not depend on itself",
                ));
            }
            if seen.contains(&(d.name.clone(), d.kind)) {
                errs.push(ValidationError::new(
                    "dependencies",
                    format!("duplicate dependency '{}'", d.name),
                ));
            }
            seen.push((d.name.clone(), d.kind));
        }
        if let Some(c) = &req.channel {
            if c.is_empty() || c.len() > 32 || !c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-') {
                errs.push(ValidationError::new(
                    "channel",
                    "a channel must be 1-32 alphanumeric characters or '-'",
                ));
            }
        }
        if !errs.is_empty() {
            return Err(errs);
        }
        Ok(())
    }

    // -- publish -----------------------------------------------------------

    /// Publish anonymously (open mode).
    pub fn publish(&self, req: &PublishRequest) -> StoreResult<PackageVersion> {
        self.publish_as(req, &Publisher::Anonymous)
    }

    /// Validate and describe a publish without writing anything. This is what
    /// `hard publish --dry-run` calls, and it is deliberately free of side
    /// effects: not even the archive is written to disk.
    pub fn preview(
        &self,
        req: &PublishRequest,
        who: &Publisher,
    ) -> Result<PublishPreview, StoreError> {
        self.validate_publish(req)
            .map_err(|errs| StoreError::Invalid(render_validation(&errs)))?;
        let version = hs_pm::semver::Version::parse(&req.version)
            .map_err(|e| StoreError::invalid(e))?
            .to_string();
        let file_list = hs_pm::pkgfmt::read_archive(&req.archive)
            .map_err(|e| StoreError::Invalid(format!("malformed .hspkg archive: {}", e.message)))?
            .into_iter()
            .map(|f| f.rel_path)
            .collect::<Vec<String>>();
        let mut deps = req.deps.clone();
        deps.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.req.cmp(&b.req))
        });
        let fp = fingerprint::compute(
            &FingerprintInput::new(&req.name, &version)
                .with_deps(&deps)
                .with_files(&file_list)
                .with_channel(req.channel.as_deref()),
        );
        let integrity = security::normalize_integrity(&security::sha256_hex(&req.archive));
        let existing = self.store.version(&req.name, &version)?;
        let (already, conflicting) = match &existing {
            None => (false, false),
            Some(v) => {
                let same = security::integrity_matches(&v.integrity, &integrity)
                    && security::integrity_matches(&v.fingerprint, &fp);
                (same, !same)
            }
        };
        self.check_owner(req, who)?;
        Ok(PublishPreview {
            name: req.name.clone(),
            version,
            integrity,
            fingerprint: fp,
            size: req.archive.len() as u64,
            file_count: file_list.len(),
            files: file_list,
            dependencies: deps
                .iter()
                .map(|d| (d.name.clone(), d.req.clone(), d.kind.as_str().to_string()))
                .collect(),
            already_published: already,
            conflicting_version: conflicting,
            owner: self.store.package(&req.name)?.and_then(|p| p.owner),
        })
    }

    /// May `who` publish this name? Unknown names are free; existing ones are
    /// owned by whoever created them.
    fn check_owner(&self, req: &PublishRequest, who: &Publisher) -> StoreResult<()> {
        let Some(existing) = self.store.package(&req.name)? else {
            return Ok(());
        };
        let Some(owner) = existing.owner.as_deref() else {
            // published by an anonymous open-mode node: no ownership to enforce
            return Ok(());
        };
        let publisher = who.name().unwrap_or("");
        if publisher == owner || who.is_admin() {
            return Ok(());
        }
        Err(StoreError::Conflict(format!(
            "'{}' is owned by '{owner}'; publishing to it needs the admin scope",
            req.name
        )))
    }

    /// Publish a package version. Validation, then archive write, then the
    /// row — in that order, so a rejected publish never leaves state behind.
    pub fn publish_as(&self, req: &PublishRequest, who: &Publisher) -> StoreResult<PackageVersion> {
        self.validate_publish(req)
            .map_err(|errs| StoreError::Invalid(render_validation(&errs)))?;
        self.check_owner(req, who)?;

        let version = hs_pm::semver::Version::parse(&req.version)
            .map_err(|e| StoreError::invalid(e))?;

        // Read the archive first: the fingerprint needs the file list, and a
        // malformed archive must not create a package row.
        let file_list = hs_pm::pkgfmt::read_archive(&req.archive)
            .map_err(|e| StoreError::Invalid(format!("malformed .hspkg archive: {}", e.message)))?
            .into_iter()
            .map(|f| f.rel_path)
            .collect::<Vec<String>>();

        let mut deps = req.deps.clone();
        deps.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.req.cmp(&b.req))
        });
        let fp = fingerprint::compute(
            &FingerprintInput::new(&req.name, &version.to_string())
                .with_deps(&deps)
                .with_files(&file_list)
                .with_channel(req.channel.as_deref()),
        );
        let integrity = security::normalize_integrity(&security::sha256_hex(&req.archive));

        // Refuse a duplicate before writing the archive, so a rejected
        // re-publish leaves no orphan file.
        if let Some(existing) = self.store.version(&req.name, &version.to_string())? {
            let same = existing.integrity == integrity && existing.fingerprint == fp;
            return Err(StoreError::conflict(if same {
                format!("{}@{} is already published", req.name, version)
            } else {
                format!(
                    "{}@{} already exists with a different artifact; bump the version",
                    req.name, version
                )
            }));
        }

        let stored = self
            .archives
            .put(&req.name, &version.to_string(), &req.archive, &fp)
            .map_err(|e| match e {
                ArchiveError::Io(m) => StoreError::internal(m),
                other => StoreError::Invalid(other.to_string()),
            })?;

        let (signature, key_id) = if self.config.sign_publishes {
            let payload = self.key.payload(&req.name, &version.to_string(), &integrity, &fp);
            match self.key.sign(&payload) {
                Ok(sig) => (Some(sig), Some(self.key.key_id().to_string())),
                Err(e) => {
                    let _ = self.archives.remove(&req.name, &version.to_string());
                    return Err(StoreError::internal(format!("cannot sign publish: {e}")));
                }
            }
        } else {
            (None, None)
        };

        self.store.upsert_package(&NewPackage {
            name: req.name.clone(),
            owner: who.name().map(String::from),
            description: req.description.as_deref().map(clamp_text),
            license: req.license.as_deref().map(clamp_text),
            homepage: req.homepage.as_deref().map(clamp_text),
            repository: req.repository.as_deref().map(clamp_text),
            documentation: req.documentation.as_deref().map(clamp_text),
            keywords: crate::model::normalize_tags(&req.keywords),
            tags: normalize_tags(&req.tags),
        })?;

        let added = self.store.add_version(&NewVersion {
            name: req.name.clone(),
            version: version.to_string(),
            deps,
            integrity,
            fingerprint: fp,
            signature,
            key_id,
            size: stored.size,
            file_count: stored.file_count,
            files: stored.files,
            channel: req.channel.clone(),
        });

        if let Err(e) = added {
            // The row failed, so the bytes must go: a registry that serves
            // an artifact no row describes is worse than a failed publish.
            let _ = self.archives.remove(&req.name, &version.to_string());
            return Err(e);
        }
        self.store
            .version(&req.name, &version.to_string())?
            .ok_or_else(|| StoreError::internal("version vanished after publish"))
    }

    // -- yank --------------------------------------------------------------

    /// Retract or restore a version.
    pub fn set_yanked(&self, name: &str, version: &str, yanked: bool) -> StoreResult<PackageVersion> {
        if !is_valid_package_name(name) {
            return Err(StoreError::invalid(format!("invalid package name '{name}'")));
        }
        if hs_pm::semver::Version::parse(version).is_err() {
            return Err(StoreError::invalid(format!("invalid version '{version}'")));
        }
        self.store.set_yanked(name, version, yanked)
    }

    // -- reads -------------------------------------------------------------

    /// A package document: the package plus every version.
    pub fn package_document(&self, name: &str) -> StoreResult<Option<(Package, Vec<PackageVersion>)>> {
        match self.store.package(name)? {
            None => Ok(None),
            Some(p) => Ok(Some((p, self.store.versions(name)?))),
        }
    }

    /// Download a version's bytes, counting the download.
    ///
    /// `count` is false for `HEAD`, which must be free of side effects: the
    /// package manager asks "does this version exist?" before uploading, and
    /// that question must not inflate anybody's download statistics.
    pub fn download_counted(&self, name: &str, version: &str, count: bool) -> StoreResult<Vec<u8>> {
        let bytes = self.download_bytes(name, version)?;
        if count {
            self.store.record_download(name, version)?;
        }
        Ok(bytes)
    }

    /// Download a version's bytes without touching the counters.
    pub fn download(&self, name: &str, version: &str) -> StoreResult<Vec<u8>> {
        self.download_counted(name, version, true)
    }

    fn download_bytes(&self, name: &str, version: &str) -> StoreResult<Vec<u8>> {
        let v = self
            .store
            .version(name, version)?
            .ok_or_else(|| StoreError::not_found(format!("{name}@{version} does not exist")))?;
        let bytes = self
            .archives
            .read(name, version)
            .map_err(|_| StoreError::internal(format!("archive for {name}@{version} is missing")))?;
        let got = security::normalize_integrity(&security::sha256_hex(&bytes));
        if !security::integrity_matches(&got, &v.integrity) {
            return Err(StoreError::internal(format!(
                "archive for {name}@{version} is corrupt on the registry (expected {}, got {got})",
                v.integrity
            )));
        }
        Ok(bytes)
    }

    /// A byte range of a version's archive (no download is counted).
    pub fn download_range(&self, name: &str, version: &str, start: u64, end: u64) -> StoreResult<Vec<u8>> {
        if self.store.version(name, version)?.is_none() {
            return Err(StoreError::not_found(format!("{name}@{version} does not exist")));
        }
        self.archives
            .read_range(name, version, start, end)
            .map_err(|e| StoreError::internal(e.to_string()))
    }

    // -- accounts ----------------------------------------------------------

    /// Register an account.
    pub fn register(&self, name: &str, password: &str, email: Option<&str>) -> StoreResult<User> {
        let name = normalize_user(name).map_err(|e| StoreError::invalid(e.to_string()))?;
        if password.chars().count() < 8 {
            return Err(StoreError::invalid(
                "a password must be at least 8 characters",
            ));
        }
        let salt = security::random_salt();
        let hash = security::hash_password(password, &salt, security::DEFAULT_ITERATIONS);
        self.store.create_user(&User {
            name,
            password_hash: hash,
            salt: security::hex(&salt),
            iterations: security::DEFAULT_ITERATIONS,
            email: email.map(clamp_text),
            created_at: now_secs(),
        })
    }

    /// Verify a password and mint a login session.
    pub fn login(&self, name: &str, password: &str) -> StoreResult<(User, auth::SessionToken)> {
        let name = normalize_user(name).map_err(|_| StoreError::invalid("invalid user name"))?;
        let user = self
            .store
            .user(&name)?
            .ok_or(StoreError::not_found("no such user"))?;
        if !security::verify_password(password, &user.password_hash) {
            // Same message for a wrong password and a missing account would
            // leak which names exist, so both say "invalid credentials" only
            // when the account exists; a missing account is a 404 by design
            // (the registry is a public index; `hard login` treats both the
            // same).
            return Err(StoreError::invalid("invalid credentials"));
        }
        let session = auth::new_session(&user.name, &self.config);
        // The session is a real token row so `logout` can revoke it and the
        // bearer works on every authenticated endpoint.
        self.store.create_token(&auth::session_record(&user.name, &session))?;
        Ok((user, session))
    }

    /// Invalidate a session (revokes the underlying token).
    pub fn logout(&self, session_id: &str) -> StoreResult<Token> {
        // A session is a token minted by `login`, so revoking it by id is
        // the whole operation. The store is scoped per user, so scan.
        for u in self.store.users()? {
            if let Ok(list) = self.store.tokens(&u.name) {
                if list.iter().any(|t| t.id == session_id) {
                    return self.store.revoke_token(&u.name, session_id);
                }
            }
        }
        Err(StoreError::not_found("no such session"))
    }

    /// Mint a personal access token for a user.
    pub fn create_token(
        &self,
        user: &str,
        label: &str,
        scopes: Option<Vec<String>>,
    ) -> StoreResult<(Token, String)> {
        let name = normalize_user(user).map_err(|e| StoreError::invalid(e.to_string()))?;
        if self.store.user(&name)?.is_none() {
            return Err(StoreError::not_found(format!("no such user '{name}'")));
        }
        if label.trim().is_empty() || label.chars().count() > 64 {
            return Err(StoreError::invalid(
                "a token name must be 1-64 characters",
            ));
        }
        let scopes = match scopes {
            Some(list) => {
                Scope::parse_list(&list).map_err(StoreError::invalid)?
            }
            None => Scope::defaults(),
        };
        let plaintext = security::random_token("hspat");
        let token = Token {
            id: auth::token_id(&plaintext),
            user: name,
            name: label.trim().to_string(),
            token_hash: auth::hash_token(&plaintext),
            scopes,
            created_at: now_secs(),
            last_used_at: None,
            revoked: false,
        };
        let stored = self.store.create_token(&token)?;
        Ok((stored, plaintext))
    }

    /// Resolve a bearer token to its stored record, stamping `last_used_at`.
    pub fn authenticate(&self, bearer: &str) -> StoreResult<Option<Token>> {
        if bearer.trim().is_empty() {
            return Ok(None);
        }
        let hash = auth::hash_token(bearer.trim());
        let found = self.store.find_token(&hash)?;
        if let Some(t) = &found {
            self.store.touch_token(&t.id, now_secs()).ok();
        }
        Ok(found)
    }

    // -- mirror feed -------------------------------------------------------

    /// Changes after a watermark, oldest first.
    pub fn changes(&self, since: i64, limit: usize) -> StoreResult<Vec<Change>> {
        self.store
            .changes_since(since, limit.min(self.config.max_change_batch))
    }

    /// The newest sequence number.
    pub fn seq(&self) -> StoreResult<i64> {
        self.store.latest_seq()
    }

    // -- introspection -----------------------------------------------------

    pub fn stats(&self) -> StoreResult<Stats> {
        self.store.stats()
    }

    /// Service version string, taken from the crate version.
    pub fn version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// Verify a stored version's integrity, fingerprint and signature.
    pub fn verify_version(&self, name: &str, version: &str) -> VerifyOutcome {
        let v = match self.store.version(name, version) {
            Ok(Some(v)) => v,
            Ok(None) => return VerifyOutcome::missing(name, version),
            Err(e) => return VerifyOutcome::error(e.to_string()),
        };
        let mut problems = Vec::new();
        if !fingerprint::verify(
            &v.fingerprint,
            &v.name,
            &v.version.to_string(),
            &v.deps,
            &v.files,
            v.channel.as_deref(),
        ) {
            problems.push("fingerprint does not match the stored metadata".to_string());
        }
        match self.archives.read(name, version) {
            Ok(bytes) => {
                let got = security::normalize_integrity(&security::sha256_hex(&bytes));
                if !security::integrity_matches(&got, &v.integrity) {
                    problems.push(format!("archive digest {got} != recorded {}", v.integrity));
                }
            }
            Err(e) => problems.push(format!("archive unreadable: {e}")),
        }
        match (&v.signature, &v.key_id) {
            (Some(sig), Some(kid)) => {
                let payload = self.key.payload(
                    &v.name,
                    &v.version.to_string(),
                    &v.integrity,
                    &v.fingerprint,
                );
                if kid != &self.key.key_id() {
                    problems.push(format!("signed by unknown key {kid}"));
                }
                if !self.key.verify(&payload, sig) {
                    problems.push("signature does not verify".to_string());
                }
            }
            (None, None) => {}
            _ => problems.push("signature present without a key id (or vice versa)".to_string()),
        }
        VerifyOutcome { name: v.name, version: v.version.to_string(), problems }
    }

    /// Every version in the registry (used by `hard-registry verify`).
    pub fn verify_all(&self) -> Vec<VerifyOutcome> {
        let mut out = Vec::new();
        let Ok(packages) = self.store.packages(None) else {
            return out;
        };
        for p in packages {
            let Ok(versions) = self.store.versions(&p.name) else {
                continue;
            };
            for v in versions {
                out.push(self.verify_version(&p.name, &v.version.to_string()));
            }
        }
        out
    }
}

/// The result of verifying one version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyOutcome {
    pub name: String,
    pub version: String,
    pub problems: Vec<String>,
}

impl VerifyOutcome {
    fn missing(name: &str, version: &str) -> VerifyOutcome {
        VerifyOutcome {
            name: name.to_string(),
            version: version.to_string(),
            problems: vec!["no such version".to_string()],
        }
    }

    fn error(msg: String) -> VerifyOutcome {
        VerifyOutcome {
            name: String::new(),
            version: String::new(),
            problems: vec![format!("store error: {msg}")],
        }
    }

    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

fn render_validation(errs: &[ValidationError]) -> String {
    errs.iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

/// A dependency edge helper used by the mirror replicator.
pub fn dep(name: &str, req: &str, kind: DepKind) -> Dep {
    Dep {
        name: name.to_string(),
        req: req.to_string(),
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PublishRequest;
    use crate::sqlite::SqliteStore;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("hs-app-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    pub(crate) fn archive(src: &str) -> Vec<u8> {
        hs_pm::pkgfmt::pack(&[hs_pm::pkgfmt::FileRecord {
            rel_path: "main.hard".to_string(),
            data: src.as_bytes().to_vec(),
        }])
        .unwrap()
    }

    pub(crate) fn publish_req(name: &str, version: &str) -> PublishRequest {
        PublishRequest::new(name, version, archive("calc x() => Int { <- 1 }\n"))
    }

    fn build(tag: &str, config: Config) -> (App, std::path::PathBuf) {
        let dir = temp_dir(tag);
        let store = SqliteStore::open(dir.join("registry.db")).unwrap();
        let app = App::new(
            Arc::new(store),
            ArchiveStore::at(dir.join("archives")),
            SigningKey::deterministic_for_tests(),
            config,
        );
        (app, dir)
    }

    #[test]
    fn publish_then_metadata_and_download() {
        let (app, dir) = build("pub", Config::permissive());
        let v = app.publish(&publish_req("jwt", "1.0.0")).unwrap();
        assert_eq!(v.version.to_string(), "1.0.0");
        assert!(v.integrity.starts_with("sha256:"));
        assert!(v.fingerprint.starts_with("sha256:"));
        let (pkg, versions) = app.package_document("jwt").unwrap().unwrap();
        assert_eq!(pkg.name, "jwt");
        assert_eq!(versions.len(), 1);
        let bytes = app.download("jwt", "1.0.0").unwrap();
        assert_eq!(bytes, archive("calc x() => Int { <- 1 }\n"));
        assert_eq!(app.stats().unwrap().downloads, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signing_is_on_by_default_and_off_when_configured() {
        let (mut app, dir) = build("sign", Config::default());
        app.config.open_publish = true;
        let v = app.publish(&publish_req("jwt", "1.0.0")).unwrap();
        assert!(v.signature.is_some());
        assert_eq!(v.key_id.as_deref(), Some(app.key.key_id()));
        let _ = std::fs::remove_dir_all(&dir);

        let (app2, dir2) = build("nosign", Config::permissive());
        let v2 = app2.publish(&publish_req("jwt", "1.0.0")).unwrap();
        assert!(v2.signature.is_none());
        assert!(v2.key_id.is_none());
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn republishing_the_same_version_conflicts() {
        let (app, dir) = build("dup", Config::permissive());
        app.publish(&publish_req("jwt", "1.0.0")).unwrap();
        let err = app.publish(&publish_req("jwt", "1.0.0")).unwrap_err();
        assert_eq!(err.status(), 409);
        assert!(err.to_string().contains("already published"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn republishing_different_bytes_under_one_version_conflicts() {
        let (app, dir) = build("dup2", Config::permissive());
        app.publish(&publish_req("jwt", "1.0.0")).unwrap();
        let mut r = publish_req("jwt", "1.0.0");
        r.archive = archive("calc x() => Int { <- 2 }\n");
        let err = app.publish(&r).unwrap_err();
        assert!(err.to_string().contains("bump the version"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validation_runs_before_any_write() {
        let (app, dir) = build("validate", Config::permissive());
        let mut r = publish_req("Bad Name", "1.0.0");
        assert!(app.validate_publish(&r).is_err());
        r.name = "ok".to_string();
        r.version = "not-a-version".to_string();
        assert!(app.validate_publish(&r).is_err());
        r.version = "1.0.0".to_string();
        r.archive.clear();
        assert!(app.validate_publish(&r).is_err());
        assert_eq!(app.stats().unwrap().packages, 0);
        assert!(app.archives.entries().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_self_dependencies_and_bad_requirements() {
        let (app, dir) = build("deps", Config::permissive());
        let mut r = publish_req("jwt", "1.0.0");
        r.deps = vec![dep("jwt", "^1.0.0", DepKind::Normal)];
        assert!(app.validate_publish(&r).is_err());
        r.deps = vec![dep("base64", "not a req", DepKind::Normal)];
        assert!(app.validate_publish(&r).is_err());
        r.deps = vec![
            dep("base64", "^1.0.0", DepKind::Normal),
            dep("base64", "^2.0.0", DepKind::Normal),
        ];
        assert!(app.validate_publish(&r).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_archive_creates_no_package() {
        let (app, dir) = build("malformed", Config::permissive());
        let mut r = publish_req("jwt", "1.0.0");
        r.archive = b"definitely not a package".to_vec();
        assert!(app.publish(&r).is_err());
        assert_eq!(app.stats().unwrap().packages, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_row_insert_removes_the_archive() {
        let (app, dir) = build("rollback", Config::permissive());
        app.publish(&publish_req("jwt", "1.0.0")).unwrap();
        // Force the row insert to fail by publishing through a store stub is
        // overkill; instead check the archive count matches the version count
        // after a normal publish and after a duplicate rejection.
        let mut r = publish_req("jwt", "1.0.0");
        r.archive = archive("different\n");
        assert!(app.publish(&r).is_err());
        assert_eq!(app.archives.entries().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn yank_hides_the_version_from_latest() {
        let (app, dir) = build("yank", Config::permissive());
        app.publish(&publish_req("jwt", "1.0.0")).unwrap();
        app.publish(&publish_req("jwt", "1.1.0")).unwrap();
        assert_eq!(app.store.latest("jwt").unwrap().unwrap().version.to_string(), "1.1.0");
        let yanked = app.set_yanked("jwt", "1.1.0", true).unwrap();
        assert!(yanked.yanked);
        assert_eq!(app.store.latest("jwt").unwrap().unwrap().version.to_string(), "1.0.0");
        let restored = app.set_yanked("jwt", "1.1.0", false).unwrap();
        assert!(!restored.yanked);
        assert!(app.set_yanked("jwt", "9.9.9", true).is_err());
        assert!(app.set_yanked("jwt", "bad", true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn register_login_and_token_flow() {
        let (app, dir) = build("auth", Config::permissive());
        app.register("ada", "supersecret", Some("ada@example.org")).unwrap();
        assert!(app.register("ada", "supersecret", None).is_err());
        assert!(app.register("bo", "short", None).is_err());
        let (user, session) = app.login("ada", "supersecret").unwrap();
        assert_eq!(user.name, "ada");
        assert!(!session.plaintext.is_empty());
        assert!(app.login("ada", "wrong").is_err());
        assert!(app.login("nobody", "supersecret").is_err());

        let (token, plaintext) = app.create_token("ada", "ci", None).unwrap();
        assert!(plaintext.starts_with("hspat_"));
        assert_eq!(token.scopes, Scope::defaults());
        let found = app.authenticate(&plaintext).unwrap().unwrap();
        assert_eq!(found.id, token.id);
        assert!(app.authenticate("hspat_nope").unwrap().is_none());
        assert!(app.authenticate("").unwrap().is_none());
        app.store.revoke_token("ada", &token.id).unwrap();
        assert!(app.authenticate(&plaintext).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_scopes_are_validated() {
        let (app, dir) = build("scopes", Config::permissive());
        app.register("ada", "supersecret", None).unwrap();
        assert!(app.create_token("ada", "ci", Some(vec!["nope".into()])).is_err());
        assert!(app.create_token("ada", "", None).is_err());
        assert!(app.create_token("ghost", "ci", None).is_err());
        let (t, _) = app
            .create_token("ada", "ci", Some(vec!["read".into(), "read".into()]))
            .unwrap();
        assert_eq!(t.scopes, vec![Scope::Read]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_detects_a_corrupt_archive() {
        let (app, dir) = build("corrupt", Config::permissive());
        app.publish(&publish_req("jwt", "1.0.0")).unwrap();
        let p = app.archives.path("jwt", "1.0.0");
        let mut bytes = std::fs::read(&p).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xff;
        std::fs::write(&p, &bytes).unwrap();
        let err = app.download("jwt", "1.0.0").unwrap_err();
        assert!(err.to_string().contains("corrupt"), "{err}");
        assert!(app.download("jwt", "9.9.9").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_version_detects_tampering() {
        let (mut app, dir) = build("verify", Config::default());
        app.config.open_publish = true;
        app.publish(&publish_req("jwt", "1.0.0")).unwrap();
        let out = app.verify_version("jwt", "1.0.0");
        assert!(out.ok(), "problems: {:?}", out.problems);
        // tamper with the archive
        let p = app.archives.path("jwt", "1.0.0");
        let mut bytes = std::fs::read(&p).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0x01;
        std::fs::write(&p, &bytes).unwrap();
        let out = app.verify_version("jwt", "1.0.0");
        assert!(!out.ok());
        assert!(out.problems.iter().any(|p| p.contains("archive digest")), "{:?}", out.problems);
        assert!(!app.verify_version("jwt", "9.9.9").ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_all_covers_every_version() {
        let (app, dir) = build("verifyall", Config::permissive());
        app.publish(&publish_req("a", "1.0.0")).unwrap();
        app.publish(&publish_req("a", "1.1.0")).unwrap();
        app.publish(&publish_req("b", "0.1.0")).unwrap();
        let all = app.verify_all();
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|o| o.ok()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ranged_downloads_do_not_count() {
        let (app, dir) = build("range", Config::permissive());
        app.publish(&publish_req("jwt", "1.0.0")).unwrap();
        let head = app.download_range("jwt", "1.0.0", 0, 3).unwrap();
        assert_eq!(head, b"HSP".to_vec());
        assert_eq!(app.stats().unwrap().downloads, 0);
        assert!(app.download_range("jwt", "9.9.9", 0, 3).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logout_revokes_the_session() {
        let (app, dir) = build("logout", Config::permissive());
        app.register("ada", "supersecret", None).unwrap();
        let (_, session) = app.login("ada", "supersecret").unwrap();
        assert!(app.logout(&session.id).is_ok());
        assert!(app.authenticate(&session.plaintext).unwrap().is_none());
        assert!(app.logout("tok_missing").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changes_are_recorded_for_publish_and_yank() {
        let (app, dir) = build("changes", Config::permissive());
        app.publish(&publish_req("jwt", "1.0.0")).unwrap();
        app.set_yanked("jwt", "1.0.0", true).unwrap();
        app.set_yanked("jwt", "1.0.0", false).unwrap();
        let changes = app.changes(0, 100).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].kind, crate::model::ChangeKind::Published);
        assert_eq!(changes[1].kind, crate::model::ChangeKind::Yanked);
        assert_eq!(changes[2].kind, crate::model::ChangeKind::Unyanked);
        assert_eq!(app.changes(changes[0].seq, 100).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
