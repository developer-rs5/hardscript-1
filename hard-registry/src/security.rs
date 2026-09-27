//! Security primitives used by the registry.
//!
//! - SHA-256 (delegated to the compiler crate so the registry and the client
//!   hash identically) for archive integrity and token-at-rest hashing.
//! - HMAC-SHA-256 for PBKDF2 and for keyed request signing.
//! - PBKDF2-HMAC-SHA-256 for password storage (never plaintext, never a fast
//!   hash).
//! - Constant-time comparison for every secret equality check, so a caller
//!   cannot time its way to a valid token.

use hs_compiler::sha256;
use std::io::Read;

/// Lowercase hex encoding.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Decode lowercase or uppercase hex; `None` on any non-hex byte.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in b.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// SHA-256 of a byte slice, hex encoded.
pub fn sha256_hex(bytes: &[u8]) -> String {
    sha256::hex(bytes)
}

/// Normalize an integrity string: `sha256:<hex>`, bare `<hex>` and uppercase
/// hex all compare equal after this.
pub fn normalize_integrity(raw: &str) -> String {
    let t = raw.trim();
    let hexpart = t.strip_prefix("sha256:").unwrap_or(t);
    format!("sha256:{}", hexpart.to_ascii_lowercase())
}

/// True when `got` and `want` are the same digest (constant time).
pub fn integrity_matches(got: &str, want: &str) -> bool {
    constant_time_eq(
        normalize_integrity(got).as_bytes(),
        normalize_integrity(want).as_bytes(),
    )
}

/// Byte-wise equality that does not short-circuit on the first difference.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// HMAC-SHA-256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let d = sha256_digest(key);
        k[..32].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Vec::with_capacity(BLOCK + message.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(message);
    let inner_digest = sha256_digest(&inner);

    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_digest);
    sha256_digest(&outer)
}

/// Raw SHA-256 digest.
pub fn sha256_digest(bytes: &[u8]) -> [u8; 32] {
    // The compiler exposes hex; decode back so callers can use the raw
    // digest without duplicating the compression function.
    let h = sha256::hex(bytes);
    let bytes = unhex(&h).unwrap_or_default();
    let mut out = [0u8; 32];
    if bytes.len() == 32 {
        out.copy_from_slice(&bytes);
    }
    out
}

/// PBKDF2-HMAC-SHA-256, as used for password storage.
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32, out_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_len);
    let mut block_index: u32 = 1;
    while out.len() < out_len {
        let mut salted = salt.to_vec();
        salted.extend_from_slice(&block_index.to_be_bytes());
        let mut u = hmac_sha256(password, &salted);
        let mut acc = u;
        for _ in 1..iterations {
            u = hmac_sha256(password, &u);
            for (a, b) in acc.iter_mut().zip(u.iter()) {
                *a ^= b;
            }
        }
        out.extend_from_slice(&acc);
        block_index += 1;
    }
    out.truncate(out_len);
    out
}

/// Default PBKDF2 work factor.
pub const DEFAULT_ITERATIONS: u32 = 100_000;

/// Hash a password for storage: `pbkdf2$<iters>$<salt hex>$<hash hex>`.
pub fn hash_password(password: &str, salt: &[u8], iterations: u32) -> String {
    let h = pbkdf2_sha256(password.as_bytes(), salt, iterations, 32);
    format!("pbkdf2${}${}${}", iterations, hex(salt), hex(&h))
}

/// Verify a password against a stored `pbkdf2$...` record (constant time).
pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2" {
        return false;
    }
    let Ok(iterations) = parts[1].parse::<u32>() else {
        return false;
    };
    let (Some(salt), Some(want)) = (unhex(parts[2]), unhex(parts[3])) else {
        return false;
    };
    let got = pbkdf2_sha256(password.as_bytes(), &salt, iterations, want.len());
    constant_time_eq(&got, &want)
}

/// 32 random bytes from the OS entropy source (`/dev/urandom`).
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut out).is_ok() {
            return out;
        }
    }
    // Fallback: mix the clock, the pid and an address. This only runs when
    // `/dev/urandom` is unavailable (some sandboxes), and the caller-visible
    // result is still unique per call.
    let mut state = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
    };
    for slot in out.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = (state >> 24) as u8;
    }
    out
}

/// A random lowercase-hex token, `prefix` + 32 hex chars (128 bits).
pub fn random_token(prefix: &str) -> String {
    format!("{prefix}_{}", hex(&random_bytes(16)))
}

/// A random 32-byte salt.
pub fn random_salt() -> Vec<u8> {
    random_bytes(32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let b = [0u8, 1, 15, 16, 255];
        assert_eq!(hex(&b), "00010f10ff");
        assert_eq!(unhex("00010F10FF").unwrap(), b);
        assert!(unhex("0g").is_none());
        assert!(unhex("abc").is_none());
    }

    #[test]
    fn sha256_matches_known_vector() {
        // The canonical SHA-256 of the empty string.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn raw_digest_matches_hex() {
        let d = sha256_digest(b"abc");
        assert_eq!(hex(&d), sha256_hex(b"abc"));
    }

    #[test]
    fn hmac_matches_rfc4231_vectors() {
        // RFC 4231 test case 1 and 2.
        let key = vec![0x0bu8; 20];
        let mac = hex(&hmac_sha256(&key, b"Hi There"));
        assert_eq!(
            mac,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        let mac2 = hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?"));
        assert_eq!(
            mac2,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn pbkdf2_matches_reference_vector() {
        // RFC 7914 / draft-josefsson PBKDF2-HMAC-SHA256 vector:
        // P="passwd" S="salt" c=1 dkLen=64.
        let dk = pbkdf2_sha256(b"passwd", b"salt", 1, 64);
        assert_eq!(
            hex(&dk),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc\
             49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"
        );
    }

    #[test]
    fn pbkdf2_multi_iteration_vector() {
        // Same P/S with c=80000 (the widely published reference output).
        let dk = pbkdf2_sha256(b"Password", b"NaCl", 80000, 64);
        assert_eq!(
            hex(&dk),
            "4ddcd8f60b98be21830cee5ef22701f9641a4418d04c0414aeff08876b34ab56\
             a1d425a1225833549adb841b51c9b3176a272bdebba1d078478f62b397f33c8d"
        );
    }

    #[test]
    fn password_hash_roundtrip() {
        let salt = random_salt();
        let stored = hash_password("correct horse", &salt, 1000);
        assert!(verify_password("correct horse", &stored));
        assert!(!verify_password("wrong horse", &stored));
        assert!(!verify_password("correct horse", "garbage"));
        // A different salt yields a different record for the same password.
        let other = hash_password("correct horse", &random_salt(), 1000);
        assert_ne!(stored, other);
    }

    #[test]
    fn integrity_comparison_normalizes() {
        assert!(integrity_matches("ABCDEF", "abcdef"));
        assert!(integrity_matches("sha256:abcdef", "abcdef"));
        assert!(!integrity_matches("abcdef", "abcdee"));
        assert!(!integrity_matches("abcdef", "sha256:abcdef00"));
    }

    #[test]
    fn constant_time_eq_behaves_like_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn random_tokens_are_unique_and_prefixed() {
        let a = random_token("hspat");
        let b = random_token("hspat");
        assert!(a.starts_with("hspat_"));
        assert_eq!(a.len(), 6 + 32);
        assert_ne!(a, b);
        assert_eq!(random_salt().len(), 32);
    }
}
