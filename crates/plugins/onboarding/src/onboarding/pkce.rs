//! PKCE (Proof Key for Code Exchange, RFC 7636) crypto.
//!
//! Three pieces, all built on base64url without `=` padding:
//!
//! ```text
//! encode_verifier(32 random bytes) -> base64url, 43 chars
//! encode_state(32 random bytes)    -> base64url, 43 chars
//! code_challenge_s256(verifier)    -> base64url(sha256(verifier as UTF-8 bytes))
//! ```
//!
//! ## Pinned rules
//!
//! 1. The verifier and state are base64url(32 random bytes) — 43
//! characters of `[A-Za-z0-9_-]`. No `=` padding.
//! 2. The challenge is `base64url(sha256(verifier_text))` where
//! `verifier_text` is the ALREADY-encoded verifier string fed in
//! as UTF-8 bytes (NOT the raw 32 random bytes). This is a
//! deliberate quirk: the hash is taken over the encoded string,
//! not the raw bytes. RFC 7636 requires hashing the verifier
//! octets, but since the verifier IS ASCII the two are identical.
//! 3. Random bytes come in from the caller so the crate
//! stays deterministic in tests.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

/// Encode 32 raw random bytes as a PKCE code verifier.
///
/// The caller is responsible for the randomness. Pass 32 fresh
/// bytes — from `OsRng.fill_bytes` or a deterministic test fixture.
pub fn encode_verifier(random_bytes: [u8; 32]) -> String {
    URL_SAFE_NO_PAD.encode(random_bytes)
}

/// Encode 32 raw random bytes as a PKCE state parameter.
///
/// Same encoding as
/// [`encode_verifier`] but a separate function so the caller MUST
/// draw fresh randomness for each (the values must be independent).
pub fn encode_state(random_bytes: [u8; 32]) -> String {
    URL_SAFE_NO_PAD.encode(random_bytes)
}

/// Compute the PKCE S256 code challenge for a given verifier.
///
/// The result is `base64url(sha256(verifier))` with the URL-safe
/// alphabet and no `=` padding.
pub fn code_challenge_s256(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    URL_SAFE_NO_PAD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned: encoding 32 zero bytes produces a known fixed string.
    /// This is the simplest reference vector and exercises the
    /// no-padding rule.
    #[test]
    fn encode_verifier_zero_bytes() {
        let v = encode_verifier([0u8; 32]);
        assert_eq!(v, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(v.len(), 43);
        assert!(!v.contains('='));
        assert!(!v.contains('+'));
        assert!(!v.contains('/'));
    }

    /// Pinned: encoding 32 max bytes (`0xff`) produces a known fixed
    /// string with no `=` padding and the URL-safe alphabet.
    #[test]
    fn encode_verifier_all_ones() {
        let v = encode_verifier([0xffu8; 32]);
        // Standard base64 of 32 0xff bytes is "//////////////////////////////////////////8="
        // URL-safe no-pad replaces `/` with `_` and drops `=`.
        assert_eq!(v, "__________________________________________8");
        assert_eq!(v.len(), 43);
    }

    /// Pinned: state and verifier use the SAME encoding function but
    /// are separate functions so independent randomness can be
    /// enforced.
    #[test]
    fn encode_state_uses_same_encoding_as_verifier() {
        let bytes = [0xab; 32];
        assert_eq!(encode_state(bytes), encode_verifier(bytes));
    }

    /// Pinned: PKCE S256 reference vector from RFC 7636 section 4.2.
    /// Verifier `dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk` →
    /// challenge `E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM`.
    #[test]
    fn rfc7636_reference_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = code_challenge_s256(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    /// Pinned: the challenge length is always 43 chars (256 bits ÷ 6
    /// bits per base64 char = 42.67 → 43, no padding).
    #[test]
    fn challenge_length_is_always_43() {
        for input in [
            "",
            "a",
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
            "x".repeat(1024).as_str(),
        ] {
            let c = code_challenge_s256(input);
            assert_eq!(c.len(), 43, "input length {}", input.len());
        }
    }

    /// Pinned: the empty-string verifier hashes to a known value.
    /// `sha256("")` is the well-known empty-string SHA-256.
    #[test]
    fn challenge_for_empty_verifier() {
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        // base64url no-pad = 47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU
        assert_eq!(
            code_challenge_s256(""),
            "47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU"
        );
    }

    /// Pinned: the challenge alphabet is URL-safe (no `+`, `/`, or
    /// `=`).
    #[test]
    fn challenge_alphabet_is_url_safe() {
        let bytes = [
            0xfb, 0xff, 0xbf, 0x00, 0x10, 0x83, 0x10, 0x51, 0x87, 0x20, 0x92, 0x8b, 0x30, 0xd3,
            0x8f, 0x41, 0x14, 0x93, 0x51, 0x55, 0x97, 0x61, 0x96, 0x9b, 0x71, 0xd7, 0x9f, 0x82,
            0x18, 0xa3, 0x92, 0x59,
        ];
        let v = encode_verifier(bytes);
        let c = code_challenge_s256(&v);
        for ch in v.chars().chain(c.chars()) {
            assert!(
                ch.is_ascii_alphanumeric() || ch == '-' || ch == '_',
                "invalid char: {ch}"
            );
        }
    }

    /// Pinned: the challenge is deterministic — calling it twice on
    /// the same verifier returns the same value.
    #[test]
    fn challenge_is_deterministic() {
        let v = "test_verifier_string_42";
        assert_eq!(code_challenge_s256(v), code_challenge_s256(v));
    }

    /// Pinned: the challenge is sensitive to the input — a one-byte
    /// difference in the verifier flips the entire challenge.
    #[test]
    fn challenge_changes_with_verifier() {
        let a = code_challenge_s256("verifier_a");
        let b = code_challenge_s256("verifier_b");
        assert_ne!(a, b);
    }

    /// Pinned: a non-ASCII verifier still hashes (`as_bytes` UTF-8
    /// encodes it, which is what hashing the verifier string does
    /// by default).
    #[test]
    fn challenge_handles_non_ascii_verifier() {
        // Should not panic and should produce a valid 43-char URL-safe string.
        let c = code_challenge_s256("verifier_with_émojis_🔐");
        assert_eq!(c.len(), 43);
    }
}
