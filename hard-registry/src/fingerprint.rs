//! Manifest fingerprints.
//!
//! A *fingerprint* is a stable digest of everything about a published version
//! that the registry promises will not change: its coordinates, its
//! dependency edges, its file list and its channel. The archive digest
//! ([`crate::security::sha256_hex`]) covers the bytes; the fingerprint covers
//! the *claim*. Both go into the signature payload, so editing metadata in a
//! mirror invalidates the signature just as editing bytes does.
//!
//! The encoding is explicit and line-oriented (never a JSON dump) so the
//! same fingerprint can be recomputed by the client, by a mirror and by an
//! auditor from a text log.

use crate::model::{Dep, DepKind, PackageVersion};
use crate::security::sha256_hex;

/// Fingerprint format version. Bump when the encoding below changes, so old
/// and new fingerprints can never be confused.
pub const FINGERPRINT_VERSION: &str = "hs-fingerprint/1";

/// Everything a fingerprint is computed from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintInput<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub deps: &'a [Dep],
    pub files: &'a [String],
    pub channel: Option<&'a str>,
}

impl<'a> FingerprintInput<'a> {
    pub fn new(name: &'a str, version: &'a str) -> FingerprintInput<'a> {
        FingerprintInput {
            name,
            version,
            deps: &[],
            files: &[],
            channel: None,
        }
    }

    pub fn with_deps(mut self, deps: &'a [Dep]) -> FingerprintInput<'a> {
        self.deps = deps;
        self
    }

    pub fn with_files(mut self, files: &'a [String]) -> FingerprintInput<'a> {
        self.files = files;
        self
    }

    pub fn with_channel(mut self, channel: Option<&'a str>) -> FingerprintInput<'a> {
        self.channel = channel;
        self
    }
}

/// The canonical text a fingerprint hashes. Deterministic: dependency edges
/// are sorted by `(name, kind, req)` and files lexicographically, so two
/// registries that received the same publish in a different order still agree.
pub fn canonical_text(input: &FingerprintInput<'_>) -> String {
    let mut deps: Vec<(String, String, String)> = input
        .deps
        .iter()
        .map(|d| (d.name.clone(), d.kind.as_str().to_string(), d.req.clone()))
        .collect();
    deps.sort();
    deps.dedup();
    let mut files: Vec<&String> = input.files.iter().collect();
    files.sort();
    let mut out = String::new();
    out.push_str(FINGERPRINT_VERSION);
    out.push('\n');
    out.push_str("name ");
    out.push_str(input.name);
    out.push('\n');
    out.push_str("version ");
    out.push_str(input.version);
    out.push('\n');
    out.push_str("channel ");
    out.push_str(input.channel.unwrap_or(""));
    out.push('\n');
    for (name, kind, req) in deps {
        out.push_str(&format!("dep {name} {kind} {req}\n"));
    }
    for f in &files {
        out.push_str("file ");
        out.push_str(f);
        out.push('\n');
    }
    out
}

/// `sha256:<hex>` of [`canonical_text`].
pub fn compute(input: &FingerprintInput<'_>) -> String {
    format!("sha256:{}", sha256_hex(canonical_text(input).as_bytes()))
}

/// Fingerprint a stored version record.
pub fn of_version(v: &PackageVersion) -> String {
    compute(&FingerprintInput {
        name: &v.name,
        version: &v.version.to_string(),
        deps: &v.deps,
        files: &v.files,
        channel: v.channel.as_deref(),
    })
}

/// Recompute a fingerprint and compare it against a recorded one.
pub fn verify(recorded: &str, name: &str, version: &str, deps: &[Dep], files: &[String], channel: Option<&str>) -> bool {
    let want = compute(
        &FingerprintInput::new(name, version)
            .with_deps(deps)
            .with_files(files)
            .with_channel(channel),
    );
    crate::security::integrity_matches(&want, recorded)
}

/// Every dependency kind appears in the fingerprint text, so promoting a
/// `normal` edge to `dev` changes the digest.
pub fn kind_is_distinct(k: DepKind) -> bool {
    matches!(k, DepKind::Normal | DepKind::Dev | DepKind::Build)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps() -> Vec<Dep> {
        vec![
            Dep {
                name: "b".to_string(),
                req: "^2.0.0".to_string(),
                kind: DepKind::Normal,
            },
            Dep {
                name: "a".to_string(),
                req: "^1.0.0".to_string(),
                kind: DepKind::Dev,
            },
        ]
    }

    fn files() -> Vec<String> {
        vec!["src/main.hard".to_string(), "hard.toml".to_string()]
    }

    #[test]
    fn canonical_text_is_sorted_and_versioned() {
        let text = canonical_text(
            &FingerprintInput::new("demo", "1.0.0")
                .with_deps(&deps())
                .with_files(&files())
                .with_channel(Some("stable")),
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], FINGERPRINT_VERSION);
        assert!(text.contains("dep a dev ^1.0.0\n"));
        // 'a' sorts before 'b' even though it was declared second
        let a = text.find("dep a").unwrap();
        let b = text.find("dep b").unwrap();
        assert!(a < b);
        assert!(text.find("file hard.toml").unwrap() < text.find("file src/main.hard").unwrap());
    }

    #[test]
    fn dependency_order_does_not_change_the_fingerprint() {
        let mut d = deps();
        let one = compute(
            &FingerprintInput::new("demo", "1.0.0")
                .with_deps(&d)
                .with_files(&files()),
        );
        d.reverse();
        let two = compute(
            &FingerprintInput::new("demo", "1.0.0")
                .with_deps(&d)
                .with_files(&files()),
        );
        assert_eq!(one, two);
    }

    #[test]
    fn every_field_changes_the_fingerprint() {
        let base = compute(&FingerprintInput::new("demo", "1.0.0").with_deps(&deps()).with_files(&files()));
        let other_name = compute(&FingerprintInput::new("other", "1.0.0").with_deps(&deps()).with_files(&files()));
        let other_version = compute(&FingerprintInput::new("demo", "1.0.1").with_deps(&deps()).with_files(&files()));
        let other_channel = compute(
            &FingerprintInput::new("demo", "1.0.0")
                .with_deps(&deps())
                .with_files(&files())
                .with_channel(Some("beta")),
        );
        let mut extra = deps();
        extra.push(Dep::normal("c", "^1.0.0"));
        let other_deps = compute(
            &FingerprintInput::new("demo", "1.0.0")
                .with_deps(&extra)
                .with_files(&files()),
        );
        let other_files = compute(
            &FingerprintInput::new("demo", "1.0.0")
                .with_deps(&deps())
                .with_files(&["only.hard".to_string()]),
        );
        for (label, fp) in [
            ("name", other_name),
            ("version", other_version),
            ("channel", other_channel),
            ("deps", other_deps),
            ("files", other_files),
        ] {
            assert_ne!(base, fp, "changing {label} must change the fingerprint");
        }
    }

    #[test]
    fn dependency_kind_changes_the_fingerprint() {
        let normal = vec![Dep::normal("a", "^1.0.0")];
        let dev = vec![Dep {
            name: "a".to_string(),
            req: "^1.0.0".to_string(),
            kind: DepKind::Dev,
        }];
        assert_ne!(
            compute(&FingerprintInput::new("d", "1.0.0").with_deps(&normal)),
            compute(&FingerprintInput::new("d", "1.0.0").with_deps(&dev))
        );
        assert!(kind_is_distinct(DepKind::Build));
    }

    #[test]
    fn verify_detects_tampering() {
        let d = deps();
        let f = files();
        let fp = compute(
            &FingerprintInput::new("demo", "1.0.0")
                .with_deps(&d)
                .with_files(&f),
        );
        assert!(verify(&fp, "demo", "1.0.0", &d, &f, None));
        let mut tampered = d.clone();
        tampered[0].req = "^9.9.9".to_string();
        assert!(!verify(&fp, "demo", "1.0.0", &tampered, &f, None));
        assert!(!verify(&fp, "demo", "1.0.0", &d, &["x".to_string()], None));
        assert!(!verify(&fp, "demo", "1.0.1", &d, &f, None));
    }

    #[test]
    fn stored_version_roundtrips_through_of_version() {
        let v = PackageVersion {
            name: "demo".to_string(),
            version: hs_pm::semver::Version::parse("1.0.0").unwrap(),
            deps: deps(),
            integrity: "sha256:aa".to_string(),
            fingerprint: String::new(),
            signature: None,
            key_id: None,
            size: 1,
            file_count: 2,
            files: files(),
            channel: Some("stable".to_string()),
            yanked: false,
            downloads: 0,
            published_at: 0,
        };
        let fp = of_version(&v);
        assert!(verify(
            &fp,
            &v.name,
            "1.0.0",
            &v.deps,
            &v.files,
            v.channel.as_deref()
        ));
    }
}
