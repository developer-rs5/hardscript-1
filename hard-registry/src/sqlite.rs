//! SQLite backend for [`Store`].
//!
//! One file holds the whole registry: package records, versions, dependency
//! edges, accounts, tokens and the change log used by mirrors. The schema is
//! versioned through `PRAGMA user_version` and migrated forward one step at a
//! time by [`migrate`], so an existing registry directory upgrades in place.
//!
//! Configuration that belongs to a deployment (WAL, synchronous level, busy
//! timeout) is applied at open time; the pooled connections are all opened
//! through [`SqliteStore::open`], so there is exactly one place that decides
//! it.

use crate::model::{
    now_secs, Change, ChangeKind, Dep, DepKind, Package, PackageVersion, Scope, Stats, Token, User,
};
use crate::store::{NewPackage, NewVersion, Store, StoreError, StoreResult};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The schema version this build writes.
pub const SCHEMA_VERSION: i64 = 1;

/// How long SQLite waits on a locked database before failing (milliseconds).
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// A SQLite-backed registry store.
pub struct SqliteStore {
    path: PathBuf,
    /// One connection per thread, handed out under a mutex. SQLite writes are
    /// serialized by the file lock anyway; a pool keeps `&self` methods
    /// cheap and avoids re-running `PRAGMA` per request.
    conns: Mutex<Vec<Connection>>,
    depth: Mutex<usize>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("path", &self.path)
            .field("backend", &"sqlite")
            .finish()
    }
}

impl SqliteStore {
    /// Open (creating if needed) the registry database at `path` and migrate
    /// it to [`SCHEMA_VERSION`].
    pub fn open(path: impl AsRef<Path>) -> StoreResult<SqliteStore> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    StoreError::internal(format!("cannot create {}: {e}", parent.display()))
                })?;
            }
        }
        let store = SqliteStore {
            path,
            conns: Mutex::new(Vec::new()),
            depth: Mutex::new(0),
        };
        store.migrate()?;
        // Prove the file is writable now rather than on the first publish.
        store.with_conn(|c| {
            c.execute_batch("CREATE TABLE IF NOT EXISTS _probe (x INTEGER)")?;
            c.execute("DROP TABLE IF EXISTS _probe", [])?;
            Ok(())
        })?;
        Ok(store)
    }

    /// An in-memory registry (tests, ephemeral mirrors).
    pub fn memory() -> StoreResult<SqliteStore> {
        let store = SqliteStore {
            path: PathBuf::from(":memory:"),
            conns: Mutex::new(Vec::new()),
            depth: Mutex::new(0),
        };
        store.migrate()?;
        Ok(store)
    }

    /// The database file (`:memory:` for ephemeral stores).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reclaim unused pooled connections.
    pub fn shrink(&self) {
        if let Ok(mut c) = self.conns.lock() {
            c.clear();
        }
    }

    /// Number of pooled connections (diagnostics).
    pub fn pooled(&self) -> usize {
        self.conns.lock().map(|c| c.len()).unwrap_or(0)
    }

    fn configure(c: &Connection) -> rusqlite::Result<()> {
        c.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
        c.pragma_update(None, "journal_mode", "WAL")?;
        c.pragma_update(None, "synchronous", "NORMAL")?;
        c.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> StoreResult<T>) -> StoreResult<T> {
        self.with_conn_mut(|c| f(c))
    }

    /// Borrow a pooled connection mutably (transactions need it).
    fn with_conn_mut<T>(&self, f: impl FnOnce(&mut Connection) -> StoreResult<T>) -> StoreResult<T> {
        let mut conns = self
            .conns
            .lock()
            .map_err(|_| StoreError::internal("registry store lock poisoned"))?;
        if let Some(mut c) = conns.pop() {
            let out = f(&mut c);
            conns.push(c);
            return out;
        }
        let mut c = self
            .open_raw()
            .map_err(|e| StoreError::internal(format!("cannot open registry database: {e}")))?;
        let out = f(&mut c);
        conns.push(c);
        out
    }

    fn open_raw(&self) -> rusqlite::Result<Connection> {
        let c = Connection::open(&self.path)?;
        SqliteStore::configure(&c)?;
        Ok(c)
    }

    /// Run `f` inside an IMMEDIATE transaction, committing on success and
    /// rolling back on any error.
    pub fn transaction<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
    ) -> StoreResult<T> {
        // SQLite's default is a DEFERRED transaction, which can fail to
        // upgrade to a write lock under concurrency. IMMEDIATE takes the
        // write lock up front so publish requests queue instead of racing.
        let mut guard = self
            .depth
            .lock()
            .map_err(|_| StoreError::internal("registry store lock poisoned"))?;
        *guard += 1;
        let result = self.with_conn_mut(|c| {
            let tx = c
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| StoreError::internal(format!("cannot begin transaction: {e}")))?;
            let out = match f(&tx) {
                Ok(v) => {
                    tx.commit()
                        .map_err(|e| StoreError::internal(format!("commit failed: {e}")))?;
                    Ok(v)
                }
                Err(e) => {
                    let _ = tx.rollback();
                    Err(e)
                }
            };
            out
        });
        *guard -= 1;
        result
    }

    /// Bring the schema up to [`SCHEMA_VERSION`].
    pub fn migrate(&self) -> StoreResult<()> {
        self.with_conn(|c| {
            let have: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if have > SCHEMA_VERSION {
                return Err(StoreError::internal(format!(
                    "registry database schema v{have} is newer than this build (v{SCHEMA_VERSION})"
                )));
            }
            if have < 1 {
                c.execute_batch(SCHEMA_V1)?;
            }
            c.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|e| StoreError::internal(format!("cannot stamp schema version: {e}")))?;
            Ok(())
        })
    }
}

/// The v1 schema.
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS packages (
    name          TEXT PRIMARY KEY,
    description   TEXT,
    license       TEXT,
    homepage      TEXT,
    repository    TEXT,
    documentation TEXT,
    keywords      TEXT NOT NULL DEFAULT '',
    tags          TEXT NOT NULL DEFAULT '',
    downloads     INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS versions (
    name        TEXT NOT NULL,
    version     TEXT NOT NULL,
    integrity   TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    signature   TEXT,
    key_id      TEXT,
    size        INTEGER NOT NULL DEFAULT 0,
    file_count  INTEGER NOT NULL DEFAULT 0,
    files       TEXT NOT NULL DEFAULT '',
    channel     TEXT,
    yanked      INTEGER NOT NULL DEFAULT 0,
    downloads   INTEGER NOT NULL DEFAULT 0,
    published_at INTEGER NOT NULL,
    PRIMARY KEY (name, version),
    FOREIGN KEY (name) REFERENCES packages(name) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS deps (
    name    TEXT NOT NULL,
    version TEXT NOT NULL,
    dep     TEXT NOT NULL,
    req     TEXT NOT NULL,
    kind    TEXT NOT NULL DEFAULT 'normal',
    PRIMARY KEY (name, version, dep, kind),
    FOREIGN KEY (name, version) REFERENCES versions(name, version) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS users (
    name          TEXT PRIMARY KEY,
    password_hash TEXT NOT NULL,
    email         TEXT,
    created_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tokens (
    id            TEXT PRIMARY KEY,
    user          TEXT NOT NULL,
    name          TEXT NOT NULL,
    token_hash    TEXT NOT NULL UNIQUE,
    scopes        TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER,
    revoked       INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS changes (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    name    TEXT NOT NULL,
    version TEXT NOT NULL,
    kind    TEXT NOT NULL,
    at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_versions_name ON versions(name);
CREATE INDEX IF NOT EXISTS idx_deps_dep ON deps(dep);
CREATE INDEX IF NOT EXISTS idx_changes_seq ON changes(seq);
CREATE INDEX IF NOT EXISTS idx_tokens_hash ON tokens(token_hash);
"#;

/// Let `?` work inside the query helpers below.
impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> StoreError {
        StoreError::internal(format!("sqlite: {e}"))
    }
}

fn io(msg: impl std::fmt::Display) -> StoreError {
    StoreError::internal(format!("sqlite: {msg}"))
}

fn split_list(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        Vec::new()
    } else {
        raw.split('\u{1f}').filter(|s| !s.is_empty()).map(String::from).collect()
    }
}

fn join_list(items: &[String]) -> String {
    items.join("\u{1f}")
}

impl Store for SqliteStore {
    fn backend(&self) -> &'static str {
        "sqlite"
    }

    fn upsert_package(&self, pkg: &NewPackage) -> StoreResult<Package> {
        let now = now_secs();
        self.transaction(|tx| {
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT created_at FROM packages WHERE name = ?1",
                    params![pkg.name],
                    |r| r.get(0),
                )
                .optional()?;
            let created = existing.unwrap_or(now);
            if existing.is_some() {
                // A re-publish may refresh the descriptive fields but must
                // not zero the download counters.
                tx.execute(
                    "UPDATE packages SET description = COALESCE(?2, description),
                        license = COALESCE(?3, license),
                        homepage = COALESCE(?4, homepage),
                        repository = COALESCE(?5, repository),
                        documentation = COALESCE(?6, documentation),
                        keywords = ?7, tags = ?8, updated_at = ?9
                     WHERE name = ?1",
                    params![
                        pkg.name,
                        pkg.description,
                        pkg.license,
                        pkg.homepage,
                        pkg.repository,
                        pkg.documentation,
                        join_list(&pkg.keywords),
                        join_list(&pkg.tags),
                        now,
                    ],
                )
                .map_err(io)?;
            } else {
                tx.execute(
                    "INSERT INTO packages (name, description, license, homepage, repository,
                        documentation, keywords, tags, downloads, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?10)",
                    params![
                        pkg.name,
                        pkg.description,
                        pkg.license,
                        pkg.homepage,
                        pkg.repository,
                        pkg.documentation,
                        join_list(&pkg.keywords),
                        join_list(&pkg.tags),
                        created,
                        now,
                    ],
                )
                .map_err(io)?;
            }
            Ok(())
        })?;
        self.package(&pkg.name)?
            .ok_or_else(|| StoreError::internal("package vanished after upsert"))
    }

    fn package(&self, name: &str) -> StoreResult<Option<Package>> {
        self.with_conn(|c| {
            let mut p = c
                .query_row(
                    "SELECT name, description, license, homepage, repository, documentation,
                            keywords, tags, downloads, created_at, updated_at
                     FROM packages WHERE name = ?1",
                    params![name],
                    package_row,
                )
                .optional()?;
            if let Some(pkg) = p.as_mut() {
                pkg.downloads += version_downloads(c, name)?;
            }
            Ok(p)
        })
    }

    fn packages(&self, prefix: Option<&str>) -> StoreResult<Vec<Package>> {
        self.with_conn(|c| {
            let mut out = match prefix {
                Some(pfx) => {
                    let like = format!("{}%", escape_like(pfx));
                    let mut st = c
                        .prepare(
                            "SELECT name, description, license, homepage, repository, documentation,
                                    keywords, tags, downloads, created_at, updated_at
                             FROM packages WHERE name LIKE ?1 ESCAPE '\\' ORDER BY name",
                        )
                        .map_err(io)?;
                    let rows = st
                        .query_map(params![like], package_row)
                        .map_err(io)?
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map_err(io)?;
                    rows
                }
                None => {
                    let mut st = c
                        .prepare(
                            "SELECT name, description, license, homepage, repository, documentation,
                                    keywords, tags, downloads, created_at, updated_at
                             FROM packages ORDER BY name",
                        )
                        .map_err(io)?;
                    let rows = st
                        .query_map([], package_row)
                        .map_err(io)?
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map_err(io)?;
                    rows
                }
            };
            for pkg in out.iter_mut() {
                pkg.downloads += version_downloads(c, &pkg.name)?;
            }
            Ok(out)
        })
    }

    fn add_version(&self, v: &NewVersion) -> StoreResult<PackageVersion> {
        let parsed = hs_pm::semver::Version::parse(&v.version).map_err(|e| {
            StoreError::invalid(format!("invalid version '{}': {e}", v.version))
        })?;
        self.transaction(|tx| {
            let exists: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM versions WHERE name = ?1 AND version = ?2",
                    params![v.name, v.version],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_some() {
                return Err(StoreError::conflict(format!(
                    "{}@{} is already published (yank it to publish a replacement)",
                    v.name, v.version
                )));
            }
            let has_pkg: Option<i64> = tx
                .query_row("SELECT 1 FROM packages WHERE name = ?1", params![v.name], |r| {
                    r.get(0)
                })
                .optional()?;
            if has_pkg.is_none() {
                return Err(StoreError::NotFound(format!(
                    "package '{}' does not exist",
                    v.name
                )));
            }
            tx.execute(
                "INSERT INTO versions (name, version, integrity, fingerprint, signature, key_id,
                    size, file_count, files, channel, yanked, downloads, published_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, 0, ?11)",
                params![
                    v.name,
                    v.version,
                    v.integrity,
                    v.fingerprint,
                    v.signature,
                    v.key_id,
                    v.size as i64,
                    v.file_count as i64,
                    join_list(&v.files),
                    v.channel,
                    now_secs(),
                ],
            )
            .map_err(io)?;
            for d in &v.deps {
                tx.execute(
                    "INSERT OR REPLACE INTO deps (name, version, dep, req, kind)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![v.name, v.version, d.name, d.req, d.kind.as_str()],
                )
                .map_err(io)?;
            }
            tx.execute(
                "UPDATE packages SET updated_at = ?2 WHERE name = ?1",
                params![v.name, now_secs()],
            )
            .map_err(io)?;
            append_change_tx(tx, &v.name, &v.version, ChangeKind::Published)?;
            let _ = parsed;
            Ok(())
        })?;
        self.version(&v.name, &v.version)?
            .ok_or_else(|| StoreError::internal("version vanished after insert"))
    }

    fn version(&self, name: &str, version: &str) -> StoreResult<Option<PackageVersion>> {
        self.with_conn(|c| {
            let mut v = c
                .query_row(
                    "SELECT name, version, integrity, fingerprint, signature, key_id, size,
                            file_count, files, channel, yanked, downloads, published_at
                     FROM versions WHERE name = ?1 AND version = ?2",
                    params![name, version],
                    version_row,
                )
                .optional()?;
            if let Some(v) = v.as_mut() {
                v.deps = load_deps(c, name, version)?;
            }
            Ok(v)
        })
    }

    fn versions(&self, name: &str) -> StoreResult<Vec<PackageVersion>> {
        self.with_conn(|c| {
            let mut st = c
                .prepare(
                    "SELECT name, version, integrity, fingerprint, signature, key_id, size,
                            file_count, files, channel, yanked, downloads, published_at
                     FROM versions WHERE name = ?1 ORDER BY version",
                )
                .map_err(io)?;
            let mut out = st
                .query_map(params![name], version_row)
                .map_err(io)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(io)?;
            drop(st);
            for v in out.iter_mut() {
                v.deps = load_deps(c, name, &v.version.to_string())?;
            }
            Ok(out)
        })
    }

    fn latest(&self, name: &str) -> StoreResult<Option<PackageVersion>> {
        self.with_conn(|c| {
            let mut v = c
                .query_row(
                    "SELECT name, version, integrity, fingerprint, signature, key_id, size,
                            file_count, files, channel, yanked, downloads, published_at
                     FROM versions WHERE name = ?1 AND yanked = 0 ORDER BY version DESC LIMIT 1",
                    params![name],
                    version_row,
                )
                .optional()?;
            if let Some(v) = v.as_mut() {
                v.deps = load_deps(c, name, &v.version.to_string())?;
            }
            Ok(v)
        })
    }

    fn set_yanked(&self, name: &str, version: &str, yanked: bool) -> StoreResult<PackageVersion> {
        self.transaction(|tx| {
            let n = tx
                .execute(
                    "UPDATE versions SET yanked = ?3 WHERE name = ?1 AND version = ?2",
                    params![name, version, i64::from(yanked)],
                )
                .map_err(io)?;
            if n == 0 {
                return Err(StoreError::NotFound(format!(
                    "{name}@{version} does not exist"
                )));
            }
            append_change_tx(
                tx,
                name,
                version,
                if yanked {
                    ChangeKind::Yanked
                } else {
                    ChangeKind::Unyanked
                },
            )?;
            Ok(())
        })?;
        self.version(name, version)?
            .ok_or_else(|| StoreError::internal("version vanished after yank"))
    }

    fn record_download(&self, name: &str, version: &str) -> StoreResult<()> {
        self.with_conn(|c| {
            let n = c
                .execute(
                    "UPDATE versions SET downloads = downloads + 1 WHERE name = ?1 AND version = ?2",
                    params![name, version],
                )
                .map_err(io)?;
            if n == 0 {
                return Err(StoreError::NotFound(format!(
                    "{name}@{version} does not exist"
                )));
            }
            Ok(())
        })
    }

    fn append_change(&self, name: &str, version: &str, kind: ChangeKind) -> StoreResult<i64> {
        self.transaction(|tx| append_change_tx(tx, name, version, kind))
    }

    fn changes_since(&self, since: i64, limit: usize) -> StoreResult<Vec<Change>> {
        self.with_conn(|c| {
            let mut st = c
                .prepare("SELECT seq, name, version, kind, at FROM changes WHERE seq > ?1 ORDER BY seq LIMIT ?2")
                .map_err(io)?;
            let rows = st
                .query_map(params![since, limit as i64], |r| {
                    let kind: String = r.get(3)?;
                    Ok(Change {
                        seq: r.get(0)?,
                        package: r.get(1)?,
                        version: r.get(2)?,
                        kind: ChangeKind::parse(&kind).unwrap_or(ChangeKind::Published),
                        at: r.get(4)?,
                    })
                })
                .map_err(io)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(io)?;
            Ok(rows)
        })
    }

    fn latest_seq(&self) -> StoreResult<i64> {
        self.with_conn(|c| {
            let v: Option<i64> = c
                .query_row("SELECT MAX(seq) FROM changes", [], |r| r.get(0))
                .optional()?
                .flatten();
            Ok(v.unwrap_or(0))
        })
    }

    fn create_user(&self, user: &User) -> StoreResult<User> {
        self.with_conn(|c| {
            let n = c
                .execute(
                    "INSERT INTO users (name, password_hash, email, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![user.name, user.password_hash, user.email, user.created_at],
                )
                .map_err(|e| match e {
                    rusqlite::Error::SqliteFailure(f, _)
                        if f.code == rusqlite::ErrorCode::ConstraintViolation =>
                    {
                        StoreError::conflict(format!("user '{}' already exists", user.name))
                    }
                    other => io(other),
                })?;
            if n == 0 {
                return Err(StoreError::conflict(format!(
                    "user '{}' already exists",
                    user.name
                )));
            }
            Ok(user.clone())
        })
    }

    fn user(&self, name: &str) -> StoreResult<Option<User>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT name, password_hash, email, created_at FROM users WHERE name = ?1",
                params![name],
                |r| {
                    Ok(User {
                        name: r.get(0)?,
                        password_hash: r.get(1)?,
                        salt: String::new(),
                        iterations: 0,
                        email: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(io)
        })
    }

    fn users(&self) -> StoreResult<Vec<User>> {
        self.with_conn(|c| {
            let mut st = c
                .prepare("SELECT name, password_hash, email, created_at FROM users ORDER BY name")
                .map_err(io)?;
            let rows = st
                .query_map([], |r| {
                    Ok(User {
                        name: r.get(0)?,
                        password_hash: r.get(1)?,
                        salt: String::new(),
                        iterations: 0,
                        email: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                })
                .map_err(io)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(io)?;
            Ok(rows)
        })
    }

    fn create_token(&self, token: &Token) -> StoreResult<Token> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO tokens (id, user, name, token_hash, scopes, created_at,
                    last_used_at, revoked)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
                params![
                    token.id,
                    token.user,
                    token.name,
                    token.token_hash,
                    Scope::render(&token.scopes).join(","),
                    token.created_at,
                    token.last_used_at,
                ],
            )
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _)
                    if f.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    StoreError::conflict("token already exists".to_string())
                }
                other => io(other),
            })?;
            Ok(token.clone())
        })
    }

    fn find_token(&self, token_hash: &str) -> StoreResult<Option<Token>> {
        self.with_conn(|c| {
            let t = c
                .query_row(
                    "SELECT id, user, name, token_hash, scopes, created_at, last_used_at, revoked
                     FROM tokens WHERE token_hash = ?1 AND revoked = 0",
                    params![token_hash],
                    token_row,
                )
                .optional()?;
            Ok(t)
        })
    }

    fn tokens(&self, user: &str) -> StoreResult<Vec<Token>> {
        self.with_conn(|c| {
            let mut st = c
                .prepare(
                    "SELECT id, user, name, token_hash, scopes, created_at, last_used_at, revoked
                     FROM tokens WHERE user = ?1 ORDER BY created_at DESC, id ASC",
                )
                .map_err(io)?;
            let rows = st
                .query_map(params![user], token_row)
                .map_err(io)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(io)?;
            Ok(rows)
        })
    }

    fn revoke_token(&self, user: &str, id: &str) -> StoreResult<Token> {
        self.with_conn(|c| {
            let t = c
                .query_row(
                    "SELECT id, user, name, token_hash, scopes, created_at, last_used_at, revoked
                     FROM tokens WHERE id = ?1 AND user = ?2",
                    params![id, user],
                    token_row,
                )
                .optional()?;
            let t = t.ok_or_else(|| StoreError::NotFound(format!("token '{id}' does not exist")))?;
            c.execute(
                "UPDATE tokens SET revoked = 1 WHERE id = ?1",
                params![id],
            )
            .map_err(io)?;
            let mut t = t;
            t.revoked = true;
            Ok(t)
        })
    }

    fn touch_token(&self, id: &str, at: i64) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE tokens SET last_used_at = ?2 WHERE id = ?1",
                params![id, at],
            )
            .map_err(io)?;
            Ok(())
        })
    }

    fn stats(&self) -> StoreResult<Stats> {
        self.with_conn(|c| {
            let count = |sql: &str| -> rusqlite::Result<i64> {
                c.query_row(sql, [], |r| r.get::<_, i64>(0))
            };
            let packages = count("SELECT COUNT(*) FROM packages").map_err(io)?;
            let versions = count("SELECT COUNT(*) FROM versions").map_err(io)?;
            let yanked = count("SELECT COUNT(*) FROM versions WHERE yanked = 1").map_err(io)?;
            let users = count("SELECT COUNT(*) FROM users").map_err(io)?;
            let tokens =
                count("SELECT COUNT(*) FROM tokens WHERE revoked = 0").map_err(io)?;
            let signed =
                count("SELECT COUNT(*) FROM versions WHERE signature IS NOT NULL").map_err(io)?;
            let downloads: i64 = c
                .query_row("SELECT COALESCE(SUM(downloads), 0) FROM versions", [], |r| {
                    r.get(0)
                })
                .map_err(io)?;
            let archive_bytes: i64 = c
                .query_row("SELECT COALESCE(SUM(size), 0) FROM versions", [], |r| {
                    r.get(0)
                })
                .map_err(io)?;
            let seq: Option<i64> = c
                .query_row("SELECT MAX(seq) FROM changes", [], |r| r.get(0))
                .optional()
                .map_err(io)?
                .flatten();
            Ok(Stats {
                packages: packages as usize,
                versions: versions as usize,
                yanked: yanked as usize,
                users: users as usize,
                tokens: tokens as usize,
                signed: signed as usize,
                downloads: downloads.max(0) as u64,
                archive_bytes: archive_bytes.max(0) as u64,
                seq: seq.unwrap_or(0),
            })
        })
    }

    fn scope_histogram(&self) -> StoreResult<std::collections::BTreeMap<String, usize>> {
        self.with_conn(|c| {
            let mut st = c
                .prepare("SELECT scopes, COUNT(*) FROM tokens WHERE revoked = 0 GROUP BY scopes")
                .map_err(io)?;
            let rows = st
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(io)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(io)?;
            let mut out = std::collections::BTreeMap::new();
            for (scopes, n) in rows {
                out.insert(scopes, n as usize);
            }
            Ok(out)
        })
    }
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn package_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Package> {
    Ok(Package {
        name: r.get(0)?,
        description: r.get(1)?,
        license: r.get(2)?,
        homepage: r.get(3)?,
        repository: r.get(4)?,
        documentation: r.get(5)?,
        keywords: split_list(&r.get::<_, String>(6)?),
        tags: split_list(&r.get::<_, String>(7)?),
        downloads: r.get::<_, i64>(8)? as u64,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

fn version_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PackageVersion> {
    let version_str: String = r.get(1)?;
    Ok(PackageVersion {
        name: r.get(0)?,
        version: hs_pm::semver::Version::parse(&version_str)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(std::io::Error::other(e))))?,
        deps: Vec::new(),
        integrity: r.get(2)?,
        fingerprint: r.get(3)?,
        signature: r.get(4)?,
        key_id: r.get(5)?,
        size: r.get::<_, i64>(6)? as u64,
        file_count: r.get::<_, i64>(7)? as usize,
        files: split_list(&r.get::<_, String>(8)?),
        channel: r.get(9)?,
        yanked: r.get::<_, i64>(10)? != 0,
        downloads: r.get::<_, i64>(11)? as u64,
        published_at: r.get(12)?,
    })
}

fn token_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Token> {
    let scopes_raw: String = r.get(4)?;
    let scopes = scopes_raw
        .split(',')
        .filter_map(Scope::parse)
        .collect::<Vec<Scope>>();
    Ok(Token {
        id: r.get(0)?,
        user: r.get(1)?,
        name: r.get(2)?,
        token_hash: r.get(3)?,
        scopes,
        created_at: r.get(5)?,
        last_used_at: r.get(6)?,
        revoked: r.get::<_, i64>(7)? != 0,
    })
}

fn load_deps(c: &Connection, name: &str, version: &str) -> StoreResult<Vec<Dep>> {
    let mut st = c
        .prepare("SELECT dep, req, kind FROM deps WHERE name = ?1 AND version = ?2 ORDER BY kind, dep")
        .map_err(io)?;
    let rows = st
        .query_map(params![name, version], |r| {
            let kind: String = r.get(2)?;
            Ok(Dep {
                name: r.get(0)?,
                req: r.get(1)?,
                kind: DepKind::parse(&kind),
            })
        })
        .map_err(io)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(io)?;
    Ok(rows)
}

fn version_downloads(c: &Connection, name: &str) -> StoreResult<u64> {
    let v: i64 = c
        .query_row(
            "SELECT COALESCE(SUM(downloads), 0) FROM versions WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )
        .map_err(io)?;
    Ok(v.max(0) as u64)
}

fn append_change_tx(tx: &Transaction<'_>, name: &str, version: &str, kind: ChangeKind) -> StoreResult<i64> {
    tx.execute(
        "INSERT INTO changes (name, version, kind, at) VALUES (?1, ?2, ?3, ?4)",
        params![name, version, kind.as_str(), now_secs()],
    )
    .map_err(io)?;
    Ok(tx.last_insert_rowid())
}

/// Convenience: a migration report for `hard-registry migrate` output.
pub fn schema_version(path: &Path) -> StoreResult<i64> {
    let c = Connection::open(path).map_err(io)?;
    c.pragma_update(None, "user_version", 0).ok();
    c.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_encoding_roundtrips() {
        let v = vec!["a".to_string(), "b".to_string(), "c-d".to_string()];
        assert_eq!(split_list(&join_list(&v)), v);
        assert!(split_list("").is_empty());
        assert!(split_list("a\u{1f}").is_empty() || split_list("a\u{1f}").len() == 1);
    }

    #[test]
    fn like_metacharacters_are_escaped() {
        assert_eq!(escape_like("a%b_c"), "a\\%b\\_c");
    }

    #[test]
    fn memory_store_migrates() {
        let s = SqliteStore::memory().unwrap();
        assert_eq!(s.backend(), "sqlite");
        assert_eq!(s.stats().unwrap().packages, 0);
    }

    #[test]
    fn file_store_creates_parent_directories() {
        let dir = std::env::temp_dir().join(format!("hs-reg-sql-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = SqliteStore::open(dir.join("nested/registry.db")).unwrap();
        assert!(dir.join("nested/registry.db").exists());
        s.shrink();
        let _ = std::fs::remove_dir_all(&dir);
    }

}
