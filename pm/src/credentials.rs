//! Registry credentials: `hard login`, `hard logout`, `hard token`.
//!
//! Tokens are stored per registry in `$HARD_HOME/credentials.toml`, written
//! with owner-only permissions. Three rules shape this module:
//!
//! 1. **Never write a token the wrong way.** The file is created 0600 and
//!    written through a temporary file plus rename, so a token is never
//!    briefly world-readable and a crash never truncates the store.
//! 2. **Environment wins.** `HARD_TOKEN` overrides the file, which is what CI
//!    and one-off commands want; the file is what a developer's machine
//!    wants.
//! 3. **Nothing is logged.** Neither this module nor the CLI prints a token
//!    after the moment it is created.
//!
//! ```toml
//! # $HARD_HOME/credentials.toml
//! [registry."https://registry.hardscript.org"]
//! token = "hspat_..."
//! user = "ada"
//! # tokens = ["hspat_..."]   # extra tokens minted for the same registry
//! ```

use crate::toml::{self, TomlValue};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The environment variable that overrides every stored token.
pub const TOKEN_ENV: &str = "HARD_TOKEN";

/// The file name inside `HARD_HOME`.
pub const FILE_NAME: &str = "credentials.toml";

/// What is known about one registry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    /// The active token.
    pub token: Option<String>,
    /// The account the token belongs to.
    pub user: Option<String>,
    /// When it was stored (unix seconds).
    pub stored_at: i64,
    /// Additional tokens for the same registry, e.g. one per machine.
    pub tokens: Vec<String>,
}

impl Credentials {
    pub fn is_logged_in(&self) -> bool {
        self.token.as_ref().map(|t| !t.is_empty()).unwrap_or(false)
    }

    /// Redacted for display: never the secret itself.
    pub fn redacted(&self) -> String {
        match (&self.user, &self.token) {
            (Some(u), Some(t)) => format!("{u} ({}…{})", &t[..t.len().min(6)], &t[t.len().saturating_sub(4)..]),
            (Some(u), None) => format!("{u} (no token)"),
            (None, Some(t)) => format!("{}…{}", &t[..t.len().min(6)], &t[t.len().saturating_sub(4)..]),
            (None, None) => "not logged in".to_string(),
        }
    }
}

/// The credential store: every registry the user is logged into.
#[derive(Clone, Debug, Default)]
pub struct CredentialStore {
    path: PathBuf,
    /// `registry url -> credentials`, keyed by the URL without a trailing `/`.
    entries: BTreeMap<String, Credentials>,
}

impl CredentialStore {
    /// The store for the current `HARD_HOME`.
    pub fn open() -> CredentialStore {
        CredentialStore::at(crate::cache::hard_home().join(FILE_NAME))
    }

    /// The store at an explicit path (tests, containers).
    pub fn at(path: PathBuf) -> CredentialStore {
        let mut store = CredentialStore {
            path,
            entries: BTreeMap::new(),
        };
        store.load();
        store
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every registry with stored credentials, in URL order.
    pub fn registries(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Credentials for one registry.
    pub fn get(&self, registry: &str) -> Option<&Credentials> {
        self.entries.get(&normalize(registry))
    }

    /// The token to present to `registry`: `HARD_TOKEN` wins, then the file.
    ///
    /// The empty string is returned rather than `None` so callers can pass it
    /// straight into an `Authorization` header.
    pub fn token_for(&self, registry: &str) -> String {
        if let Ok(t) = std::env::var(TOKEN_ENV) {
            if !t.trim().is_empty() {
                return t.trim().to_string();
            }
        }
        self.get(registry)
            .and_then(|c| c.token.clone())
            .unwrap_or_default()
    }

    /// Is there a token for `registry` (from the file, or the environment)?
    pub fn is_logged_in(&self, registry: &str) -> bool {
        !self.token_for(registry).is_empty()
    }

    /// Store a token, replacing any previous one for the registry.
    pub fn login(&mut self, registry: &str, user: &str, token: &str) -> Result<(), String> {
        if token.trim().is_empty() {
            return Err("refusing to store an empty token".to_string());
        }
        let key = normalize(registry);
        let entry = self.entries.entry(key).or_default();
        entry.token = Some(token.trim().to_string());
        entry.user = Some(user.to_string());
        entry.stored_at = now_secs();
        self.save()
    }

    /// Forget one registry's credentials.
    pub fn logout(&mut self, registry: &str) -> Result<bool, String> {
        let key = normalize(registry);
        let removed = self.entries.remove(&key).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Forget every registry.
    pub fn logout_all(&mut self) -> Result<usize, String> {
        let n = self.entries.len();
        self.entries.clear();
        self.save()?;
        Ok(n)
    }

    /// Add an extra token for a registry (a second machine, say).
    pub fn add_token(&mut self, registry: &str, token: &str) -> Result<(), String> {
        if token.trim().is_empty() {
            return Err("refusing to store an empty token".to_string());
        }
        let entry = self.entries.entry(normalize(registry)).or_default();
        let t = token.trim().to_string();
        if entry.token.as_deref() == Some(t.as_str()) {
            return Ok(());
        }
        if !entry.tokens.contains(&t) {
            entry.tokens.push(t);
        }
        self.save()
    }

    /// Parse a credentials file without touching the filesystem. Returns an
    /// empty store when the text is not credentials at all.
    pub fn parse(text: &str) -> CredentialStore {
        let mut store = CredentialStore {
            path: PathBuf::new(),
            entries: BTreeMap::new(),
        };
        let Ok(doc) = toml::parse(text) else {
            return store;
        };
        let Some(table) = doc.table("registry") else {
            return store;
        };
        for (url, value) in table {
            let Some(t) = value.as_table() else { continue };
            let creds = Credentials {
                token: t.get("token").and_then(|v| v.as_str()).map(String::from),
                user: t.get("user").and_then(|v| v.as_str()).map(String::from),
                stored_at: t.get("stored_at").and_then(|v| v.as_int()).unwrap_or(0) as i64,
                tokens: t
                    .get("tokens")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|i| i.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
            };
            store.entries.insert(normalize(url), creds);
        }
        store
    }

    /// Render the store back to TOML. Deterministic: registries sorted, keys
    /// in a fixed order, so the file only changes when the data does.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("# hard credentials — written by `hard login`.\n");
        out.push_str("# Each token is a secret: keep this file private (mode 0600).\n");
        for (url, c) in &self.entries {
            out.push_str(&format!("\n[registry.\"{url}\"]\n"));
            if let Some(t) = &c.token {
                out.push_str(&format!("token = \"{}\"\n", escape(t)));
            }
            if let Some(u) = &c.user {
                out.push_str(&format!("user = \"{}\"\n", escape(u)));
            }
            if c.stored_at > 0 {
                out.push_str(&format!("stored_at = {}\n", c.stored_at));
            }
            if !c.tokens.is_empty() {
                let list: Vec<String> = c.tokens.iter().map(|t| format!("\"{}\"", escape(t))).collect();
                out.push_str(&format!("tokens = [{}]\n", list.join(", ")));
            }
        }
        out
    }

    /// Write the store, owner-readable only.
    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let text = self.render();
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        restrict(&tmp);
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("cannot rename into {}: {e}", self.path.display()))?;
        restrict(&self.path);
        Ok(())
    }

    /// (Re)read the file.
    pub fn load(&mut self) -> bool {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                let parsed = CredentialStore::parse(&text);
                self.entries = parsed.entries;
                true
            }
            Err(_) => false,
        }
    }

    /// True when the file exists and is not readable by anybody else.
    pub fn is_private(&self) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::metadata(&self.path) {
                Ok(m) => m.permissions().mode() & 0o077 == 0,
                Err(_) => false,
            }
        }
        #[cfg(not(unix))]
        {
            true
        }
    }
}

/// A registry URL without a trailing slash, so `http://x/` and `http://x`
/// are the same registry.
pub fn normalize(registry: &str) -> String {
    registry.trim().trim_end_matches('/').to_string()
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// A one-line `registry user (tok…ate)` summary for `hard whoami`.
pub fn describe(registry: &str, creds: Option<&Credentials>) -> String {
    match creds {
        Some(c) => format!("{registry}: {}", c.redacted()),
        None => format!("{registry}: not logged in"),
    }
}

/// Scope names a token may carry, for `hard token create --scope`.
pub const SCOPES: &[&str] = &["read", "publish", "yank", "token", "admin"];

/// Parse and validate a `--scope` list.
pub fn parse_scopes(raw: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for r in raw {
        for part in r.split(',') {
            let p = part.trim().to_ascii_lowercase();
            if p.is_empty() {
                continue;
            }
            if !SCOPES.contains(&p.as_str()) {
                return Err(format!(
                    "unknown scope '{p}' (valid: {})",
                    SCOPES.join(", ")
                ));
            }
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Whether a JSON-ish value is a bare string (used when reading a token out
/// of a command's stdout).
pub fn as_token(value: &TomlValue) -> Option<String> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("hs-cred-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn login_then_read_back() {
        let dir = temp("login");
        let path = dir.join("credentials.toml");
        let mut store = CredentialStore::at(path.clone());
        assert!(!store.is_logged_in("http://example.org"));
        store.login("http://example.org", "ada", "hspat_abc").unwrap();
        assert!(store.is_logged_in("http://example.org"));
        let reopened = CredentialStore::at(path);
        assert_eq!(
            reopened.get("http://example.org").unwrap().token.as_deref(),
            Some("hspat_abc")
        );
        assert_eq!(
            reopened.get("http://example.org").unwrap().user.as_deref(),
            Some("ada")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_is_owner_only() {
        let dir = temp("perm");
        let path = dir.join("credentials.toml");
        let mut store = CredentialStore::at(path.clone());
        store.login("http://example.org", "ada", "hspat_abc").unwrap();
        assert!(store.is_private(), "the credentials file must not be readable by others");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode was {mode:o}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trailing_slashes_do_not_create_a_second_entry() {
        let dir = temp("slash");
        let mut store = CredentialStore::at(dir.join("credentials.toml"));
        store.login("http://example.org/", "ada", "hspat_abc").unwrap();
        assert!(store.is_logged_in("http://example.org"));
        assert_eq!(store.registries(), vec!["http://example.org".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn login_replaces_the_previous_token() {
        let dir = temp("replace");
        let path = dir.join("credentials.toml");
        let mut store = CredentialStore::at(path.clone());
        store.login("http://example.org", "ada", "hspat_one").unwrap();
        store.login("http://example.org", "ada", "hspat_two").unwrap();
        let reopened = CredentialStore::at(path);
        assert_eq!(
            reopened.get("http://example.org").unwrap().token.as_deref(),
            Some("hspat_two")
        );
        assert_eq!(reopened.registries().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logout_removes_only_the_named_registry() {
        let dir = temp("logout");
        let path = dir.join("credentials.toml");
        let mut store = CredentialStore::at(path.clone());
        store.login("http://a.example", "ada", "hspat_a").unwrap();
        store.login("http://b.example", "bo", "hspat_b").unwrap();
        assert!(store.logout("http://a.example").unwrap());
        assert!(!store.logout("http://a.example").unwrap(), "already gone");
        let reopened = CredentialStore::at(path);
        assert!(reopened.get("http://a.example").is_none());
        assert!(reopened.get("http://b.example").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logout_all_clears_everything() {
        let dir = temp("logoutall");
        let path = dir.join("credentials.toml");
        let mut store = CredentialStore::at(path.clone());
        store.login("http://a.example", "ada", "hspat_a").unwrap();
        store.login("http://b.example", "bo", "hspat_b").unwrap();
        assert_eq!(store.logout_all().unwrap(), 2);
        assert!(CredentialStore::at(path).registries().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_token_is_refused() {
        let dir = temp("empty");
        let mut store = CredentialStore::at(dir.join("credentials.toml"));
        assert!(store.login("http://example.org", "ada", "").is_err());
        assert!(store.login("http://example.org", "ada", "   ").is_err());
        assert!(store.add_token("http://example.org", "").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extra_tokens_are_kept_separately() {
        let dir = temp("extra");
        let path = dir.join("credentials.toml");
        let mut store = CredentialStore::at(path.clone());
        store.login("http://example.org", "ada", "hspat_main").unwrap();
        store.add_token("http://example.org", "hspat_ci").unwrap();
        store.add_token("http://example.org", "hspat_ci").unwrap();
        let reopened = CredentialStore::at(path);
        let c = reopened.get("http://example.org").unwrap();
        assert_eq!(c.token.as_deref(), Some("hspat_main"));
        assert_eq!(c.tokens, vec!["hspat_ci".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_rendered_file_is_stable_and_parses_back() {
        let dir = temp("render");
        let mut store = CredentialStore::at(dir.join("credentials.toml"));
        store.login("http://b.example", "bo", "hspat_b").unwrap();
        store.login("http://a.example", "ada", "hspat_a").unwrap();
        let one = store.render();
        let two = store.render();
        assert_eq!(one, two, "rendering must be deterministic");
        // sorted by url
        assert!(one.find("a.example").unwrap() < one.find("b.example").unwrap());
        let parsed = CredentialStore::parse(&one);
        assert_eq!(parsed.registries().len(), 2);
        assert_eq!(
            parsed.get("http://a.example").unwrap().token.as_deref(),
            Some("hspat_a")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_files_parse_to_an_empty_store() {
        for text in ["", "not toml [", "[other]\nx = 1", "registry = 5"] {
            let store = CredentialStore::parse(text);
            assert!(store.registries().is_empty(), "{text:?} produced {:?}", store.registries());
        }
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = temp("missing");
        let store = CredentialStore::at(dir.join("nope/credentials.toml"));
        assert!(store.registries().is_empty());
        assert!(!store.is_logged_in("http://example.org"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redaction_never_shows_the_whole_token() {
        let c = Credentials {
            token: Some("hspat_0123456789abcdef".to_string()),
            user: Some("ada".to_string()),
            ..Credentials::default()
        };
        let text = c.redacted();
        assert!(text.contains("ada"), "{text}");
        assert!(!text.contains("0123456789abcdef"), "{text}");
        assert!(c.is_logged_in());
        let anon = Credentials::default();
        assert!(!anon.is_logged_in());
        assert_eq!(anon.redacted(), "not logged in");
    }

    #[test]
    fn describe_handles_both_cases() {
        let c = Credentials {
            token: Some("hspat_abcdef".to_string()),
            user: Some("ada".to_string()),
            ..Credentials::default()
        };
        assert!(describe("http://x", Some(&c)).contains("ada"));
        assert!(describe("http://x", None).contains("not logged in"));
    }

    #[test]
    fn scopes_are_validated_and_deduped() {
        assert_eq!(
            parse_scopes(&["publish".to_string(), "read,publish".to_string()]).unwrap(),
            vec!["publish".to_string(), "read".to_string()]
        );
        let err = parse_scopes(&["nope".to_string()]).unwrap_err();
        assert!(err.contains("unknown scope"), "{err}");
        assert!(parse_scopes(&["".to_string()]).unwrap().is_empty());
    }

    #[test]
    fn token_extraction_trims_and_rejects_empty() {
        assert_eq!(
            as_token(&TomlValue::Str(" hspat_x ".to_string())),
            Some("hspat_x".to_string())
        );
        assert_eq!(as_token(&TomlValue::Str("   ".to_string())), None);
        assert_eq!(as_token(&TomlValue::Int(1)), None);
    }

    #[test]
    fn normalize_strips_whitespace_and_trailing_slashes() {
        assert_eq!(normalize(" http://x/ "), "http://x");
        assert_eq!(normalize("http://x"), "http://x");
    }
}
