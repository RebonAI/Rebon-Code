//! Error-message classification for native auto-updater failures.
//!
//! ## Behaviour
//!
//! The matcher is a cascade of case-sensitive substring checks over the
//! message. **Order matters**:
//!
//! * `"timeout"` → [`ErrorKind::Timeout`] (first, so a "timeout fetching
//!   from npm" error classifies as Timeout, not NpmError).
//! * `"Checksum mismatch"` → [`ErrorKind::ChecksumMismatch`].
//! * `"ENOENT"` OR `"not found"` → [`ErrorKind::NotFound`].
//! * `"EACCES"` OR `"permission"` → [`ErrorKind::PermissionDenied`].
//! * `"ENOSPC"` → [`ErrorKind::DiskFull`].
//! * `"npm"` → [`ErrorKind::NpmError`].
//! * `"network"` OR `"ECONNREFUSED"` OR `"ENOTFOUND"` →
//!   [`ErrorKind::NetworkError`].
//! * else → [`ErrorKind::Unknown`].
//!
//! Eight classes; eight enum variants. Every check is a
//! **case-sensitive substring** match.
//!
//! ## Why the order matters
//!
//! Several substrings overlap:
//!
//! * "ENOENT not found" matches both ENOENT and "not found", but
//!   they both classify as NotFound, so it doesn't matter.
//! * "npm timeout" matches both "timeout" and "npm" — the order
//!   makes it Timeout. Flipping the order would
//!   silently reclassify such failures.
//! * "EACCES permission denied" matches both EACCES and "permission"
//!   → PermissionDenied either way. No issue.
//!
//! The order is part of the contract; tests pin it explicitly with
//! ambiguous fixtures.

/// Categorised error type. Eight variants, one per class; each has a
/// stable snake_case name ([`ErrorKind::as_str`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Timeout,
    ChecksumMismatch,
    NotFound,
    PermissionDenied,
    DiskFull,
    NpmError,
    NetworkError,
    Unknown,
}

impl ErrorKind {
    /// The stable snake_case name for this variant.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::ChecksumMismatch => "checksum_mismatch",
            Self::NotFound => "not_found",
            Self::PermissionDenied => "permission_denied",
            Self::DiskFull => "disk_full",
            Self::NpmError => "npm_error",
            Self::NetworkError => "network_error",
            Self::Unknown => "unknown",
        }
    }
}

/// Classify an updater error message into an [`ErrorKind`].
///
/// The checks run in the order below; an ambiguous message reaches the
/// first matching one.
pub fn classify_error_message(message: &str) -> ErrorKind {
    if message.contains("timeout") {
        return ErrorKind::Timeout;
    }
    if message.contains("Checksum mismatch") {
        return ErrorKind::ChecksumMismatch;
    }
    if message.contains("ENOENT") || message.contains("not found") {
        return ErrorKind::NotFound;
    }
    if message.contains("EACCES") || message.contains("permission") {
        return ErrorKind::PermissionDenied;
    }
    if message.contains("ENOSPC") {
        return ErrorKind::DiskFull;
    }
    if message.contains("npm") {
        return ErrorKind::NpmError;
    }
    if message.contains("network")
        || message.contains("ECONNREFUSED")
        || message.contains("ENOTFOUND")
    {
        return ErrorKind::NetworkError;
    }
    ErrorKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Per-branch matches
    // -----------------------------------------------------------------

    #[test]
    fn timeout_is_first_branch() {
        assert_eq!(
            classify_error_message("Operation timeout after 5000ms"),
            ErrorKind::Timeout
        );
        assert_eq!(classify_error_message("timeout"), ErrorKind::Timeout);
    }

    #[test]
    fn checksum_mismatch_is_case_sensitive() {
        assert_eq!(
            classify_error_message("Checksum mismatch: expected abc got def"),
            ErrorKind::ChecksumMismatch
        );
        // Lowercase 'checksum' must NOT match (the `includes` check is
        // case-sensitive).
        assert_eq!(
            classify_error_message("checksum mismatch"),
            ErrorKind::Unknown
        );
    }

    #[test]
    fn enoent_classified_as_not_found() {
        assert_eq!(
            classify_error_message("ENOENT: no such file or directory"),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn not_found_string_classified_as_not_found() {
        assert_eq!(classify_error_message("404 not found"), ErrorKind::NotFound);
    }

    #[test]
    fn eacces_classified_as_permission_denied() {
        assert_eq!(
            classify_error_message("EACCES: write access denied"),
            ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn permission_string_classified_as_permission_denied() {
        // Lowercase 'permission' is the substring; uppercase
        // 'Permission' must not match.
        assert_eq!(
            classify_error_message("requires permission"),
            ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn permission_uppercase_does_not_match() {
        assert_eq!(
            classify_error_message("Permission denied"),
            ErrorKind::Unknown
        );
    }

    #[test]
    fn enospc_classified_as_disk_full() {
        assert_eq!(
            classify_error_message("ENOSPC: no space left on device"),
            ErrorKind::DiskFull
        );
    }

    #[test]
    fn npm_classified_as_npm_error() {
        assert_eq!(
            classify_error_message("npm install failed"),
            ErrorKind::NpmError
        );
    }

    #[test]
    fn network_classified_as_network_error() {
        assert_eq!(
            classify_error_message("network unreachable"),
            ErrorKind::NetworkError
        );
    }

    #[test]
    fn econnrefused_classified_as_network_error() {
        assert_eq!(
            classify_error_message("ECONNREFUSED: connection refused"),
            ErrorKind::NetworkError
        );
    }

    #[test]
    fn enotfound_classified_as_network_error() {
        assert_eq!(
            classify_error_message("ENOTFOUND: getaddrinfo failed"),
            ErrorKind::NetworkError
        );
    }

    #[test]
    fn unknown_returns_unknown() {
        assert_eq!(
            classify_error_message("something else broke"),
            ErrorKind::Unknown
        );
        assert_eq!(classify_error_message(""), ErrorKind::Unknown);
        assert_eq!(classify_error_message("EAGAIN"), ErrorKind::Unknown);
    }

    // -----------------------------------------------------------------
    // Order-sensitive ambiguous fixtures
    // -----------------------------------------------------------------

    #[test]
    fn timeout_beats_npm() {
        // "npm timeout" matches both 'timeout' and 'npm'. Order
        // says Timeout wins.
        assert_eq!(
            classify_error_message("npm install: request timeout"),
            ErrorKind::Timeout
        );
    }

    #[test]
    fn timeout_beats_network() {
        assert_eq!(
            classify_error_message("network timeout"),
            ErrorKind::Timeout
        );
    }

    #[test]
    fn checksum_beats_not_found() {
        // "Checksum mismatch ... not found" → ChecksumMismatch wins.
        assert_eq!(
            classify_error_message("Checksum mismatch: file not found"),
            ErrorKind::ChecksumMismatch
        );
    }

    #[test]
    fn not_found_beats_permission() {
        assert_eq!(
            classify_error_message("ENOENT: file not found, permission would be ok"),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn permission_beats_disk_full() {
        assert_eq!(
            classify_error_message("EACCES ... ENOSPC"),
            ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn disk_full_beats_npm() {
        // Without 'timeout' or 'Checksum mismatch' or 'ENOENT' or
        // 'EACCES' / 'permission', the next branch is ENOSPC.
        assert_eq!(
            classify_error_message("npm error: ENOSPC out of space"),
            ErrorKind::DiskFull
        );
    }

    #[test]
    fn npm_beats_network() {
        // Without timeout/checksum/not_found/permission/disk_full,
        // 'npm' wins over 'network'.
        assert_eq!(
            classify_error_message("npm fetch network error"),
            ErrorKind::NpmError
        );
    }

    // -----------------------------------------------------------------
    // String mapping
    // -----------------------------------------------------------------

    #[test]
    fn strings_are_pinned() {
        assert_eq!(ErrorKind::Timeout.as_str(), "timeout");
        assert_eq!(ErrorKind::ChecksumMismatch.as_str(), "checksum_mismatch");
        assert_eq!(ErrorKind::NotFound.as_str(), "not_found");
        assert_eq!(ErrorKind::PermissionDenied.as_str(), "permission_denied");
        assert_eq!(ErrorKind::DiskFull.as_str(), "disk_full");
        assert_eq!(ErrorKind::NpmError.as_str(), "npm_error");
        assert_eq!(ErrorKind::NetworkError.as_str(), "network_error");
        assert_eq!(ErrorKind::Unknown.as_str(), "unknown");
    }

    /// Exhaustive table covering each branch + each ambiguous
    /// fixture.
    #[test]
    fn classification_table() {
        let cases: &[(&str, ErrorKind)] = &[
            ("Operation timeout after 5000ms", ErrorKind::Timeout),
            ("Checksum mismatch", ErrorKind::ChecksumMismatch),
            ("ENOENT", ErrorKind::NotFound),
            ("not found", ErrorKind::NotFound),
            ("EACCES", ErrorKind::PermissionDenied),
            ("permission denied", ErrorKind::PermissionDenied),
            ("ENOSPC: no space left on device", ErrorKind::DiskFull),
            ("npm install failed", ErrorKind::NpmError),
            ("network error", ErrorKind::NetworkError),
            ("ECONNREFUSED", ErrorKind::NetworkError),
            ("ENOTFOUND", ErrorKind::NetworkError),
            ("something else", ErrorKind::Unknown),
            ("", ErrorKind::Unknown),
            // Order-sensitive
            ("npm install: request timeout", ErrorKind::Timeout),
            ("network timeout", ErrorKind::Timeout),
            (
                "Checksum mismatch: file not found",
                ErrorKind::ChecksumMismatch,
            ),
            ("ENOENT: file not found", ErrorKind::NotFound),
            ("EACCES with ENOSPC", ErrorKind::PermissionDenied),
            ("npm fetch network error", ErrorKind::NpmError),
            // Case-sensitive
            ("checksum mismatch", ErrorKind::Unknown),
            ("Permission denied", ErrorKind::Unknown),
            ("NPM is broken", ErrorKind::Unknown),
        ];
        for (msg, expected) in cases {
            assert_eq!(
                classify_error_message(msg),
                *expected,
                "classification table failed for {msg:?}",
            );
        }
    }
}
