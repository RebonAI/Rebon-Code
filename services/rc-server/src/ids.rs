//! Identifier and bearer-token primitives.
//!
//! Every RC credential is the same shape as a relay token: 32 random
//! bytes rendered as unpadded base64url (43 characters). Keeping one
//! shape means one parser ([`bearer`]) validates every credential, and
//! a token that is syntactically valid for one class cannot be
//! *semantically* confused with another because each class is hashed
//! under its own domain separator (see [`crate::auth`]).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{SecondsFormat, TimeZone, Utc};
use hmac::{Hmac, Mac};
use http::{header, HeaderMap};
use rand::{rngs::OsRng, RngCore};
use sha2::Sha256;

/// HMAC-SHA256 digest of a credential. Never the credential itself.
pub type TokenDigest = [u8; 32];

/// Characters in the canonical unpadded base64url form of 32 bytes.
pub const TOKEN_CHARS: usize = 43;
/// Characters in the canonical unpadded base64url form of 16 bytes.
const ID_CHARS: usize = 22;

/// Mint a fresh 32-byte credential, returned in its wire form.
///
/// The raw bytes are zeroed before returning so the only copy left in
/// memory is the string the caller is about to hand to the client.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    bytes.fill(0);
    token
}

/// Mint a fresh prefixed resource id, e.g. `env_xS1…`.
///
/// Resource ids are not credentials — they appear in URLs and logs —
/// but they are still 128 bits of entropy so they cannot be guessed or
/// enumerated.
pub fn generate_id(prefix: &str) -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    format!("{prefix}_{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// Whether `value` is a well-formed prefixed id minted by [`generate_id`].
///
/// Path parameters are checked with this before they reach a query, so
/// a malformed id is rejected as a bad request rather than becoming a
/// wildcard in the database.
pub fn valid_id(value: &str, prefix: &str) -> bool {
    let Some(rest) = value.strip_prefix(prefix).and_then(|r| r.strip_prefix('_')) else {
        return false;
    };
    rest.len() == ID_CHARS
        && URL_SAFE_NO_PAD
            .decode(rest)
            .is_ok_and(|bytes| bytes.len() == 16 && URL_SAFE_NO_PAD.encode(&bytes) == rest)
}

/// Keyed digest with a domain separator, so the same token bytes hash
/// differently per credential class.
pub fn domain_digest(key: &[u8; 32], domain: &[u8], value: &[u8]) -> TokenDigest {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key");
    mac.update(domain);
    mac.update(&[0]);
    mac.update(value);
    mac.finalize().into_bytes().into()
}

/// Extract a canonical 32-byte bearer credential from the request headers.
///
/// Returns `None` for a missing header, a duplicated header, a
/// non-`Bearer` scheme, or anything that is not exactly the canonical
/// unpadded base64url encoding of 32 bytes.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let token = value.strip_prefix("Bearer ")?;
    canonical_token(token).then_some(token)
}

/// Whether `token` is the canonical unpadded base64url form of 32 bytes.
pub fn canonical_token(token: &str) -> bool {
    if token.len() != TOKEN_CHARS {
        return false;
    }
    URL_SAFE_NO_PAD
        .decode(token)
        .is_ok_and(|bytes| bytes.len() == 32 && URL_SAFE_NO_PAD.encode(&bytes) == token)
}

/// Decode a validated credential into its raw bytes for hashing.
pub fn token_bytes(token: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD
        .decode(token)
        .expect("token was validated as canonical base64url")
}

/// Current wall-clock time as a Unix timestamp in seconds.
pub fn now_unix() -> i64 {
    Utc::now().timestamp()
}

/// Render a Unix timestamp as the ISO-8601 form the bridge protocol uses.
pub fn rfc3339(unix: i64) -> String {
    Utc.timestamp_opt(unix, 0)
        .single()
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// First 8 bytes of a digest, hex-encoded — a stable correlation handle
/// safe to log in place of the credential it was derived from.
pub fn hex8(bytes: &[u8]) -> String {
    bytes[..8].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_canonical_bearers() {
        let token = generate_token();
        assert_eq!(token.len(), TOKEN_CHARS);
        assert!(canonical_token(&token));
    }

    #[test]
    fn generated_ids_validate_under_their_own_prefix_only() {
        let id = generate_id("env");
        assert!(valid_id(&id, "env"));
        assert!(!valid_id(&id, "dev"));
        assert!(!valid_id("env_short", "env"));
        assert!(!valid_id("nonsense", "env"));
    }

    #[test]
    fn domain_separation_changes_the_digest() {
        let key = [7u8; 32];
        let value = b"same-token-bytes";
        assert_ne!(
            domain_digest(&key, b"a", value),
            domain_digest(&key, b"b", value)
        );
    }

    #[test]
    fn bearer_rejects_non_canonical_and_duplicated_headers() {
        let token = generate_token();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().expect("header"),
        );
        assert_eq!(bearer(&headers), Some(token.as_str()));

        headers.append(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().expect("header"),
        );
        assert_eq!(bearer(&headers), None);

        let mut short = HeaderMap::new();
        short.insert(header::AUTHORIZATION, "Bearer abc".parse().expect("header"));
        assert_eq!(bearer(&short), None);
    }

    #[test]
    fn rfc3339_uses_millisecond_precision_and_a_z_suffix() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
    }
}
