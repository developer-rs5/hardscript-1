//! Turning an HTTP publish request into a [`PublishRequest`].
//!
//! Two shapes are accepted, and both carry the same information:
//!
//! 1. **Raw upload** — the body *is* the `.hspkg` archive and the metadata
//!    rides in `X-Hard-*` headers. This is what `hard publish` sends: no
//!    base64 inflation, a body the registry can stream to disk, and a
//!    `curl` one-liner for humans.
//! 2. **JSON envelope** — `Content-Type: application/json` with the archive
//!    base64-encoded. Convenient for scripts that already build JSON.
//!
//! When the upload carries no metadata at all, the registry falls back to the
//! `hard.toml` *inside* the archive, so `tar`-style tooling that only knows
//! how to produce a `.hspkg` still publishes something coherent.

use crate::app::PublishRequest;
use crate::codec::{parse_body, publish_from_json, sort_deps, DecodeError, FieldError};
use crate::http::Request;
use crate::model::Dep;

/// Parse a publish request in either supported shape.
pub fn parse_publish_request(req: &Request) -> Result<PublishRequest, DecodeError> {
    let ctype = req.header("content-type").unwrap_or("").to_ascii_lowercase();
    let mut out = if ctype.contains("application/json") {
        let body = parse_body(&req.text())?;
        publish_from_json(&body)?
    } else {
        from_headers(req)
    };
    if out.archive.is_empty() {
        return Err(DecodeError::Fields(vec![FieldError::new(
            "archive",
            "a publish must carry the .hspkg archive (as the body, or as archive_base64)",
        )]));
    }
    if out.name.is_empty() || out.version.is_empty() {
        if let Some(from_archive) = manifest_fields(&out.archive) {
            if out.name.is_empty() {
                out.name = from_archive.name;
            }
            if out.version.is_empty() {
                out.version = from_archive.version;
            }
            if out.description.is_none() {
                out.description = from_archive.description;
            }
            if out.license.is_none() {
                out.license = from_archive.license;
            }
            if out.deps.is_empty() {
                out.deps = from_archive.deps;
            }
        }
    }
    sort_deps(&mut out);
    Ok(out)
}

/// The raw-upload shape: metadata in headers, archive in the body.
fn from_headers(req: &Request) -> PublishRequest {
    let mut out = PublishRequest {
        archive: req.body.clone(),
        ..PublishRequest::default()
    };
    let h = |name: &str| req.header(name).map(str::trim).filter(|v| !v.is_empty());
    out.name = h("x-hard-package").unwrap_or("").to_string();
    out.version = h("x-hard-version").unwrap_or("").to_string();
    out.description = h("x-hard-description").map(String::from);
    out.license = h("x-hard-license").map(String::from);
    out.homepage = h("x-hard-homepage").map(String::from);
    out.repository = h("x-hard-repository").map(String::from);
    out.documentation = h("x-hard-documentation").map(String::from);
    out.channel = h("x-hard-channel").map(String::from);
    out.tags = split_list(h("x-hard-tags"));
    out.keywords = split_list(h("x-hard-keywords"));
    if let Some(raw) = h("x-hard-dependencies") {
        out.deps = parse_deps(raw);
    }
    out
}

/// Split a comma or space separated list.
pub fn split_list(raw: Option<&str>) -> Vec<String> {
    raw.map(|r| {
        r.split([',', ' ', '\t'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    })
    .unwrap_or_default()
}

/// Parse a dependency header. Accepts `name@req` pairs separated by commas or
/// spaces, and `kind:name@req` for non-normal edges:
/// `base64@^0.4.0,dev:q@1.0.0`.
pub fn parse_deps(raw: &str) -> Vec<Dep> {
    let mut out = Vec::new();
    for piece in raw.split([',', ' ', '\t']) {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let (kind, rest) = match piece.split_once(':') {
            Some((k, r)) => (
                match k {
                    "dev" => crate::model::DepKind::Dev,
                    "build" => crate::model::DepKind::Build,
                    _ => crate::model::DepKind::Normal,
                },
                r,
            ),
            None => (crate::model::DepKind::Normal, piece),
        };
        let (name, req) = match rest.split_once('@') {
            Some((n, r)) => (n.trim(), r.trim()),
            None => (rest, "*"),
        };
        if name.is_empty() {
            continue;
        }
        out.push(Dep {
            name: name.to_string(),
            req: req.to_string(),
            kind,
        });
    }
    out
}

/// Render a dependency list back into the header form (used by mirrors).
pub fn render_deps(deps: &[Dep]) -> String {
    deps.iter()
        .map(|d| {
            if d.kind == crate::model::DepKind::Normal {
                format!("{}@{}", d.name, d.req)
            } else {
                format!("{}:{}@{}", d.kind.as_str(), d.name, d.req)
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The publishable fields a package's own `hard.toml` contributes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManifestFields {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub license: Option<String>,
    pub deps: Vec<Dep>,
}

/// Read `hard.toml` out of a `.hspkg` archive, if it has one.
pub fn manifest_fields(archive: &[u8]) -> Option<ManifestFields> {
    let files = hs_pm::pkgfmt::read_archive(archive).ok()?;
    let toml = files
        .iter()
        .find(|f| f.rel_path == "hard.toml")
        .map(|f| String::from_utf8_lossy(&f.data).into_owned())?;
    let doc = hs_pm::toml::parse(&toml).ok()?;
    let get = |k: &str| {
        doc.get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let name = get("name")?;
    let version = get("version")?;
    let mut deps = Vec::new();
    for (table, kind) in [
        ("dependencies", crate::model::DepKind::Normal),
        ("dev-dependencies", crate::model::DepKind::Dev),
    ] {
        if let Some(t) = doc.table(table) {
            for (k, v) in t {
                if let Some(req) = v.as_str() {
                    deps.push(Dep {
                        name: k.clone(),
                        req: req.to_string(),
                        kind,
                    });
                }
            }
        }
    }
    Some(ManifestFields {
        name,
        version,
        description: get("description"),
        license: get("license"),
        deps,
    })
}

/// Build a `.hspkg` archive from a manifest and a set of source files. Used
/// by `hard publish` and by the mirror tools' test fixtures.
pub fn build_archive(
    manifest_toml: &str,
    files: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    let mut records = vec![hs_pm::pkgfmt::FileRecord {
        rel_path: "hard.toml".to_string(),
        data: manifest_toml.as_bytes().to_vec(),
    }];
    for (path, data) in files {
        records.push(hs_pm::pkgfmt::FileRecord {
            rel_path: path.clone(),
            data: data.clone(),
        });
    }
    records.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    records.dedup_by(|a, b| a.rel_path == b.rel_path);
    hs_pm::pkgfmt::pack(&records).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Request;

    fn archive() -> Vec<u8> {
        hs_pm::pkgfmt::pack(&[hs_pm::pkgfmt::FileRecord {
            rel_path: "main.hard".to_string(),
            data: b"calc x() => Int { <- 1 }\n".to_vec(),
        }])
        .unwrap()
    }

    #[test]
    fn header_upload_shape() {
        let mut req = Request::new("POST", "/api/publish").with_body(archive());
        req.set_header("Content-Type", "application/vnd.hardscript.package");
        req.set_header("X-Hard-Package", "jwt");
        req.set_header("X-Hard-Version", "1.0.0");
        req.set_header("X-Hard-Description", "tokens");
        req.set_header("X-Hard-License", "MIT");
        req.set_header("X-Hard-Tags", "auth, security");
        req.set_header("X-Hard-Keywords", "jwt web");
        req.set_header("X-Hard-Channel", "stable");
        req.set_header("X-Hard-Dependencies", "base64@^0.4.0, dev:q@1.0.0");
        let p = parse_publish_request(&req).unwrap();
        assert_eq!(p.name, "jwt");
        assert_eq!(p.version, "1.0.0");
        assert_eq!(p.description.as_deref(), Some("tokens"));
        assert_eq!(p.license.as_deref(), Some("MIT"));
        assert_eq!(p.tags, vec!["auth", "security"]);
        assert_eq!(p.keywords, vec!["jwt", "web"]);
        assert_eq!(p.channel.as_deref(), Some("stable"));
        assert_eq!(p.deps.len(), 2);
        assert_eq!(p.deps[0].name, "base64");
        assert_eq!(p.deps[1].kind, crate::model::DepKind::Dev);
    }

    #[test]
    fn json_envelope_shape() {
        let b64 = crate::base64::encode(&archive());
        let body = format!(
            r#"{{"name":"jwt","version":"2.0.0","archive_base64":"{b64}","tags":["auth"]}}"#
        );
        let req = Request::new("POST", "/api/publish")
            .with_body(body.into_bytes())
            .with_header("Content-Type", "application/json");
        let p = parse_publish_request(&req).unwrap();
        assert_eq!(p.name, "jwt");
        assert_eq!(p.version, "2.0.0");
        assert_eq!(p.archive, archive());
        assert_eq!(p.tags, vec!["auth"]);
    }

    #[test]
    fn an_empty_body_is_rejected() {
        let req = Request::new("POST", "/api/publish");
        let err = parse_publish_request(&req).unwrap_err();
        assert!(err.to_string().contains("archive"), "{err}");
    }

    #[test]
    fn falls_back_to_the_archives_own_manifest() {
        let archive = build_archive(
            "name = \"fallback\"\nversion = \"3.1.0\"\ndescription = \"from manifest\"\nlicense = \"MIT\"\n\n[dependencies]\nbase64 = \"^0.4.0\"\n",
            &[("main.hard".to_string(), b"calc x() => Int { <- 1 }\n".to_vec())],
        )
        .unwrap();
        let req = Request::new("POST", "/api/publish").with_body(archive);
        let p = parse_publish_request(&req).unwrap();
        assert_eq!(p.name, "fallback");
        assert_eq!(p.version, "3.1.0");
        assert_eq!(p.description.as_deref(), Some("from manifest"));
        assert_eq!(p.license.as_deref(), Some("MIT"));
        assert_eq!(p.deps, vec![Dep::normal("base64", "^0.4.0")]);
    }

    #[test]
    fn headers_win_over_the_archive_manifest() {
        let archive = build_archive(
            "name = \"fallback\"\nversion = \"3.1.0\"\n",
            &[("main.hard".to_string(), b"x".to_vec())],
        )
        .unwrap();
        let mut req = Request::new("POST", "/api/publish").with_body(archive);
        req.set_header("X-Hard-Package", "chosen");
        req.set_header("X-Hard-Version", "9.9.9");
        let p = parse_publish_request(&req).unwrap();
        assert_eq!((p.name.as_str(), p.version.as_str()), ("chosen", "9.9.9"));
    }

    #[test]
    fn dependency_header_parsing() {
        assert_eq!(
            parse_deps("a@^1.0.0, b@2.0.0"),
            vec![
                Dep::normal("a", "^1.0.0"),
                Dep::normal("b", "2.0.0")
            ]
        );
        // no requirement means "any"
        assert_eq!(parse_deps("a"), vec![Dep::normal("a", "*")]);
        // kinds
        let d = parse_deps("dev:a@1, build:b@2, c@3");
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].kind, crate::model::DepKind::Dev);
        assert_eq!(d[1].kind, crate::model::DepKind::Build);
        assert_eq!(d[2].kind, crate::model::DepKind::Normal);
        assert!(parse_deps("  ").is_empty());
        assert!(parse_deps("@1.0.0").is_empty());
    }

    #[test]
    fn dependency_header_roundtrip() {
        let deps = vec![
            Dep::normal("a", "^1.0.0"),
            Dep {
                name: "b".to_string(),
                req: "2.0.0".to_string(),
                kind: crate::model::DepKind::Dev,
            },
        ];
        assert_eq!(parse_deps(&render_deps(&deps)), deps);
    }

    #[test]
    fn list_splitting() {
        assert_eq!(split_list(Some("a, b ,c")), vec!["a", "b", "c"]);
        assert!(split_list(None).is_empty());
        assert!(split_list(Some("")).is_empty());
    }

    #[test]
    fn manifest_fields_absent_for_a_manifestless_archive() {
        assert!(manifest_fields(&archive()).is_none());
        let bad = build_archive("not toml at all [", &[]).unwrap();
        assert!(manifest_fields(&bad).is_none());
    }

    #[test]
    fn build_archive_is_deterministic() {
        let files = vec![
            ("b.hard".to_string(), b"2".to_vec()),
            ("a.hard".to_string(), b"1".to_vec()),
        ];
        let one = build_archive("name = \"x\"\nversion = \"1.0.0\"\n", &files).unwrap();
        let two = build_archive("name = \"x\"\nversion = \"1.0.0\"\n", &files).unwrap();
        assert_eq!(one, two);
        let read = hs_pm::pkgfmt::read_archive(&one).unwrap();
        assert_eq!(read[0].rel_path, "a.hard");
        assert_eq!(read[1].rel_path, "b.hard");
        assert_eq!(read[2].rel_path, "hard.toml");
    }
}
