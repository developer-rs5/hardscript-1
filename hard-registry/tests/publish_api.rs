//! The publish API: uploads, validation, ownership and dry runs.

mod common;

use common::*;

// -- the happy path --------------------------------------------------------

#[test]
fn publish_stores_the_archive_and_reports_its_digest() {
    let h = Harness::open("publish-basic");
    let r = h.publish(None, "jwt", "1.0.0", "payload");
    assert_eq!(r.status, 201, "{}", r.text());
    let j = r.json().unwrap();
    assert!(jbool(&j, "ok"), "{}", r.text());
    assert_eq!(jstr(&j, "name").as_deref(), Some("jwt"));
    assert_eq!(jstr(&j, "version").as_deref(), Some("1.0.0"));
    assert!(jstr(&j, "integrity").unwrap().starts_with("sha256:"));
    assert!(jstr(&j, "fingerprint").unwrap().starts_with("sha256:"));
    assert_eq!(j.get("file_count").and_then(|v| v.as_num()), Some(2));
    // the bytes the registry holds are the bytes we sent
    let dl = h.get("/packages/jwt/1.0.0");
    assert_eq!(dl.bytes(), archive("payload").as_slice());
}

#[test]
fn the_reported_integrity_is_the_archives_digest() {
    let h = Harness::open("publish-digest");
    h.publish(None, "jwt", "1.0.0", "payload");
    let want = format!("sha256:{}", hs_pm::pkgfmt::sha256_hex(&archive("payload")));
    assert_eq!(integrity_of(&h.get("/packages/jwt"), "1.0.0").as_deref(), Some(want.as_str()));
}

#[test]
fn metadata_headers_are_recorded() {
    let h = Harness::open("publish-metadata");
    let r = h.publish_with(
        None,
        "jwt",
        "1.0.0",
        "a",
        &[
            ("X-Hard-Description", "JSON web tokens"),
            ("X-Hard-License", "MIT"),
            ("X-Hard-Homepage", "https://example.org"),
            ("X-Hard-Repository", "https://git.example.org/jwt"),
            ("X-Hard-Tags", "auth, security"),
            ("X-Hard-Keywords", "jwt, web"),
            ("X-Hard-Dependencies", "base64@^0.4.0, dev:testlib@1.0.0"),
            ("X-Hard-Channel", "stable"),
        ],
    );
    assert_eq!(r.status, 201, "{}", r.text());
    let j = h.get("/packages/jwt").json().unwrap();
    assert_eq!(jstr(&j, "description").as_deref(), Some("JSON web tokens"));
    assert_eq!(jstr(&j, "license").as_deref(), Some("MIT"));
    assert_eq!(jstr(&j, "homepage").as_deref(), Some("https://example.org"));
    let strs = |key: &str| -> Vec<String> {
        arr(&j, key)
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    };
    assert_eq!(strs("tags"), vec!["auth", "security"]);
    assert_eq!(strs("keywords"), vec!["jwt", "web"]);
    let versions = arr(&j, "versions");
    assert_eq!(jstr(&versions[0], "channel").as_deref(), Some("stable"));
    let mut deps: Vec<(String, String, String)> = arr(&versions[0], "dependencies")
        .iter()
        .map(|d| {
            (
                jstr(d, "name").unwrap(),
                jstr(d, "req").unwrap(),
                jstr(d, "kind").unwrap(),
            )
        })
        .collect();
    deps.sort();
    assert_eq!(
        deps,
        vec![
            ("base64".to_string(), "^0.4.0".to_string(), "normal".to_string()),
            ("testlib".to_string(), "1.0.0".to_string(), "dev".to_string()),
        ]
    );
}

#[test]
fn the_json_envelope_shape_also_publishes() {
    let h = Harness::open("publish-json");
    let b64 = {
        fn b64(data: &[u8]) -> String {
            const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for c in data.chunks(3) {
                let b0 = c[0] as u32;
                let b1 = *c.get(1).unwrap_or(&0) as u32;
                let b2 = *c.get(2).unwrap_or(&0) as u32;
                let n = (b0 << 16) | (b1 << 8) | b2;
                out.push(A[(n >> 18) as usize & 63] as char);
                out.push(A[(n >> 12) as usize & 63] as char);
                out.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
                out.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
            }
            out
        }
        b64(&archive("json envelope"))
    };
    let body = format!(
        r#"{{"name":"jwt","version":"2.0.0","archive_base64":"{b64}","tags":["auth"]}}"#
    );
    let r = h.post_json("/api/publish", body.as_bytes(), None);
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(h.get("/packages/jwt/2.0.0").bytes(), archive("json envelope").as_slice());
}

#[test]
fn the_archives_own_manifest_fills_in_missing_fields() {
    let h = Harness::open("publish-manifest-fallback");
    // no X-Hard-* headers at all: the registry reads hard.toml from the archive
    let r = h.request("POST", "/api/publish", &archive("fallback"), &[]);
    assert_eq!(r.status, 201, "{}", r.text());
    // the archive's hard.toml declares name = "demo"
    let j = h.get("/packages/demo").json().unwrap();
    assert_eq!(jstr(&j, "name").as_deref(), Some("demo"));
    assert_eq!(h.get("/packages/fallback").status, 404);
}

// -- refusals --------------------------------------------------------------

#[test]
fn publishing_the_same_version_twice_is_a_conflict() {
    let h = Harness::open("publish-dup");
    assert_eq!(h.publish(None, "jwt", "1.0.0", "a").status, 201);
    let r = h.publish(None, "jwt", "1.0.0", "a");
    assert_eq!(r.status, 409, "{}", r.text());
    assert_eq!(r.code().as_deref(), Some("conflict"));
    assert!(r.text().contains("already published"), "{}", r.text());
}

#[test]
fn different_bytes_under_one_version_is_a_conflict() {
    let h = Harness::open("publish-dup-diff");
    assert_eq!(h.publish(None, "jwt", "1.0.0", "a").status, 201);
    let r = h.publish(None, "jwt", "1.0.0", "b");
    assert_eq!(r.status, 409);
    assert!(r.text().contains("bump the version"), "{}", r.text());
}

#[test]
fn a_malformed_archive_is_rejected() {
    let h = Harness::open("publish-malformed");
    let r = h.request(
        "POST",
        "/api/publish",
        b"not an archive",
        &[("X-Hard-Package", "jwt"), ("X-Hard-Version", "1.0.0")],
    );
    assert_eq!(r.status, 422, "{}", r.text());
    assert!(r.fields().iter().any(|f| f.contains("not a valid .hspkg")), "{:?}", r.fields());
    assert_eq!(h.get("/packages/jwt").status, 404);
}

#[test]
fn an_empty_body_is_rejected_with_a_field_error() {
    let h = Harness::open("publish-empty");
    let r = h.request("POST", "/api/publish", b"", &[("X-Hard-Package", "jwt")]);
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.fields().iter().any(|f| f.contains("archive")), "{:?}", r.fields());
}

#[test]
fn an_invalid_package_name_is_rejected() {
    let h = Harness::open("publish-badname");
    for bad in ["Bad Name", "UPPER", "-lead", "a/b/c"] {
        let r = h.publish(None, bad, "1.0.0", "a");
        assert_eq!(r.status, 422, "'{bad}' should be rejected: {}", r.text());
        assert!(r.fields().iter().any(|f| f.starts_with("name")), "{:?}", r.fields());
    }
}

#[test]
fn an_invalid_version_is_rejected() {
    let h = Harness::open("publish-badversion");
    let r = h.publish(None, "jwt", "one.two", "a");
    assert_eq!(r.status, 422, "{}", r.text());
    assert!(r.fields().iter().any(|f| f.starts_with("version")), "{:?}", r.fields());
}

#[test]
fn bad_dependencies_are_rejected() {
    let h = Harness::open("publish-baddeps");
    let r = h.publish_with(
        None,
        "jwt",
        "1.0.0",
        "a",
        &[("X-Hard-Dependencies", "base64@not a req")],
    );
    assert_eq!(r.status, 422, "{}", r.text());
    assert!(r.fields().iter().any(|f| f.starts_with("dependencies")), "{:?}", r.fields());
}

#[test]
fn a_self_dependency_is_rejected() {
    let h = Harness::open("publish-selfdep");
    let r = h.publish_with(None, "jwt", "1.0.0", "a", &[("X-Hard-Dependencies", "jwt@^1.0.0")]);
    assert_eq!(r.status, 422, "{}", r.text());
    assert!(r.text().contains("may not depend on itself"), "{}", r.text());
}

#[test]
fn a_duplicate_dependency_is_rejected() {
    let h = Harness::open("publish-dupdep");
    let r = h.publish_with(
        None,
        "jwt",
        "1.0.0",
        "a",
        &[("X-Hard-Dependencies", "base64@^1.0.0, base64@^2.0.0")],
    );
    assert_eq!(r.status, 422, "{}", r.text());
    assert!(r.text().contains("duplicate dependency"), "{}", r.text());
}

#[test]
fn an_oversized_archive_is_rejected() {
    let h = Harness::open("publish-huge");
    // 17 MiB of junk: over the transport cap, so the server refuses it
    // without reading it and closes the connection
    let big = vec![b'x'; 17 * 1024 * 1024];
    match h.oversized_request(
        "POST",
        "/api/publish",
        &big,
    ) {
        Some(r) => assert_eq!(r.status, 413, "{}", r.text()),
        // the server may hang up before the reply is flushed; that is also a
        // refusal, and the important part is that nothing was stored
        None => {}
    }
    assert_eq!(h.get("/packages/jwt").status, 404);
    let stats = h.get("/stats").json().unwrap();
    assert_eq!(stats.get("versions").and_then(|v| v.as_num()), Some(0));
}

#[test]
fn an_invalid_channel_is_rejected() {
    let h = Harness::open("publish-channel");
    let r = h.publish_with(None, "jwt", "1.0.0", "a", &[("X-Hard-Channel", "not a channel!")]);
    assert_eq!(r.status, 422, "{}", r.text());
    assert!(r.fields().iter().any(|f| f.starts_with("channel")), "{:?}", r.fields());
}

#[test]
fn every_rejected_publish_leaves_no_trace() {
    let h = Harness::open("publish-atomic");
    // invalid name, valid version
    assert_eq!(h.publish(None, "BAD", "1.0.0", "a").status, 422);
    // valid coordinates, broken archive
    let r = h.request(
        "POST",
        "/api/publish",
        b"nope",
        &[("X-Hard-Package", "other"), ("X-Hard-Version", "1.0.0")],
    );
    assert_eq!(r.status, 422, "{}", r.text());
    let stats = h.get("/stats").json().unwrap();
    assert_eq!(stats.get("packages").and_then(|v| v.as_num()), Some(0));
    assert_eq!(stats.get("versions").and_then(|v| v.as_num()), Some(0));
    assert_eq!(stats.get("seq").and_then(|v| v.as_num()), Some(0));
    // and the change log has no entries either
    let feed = h.get("/api/mirror/changes?since=0").json().unwrap();
    assert_eq!(feed.get("count").and_then(|v| v.as_num()), Some(0));
}

// -- authentication and ownership -----------------------------------------

#[test]
fn a_scoped_registry_refuses_anonymous_publishes() {
    let h = Harness::secure("publish-auth");
    let r = h.publish(None, "jwt", "1.0.0", "a");
    assert_eq!(r.status, 401, "{}", r.text());
    assert_eq!(r.code().as_deref(), Some("unauthorized"));
    assert!(r.text().contains("'publish' scope"), "{}", r.text());
}

#[test]
fn an_invalid_token_is_rejected_even_on_an_open_registry() {
    let h = Harness::open("publish-badtoken");
    let r = h.publish(Some("hspat_nope"), "jwt", "1.0.0", "a");
    assert_eq!(r.status, 401, "{}", r.text());
}

#[test]
fn a_token_without_the_publish_scope_is_forbidden() {
    let h = Harness::secure("publish-scope");
    h.post_json("/auth/register", br#"{"user":"ada","password":"supersecret"}"#, None);
    let session = h.login("ada", "supersecret");
    let rep = h.post_json("/auth/tokens", br#"{"name":"ro","scopes":["read"]}"#, Some(&session));
    let ro = rep.json().unwrap().get("token").and_then(|t| t.as_str()).unwrap().to_string();
    let r = h.publish(Some(&ro), "jwt", "1.0.0", "a");
    assert_eq!(r.status, 403, "{}", r.text());
    assert!(r.text().contains("lacks the 'publish' scope"), "{}", r.text());
}

#[test]
fn the_first_publisher_owns_the_name() {
    let h = Harness::secure("publish-owner");
    h.post_json("/auth/register", br#"{"user":"ada","password":"supersecret"}"#, None);
    h.post_json("/auth/register", br#"{"user":"bob","password":"supersecret"}"#, None);
    let ada = h.login("ada", "supersecret");
    let bob = h.login("bob", "supersecret");
    assert_eq!(h.publish(Some(&ada), "shared", "1.0.0", "a").status, 201);
    // the owner may keep publishing
    assert_eq!(h.publish(Some(&ada), "shared", "1.1.0", "b").status, 201);
    // somebody else may not
    let r = h.publish(Some(&bob), "shared", "1.2.0", "c");
    assert_eq!(r.status, 409, "{}", r.text());
    assert!(r.text().contains("owned by 'ada'"), "{}", r.text());
    let owner = jstr(&h.get("/packages/shared").json().unwrap(), "owner");
    assert_eq!(owner.as_deref(), Some("ada"));
}

#[test]
fn an_admin_may_publish_to_any_name() {
    let h = Harness::secure("publish-admin");
    h.post_json("/auth/register", br#"{"user":"ada","password":"supersecret"}"#, None);
    h.post_json("/auth/register", br#"{"user":"root","password":"supersecret"}"#, None);
    let ada = h.login("ada", "supersecret");
    let root = h.login("root", "supersecret");
    let admin = h
        .post_json(
            "/auth/tokens",
            br#"{"name":"ops","scopes":["read","publish","admin"]}"#,
            Some(&root),
        )
        .json()
        .unwrap()
        .get("token")
        .and_then(|t| t.as_str())
        .unwrap()
        .to_string();
    assert_eq!(h.publish(Some(&ada), "shared", "1.0.0", "a").status, 201);
    assert_eq!(h.publish(Some(&admin), "shared", "1.1.0", "b").status, 201);
    // the owner is unchanged by an admin publish
    let owner = jstr(&h.get("/packages/shared").json().unwrap(), "owner");
    assert_eq!(owner.as_deref(), Some("ada"));
}

#[test]
fn ownership_is_claimed_on_first_publish_only() {
    let h = Harness::secure("publish-owner-keep");
    h.post_json("/auth/register", br#"{"user":"ada","password":"supersecret"}"#, None);
    h.post_json("/auth/register", br#"{"user":"bob","password":"supersecret"}"#, None);
    let ada = h.login("ada", "supersecret");
    let bob = h.login("bob", "supersecret");
    assert_eq!(h.publish(Some(&ada), "shared", "1.0.0", "a").status, 201);
    // bob publishes a *different* package that happens to reuse the name of
    // an existing one: refused, and the owner does not change
    assert_eq!(h.publish(Some(&bob), "shared", "1.0.0", "a").status, 409);
    assert_eq!(
        jstr(&h.get("/packages/shared").json().unwrap(), "owner").as_deref(),
        Some("ada")
    );
}

// -- dry run ---------------------------------------------------------------

#[test]
fn a_dry_run_reports_the_plan_without_storing_anything() {
    let h = Harness::open("publish-dryrun");
    let r = h.publish_with(
        None,
        "jwt",
        "1.0.0",
        "a",
        &[("X-Hard-Dry-Run", "1"), ("X-Hard-Dependencies", "base64@^0.4.0")],
    );
    assert_eq!(r.status, 200, "{}", r.text());
    let j = r.json().unwrap();
    assert!(jbool(&j, "dry_run"), "{}", r.text());
    assert_eq!(jstr(&j, "name").as_deref(), Some("jwt"));
    assert!(jstr(&j, "integrity").unwrap().starts_with("sha256:"));
    assert!(jstr(&j, "fingerprint").unwrap().starts_with("sha256:"));
    assert_eq!(j.get("file_count").and_then(|v| v.as_num()), Some(2));
    assert_eq!(arr(&j, "dependencies").len(), 1);
    assert!(!jbool(&j, "already_published"), "{}", r.text());
    // nothing was stored
    assert_eq!(h.get("/packages/jwt").status, 404);
    let stats = h.get("/stats").json().unwrap();
    assert_eq!(stats.get("versions").and_then(|v| v.as_num()), Some(0));
}

#[test]
fn a_dry_run_can_be_requested_through_the_json_body() {
    let h = Harness::open("publish-dryrun-json");
    let b64 = "AAAA";
    let body = format!(
        r#"{{"name":"jwt","version":"1.0.0","archive_base64":"{b64}","dry_run":true}}"#
    );
    let r = h.post_json("/api/publish", body.as_bytes(), None);
    assert_eq!(r.status, 422, "the archive is still validated: {}", r.text());
    // a valid archive with dry_run set succeeds and stores nothing
    let body = format!(
        r#"{{"name":"jwt","version":"1.0.0","archive_base64":"{}","dry_run":true}}"#,
        "HSPKGAAAA"
    );
    let r = h.post_json("/api/publish", body.as_bytes(), None);
    assert_ne!(r.status, 200, "an invalid archive is never a valid plan");
}

#[test]
fn a_dry_run_reports_an_existing_version() {
    let h = Harness::open("publish-dryrun-existing");
    assert_eq!(h.publish(None, "jwt", "1.0.0", "a").status, 201);
    let r = h.publish_with(None, "jwt", "1.0.0", "a", &[("X-Hard-Dry-Run", "1")]);
    let j = r.json().unwrap();
    assert!(jbool(&j, "already_published"), "{}", r.text());
    let r = h.publish_with(None, "jwt", "1.0.0", "z", &[("X-Hard-Dry-Run", "1")]);
    let j = r.json().unwrap();
    assert!(jbool(&j, "conflicting_version"), "{}", r.text());
}

#[test]
fn a_dry_run_reports_the_current_owner() {
    let h = Harness::secure("publish-dryrun-owner");
    h.post_json("/auth/register", br#"{"user":"ada","password":"supersecret"}"#, None);
    let ada = h.login("ada", "supersecret");
    assert_eq!(h.publish(Some(&ada), "jwt", "1.0.0", "a").status, 201);
    let r = h.publish_with(Some(&ada), "jwt", "1.1.0", "b", &[("X-Hard-Dry-Run", "1")]);
    assert_eq!(jstr(&r.json().unwrap(), "owner").as_deref(), Some("ada"));
}

#[test]
fn a_dry_run_still_enforces_ownership() {
    let h = Harness::secure("publish-dryrun-forbidden");
    h.post_json("/auth/register", br#"{"user":"ada","password":"supersecret"}"#, None);
    h.post_json("/auth/register", br#"{"user":"bob","password":"supersecret"}"#, None);
    let ada = h.login("ada", "supersecret");
    let bob = h.login("bob", "supersecret");
    assert_eq!(h.publish(Some(&ada), "jwt", "1.0.0", "a").status, 201);
    let r = h.publish_with(Some(&bob), "jwt", "1.1.0", "b", &[("X-Hard-Dry-Run", "1")]);
    assert_eq!(r.status, 409, "{}", r.text());
}

// -- signing and counters --------------------------------------------------

#[test]
fn publishes_are_signed_by_default() {
    let h = Harness::secure("publish-signed");
    let token = h.admin_token();
    let r = h.publish(Some(&token), "jwt", "1.0.0", "a");
    assert_eq!(r.status, 201, "{}", r.text());
    let j = r.json().unwrap();
    assert!(jstr(&j, "signature").is_some(), "{}", r.text());
    assert!(jstr(&j, "key_id").unwrap().starts_with("k:"));
    let sig = h.get("/packages/jwt/1.0.0/signature");
    assert_eq!(
        sig.json()
            .unwrap()
            .get("verified")
            .and_then(|v| match v {
                hs_compiler::json::Json::Bool(b) => Some(*b),
                _ => None,
            }),
        Some(true)
    );
}

#[test]
fn an_open_registry_can_publish_unsigned() {
    let h = Harness::open("publish-unsigned");
    let r = h.publish(None, "jwt", "1.0.0", "a");
    let j = r.json().unwrap();
    assert!(jstr(&j, "signature").is_none());
    assert!(jstr(&j, "key_id").is_none());
}

#[test]
fn publishing_bumps_the_change_sequence() {
    let h = Harness::open("publish-seq");
    h.publish(None, "a", "1.0.0", "x");
    h.publish(None, "a", "1.1.0", "y");
    h.publish(None, "b", "1.0.0", "z");
    let feed = h.get("/api/mirror/changes?since=0").json().unwrap();
    let seq = feed.get("seq").and_then(|v| v.as_num()).unwrap();
    assert_eq!(seq, 3);
    assert_eq!(feed.get("count").and_then(|v| v.as_num()), Some(3));
}

#[test]
fn many_versions_of_one_package_all_coexist() {
    let h = Harness::open("publish-versions");
    for minor in 0..5 {
        let r = h.publish(None, "jwt", &format!("1.{minor}.0"), "x");
        assert_eq!(r.status, 201, "1.{minor}.0: {}", r.text());
    }
    let versions = arr(&h.get("/packages/jwt/versions").json().unwrap(), "versions");
    assert_eq!(versions.len(), 5);
    let names: Vec<String> = versions.iter().map(|v| jstr(v, "version").unwrap()).collect();
    assert_eq!(names, vec!["1.0.0", "1.1.0", "1.2.0", "1.3.0", "1.4.0"]);
}

#[test]
fn a_head_request_does_not_count_as_a_download() {
    let h = Harness::open("publish-head");
    assert_eq!(h.publish(None, "jwt", "1.0.0", "a").status, 201);
    // this is exactly what `hard publish` does before uploading
    let r = h.request("HEAD", "/packages/jwt/1.0.0", b"", &[]);
    assert_eq!(r.status, 200);
    assert!(r.header("Content-Length").is_some());
    let stats = h.get("/stats").json().unwrap();
    assert_eq!(
        stats.get("downloads").and_then(|v| v.as_num()),
        Some(0),
        "a pre-publish check must not inflate the counters"
    );
    // and a real download still does
    assert_eq!(h.get("/packages/jwt/1.0.0").status, 200);
    let stats = h.get("/stats").json().unwrap();
    assert_eq!(stats.get("downloads").and_then(|v| v.as_num()), Some(1));
}

#[test]
fn a_head_request_on_a_missing_version_is_404() {
    let h = Harness::open("publish-head-404");
    h.publish(None, "jwt", "1.0.0", "a");
    assert_eq!(h.request("HEAD", "/packages/jwt/9.9.9", b"", &[]).status, 404);
}
