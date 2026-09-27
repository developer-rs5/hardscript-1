//! hs_pm — the HardScript package manager library.
//!
//! Ecosystem foundation for the HardScript toolchain:
//!
//! - [`manifest`] — `hard.toml` parsing, validation, rendering (`hard init`)
//! - [`semver`] — semantic versioning and requirement matching
//! - [`resolver`] — deterministic dependency resolution
//! - [`lockfile`] — deterministic `hard.lock`
//! - [`pkgfmt`] — the `.hspkg` package archive format
//! - [`cache`] — the shared `~/.hard/cache/` store (offline installs)
//! - [`download`] — the download client: cache-first, resumable, parallel
//! - [`credentials`] — per-registry tokens for `hard login` / `hard logout`
//! - [`auth`] — the registry authentication client (login, tokens, whoami)
//! - [`registry`] — registry HTTP/1.1 client (read, publish, auth)
//! - [`install`] — install/remove/update/list/outdated orchestration
//! - [`publish`] — `hard publish`: `.hspkg` building and upload
//! - [`workspace`] — monorepo support
//! - [`search`] — `hard search`: query shaping, offline fallback, ranking
//! - [`verify`] — Ed25519 signature verification and the trust store
//! - [`templates`] — official project templates
//! - [`report`] — automatic milestone reports

pub mod auth;
pub mod cache;
pub mod credentials;
pub mod download;
pub mod install;
pub mod lockfile;
pub mod manifest;
pub mod pkgfmt;
pub mod publish;
pub mod registry;
pub mod report;
pub mod resolver;
pub mod search;
pub mod verify;
pub mod semver;
pub mod templates;
pub mod toml;
pub mod tui;
pub mod workspace;

pub use manifest::{Manifest, ManifestError, ManifestWarning};
