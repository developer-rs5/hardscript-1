//! Base64 (RFC 4648 standard alphabet, with padding).
//!
//! Publish envelopes carry the `.hspkg` bytes and signatures are exchanged as
//! base64, so the registry needs its own codec rather than a dependency. The
//! decoder is strict: unknown characters, bad padding and interior padding
//! are all errors, because a publish body must never be silently mangled.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes as base64 with `=` padding.
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn value(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 26),
        b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decode standard base64. Returns `None` on any malformed input.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    let mut i = 0;
    while i < b.len() {
        let quad = &b[i..i + 4];
        let last = i + 4 == b.len();
        let mut acc = 0u32;
        let mut pad = 0usize;
        for (j, &c) in quad.iter().enumerate() {
            if c == b'=' {
                // Padding is only legal in the final quad's last two slots.
                if !last || j < 2 {
                    return None;
                }
                pad += 1;
                acc <<= 6;
            } else {
                if pad > 0 {
                    return None;
                }
                acc = (acc << 6) | value(c)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_rfc4648_vectors() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(raw.as_bytes()), enc, "encoding {raw:?}");
            assert_eq!(decode(enc).as_deref(), Some(raw.as_bytes()), "decoding {enc:?}");
        }
    }

    #[test]
    fn roundtrips_binary_of_every_length_remainder() {
        for n in 0..40usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 7 % 256) as u8).collect();
            let enc = encode(&data);
            assert_eq!(decode(&enc).as_deref(), Some(data.as_slice()), "len {n}");
        }
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(decode("Zg=").is_none()); // length
        assert!(decode("Zg=x").is_none()); // bad length
        assert!(decode("Z===").is_none()); // too much padding
        assert!(decode("Zm=v").is_none()); // interior padding
        assert!(decode("Z m9v").is_none()); // space
        assert!(decode("****").is_none()); // out of alphabet
    }

    #[test]
    fn archive_bytes_survive_a_roundtrip() {
        let mut archive = b"HSPKG\x00\x01".to_vec();
        archive.extend_from_slice(&[0xff, 0x00, 0x7f, 0x80, 0x01]);
        assert_eq!(decode(&encode(&archive)).unwrap(), archive);
    }
}
