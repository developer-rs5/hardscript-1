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
//! - [`registry`] — registry HTTP/1.1 client (no backend required)
//! - [`install`] — install/remove/update/list/outdated orchestration
//! - [`workspace`] — monorepo support
//! - [`templates`] — official project templates
//! - [`report`] — automatic milestone reports

pub mod cache;
pub mod install;
pub mod lockfile;
pub mod manifest;
pub mod pkgfmt;
pub mod registry;
pub mod report;
pub mod resolver;
pub mod semver;
pub mod templates;
pub mod toml;
pub mod tui;
pub mod workspace;

pub use manifest::{Manifest, ManifestError, ManifestWarning};
