//! Canonical JSON views over the domain model.
//!
//! The registry API is the contract between `hard` and the server, so the
//! shapes are spelled out here rather than derived ad hoc in the router:
//!
//! - `GET /packages/{name}` -> a package document with a `versions` array
//! - `GET /search` -> `{"results":[...]}`
//! - errors -> `{"error":{"code":..,"message":..}}`
//!
//! All output goes through [`hs_compiler::json`], whose serializer emits
//! objects in insertion order with no whitespace, so responses are
//! byte-stable and can be compared in tests.

use crate::model::{Change, Dep, Package, PackageVersion, Scope, Stats, User};
use hs_compiler::json::Json;

fn s(v: &Option<String>) -> Json {
    match v {
        Some(t) => Json::str(t),
        None => Json::Null,
    }
}

fn list(items: &[String]) -> Json {
    Json::arr(items.iter().map(Json::str).collect())
}

/// A dependency edge, as served in version documents.
pub fn dep_json(d: &Dep) -> Json {
    Json::obj(vec![
        ("name", Json::str(&d.name)),
        ("req", Json::str(&d.req)),
        ("kind", Json::str(d.kind.as_str())),
    ])
}

/// One published version inside a package document.
pub fn version_json(v: &PackageVersion) -> Json {
    let mut pairs = vec![
        ("name", Json::str(&v.name)),
        ("version", Json::str(v.version.to_string())),
        ("dependencies", Json::arr(v.deps.iter().map(dep_json).collect())),
        ("integrity", Json::str(&v.integrity)),
        ("fingerprint", Json::str(&v.fingerprint)),
        ("size", Json::num(v.size as i64)),
        ("file_count", Json::num(v.file_count as i64)),
        ("files", list(&v.files)),
        ("channel", s(&v.channel)),
        ("yanked", Json::Bool(v.yanked)),
        ("downloads", Json::num(v.downloads as i64)),
        ("published_at", Json::num(v.published_at)),
    ];
    if let Some(sig) = &v.signature {
        pairs.push(("signature", Json::str(sig)));
    }
    if let Some(k) = &v.key_id {
        pairs.push(("key_id", Json::str(k)));
    }
    Json::obj(pairs)
}

/// A package document: descriptive fields plus every version.
pub fn package_json(p: &Package, versions: &[PackageVersion]) -> Json {
    let latest = versions
        .iter()
        .filter(|v| !v.yanked)
        .max_by(|a, b| a.version.cmp(&b.version));
    let mut pairs = vec![
        ("name", Json::str(&p.name)),
        ("owner", s(&p.owner)),
        ("description", s(&p.description)),
        ("license", s(&p.license)),
        ("homepage", s(&p.homepage)),
        ("repository", s(&p.repository)),
        ("documentation", s(&p.documentation)),
        ("keywords", list(&p.keywords)),
        ("tags", list(&p.tags)),
        ("downloads", Json::num(p.downloads as i64)),
        ("created_at", Json::num(p.created_at)),
        ("updated_at", Json::num(p.updated_at)),
        (
            "latest",
            match latest {
                Some(v) => Json::str(v.version.to_string()),
                None => Json::Null,
            },
        ),
        (
            "versions",
            Json::arr(versions.iter().map(version_json).collect()),
        ),
    ];
    // A `versions` map keyed by version is what the legacy v0.7 client reads
    // for metadata; keeping both shapes avoids a flag day in the client.
    let map: Vec<(String, Json)> = versions
        .iter()
        .map(|v| (v.version.to_string(), version_json(v)))
        .collect();
    pairs.push(("version_map", Json::Obj(map)));
    Json::obj(pairs)
}

/// A compact per-version document (`GET /packages/{name}/versions`).
pub fn version_summary_json(v: &PackageVersion) -> Json {
    Json::obj(vec![
        ("name", Json::str(&v.name)),
        ("version", Json::str(v.version.to_string())),
        ("yanked", Json::Bool(v.yanked)),
        ("downloads", Json::num(v.downloads as i64)),
        ("integrity", Json::str(&v.integrity)),
        ("fingerprint", Json::str(&v.fingerprint)),
        (
            "channel",
            match &v.channel {
                Some(c) => Json::str(c),
                None => Json::Null,
            },
        ),
        ("published_at", Json::num(v.published_at)),
    ])
}

/// One search hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub downloads: u64,
    pub tags: Vec<String>,
    pub keywords: Vec<String>,
    pub owner: Option<String>,
    pub score: i64,
}

impl SearchHit {
    pub fn json(&self) -> Json {
        Json::obj(vec![
            ("name", Json::str(&self.name)),
            (
                "version",
                match &self.version {
                    Some(v) => Json::str(v),
                    None => Json::Null,
                },
            ),
            ("description", s(&self.description)),
            ("license", s(&self.license)),
            ("downloads", Json::num(self.downloads as i64)),
            ("tags", list(&self.tags)),
            ("keywords", list(&self.keywords)),
            ("owner", s(&self.owner)),
            ("score", Json::num(self.score)),
        ])
    }
}

/// A `{"results":[...]}` search response.
pub fn search_json(hits: &[SearchHit], query: &str, total: usize) -> Json {
    Json::obj(vec![
        ("query", Json::str(query)),
        ("total", Json::num(total as i64)),
        ("count", Json::num(hits.len() as i64)),
        ("results", Json::arr(hits.iter().map(SearchHit::json).collect())),
    ])
}

/// A publish acknowledgement.
pub fn publish_json(v: &PackageVersion) -> Json {
    Json::obj(vec![
        ("ok", Json::Bool(true)),
        ("name", Json::str(&v.name)),
        ("version", Json::str(v.version.to_string())),
        ("integrity", Json::str(&v.integrity)),
        ("fingerprint", Json::str(&v.fingerprint)),
        (
            "signature",
            match &v.signature {
                Some(s) => Json::str(s),
                None => Json::Null,
            },
        ),
        (
            "key_id",
            match &v.key_id {
                Some(s) => Json::str(s),
                None => Json::Null,
            },
        ),
        ("size", Json::num(v.size as i64)),
        ("file_count", Json::num(v.file_count as i64)),
        ("yanked", Json::Bool(v.yanked)),
    ])
}

/// A yank acknowledgement.
pub fn yank_json(name: &str, version: &str, yanked: bool) -> Json {
    Json::obj(vec![
        ("ok", Json::Bool(true)),
        ("name", Json::str(name)),
        ("version", Json::str(version)),
        ("yanked", Json::Bool(yanked)),
    ])
}

/// An authentication result: the token is only ever returned here.
pub fn token_json(
    id: &str,
    user: &str,
    name: &str,
    scopes: &[Scope],
    plaintext: Option<&str>,
    created_at: i64,
) -> Json {
    let mut pairs = vec![
        ("id", Json::str(id)),
        ("user", Json::str(user)),
        ("name", Json::str(name)),
        (
            "scopes",
            Json::arr(Scope::render(scopes).into_iter().map(Json::str).collect()),
        ),
        ("created_at", Json::num(created_at)),
    ];
    if let Some(p) = plaintext {
        pairs.insert(0, ("token", Json::str(p)));
    }
    Json::obj(pairs)
}

/// A stored token, without its hash.
pub fn token_view(t: &crate::model::Token) -> Json {
    Json::obj(vec![
        ("id", Json::str(&t.id)),
        ("user", Json::str(&t.user)),
        ("name", Json::str(&t.name)),
        (
            "scopes",
            Json::arr(Scope::render(&t.scopes).into_iter().map(Json::str).collect()),
        ),
        ("created_at", Json::num(t.created_at)),
        (
            "last_used_at",
            match t.last_used_at {
                Some(v) => Json::num(v),
                None => Json::Null,
            },
        ),
        ("revoked", Json::Bool(t.revoked)),
    ])
}

/// A user record, without credential material.
pub fn user_json(u: &User) -> Json {
    Json::obj(vec![
        ("name", Json::str(&u.name)),
        ("email", s(&u.email)),
        ("created_at", Json::num(u.created_at)),
    ])
}

/// Registry statistics.
pub fn stats_json(st: &Stats, backend: &str, seq: i64) -> Json {
    Json::obj(vec![
        ("backend", Json::str(backend)),
        ("packages", Json::num(st.packages as i64)),
        ("versions", Json::num(st.versions as i64)),
        ("yanked", Json::num(st.yanked as i64)),
        ("users", Json::num(st.users as i64)),
        ("tokens", Json::num(st.tokens as i64)),
        ("signed", Json::num(st.signed as i64)),
        ("downloads", Json::num(st.downloads as i64)),
        ("archive_bytes", Json::num(st.archive_bytes as i64)),
        ("seq", Json::num(seq)),
    ])
}

/// A change-log entry (mirror replication feed).
pub fn change_json(c: &Change) -> Json {
    Json::obj(vec![
        ("seq", Json::num(c.seq)),
        ("name", Json::str(&c.package)),
        ("version", Json::str(&c.version)),
        ("kind", Json::str(c.kind.as_str())),
        ("at", Json::num(c.at)),
    ])
}

/// Service index (`GET /`).
pub fn index_json(service: &str, version: &str, backend: &str, st: &Stats) -> Json {
    Json::obj(vec![
        ("service", Json::str(service)),
        ("version", Json::str(version)),
        ("backend", Json::str(backend)),
        ("packages", Json::num(st.packages as i64)),
        ("versions", Json::num(st.versions as i64)),
        (
            "endpoints",
            Json::arr(
                [
                    "/health",
                    "/stats",
                    "/keys",
                    "/search?q=",
                    "/packages/{name}",
                    "/packages/{name}/{version}",
                    "/packages/{name}/versions",
                    "/api/publish",
                    "/api/yank",
                    "/auth/login",
                    "/auth/logout",
                    "/auth/tokens",
                    "/api/mirror/changes",
                ]
                .into_iter()
                .map(Json::str)
                .collect::<Vec<Json>>(),
            ),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::now_secs;
    use hs_pm::semver::Version;

    fn sample_version() -> PackageVersion {
        PackageVersion {
            name: "jwt".to_string(),
            version: Version::parse("1.2.0").unwrap(),
            deps: vec![Dep::normal("base64", "^0.4.0")],
            integrity: "sha256:aa".to_string(),
            fingerprint: "sha256:bb".to_string(),
            signature: Some("sig".to_string()),
            key_id: Some("k:11".to_string()),
            size: 42,
            file_count: 2,
            files: vec!["hard.toml".to_string(), "src/main.hard".to_string()],
            channel: Some("stable".to_string()),
            yanked: false,
            downloads: 7,
            published_at: 1_700_000_000,
        }
    }

    #[test]
    fn version_json_carries_every_security_field() {
        let j = version_json(&sample_version()).to_string();
        assert!(j.contains("\"integrity\":\"sha256:aa\""));
        assert!(j.contains("\"fingerprint\":\"sha256:bb\""));
        assert!(j.contains("\"signature\":\"sig\""));
        assert!(j.contains("\"key_id\":\"k:11\""));
        assert!(j.contains("\"dependencies\":[{\"name\":\"base64\",\"req\":\"^0.4.0\",\"kind\":\"normal\"}]"));
        // deterministic: same input, same bytes
        assert_eq!(version_json(&sample_version()).to_string(), j);
    }

    #[test]
    fn version_json_omits_absent_signature() {
        let mut v = sample_version();
        v.signature = None;
        v.key_id = None;
        let j = version_json(&v).to_string();
        assert!(!j.contains("signature"));
        assert!(!j.contains("key_id"));
    }

    #[test]
    fn package_json_reports_latest_unyanked_version() {
        let mut p = Package::new("jwt");
        p.tags = vec!["auth".to_string()];
        let mut v1 = sample_version();
        v1.version = Version::parse("1.0.0").unwrap();
        let mut v2 = sample_version();
        v2.version = Version::parse("2.0.0").unwrap();
        v2.yanked = true;
        let j = package_json(&p, &[v1, v2]).to_string();
        // 2.0.0 is yanked, so `latest` falls back to 1.0.0
        assert!(j.contains("\"latest\":\"1.0.0\""), "{j}");
        assert!(j.contains("\"version_map\":{\"1.0.0\":"));
    }

    #[test]
    fn package_json_with_no_versions_has_null_latest() {
        let p = Package::new("empty");
        let j = package_json(&p, &[]).to_string();
        assert!(j.contains("\"latest\":null"));
        assert!(j.contains("\"versions\":[]"));
    }

    #[test]
    fn search_json_reports_totals() {
        let hits = vec![SearchHit {
            name: "jwt".to_string(),
            version: Some("1.0.0".to_string()),
            description: Some("tokens".to_string()),
            license: None,
            downloads: 3,
            tags: vec!["auth".to_string()],
            keywords: vec!["tokens".to_string()],
            owner: Some("ada".to_string()),
            score: 90,
        }];
        let j = search_json(&hits, "jw", 1).to_string();
        assert!(j.contains("\"total\":1"));
        assert!(j.contains("\"count\":1"));
        assert!(j.contains("\"score\":90"));
        assert!(j.contains("\"owner\":\"ada\""), "{j}");
        assert!(j.contains("\"keywords\":[\"tokens\"]"), "{j}");
    }

    #[test]
    fn stats_json_is_stable() {
        let st = Stats {
            packages: 2,
            versions: 5,
            yanked: 1,
            users: 1,
            tokens: 2,
            signed: 4,
            downloads: 10,
            archive_bytes: 900,
            seq: 3,
        };
        let j = stats_json(&st, "sqlite", 3).to_string();
        assert_eq!(j, stats_json(&st, "sqlite", 3).to_string());
        assert!(j.contains("\"backend\":\"sqlite\""));
    }

    #[test]
    fn token_json_only_includes_plaintext_when_given() {
        let with = token_json("tok_1", "ada", "ci", &[Scope::Read], Some("hspat_x"), now_secs())
            .to_string();
        assert!(with.contains("\"token\":\"hspat_x\""));
        let without = token_json("tok_1", "ada", "ci", &[Scope::Read], None, 0).to_string();
        assert!(!without.contains("hspat_x"));
    }
}
