//! Package signing.
//!
//! Every publish can be signed with an Ed25519 key held by the registry. The
//! signed payload is a small, line-oriented document that binds together the
//! four things a consumer actually cares about:
//!
//! ```text
//! hs-signature/1
//! name jwt
//! version 1.0.0
//! integrity sha256:<archive digest>
//! fingerprint sha256:<manifest fingerprint>
//! ```
//!
//! Signing the *archive digest* and the *manifest fingerprint* separately
//! means neither a modified tarball nor edited metadata can pass verification,
//! and it means a mirror can re-verify a copy without re-reading the archive.
//!
//! The public key is published at `GET /keys`, so a client can pin a key id
//! and refuse a publish signed by anything else.

use crate::base64;
use crate::security;
use ed25519_dalek::{Signature, Signer, SigningKey as DalekSigningKey, Verifier, VerifyingKey};
use std::path::Path;

/// Payload format version.
pub const SIGNATURE_VERSION: &str = "hs-signature/1";

/// Deterministic seed used when no key is configured. It exists so tests and
/// local sandboxes are reproducible; a real registry must supply its own seed
/// (see [`SigningKey::from_env`]), and the `/keys` endpoint says which it is.
const TEST_SEED: &[u8; 32] = b"hardscript-registry-test-key-001";

/// A registry signing key.
pub struct SigningKey {
    signing: DalekSigningKey,
    verifying: VerifyingKey,
    key_id: String,
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKey")
            .field("key_id", &self.key_id)
            .field("public_key", &self.public_hex())
            .finish()
    }
}

impl Clone for SigningKey {
    fn clone(&self) -> Self {
        SigningKey {
            signing: DalekSigningKey::from_bytes(&self.signing.to_bytes()),
            verifying: self.verifying,
            key_id: self.key_id.clone(),
        }
    }
}

impl SigningKey {
    /// Derive a key from a 32-byte seed.
    pub fn from_seed(seed: &[u8; 32]) -> SigningKey {
        let signing = DalekSigningKey::from_bytes(seed);
        let verifying = signing.verifying_key();
        let key_id = key_id_of(&verifying);
        SigningKey {
            signing,
            verifying,
            key_id,
        }
    }

    /// A fixed key for tests and ephemeral sandboxes.
    pub fn deterministic_for_tests() -> SigningKey {
        SigningKey::from_seed(TEST_SEED)
    }

    /// A key loaded from a file whose contents are exactly 32 seed bytes.
    pub fn from_file(path: &Path) -> Result<SigningKey, String> {
        let bytes = std::fs::read(path)
            .map_err(|e| format!("cannot read signing key {}: {e}", path.display()))?;
        let seed: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "a signing key file must contain exactly 32 bytes".to_string())?;
        Ok(SigningKey::from_seed(&seed))
    }

    /// Write the seed to `path` with owner-only permissions.
    pub fn write_seed(&self, path: &Path) -> Result<(), String> {
        std::fs::write(path, self.signing.to_bytes())
            .map_err(|e| format!("cannot write signing key {}: {e}", path.display()))?;
        restrict_permissions(path);
        Ok(())
    }

    /// Load the key named by `HARD_REGISTRY_SIGNING_KEY` (a file path), or
    /// fall back to the deterministic test key when unset.
    pub fn from_env() -> SigningKey {
        match std::env::var("HARD_REGISTRY_SIGNING_KEY") {
            Ok(p) if !p.is_empty() => match SigningKey::from_file(Path::new(&p)) {
                Ok(k) => k,
                Err(e) => {
                    eprintln!("warning: {e}; falling back to the built-in test key");
                    SigningKey::deterministic_for_tests()
                }
            },
            _ => SigningKey::deterministic_for_tests(),
        }
    }

    /// The key's public identifier, `k:<16 hex>`.
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// The raw 32-byte public key.
    pub fn public_bytes(&self) -> [u8; 32] {
        self.verifying.to_bytes()
    }

    /// Hex-encoded public key, as served by `GET /keys`.
    pub fn public_hex(&self) -> String {
        security::hex(&self.public_bytes())
    }

    /// Base64-encoded public key.
    pub fn public_base64(&self) -> String {
        base64::encode(&self.public_bytes())
    }

    /// Is this the built-in test key? Surfaced by `/keys` so nobody mistakes
    /// a sandbox registry for a production one.
    pub fn is_test_key(&self) -> bool {
        self.public_bytes() == SigningKey::deterministic_for_tests().public_bytes()
    }

    /// The canonical signing payload for one published version.
    pub fn payload(&self, name: &str, version: &str, integrity: &str, fingerprint: &str) -> String {
        payload(name, version, integrity, fingerprint)
    }

    /// Sign a payload, returning a base64 signature.
    pub fn sign(&self, payload: &str) -> Result<String, String> {
        let sig: Signature = self.signing.sign(payload.as_bytes());
        Ok(base64::encode(&sig.to_bytes()))
    }

    /// Verify a base64 signature against this key.
    pub fn verify(&self, payload: &str, signature_b64: &str) -> bool {
        let Some(raw) = base64::decode(signature_b64) else {
            return false;
        };
        let Ok(bytes) = <[u8; 64]>::try_from(raw.as_slice()) else {
            return false;
        };
        self.verify_bytes(payload, &Signature::from_bytes(&bytes).to_bytes())
    }

    /// Verify a raw 64-byte signature.
    pub fn verify_bytes(&self, payload: &str, sig: &[u8]) -> bool {
        let Ok(bytes) = <[u8; 64]>::try_from(sig) else {
            return false;
        };
        self.verifying
            .verify(payload.as_bytes(), &Signature::from_bytes(&bytes))
            .is_ok()
    }

    /// Sign and return `(signature, key_id)`.
    pub fn sign_pair(&self, payload: &str) -> Result<(String, String), String> {
        Ok((self.sign(payload)?, self.key_id.clone()))
    }
}

/// The canonical text that gets signed.
///
/// This delegates to the client so producer and consumer cannot drift apart:
/// a client that built the payload differently would reject every signature
/// this registry produced. `canonical_payload_matches_the_client` in the tests
/// below pins the format from both sides.
pub fn payload(name: &str, version: &str, integrity: &str, fingerprint: &str) -> String {
    hs_pm::verify::signature_payload(name, version, integrity, fingerprint)
}

/// Verify a signature against an explicit public key (used by clients and by
/// the `hard-registry verify` command, which must not need the private key).
pub fn verify_with_public_key(
    public_hex: &str,
    payload_text: &str,
    signature_b64: &str,
) -> bool {
    let Some(pk) = security::unhex(public_hex) else {
        return false;
    };
    let Ok(bytes) = <[u8; 32]>::try_from(pk.as_slice()) else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&bytes) else {
        return false;
    };
    let Some(sig_raw) = base64::decode(signature_b64) else {
        return false;
    };
    let Ok(sig_bytes) = <[u8; 64]>::try_from(sig_raw.as_slice()) else {
        return false;
    };
    vk.verify(payload_text.as_bytes(), &Signature::from_bytes(&sig_bytes))
        .is_ok()
}

/// The key id derived from a public key.
pub fn key_id_of_hex(public_hex: &str) -> Option<String> {
    let pk = security::unhex(public_hex)?;
    if pk.len() != 32 {
        return None;
    }
    Some(format!("k:{}", &security::hex(&pk)[..16]))
}

/// Restrict a file to owner read/write (a no-op off unix).
fn restrict_permissions(path: &Path) {
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

fn key_id_of(vk: &VerifyingKey) -> String {
    format!("k:{}", &security::hex(&vk.to_bytes())[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_key_is_reproducible() {
        let a = SigningKey::deterministic_for_tests();
        let b = SigningKey::from_seed(TEST_SEED);
        assert_eq!(a.public_hex(), b.public_hex());
        assert_eq!(a.key_id(), b.key_id());
        assert!(a.key_id().starts_with("k:"));
        assert_eq!(a.key_id().len(), 18);
        assert!(a.is_test_key());
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let k = SigningKey::deterministic_for_tests();
        let p = k.payload("jwt", "1.0.0", "sha256:aa", "sha256:bb");
        let sig = k.sign(&p).unwrap();
        assert!(k.verify(&p, &sig));
        assert!(verify_with_public_key(&k.public_hex(), &p, &sig));
    }

    #[test]
    fn any_field_change_breaks_the_signature() {
        let k = SigningKey::deterministic_for_tests();
        let sig = k.sign(&k.payload("jwt", "1.0.0", "sha256:aa", "sha256:bb")).unwrap();
        for other in [
            k.payload("other", "1.0.0", "sha256:aa", "sha256:bb"),
            k.payload("jwt", "1.0.1", "sha256:aa", "sha256:bb"),
            k.payload("jwt", "1.0.0", "sha256:cc", "sha256:bb"),
            k.payload("jwt", "1.0.0", "sha256:aa", "sha256:cc"),
        ] {
            assert!(!k.verify(&other, &sig), "must not verify: {other}");
        }
    }

    #[test]
    fn a_different_key_cannot_verify() {
        let a = SigningKey::from_seed(&[7u8; 32]);
        let b = SigningKey::from_seed(&[9u8; 32]);
        let p = a.payload("jwt", "1.0.0", "sha256:aa", "sha256:bb");
        let sig = a.sign(&p).unwrap();
        assert!(a.verify(&p, &sig));
        assert!(!b.verify(&p, &sig));
        assert!(!verify_with_public_key(&b.public_hex(), &p, &sig));
        assert_ne!(a.key_id(), b.key_id());
    }

    #[test]
    fn malformed_signatures_are_rejected() {
        let k = SigningKey::deterministic_for_tests();
        let p = k.payload("jwt", "1.0.0", "sha256:aa", "sha256:bb");
        assert!(!k.verify(&p, "not base64!"));
        assert!(!k.verify(&p, "AAAA"));
        let good = k.sign(&p).unwrap();
        let mut flipped = base64::decode(&good).unwrap();
        flipped[0] ^= 0xff;
        assert!(!k.verify(&p, &base64::encode(&flipped)));
    }

    #[test]
    fn payload_normalizes_digests() {
        assert_eq!(
            payload("a", "1.0.0", "AABB", "CCDD"),
            payload("a", "1.0.0", "sha256:aabb", "sha256:ccdd")
        );
        assert!(payload("a", "1.0.0", "x", "y").starts_with(SIGNATURE_VERSION));
    }

    #[test]
    fn key_files_roundtrip_with_owner_only_permissions() {
        let dir = std::env::temp_dir().join(format!("hs-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.key");
        let k = SigningKey::from_seed(&[3u8; 32]);
        k.write_seed(&path).unwrap();
        let loaded = SigningKey::from_file(&path).unwrap();
        assert_eq!(loaded.public_hex(), k.public_hex());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode was {mode:o}");
        }
        assert!(SigningKey::from_file(&dir.join("missing.key")).is_err());
        std::fs::write(&path, b"short").unwrap();
        assert!(SigningKey::from_file(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_id_derivation_from_hex() {
        let k = SigningKey::deterministic_for_tests();
        assert_eq!(key_id_of_hex(&k.public_hex()).as_deref(), Some(k.key_id()));
        assert!(key_id_of_hex("zz").is_none());
        assert!(key_id_of_hex("aabb").is_none());
    }

    #[test]
    fn public_base64_matches_hex() {
        let k = SigningKey::deterministic_for_tests();
        assert_eq!(
            base64::decode(&k.public_base64()).unwrap(),
            k.public_bytes().to_vec()
        );
    }

    #[test]
    fn canonical_payload_matches_the_client() {
        // The client builds the same text; if these ever diverge, every
        // signature this registry produces becomes unverifiable.
        let p = payload("jwt", "1.2.3", "sha256:AB", "SHA256:cd");
        assert_eq!(
            p,
            hs_pm::verify::signature_payload("jwt", "1.2.3", "sha256:AB", "SHA256:cd")
        );
        assert_eq!(p, format!("{SIGNATURE_VERSION}\nname jwt\nversion 1.2.3\nintegrity sha256:ab\nfingerprint sha256:cd\n"));
        assert_eq!(p.lines().count(), 5, "no timestamps, no counts");
    }

    #[test]
    fn a_signature_made_here_verifies_on_the_client() {
        let k = SigningKey::deterministic_for_tests();
        let p = payload("jwt", "1.0.0", "sha256:aa", "sha256:bb");
        let sig = k.sign(&p).unwrap();
        assert!(
            hs_pm::verify::verify_with_public_key(&k.public_hex(), &p, &sig),
            "the client must accept what the registry signs"
        );
        assert!(!hs_pm::verify::verify_with_public_key(&k.public_hex(), "tampered", &sig));
    }
}
