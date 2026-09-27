//! The REST surface, end to end over a real socket.
//!
//! These tests never call the store directly: everything goes through the
//! listener a `hard` client talks to, so framing, status codes, headers and
//! binary bodies are all covered.

mod common;

use common::*;

// -- service endpoints -----------------------------------------------------

#[test]
fn index_reports_service_and_endpoints() {
    let h = Harness::secure("index");
    let r = h.get("/");
    assert_eq!(r.status, 200);
    let j = r.json().expect("json");
    assert_eq!(jstr(&j, "service").as_deref(), Some("hardscript-registry"));
    assert_eq!(jstr(&j, "backend").as_deref(), Some("sqlite"));
    let endpoints = arr(&j, "endpoints");
    assert!(endpoints.iter().any(|e| e.as_str() == Some("/packages/{name}")));
}

#[test]
fn health_is_ok_when_the_store_answers() {
    let h = Harness::secure("health");
    let r = h.get("/health");
    assert_eq!(r.status, 200);
    assert_eq!(jstr(&r.json().unwrap(), "status").as_deref(), Some("ok"));
}

#[test]
fn stats_start_empty() {
    let h = Harness::secure("stats-empty");
    let r = h.get("/stats");
    let j = r.json().unwrap();
    assert_eq!(j.get("packages").and_then(|v| v.as_num()), Some(0));
    assert_eq!(j.get("versions").and_then(|v| v.as_num()), Some(0));
    assert_eq!(j.get("seq").and_then(|v| v.as_num()), Some(0));
}

#[test]
fn keys_expose_the_ed25519_public_key() {
    let h = Harness::secure("keys");
    let r = h.get("/keys");
    let j = r.json().unwrap();
    assert_eq!(jstr(&j, "algorithm").as_deref(), Some("ed25519"));
    let pk = jstr(&j, "public_key").expect("public key");
    assert_eq!(pk.len(), 64, "hex-encoded ed25519 public key: {pk}");
    assert!(jstr(&j, "key_id").unwrap().starts_with("k:"));
    assert_eq!(j.get("test_key").and_then(|v| match v {
        hs_compiler::json::Json::Bool(b) => Some(*b),
        _ => None,
    }), Some(true));
}

#[test]
fn every_response_carries_the_service_header() {
    let h = Harness::secure("headers");
    for path in ["/", "/health", "/stats", "/keys", "/search?q=x"] {
        let r = h.get(path);
        assert_eq!(
            r.header("X-Hard-Registry"),
            Some("hardscript-registry"),
            "missing header on {path}"
        );
        assert_eq!(r.header("X-Content-Type-Options"), Some("nosniff"), "on {path}");
        assert!(r.header("Content-Length").is_some(), "no Content-Length on {path}");
    }
}

#[test]
fn unknown_routes_are_404_json() {
    let h = Harness::secure("404");
    let r = h.get("/does/not/exist");
    assert_eq!(r.status, 404);
    assert_eq!(r.code().as_deref(), Some("not_found"));
    assert_eq!(r.header("Content-Type"), Some("application/json"));
}

#[test]
fn wrong_verbs_are_405() {
    let h = Harness::secure("405");
    assert_eq!(h.get("/packages").status, 405);
    assert_eq!(h.get("/api/publish").status, 405);
    assert_eq!(h.request("PUT", "/packages/x", b"", &[]).status, 405);
}

// -- metadata --------------------------------------------------------------

#[test]
fn metadata_lists_every_version_with_dependencies() {
    let h = Harness::open("meta-versions");
    h.seed("app", "1.0.0", "a");
    h.seed("app", "1.5.0", "b");
    let r = h.get("/packages/app");
    assert_eq!(r.status, 200);
    let j = r.json().unwrap();
    assert_eq!(jstr(&j, "latest").as_deref(), Some("1.5.0"));
    let versions = arr(&j, "versions");
    assert_eq!(versions.len(), 2);
    assert_eq!(jstr(&versions[0], "version").as_deref(), Some("1.0.0"));
}

#[test]
fn metadata_for_an_unknown_package_is_404() {
    let h = Harness::open("meta-404");
    let r = h.get("/packages/nope");
    assert_eq!(r.status, 404);
    assert!(r.text().contains("no package named 'nope'"), "{}", r.text());
}

#[test]
fn version_list_endpoint() {
    let h = Harness::open("version-list");
    h.seed("lib", "1.0.0", "a");
    h.seed("lib", "2.0.0", "b");
    h.seed("lib", "2.1.0", "c");
    let r = h.get("/packages/lib/versions");
    assert_eq!(r.status, 200);
    let j = r.json().unwrap();
    assert_eq!(j.get("count").and_then(|v| v.as_num()), Some(3));
    let list = arr(&j, "versions");
    let names: Vec<String> = list.iter().map(|v| jstr(v, "version").unwrap()).collect();
    assert_eq!(names, vec!["1.0.0", "2.0.0", "2.1.0"], "versions must ascend");
    assert_eq!(h.get("/packages/nope/versions").status, 404);
}

#[test]
fn per_version_manifest_endpoint() {
    let h = Harness::open("manifest");
    h.seed("jwt", "1.0.0", "a");
    let r = h.get("/packages/jwt/1.0.0/manifest");
    assert_eq!(r.status, 200);
    let j = r.json().unwrap();
    assert_eq!(jstr(&j, "name").as_deref(), Some("jwt"));
    assert!(jstr(&j, "fingerprint").unwrap().starts_with("sha256:"));
    assert!(jstr(&j, "integrity").unwrap().starts_with("sha256:"));
    assert_eq!(h.get("/packages/jwt/9.9.9/manifest").status, 404);
}

// -- download --------------------------------------------------------------

#[test]
fn download_returns_the_exact_archive_bytes() {
    let h = Harness::open("download");
    h.seed("jwt", "1.0.0", "payload");
    let r = h.get("/packages/jwt/1.0.0");
    assert_eq!(r.status, 200);
    assert_eq!(r.bytes(), archive("payload").as_slice());
    assert_eq!(
        r.header("Content-Type"),
        Some("application/vnd.hardscript.package")
    );
    assert!(r.header("X-Hard-Integrity").unwrap().starts_with("sha256:"));
    assert!(r.header("ETag").unwrap().starts_with('"'));
}

#[test]
fn download_reports_the_recorded_integrity() {
    let h = Harness::open("download-integrity");
    h.seed("jwt", "1.0.0", "payload");
    let meta = h.get("/packages/jwt");
    let want = integrity_of(&meta, "1.0.0").expect("integrity");
    let r = h.get("/packages/jwt/1.0.0");
    assert_eq!(r.header("X-Hard-Integrity"), Some(want.as_str()));
}

#[test]
fn downloading_an_unknown_version_is_404() {
    let h = Harness::open("download-404");
    h.seed("jwt", "1.0.0", "a");
    assert_eq!(h.get("/packages/jwt/2.0.0").status, 404);
    assert_eq!(h.get("/packages/ghost/1.0.0").status, 404);
}

#[test]
fn downloads_are_counted_per_version_and_per_package() {
    let h = Harness::open("download-count");
    h.seed("jwt", "1.0.0", "a");
    h.seed("jwt", "2.0.0", "b");
    h.get("/packages/jwt/1.0.0");
    h.get("/packages/jwt/1.0.0");
    h.get("/packages/jwt/2.0.0");
    let j = h.get("/stats").json().unwrap();
    assert_eq!(j.get("downloads").and_then(|v| v.as_num()), Some(3));
    let meta = h.get("/packages/jwt").json().unwrap();
    assert_eq!(meta.get("downloads").and_then(|v| v.as_num()), Some(3));
}

#[test]
fn ranged_download_returns_a_slice() {
    let h = Harness::open("range");
    h.seed("jwt", "1.0.0", "a");
    let r = h.request("GET", "/packages/jwt/1.0.0", b"", &[("Range", "bytes=0-3")]);
    assert_eq!(r.status, 206);
    assert_eq!(r.bytes(), b"HSPK");
    assert!(r.header("Content-Range").unwrap().starts_with("bytes 0-3/"));
    // a resumed transfer is not a new download
    let j = h.get("/stats").json().unwrap();
    assert_eq!(j.get("downloads").and_then(|v| v.as_num()), Some(0));
}

#[test]
fn a_range_past_the_end_is_416() {
    let h = Harness::open("range-416");
    h.seed("jwt", "1.0.0", "a");
    let r = h.request("GET", "/packages/jwt/1.0.0", b"", &[("Range", "bytes=999999-999999")]);
    assert_eq!(r.status, 416);
    assert_eq!(r.code().as_deref(), Some("range_not_satisfiable"));
}

// -- search ----------------------------------------------------------------

#[test]
fn search_finds_exact_names_first() {
    let h = Harness::open("search-exact");
    h.seed("jwt", "1.0.0", "a");
    h.seed("jsonwebtoken", "1.0.0", "b");
    let names = result_names(&h.get("/search?q=jwt"));
    assert_eq!(names.first().map(String::as_str), Some("jwt"));
}

#[test]
fn search_reports_the_latest_version_and_downloads() {
    let h = Harness::open("search-latest");
    h.seed("jwt", "1.0.0", "a");
    h.seed("jwt", "1.4.0", "b");
    h.get("/packages/jwt/1.4.0");
    let r = h.get("/search?q=jwt");
    let results = arr(&r.json().unwrap(), "results");
    assert_eq!(jstr(&results[0], "version").as_deref(), Some("1.4.0"));
    assert_eq!(results[0].get("downloads").and_then(|v| v.as_num()), Some(1));
}

#[test]
fn search_filters_by_tag() {
    let h = Harness::open("search-tag");
    // tags are metadata, so publish through the API
    let r = h.publish_with(None, "web-server", "1.0.0", "a", &[("X-Hard-Tags", "web,http")]);
    assert_eq!(r.status, 201, "{}", r.text());
    let r = h.publish_with(None, "cli-tool", "1.0.0", "b", &[("X-Hard-Tags", "cli")]);
    assert_eq!(r.status, 201, "{}", r.text());
    let names = result_names(&h.get("/search?tag=web"));
    assert_eq!(names, vec!["web-server".to_string()]);
}

#[test]
fn search_by_prefix_is_a_hard_filter() {
    let h = Harness::open("search-prefix");
    h.seed("hard-toml", "1.0.0", "a");
    h.seed("json", "1.0.0", "b");
    let names = result_names(&h.get("/search?prefix=hard"));
    assert_eq!(names, vec!["hard-toml".to_string()]);
}

#[test]
fn search_respects_the_limit() {
    let h = Harness::open("search-limit");
    for i in 0..5 {
        h.seed(&format!("tool{i}"), "1.0.0", "a");
    }
    let r = h.get("/search?q=tool&limit=2");
    assert_eq!(r.json().unwrap().get("count").and_then(|v| v.as_num()), Some(2));
}

#[test]
fn an_empty_search_is_rejected() {
    let h = Harness::secure("search-empty");
    let r = h.get("/search");
    assert_eq!(r.status, 400);
    assert!(r.text().contains("?q="), "{}", r.text());
}

#[test]
fn search_with_no_hits_is_an_empty_result_not_an_error() {
    let h = Harness::open("search-nohits");
    h.seed("jwt", "1.0.0", "a");
    let r = h.get("/search?q=zzzzzzzz");
    assert_eq!(r.status, 200);
    assert!(result_names(&r).is_empty());
}

// -- mirror feeds ----------------------------------------------------------

#[test]
fn the_change_feed_lists_publishes_in_order() {
    let h = Harness::open("feed");
    h.seed("a", "1.0.0", "x");
    h.seed("a", "1.1.0", "y");
    h.seed("b", "0.1.0", "z");
    let r = h.get("/api/mirror/changes?since=0");
    let j = r.json().unwrap();
    let changes = arr(&j, "changes");
    assert_eq!(changes.len(), 3);
    let seqs: Vec<i64> = changes
        .iter()
        .map(|c| c.get("seq").and_then(|v| v.as_num()).unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq must ascend: {seqs:?}");
    assert_eq!(jstr(&changes[0], "kind").as_deref(), Some("published"));
}

#[test]
fn the_change_feed_honours_a_watermark_and_a_limit() {
    let h = Harness::open("feed-watermark");
    for i in 0..5 {
        h.seed(&format!("p{i}"), "1.0.0", "x");
    }
    let all = h.get("/api/mirror/changes?since=0&limit=100").json().unwrap();
    let seq = all.get("seq").and_then(|v| v.as_num()).unwrap();
    let r = h.get("/api/mirror/changes?since=0&limit=2").json().unwrap();
    assert_eq!(r.get("count").and_then(|v| v.as_num()), Some(2));
    let r = h.get(&format!("/api/mirror/changes?since={seq}")).json().unwrap();
    assert_eq!(r.get("count").and_then(|v| v.as_num()), Some(0));
}

#[test]
fn the_mirror_manifest_lists_every_version() {
    let h = Harness::open("feed-manifest");
    h.seed("a", "1.0.0", "x");
    h.seed("a", "2.0.0", "y");
    h.seed("b", "1.0.0", "z");
    let r = h.get("/api/mirror/manifest");
    assert_eq!(r.status, 200);
    let j = r.json().unwrap();
    assert_eq!(j.get("count").and_then(|v| v.as_num()), Some(3));
    assert!(j.get("seq").and_then(|v| v.as_num()).unwrap() >= 3);
}

// -- protocol details ------------------------------------------------------

#[test]
fn responses_are_byte_stable_across_repeats() {
    let h = Harness::open("stable");
    h.seed("jwt", "1.0.0", "a");
    let a = h.get("/packages/jwt");
    let b = h.get("/packages/jwt");
    assert_eq!(a.body, b.body, "identical state must render identical bytes");
    let s = h.get("/search?q=jwt");
    let s2 = h.get("/search?q=jwt");
    assert_eq!(s.body, s2.body);
}

#[test]
fn keep_alive_connections_serve_several_requests() {
    use std::io::{Read, Write};
    let h = Harness::open("keepalive");
    let addr = h.server.addr;
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    let head = format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: keep-alive\r\n\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    // read exactly the first response: the head, then Content-Length bytes
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).unwrap();
        assert!(n > 0, "connection closed before the head was complete");
        buf.push(byte[0]);
    }
    let head_text = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let len: usize = head_text
        .split("\r\n")
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).unwrap();
    buf.extend_from_slice(&body);
    let first = parse_reply(&buf);
    assert_eq!(first.status, 200);
    assert!(first.text().contains("\"status\":\"ok\""), "{}", first.text());
    let second_head = format!("GET /stats HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(second_head.as_bytes()).unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    let second = parse_reply(&rest);
    assert_eq!(second.status, 200);
    assert!(second.text().contains("packages"));
}

#[test]
fn a_malformed_request_line_gets_400() {
    use std::io::{Read, Write};
    let h = Harness::secure("bad-request");
    let addr = h.server.addr;
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    // no target at all: the server has nothing to route
    stream.write_all(b"\r\n").unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let r = parse_reply(&raw);
    assert!(r.status == 400 || r.status == 404, "status {}", r.status);
}

#[test]
fn head_requests_return_headers_without_a_body() {
    let h = Harness::open("head");
    h.seed("jwt", "1.0.0", "a");
    let r = h.request("HEAD", "/packages/jwt/1.0.0", b"", &[]);
    assert_eq!(r.status, 200);
    assert!(r.header("Content-Length").is_some());
    assert!(r.bytes().is_empty(), "HEAD must not carry a body");
}

#[test]
fn percent_encoded_package_names_round_trip() {
    let h = Harness::open("encoded");
    h.seed("acme/http", "1.0.0", "a");
    let r = h.get("/packages/acme%2Fhttp");
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(jstr(&r.json().unwrap(), "name").as_deref(), Some("acme/http"));
}

#[test]
fn an_open_ended_range_resumes_to_the_end() {
    let h = Harness::open("range-open");
    h.seed("jwt", "1.0.0", "a");
    let full = h.get("/packages/jwt/1.0.0");
    let size = full.bytes().len() as u64;
    let r = h.request("GET", "/packages/jwt/1.0.0", b"", &[("Range", "bytes=7-")]);
    assert_eq!(r.status, 206, "an open-ended Range must be honoured");
    assert_eq!(r.bytes().len() as u64, size - 7);
    assert_eq!(r.header("Content-Range"), Some(format!("bytes 7-{}/{}", size - 1, size).as_str()));
    assert_eq!(
        r.bytes(),
        &full.bytes()[7..],
        "a resumed download must continue, not restart"
    );
}

#[test]
fn a_range_past_the_end_is_refused_but_a_full_tail_works() {
    let h = Harness::open("range-tail");
    h.seed("jwt", "1.0.0", "a");
    let size = h.get("/packages/jwt/1.0.0").bytes().len() as u64;
    assert_eq!(
        h.request("GET", "/packages/jwt/1.0.0", b"", &[("Range", &format!("bytes={size}-"))]).status,
        416
    );
    // an end past the last byte is clamped rather than refused
    let r = h.request("GET", "/packages/jwt/1.0.0", b"", &[("Range", &format!("bytes=0-{}", size + 100))]);
    assert_eq!(r.status, 206);
    assert_eq!(r.bytes().len() as u64, size);
}
