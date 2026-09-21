//! Constant-time comparison for tokens and pairing codes.

/// Compare two strings without short-circuiting on the first
/// mismatching byte, so a caller that can retry quickly cannot learn
/// how long a matching prefix was. Unequal lengths still return early:
/// the length of a token is not the secret.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (left, right)| acc | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_strings_match() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn different_content_does_not_match() {
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "xbc"));
    }

    #[test]
    fn different_lengths_do_not_match() {
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("ab", "abc"));
    }
}
