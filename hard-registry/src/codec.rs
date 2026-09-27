//! Request codecs: turning a decoded JSON body into typed values.
//!
//! Every failure carries the field it came from, so the router can answer
//! with a `fields` array instead of a bare string — `hard publish` prints it
//! verbatim, and a human can tell `version` from `archive` at a glance.

use crate::app::PublishRequest;
use crate::model::{Dep, DepKind, MAX_ARCHIVE_BYTES};
use hs_compiler::json::Json;

/// Why a body could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The body was not JSON at all.
    Json(String),
    /// The body was JSON but fields were missing or malformed.
    Fields(Vec<FieldError>),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Json(m) => write!(f, "{m}"),
            DecodeError::Fields(errs) => write!(
                f,
                "{}",
                errs.iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
    }
}

impl DecodeError {
    pub fn status(&self) -> u16 {
        400
    }

    pub fn code(&self) -> &'static str {
        match self {
            DecodeError::Json(_) => "invalid_json",
            DecodeError::Fields(_) => "invalid_request",
        }
    }
}

/// A field-level problem with a decoded request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

impl FieldError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> FieldError {
        FieldError {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

/// Result alias for decoding.
pub type Decoded<T> = Result<T, DecodeError>;

/// Parse a request body as JSON.
pub fn parse_body(text: &str) -> Decoded<Json> {
    hs_compiler::json::parse(text.trim())
        .ok_or_else(|| DecodeError::Json("body is not valid JSON".to_string()))
}

/// Decode the JSON envelope publish form:
///
/// ```json
/// { "name": "jwt", "version": "1.0.0", "description": "...",
///   "dependencies": [{"name":"base64","req":"^0.4.0","kind":"normal"}],
///   "tags": ["auth"], "archive_base64": "..." }
/// ```
pub fn publish_from_json(j: &Json) -> Decoded<PublishRequest> {
    let mut errs = Vec::new();
    let name = take_string(j, "name", &mut errs);
    let version = take_string(j, "version", &mut errs);
    let mut deps = Vec::new();
    match j.get("dependencies") {
        None | Some(Json::Null) => {}
        Some(Json::Arr(items)) => {
            for it in items {
                match decode_dep(it) {
                    Ok(d) => deps.push(d),
                    Err(e) => errs.push(e),
                }
            }
        }
        Some(Json::Obj(pairs)) => {
            for (k, v) in pairs {
                let Some(req) = v.as_str() else {
                    errs.push(FieldError::new(
                        format!("dependencies.{k}"),
                        "a dependency must map a name to a requirement string",
                    ));
                    continue;
                };
                deps.push(Dep::normal(k, req));
            }
        }
        Some(_) => errs.push(FieldError::new(
            "dependencies",
            "dependencies must be an array or an object",
        )),
    }
    let archive = match j.get("archive_base64").or_else(|| j.get("archive")) {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Str(b64)) => {
            // Reject on the encoded length first: decoding a hostile 100 MB
            // string just to discover it is too big is exactly the work the
            // size limit exists to prevent.
            if b64.len() > MAX_ARCHIVE_BYTES / 3 * 4 + 8 {
                errs.push(FieldError::new(
                    "archive_base64",
                    format!(
                        "encoded archive is {} bytes, over the {MAX_ARCHIVE_BYTES} byte limit",
                        MAX_ARCHIVE_BYTES / 3 * 4 + 8
                    ),
                ));
                Vec::new()
            } else {
                match crate::base64::decode(b64) {
                    Some(bytes) => bytes,
                    None => {
                        errs.push(FieldError::new("archive_base64", "not valid base64"));
                        Vec::new()
                    }
                }
            }
        }
        Some(_) => {
            errs.push(FieldError::new(
                "archive_base64",
                "archive_base64 must be a base64 string",
            ));
            Vec::new()
        }
    };
    if archive.len() > MAX_ARCHIVE_BYTES {
        errs.push(FieldError::new(
            "archive_base64",
            format!(
                "archive is {} bytes, over the {MAX_ARCHIVE_BYTES} byte limit",
                archive.len()
            ),
        ));
    }
    if !errs.is_empty() {
        return Err(DecodeError::Fields(errs));
    }
    let mut req = PublishRequest {
        name,
        version,
        description: opt_string(j, "description"),
        license: opt_string(j, "license"),
        homepage: opt_string(j, "homepage"),
        repository: opt_string(j, "repository"),
        documentation: opt_string(j, "documentation"),
        keywords: string_list(j, "keywords"),
        tags: string_list(j, "tags"),
        deps,
        channel: opt_string(j, "channel"),
        archive,
    };
    sort_deps(&mut req);
    Ok(req)
}

/// Canonical dependency order, so two registries fingerprint the same
/// publish identically regardless of the order the client listed.
pub fn sort_deps(req: &mut PublishRequest) {
    req.deps.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.req.cmp(&b.req))
    });
    req.deps.dedup();
}

fn decode_dep(j: &Json) -> Result<Dep, FieldError> {
    match j {
        Json::Str(s) => Err(FieldError::new(
            "dependencies",
            format!("'{s}' must be an object with 'name' and 'req'"),
        )),
        Json::Obj(_) => {
            let name = j
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| FieldError::new("dependencies.name", "missing dependency name"))?;
            let req = j
                .get("req")
                .or_else(|| j.get("version"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| FieldError::new("dependencies.req", "missing requirement"))?;
            let kind = j
                .get("kind")
                .and_then(|v| v.as_str())
                .map(DepKind::parse)
                .unwrap_or(DepKind::Normal);
            Ok(Dep {
                name: name.to_string(),
                req: req.to_string(),
                kind,
            })
        }
        _ => Err(FieldError::new(
            "dependencies",
            "each dependency must be an object",
        )),
    }
}

fn take_string(j: &Json, key: &str, errs: &mut Vec<FieldError>) -> String {
    match j.get(key).and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            errs.push(FieldError::new(key, "missing required field"));
            String::new()
        }
    }
}

fn opt_string(j: &Json, key: &str) -> Option<String> {
    j.get(key).and_then(|v| v.as_str()).map(String::from)
}

fn string_list(j: &Json, key: &str) -> Vec<String> {
    match j.get(key) {
        Some(Json::Arr(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(Json::Str(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// A login request: `{"user":"ada","password":"..."}`.
pub fn decode_login(j: &Json) -> Decoded<(String, String)> {
    let mut errs = Vec::new();
    // `user` and `username` are both accepted; only complain if neither is there.
    let user = j
        .get("user")
        .or_else(|| j.get("username"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if user.is_empty() {
        errs.push(FieldError::new("user", "missing required field"));
    }
    let password = take_string(j, "password", &mut errs);
    if !errs.is_empty() {
        return Err(DecodeError::Fields(errs));
    }
    Ok((user, password))
}

/// A token-create request: `{"name":"ci","scopes":["read","publish"]}`.
pub fn decode_token_request(j: &Json) -> Decoded<(String, Option<Vec<String>>)> {
    let mut errs = Vec::new();
    let name = take_string(j, "name", &mut errs);
    let scopes = match j.get("scopes") {
        Some(Json::Arr(items)) => Some(
            items
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<String>>(),
        ),
        Some(Json::Null) | None => None,
        Some(Json::Str(s)) => Some(vec![s.clone()]),
        Some(_) => {
            errs.push(FieldError::new("scopes", "scopes must be an array of strings"));
            None
        }
    };
    if !errs.is_empty() {
        return Err(DecodeError::Fields(errs));
    }
    Ok((name, scopes))
}

/// A yank request: `{"name":"jwt","version":"1.0.0","yanked":true}`.
pub fn decode_yank(j: &Json) -> Decoded<(String, String, bool)> {
    let mut errs = Vec::new();
    let name = take_string(j, "name", &mut errs);
    let version = take_string(j, "version", &mut errs);
    let yanked = match j.get("yanked") {
        Some(Json::Bool(b)) => *b,
        None | Some(Json::Null) => true,
        Some(Json::Str(s)) => !matches!(s.as_str(), "false" | "0" | "no"),
        Some(_) => {
            errs.push(FieldError::new("yanked", "yanked must be a boolean"));
            true
        }
    };
    if !errs.is_empty() {
        return Err(DecodeError::Fields(errs));
    }
    Ok((name, version, yanked))
}

/// A JSON error body listing every field problem.
pub fn field_errors_json(errs: &[FieldError]) -> Json {
    Json::obj(vec![
        (
            "error",
            Json::obj(vec![
                ("code", Json::str("invalid_request")),
                (
                    "message",
                    Json::str(
                        errs
                            .iter()
                            .map(|e| e.to_string())
                            .collect::<Vec<_>>()
                            .join("; "),
                    ),
                ),
            ]),
        ),
        (
            "fields",
            Json::arr(
                errs
                    .iter()
                    .map(|e| {
                        Json::obj(vec![
                            ("field", Json::str(&e.field)),
                            ("message", Json::str(&e.message)),
                        ])
                    })
                    .collect::<Vec<Json>>(),
            ),
        ),
    ])
}

/// A 422 body for validation failures (well-formed request, bad values).
pub fn validation_json(errs: &[crate::model::ValidationError]) -> Json {
    Json::obj(vec![
        (
            "error",
            Json::obj(vec![
                ("code", Json::str("invalid_package")),
                (
                    "message",
                    Json::str(
                        errs
                            .iter()
                            .map(|e| e.to_string())
                            .collect::<Vec<_>>()
                            .join("; "),
                    ),
                ),
            ]),
        ),
        (
            "fields",
            Json::arr(
                errs
                    .iter()
                    .map(|e| {
                        Json::obj(vec![
                            ("field", Json::str(&e.field)),
                            ("message", Json::str(&e.message)),
                        ])
                    })
                    .collect::<Vec<Json>>(),
            ),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(src: &str) -> Json {
        hs_compiler::json::parse(src).unwrap()
    }

    #[test]
    fn decodes_a_full_publish_request() {
        let body = j(r#"{
            "name":"jwt","version":"1.2.0","description":"tokens","license":"MIT",
            "homepage":"https://example.org","repository":"https://git/x",
            "documentation":"https://docs", "keywords":["auth","token"],
            "tags":["auth"],
            "dependencies":[{"name":"base64","req":"^0.4.0"},{"name":"q","req":"1","kind":"dev"}],
            "channel":"stable","archive_base64":"aGVsbG8="
        }"#);
        let r = publish_from_json(&body).unwrap();
        assert_eq!(r.name, "jwt");
        assert_eq!(r.version, "1.2.0");
        assert_eq!(r.description.as_deref(), Some("tokens"));
        assert_eq!(r.license.as_deref(), Some("MIT"));
        assert_eq!(r.homepage.as_deref(), Some("https://example.org"));
        assert_eq!(r.keywords, vec!["auth", "token"]);
        assert_eq!(r.tags, vec!["auth"]);
        assert_eq!(r.deps.len(), 2);
        assert_eq!(r.deps[0].name, "base64");
        assert_eq!(r.deps[1].kind, DepKind::Dev);
        assert_eq!(r.archive, b"hello".to_vec());
        assert_eq!(r.channel.as_deref(), Some("stable"));
    }

    #[test]
    fn dependencies_may_be_an_object() {
        let body = j(r#"{"name":"a","version":"1.0.0","dependencies":{"b":"^2.0.0"}}"#);
        let r = publish_from_json(&body).unwrap();
        assert_eq!(r.deps, vec![Dep::normal("b", "^2.0.0")]);
    }

    #[test]
    fn version_may_stand_in_for_req() {
        let body = j(r#"{"name":"a","version":"1.0.0","dependencies":[{"name":"b","version":"2.0.0"}]}"#);
        let r = publish_from_json(&body).unwrap();
        assert_eq!(r.deps[0].req, "2.0.0");
    }

    #[test]
    fn reports_every_missing_required_field() {
        let errs = publish_from_json(&j("{}")).unwrap_err();
        let DecodeError::Fields(fields) = errs else {
            panic!("expected field errors")
        };
        assert_eq!(fields.len(), 2);
        assert!(fields.iter().any(|e| e.field == "name"));
        assert!(fields.iter().any(|e| e.field == "version"));
        let out = field_errors_json(&fields).to_string();
        assert!(out.contains("invalid_request"));
        assert!(out.contains("\"fields\":["));
    }

    #[test]
    fn rejects_bad_dependency_shapes() {
        assert!(matches!(
            publish_from_json(&j(r#"{"name":"a","version":"1","dependencies":["b"]}"#)),
            Err(DecodeError::Fields(_))
        ));
        assert!(matches!(
            publish_from_json(&j(r#"{"name":"a","version":"1","dependencies":[{}]}"#)),
            Err(DecodeError::Fields(_))
        ));
        assert!(matches!(
            publish_from_json(&j(r#"{"name":"a","version":"1","dependencies":7}"#)),
            Err(DecodeError::Fields(_))
        ));
    }

    #[test]
    fn rejects_bad_base64_and_oversized_archives() {
        assert!(matches!(
            publish_from_json(&j(r#"{"name":"a","version":"1","archive_base64":"!!!"}"#)),
            Err(DecodeError::Fields(_))
        ));
        assert!(matches!(
            publish_from_json(&j(r#"{"name":"a","version":"1","archive_base64":5}"#)),
            Err(DecodeError::Fields(_))
        ));
        let big = "A".repeat(MAX_ARCHIVE_BYTES * 2);
        let err = publish_from_json(&j(&format!(
            r#"{{"name":"a","version":"1","archive_base64":"{big}"}}"#
        )))
        .unwrap_err();
        assert!(err.to_string().contains("archive_base64"), "{err}");
    }

    #[test]
    fn dependencies_are_sorted_and_deduped() {
        let body = j(r#"{"name":"a","version":"1.0.0","dependencies":[
            {"name":"z","req":"1"},{"name":"a","req":"1"},{"name":"a","req":"1"}]}"#);
        let r = publish_from_json(&body).unwrap();
        assert_eq!(r.deps.len(), 2);
        assert_eq!(r.deps[0].name, "a");
    }

    #[test]
    fn body_parser_rejects_non_json() {
        assert!(matches!(parse_body("not json"), Err(DecodeError::Json(_))));
        assert!(parse_body("  {\"a\":1}  ").is_ok());
    }

    #[test]
    fn decodes_login_and_token_requests() {
        let (u, p) = decode_login(&j(r#"{"user":"ada","password":"secret"}"#)).unwrap();
        assert_eq!((u.as_str(), p.as_str()), ("ada", "secret"));
        let (u, _) = decode_login(&j(r#"{"username":"ada","password":"x"}"#)).unwrap();
        assert_eq!(u, "ada");
        assert!(decode_login(&j("{}")).is_err());

        let (name, scopes) = decode_token_request(&j(r#"{"name":"ci","scopes":["read"]}"#)).unwrap();
        assert_eq!(name, "ci");
        assert_eq!(scopes.unwrap(), vec!["read"]);
        let (_, none) = decode_token_request(&j(r#"{"name":"ci"}"#)).unwrap();
        assert!(none.is_none());
        let (_, single) = decode_token_request(&j(r#"{"name":"ci","scopes":"read"}"#)).unwrap();
        assert_eq!(single.unwrap(), vec!["read"]);
        assert!(decode_token_request(&j(r#"{"name":"ci","scopes":3}"#)).is_err());
    }

    #[test]
    fn decodes_yank_requests() {
        let (n, v, y) = decode_yank(&j(r#"{"name":"a","version":"1.0.0"}"#)).unwrap();
        assert_eq!((n.as_str(), v.as_str(), y), ("a", "1.0.0", true));
        let (_, _, y) = decode_yank(&j(r#"{"name":"a","version":"1.0.0","yanked":false}"#)).unwrap();
        assert!(!y);
        let (_, _, y) = decode_yank(&j(r#"{"name":"a","version":"1.0.0","yanked":"no"}"#)).unwrap();
        assert!(!y);
        assert!(matches!(
            decode_yank(&j(r#"{"name":"a"}"#)),
            Err(DecodeError::Fields(_))
        ));
    }

    #[test]
    fn validation_json_lists_fields() {
        let errs = vec![
            crate::model::ValidationError::new("name", "bad name"),
            crate::model::ValidationError::new("version", "bad version"),
        ];
        let j = validation_json(&errs).to_string();
        assert!(j.contains("invalid_package"));
        assert!(j.contains("\"field\":\"name\""));
        assert!(j.contains("\"field\":\"version\""));
    }
}
