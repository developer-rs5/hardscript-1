//! REST routing.
//!
//! | route | method | auth | purpose |
//! |-------|--------|------|---------|
//! | `/` | GET | — | service index |
//! | `/health` | GET | — | liveness + backend |
//! | `/stats` | GET | — | counters |
//! | `/keys` | GET | — | signing public key |
//! | `/search` | GET | — | fuzzy search |
//! | `/packages/{name}` | GET | — | full metadata document |
//! | `/packages/{name}/versions` | GET | — | version summaries |
//! | `/packages/{name}/{version}` | GET | — | download the `.hspkg` |
//! | `/packages/{name}/{version}/manifest` | GET | — | this version's metadata |
//! | `/packages/{name}/{version}/signature` | GET | — | signature + public key |
//! | `/api/publish` | POST | `publish` | publish a version |
//! | `/api/yank` | POST | `yank` | retract a version |
//! | `/api/unyank` | POST | `yank` | restore a version |
//! | `/auth/register` | POST | — | create an account |
//! | `/auth/login` | POST | — | exchange a password for a session |
//! | `/auth/logout` | POST | — | revoke the presented token |
//! | `/auth/tokens` | GET/POST | `token` | list / create PATs |
//! | `/auth/tokens/{id}` | DELETE | `token` | revoke a PAT |
//! | `/auth/whoami` | GET | any | who the bearer token belongs to |
//! | `/api/mirror/changes` | GET | — | change feed for replication |
//! | `/api/mirror/manifest` | GET | — | full listing for a cold mirror |
//!
//! The router is the only place that knows about HTTP status codes; it
//! delegates all policy to [`App`].

use crate::app::App;
use crate::codec::{decode_login, decode_token_request, decode_yank, field_errors_json, DecodeError};
use crate::http::{Request, Response};
use crate::model::{normalize_user, now_secs, Scope};
use crate::search::{self, Query};
use crate::signing;
use crate::store::{has_scope, StoreError};
use crate::views;
use hs_compiler::json::Json;
use std::sync::Arc;

/// What a request maps to, once method and path shape are matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Index,
    Health,
    Stats,
    Keys,
    Search,
    Metadata,
    Versions,
    Download,
    VersionManifest,
    Signature,
    Publish,
    Yank(bool),
    Register,
    Login,
    Logout,
    ListTokens,
    CreateToken,
    RevokeToken,
    WhoAmI,
    Changes,
    MirrorManifest,
    Unknown,
}

/// The REST API over an [`App`].
#[derive(Clone)]
pub struct Router {
    pub app: Arc<App>,
}

impl Router {
    pub fn new(app: Arc<App>) -> Router {
        Router { app }
    }

    /// Wrap in the [`crate::http::Handler`] trait object.
    pub fn into_handler(self) -> Arc<dyn crate::http::Handler> {
        Arc::new(self)
    }

    /// Handle one request. Never panics on a bad request: every failure path
    /// becomes a JSON error body.
    pub fn handle_parsed(&self, req: &Request) -> Response {
        let method = req.method.to_ascii_uppercase();
        let seg = req.segments();
        let mut resp = self.route(&method, &seg, req);
        // Security headers every registry response carries.
        resp.set_header("X-Hard-Registry", crate::SERVICE_NAME);
        resp.set_header("X-Content-Type-Options", "nosniff");
        resp
    }

    fn route(&self, method: &str, seg: &[String], req: &Request) -> Response {
        // Route on the *decoded* segments, but only through `at`, so a
        // package called "health" can never be mistaken for a route.
        let at = |i: usize| seg.get(i).map(String::as_str);
        let route = match (method, seg.len()) {
            ("GET", 0) | ("HEAD", 0) => Route::Index,
            ("GET", 1) | ("HEAD", 1) => match at(0) {
                Some("health") => Route::Health,
                Some("stats") => Route::Stats,
                Some("keys") => Route::Keys,
                Some("search") => Route::Search,
                _ => Route::Unknown,
            },
            ("GET", 2) | ("HEAD", 2) => match at(0) {
                Some("packages") => Route::Metadata,
                Some("auth") => match at(1) {
                    Some("tokens") => Route::ListTokens,
                    Some("whoami") => Route::WhoAmI,
                    _ => Route::Unknown,
                },
                _ => Route::Unknown,
            },
            ("GET", 3) | ("HEAD", 3) => match (at(0), at(2)) {
                (Some("packages"), Some("versions")) => Route::Versions,
                (Some("api"), Some("changes")) => Route::Changes,
                (Some("api"), Some("manifest")) => Route::MirrorManifest,
                (Some("packages"), _) => Route::Download,
                _ => Route::Unknown,
            },
            ("GET", 4) | ("HEAD", 4) => match (at(0), at(3)) {
                (Some("packages"), Some("manifest")) => Route::VersionManifest,
                (Some("packages"), Some("signature")) => Route::Signature,
                (Some("packages"), _) => Route::Download,
                _ => Route::Unknown,
            },
            ("POST", 2) => match (at(0), at(1)) {
                (Some("api"), Some("publish")) => Route::Publish,
                (Some("api"), Some("yank")) => Route::Yank(true),
                (Some("api"), Some("unyank")) => Route::Yank(false),
                (Some("auth"), Some("register")) => Route::Register,
                (Some("auth"), Some("login")) => Route::Login,
                (Some("auth"), Some("logout")) => Route::Logout,
                (Some("auth"), Some("tokens")) => Route::CreateToken,
                _ => Route::Unknown,
            },
            ("DELETE", 3) => match (at(0), at(1)) {
                (Some("auth"), Some("tokens")) => Route::RevokeToken,
                _ => Route::Unknown,
            },
            _ => Route::Unknown,
        };
        match route {
            Route::Index => self.index(),
            Route::Health => self.health(),
            Route::Stats => self.stats(),
            Route::Keys => self.keys(),
            Route::Search => self.search(req),
            Route::Metadata => self.metadata(at(1).unwrap_or(""), req),
            Route::Versions => self.versions(at(1).unwrap_or("")),
            Route::Download => self.download(at(1).unwrap_or(""), at(2).unwrap_or(""), req),
            Route::VersionManifest => {
                self.version_manifest(at(1).unwrap_or(""), at(2).unwrap_or(""))
            }
            Route::Signature => self.signature(at(1).unwrap_or(""), at(2).unwrap_or("")),
            Route::Publish => self.publish(req),
            Route::Yank(default) => self.yank(req, default),
            Route::Register => self.register(req),
            Route::Login => self.login(req),
            Route::Logout => self.logout(req),
            Route::ListTokens => self.list_tokens(req),
            Route::CreateToken => self.create_token(req),
            Route::RevokeToken => self.revoke_token(req, at(2).unwrap_or("")),
            Route::WhoAmI => self.whoami(req),
            Route::Changes => self.mirror_changes(req),
            Route::MirrorManifest => self.mirror_manifest(req),
            Route::Unknown => self.unknown(method, seg),
        }
    }

    /// No route matched: distinguish "wrong verb" from "no such resource",
    /// but only when the path shape itself is known.
    fn unknown(&self, method: &str, seg: &[String]) -> Response {
        let known = matches!(
            seg.first().map(String::as_str),
            Some("packages") | Some("api") | Some("auth") | Some("search") | Some("health")
                | Some("stats") | Some("keys")
        );
        if known {
            return method_not_allowed(method, seg);
        }
        Response::error(
            404,
            "not_found",
            format!("no route for {method} /{}", seg.join("/")),
        )
    }

    // -- service endpoints -------------------------------------------------

    fn index(&self) -> Response {
        let stats = self.app.stats().unwrap_or_default();
        let j = views::index_json(&self.app.config.service, App::version(), self.app.store.backend(), &stats);
        Response::json(200, &j)
    }

    fn health(&self) -> Response {
        let stats = self.app.store.stats();
        let ok = stats.is_ok();
        // A mirror client needs to know what it is talking to before it syncs
        // anything: whether the registry holds anything yet, which change
        // sequence it is on, and whether it signs with the reproducible test
        // key that anybody can regenerate.
        let (packages, seq) = match &stats {
            Ok(st) => (st.packages as i64, self.app.seq().unwrap_or(0)),
            Err(_) => (0, 0),
        };
        let j = Json::obj(vec![
            ("status", Json::str(if ok { "ok" } else { "degraded" })),
            ("backend", Json::str(self.app.store.backend())),
            ("version", Json::str(App::version())),
            ("at", Json::num(now_secs())),
            ("packages", Json::num(packages)),
            ("seq", Json::num(seq)),
            ("key_id", Json::str(self.app.key.key_id())),
            ("test_key", Json::Bool(self.app.key.is_test_key())),
        ]);
        Response::json(if ok { 200 } else { 503 }, &j)
    }

    fn stats(&self) -> Response {
        match (self.app.stats(), self.app.seq()) {
            (Ok(st), Ok(seq)) => Response::json(200, &views::stats_json(&st, self.app.store.backend(), seq)),
            (Err(e), _) | (_, Err(e)) => store_error(&e),
        }
    }

    fn keys(&self) -> Response {
        let key = &self.app.key;
        let j = Json::obj(vec![
            ("algorithm", Json::str("ed25519")),
            ("key_id", Json::str(key.key_id())),
            ("public_key", Json::str(key.public_hex())),
            ("public_key_base64", Json::str(key.public_base64())),
            ("payload_format", Json::str(signing::SIGNATURE_VERSION)),
            ("test_key", Json::Bool(key.is_test_key())),
        ]);
        Response::json(200, &j)
    }

    // -- reads -------------------------------------------------------------

    fn search(&self, req: &Request) -> Response {
        let text = req.param("q").unwrap_or("");
        let limit = match count_param(req.param("limit"), 20, self.app.config.max_search_results) {
            Ok(v) => v,
            Err(e) => return Response::error(400, "invalid_request", e),
        };
        let offset = match count_param(req.param("offset"), 0, MAX_SEARCH_OFFSET) {
            Ok(v) => v,
            Err(e) => return Response::error(400, "invalid_request", e),
        };
        let tags: Vec<String> = req.multi.get("tag").cloned().unwrap_or_default();
        // `?tag=` is a filter that cannot mean anything. Silently dropping it
        // would turn the request into "match everything".
        if tags.iter().any(|t| t.trim().is_empty()) {
            return Response::error(400, "invalid_request", "a tag filter cannot be empty");
        }
        let q = Query::parse(text, &tags, req.param("prefix"));
        if text.trim().is_empty() && tags.is_empty() && q.prefix.is_none() {
            return Response::error(400, "invalid_request", "search needs ?q=, ?tag= or ?prefix=");
        }
        let ranked = search::run(self.app.store.as_ref(), &q);
        let total = ranked.len();
        let page: Vec<views::SearchHit> = ranked.into_iter().skip(offset).take(limit).collect();
        Response::json(200, &views::search_json(&page, text, total))
    }

    fn metadata(&self, name: &str, req: &Request) -> Response {
        match self.app.package_document(name) {
            Ok(Some((p, versions))) => Response::json(200, &views::package_json(&p, &versions)),
            Ok(None) => not_found_package(name),
            Err(e) => store_error(&e),
        }
        .tap_cache(req)
    }

    fn versions(&self, name: &str) -> Response {
        match self.app.store.package(name) {
            Ok(Some(_)) => match self.app.store.versions(name) {
                Ok(vs) => {
                    let j = Json::obj(vec![
                        ("name", Json::str(name)),
                        ("count", Json::num(vs.len() as i64)),
                        (
                            "versions",
                            Json::arr(vs.iter().map(views::version_summary_json).collect()),
                        ),
                    ]);
                    Response::json(200, &j)
                }
                Err(e) => store_error(&e),
            },
            Ok(None) => not_found_package(name),
            Err(e) => store_error(&e),
        }
    }

    fn version_manifest(&self, name: &str, version: &str) -> Response {
        match self.app.store.version(name, version) {
            Ok(Some(v)) => {
                let j = Json::obj(vec![
                    ("name", Json::str(&v.name)),
                    ("version", Json::str(v.version.to_string())),
                    ("dependencies", Json::arr(v.deps.iter().map(views::dep_json).collect())),
                    ("fingerprint", Json::str(&v.fingerprint)),
                    ("integrity", Json::str(&v.integrity)),
                    ("files", Json::arr(v.files.iter().map(Json::str).collect())),
                    ("yanked", Json::Bool(v.yanked)),
                ]);
                Response::json(200, &j)
            }
            Ok(None) => not_found_version(name, version),
            Err(e) => store_error(&e),
        }
    }

    fn signature(&self, name: &str, version: &str) -> Response {
        match self.app.store.version(name, version) {
            Ok(Some(v)) => {
                let payload = self.app.key.payload(
                    &v.name,
                    &v.version.to_string(),
                    &v.integrity,
                    &v.fingerprint,
                );
                let j = Json::obj(vec![
                    ("name", Json::str(&v.name)),
                    ("version", Json::str(v.version.to_string())),
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
                    ("algorithm", Json::str("ed25519")),
                    ("public_key", Json::str(self.app.key.public_hex())),
                    // The client needs the two digests to re-derive the
                    // payload itself rather than trusting this server's copy.
                    ("integrity", Json::str(&v.integrity)),
                    ("fingerprint", Json::str(&v.fingerprint)),
                    ("payload", Json::str(&payload)),
                    (
                        "verified",
                        Json::Bool(match &v.signature {
                            Some(sig) => self.app.key.verify(&payload, sig),
                            None => false,
                        }),
                    ),
                ]);
                Response::json(200, &j)
            }
            Ok(None) => not_found_version(name, version),
            Err(e) => store_error(&e),
        }
    }

    fn download(&self, name: &str, version: &str, req: &Request) -> Response {
        let v = match self.app.store.version(name, version) {
            Ok(Some(v)) => v,
            Ok(None) => return not_found_version(name, version),
            Err(e) => return store_error(&e),
        };
        if v.yanked && !self.app.config.serve_yanked {
            return Response::error(410, "yanked", format!("{name}@{version} has been yanked"));
        }
        // Ranged requests answer 206 with a slice and do not count as a
        // download: a resumed transfer must not inflate the counters.
        if let Some((start, end)) = req.header("range").and_then(parse_range) {
            if start >= v.size {
                return Response::error(416, "range_not_satisfiable", "range starts past the end");
            }
            // An open-ended range runs to the last byte.
            let last = end.unwrap_or(v.size.saturating_sub(1)).min(v.size.saturating_sub(1));
            // HTTP byte ranges are inclusive on both ends; the archive store
            // takes an exclusive end, hence the +1.
            return match self.app.download_range(name, version, start, last + 1) {
                Ok(bytes) => {
                    let mut r = Response::with_body(206, bytes);
                    r.set_header("Content-Type", "application/vnd.hardscript.package");
                    r.set_header("Content-Range", &format!("bytes {start}-{last}/{}", v.size));
                    r.set_header("X-Hard-Integrity", &v.integrity);
                    r
                }
                Err(e) => store_error(&e),
            };
        }
        // HEAD asks "does this exist, and how big is it" without counting a
        // download: the client uses it as a pre-publish conflict check.
        let count = !req.method.eq_ignore_ascii_case("HEAD");
        match self.app.download_counted(name, version, count) {
            Ok(bytes) => {
                let mut r = Response::with_body(200, bytes);
                r.set_header("Content-Type", "application/vnd.hardscript.package");
                r.set_header("X-Hard-Integrity", &v.integrity);
                r.set_header("X-Hard-Fingerprint", &v.fingerprint);
                r.set_header(
                    "X-Hard-Signature",
                    v.signature.as_deref().unwrap_or(""),
                );
                r.set_header("X-Hard-Key-Id", v.key_id.as_deref().unwrap_or(""));
                r.set_header("ETag", &format!("\"{}\"", short_digest(&v.integrity)));
                r
            }
            Err(e) => store_error(&e),
        }
    }

    // -- writes ------------------------------------------------------------

    fn publish(&self, req: &Request) -> Response {
        let token = match self.publish_token(req) {
            Ok(t) => t,
            Err(r) => return r,
        };
        let who = crate::app::Publisher::from_token(token.as_ref());
        let parsed = match crate::publish::parse_publish_request(req) {
            Ok(p) => p,
            Err(e) => return decode_error(&e),
        };
        if let Err(errs) = self.app.validate_publish(&parsed) {
            return Response::json(422, &crate::codec::validation_json(&errs));
        }
        // A structurally broken archive is a problem with the *package*, not
        // with the request, so it is reported like any other validation
        // failure (422) with the reason attached to the field.
        if let Err(e) = hs_pm::pkgfmt::read_archive(&parsed.archive) {
            return Response::json(
                422,
                &crate::codec::validation_json(&[crate::model::ValidationError::new(
                    "archive",
                    format!("not a valid .hspkg archive: {}", e.message),
                )]),
            );
        }
        if is_dry_run(req) {
            return match self.app.preview(&parsed, &who) {
                Ok(p) => Response::json(200, &preview_json(&p)),
                Err(e) => store_error(&e),
            };
        }
        match self.app.publish_as(&parsed, &who) {
            Ok(v) => {
                let mut j = views::publish_json(&v);
                if let Json::Obj(pairs) = &mut j {
                    pairs.push((
                        "published_by".to_string(),
                        Json::str(who.name().unwrap_or("anonymous")),
                    ));
                }
                Response::json(201, &j)
            }
            Err(e) => store_error(&e),
        }
    }

    /// `POST /api/yank` and `POST /api/unyank`. The route decides the default;
    /// an explicit `yanked` field in a JSON body always wins.
    fn yank(&self, req: &Request, route_default: bool) -> Response {
        if let Err(r) = self.authorize(req, Scope::Yank) {
            return r;
        }
        let (name, version, want) = match self.json_body(req) {
            Some(body) => match decode_yank(&body) {
                Ok((n, v, flag)) => (n, v, flag),
                Err(e) => return decode_error(&e),
            },
            None => (
                req.param("name").unwrap_or("").to_string(),
                req.param("version").unwrap_or("").to_string(),
                route_default,
            ),
        };
        match self.app.set_yanked(&name, &version, want) {
            Ok(v) => Response::json(200, &views::yank_json(&v.name, &v.version.to_string(), v.yanked)),
            Err(e) => store_error(&e),
        }
    }

    // -- accounts ----------------------------------------------------------

    fn register(&self, req: &Request) -> Response {
        let Some(body) = self.json_body(req) else {
            return Response::error(400, "invalid_request", "a JSON body is required");
        };
        let user = body.get("user").or_else(|| body.get("name"));
        let Some(user) = user.and_then(|v| v.as_str()) else {
            return Response::error(400, "invalid_request", "missing field 'user'");
        };
        let Some(password) = body.get("password").and_then(|v| v.as_str()) else {
            return Response::error(400, "invalid_request", "missing field 'password'");
        };
        let email = body.get("email").and_then(|v| v.as_str());
        match self.app.register(user, password, email) {
            Ok(u) => {
                let mut j = views::user_json(&u);
                if let Json::Obj(pairs) = &mut j {
                    pairs.push((
                        "password_strength".to_string(),
                        Json::str(crate::auth::password_strength(password).as_str()),
                    ));
                }
                Response::json(201, &j)
            }
            Err(e) => store_error(&e),
        }
    }

    fn login(&self, req: &Request) -> Response {
        let Some(body) = self.json_body(req) else {
            return Response::error(400, "invalid_request", "a JSON body is required");
        };
        let (user, password) = match decode_login(&body) {
            Ok(v) => v,
            Err(e) => return decode_error(&e),
        };
        match self.app.login(&user, &password) {
            Ok((u, session)) => {
                let j = views::token_json(
                    &session.id,
                    &u.name,
                    "session",
                    &crate::model::Scope::all(),
                    Some(&session.plaintext),
                    now_secs(),
                );
                let mut resp = Response::json(200, &j);
                resp.set_header("X-Hard-Session-Id", &session.id);
                resp
            }
            Err(e) => store_error(&e),
        }
    }

    fn logout(&self, req: &Request) -> Response {
        // Logout needs no scope: presenting a valid token is the proof.
        let token = match self.token(req) {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Response::error(401, "unauthorized", "a bearer token is required to log out")
            }
            Err(r) => return r,
        };
        match self.app.logout(&token.id) {
            Ok(_) => Response::json(200, &Json::obj(vec![("ok", Json::Bool(true)), ("revoked", Json::str(&token.id))])),
            Err(e) => store_error(&e),
        }
    }

    fn list_tokens(&self, req: &Request) -> Response {
        let token = match self.token_for(req, Scope::Token) {
            Ok(t) => t,
            Err(r) => return r,
        };
        match self.app.store.tokens(&token.user) {
            Ok(list) => {
                let j = Json::obj(vec![
                    ("user", Json::str(&token.user)),
                    ("count", Json::num(list.len() as i64)),
                    ("tokens", Json::arr(list.iter().map(views::token_view).collect())),
                ]);
                Response::json(200, &j)
            }
            Err(e) => store_error(&e),
        }
    }

    fn create_token(&self, req: &Request) -> Response {
        let owner_user = match self.token_for(req, Scope::Token) {
            Ok(t) => t.user,
            Err(r) => return r,
        };
        let (label, scopes) = match self.json_body(req) {
            Some(body) => match decode_token_request(&body) {
                Ok(v) => v,
                Err(e) => return decode_error(&e),
            },
            None => (req.param("name").unwrap_or("default").to_string(), None),
        };
        match self.app.create_token(&owner_user, &label, scopes) {
            Ok((stored, plaintext)) => {
                let j = views::token_json(
                    &stored.id,
                    &stored.user,
                    &stored.name,
                    &stored.scopes,
                    Some(&plaintext),
                    stored.created_at,
                );
                Response::json(201, &j)
            }
            Err(e) => store_error(&e),
        }
    }

    fn revoke_token(&self, req: &Request, id: &str) -> Response {
        let owner = match self.token_for(req, Scope::Token) {
            Ok(t) => t.user,
            Err(r) => return r,
        };
        match self.app.store.revoke_token(&owner, id) {
            Ok(t) => Response::json(200, &views::token_view(&t)),
            Err(e) => store_error(&e),
        }
    }

    fn whoami(&self, req: &Request) -> Response {
        match self.token(req) {
            Ok(Some(t)) => {
                let j = Json::obj(vec![
                    ("user", Json::str(&t.user)),
                    ("token_id", Json::str(&t.id)),
                    ("name", Json::str(&t.name)),
                    (
                        "scopes",
                        Json::arr(Scope::render(&t.scopes).into_iter().map(Json::str).collect()),
                    ),
                    (
                        "created_at",
                        match t.created_at {
                            v => Json::num(v),
                        },
                    ),
                    (
                        "last_used_at",
                        match t.last_used_at {
                            Some(v) => Json::num(v),
                            None => Json::Null,
                        },
                    ),
                ]);
                Response::json(200, &j)
            }
            Ok(None) => Response::error(401, "unauthorized", "no bearer token presented"),
            Err(r) => r,
        }
    }

    // -- mirror feeds ------------------------------------------------------

    fn mirror_changes(&self, req: &Request) -> Response {
        let since = req
            .param("since")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        let limit = req.param_usize("limit", 100, self.app.config.max_change_batch);
        match (self.app.changes(since, limit), self.app.seq()) {
            (Ok(changes), Ok(seq)) => {
                let j = Json::obj(vec![
                    ("since", Json::num(since)),
                    ("seq", Json::num(seq)),
                    ("count", Json::num(changes.len() as i64)),
                    (
                        "changes",
                        Json::arr(changes.iter().map(views::change_json).collect()),
                    ),
                ]);
                Response::json(200, &j)
            }
            (Err(e), _) | (_, Err(e)) => store_error(&e),
        }
    }

    fn mirror_manifest(&self, req: &Request) -> Response {
        let prefix = req.param("prefix");
        match self.app.store.packages(prefix) {
            Ok(pkgs) => {
                let mut versions = Vec::new();
                for p in &pkgs {
                    if let Ok(vs) = self.app.store.versions(&p.name) {
                        for v in vs {
                            versions.push(views::version_json(&v));
                        }
                    }
                }
                let j = Json::obj(vec![
                    ("count", Json::num(versions.len() as i64)),
                    ("seq", Json::num(self.app.seq().unwrap_or(0))),
                    ("versions", Json::arr(versions)),
                ]);
                Response::json(200, &j)
            }
            Err(e) => store_error(&e),
        }
    }

    // -- helpers -----------------------------------------------------------

    /// The parsed JSON body, or `None` when the request has none.
    fn json_body(&self, req: &Request) -> Option<Json> {
        if req.body.is_empty() {
            return None;
        }
        hs_compiler::json::parse(&req.text())
    }

    /// Authenticate without checking a scope. `Ok(None)` when no token was
    /// presented at all (the caller decides whether that is an error).
    fn token(&self, req: &Request) -> Result<Option<crate::model::Token>, Response> {
        let bearer = req.bearer_default();
        if bearer.is_empty() {
            return Ok(None);
        }
        match self.app.authenticate(bearer) {
            Ok(None) => Err(Response::error(
                401,
                "unauthorized",
                "the presented token is not valid (or was revoked)",
            )),
            Ok(Some(t)) => Ok(Some(t)),
            Err(e) => Err(store_error(&e)),
        }
    }

    /// Who is publishing.
    ///
    /// On a normal registry this is a token holding `publish`. On an
    /// open-publishing node (local development, sandboxes) nobody has to
    /// present one — but a token that *is* presented must still be valid, and
    /// a scoped registry still checks the scope.
    fn publish_token(&self, req: &Request) -> Result<Option<crate::model::Token>, Response> {
        let open = self.app.config.open_publish && !self.app.config.require_auth;
        match self.token(req)? {
            Some(t) => {
                if !open && !has_scope(&t.scopes, Scope::Publish) {
                    return Err(Response::error(
                        403,
                        "forbidden",
                        format!("token {} lacks the 'publish' scope", t.id),
                    ));
                }
                Ok(Some(t))
            }
            None if open => Ok(None),
            None => Err(Response::error(
                401,
                "unauthorized",
                "this endpoint needs a token with the 'publish' scope (Authorization: Bearer <token>)",
            )),
        }
    }

    /// Require a token holding `scope`, returning the token record.
    ///
    /// Unlike [`Router::publish_token`], this never honours open mode:
    /// managing tokens is always an authenticated act, even on a registry
    /// that lets anybody publish.
    fn token_for(&self, req: &Request, scope: Scope) -> Result<crate::model::Token, Response> {
        let token = match self.token(req)? {
            Some(t) => t,
            None => {
                return Err(Response::error(
                    401,
                    "unauthorized",
                    format!(
                        "this endpoint needs a token with the '{}' scope (Authorization: Bearer <token>)",
                        scope.as_str()
                    ),
                ))
            }
        };
        if !has_scope(&token.scopes, scope) {
            return Err(Response::error(
                403,
                "forbidden",
                format!("token {} lacks the '{}' scope", token.id, scope.as_str()),
            ));
        }
        Ok(token)
    }

    /// Require a token holding `scope`, returning the token's user name.
    fn authorize(&self, req: &Request, scope: Scope) -> Result<String, Response> {
        if !self.app.config.require_auth && self.app.config.open_publish {
            return Ok(self.app.anonymous_owner.clone());
        }
        let token = match self.token(req)? {
            Some(t) => t,
            None => {
                return Err(Response::error(
                    401,
                    "unauthorized",
                    format!(
                        "this endpoint needs a token with the '{}' scope (Authorization: Bearer <token>)",
                        scope.as_str()
                    ),
                ))
            }
        };
        if !has_scope(&token.scopes, scope) {
            return Err(Response::error(
                403,
                "forbidden",
                format!(
                    "token {} lacks the '{}' scope",
                    token.id,
                    scope.as_str()
                ),
            ));
        }
        Ok(token.user)
    }
}

impl crate::http::Handler for Router {
    fn handle(&self, req: &Request) -> Response {
        self.handle_parsed(req)
    }
}

/// Small helper so `metadata` can set an ETag in one expression.
trait TapCache {
    fn tap_cache(self, req: &Request) -> Self;
}

impl TapCache for Response {
    fn tap_cache(self, req: &Request) -> Self {
        if self.status == 200 && req.header("if-none-match").is_some() {
            let mut r = self;
            r.set_header("Cache-Control", "public, max-age=60");
            r
        } else if self.status == 200 {
            let mut r = self;
            r.set_header("Cache-Control", "public, max-age=60");
            r
        } else {
            self
        }
    }
}

/// Did the client ask for a dry run? (header or JSON body flag)
fn is_dry_run(req: &Request) -> bool {
    if req
        .header("x-hard-dry-run")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
    {
        return true;
    }
    req.json_body_flag("dry_run")
}

fn decode_error(e: &DecodeError) -> Response {
    match e {
        DecodeError::Fields(errs) => Response::json(400, &field_errors_json(errs)),
        DecodeError::Json(msg) => Response::error(400, "invalid_json", msg),
    }
}

fn store_error(e: &StoreError) -> Response {
    Response::error(e.status(), e.code(), e.to_string())
}

/// The largest `offset` a client may ask for. Pagination is for skipping a
/// few pages, not for walking the whole index one row at a time.
const MAX_SEARCH_OFFSET: usize = 10_000;

/// A count-valued query parameter: absent or blank means `default`, anything
/// unparseable (or negative) is a client error rather than a silent default.
fn count_param(raw: Option<&str>, default: usize, max: usize) -> Result<usize, String> {
    match raw {
        None => Ok(default),
        Some(s) if s.trim().is_empty() => Ok(default),
        Some(s) => s
            .parse::<usize>()
            .map(|v| v.min(max))
            .map_err(|_| format!("'{s}' is not a valid count")),
    }
}

fn not_found_package(name: &str) -> Response {
    Response::error(404, "not_found", format!("no package named '{name}'"))
}

fn not_found_version(name: &str, version: &str) -> Response {
    Response::error(404, "not_found", format!("no version {name}@{version}"))
}

fn method_not_allowed(method: &str, seg: &[String]) -> Response {
    Response::error(
        405,
        "method_not_allowed",
        format!("{method} is not allowed on /{}", seg.join("/")),
    )
}

/// Parse a `Range` header into `(start, end)`.
///
/// Both `bytes=N-M` and the open-ended `bytes=N-` are accepted — the second
/// is what a resumed download sends, so refusing it would silently turn every
/// resume into a full restart. The suffix form (`bytes=-N`) is rejected: the
/// package manager always knows its start offset.
pub fn parse_range(raw: &str) -> Option<(u64, Option<u64>)> {
    let spec = raw.trim().strip_prefix("bytes=")?.trim();
    let (a, b) = spec.split_once('-')?;
    let start: u64 = a.trim().parse().ok()?;
    let end = match b.trim() {
        "" => None,
        text => Some(text.parse::<u64>().ok()?),
    };
    if let Some(e) = end {
        if e < start {
            return None;
        }
    }
    Some((start, end))
}

/// The preview document a dry run returns.
fn preview_json(p: &crate::app::PublishPreview) -> Json {
    Json::obj(vec![
        ("ok", Json::Bool(true)),
        ("dry_run", Json::Bool(true)),
        ("name", Json::str(&p.name)),
        ("version", Json::str(&p.version)),
        ("integrity", Json::str(&p.integrity)),
        ("fingerprint", Json::str(&p.fingerprint)),
        ("size", Json::num(p.size as i64)),
        ("file_count", Json::num(p.file_count as i64)),
        (
            "files",
            Json::arr(p.files.iter().map(Json::str).collect()),
        ),
        (
            "dependencies",
            Json::arr(
                p.dependencies
                    .iter()
                    .map(|(n, r, k)| {
                        Json::obj(vec![
                            ("name", Json::str(n)),
                            ("req", Json::str(r)),
                            ("kind", Json::str(k)),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("already_published", Json::Bool(p.already_published)),
        ("conflicting_version", Json::Bool(p.conflicting_version)),
        (
            "owner",
            match &p.owner {
                Some(o) => Json::str(o),
                None => Json::Null,
            },
        ),
    ])
}

/// The first 16 hex characters of a digest, for ETags.
pub fn short_digest(integrity: &str) -> &str {
    let h = integrity.strip_prefix("sha256:").unwrap_or(integrity);
    &h[..h.len().min(16)]
}

/// Normalize a user name the way the account endpoints do.
pub fn normalized_user(name: &str) -> Result<String, String> {
    normalize_user(name).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Config, PublishRequest};
    use crate::archives::ArchiveStore;
    use crate::http::TestServer;
    use crate::publish::parse_publish_request;
    use crate::signing::SigningKey;
    use crate::sqlite::SqliteStore;
    use hs_pm::registry::http_get;
    use std::path::PathBuf;
    use std::sync::Arc;

    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("hs-router-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    pub(crate) fn archive() -> Vec<u8> {
        hs_pm::pkgfmt::pack(&[hs_pm::pkgfmt::FileRecord {
            rel_path: "main.hard".to_string(),
            data: b"calc x() => Int { <- 1 }\n".to_vec(),
        }])
        .unwrap()
    }

    fn build(tag: &str, config: Config) -> (Arc<App>, PathBuf) {
        let dir = temp_dir(tag);
        let store = SqliteStore::open(dir.join("registry.db")).unwrap();
        let app = App::new(
            Arc::new(store),
            ArchiveStore::at(dir.join("archives")),
            SigningKey::deterministic_for_tests(),
            config,
        );
        (Arc::new(app), dir)
    }

    fn app_permissive(tag: &str) -> (Arc<App>, PathBuf) {
        build(tag, Config::permissive())
    }

    fn app_secure(tag: &str) -> (Arc<App>, PathBuf) {
        build(tag, Config::default())
    }

    /// Start a real listener in front of `app`.
    fn start(app: Arc<App>) -> TestServer {
        TestServer::start(Router::new(app).into_handler())
    }

    fn get(server: &TestServer, path: &str) -> (u16, String) {
        let (code, bytes) = get_bytes(server, path);
        (code, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// GET that keeps the body as raw bytes (package archives are binary).
    fn get_bytes(server: &TestServer, path: &str) -> (u16, Vec<u8>) {
        let r = http_get(&format!("{}{}", server.base_url, path), 4000, 0).unwrap();
        (r.status, r.body)
    }

    fn call(app: &Arc<App>, req: Request) -> Response {
        Router::new(Arc::clone(app)).handle_parsed(&req)
    }

    #[test]
    fn index_health_stats_and_keys() {
        let (app, dir) = app_permissive("index");
        let s = start(app);
        let (code, body) = get(&s, "/");
        assert_eq!(code, 200);
        assert!(body.contains("hardscript-registry"));
        assert!(body.contains("\"backend\":\"sqlite\""));

        let (code, body) = get(&s, "/health");
        assert_eq!(code, 200);
        assert!(body.contains("\"status\":\"ok\""));

        let (code, body) = get(&s, "/stats");
        assert_eq!(code, 200);
        assert!(body.contains("\"packages\":0"));

        let (code, body) = get(&s, "/keys");
        assert_eq!(code, 200);
        assert!(body.contains("\"algorithm\":\"ed25519\""));
        assert!(body.contains("\"test_key\":true"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_routes_and_methods() {
        let (app, dir) = app_permissive("routes");
        let s = start(app);
        let (code, body) = get(&s, "/nope");
        assert_eq!(code, 404);
        assert!(body.contains("not_found"));
        let (code, _) = get(&s, "/packages");
        assert_eq!(code, 405);
        let (code, _) = get(&s, "/api/publish");
        assert_eq!(code, 405);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn metadata_download_and_versions() {
        let (app, dir) = app_permissive("meta");
        app.publish(&PublishRequest::new("jwt", "1.0.0", archive()).with_description("tokens"))
            .unwrap();
        app.publish(&PublishRequest::new("jwt", "1.1.0", archive())).unwrap();
        let s = start(app);

        let (code, body) = get(&s, "/packages/jwt");
        assert_eq!(code, 200);
        assert!(body.contains("\"latest\":\"1.1.0\""));
        assert!(body.contains("\"version\":\"1.0.0\""));

        let (code, body) = get(&s, "/packages/jwt/versions");
        assert_eq!(code, 200);
        assert!(body.contains("\"count\":2"));

        let (code, body) = get_bytes(&s, "/packages/jwt/1.0.0");
        assert_eq!(code, 200);
        assert!(body.starts_with(b"HSPKG\x00\x01"));
        assert_eq!(body, archive());

        let (code, body) = get(&s, "/packages/jwt/1.0.0/manifest");
        assert_eq!(code, 200);
        assert!(body.contains("\"fingerprint\""));

        let (code, _) = get(&s, "/packages/nope");
        assert_eq!(code, 404);
        let (code, _) = get(&s, "/packages/jwt/9.9.9");
        assert_eq!(code, 404);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn responses_carry_the_security_headers() {
        let (app, dir) = app_permissive("headers");
        app.publish(&PublishRequest::new("jwt", "1.0.0", archive())).unwrap();
        let s = start(app);
        let head = raw_head(&s, "GET /health HTTP/1.1");
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("X-Hard-Registry: hardscript-registry"), "{head}");
        assert!(head.contains("X-Content-Type-Options: nosniff"), "{head}");
        assert!(head.contains("Content-Length:"), "{head}");
        let head = raw_head(&s, "GET /packages/jwt/1.0.0 HTTP/1.1");
        assert!(head.contains("X-Hard-Integrity: sha256:"), "{head}");
        assert!(head.contains("ETag: \""), "{head}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue a raw request and return the response head, so header handling
    /// is asserted on the wire rather than through the client.
    fn raw_head(server: &TestServer, request_line: &str) -> String {
        use std::io::{Read, Write};
        let mut stream =
            std::net::TcpStream::connect(server.addr).expect("connect to the test registry");
        let host = server.addr;
        let req = format!("{request_line}\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let head = match text.split("\r\n\r\n").next() {
            Some(h) => h.to_string(),
            None => String::new(),
        };
        // the body may contain invalid utf-8; the head never does
        match String::from_utf8(buf) {
            Ok(full) => match full.split("\r\n\r\n").next() {
                Some(h) => h.to_string(),
                None => head,
            },
            Err(e) => String::from_utf8_lossy(e.as_bytes())
                .split("\r\n\r\n")
                .next()
                .unwrap_or_default()
                .to_string(),
        }
    }

    #[test]
    fn download_headers_carry_integrity_and_etag() {
        let (app, dir) = app_permissive("dlheaders");
        app.publish(&PublishRequest::new("jwt", "1.0.0", archive()))
            .unwrap();
        let resp = call(
            &app,
            Request::new("GET", "/packages/jwt/1.0.0"),
        );
        assert_eq!(resp.status, 200);
        assert!(resp.header("X-Hard-Integrity").unwrap().starts_with("sha256:"));
        assert!(resp.header("X-Hard-Fingerprint").is_some());
        assert!(resp.header("ETag").unwrap().starts_with('"'));
        assert!(resp
            .header("Content-Type")
            .unwrap()
            .contains("hardscript.package"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn downloads_are_counted() {
        let (app, dir) = app_permissive("downloads");
        app.publish(&PublishRequest::new("jwt", "1.0.0", archive())).unwrap();
        let s = start(app);
        assert_eq!(get(&s, "/packages/jwt/1.0.0").0, 200);
        let (_, body) = get(&s, "/stats");
        assert!(body.contains("\"downloads\":1"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ranged_downloads_answer_206() {
        let (app, dir) = app_permissive("range");
        app.publish(&PublishRequest::new("jwt", "1.0.0", archive())).unwrap();
        let req = Request::new("GET", "/packages/jwt/1.0.0").with_header("Range", "bytes=0-3");
        let resp = call(&app, req);
        assert_eq!(resp.status, 206);
        assert_eq!(resp.body, b"HSPK".to_vec());
        assert!(resp.header("Content-Range").unwrap().starts_with("bytes 0-3/"));
        // a range is not counted as a download
        let (_, body) = {
            let s = start(Arc::clone(&app));
            get(&s, "/stats")
        };
        assert!(body.contains("\"downloads\":0"), "{body}");

        let req = Request::new("GET", "/packages/jwt/1.0.0").with_header("Range", "bytes=99999-99999");
        assert_eq!(call(&app, req).status, 416);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn range_parsing_rules() {
        assert_eq!(parse_range("bytes=0-10"), Some((0, Some(10))));
        assert_eq!(parse_range("bytes= 5 - 6 "), Some((5, Some(6))));
        assert_eq!(parse_range("bytes=0-0"), Some((0, Some(0))));
        // the open-ended form is what a resumed download sends
        assert_eq!(parse_range("bytes=20-"), Some((20, None)));
        assert_eq!(parse_range("bytes=10-5"), None);
        assert_eq!(parse_range("bytes=-5"), None);
        assert_eq!(parse_range("items=0-5"), None);
        assert_eq!(parse_range("bytes=abc-def"), None);
        assert_eq!(parse_range("bytes=-"), None);
    }

    #[test]
    fn search_endpoint() {
        let (app, dir) = app_permissive("search");
        app.publish(&PublishRequest::new("jwt", "1.0.0", archive()).with_tag("auth"))
            .unwrap();
        app.publish(&PublishRequest::new("hardscript-json", "0.1.0", archive()))
            .unwrap();
        let s = start(app);
        let (code, body) = get(&s, "/search?q=jwt");
        assert_eq!(code, 200);
        assert!(body.contains("\"name\":\"jwt\""), "{body}");
        let (code, body) = get(&s, "/search?tag=auth");
        assert_eq!(code, 200);
        assert!(body.contains("\"name\":\"jwt\""));
        let (code, body) = get(&s, "/search?prefix=hardscript");
        assert_eq!(code, 200);
        assert!(body.contains("hardscript-json"));
        let (_, body) = get(&s, "/search?q=jwt&limit=1");
        assert!(body.contains("\"count\":1"), "{body}");
        let (code, _) = get(&s, "/search");
        assert_eq!(code, 400);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signature_endpoint() {
        let (app, dir) = build("sig", Config { open_publish: true, ..Config::default() });
        app.publish(&PublishRequest::new("jwt", "1.0.0", archive())).unwrap();
        let s = start(app);
        let (code, body) = get(&s, "/packages/jwt/1.0.0/signature");
        assert_eq!(code, 200);
        assert!(body.contains("\"verified\":true"), "{body}");
        assert!(body.contains("\"algorithm\":\"ed25519\""));
        let (code, _) = get(&s, "/packages/jwt/9.9.9/signature");
        assert_eq!(code, 404);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mirror_feeds() {
        let (app, dir) = app_permissive("mirror");
        app.publish(&PublishRequest::new("jwt", "1.0.0", archive())).unwrap();
        let s = start(app);
        let (code, body) = get(&s, "/api/mirror/changes?since=0");
        assert_eq!(code, 200);
        assert!(body.contains("\"kind\":\"published\""));
        let (code, body) = get(&s, "/api/mirror/manifest");
        assert_eq!(code, 200);
        assert!(body.contains("\"count\":1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn whoami_requires_a_token() {
        let (app, dir) = app_permissive("whoami");
        let s = start(app);
        let (code, body) = get(&s, "/auth/whoami");
        assert_eq!(code, 401);
        assert!(body.contains("unauthorized"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn publish_requires_auth_when_configured() {
        let (app, dir) = app_secure("secure");
        app.register("ada", "supersecret", None).unwrap();
        let (_, plaintext) = app.create_token("ada", "ci", None).unwrap();
        let mut req = Request::new("POST", "/api/publish")
            .with_body(archive())
            .with_header("X-Hard-Package", "jwt")
            .with_header("X-Hard-Version", "1.0.0");
        let resp = call(&app, req.clone());
        assert_eq!(resp.status, 401);
        assert!(resp.text_body().contains("unauthorized"));
        req.set_header("Authorization", &format!("Bearer {plaintext}"));
        let resp = call(&app, req);
        assert_eq!(resp.status, 201, "{}", resp.text_body());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_token_is_401() {
        let (app, dir) = app_secure("badtok");
        let req = Request::new("POST", "/api/publish")
            .with_body(archive())
            .with_header("X-Hard-Package", "jwt")
            .with_header("X-Hard-Version", "1.0.0")
            .with_header("Authorization", "Bearer hspat_nope");
        let resp = call(&app, req);
        assert_eq!(resp.status, 401);
        assert!(resp.text_body().contains("not valid"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn yank_route_marks_and_restores() {
        let (app, dir) = app_permissive("yankroute");
        app.publish(&PublishRequest::new("jwt", "1.0.0", archive())).unwrap();
        let resp = call(
            &app,
            Request::new("POST", "/api/yank?name=jwt&version=1.0.0"),
        );
        assert_eq!(resp.status, 200, "{}", resp.text_body());
        assert!(resp.text_body().contains("\"yanked\":true"));
        let resp = call(
            &app,
            Request::new("POST", "/api/unyank?name=jwt&version=1.0.0"),
        );
        assert!(resp.text_body().contains("\"yanked\":false"));
        let resp = call(
            &app,
            Request::new("POST", "/api/yank?name=jwt&version=9.9.9"),
        );
        assert_eq!(resp.status, 404);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn yank_route_accepts_a_json_body() {
        let (app, dir) = app_permissive("yankjson");
        app.publish(&PublishRequest::new("jwt", "1.0.0", archive())).unwrap();
        let resp = call(
            &app,
            Request::new("POST", "/api/yank")
                .with_header("Content-Type", "application/json")
                .with_body(br#"{"name":"jwt","version":"1.0.0"}"#.to_vec()),
        );
        assert_eq!(resp.status, 200, "{}", resp.text_body());
        assert!(resp.text_body().contains("\"yanked\":true"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn account_endpoints() {
        let (app, dir) = app_permissive("accounts");
        let resp = call(
            &app,
            Request::new("POST", "/auth/register")
                .with_body(br#"{"user":"ada","password":"supersecret","email":"a@b.c"}"#.to_vec()),
        );
        assert_eq!(resp.status, 201, "{}", resp.text_body());
        assert!(resp.text_body().contains("password_strength"));

        let resp = call(
            &app,
            Request::new("POST", "/auth/login")
                .with_body(br#"{"user":"ada","password":"supersecret"}"#.to_vec()),
        );
        assert_eq!(resp.status, 200, "{}", resp.text_body());
        let body = hs_compiler::json::parse(&resp.text_body()).unwrap();
        let session = body.get("token").and_then(|t| t.as_str()).unwrap().to_string();
        assert!(session.starts_with("hssess_"));
        let sid = resp.header("X-Hard-Session-Id").unwrap().to_string();

        let resp = call(
            &app,
            Request::new("GET", "/auth/whoami").with_header("Authorization", &format!("Bearer {session}")),
        );
        assert_eq!(resp.status, 200);
        assert!(resp.text_body().contains("\"user\":\"ada\""));

        // a session carries every scope except admin, so it may mint tokens
        let resp = call(
            &app,
            Request::new("POST", "/auth/tokens")
                .with_header("Authorization", &format!("Bearer {session}"))
                .with_body(br#"{"name":"ci"}"#.to_vec()),
        );
        assert_eq!(resp.status, 201, "{}", resp.text_body());
        assert!(resp.text_body().contains("\"scopes\":[\"read\",\"publish\",\"yank\"]"), "{}", resp.text_body());
        let pat = hs_compiler::json::parse(&resp.text_body())
            .unwrap()
            .get("token")
            .and_then(|t| t.as_str())
            .unwrap()
            .to_string();
        let token_id = hs_compiler::json::parse(&resp.text_body())
            .unwrap()
            .get("id")
            .and_then(|t| t.as_str())
            .unwrap()
            .to_string();

        // the default PAT has no `token` scope, so it may not manage tokens
        let resp = call(
            &app,
            Request::new("GET", "/auth/tokens").with_header("Authorization", &format!("Bearer {pat}")),
        );
        assert_eq!(resp.status, 403);
        // the session may, and sees both records
        let resp = call(
            &app,
            Request::new("GET", "/auth/tokens").with_header("Authorization", &format!("Bearer {session}")),
        );
        assert_eq!(resp.status, 200);
        assert!(resp.text_body().contains(&token_id));

        let resp = call(
            &app,
            Request::new("DELETE", &format!("/auth/tokens/{token_id}"))
                .with_header("Authorization", &format!("Bearer {session}")),
        );
        assert_eq!(resp.status, 200);
        let resp = call(
            &app,
            Request::new("GET", "/auth/whoami").with_header("Authorization", &format!("Bearer {pat}")),
        );
        assert_eq!(resp.status, 401);

        let resp = call(
            &app,
            Request::new("POST", "/auth/logout").with_header("Authorization", &format!("Bearer {session}")),
        );
        assert_eq!(resp.status, 200);
        assert!(resp.text_body().contains(&sid));
        let resp = call(
            &app,
            Request::new("POST", "/auth/logout").with_header("Authorization", "Bearer hssess_x"),
        );
        assert_eq!(resp.status, 401);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_creation_validates_scopes_and_names() {
        let (app, dir) = app_permissive("tokval");
        // token management needs a real token even on an open registry
        let ghost = call(&app, Request::new("POST", "/auth/tokens"));
        assert_eq!(ghost.status, 401);
        app.register("ada", "supersecret", None).unwrap();
        let (_, pat) = app
            .create_token("ada", "root", Some(vec!["token".into(), "read".into()]))
            .unwrap();
        let resp = call(
            &app,
            Request::new("POST", "/auth/tokens")
                .with_header("Authorization", &format!("Bearer {pat}"))
                .with_body(br#"{"name":"x","scopes":["nope"]}"#.to_vec()),
        );
        assert_eq!(resp.status, 400);
        assert!(resp.text_body().contains("unknown scope"));
        let resp = call(
            &app,
            Request::new("POST", "/auth/tokens").with_header("Authorization", &format!("Bearer {pat}")),
        );
        assert_eq!(resp.status, 201, "default name should work");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scope_enforcement_is_per_scope() {
        // Auth is on (no open publishing), so scopes are enforced.
        let (app, dir) = build(
            "scopes",
            Config {
                sign_publishes: false,
                ..Config::default()
            },
        );
        app.register("ada", "supersecret", None).unwrap();
        let (_, pat) = app.create_token("ada", "readonly", Some(vec!["read".into()])).unwrap();
        let mut req = Request::new("POST", "/api/publish")
            .with_body(archive())
            .with_header("X-Hard-Package", "jwt")
            .with_header("X-Hard-Version", "1.0.0")
            .with_header("Authorization", &format!("Bearer {pat}"));
        let resp = call(&app, req.clone());
        assert_eq!(resp.status, 403);
        assert!(resp.text_body().contains("forbidden"));
        // a decodable-but-invalid publish is only reported once the caller is
        // authorized: an invalid package name is 422, a malformed body is 400
        let (_, writer) = app.create_token("ada", "publisher", None).unwrap();
        req.set_header("Authorization", &format!("Bearer {writer}"));
        req.set_header("X-Hard-Package", "Bad Name");
        let resp = call(&app, req.clone());
        assert_eq!(resp.status, 422, "validation runs after auth: {}", resp.text_body());
        assert!(resp.text_body().contains("invalid_package"), "{}", resp.text_body());
        req.set_header("Content-Type", "application/json");
        req.body = br#"{"name":"jwt"}"#.to_vec();
        let resp = call(&app, req);
        assert_eq!(resp.status, 400);
        assert!(resp.text_body().contains("version"), "{}", resp.text_body());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn register_and_login_errors() {
        let (app, dir) = app_permissive("autherr");
        let resp = call(
            &app,
            Request::new("POST", "/auth/register").with_body(b"not json".to_vec()),
        );
        assert_eq!(resp.status, 400);
        let resp = call(
            &app,
            Request::new("POST", "/auth/register").with_body(br#"{"user":"ada"}"#.to_vec()),
        );
        assert_eq!(resp.status, 400);
        let resp = call(
            &app,
            Request::new("POST", "/auth/login").with_body(br#"{"user":"ghost","password":"x"}"#.to_vec()),
        );
        assert_eq!(resp.status, 404);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_publish_request_reads_headers() {
        let mut req = Request::new("POST", "/api/publish").with_body(archive());
        req.set_header("X-Hard-Package", "jwt");
        req.set_header("X-Hard-Version", "1.2.0");
        req.set_header("X-Hard-Description", "JSON web tokens");
        req.set_header("X-Hard-License", "MIT");
        req.set_header("X-Hard-Tags", "auth, security");
        let p = parse_publish_request(&req).unwrap();
        assert_eq!(p.name, "jwt");
        assert_eq!(p.version, "1.2.0");
        assert_eq!(p.description.as_deref(), Some("JSON web tokens"));
        assert_eq!(p.license.as_deref(), Some("MIT"));
        assert_eq!(p.tags, vec!["auth", "security"]);
        assert_eq!(p.archive, archive());
    }

    #[test]
    fn etag_shortening() {
        assert_eq!(short_digest("sha256:0123456789abcdef"), "0123456789abcdef");
        assert_eq!(short_digest("abc"), "abc");
    }
}
