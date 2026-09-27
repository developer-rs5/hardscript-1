//! Shared test harness for the registry integration suites.
//!
//! Every suite in this directory talks to a *real* listener over a real
//! socket using a hand-rolled HTTP/1.1 client, so the tests exercise the
//! same code path a `hard` client does: framing, headers, status codes and
//! binary bodies. Nothing here reaches into the store directly.

#![allow(dead_code)]

use hard_registry::app::{App, Config, PublishRequest};
use hard_registry::archives::ArchiveStore;
use hard_registry::http::TestServer;
use hard_registry::signing::SigningKey;
use hard_registry::sqlite::SqliteStore;
use hard_registry::Router;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A response, kept as raw bytes so archives survive intact.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    /// The body as text (lossy; use [`Reply::bytes`] for archives).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.body
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The parsed JSON body, or `None` when the body is not JSON.
    pub fn json(&self) -> Option<hs_compiler::json::Json> {
        hs_compiler::json::parse(&self.text())
    }

    /// A `field -> message` view of a `{"fields":[...]}` error body.
    pub fn fields(&self) -> Vec<String> {
        let j = match self.json() {
            Some(j) => j,
            None => return Vec::new(),
        };
        let mut out = Vec::new();
        if let Some(hs_compiler::json::Json::Arr(items)) = j.get("fields") {
            for it in items {
                let f = it.get("field").and_then(|v| v.as_str()).unwrap_or("");
                let m = it.get("message").and_then(|v| v.as_str()).unwrap_or("");
                out.push(format!("{f}: {m}"));
            }
        }
        out
    }

    /// The `error.code` of an error body.
    pub fn code(&self) -> Option<String> {
        self.json()?
            .get("error")?
            .get("code")?
            .as_str()
            .map(String::from)
    }
}

/// Counter keeping temporary directories unique inside one test binary.
static SEQ: AtomicU64 = AtomicU64::new(0);

fn unique(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("hs-reg-it-{tag}-{}-{n}", std::process::id()))
}

/// A `.hspkg` archive whose contents depend on `src`, so two archives built
/// from different sources have different digests.
pub fn archive(src: &str) -> Vec<u8> {
    hs_pm::pkgfmt::pack(&[
        hs_pm::pkgfmt::FileRecord {
            rel_path: "hard.toml".to_string(),
            data: format!("name = \"demo\"\nversion = \"1.0.0\"\n\n# {src}\n").into_bytes(),
        },
        hs_pm::pkgfmt::FileRecord {
            rel_path: "src/main.hard".to_string(),
            data: src.as_bytes().to_vec(),
        },
    ])
    .expect("pack archive")
}

/// A registry plus a listener in front of it.
pub struct Harness {
    pub app: Arc<App>,
    pub server: TestServer,
    pub dir: PathBuf,
    /// A token with every scope, created on first use.
    admin: std::cell::RefCell<Option<String>>,
}

impl Harness {
    /// A registry with authentication enforced and signing on.
    pub fn secure(tag: &str) -> Harness {
        Harness::with(tag, Config::default())
    }

    /// A registry that lets anybody publish and does not sign.
    pub fn open(tag: &str) -> Harness {
        Harness::with(tag, Config::permissive())
    }

    pub fn with(tag: &str, config: Config) -> Harness {
        let dir = unique(tag);
        let store = SqliteStore::open(dir.join("registry.db")).expect("open store");
        let app = Arc::new(App::new(
            Arc::new(store),
            ArchiveStore::at(dir.join("archives")),
            SigningKey::deterministic_for_tests(),
            config,
        ));
        let server = TestServer::start(Router::new(Arc::clone(&app)).into_handler());
        Harness {
            app,
            server,
            dir,
            admin: std::cell::RefCell::new(None),
        }
    }

    pub fn base(&self) -> &str {
        &self.server.base_url
    }

    /// Register an account and mint a token with every scope.
    pub fn admin_token(&self) -> String {
        if let Some(t) = self.admin.borrow().as_ref() {
            return t.clone();
        }
        let rep = self.post_json(
            "/auth/register",
            br#"{"user":"ada","password":"supersecret"}"#,
            None,
        );
        assert_eq!(rep.status, 201, "register: {}", rep.text());
        let session = self.login("ada", "supersecret");
        let rep = self.post_json(
            "/auth/tokens",
            br#"{"name":"qa","scopes":["read","publish","yank","token","admin"]}"#,
            Some(&session),
        );
        assert_eq!(rep.status, 201, "token: {}", rep.text());
        let token = rep
            .json()
            .and_then(|j| j.get("token").and_then(|t| t.as_str()).map(String::from))
            .expect("token plaintext");
        *self.admin.borrow_mut() = Some(token.clone());
        token
    }

    pub fn login(&self, user: &str, password: &str) -> String {
        let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
        let rep = self.post_json("/auth/login", body.as_bytes(), None);
        assert_eq!(rep.status, 200, "login: {}", rep.text());
        rep.json()
            .and_then(|j| j.get("token").and_then(|t| t.as_str()).map(String::from))
            .expect("session token")
    }

    // -- raw requests ------------------------------------------------------

    /// Issue an arbitrary request against the live listener.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> Reply {
        let addr = self.server.addr;
        let mut stream = TcpStream::connect(addr).expect("connect");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(20)));
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        head.push_str("Connection: close\r\n\r\n");
        stream.write_all(head.as_bytes()).expect("write head");
        if !body.is_empty() {
            stream.write_all(body).expect("write body");
        }
        stream.flush().expect("flush");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read reply");
        parse_reply(&raw)
    }

    pub fn get(&self, path: &str) -> Reply {
        self.request("GET", path, b"", &[])
    }

    pub fn post_json(&self, path: &str, body: &[u8], token: Option<&str>) -> Reply {
        let auth = token.map(|t| format!("Bearer {t}"));
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        if let Some(a) = auth.as_deref() {
            headers.push(("Authorization", a));
        }
        self.request("POST", path, body, &headers)
    }

    pub fn delete(&self, path: &str, token: Option<&str>) -> Reply {
        let auth = token.map(|t| format!("Bearer {t}"));
        let headers: Vec<(&str, &str)> = match auth.as_deref() {
            Some(a) => vec![("Authorization", a)],
            None => Vec::new(),
        };
        self.request("DELETE", path, b"", &headers)
    }

    /// Publish through the REST API using the raw-upload shape.
    pub fn publish(&self, token: Option<&str>, name: &str, version: &str, src: &str) -> Reply {
        self.publish_with(token, name, version, src, &[])
    }

    /// Publish with extra `X-Hard-*` headers.
    pub fn publish_with(
        &self,
        token: Option<&str>,
        name: &str,
        version: &str,
        src: &str,
        extra: &[(&str, &str)],
    ) -> Reply {
        let mut headers: Vec<(&str, &str)> = vec![
            ("Content-Type", "application/vnd.hardscript.package"),
            ("X-Hard-Package", name),
            ("X-Hard-Version", version),
        ];
        for (k, v) in extra {
            headers.push((k, v));
        }
        let auth = token.map(|t| format!("Bearer {t}"));
        if let Some(a) = auth.as_deref() {
            headers.push(("Authorization", a));
        }
        self.request("POST", "/api/publish", &archive(src), &headers)
    }

    /// Publish straight into the store (no HTTP), for read-only fixtures.
    pub fn seed(&self, name: &str, version: &str, src: &str) {
        self.app
            .publish(&PublishRequest::new(name, version, archive(src)))
            .unwrap_or_else(|e| panic!("seed {name}@{version}: {e}"));
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Split a raw HTTP response into status, headers and body.
pub fn parse_reply(raw: &[u8]) -> Reply {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = raw[(split + 4).min(raw.len())..].to_vec();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut headers = Vec::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Reply {
        status,
        headers,
        body,
    }
}

/// Read a value out of a JSON object as a string.
pub fn jstr(j: &hs_compiler::json::Json, key: &str) -> Option<String> {
    j.get(key).and_then(|v| v.as_str()).map(String::from)
}

/// A JSON array field as an owned vector (the shared reader returns a slice).
pub fn arr(j: &hs_compiler::json::Json, key: &str) -> Vec<hs_compiler::json::Json> {
    match j.get(key) {
        Some(hs_compiler::json::Json::Arr(items)) => items.clone(),
        _ => Vec::new(),
    }
}

/// Collect the `name` fields of a search result array.
pub fn result_names(rep: &Reply) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(hs_compiler::json::Json::Arr(items)) = rep.json().and_then(|j| j.get("results").cloned()) {
        for it in items {
            if let Some(n) = it.get("name").and_then(|v| v.as_str()) {
                out.push(n.to_string());
            }
        }
    }
    out
}

/// The integrity string a metadata document reports for one version.
pub fn integrity_of(rep: &Reply, version: &str) -> Option<String> {
    let j = rep.json()?;
    for v in arr(&j, "versions") {
        if jstr(&v, "version").as_deref() == Some(version) {
            return jstr(&v, "integrity");
        }
    }
    None
}

/// Remove a directory tree, ignoring errors (used by fixtures).
pub fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}
