//! `hard-registry` — the HardScript package registry.
//!
//! v0.9 turns the registry from a *client* (which is all v0.4 shipped) into a
//! *service*: a REST API over a pluggable store, with real archives on disk,
//! Ed25519-signed publishes, personal access tokens, fuzzy search and the
//! change feed a mirror replicates from.
//!
//! # Layout
//!
//! | module | role |
//! |--------|------|
//! | [`model`] | domain types: packages, versions, deps, users, tokens |
//! | [`store`] | the persistence contract (SQLite today, PostgreSQL next) |
//! | [`sqlite`] | the SQLite implementation of [`store::Store`] |
//! | [`archives`] | on-disk `.hspkg` storage, validation and ranges |
//! | [`security`] | SHA-256, HMAC, PBKDF2, constant-time compares |
//! | [`auth`] | passwords, sessions, token hashing, backoff |
//! | [`signing`] | Ed25519 package signatures and key management |
//! | [`fingerprint`] | canonical manifest fingerprints |
//! | [`codec`] | request decoding with per-field errors |
//! | [`publish`] | publish-request parsing and archive building |
//! | [`search`] | fuzzy scoring, tags, downloads |
//! | [`app`] | the service: policy, publish, yank, auth, mirror feed |
//! | [`router`] | the REST surface |
//! | [`views`] | canonical JSON documents |
//! | [`http`] | the dependency-free HTTP/1.1 server |
//!
//! # Guarantees
//!
//! - **Deterministic output.** Every JSON document is built through
//!   [`hs_compiler::json`], so identical state produces identical bytes.
//! - **Authenticated writes.** Publish and yank need a token holding the
//!   matching scope; tokens are stored hashed and can be revoked.
//! - **Signed publishes.** The default configuration signs every version and
//!   binds the signature to both the archive digest and the manifest
//!   fingerprint.
//! - **Validated archives.** An upload is checked for size, structure, path
//!   safety and emptiness before any row is written, and a failed row write
//!   removes the bytes again.
//!
//! # Example
//!
//! ```no_run
//! use hard_registry::{App, Config, Router, Server, ArchiveStore, SqliteStore, SigningKey};
//! use std::sync::Arc;
//!
//! let store = Arc::new(SqliteStore::open("registry.db")?);
//! let app = Arc::new(App::new(
//!     store,
//!     ArchiveStore::new(std::path::Path::new(".")),
//!     SigningKey::from_env(),
//!     Config::default(),
//! ));
//! let server = Server::bind("127.0.0.1:8484", Router::new(app).into_handler())?;
//! println!("listening on {}", server.base_url());
//! server.serve_forever()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod app;
pub mod archives;
pub mod auth;
pub mod base64;
pub mod codec;
pub mod fingerprint;
pub mod http;
pub mod model;
pub mod publish;
pub mod router;
pub mod search;
pub mod security;
pub mod signing;
pub mod sqlite;
pub mod store;
pub mod views;

pub use app::{App, Config, PublishRequest, VerifyOutcome};
pub use archives::ArchiveStore;
pub use http::{Request, Response, Server, TestServer};
pub use router::Router;
pub use signing::SigningKey;
pub use sqlite::SqliteStore;
pub use store::{Store, StoreError};

/// The service name reported by `GET /` and the `X-Hard-Registry` header.
pub const SERVICE_NAME: &str = "hardscript-registry";

/// The API version string clients can feature-detect against.
pub const API_VERSION: &str = "v1";

/// The default listen address for a local registry.
pub const DEFAULT_ADDR: &str = "127.0.0.1:8484";

/// Assemble the default production wiring: a SQLite store, an archive area
/// under `data/`, the configured signing key and the default policy.
pub fn default_app(data_dir: &std::path::Path, config: Config) -> Result<App, StoreError> {
    let store = SqliteStore::open(data_dir.join("registry.db"))?;
    Ok(App::new(
        std::sync::Arc::new(store),
        ArchiveStore::new(data_dir),
        SigningKey::from_env(),
        config,
    ))
}
