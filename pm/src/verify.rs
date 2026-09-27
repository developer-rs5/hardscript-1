//! Package signature verification and the client trust store.
//!
//! A registry signs every publish with Ed25519 over a small canonical payload
//! (see [`signature_payload`]). This module is the *consumer* half of that
//! scheme: it re-derives the payload, checks the signature against a key the
//! user has actually pinned, and turns the result into one of three verdicts
//! depending on policy.
//!
//! # Why the payload is what it is
//!
//! The signed text binds four things and nothing else:
//!
//! ```text
//! hs-signature/1
//! name jwt
//! version 1.0.0
//! integrity sha256:<digest of the .hspkg>
//! fingerprint sha256:<canonical manifest fingerprint>
//! ```
//!
//! Deliberately absent: timestamps, download counts, transport headers, the
//! publisher's IP. Two mirrors serving the same version produce the same
//! signature, and re-publishing metadata that does not change the package
//! identity does not change the signature.
//!
//! # Trust
//!
//! A signature proves *who* signed something, not that you should trust them.
//! That is what the trust store is for: `hard keys add` records a key it has
//! seen, `hard keys trust <id>` pins it, and only pinned keys count as trusted
//! under [`VerifyPolicy::Strict`]. Keys may also be scoped to one package name,
//! which is what `hard keys add --for jwt` is for.
//!
//! # Where this lives
//!
//! ```text
//! $HARD_HOME/trusted-keys.toml
//! ```

use crate::pkgfmt::sha256_hex;
use crate::registry::{HttpResponse, Method, Registry, RegistryError};
use ed25519_dalek::{Signature, Signer as _, Verifier as _, VerifyingKey};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The signed payload format. Bumping this changes every signature.
pub const SIGNATURE_VERSION: &str = "hs-signature/1";

/// The signature algorithm. Only Ed25519 is accepted; an unknown algorithm is
/// a failure, never a "maybe".
pub const SIGNATURE_ALGORITHM: &str = "ed25519";

/// How much a bad or unknown signature should cost the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyPolicy {
    /// Do not look at signatures at all.
    Off,
    /// Verify, and print a warning when verification does not succeed.
    Warn,
    /// Verify, and fail the command when it does not succeed.
    Strict,
}

impl Default for VerifyPolicy {
    fn default() -> VerifyPolicy {
        VerifyPolicy::Warn
    }
}

impl VerifyPolicy {
    /// Parse `strict`, `warn` or `off` (case-insensitive).
    pub fn parse(raw: &str) -> Result<VerifyPolicy, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "skip" => Ok(VerifyPolicy::Off),
            "warn" | "warning" => Ok(VerifyPolicy::Warn),
            "strict" => Ok(VerifyPolicy::Strict),
            other => Err(format!(
                "unknown verification mode '{other}' (expected strict, warn or off)"
            )),
        }
    }

    /// The value accepted by [`VerifyPolicy::parse`].
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyPolicy::Off => "off",
            VerifyPolicy::Warn => "warn",
            VerifyPolicy::Strict => "strict",
        }
    }

    /// Does a failed verification stop the command?
    pub fn is_fatal(self) -> bool {
        self == VerifyPolicy::Strict
    }

    /// Read `--verify=<mode>`, falling back to `HARD_VERIFY`, then the default.
    ///
    /// A bad value is an error rather than a silent downgrade to `warn`:
    /// someone who asked for `strict` must never quietly get `warn`.
    pub fn from_flags(flag: Option<&str>) -> Result<VerifyPolicy, String> {
        if let Some(v) = flag {
            return VerifyPolicy::parse(v);
        }
        match std::env::var("HARD_VERIFY") {
            Ok(v) if !v.trim().is_empty() => VerifyPolicy::parse(&v),
            _ => Ok(VerifyPolicy::default()),
        }
    }
}

impl std::fmt::Display for VerifyPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// -- canonical payload ------------------------------------------------------

/// Normalize an integrity string: `sha256:<hex>`, a bare digest, an uppercase
/// `SHA256:` prefix and uppercase hex all reduce to the same text, so a
/// signature cannot be invalidated by how a client happened to spell it.
pub fn normalize_integrity(raw: &str) -> String {
    let t = raw.trim();
    let hexpart = if t.len() >= 7 && t[..7].eq_ignore_ascii_case("sha256:") {
        &t[7..]
    } else {
        t
    };
    format!("sha256:{}", hexpart.to_ascii_lowercase())
}

/// The exact text a registry signs for one version.
///
/// The registry calls this function too (`hard_registry::signing::payload`
/// delegates here) so the two can never drift apart.
pub fn signature_payload(name: &str, version: &str, integrity: &str, fingerprint: &str) -> String {
    format!(
        "{SIGNATURE_VERSION}\nname {name}\nversion {version}\nintegrity {}\nfingerprint {}\n",
        normalize_integrity(integrity),
        normalize_integrity(fingerprint),
    )
}

/// The SHA-256 of the canonical payload, for display and for equality checks.
pub fn payload_hash(payload: &str) -> String {
    format!("sha256:{}", sha256_hex(payload.as_bytes()))
}

// -- primitives ------------------------------------------------------------

/// Hex-encode bytes.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Decode a hex string, ignoring case and an optional `sha256:` prefix.
pub fn unhex(raw: &str) -> Option<Vec<u8>> {
    let t = raw.trim();
    let t = t.strip_prefix("sha256:").unwrap_or(t);
    if t.is_empty() || t.len() % 2 != 0 {
        return None;
    }
    let b = t.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Decode standard base64. Whitespace is ignored; anything else is rejected.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let cleaned: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if cleaned.len() % 4 != 0 {
        return None;
    }
    let val = |c: u8| -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    let mut i = 0;
    while i < cleaned.len() {
        let quad = &cleaned[i..i + 4];
        let last = i + 4 == cleaned.len();
        let mut acc = 0u32;
        let mut pad = 0usize;
        for (j, &c) in quad.iter().enumerate() {
            if c == b'=' {
                if !last || j < 2 {
                    return None;
                }
                pad += 1;
                acc <<= 6;
            } else {
                if pad > 0 {
                    return None;
                }
                acc = (acc << 6) | val(c)? as u32;
            }
        }
        out.push((acc >> 16) as u8);
        if pad < 2 {
            out.push((acc >> 8) as u8);
        }
        if pad < 1 {
            out.push(acc as u8);
        }
        i += 4;
    }
    Some(out)
}

/// The key id a public key hashes to: `k:` plus the first 16 hex characters.
pub fn key_id_of(public_hex: &str) -> Option<String> {
    let pk = unhex(public_hex)?;
    if pk.len() != 32 {
        return None;
    }
    Some(format!("k:{}", &hex(&pk)[..16]))
}

/// True when a public key is well formed (32 bytes, on the curve).
pub fn is_valid_public_key(public_hex: &str) -> bool {
    public_key(public_hex).is_some()
}

fn public_key(public_hex: &str) -> Option<VerifyingKey> {
    let pk = unhex(public_hex)?;
    let bytes: [u8; 32] = pk.clone().try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Verify a base64 signature over `payload_text` with an explicit public key.
pub fn verify_with_public_key(public_hex: &str, payload_text: &str, signature_b64: &str) -> bool {
    let Some(vk) = public_key(public_hex) else {
        return false;
    };
    let Some(raw) = base64_decode(signature_b64) else {
        return false;
    };
    let Ok(sig) = <[u8; 64]>::try_from(raw.as_slice()) else {
        return false;
    };
    vk.verify(payload_text.as_bytes(), &Signature::from_bytes(&sig))
        .is_ok()
}

/// Sign a payload with a 32-byte seed. Used by tests and by `hard keys`
/// fixtures; a client never signs anything it publishes to a registry.
pub fn sign_with_seed(seed: &[u8; 32], payload_text: &str) -> String {
    let sk = ed25519_dalek::SigningKey::from_bytes(seed);
    base64_encode(&sk.sign(payload_text.as_bytes()).to_bytes())
}

/// The public key (hex) for a 32-byte seed.
pub fn public_key_of_seed(seed: &[u8; 32]) -> String {
    hex(&ed25519_dalek::SigningKey::from_bytes(seed).verifying_key().to_bytes())
}

// -- signature records -----------------------------------------------------

/// What a registry says about one version's signature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SignatureRecord {
    pub name: String,
    pub version: String,
    pub integrity: String,
    pub fingerprint: String,
    pub signature: String,
    pub key_id: String,
    pub public_key: String,
    pub algorithm: String,
}

impl SignatureRecord {
    /// The payload a signature over this record must cover.
    pub fn payload(&self) -> String {
        signature_payload(&self.name, &self.version, &self.integrity, &self.fingerprint)
    }

    /// The digest of that payload.
    pub fn payload_hash(&self) -> String {
        payload_hash(&self.payload())
    }

    /// Is the record complete enough to verify?
    pub fn is_complete(&self) -> bool {
        !self.signature.is_empty()
            && !self.key_id.is_empty()
            && !self.public_key.is_empty()
            && !self.integrity.is_empty()
            && !self.fingerprint.is_empty()
    }

    /// Parse the JSON of `GET /packages/{name}/{version}/signature`.
    pub fn from_json(j: &hs_compiler::json::Json) -> SignatureRecord {
        let s = |k: &str| {
            j.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        SignatureRecord {
            name: s("name"),
            version: s("version"),
            integrity: s("integrity"),
            fingerprint: s("fingerprint"),
            signature: s("signature"),
            key_id: s("key_id"),
            public_key: s("public_key"),
            algorithm: {
                let a = s("algorithm");
                if a.is_empty() {
                    SIGNATURE_ALGORITHM.to_string()
                } else {
                    a
                }
            },
        }
    }

    /// Serialize for `--json`, for the `.sig` sidecar and for lockfiles.
    pub fn to_json(&self) -> hs_compiler::json::Json {
        use hs_compiler::json::Json;
        Json::obj(vec![
            ("name", Json::str(&self.name)),
            ("version", Json::str(&self.version)),
            ("integrity", Json::str(&self.integrity)),
            ("fingerprint", Json::str(&self.fingerprint)),
            ("signature", Json::str(&self.signature)),
            ("key_id", Json::str(&self.key_id)),
            ("public_key", Json::str(&self.public_key)),
            ("algorithm", Json::str(&self.algorithm)),
            ("payload_format", Json::str(SIGNATURE_VERSION)),
            ("payload_hash", Json::str(&self.payload_hash())),
        ])
    }

    /// Parse a `.sig` sidecar.
    pub fn from_sidecar(text: &str) -> Result<SignatureRecord, String> {
        let j = hs_compiler::json::parse(text)
            .ok_or_else(|| "the sidecar is not valid JSON".to_string())?;
        Ok(SignatureRecord::from_json(&j))
    }

    /// Fetch the record for one version from a registry.
    pub fn fetch(
        registry: &Registry,
        name: &str,
        version: &str,
    ) -> Result<SignatureRecord, RegistryError> {
        let path = format!("/packages/{name}/{version}/signature");
        let resp = registry.request(Method::Get, &path, Vec::new(), &[], None)?;
        signature_from_response(&resp)
    }
}

/// Turn a signature response into a record, or an error worth printing.
pub fn signature_from_response(resp: &HttpResponse) -> Result<SignatureRecord, RegistryError> {
    if !resp.is_success() {
        return Err(RegistryError::other(Registry::error_message(&resp)));
    }
    let j = resp
        .json()
        .ok_or_else(|| RegistryError::other("the registry returned a body that is not JSON"))?;
    Ok(SignatureRecord::from_json(&j))
}

/// A registry's current signing key, as served by `GET /keys`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistryKey {
    pub key_id: String,
    pub public_key: String,
    pub algorithm: String,
    pub payload_format: String,
    pub test_key: bool,
}

impl RegistryKey {
    pub fn from_json(j: &hs_compiler::json::Json) -> RegistryKey {
        let s = |k: &str| {
            j.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        RegistryKey {
            key_id: s("key_id"),
            public_key: s("public_key"),
            algorithm: s("algorithm"),
            payload_format: s("payload_format"),
            test_key: j.get("test_key") == Some(&hs_compiler::json::Json::Bool(true)),
        }
    }

    /// Fetch the registry's key. `None` when the registry has no `/keys`.
    pub fn fetch(registry: &Registry) -> Result<RegistryKey, RegistryError> {
        let resp = registry.request(Method::Get, "/keys", Vec::new(), &[], None)?;
        if !resp.is_success() {
            return Err(RegistryError::other(Registry::error_message(&resp)));
        }
        let j = resp
            .json()
            .ok_or_else(|| RegistryError::other("the registry returned a body that is not JSON"))?;
        Ok(RegistryKey::from_json(&j))
    }
}

// -- trust store -----------------------------------------------------------

/// One key the client knows about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedKey {
    pub key_id: String,
    pub public_key: String,
    /// Free-form note, usually where the key came from.
    pub label: String,
    /// `true` once the user has pinned it. Untrusted keys are remembered but
    /// do not satisfy strict verification.
    pub trusted: bool,
    /// Restrict trust to these package names. `None` means "any package";
    /// `Some(list)` means only those. An empty `Some` trusts nothing, which is
    /// what a key nobody has scoped yet should not say.
    pub packages: Option<Vec<String>>,
}

impl TrustedKey {
    pub fn new(key_id: &str, public_key: &str) -> TrustedKey {
        TrustedKey {
            key_id: key_id.to_string(),
            public_key: public_key.to_string(),
            label: String::new(),
            trusted: true,
            packages: None,
        }
    }

    /// Is this key trusted for `name`?
    pub fn covers(&self, name: &str) -> bool {
        match &self.packages {
            None => true,
            Some(list) => list.iter().any(|p| p == name),
        }
    }

    /// How specific this key is; a scoped key beats a general one.
    pub fn specificity(&self) -> usize {
        self.packages.as_ref().map_or(0, |p| p.len())
    }
}

/// `$HARD_HOME/trusted-keys.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustStore {
    pub path: PathBuf,
    pub keys: Vec<TrustedKey>,
}

impl TrustStore {
    /// The default location, honouring `HARD_HOME`.
    pub fn default_path() -> PathBuf {
        crate::cache::hard_home().join("trusted-keys.toml")
    }

    /// An empty store at the default location.
    pub fn load_default() -> TrustStore {
        TrustStore::load(TrustStore::default_path())
    }

    /// Read a store. A missing file is an empty store, not an error.
    pub fn load(path: PathBuf) -> TrustStore {
        let keys = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| TrustStore::parse(&t).ok())
            .map(|s| s.keys)
            .unwrap_or_default();
        TrustStore { path, keys }
    }

    /// Parse the TOML document.
    pub fn parse(text: &str) -> Result<TrustStore, String> {
        let mut store = TrustStore::default();
        let mut current: Option<TrustedKey> = None;
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("[[key]]") {
                if rest.trim().is_empty() {
                    if let Some(k) = current.take() {
                        store.keys.push(k);
                    }
                    current = Some(TrustedKey {
                        key_id: String::new(),
                        public_key: String::new(),
                        label: String::new(),
                        trusted: false,
                        packages: None,
                    });
                    continue;
                }
            }
            if line.starts_with('[') && line.ends_with(']') && !line.starts_with("[[") {
                if let Some(k) = current.take() {
                    store.keys.push(k);
                }
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("line {}: expected key = value", lineno + 1));
            };
            let key = key.trim();
            let value = unquote(value.trim());
            let Some(k) = current.as_mut() else {
                return Err(format!(
                    "line {}: '{}' appears before any [[key]] table",
                    lineno + 1,
                    key
                ));
            };
            match key {
                "id" | "key_id" => k.key_id = value,
                "public_key" => k.public_key = value,
                "label" | "note" => k.label = value,
                "trusted" => k.trusted = value == "true",
                "packages" => {
                    k.packages = Some(
                        value
                            .trim_start_matches('[')
                            .trim_end_matches(']')
                            .split(',')
                            .map(|p| unquote(p.trim()))
                            .filter(|p| !p.is_empty())
                            .collect(),
                    )
                }
                other => return Err(format!("line {}: unknown field '{other}'", lineno + 1)),
            }
        }
        if let Some(k) = current.take() {
            store.keys.push(k);
        }
        for k in &store.keys {
            if k.key_id.is_empty() {
                return Err("a [[key]] entry has no id".to_string());
            }
        }
        Ok(store)
    }

    /// Render the store as TOML. Output is stable, so a clean tree stays clean.
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# HardScript trusted signing keys\n\
             # Managed by `hard keys`; edit by hand if you like, but keep the shape.\n\
             # A key is only trusted for strict verification once `trusted = true`.\n\n",
        );
        for k in &self.keys {
            out.push_str("[[key]]\n");
            out.push_str(&format!("id = \"{}\"\n", k.key_id));
            out.push_str(&format!("public_key = \"{}\"\n", k.public_key));
            if !k.label.is_empty() {
                out.push_str(&format!("label = \"{}\"\n", k.label));
            }
            out.push_str(&format!("trusted = {}\n", k.trusted));
            if let Some(packages) = &k.packages {
                out.push_str(&format!("packages = [{}]\n", packages
                    .iter()
                    .map(|p| format!("\"{p}\""))
                    .collect::<Vec<_>>()
                    .join(", ")));
            }
            out.push('\n');
        }
        out
    }

    /// Write the store, creating `$HARD_HOME` if needed.
    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&self.path, self.render())
            .map_err(|e| format!("cannot write {}: {e}", self.path.display()))
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// How many keys are actually pinned.
    pub fn trusted_count(&self) -> usize {
        self.keys.iter().filter(|k| k.trusted).count()
    }

    /// The recorded key with this id, pinned or not.
    pub fn get(&self, key_id: &str) -> Option<&TrustedKey> {
        self.keys.iter().find(|k| k.key_id == key_id)
    }

    /// The pinned key that covers `name`, preferring the most specific one.
    ///
    /// A key scoped to exactly this package beats a catch-all key, so adding
    /// a narrow key can tighten trust without removing the general one.
    pub fn trusted_for(&self, name: &str) -> Option<&TrustedKey> {
        self.keys
            .iter()
            .filter(|k| k.trusted && k.covers(name))
            .max_by_key(|k| k.specificity())
    }

    /// Record a key. Re-adding an existing id updates it in place.
    pub fn add(&mut self, key: TrustedKey) -> bool {
        let fresh = self.get(&key.key_id).is_none();
        if let Some(existing) = self.keys.iter_mut().find(|k| k.key_id == key.key_id) {
            existing.public_key = key.public_key;
            if !key.label.is_empty() {
                existing.label = key.label;
            }
            if key.trusted {
                existing.trusted = true;
            }
            // Narrowing an existing general key by accident would silently
            // stop trusting it everywhere else, so a general record wins.
            existing.packages = match (existing.packages.take(), key.packages) {
                (None, _) | (_, None) => None,
                (Some(mut a), Some(b)) => {
                    for p in b {
                        if !a.contains(&p) {
                            a.push(p);
                        }
                    }
                    Some(a)
                }
            };
        } else {
            self.keys.push(key);
        }
        fresh
    }

    /// Forget a key. Returns false when it was not recorded.
    pub fn remove(&mut self, key_id: &str) -> bool {
        let before = self.keys.len();
        self.keys.retain(|k| k.key_id != key_id);
        self.keys.len() != before
    }

    /// Pin a key that was already recorded.
    pub fn trust(&mut self, key_id: &str) -> Result<(), String> {
        match self.keys.iter_mut().find(|k| k.key_id == key_id) {
            Some(k) => {
                k.trusted = true;
                Ok(())
            }
            None => Err(format!("no key with id '{key_id}'; add it first")),
        }
    }

    /// The store as JSON, for `--json` and for `hard keys export`.
    pub fn to_json(&self) -> hs_compiler::json::Json {
        use hs_compiler::json::Json;
        Json::arr(
            self.keys
                .iter()
                .map(|k| {
                    Json::obj(vec![
                        ("id", Json::str(&k.key_id)),
                        ("public_key", Json::str(&k.public_key)),
                        ("label", Json::str(&k.label)),
                        ("trusted", Json::Bool(k.trusted)),
                        (
                            "packages",
                            match &k.packages {
                                None => Json::Null,
                                Some(p) => Json::arr(p.iter().map(Json::str).collect()),
                            },
                        ),
                    ])
                })
                .collect(),
        )
    }
}

fn unquote(v: &str) -> String {
    let t = v.trim();
    let t = t.strip_prefix('"').unwrap_or(t);
    let t = t.strip_suffix('"').unwrap_or(t);
    t.replace("\\\"", "\"").replace("\\\\", "\\")
}

// -- verification ----------------------------------------------------------

/// Why a package passed, failed, or was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The signature is valid and made by a pinned key.
    Verified,
    /// The signature is valid, but the key is not pinned.
    UntrustedKey,
    /// The signature does not match the payload or the key.
    BadSignature,
    /// The registry published no signature for this version.
    Unsigned,
    /// The record was too incomplete to check.
    Incomplete,
    /// The policy is `off`.
    Skipped,
    /// The archive's digest does not match the digest that was signed.
    IntegrityMismatch,
    /// The archive is not a package, or cannot be read.
    Unreadable,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Verified => "verified",
            Status::UntrustedKey => "untrusted-key",
            Status::BadSignature => "bad-signature",
            Status::Unsigned => "unsigned",
            Status::Incomplete => "incomplete",
            Status::Skipped => "skipped",
            Status::IntegrityMismatch => "integrity-mismatch",
            Status::Unreadable => "unreadable",
        }
    }

    /// Should a command stop on this status?
    pub fn is_failure(self) -> bool {
        matches!(
            self,
            Status::BadSignature | Status::IntegrityMismatch | Status::UntrustedKey
        )
    }

    /// Is this a "cannot check" outcome rather than a "checked and failed" one?
    pub fn is_inconclusive(self) -> bool {
        matches!(self, Status::Unsigned | Status::Incomplete | Status::Unreadable)
    }
}

/// The result of verifying one package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub name: String,
    pub version: String,
    pub status: Status,
    pub key_id: String,
    /// The digest that was checked, when the record had one.
    pub integrity: String,
    /// The digest of the signed payload.
    pub payload_hash: String,
    /// A sentence for a human.
    pub detail: String,
}

impl Outcome {
    fn new(name: &str, version: &str, status: Status, detail: impl Into<String>) -> Outcome {
        Outcome {
            name: name.to_string(),
            version: version.to_string(),
            status,
            key_id: String::new(),
            integrity: String::new(),
            payload_hash: String::new(),
            detail: detail.into(),
        }
    }

    /// Did verification succeed under any policy?
    pub fn ok(&self) -> bool {
        self.status == Status::Verified
    }

    /// Does this outcome stop a strict command?
    pub fn blocks(&self, policy: VerifyPolicy) -> bool {
        match policy {
            VerifyPolicy::Off => false,
            // Strict also refuses a package it could not check: an unverifiable
            // package is not a verified one.
            VerifyPolicy::Strict => !matches!(self.status, Status::Verified | Status::Skipped),
            VerifyPolicy::Warn => false,
        }
    }

    /// One line for the terminal.
    pub fn render(&self) -> String {
        let mut line = format!(
            "{} {} {}",
            self.name,
            if self.version.is_empty() {
                "-"
            } else {
                &self.version
            },
            self.status.as_str()
        );
        if !self.key_id.is_empty() {
            line.push_str(&format!(" [{}{}]", self.key_id, if self.ok() { "" } else { "!" }));
        }
        if !self.detail.is_empty() {
            line.push_str(&format!(": {}", self.detail));
        }
        line
    }

    /// The outcome as JSON.
    pub fn to_json(&self) -> hs_compiler::json::Json {
        use hs_compiler::json::Json;
        Json::obj(vec![
            ("name", Json::str(&self.name)),
            ("version", Json::str(&self.version)),
            ("status", Json::str(self.status.as_str())),
            ("verified", Json::Bool(self.ok())),
            ("key_id", Json::str(&self.key_id)),
            ("integrity", Json::str(&self.integrity)),
            ("payload_hash", Json::str(&self.payload_hash)),
            ("detail", Json::str(&self.detail)),
        ])
    }
}

/// The verifier: a policy plus a trust store.
#[derive(Clone, Debug)]
pub struct Verifier {
    pub policy: VerifyPolicy,
    pub store: TrustStore,
}

impl Verifier {
    pub fn new(policy: VerifyPolicy) -> Verifier {
        Verifier {
            policy,
            store: TrustStore::load_default(),
        }
    }

    pub fn with_store(policy: VerifyPolicy, store: TrustStore) -> Verifier {
        Verifier { policy, store }
    }

    /// Verify one record.
    ///
    /// Order matters: the signature is checked *before* trust is consulted, so
    /// a key that is pinned but whose signature is wrong is a
    /// `BadSignature`, not a pass.
    pub fn verify(&self, record: &SignatureRecord) -> Outcome {
        let mut out = Outcome::new(
            &record.name,
            &record.version,
            Status::Verified,
            "the signature is valid",
        );
        out.key_id = record.key_id.clone();
        out.integrity = record.integrity.clone();
        out.payload_hash = record.payload_hash();

        if self.policy == VerifyPolicy::Off {
            out.status = Status::Skipped;
            out.detail = "verification is off".to_string();
            return out;
        }
        if record.signature.is_empty() {
            out.status = Status::Unsigned;
            out.detail = "the registry published no signature for this version".to_string();
            return out;
        }
        if !record.is_complete() {
            out.status = Status::Incomplete;
            out.detail = "the signature record is missing a field".to_string();
            return out;
        }
        if !record.algorithm.is_empty() && record.algorithm != SIGNATURE_ALGORITHM {
            out.status = Status::BadSignature;
            out.detail = format!(
                "unsupported signature algorithm '{}' (only {SIGNATURE_ALGORITHM} is accepted)",
                record.algorithm
            );
            return out;
        }
        if !is_valid_public_key(&record.public_key) {
            out.status = Status::BadSignature;
            out.detail = "the record's public key is not a valid ed25519 key".to_string();
            return out;
        }
        if let Some(expected) = key_id_of(&record.public_key) {
            if !record.key_id.is_empty() && record.key_id != expected {
                out.status = Status::BadSignature;
                out.detail = format!(
                    "the key id '{}' does not match the public key ('{expected}')",
                    record.key_id
                );
                return out;
            }
        }
        if !verify_with_public_key(
            &record.public_key,
            &record.payload(),
            &record.signature,
        ) {
            out.status = Status::BadSignature;
            out.detail = "the signature does not cover this package's metadata".to_string();
            return out;
        }

        match self.store.trusted_for(&record.name) {
            Some(key) if key.key_id == record.key_id => {
                out.status = Status::Verified;
                out.detail = match &key.packages {
                    Some(p) if p.len() == 1 => {
                        format!("signed by trusted key {} (trusted for {})", key.key_id, p[0])
                    }
                    _ => format!("signed by trusted key {}", key.key_id),
                };
            }
            Some(key) => {
                out.status = Status::BadSignature;
                out.detail = format!(
                    "signed by '{}', but '{}' is the key trusted for this package",
                    record.key_id, key.key_id
                );
            }
            None => {
                out.status = Status::UntrustedKey;
                out.detail = format!(
                    "signed by {}, which is not in the trust store (hard keys trust {})",
                    record.key_id, record.key_id
                );
            }
        }
        out
    }

    /// Verify a package that has just been downloaded, checking that the bytes
    /// on disk are the bytes that were signed.
    pub fn verify_downloaded(&self, record: &SignatureRecord, archive: &[u8]) -> Outcome {
        let mut out = self.verify(record);
        let got = format!("sha256:{}", sha256_hex(archive));
        if normalize_integrity(&got) != normalize_integrity(&record.integrity) {
            out.status = Status::IntegrityMismatch;
            out.detail = format!(
                "the archive hashes to {got}, but {} was signed",
                normalize_integrity(&record.integrity)
            );
            out.integrity = record.integrity.clone();
            return out;
        }
        if out.status == Status::Verified {
            out.detail = format!("signed by trusted key {} (archive digest matches)", record.key_id);
        }
        out
    }

    /// Fetch and verify one version from a registry.
    pub fn verify_remote(
        &self,
        registry: &Registry,
        name: &str,
        version: &str,
    ) -> Outcome {
        match SignatureRecord::fetch(registry, name, version) {
            Ok(rec) => self.verify(&rec),
            Err(e) => Outcome::new(
                name,
                version,
                Status::Unsigned,
                format!("no signature available: {e}"),
            ),
        }
    }
}

/// Verify a local `.hspkg` file.
///
/// Without a signature record there is nothing to check the bytes against, so
/// the answer is "unreadable"/"unsigned" rather than a false pass. A sidecar
/// `<file>.sig` (written by `hard verify --write-sidecar`) supplies the record
/// and makes offline verification possible.
pub fn verify_archive_file(
    verifier: &Verifier,
    path: &Path,
) -> (Outcome, Option<SignatureRecord>) {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            return (
                Outcome::new(
                    &name,
                    "",
                    Status::Unreadable,
                    format!("cannot read {}: {e}", path.display()),
                ),
                None,
            )
        }
    };
    if crate::pkgfmt::read_archive(&bytes).is_err() {
        return (
            Outcome::new(
                &name,
                "",
                Status::Unreadable,
                format!("{} is not a .hspkg archive", path.display()),
            ),
            None,
        );
    }
    let sidecar = sidecar_path(path);
    let record = match std::fs::read_to_string(&sidecar) {
        Ok(text) => match SignatureRecord::from_sidecar(&text) {
            Ok(r) => Some(r),
            Err(e) => {
                return (
                    Outcome::new(&name, "", Status::Incomplete, e),
                    None,
                )
            }
        },
        Err(_) => None,
    };
    match record {
        Some(rec) => {
            let mut out = verifier.verify_downloaded(&rec, &bytes);
            if out.name.is_empty() {
                out.name = name;
            }
            (out, Some(rec))
        }
        None => {
            let digest = format!("sha256:{}", sha256_hex(&bytes));
            (
                Outcome::new(
                    &name,
                    "",
                    Status::Unsigned,
                    format!(
                        "no signature sidecar at {}; the archive hashes to {digest}",
                        sidecar.display()
                    ),
                ),
                None,
            )
        }
    }
}

/// The sidecar path for an archive: `<file>.sig`.
pub fn sidecar_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".sig");
    PathBuf::from(s)
}

/// Write a sidecar next to an archive, so it can be verified offline later.
pub fn write_sidecar(record: &SignatureRecord, archive: &Path) -> Result<PathBuf, String> {
    let p = sidecar_path(archive);
    std::fs::write(&p, format!("{}\n", record.to_json().to_string()))
        .map_err(|e| format!("cannot write {}: {e}", p.display()))?;
    Ok(p)
}

/// A quick look at what a registry is signing with, for `hard doctor`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    pub registry: String,
    pub key_id: String,
    pub test_key: bool,
    pub algorithm: String,
    pub payload_format: String,
    pub trusted_keys: usize,
    pub total_keys: usize,
    /// Why a key could not be checked, if it could not.
    pub error: Option<String>,
    /// Keys the registry signs with that the client does not trust.
    pub unknown_key: bool,
}

impl Diagnostics {
    /// A sentence per concern; empty when everything lines up.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(e) = &self.error {
            out.push(format!("cannot read the registry's signing key: {e}"));
        }
        if self.test_key {
            out.push(
                "the registry is signing with its built-in test key; packages are not trustworthy"
                    .to_string(),
            );
        }
        if self.unknown_key {
            out.push(format!(
                "the registry signs with {}, which is not pinned (hard keys trust {})",
                self.key_id, self.key_id
            ));
        }
        if self.trusted_keys == 0 {
            out.push("no signing keys are pinned; strict verification will refuse everything"
                .to_string());
        }
        if self.payload_format != SIGNATURE_VERSION && !self.payload_format.is_empty() {
            out.push(format!(
                "the registry signs '{}' payloads, this client speaks '{}'",
                self.payload_format, SIGNATURE_VERSION
            ));
        }
        out
    }

    /// `hard doctor` rendering.
    pub fn render(&self) -> String {
        let mut out = format!("registry      {}\n", self.registry);
        out.push_str(&format!("signing key   {}\n", self.key_id));
        out.push_str(&format!("algorithm     {}\n", self.algorithm));
        out.push_str(&format!("payload       {}\n", self.payload_format));
        out.push_str(&format!(
            "trusted keys  {} of {} recorded\n",
            self.trusted_keys, self.total_keys
        ));
        for w in self.warnings() {
            out.push_str(&format!("warning: {w}\n"));
        }
        out
    }
}

/// Collect signature diagnostics for a registry plus the local trust store.
pub fn diagnose(registry: &Registry, store: &TrustStore) -> Diagnostics {
    let mut d = Diagnostics {
        registry: registry.config.url.clone(),
        trusted_keys: store.trusted_count(),
        total_keys: store.len(),
        ..Diagnostics::default()
    };
    match RegistryKey::fetch(registry) {
        Ok(k) => {
            d.key_id = k.key_id.clone();
            d.algorithm = k.algorithm.clone();
            d.payload_format = k.payload_format.clone();
            d.test_key = k.test_key;
            d.unknown_key = !k.key_id.is_empty() && store.get(&k.key_id).is_none();
        }
        Err(e) => d.error = Some(e.message),
    }
    d
}

/// Keys seen in the store, as `key_id -> public key`, for shell use.
pub fn key_map(store: &TrustStore) -> BTreeMap<String, String> {
    store
        .keys
        .iter()
        .map(|k| (k.key_id.clone(), k.public_key.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(n: u8) -> [u8; 32] {
        [n; 32]
    }

    fn record(s: &[u8; 32], name: &str, version: &str) -> SignatureRecord {
        let pk = public_key_of_seed(s);
        let integrity = format!("sha256:{}", sha256_hex(b"archive"));
        let fingerprint = format!("sha256:{}", sha256_hex(b"manifest"));
        let payload = signature_payload(name, version, &integrity, &fingerprint);
        SignatureRecord {
            name: name.to_string(),
            version: version.to_string(),
            integrity,
            fingerprint,
            signature: sign_with_seed(s, &payload),
            key_id: key_id_of(&pk).unwrap(),
            public_key: pk,
            algorithm: SIGNATURE_ALGORITHM.to_string(),
        }
    }

    fn store_with(rec: &SignatureRecord) -> TrustStore {
        let mut st = TrustStore {
            path: PathBuf::from("/tmp/does-not-matter.toml"),
            keys: Vec::new(),
        };
        st.add(TrustedKey::new(&rec.key_id, &rec.public_key));
        st
    }

    // -- payload -----------------------------------------------------------

    #[test]
    fn the_payload_is_exactly_the_documented_text() {
        let p = signature_payload("jwt", "1.0.0", "sha256:AB", "sha256:CD");
        assert_eq!(
            p,
            "hs-signature/1\nname jwt\nversion 1.0.0\nintegrity sha256:ab\nfingerprint sha256:cd\n"
        );
    }

    #[test]
    fn the_payload_never_contains_a_timestamp_or_a_count() {
        let p = signature_payload("jwt", "1.0.0", "sha256:ab", "sha256:cd");
        for forbidden in ["published_at", "downloads", "2024", "timestamp", "http"] {
            assert!(!p.contains(forbidden), "{forbidden} leaked into the payload");
        }
        assert_eq!(p.lines().count(), 5);
    }

    #[test]
    fn digest_spelling_does_not_change_the_payload() {
        let a = signature_payload("jwt", "1.0.0", "sha256:ABCDEF", "sha256:00ff");
        let b = signature_payload("jwt", "1.0.0", "abcdef", "00FF");
        assert_eq!(a, b);
        assert_eq!(normalize_integrity(" sha256:AB "), "sha256:ab");
        assert_eq!(normalize_integrity("SHA256:AB"), "sha256:ab", "the prefix is case-blind");
        assert_eq!(normalize_integrity("sha256:sha256:ab"), "sha256:sha256:ab", "no double stripping");
        assert_eq!(normalize_integrity(""), "sha256:");
    }

    #[test]
    fn the_payload_hash_is_a_digest_of_the_payload() {
        let p = signature_payload("jwt", "1.0.0", "sha256:ab", "sha256:cd");
        assert_eq!(payload_hash(&p), format!("sha256:{}", sha256_hex(p.as_bytes())));
    }

    // -- primitives -------------------------------------------------------

    #[test]
    fn base64_roundtrips() {
        for case in [&b""[..], b"a", b"ab", b"abc", b"abcd", b"hello world, this is longer"] {
            let e = base64_encode(case);
            assert_eq!(base64_decode(&e).as_deref(), Some(case), "{e}");
        }
        assert_eq!(base64_encode(b"abc"), "YWJj");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"a"), "YQ==");
    }

    #[test]
    fn base64_rejects_junk() {
        assert!(base64_decode("YWJ").is_none(), "length must be a multiple of 4");
        assert!(base64_decode("!!!!").is_none());
        assert!(base64_decode("=AAA").is_none());
        assert!(base64_decode("A=AA").is_none());
        assert_eq!(base64_decode("YWJj\n").as_deref(), Some(&b"abc"[..]), "whitespace is fine");
    }

    #[test]
    fn hex_roundtrips_and_rejects_junk() {
        assert_eq!(hex(&[0x00, 0xff, 0x1a]), "00ff1a");
        assert_eq!(unhex("00FF1a").as_deref(), Some(&[0x00, 0xff, 0x1a][..]));
        assert_eq!(unhex("sha256:ab").as_deref(), Some(&[0xab][..]));
        assert!(unhex("abc").is_none(), "odd length");
        assert!(unhex("").is_none());
        assert!(unhex("zz").is_none());
    }

    #[test]
    fn a_key_id_is_the_first_16_hex_characters() {
        let pk = public_key_of_seed(&seed(7));
        let id = key_id_of(&pk).unwrap();
        assert!(id.starts_with("k:"));
        assert_eq!(id.len(), 18);
        assert_eq!(&id[2..], &pk[..16]);
        assert!(key_id_of("aabb").is_none(), "a short key has no id");
        assert!(key_id_of("").is_none());
    }

    #[test]
    fn a_signature_verifies_only_under_its_own_key() {
        let s = seed(1);
        let payload = signature_payload("jwt", "1.0.0", "sha256:ab", "sha256:cd");
        let sig = sign_with_seed(&s, &payload);
        assert!(verify_with_public_key(&public_key_of_seed(&s), &payload, &sig));
        assert!(!verify_with_public_key(&public_key_of_seed(&seed(2)), &payload, &sig));
        assert!(!verify_with_public_key(&public_key_of_seed(&s), &payload, "AAAA"));
        assert!(!verify_with_public_key("not-a-key", &payload, &sig));
    }

    #[test]
    fn a_tampered_payload_fails() {
        let s = seed(1);
        let sig = sign_with_seed(&s, &signature_payload("jwt", "1.0.0", "sha256:ab", "sha256:cd"));
        for (name, version, integrity, fingerprint) in [
            ("evil", "1.0.0", "sha256:ab", "sha256:cd"),
            ("jwt", "9.9.9", "sha256:ab", "sha256:cd"),
            ("jwt", "1.0.0", "sha256:ff", "sha256:cd"),
            ("jwt", "1.0.0", "sha256:ab", "sha256:ff"),
        ] {
            let payload = signature_payload(name, version, integrity, fingerprint);
            assert!(
                !verify_with_public_key(&public_key_of_seed(&s), &payload, &sig),
                "a signature must not survive a change to {name} {version} {integrity} {fingerprint}"
            );
        }
    }

    #[test]
    fn public_key_validity() {
        assert!(is_valid_public_key(&public_key_of_seed(&seed(3))));
        assert!(!is_valid_public_key(""));
        assert!(!is_valid_public_key("00"), "wrong length");
        assert!(!is_valid_public_key("zz"), "not hex");
        // 0x02 followed by zeros does not decompress to a curve point
        let mut not_a_point = [0u8; 32];
        not_a_point[0] = 2;
        assert!(!is_valid_public_key(&hex(&not_a_point)), "not a curve point");
    }

    // -- policy -----------------------------------------------------------

    #[test]
    fn policies_parse() {
        assert_eq!(VerifyPolicy::parse("strict"), Ok(VerifyPolicy::Strict));
        assert_eq!(VerifyPolicy::parse(" WARN "), Ok(VerifyPolicy::Warn));
        assert_eq!(VerifyPolicy::parse("off"), Ok(VerifyPolicy::Off));
        assert!(VerifyPolicy::parse("maybe").is_err());
        assert_eq!(VerifyPolicy::default(), VerifyPolicy::Warn);
        assert_eq!(VerifyPolicy::Strict.as_str(), "strict");
        assert_eq!(VerifyPolicy::Strict.to_string(), "strict");
        assert!(VerifyPolicy::Strict.is_fatal());
        assert!(!VerifyPolicy::Warn.is_fatal());
    }

    #[test]
    fn a_flag_beats_the_environment_and_bad_values_are_errors() {
        assert_eq!(VerifyPolicy::from_flags(Some("off")), Ok(VerifyPolicy::Off));
        assert!(VerifyPolicy::from_flags(Some("loose")).is_err(), "never silently downgrade");
        std::env::set_var("HARD_VERIFY", "strict");
        assert_eq!(VerifyPolicy::from_flags(None), Ok(VerifyPolicy::Strict));
        assert_eq!(VerifyPolicy::from_flags(Some("off")), Ok(VerifyPolicy::Off));
        std::env::remove_var("HARD_VERIFY");
        assert_eq!(VerifyPolicy::from_flags(None), Ok(VerifyPolicy::Warn));
    }

    // -- verification -----------------------------------------------------

    #[test]
    fn a_pinned_signature_verifies() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let v = Verifier::with_store(VerifyPolicy::Strict, store_with(&rec));
        let out = v.verify(&rec);
        assert_eq!(out.status, Status::Verified, "{}", out.detail);
        assert!(out.ok());
        assert_eq!(out.key_id, rec.key_id);
        assert!(!out.payload_hash.is_empty());
        assert!(!out.blocks(VerifyPolicy::Strict));
    }

    #[test]
    fn an_unknown_key_is_untrusted_even_though_the_signature_is_valid() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let empty = TrustStore {
            path: PathBuf::from("/tmp/x.toml"),
            keys: Vec::new(),
        };
        let out = Verifier::with_store(VerifyPolicy::Strict, empty).verify(&rec);
        assert_eq!(out.status, Status::UntrustedKey);
        assert!(out.detail.contains("hard keys trust"), "{}", out.detail);
        assert!(out.blocks(VerifyPolicy::Strict));
        assert!(!out.blocks(VerifyPolicy::Warn));
    }

    #[test]
    fn a_forged_signature_is_a_bad_signature() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let mut forged = rec.clone();
        forged.signature = sign_with_seed(&seed(9), &rec.payload());
        let v = Verifier::with_store(VerifyPolicy::Strict, store_with(&rec));
        let out = v.verify(&forged);
        assert_eq!(out.status, Status::BadSignature);
        assert!(out.blocks(VerifyPolicy::Strict));
    }

    #[test]
    fn a_key_id_that_does_not_match_its_public_key_is_refused() {
        let good = record(&seed(1), "jwt", "1.0.0");
        let other = record(&seed(2), "jwt", "1.0.0");
        // The signature is valid under key B, but the record claims key A.
        let mut swapped = other.clone();
        swapped.key_id = good.key_id.clone();
        let v = Verifier::with_store(VerifyPolicy::Strict, store_with(&good));
        let out = v.verify(&swapped);
        assert_eq!(out.status, Status::BadSignature);
        assert!(out.detail.contains("does not match the public key"), "{}", out.detail);
    }

    #[test]
    fn a_valid_signature_from_a_pinned_key_is_not_a_key_swap() {
        // Same key, different package: this is a legitimate publish, and the
        // signature must still verify.
        let good = record(&seed(1), "jwt", "1.0.0");
        let also = record(&seed(1), "jwt", "2.0.0");
        let v = Verifier::with_store(VerifyPolicy::Strict, store_with(&good));
        assert_eq!(v.verify(&also).status, Status::Verified);
    }

    #[test]
    fn a_key_pinned_for_another_package_does_not_count() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let mut st = store_with(&rec);
        st.keys[0].packages = Some(vec!["other".to_string()]);
        let out = Verifier::with_store(VerifyPolicy::Strict, st).verify(&rec);
        assert_ne!(out.status, Status::Verified);
    }

    #[test]
    fn a_key_pinned_for_this_package_does_count() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let mut st = store_with(&rec);
        st.keys[0].packages = Some(vec!["other".to_string(), "jwt".to_string()]);
        let out = Verifier::with_store(VerifyPolicy::Strict, st).verify(&rec);
        assert_eq!(out.status, Status::Verified, "{}", out.detail);
    }

    #[test]
    fn an_unpinned_key_never_verifies() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let mut st = store_with(&rec);
        st.keys[0].trusted = false;
        let out = Verifier::with_store(VerifyPolicy::Strict, st).verify(&rec);
        assert_eq!(out.status, Status::UntrustedKey);
    }

    #[test]
    fn the_off_policy_skips_everything() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let mut broken = rec.clone();
        broken.signature = "garbage".to_string();
        let v = Verifier::with_store(VerifyPolicy::Off, TrustStore::default());
        let out = v.verify(&broken);
        assert_eq!(out.status, Status::Skipped);
        assert!(!out.blocks(VerifyPolicy::Strict));
    }

    #[test]
    fn an_unsigned_package_is_inconclusive_but_blocks_strict() {
        let mut rec = record(&seed(1), "jwt", "1.0.0");
        rec.signature = String::new();
        let v = Verifier::with_store(VerifyPolicy::Warn, store_with(&rec));
        let out = v.verify(&rec);
        assert_eq!(out.status, Status::Unsigned);
        assert!(out.status.is_inconclusive());
        assert!(out.blocks(VerifyPolicy::Strict), "cannot check is not verified");
        assert!(!out.blocks(VerifyPolicy::Warn));
    }

    #[test]
    fn an_incomplete_record_is_reported_as_such() {
        let mut rec = record(&seed(1), "jwt", "1.0.0");
        rec.fingerprint = String::new();
        assert!(!rec.is_complete());
        let v = Verifier::with_store(VerifyPolicy::Warn, store_with(&rec));
        assert_eq!(v.verify(&rec).status, Status::Incomplete);
    }

    #[test]
    fn an_unknown_algorithm_is_refused() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let mut alien = rec.clone();
        alien.algorithm = "rsa-pss".to_string();
        let v = Verifier::with_store(VerifyPolicy::Strict, store_with(&rec));
        let out = v.verify(&alien);
        assert_eq!(out.status, Status::BadSignature);
        assert!(out.detail.contains("rsa-pss"), "{}", out.detail);
    }

    #[test]
    fn a_downloaded_archive_is_checked_against_the_signed_digest() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let archive = b"archive".to_vec();
        let v = Verifier::with_store(VerifyPolicy::Strict, store_with(&rec));
        let good = v.verify_downloaded(&rec, &archive);
        assert_eq!(good.status, Status::Verified, "{}", good.detail);
        assert!(good.detail.contains("digest matches"), "{}", good.detail);

        let tampered = b"archive!".to_vec();
        let bad = v.verify_downloaded(&rec, &tampered);
        assert_eq!(bad.status, Status::IntegrityMismatch);
        assert!(bad.blocks(VerifyPolicy::Strict));
    }

    #[test]
    fn an_integrity_mismatch_outranks_a_key_problem() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let v = Verifier::with_store(VerifyPolicy::Strict, TrustStore::default());
        let out = v.verify_downloaded(&rec, b"different".as_slice());
        assert_eq!(out.status, Status::IntegrityMismatch);
    }

    #[test]
    fn outcomes_render_and_serialize() {
        let out = Outcome {
            name: "jwt".to_string(),
            version: "1.0.0".to_string(),
            status: Status::Verified,
            key_id: "k:0123456789abcdef".to_string(),
            integrity: "sha256:ab".to_string(),
            payload_hash: "sha256:cd".to_string(),
            detail: "signed by trusted key k:0123456789abcdef".to_string(),
        };
        let line = out.render();
        assert!(line.starts_with("jwt 1.0.0 verified"), "{line}");
        assert!(line.contains("k:0123456789abcdef"), "{line}");
        let j = out.to_json().to_string();
        assert!(j.contains("\"verified\":true"), "{j}");
        assert!(j.contains("\"status\":\"verified\""), "{j}");
    }

    #[test]
    fn statuses_classify_correctly() {
        assert!(Status::BadSignature.is_failure());
        assert!(Status::IntegrityMismatch.is_failure());
        assert!(Status::UntrustedKey.is_failure());
        assert!(!Status::Verified.is_failure());
        assert!(!Status::Unsigned.is_failure());
        assert!(Status::Unsigned.is_inconclusive());
        assert!(Status::Unreadable.is_inconclusive());
        assert_eq!(Status::BadSignature.as_str(), "bad-signature");
    }

    // -- trust store ------------------------------------------------------

    #[test]
    fn a_store_roundtrips_through_toml() {
        let mut st = TrustStore {
            path: PathBuf::from("/tmp/x.toml"),
            keys: Vec::new(),
        };
        st.add(TrustedKey::new("k:aaaa", &"ab".repeat(32)));
        let mut scoped = TrustedKey::new("k:bbbb", &"cd".repeat(32));
        scoped.label = "a mirror".to_string();
        scoped.trusted = false;
        scoped.packages = Some(vec!["jwt".to_string(), "jsonwebtoken".to_string()]);
        st.add(scoped);
        let text = st.render();
        let back = TrustStore::parse(&text).unwrap();
        assert_eq!(back.keys, st.keys, "{text}");
        assert_eq!(back.len(), 2);
        assert_eq!(back.trusted_count(), 1);
    }

    #[test]
    fn a_store_parses_the_documented_shape() {
        let text = r#"
# comment
[[key]]
id = "k:1111222233334444"
public_key = "aa11"
label = "prod"
trusted = true
packages = ["jwt"]

[[key]]
id = "k:5555666677778888"
public_key = "bb22"
trusted = false
"#;
        let st = TrustStore::parse(text).unwrap();
        assert_eq!(st.len(), 2);
        assert_eq!(st.keys[0].packages.as_deref(), Some(&["jwt".to_string()][..]));
        assert!(st.keys[0].covers("jwt"));
        assert!(!st.keys[0].covers("other"));
        assert!(st.keys[1].covers("anything"), "an unscoped key covers everything");
        assert_eq!(st.trusted_count(), 1);
    }

    #[test]
    fn a_broken_store_is_reported_not_silently_emptied() {
        assert!(TrustStore::parse("trusted = true").is_err(), "field before a table");
        assert!(TrustStore::parse("[[key]]\npublic_key = \"aa\"").is_err(), "no id");
        assert!(TrustStore::parse("[[key]]\nid = \"k:1\"\nnope = 1").is_err());
    }

    #[test]
    fn a_missing_file_is_an_empty_store() {
        let st = TrustStore::load(PathBuf::from("/tmp/definitely-not-here-9182.toml"));
        assert!(st.is_empty());
        assert_eq!(st.trusted_count(), 0);
    }

    #[test]
    fn adding_the_same_key_twice_updates_it() {
        let mut st = TrustStore::default();
        assert!(st.add(TrustedKey::new("k:1", "aa")));
        assert!(!st.add(TrustedKey::new("k:1", "bb")), "not a new key");
        assert_eq!(st.len(), 1);
        assert_eq!(st.get("k:1").unwrap().public_key, "bb");
        assert!(st.remove("k:1"));
        assert!(!st.remove("k:1"));
        assert!(st.get("k:1").is_none());
    }

    #[test]
    fn trust_promotes_a_recorded_key() {
        let mut st = TrustStore::default();
        let mut k = TrustedKey::new("k:1", "aa");
        k.trusted = false;
        st.add(k);
        assert_eq!(st.trusted_count(), 0);
        st.trust("k:1").unwrap();
        assert_eq!(st.trusted_count(), 1);
        assert!(st.trust("k:nope").is_err());
    }

    #[test]
    fn the_most_specific_trusted_key_wins() {
        let mut st = TrustStore::default();
        let mut wide = TrustedKey::new("k:wide", "aa");
        wide.trusted = true;
        st.add(wide);
        let mut narrow = TrustedKey::new("k:narrow", "bb");
        narrow.trusted = true;
        narrow.packages = Some(vec!["jwt".to_string()]);
        st.add(narrow);
        assert_eq!(st.trusted_for("jwt").unwrap().key_id, "k:narrow");
        assert_eq!(st.trusted_for("other").unwrap().key_id, "k:wide");
    }

    #[test]
    fn a_general_key_stays_general_when_a_scope_is_added() {
        // Narrowing by accident would silently stop trusting the key
        // everywhere else, which is the opposite of what "add --for" means.
        let mut st = TrustStore::default();
        st.add(TrustedKey::new("k:1", "aa"));
        let mut scoped = TrustedKey::new("k:1", "aa");
        scoped.packages = Some(vec!["jwt".to_string()]);
        st.add(scoped.clone());
        assert_eq!(st.len(), 1);
        assert_eq!(st.keys[0].packages, None, "the general scope is kept");
        assert!(st.keys[0].covers("anything"));
        // and a genuinely scoped key stays scoped
        let mut st2 = TrustStore::default();
        st2.add(scoped.clone());
        assert_eq!(st2.keys[0].packages, Some(vec!["jwt".to_string()]));
        assert!(st2.keys[0].covers("jwt"));
        assert!(!st2.keys[0].covers("other"));
        // scopes merge when both are scoped
        let mut other = TrustedKey::new("k:1", "aa");
        other.packages = Some(vec!["other".to_string()]);
        st2.add(other);
        assert_eq!(
            st2.keys[0].packages,
            Some(vec!["jwt".to_string(), "other".to_string()])
        );
    }

    #[test]
    fn an_empty_scope_list_trusts_nothing() {
        let k = TrustedKey {
            key_id: "k:1".into(),
            public_key: "aa".into(),
            label: String::new(),
            trusted: true,
            packages: Some(Vec::new()),
        };
        assert!(!k.covers("jwt"));
        assert_eq!(k.specificity(), 0);
        let round = TrustStore::parse(&format!("[[key]]\nid = \"k:1\"\npublic_key = \"aa\"\ntrusted = true\npackages = []\n")).unwrap();
        assert_eq!(round.keys[0].packages, Some(Vec::new()));
    }

    #[test]
    fn the_store_serializes_to_json() {
        let mut st = TrustStore::default();
        st.add(TrustedKey::new("k:1", "aa"));
        let j = st.to_json().to_string();
        assert!(j.contains("\"id\":\"k:1\""), "{j}");
        assert!(j.contains("\"trusted\":true"), "{j}");
        let map = key_map(&st);
        assert_eq!(map.get("k:1").map(String::as_str), Some("aa"));
    }

    #[test]
    fn the_default_path_follows_hard_home() {
        std::env::set_var("HARD_HOME", "/tmp/hard-home-for-verify-test");
        let p = TrustStore::default_path();
        std::env::remove_var("HARD_HOME");
        assert!(p.ends_with("trusted-keys.toml"), "{}", p.display());
        assert!(p.to_string_lossy().contains("hard-home-for-verify-test"));
    }

    // -- records ----------------------------------------------------------

    #[test]
    fn a_record_roundtrips_through_json() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let back = SignatureRecord::from_json(&rec.to_json());
        assert_eq!(back, rec);
    }

    #[test]
    fn a_sidecar_roundtrips() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let text = format!("{}\n", rec.to_json().to_string());
        assert_eq!(SignatureRecord::from_sidecar(&text).unwrap(), rec);
        assert!(SignatureRecord::from_sidecar("not json").is_err());
    }

    #[test]
    fn a_registry_response_becomes_a_record() {
        let rec = record(&seed(1), "jwt", "1.0.0");
        let resp = HttpResponse {
            status: 200,
            body: rec.to_json().to_string().into_bytes(),
            headers: Vec::new(),
        };
        let got = signature_from_response(&resp).unwrap();
        assert_eq!(got, rec);

        let err = HttpResponse {
            status: 404,
            body: br#"{"error":"not found"}"#.to_vec(),
            headers: Vec::new(),
        };
        assert!(signature_from_response(&err).is_err());

        let junk = HttpResponse {
            status: 200,
            body: b"<html/>".to_vec(),
            headers: Vec::new(),
        };
        assert!(signature_from_response(&junk).is_err());
    }

    #[test]
    fn a_registry_key_parses() {
        let j = hs_compiler::json::parse(
            r#"{"algorithm":"ed25519","key_id":"k:aa","public_key":"bb","payload_format":"hs-signature/1","test_key":true}"#,
        )
        .unwrap();
        let k = RegistryKey::from_json(&j);
        assert_eq!(k.key_id, "k:aa");
        assert!(k.test_key);
        assert_eq!(k.payload_format, SIGNATURE_VERSION);
    }

    // -- local archives ---------------------------------------------------

    #[test]
    fn an_archive_with_a_sidecar_verifies_offline() {
        let dir = std::env::temp_dir().join(format!("hs-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("demo-1.0.0.hspkg");
        let bytes = crate::pkgfmt::pack(&[crate::pkgfmt::FileRecord {
            rel_path: "main.hard".to_string(),
            data: b"calc x() => Int { <- 1 }".to_vec(),
        }])
        .unwrap();
        std::fs::write(&archive, &bytes).unwrap();

        let seed = seed(4);
        let rec = SignatureRecord {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            integrity: format!("sha256:{}", sha256_hex(&bytes)),
            fingerprint: format!("sha256:{}", sha256_hex(b"manifest")),
            signature: String::new(),
            key_id: String::new(),
            public_key: String::new(),
            algorithm: SIGNATURE_ALGORITHM.to_string(),
        };
        let mut rec = rec;
        rec.signature = sign_with_seed(&seed, &rec.payload());
        rec.key_id = key_id_of(&public_key_of_seed(&seed)).unwrap();
        rec.public_key = public_key_of_seed(&seed);
        let written = write_sidecar(&rec, &archive).unwrap();
        assert!(written.exists());
        assert_eq!(written, sidecar_path(&archive));

        let v = Verifier::with_store(VerifyPolicy::Strict, store_with(&rec));
        let (out, got) = verify_archive_file(&v, &archive);
        assert_eq!(out.status, Status::Verified, "{}", out.detail);
        assert_eq!(got.unwrap(), rec);

        // and a swapped archive is caught by the digest, not the signature
        std::fs::write(&archive, b"not an archive").unwrap();
        let (out2, _) = verify_archive_file(&v, &archive);
        assert_eq!(out2.status, Status::Unreadable);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_archive_without_a_sidecar_is_unsigned_not_verified() {
        let dir = std::env::temp_dir().join(format!("hs-verify-nosidecar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("demo-1.0.0.hspkg");
        std::fs::write(
            &archive,
            crate::pkgfmt::pack(&[crate::pkgfmt::FileRecord {
                rel_path: "main.hard".to_string(),
                data: b"x".to_vec(),
            }])
            .unwrap(),
        )
        .unwrap();
        let (out, rec) = verify_archive_file(&Verifier::new(VerifyPolicy::Strict), &archive);
        assert_eq!(out.status, Status::Unsigned);
        assert!(out.detail.contains("no signature sidecar"), "{}", out.detail);
        assert!(rec.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_archive_is_unreadable() {
        let (out, _) = verify_archive_file(
            &Verifier::new(VerifyPolicy::Strict),
            Path::new("/tmp/definitely-not-here-9182.hspkg"),
        );
        assert_eq!(out.status, Status::Unreadable);
    }

    // -- diagnostics ------------------------------------------------------

    #[test]
    fn diagnostics_speak_up_when_something_is_wrong() {
        let d = Diagnostics {
            registry: "http://127.0.0.1:1".to_string(),
            key_id: "k:aa".to_string(),
            test_key: true,
            algorithm: "ed25519".to_string(),
            payload_format: SIGNATURE_VERSION.to_string(),
            trusted_keys: 0,
            total_keys: 1,
            error: None,
            unknown_key: true,
        };
        let w = d.warnings();
        assert!(w.iter().any(|x| x.contains("test key")), "{w:?}");
        assert!(w.iter().any(|x| x.contains("not pinned")), "{w:?}");
        assert!(w.iter().any(|x| x.contains("no signing keys")), "{w:?}");
        assert!(d.render().contains("0 of 1 recorded"), "{}", d.render());
    }

    #[test]
    fn a_healthy_registry_warns_about_nothing() {
        let mut st = TrustStore::default();
        st.add(TrustedKey::new("k:aa", &"ab".repeat(32)));
        let d = Diagnostics {
            registry: "http://localhost".to_string(),
            key_id: "k:aa".to_string(),
            test_key: false,
            algorithm: SIGNATURE_ALGORITHM.to_string(),
            payload_format: SIGNATURE_VERSION.to_string(),
            trusted_keys: st.trusted_count(),
            total_keys: st.len(),
            error: None,
            unknown_key: false,
        };
        assert!(d.warnings().is_empty(), "{:?}", d.warnings());
    }

    #[test]
    fn a_payload_format_mismatch_is_flagged() {
        let d = Diagnostics {
            payload_format: "hs-signature/2".to_string(),
            trusted_keys: 1,
            ..Diagnostics::default()
        };
        assert!(d.warnings().iter().any(|x| x.contains("hs-signature/2")));
    }
}
