//! The registry authentication client: login, logout, tokens, whoami.
//!
//! This is the *client* half of M8.5. The server half lives in
//! `hard-registry` (`/auth/login`, `/auth/tokens`, ...); this module speaks
//! that protocol and stores what comes back through
//! [`crate::credentials`].
//!
//! Tokens are treated as write-once: the registry returns a PAT's plaintext
//! exactly once, in the response that creates it, and everything after that
//! works from the token id.

use crate::credentials::CredentialStore;
use crate::registry::{HttpResponse, Method, Registry};
use hs_compiler::json::Json;

/// What a login produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Login {
    /// The plaintext session token, returned once.
    pub token: String,
    /// The public id of the stored token record (`tok_...`).
    pub id: String,
    pub user: String,
    pub scopes: Vec<String>,
}

/// A token as the registry describes it. Never carries the plaintext.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenInfo {
    pub id: String,
    pub user: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked: bool,
}

/// Everything `hard whoami` prints.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Whoami {
    pub user: String,
    pub token_id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// The authentication client.
#[derive(Clone, Debug)]
pub struct Auth {
    pub registry: Registry,
    pub store: CredentialStore,
}

impl Auth {
    pub fn new(registry: Registry, store: CredentialStore) -> Auth {
        Auth { registry, store }
    }

    /// The registry this client talks to, normalized.
    pub fn url(&self) -> String {
        crate::credentials::normalize(&self.registry.config.url)
    }

    /// Exchange a password for a session token and remember it.
    ///
    /// The token is stored before it is returned, so a caller cannot hold a
    /// valid credential it failed to save.
    pub fn login(&self, user: &str, password: &str) -> Result<Login, String> {
        if user.trim().is_empty() {
            return Err("a user name is required".to_string());
        }
        let body = format!(
            "{{\"user\":\"{}\",\"password\":\"{}\"}}",
            escape(user),
            escape(password)
        );
        let resp = self
            .registry
            .post_json("/auth/login", &body, None)
            .map_err(|e| format!("cannot reach the registry: {e}"))?;
        let parsed = expect_ok(resp, "login")?;
        let login = parse_login(&parsed);
        if login.token.is_empty() {
            return Err("the registry did not return a token".to_string());
        }
        let mut store = self.store.clone();
        store.login(&self.url(), &login.user, &login.token)?;
        Ok(login)
    }

    /// Store a token obtained elsewhere (a CI secret, `hard token create`
    /// piped into a machine).
    pub fn adopt(&self, user: &str, token: &str) -> Result<(), String> {
        let mut store = self.store.clone();
        store.login(&self.url(), user, token)
    }

    /// Forget the stored credentials, and ask the registry to revoke the
    /// session too when it still exists there.
    pub fn logout(&self) -> Result<bool, String> {
        let token = self.store.token_for(&self.url());
        let mut revoked = false;
        if !token.is_empty() {
            if let Ok(resp) = self
                .registry
                .request(Method::Post, "/auth/logout", b"{}".to_vec(), &[("Content-Type", "application/json")], Some(&token))
            {
                revoked = resp.is_success();
            }
        }
        let mut store = self.store.clone();
        let removed = store.logout(&self.url())?;
        Ok(removed || revoked)
    }

    /// Create a personal access token. The plaintext is returned once and
    /// added to the store as an extra token for this registry.
    pub fn create_token(
        &self,
        label: &str,
        scopes: &[String],
    ) -> Result<(TokenInfo, String), String> {
        let token = self.store.token_for(&self.url());
        if token.is_empty() {
            return Err(format!(
                "not logged in to {}; run `hard login` first",
                self.url()
            ));
        }
        let mut body = format!("{{\"name\":\"{}\"", escape(label));
        if !scopes.is_empty() {
            let list: Vec<String> = scopes.iter().map(|s| format!("\"{}\"", escape(s))).collect();
            body.push_str(&format!(",\"scopes\":[{}]", list.join(", ")));
        }
        body.push('}');
        let resp = self
            .registry
            .post_json("/auth/tokens", &body, Some(&token))
            .map_err(|e| format!("cannot reach the registry: {e}"))?;
        let parsed = expect_ok(resp, "token create")?;
        let j = parsed
            .json()
            .ok_or_else(|| "the registry did not return JSON".to_string())?;
        let plaintext = j
            .get("token")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string();
        if plaintext.is_empty() {
            return Err("the registry did not return the new token".to_string());
        }
        let info = parse_token(&j);
        let mut store = self.store.clone();
        store.add_token(&self.url(), &plaintext)?;
        Ok((info, plaintext))
    }

    /// List the caller's tokens.
    pub fn list_tokens(&self) -> Result<Vec<TokenInfo>, String> {
        let token = self.store.token_for(&self.url());
        if token.is_empty() {
            return Err(format!("not logged in to {}", self.url()));
        }
        let resp = self
            .registry
            .request(Method::Get, "/auth/tokens", Vec::new(), &[], Some(&token))
            .map_err(|e| format!("cannot reach the registry: {e}"))?;
        let parsed = expect_ok(resp, "token list")?;
        let j = parsed
            .json()
            .ok_or_else(|| "the registry did not return JSON".to_string())?;
        let mut out = Vec::new();
        if let Some(Json::Arr(items)) = j.get("tokens") {
            for it in items {
                out.push(parse_token(it));
            }
        }
        Ok(out)
    }

    /// Revoke one of the caller's tokens.
    pub fn revoke_token(&self, id: &str) -> Result<TokenInfo, String> {
        let token = self.store.token_for(&self.url());
        if token.is_empty() {
            return Err(format!("not logged in to {}", self.url()));
        }
        if id.trim().is_empty() {
            return Err("a token id is required".to_string());
        }
        let path = format!("/auth/tokens/{}", crate::publish::urlencode(id));
        let resp = self
            .registry
            .request(Method::Delete, &path, Vec::new(), &[], Some(&token))
            .map_err(|e| format!("cannot reach the registry: {e}"))?;
        let parsed = expect_ok(resp, "token revoke")?;
        let j = parsed
            .json()
            .ok_or_else(|| "the registry did not return JSON".to_string())?;
        Ok(parse_token(&j))
    }

    /// Who the presented token belongs to.
    pub fn whoami(&self) -> Result<Whoami, String> {
        let token = self.store.token_for(&self.url());
        if token.is_empty() {
            return Err(format!("not logged in to {}", self.url()));
        }
        let resp = self
            .registry
            .request(Method::Get, "/auth/whoami", Vec::new(), &[], Some(&token))
            .map_err(|e| format!("cannot reach the registry: {e}"))?;
        let parsed = expect_ok(resp, "whoami")?;
        let j = parsed
            .json()
            .ok_or_else(|| "the registry did not return JSON".to_string())?;
        Ok(Whoami {
            user: str_of(&j, "user").unwrap_or_default(),
            token_id: str_of(&j, "token_id").unwrap_or_default(),
            name: str_of(&j, "name").unwrap_or_default(),
            scopes: strings_of(&j, "scopes"),
            created_at: j.get("created_at").and_then(|v| v.as_num()).unwrap_or(0),
            last_used_at: j.get("last_used_at").and_then(|v| v.as_num()),
        })
    }

    /// Create an account. Open registration is a registry policy, so a
    /// refusal is reported as-is.
    pub fn register(&self, user: &str, password: &str, email: Option<&str>) -> Result<String, String> {
        let mut body = format!(
            "{{\"user\":\"{}\",\"password\":\"{}\"",
            escape(user),
            escape(password)
        );
        if let Some(e) = email {
            body.push_str(&format!(",\"email\":\"{}\"", escape(e)));
        }
        body.push('}');
        let resp = self
            .registry
            .post_json("/auth/register", &body, None)
            .map_err(|e| format!("cannot reach the registry: {e}"))?;
        let parsed = expect_ok(resp, "register")?;
        let j = match parsed.json() {
            Some(j) => j,
            None => return Err("the registry did not return JSON".to_string()),
        };
        let strength = str_of(&j, "password_strength").unwrap_or_default();
        let name = str_of(&j, "name").unwrap_or_else(|| user.to_string());
        if strength.is_empty() {
            Ok(name)
        } else {
            Ok(format!("{name} (password strength: {strength})"))
        }
    }

    /// The token the client would present, redacted for display.
    pub fn describe(&self) -> String {
        crate::credentials::describe(&self.url(), self.store.get(&self.url()))
    }

    /// A copy of this client with a different token in play (used by
    /// `--token`).
    pub fn with_token(&self, token: &str) -> Auth {
        let mut store = self.store.clone();
        if !token.is_empty() {
            let _ = store.add_token(&self.url(), token);
        }
        Auth {
            registry: self.registry.clone(),
            store,
        }
    }
}

fn expect_ok(resp: HttpResponse, what: &str) -> Result<HttpResponse, String> {
    if resp.is_success() {
        return Ok(resp);
    }
    Err(format!("{what}: {}", Registry::error_message(&resp)))
}

fn parse_login(resp: &HttpResponse) -> Login {
    let j = match resp.json() {
        Some(j) => j,
        None => return Login::default(),
    };
    Login {
        token: str_of(&j, "token").unwrap_or_default(),
        id: str_of(&j, "id").unwrap_or_default(),
        user: str_of(&j, "user").unwrap_or_default(),
        scopes: strings_of(&j, "scopes"),
    }
}

fn parse_token(j: &Json) -> TokenInfo {
    TokenInfo {
        id: str_of(j, "id").unwrap_or_default(),
        user: str_of(j, "user").unwrap_or_default(),
        name: str_of(j, "name").unwrap_or_default(),
        scopes: strings_of(j, "scopes"),
        created_at: j.get("created_at").and_then(|v| v.as_num()).unwrap_or(0),
        last_used_at: j.get("last_used_at").and_then(|v| v.as_num()),
        revoked: j
            .get("revoked")
            .map(|v| matches!(v, Json::Bool(true)))
            .unwrap_or(false),
    }
}

fn str_of(j: &Json, key: &str) -> Option<String> {
    j.get(key).and_then(|v| v.as_str()).map(String::from)
}

fn strings_of(j: &Json, key: &str) -> Vec<String> {
    match j.get(key) {
        Some(Json::Arr(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Registry, RegistryConfig};
    use std::path::PathBuf;

    fn store(tag: &str) -> (CredentialStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hs-auth-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (CredentialStore::at(dir.join("credentials.toml")), dir)
    }

    fn client(tag: &str) -> (Auth, PathBuf) {
        let (s, dir) = store(tag);
        let registry = Registry::new(RegistryConfig::local("http://127.0.0.1:1"));
        (Auth::new(registry, s), dir)
    }

    fn json(text: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: text.as_bytes().to_vec(),
            headers: Vec::new(),
        }
    }

    #[test]
    fn login_parses_the_session() {
        let (_a, dir) = client("login");
        let resp = json(
            r#"{"token":"hssess_abc","id":"tok_1","user":"ada","scopes":["read","publish"]}"#,
        );
        let login = parse_login(&resp);
        assert_eq!(login.token, "hssess_abc");
        assert_eq!(login.id, "tok_1");
        assert_eq!(login.user, "ada");
        assert_eq!(login.scopes, vec!["read", "publish"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tokens_are_parsed_without_the_plaintext() {
        let j = hs_compiler::json::parse(
            r#"{"id":"tok_1","user":"ada","name":"ci","scopes":["read"],"created_at":10,"last_used_at":20}"#,
        )
        .unwrap();
        let t = parse_token(&j);
        assert_eq!(t.id, "tok_1");
        assert_eq!(t.name, "ci");
        assert_eq!(t.created_at, 10);
        assert_eq!(t.last_used_at, Some(20));
        assert!(!t.revoked);
    }

    #[test]
    fn whoami_fields_are_read() {
        let j = hs_compiler::json::parse(
            r#"{"user":"ada","token_id":"tok_1","name":"ci","scopes":["read","yank"],"created_at":5,"last_used_at":null}"#,
        )
        .unwrap();
        let w = Whoami {
            user: str_of(&j, "user").unwrap(),
            token_id: str_of(&j, "token_id").unwrap(),
            name: str_of(&j, "name").unwrap(),
            scopes: strings_of(&j, "scopes"),
            created_at: j.get("created_at").and_then(|v| v.as_num()).unwrap(),
            last_used_at: j.get("last_used_at").and_then(|v| v.as_num()),
        };
        assert_eq!(w.user, "ada");
        assert_eq!(w.scopes, vec!["read", "yank"]);
        assert_eq!(w.last_used_at, None);
    }

    #[test]
    fn a_registry_url_is_normalized() {
        let (a, dir) = client("url");
        assert_eq!(a.url(), "http://127.0.0.1:1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn describing_a_client_never_leaks_the_token() {
        let (mut a, dir) = client("describe");
        a.store
            .login(&a.url(), "ada", "hspat_0123456789abcdef")
            .unwrap();
        let text = a.describe();
        assert!(text.contains("ada"), "{text}");
        assert!(!text.contains("0123456789abcdef"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commands_refuse_without_a_stored_token() {
        let (a, dir) = client("anon");
        // the registry is unreachable in these tests, but the guard must fire
        // first
        assert!(a.list_tokens().unwrap_err().contains("not logged in"));
        assert!(a.whoami().unwrap_err().contains("not logged in"));
        assert!(a
            .create_token("ci", &[])
            .unwrap_err()
            .contains("not logged in"));
        assert!(a.revoke_token("tok_1").unwrap_err().contains("not logged in"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn login_requires_a_user() {
        let (a, dir) = client("nouser");
        assert!(a.login("", "secret").is_err());
        assert!(a.login("  ", "secret").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_bodies_are_surfaced_verbatim() {
        let resp = HttpResponse {
            status: 401,
            body: br#"{"error":{"code":"unauthorized","message":"invalid credentials"}}"#.to_vec(),
            headers: Vec::new(),
        };
        let err = expect_ok(resp, "login").unwrap_err();
        assert!(err.contains("invalid credentials"), "{err}");
        assert!(err.starts_with("login: "), "{err}");
    }

    #[test]
    fn escaping_is_applied_to_credentials() {
        assert_eq!(escape("a\"b"), "a\\\"b");
        assert_eq!(escape("a\\b"), "a\\\\b");
        let body = format!("{{\"user\":\"{}\"}}", escape("od\"d"));
        assert_eq!(body, "{\"user\":\"od\\\"d\"}");
    }
}
