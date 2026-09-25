//! Registry client: package metadata, downloads, search — over a minimal
//! dependency-free HTTP/1.1 client.
//!
//! There is no registry *server* in this milestone: only the *client* side.
//! Tests exercise it against a local mock registry (`http://127.0.0.1`), and
//! real deployments point it at `https://registry.hardscript.org` — HTTPS is
//! handled by delegating to `curl` when it is available, plain HTTP by a
//! direct [`std::net::TcpStream`]. The client supports timeouts, retries and
//! redirect following.

use crate::semver::Version;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Default registry used when the manifest does not pin one.
pub const DEFAULT_REGISTRY: &str = "https://registry.hardscript.org";

/// Number of redirects the client will follow before giving up.
pub const MAX_REDIRECTS: usize = 5;

/// A finished HTTP response.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Configuration for one registry client.
#[derive(Clone, Debug)]
pub struct RegistryConfig {
    pub url: String,
    pub timeout_ms: u64,
    pub retries: u32,
    pub offline: bool,
    pub verbose: bool,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        RegistryConfig {
            url: DEFAULT_REGISTRY.to_string(),
            timeout_ms: 30_000,
            retries: 2,
            offline: false,
            verbose: false,
        }
    }
}

impl RegistryConfig {
    /// Resolve the effective registry URL from manifest + environment.
    pub fn resolve(manifest_registry: Option<&str>, offline: bool) -> RegistryConfig {
        let url = manifest_registry
            .map(String::from)
            .or_else(|| std::env::var("HARD_REGISTRY").ok())
            .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());
        let timeout_ms = std::env::var("HARD_REGISTRY_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30_000);
        let retries = std::env::var("HARD_REGISTRY_RETRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2);
        RegistryConfig {
            url,
            timeout_ms,
            retries,
            offline,
            verbose: std::env::var("HARD_VERBOSE").is_ok(),
        }
    }

    /// A config pointing at a local registry (used by tests / local mirrors).
    pub fn local(base: &str) -> RegistryConfig {
        RegistryConfig {
            url: base.trim_end_matches('/').to_string(),
            ..RegistryConfig::default()
        }
    }
}

/// A published version inside package metadata.
#[derive(Clone, Debug)]
pub struct RegistryVersion {
    pub version: Version,
    pub dependencies: BTreeMap<String, String>,
    pub integrity: Option<String>,
    pub description: Option<String>,
}

/// Package metadata as served by the registry.
#[derive(Clone, Debug, Default)]
pub struct PackageMeta {
    pub name: String,
    pub versions: Vec<RegistryVersion>,
}

/// One search result.
#[derive(Clone, Debug, Default)]
pub struct SearchResult {
    pub name: String,
    pub version: Option<Version>,
    pub description: Option<String>,
}

/// Every registry error carries a human-readable message.
#[derive(Clone, Debug)]
pub struct RegistryError {
    pub message: String,
}

impl RegistryError {
    fn new(message: impl Into<String>) -> RegistryError {
        RegistryError {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

// ---------------------------------------------------------------------------
// HTTP transport
// ---------------------------------------------------------------------------

/// URL split into the pieces a raw HTTP/1.1 request needs.
#[derive(Clone, Debug)]
struct ParsedUrl {
    scheme: String,
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<ParsedUrl, String> {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s.to_lowercase(), r),
        None => return Err(format!("'{url}' needs a scheme (http or https)")),
    };
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "unsupported registry scheme '{scheme}' (use http or https)"
        ));
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
            (h, p.parse::<u16>().unwrap_or(80))
        }
        _ => (authority, if scheme == "https" { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(format!("'{url}' has an empty host"));
    }
    Ok(ParsedUrl {
        scheme,
        host: host.to_string(),
        port,
        path: if path.is_empty() { "/".to_string() } else { path.to_string() },
    })
}

/// Perform one HTTP GET, following redirects, with retries + timeouts.
pub fn http_get(
    url: &str,
    timeout_ms: u64,
    retries: u32,
) -> Result<HttpResponse, RegistryError> {
    http_get_with_depth(url, timeout_ms, retries, 0)
}

fn http_get_with_depth(
    url: &str,
    timeout_ms: u64,
    retries: u32,
    depth: usize,
) -> Result<HttpResponse, RegistryError> {
    if depth > MAX_REDIRECTS {
        return Err(RegistryError::new("too many redirects while fetching"));
    }
    let parsed = parse_url(url).map_err(RegistryError::new)?;
    if parsed.scheme == "https" {
        return http_get_https(&parsed, url, timeout_ms, retries);
    }
    let mut attempt = 0u32;
    loop {
        match raw_get(&parsed.host, parsed.port, &parsed.path, timeout_ms) {
            Ok((headers, body)) => {
                if (300..=399).contains(&headers_status(&headers)) {
                    if let Some(loc) = location_header(&headers) {
                        // Resolve relative redirects against the current URL.
                        let next = resolve_url(url, &loc);
                        return match http_get_with_depth(&next, timeout_ms, retries, depth + 1) {
                            Ok(r) => Ok(r),
                            Err(e) => Err(RegistryError::new(format!(
                                "redirect to '{loc}' failed: {e}"
                            ))),
                        };
                    }
                }
                return Ok(HttpResponse {
                    status: headers_status(&headers),
                    body,
                });
            }
            Err(e) => {
                attempt += 1;
                if attempt > retries {
                    return Err(RegistryError::new(format!(
                        "GET {}/{} failed after {attempt} attempt(s): {e}",
                        parsed.host, parsed.path
                    )));
                }
                std::thread::sleep(Duration::from_millis(200 * attempt as u64));
            }
        }
    }
}

fn headers_status(headers: &[(String, String)]) -> u16 {
    // The raw response carries headers plus a synthetic status fetch below.
    headers
        .iter()
        .find(|(k, _)| k == "__status")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(200)
}

fn http_get_https(
    parsed: &ParsedUrl,
    original: &str,
    timeout_ms: u64,
    retries: u32,
) -> Result<HttpResponse, RegistryError> {
    match curl_get(original, timeout_ms, retries) {
        Some(Ok(r)) => {
            let _ = parsed;
            Ok(r)
        }
        Some(Err(e)) => Err(e),
        None => Err(RegistryError::new(
            "HTTPS registry access requires 'curl' on PATH (or use an http:// registry)",
        )),
    }
}

fn curl_get(
    original: &str,
    timeout_ms: u64,
    retries: u32,
) -> Option<Result<HttpResponse, RegistryError>> {
    let mut temp = std::env::temp_dir();
    temp.push(format!(
        "hard-download-{}-{}.body",
        std::process::id(),
        instantaneous_nanos()
    ));
    for _attempt in 0..=retries {
        let out = Command::new("curl")
            .arg("-sS")
            .arg("-L")
            .arg("--max-redirs")
            .arg(MAX_REDIRECTS.to_string())
            .arg("--max-time")
            .arg((timeout_ms / 1000).max(1).to_string())
            .arg("--connect-timeout")
            .arg((timeout_ms / 1000).max(1).to_string())
            .arg("-o")
            .arg(&temp)
            .arg("-w")
            .arg("%{http_code}")
            .arg(original)
            .output();
        return match out {
            Ok(prog) => {
                let code = String::from_utf8_lossy(&prog.stdout)
                    .trim()
                    .parse::<u16>()
                    .unwrap_or(0);
                let body = std::fs::read(&temp).unwrap_or_default();
                let _ = std::fs::remove_file(&temp);
                Some(Ok(HttpResponse { status: code, body }))
            }
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                Some(Err(RegistryError::new(format!(
                    "could not invoke curl: {e} (install it or use an http:// registry)"
                ))))
            }
        };
    }
    let _ = std::fs::remove_file(&temp);
    None
}

fn instantaneous_nanos() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn resolve_url(base: &str, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    if let Some((scheme_host, _)) = base.split_once("/") {
        let _ = scheme_host;
    }
    let (origin, base_path) = match base.split_once("://") {
        Some((s, rest)) => {
            let slash = rest.find('/').unwrap_or(rest.len());
            (format!("{s}://{}", &rest[..slash]), &rest[slash..])
        }
        None => (String::new(), base),
    };
    if origin.is_empty() {
        return loc.to_string();
    }
    if loc.starts_with('/') {
        return format!("{origin}{loc}");
    }
    // relative to the base path's directory
    let mut segments: Vec<&str> = base_path.rsplitn(2, '/').collect();
    let dir = if segments.len() == 2 {
        segments.pop().unwrap();
        segments[0]
    } else {
        ""
    };
    format!("{origin}{dir}/{loc}")
}

/// A `(headers, body)` response from a raw request. The status is stitched
/// into the headers as a synthetic `__status` entry.
type RawResponse = (Vec<(String, String)>, Vec<u8>);

fn location_header(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("location"))
        .map(|(_, v)| v.clone())
}

fn raw_get(host: &str, port: u16, path: &str, timeout_ms: u64) -> Result<RawResponse, String> {
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {host}:{port}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(timeout_ms))
        .map_err(|e| format!("connect {host}:{port}: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(timeout_ms)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(timeout_ms)));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: hard-pm/0.4\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("send request: {e}"))?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read response: {e}"))?;
    parse_response(&buf)
}

fn parse_response(buf: &[u8]) -> Result<RawResponse, String> {
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "malformed HTTP response (no header terminator)".to_string())?;
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.split_whitespace();
    parts.next(); // HTTP/1.1
    let code = parts
        .next()
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| format!("bad status line: '{status_line}'"))?;
    let mut headers = Vec::new();
    let mut close = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
            if k.eq_ignore_ascii_case("connection") && v.trim().eq_ignore_ascii_case("close") {
                close = true;
            }
        }
    }
    headers.push(("__status".to_string(), code.to_string()));
    // The status is honored from the __status header so callers get one
    // unified `headers` type.
    let body_start = head_end + 4;
    let body = if let Some(cl) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
    {
        let available = buf.len() - body_start;
        if available < cl {
            return Err("response body shorter than Content-Length".to_string());
        }
        buf[body_start..body_start + cl].to_vec()
    } else if close {
        buf[body_start..].to_vec()
    } else {
        Vec::new()
    };
    Ok((headers, body))
}

// ---------------------------------------------------------------------------
// Registry API
// ---------------------------------------------------------------------------

/// The registry client.
#[derive(Clone, Debug)]
pub struct Registry {
    pub config: RegistryConfig,
}

impl Registry {
    pub fn new(config: RegistryConfig) -> Registry {
        Registry { config }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.url.trim_end_matches('/'), path)
    }

    pub fn get(&self, path: &str) -> Result<HttpResponse, RegistryError> {
        if self.config.offline {
            return Err(RegistryError::new(
                "registry is disabled (offline mode); use the package cache",
            ));
        }
        http_get(
            &self.url(path),
            self.config.timeout_ms,
            self.config.retries,
        )
    }

    fn parse_json(&self, resp: &HttpResponse, what: &str) -> Result<hs_compiler::json::Json, RegistryError> {
        if !(200..=299).contains(&resp.status) {
            return Err(RegistryError::new(format!(
                "{what}: registry returned HTTP {}",
                resp.status
            )));
        }
        let text = String::from_utf8_lossy(&resp.body);
        hs_compiler::json::parse(&text).ok_or_else(|| {
            RegistryError::new(format!(
                "{what}: registry returned unparseable JSON ({} bytes)",
                resp.body.len()
            ))
        })
    }

    /// Fetch metadata for one package.
    pub fn metadata(&self, name: &str) -> Result<PackageMeta, RegistryError> {
        let path = format!("/packages/{}", urlencode(name));
        let resp = self.get(&path)?;
        let j = self.parse_json(&resp, "metadata")?;
        let meta_name = j.get("name").and_then(|v| v.as_str()).unwrap_or(name).to_string();
        let mut versions = Vec::new();
        if let Some(arr) = j.get("versions").and_then(|v| v.as_arr()) {
            for v in arr {
                let version_str = v
                    .get("version")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| RegistryError::new("registry entry missing 'version'"))?;
                let version = Version::parse(version_str).map_err(|e| {
                    RegistryError::new(format!("registry served bad version '{version_str}': {e}"))
                })?;
                let mut deps = BTreeMap::new();
                if let Some(d) = v.get("dependencies").and_then(|x| x.as_arr()) {
                    for item in d {
                        let dn = item.get("name").and_then(|x| x.as_str());
                        let dr = item.get("req").and_then(|x| x.as_str());
                        if let (Some(dn), Some(dr)) = (dn, dr) {
                            deps.insert(dn.to_string(), dr.to_string());
                        }
                    }
                }
                versions.push(RegistryVersion {
                    version,
                    dependencies: deps,
                    integrity: v.get("integrity").and_then(|x| x.as_str()).map(String::from),
                    description: v.get("description").and_then(|x| x.as_str()).map(String::from),
                });
            }
        }
        if versions.is_empty() {
            return Err(RegistryError::new(format!(
                "registry has no versions for '{name}'"
            )));
        }
        Ok(PackageMeta {
            name: meta_name,
            versions,
        })
    }

    /// Download a package archive (`.hspkg` bytes).
    pub fn download(&self, name: &str, version: &Version, checksum: Option<&str>) -> Result<Vec<u8>, RegistryError> {
        let path = format!("/packages/{}/{}", urlencode(name), version);
        let resp = self.get(&path)?;
        if !(200..=299).contains(&resp.status) {
            return Err(RegistryError::new(format!(
                "download {name}@{version}: registry returned HTTP {}",
                resp.status
            )));
        }
        if let Some(want) = checksum {
            let got = hs_compiler::sha256::hex(&resp.body);
            let want_normalized = want.strip_prefix("sha256:").unwrap_or(want);
            if got != want_normalized {
                return Err(RegistryError::new(format!(
                    "download {name}@{version}: integrity mismatch (got {got}, want {want_normalized})"
                )));
            }
        }
        Ok(resp.body)
    }

    /// Search for packages by name/keyword.
    pub fn search(&self, query: &str) -> Result<Vec<SearchResult>, RegistryError> {
        let path = format!("/search?q={}", urlencode(query));
        let resp = self.get(&path)?;
        let j = self.parse_json(&resp, "search")?;
        let mut out = Vec::new();
        if let Some(arr) = j.get("results").and_then(|v| v.as_arr()) {
            for r in arr {
                let name = r.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let version = r
                    .get("version")
                    .and_then(|x| x.as_str())
                    .and_then(|s| Version::parse(s).ok());
                let description = r.get("description").and_then(|x| x.as_str()).map(String::from);
                out.push(SearchResult {
                    name,
                    version,
                    description,
                });
            }
        }
        Ok(out)
    }
}

/// Percent-encode a path segment (names are simple, so this is minimal).
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Best-effort platform string used in reports and the lockfile.
pub fn platform() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let mut s = format!("{os}-{arch}");
    if let Some(k) = read_first_line("/proc/sys/kernel/osrelease") {
        s.push(' ');
        s.push_str(k.trim());
    }
    s
}

fn read_first_line(path: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    content.lines().next().map(String::from)
}

/// Write a file atomically (temp + rename) to keep caches/locks consistent.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}