//! A small, dependency-free HTTP/1.1 server.
//!
//! The registry needs request/response plumbing, not a framework: the whole
//! API is a dozen routes over a handful of verbs, the bodies are small
//! (an `.hspkg` archive, capped at 16 MiB) and every byte on the wire is
//! worth accounting for in the benchmarks. So this is a thread-per-connection
//! listener with a fixed worker pool, keep-alive, `Content-Length` bodies and
//! no chunked transfer support (clients always send a length).
//!
//! The same [`handle_request`] entry point serves both the real socket and
//! the in-process test harness, so tests exercise the real routing code
//! without a network.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Default cap on a request body (16 MiB).
pub const MAX_BODY: usize = 16 * 1024 * 1024;
/// Longest request line accepted.
pub const MAX_LINE: usize = 16 * 1024;
/// Longest header block accepted.
pub const MAX_HEADERS: usize = 64 * 1024;
/// Maximum header count.
pub const MAX_HEADER_COUNT: usize = 128;

/// A parsed request.
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub method: String,
    /// Path with the query string removed, still percent-encoded.
    ///
    /// Decoding happens per segment ([`Request::segments`]) so an encoded
    /// `/` inside a package name (`acme%2Fhttp`) stays part of the name
    /// instead of silently becoming a path separator.
    pub path: String,
    /// Raw (still encoded) query string.
    pub query: String,
    /// Decoded query parameters.
    pub params: BTreeMap<String, String>,
    /// Repeated query parameters, in order.
    pub multi: BTreeMap<String, Vec<String>>,
    /// Lower-cased header names.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    /// Peer address, when the request came off a socket.
    pub peer: Option<String>,
}

impl Request {
    /// A request built in-process (used by tests and by internal calls).
    pub fn new(method: &str, target: &str) -> Request {
        let mut r = Request {
            method: method.to_ascii_uppercase(),
            ..Request::default()
        };
        r.set_target(target);
        r
    }

    /// Parse `path?query` into the request's path/params fields.
    pub fn set_target(&mut self, target: &str) {
        match target.split_once('?') {
            Some((p, q)) => {
                self.path = p.to_string();
                self.query = q.to_string();
            }
            None => {
                self.path = target.to_string();
                self.query.clear();
            }
        }
        self.params.clear();
        self.multi.clear();
        for (k, v) in parse_query(&self.query) {
            self.multi.entry(k.clone()).or_default().push(v.clone());
            self.params.entry(k).or_insert(v);
        }
    }

    /// Path split into non-empty, percent-decoded segments.
    pub fn segments(&self) -> Vec<String> {
        self.path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| percent_decode(s).unwrap_or_else(|| s.to_string()))
            .collect()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(String::as_str)
    }

    /// Query parameter as a `usize`, clamped to `max`.
    pub fn param_usize(&self, name: &str, default: usize, max: usize) -> usize {
        self.params
            .get(name)
            .and_then(|v| v.parse::<usize>().ok())
            .map(|v| v.min(max))
            .unwrap_or(default)
    }

    /// Query parameter interpreted as a boolean flag.
    pub fn flag(&self, name: &str) -> bool {
        match self.params.get(name).map(String::as_str) {
            Some("1") | Some("true") | Some("yes") | Some("") => true,
            _ => false,
        }
    }

    pub fn with_body(mut self, body: Vec<u8>) -> Request {
        self.body = body;
        self
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Request {
        self.set_header(name, value);
        self
    }

    /// Set (or replace) a header, returning `&mut self` for chaining.
    pub fn set_header(&mut self, name: &str, value: &str) -> &mut Request {
        self.headers.insert(name.to_ascii_lowercase(), value.to_string());
        self
    }

    /// The bearer token from `Authorization: Bearer <token>`, if present.
    pub fn bearer(&self) -> Option<&str> {
        let raw = self.header("authorization")?;
        let rest = raw
            .strip_prefix("Bearer ")
            .or_else(|| raw.strip_prefix("bearer "))?;
        Some(rest.trim())
    }

    /// The bearer token, or `""` (a constant, so callers never branch on
    /// presence before comparing).
    pub fn bearer_default(&self) -> &str {
        self.bearer().unwrap_or("")
    }

    /// The body decoded as UTF-8 (lossy).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A response ready to be written.
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Default for Response {
    fn default() -> Self {
        Response {
            status: 200,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

impl Response {
    pub fn new(status: u16) -> Response {
        Response {
            status,
            ..Response::default()
        }
    }

    pub fn with_body(status: u16, body: Vec<u8>) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body,
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Response {
        let mut r = Response::with_body(status, body.into().into_bytes());
        r.set_header("Content-Type", "text/plain; charset=utf-8");
        r
    }

    /// A JSON response. Serialization is the registry's canonical, compact
    /// form (no whitespace) so responses are byte-stable.
    pub fn json(status: u16, value: &hs_compiler::json::Json) -> Response {
        let mut r = Response::with_body(status, value.to_string().into_bytes());
        r.set_header("Content-Type", "application/json");
        r
    }

    pub fn set_header(&mut self, name: &str, value: &str) -> &mut Response {
        self.headers
            .retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// An error body: `{"error":{"code":..,"message":..}}`.
    pub fn error(status: u16, code: &str, message: impl Into<String>) -> Response {
        let j = hs_compiler::json::Json::obj(vec![(
            "error",
            hs_compiler::json::Json::obj(vec![
                ("code", hs_compiler::json::Json::str(code)),
                ("message", hs_compiler::json::Json::str(message.into())),
            ]),
        )]);
        Response::json(status, &j)
    }

    pub fn text_body(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// The reason phrase for a status code.
pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Percent-decode a URL component (`+` is *not* treated as a space in paths).
pub fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = (bytes[i + 1] as char).to_digit(16)?;
                let lo = (bytes[i + 2] as char).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Percent-encode a URL component.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Parse `a=1&b=2` into decoded pairs.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in q.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => (part, ""),
        };
        let k = decode_form(k);
        let v = decode_form(v);
        out.push((k, v));
    }
    out
}

fn decode_form(s: &str) -> String {
    let replaced = s.replace('+', " ");
    percent_decode(&replaced).unwrap_or(replaced)
}

/// Counters the server keeps; reported by `GET /stats` and the benchmarks.
#[derive(Debug, Default)]
pub struct ServerMetrics {
    pub requests: AtomicU64,
    pub errors: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
}

impl ServerMetrics {
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }
    pub fn bytes_in(&self) -> u64 {
        self.bytes_in.load(Ordering::Relaxed)
    }
    pub fn bytes_out(&self) -> u64 {
        self.bytes_out.load(Ordering::Relaxed)
    }
    /// A snapshot suitable for JSON.
    pub fn snapshot(&self) -> Vec<(&'static str, u64)> {
        vec![
            ("requests", self.requests()),
            ("errors", self.errors()),
            ("bytes_in", self.bytes_in()),
            ("bytes_out", self.bytes_out()),
        ]
    }
}

/// Read one request from a buffered stream. Returns `Ok(None)` on a clean
/// EOF before any bytes (a keep-alive connection going away).
pub fn read_request(reader: &mut BufReader<TcpStream>) -> Result<Option<Request>, String> {
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .map_err(|e| format!("read request line: {e}"))?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_LINE {
        return Err("request line too long".to_string());
    }
    let mut parts = line.trim_end().split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return Err("missing method".to_string());
    }
    let mut req = Request::new(&method, &target);

    let mut total = 0usize;
    loop {
        let mut h = String::new();
        let n = reader
            .read_line(&mut h)
            .map_err(|e| format!("read header: {e}"))?;
        if n == 0 {
            break;
        }
        total += n;
        if total > MAX_HEADERS || req.headers.len() >= MAX_HEADER_COUNT {
            return Err("header block too large".to_string());
        }
        let t = h.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            req.headers
                .insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let len: usize = req
        .header("content-length")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err(format!("body of {len} bytes exceeds the {MAX_BODY} byte limit"));
    }
    if len > 0 {
        let mut body = vec![0u8; len];
        reader
            .read_exact(&mut body)
            .map_err(|e| format!("read body: {e}"))?;
        req.body = body;
    }
    Ok(Some(req))
}

/// Serialize a response onto a stream.
pub fn write_response(
    out: &mut impl Write,
    resp: &Response,
    keep_alive: bool,
    head_only: bool,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, reason(resp.status));
    let mut seen_len = false;
    for (k, v) in &resp.headers {
        if k.eq_ignore_ascii_case("content-length") {
            seen_len = true;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !seen_len {
        head.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    }
    head.push_str(if keep_alive {
        "Connection: keep-alive\r\n"
    } else {
        "Connection: close\r\n"
    });
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    if !head_only && !resp.body.is_empty() {
        out.write_all(&resp.body)?;
    }
    out.flush()
}

/// Anything that can answer a request.
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, req: &Request) -> Response;
}

impl<F> Handler for F
where
    F: Fn(&Request) -> Response + Send + Sync + 'static,
{
    fn handle(&self, req: &Request) -> Response {
        self(req)
    }
}

/// Handle a fully-parsed request: the single entry point shared by the
/// socket server and the in-process test harness.
pub fn dispatch(h: &dyn Handler, req: &Request, metrics: &ServerMetrics) -> Response {
    metrics.requests.fetch_add(1, Ordering::Relaxed);
    metrics
        .bytes_in
        .fetch_add(req.body.len() as u64, Ordering::Relaxed);
    let resp = h.handle(req);
    if resp.status >= 500 {
        metrics.errors.fetch_add(1, Ordering::Relaxed);
    }
    metrics
        .bytes_out
        .fetch_add(resp.body.len() as u64, Ordering::Relaxed);
    resp
}

/// A bound listener plus the thread pool serving it.
pub struct Server {
    listener: TcpListener,
    handler: Arc<dyn Handler>,
    metrics: Arc<ServerMetrics>,
    shutdown: Arc<AtomicBool>,
    /// How long a connection may stay idle before the server closes it.
    keep_alive_secs: u64,
}

impl Server {
    /// Bind `addr` and prepare to serve. The bound address is available via
    /// [`Server::addr`], which is what lets tests use port 0.
    pub fn bind(addr: &str, handler: Arc<dyn Handler>) -> std::io::Result<Server> {
        let listener = TcpListener::bind(addr)?;
        Ok(Server {
            listener,
            handler,
            metrics: Arc::new(ServerMetrics::default()),
            shutdown: Arc::new(AtomicBool::new(false)),
            keep_alive_secs: 30,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 0)))
    }

    /// The base URL clients should use.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr())
    }

    pub fn metrics(&self) -> Arc<ServerMetrics> {
        Arc::clone(&self.metrics)
    }

    /// A flag that makes [`Server::serve_forever`] return.
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }

    pub fn set_keep_alive_secs(&mut self, secs: u64) {
        self.keep_alive_secs = secs;
    }

    /// Accept connections until the shutdown flag is set. Each connection is
    /// served on its own thread; a connection may carry many keep-alive
    /// requests.
    pub fn serve_forever(&self) -> std::io::Result<()> {
        for stream in self.listener.incoming() {
            if self.shutdown.load(Ordering::Relaxed) {
                return Ok(());
            }
            match stream {
                Ok(s) => {
                    let handler = Arc::clone(&self.handler);
                    let metrics = Arc::clone(&self.metrics);
                    let keep_alive = self.keep_alive_secs;
                    std::thread::spawn(move || {
                        let _ = serve_connection(s, handler.as_ref(), &metrics, keep_alive);
                    });
                }
                Err(_) => continue,
            }
        }
        Ok(())
    }
}

fn serve_connection(
    stream: TcpStream,
    handler: &dyn Handler,
    metrics: &ServerMetrics,
    keep_alive_secs: u64,
) -> std::io::Result<()> {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let _ = stream.set_read_timeout(Some(Duration::from_secs(keep_alive_secs.max(1))));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(keep_alive_secs.max(1))));
    let _ = stream.set_nodelay(true);
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut req = match read_request(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()),
            Err(msg) => {
                let resp = Response::error(400, "bad_request", msg);
                let _ = write_response(&mut writer, &resp, false, false);
                return Ok(());
            }
        };
        req.peer = Some(peer.clone());
        let close = req
            .header("connection")
            .map(|v| v.eq_ignore_ascii_case("close"))
            .unwrap_or(false);
        let resp = dispatch(handler, &req, metrics);
        let head_only = req.method.eq_ignore_ascii_case("HEAD");
        write_response(&mut writer, &resp, !close, head_only)?;
        if close {
            return Ok(());
        }
    }
}

/// A running in-process server, for tests and benchmarks.
pub struct TestServer {
    pub base_url: String,
    pub addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    metrics: Arc<ServerMetrics>,
}

impl TestServer {
    /// Bind an ephemeral port and serve `handler` on a background thread.
    pub fn start(handler: Arc<dyn Handler>) -> TestServer {
        let server = Server::bind("127.0.0.1:0", handler).expect("bind 127.0.0.1:0");
        let addr = server.addr();
        let base_url = format!("http://{addr}");
        let shutdown = server.shutdown_flag();
        let metrics = server.metrics();
        // Unblock `accept` on shutdown by connecting to ourselves once the
        // flag is observed; the loop checks the flag per iteration.
        let handle = std::thread::spawn(move || {
            let _ = server.serve_forever();
        });
        TestServer {
            base_url,
            addr,
            shutdown,
            handle: Some(handle),
            metrics,
        }
    }

    pub fn metrics(&self) -> &Arc<ServerMetrics> {
        &self.metrics
    }

    /// Stop the listener. Safe to call more than once.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Nudge the accept loop so it notices the flag.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_roundtrip() {
        for s in ["hello", "a-b_c.d~e", "with space", "sl/ash", "üñî"] {
            let e = percent_encode(s);
            assert_eq!(percent_decode(&e).as_deref(), Some(s), "roundtrip {s} -> {e}");
        }
    }

    #[test]
    fn percent_decode_rejects_truncation() {
        assert!(percent_decode("%4").is_none());
        assert!(percent_decode("%zz").is_none());
        assert!(percent_decode("%ff").is_none());
    }

    #[test]
    fn query_parsing_handles_repeats_and_escapes() {
        let q = parse_query("q=hard%20script&tag=web&tag=api&flag");
        assert_eq!(q[0], ("q".to_string(), "hard script".to_string()));
        assert_eq!(q[1], ("tag".to_string(), "web".to_string()));
        assert_eq!(q[2], ("tag".to_string(), "api".to_string()));
        assert_eq!(q[3], ("flag".to_string(), String::new()));

        let r = Request::new("GET", "/search?q=ab&tag=x&tag=y");
        assert_eq!(r.param("q"), Some("ab"));
        assert_eq!(r.param("tag"), Some("x"));
        assert_eq!(r.multi.get("tag").map(Vec::len), Some(2));
        assert_eq!(r.segments(), vec!["search"]);
    }

    #[test]
    fn plus_is_a_space_in_queries() {
        let q = parse_query("q=a+b");
        assert_eq!(q[0].1, "a b");
    }

    #[test]
    fn bearer_extraction() {
        let r = Request::new("GET", "/x").with_header("Authorization", "Bearer hspat_abc");
        assert_eq!(r.bearer(), Some("hspat_abc"));
        let r = Request::new("GET", "/x").with_header("authorization", "bearer  z ");
        assert_eq!(r.bearer(), Some("z"));
        let r = Request::new("GET", "/x").with_header("Authorization", "Basic abc");
        assert_eq!(r.bearer(), None);
        assert_eq!(r.bearer_default(), "");
    }

    #[test]
    fn param_helpers_clamp() {
        let r = Request::new("GET", "/x?limit=5&limit=9&flag=1&nope=x");
        assert_eq!(r.param_usize("limit", 20, 10), 5);
        assert_eq!(r.param_usize("nope", 7, 10), 7);
        assert!(r.flag("flag"));
        assert!(!r.flag("nope"));
    }

    #[test]
    fn response_helpers() {
        let r = Response::text(404, "nope");
        assert_eq!(r.status, 404);
        assert_eq!(r.header("content-type"), Some("text/plain; charset=utf-8"));
        let e = Response::error(409, "conflict", "already there");
        let j = hs_compiler::json::parse(&e.text_body()).unwrap();
        assert_eq!(j.get("error").and_then(|x| x.get("code")).and_then(|c| c.as_str()), Some("conflict"));
    }

    #[test]
    fn set_header_replaces() {
        let mut r = Response::new(200);
        r.set_header("X-A", "1");
        r.set_header("x-a", "2");
        assert_eq!(r.headers.len(), 1);
        assert_eq!(r.header("X-A"), Some("2"));
    }

    #[test]
    fn segments_are_decoded_individually() {
        // An encoded separator stays inside one segment, so a scoped package
        // name is addressable while `a/b/c` is still three segments.
        let r = Request::new("GET", "/packages/acme%2Fhttp");
        assert_eq!(r.segments(), vec!["packages".to_string(), "acme/http".to_string()]);
        let r = Request::new("GET", "/packages/acme/http/1.0.0");
        assert_eq!(r.segments().len(), 4);
    }

    #[test]
    fn head_requests_send_headers_only() {
        let mut buf: Vec<u8> = Vec::new();
        let resp = Response::text(200, "hello");
        write_response(&mut buf, &resp, false, true).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK"));
        assert!(text.contains("Content-Length: 5"));
        assert!(text.ends_with("\r\n\r\n"), "body must be suppressed: {text:?}");
    }

    #[test]
    fn reason_phrases_cover_common_codes() {
        for c in [200u16, 201, 204, 301, 400, 401, 403, 404, 405, 409, 413, 500] {
            assert_ne!(reason(c), "Status", "missing reason for {c}");
        }
    }

    #[test]
    fn server_serves_over_tcp() {
        let ts = TestServer::start(Arc::new(|req: &Request| {
            Response::text(200, format!("hello {}", req.path))
        }));
        let body = hs_pm::registry::http_get(&format!("{}/ping", ts.base_url), 2000, 0)
            .unwrap();
        assert_eq!(body.status, 200);
        assert_eq!(String::from_utf8_lossy(&body.body), "hello /ping");
        assert!(ts.metrics().requests() >= 1);
    }
}
