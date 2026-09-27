//! Authentication: password checks, session minting and token hashing.
//!
//! One rule shapes this module: **the registry never stores a secret it can
//! replay.** Passwords are stored as PBKDF2-HMAC-SHA256 records, tokens are
//! stored as SHA-256 hashes, and the plaintext of a personal access token is
//! returned exactly once — in the response to the request that created it.
//! A database leak therefore yields nothing an attacker can use.

use crate::model::{now_secs, Scope, Token};
use crate::security;
use std::time::Duration;

/// How long a login session stays valid.
pub const SESSION_TTL_SECS: i64 = 24 * 60 * 60;

/// A freshly minted secret plus the metadata the store keeps about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionToken {
    /// Public identifier of the stored record.
    pub id: String,
    /// The plaintext, returned to the client once.
    pub plaintext: String,
    /// When the record stops being accepted.
    pub expires_at: i64,
}

impl SessionToken {
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }
}

/// Hash a token plaintext for storage.
pub fn hash_token(plaintext: &str) -> String {
    security::sha256_hex(plaintext.trim().as_bytes())
}

/// The public id of a token: the first 16 hex characters of its hash. Short
/// enough to quote in a shell, wide enough to be unguessable in context.
pub fn token_id(plaintext: &str) -> String {
    let h = hash_token(plaintext);
    format!("tok_{}", &h[..16])
}

/// Mint a login session: a token with the default scopes and a TTL.
pub fn new_session(user: &str, config: &crate::app::Config) -> SessionToken {
    new_session_with_ttl(user, config, SESSION_TTL_SECS)
}

/// Mint a login session with an explicit TTL (used by tests).
pub fn new_session_with_ttl(_user: &str, _config: &crate::app::Config, ttl: i64) -> SessionToken {
    let plaintext = security::random_token("hssess");
    SessionToken {
        id: token_id(&plaintext),
        plaintext,
        expires_at: now_secs() + ttl,
    }
}

/// Build the stored record for a session.
pub fn session_record(user: &str, session: &SessionToken) -> Token {
    Token {
        id: session.id.clone(),
        user: user.to_string(),
        name: "session".to_string(),
        token_hash: hash_token(&session.plaintext),
        scopes: vec![Scope::Read, Scope::Publish, Scope::Yank, Scope::Token],
        created_at: now_secs(),
        last_used_at: None,
        revoked: false,
    }
}

/// A coarse password strength verdict, used to warn at registration time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strength {
    Weak,
    Fair,
    Good,
    Strong,
}

impl Strength {
    pub fn as_str(self) -> &'static str {
        match self {
            Strength::Weak => "weak",
            Strength::Fair => "fair",
            Strength::Good => "good",
            Strength::Strong => "strong",
        }
    }
}

/// Rate password strength: length dominates, character classes refine it.
pub fn password_strength(pw: &str) -> Strength {
    let n = pw.chars().count();
    let classes = [
        pw.chars().any(|c| c.is_ascii_lowercase()),
        pw.chars().any(|c| c.is_ascii_uppercase()),
        pw.chars().any(|c| c.is_ascii_digit()),
        pw.chars().any(|c| !c.is_alphanumeric()),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    let score = n + classes * 2;
    match score {
        0..=9 => Strength::Weak,
        10..=13 => Strength::Fair,
        14..=17 => Strength::Good,
        _ => Strength::Strong,
    }
}

/// Backoff for repeated failed logins against one account.
#[derive(Clone, Copy, Debug, Default)]
pub struct Backoff {
    pub failures: u32,
    pub wait: Duration,
}

impl Backoff {
    /// Record a failure and return how long to wait before the next attempt.
    pub fn fail(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        // 1s, 2s, 4s ... capped at 64s.
        self.wait = Duration::from_secs(1u64 << self.failures.min(7).saturating_sub(1));
        self.wait
    }

    /// Clear the backoff after a success.
    pub fn reset(&mut self) {
        self.failures = 0;
        self.wait = Duration::from_secs(0);
    }

    pub fn is_blocked(&self) -> bool {
        self.wait > Duration::from_secs(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_hash_is_stable_and_id_is_short() {
        let t = "hspat_0123456789abcdef";
        assert_eq!(hash_token(t), hash_token(&format!("  {t} ")));
        let id = token_id(t);
        assert!(id.starts_with("tok_"));
        assert_eq!(id.len(), 20);
        assert_ne!(id, token_id("hspat_other"));
    }

    #[test]
    fn sessions_carry_a_ttl() {
        let cfg = crate::app::Config::default();
        let s = new_session("ada", &cfg);
        assert!(s.plaintext.starts_with("hssess_"));
        assert!(!s.is_expired(now_secs()));
        assert!(s.is_expired(s.expires_at));
        let rec = session_record("ada", &s);
        assert_eq!(rec.user, "ada");
        assert_eq!(rec.name, "session");
        assert!(rec.scopes.contains(&Scope::Token));
        let short = new_session_with_ttl("ada", &cfg, -1);
        assert!(short.is_expired(now_secs()));
    }

    #[test]
    fn strength_ranking() {
        assert_eq!(password_strength("abc"), Strength::Weak);
        assert!(password_strength("correct-horse-battery") > password_strength("abcdefgh"));
        assert_eq!(password_strength("aB3$xY9!kL2m"), Strength::Strong);
    }

    #[test]
    fn backoff_grows_then_resets() {
        let mut b = Backoff::default();
        assert!(!b.is_blocked());
        let first = b.fail();
        assert_eq!(first, Duration::from_secs(1));
        let second = b.fail();
        assert_eq!(second, Duration::from_secs(2));
        assert!(b.is_blocked());
        b.reset();
        assert!(!b.is_blocked());
        // capped
        for _ in 0..20 {
            b.fail();
        }
        assert_eq!(b.wait, Duration::from_secs(64));
    }
}
