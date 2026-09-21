//! Installation type — the discriminant the dispatcher branches on.
//!
//! ## Behaviour
//!
//! Six variants, mapped one-to-one onto the wire strings:
//!
//! ```text
//! npm-global | npm-local | native | package-manager | development | unknown
//! ```
//!
//! Those strings are the wire format and surface in:
//!
//! * Branches on `"development"`, `"npm-local"`, `"npm-global"`,
//! `"native"`, with `"unknown"` as the default fallback.
//! * Branches on `"native"` and `"package-manager"` to pick which
//! concrete updater to mount.
//! * The detection path that produces the value.
//!
//! [`parse_installation_type`] exists so a caller that reads the wire
//! format (a JSON config blob, a process-spawn IPC message) can
//! round-trip the
//! string into the typed enum without re-implementing the match.

/// Detected installation type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstallationType {
    /// Globally-installed via `npm install -g`.
    NpmGlobal,
    /// Locally-installed under `~/.rebon/local`.
    NpmLocal,
    /// Native standalone binary that replaces itself in place.
    Native,
    /// Installed via a system package manager (homebrew, winget,
    /// pacman, …). The updater is **info-only** here — the binary
    /// can't replace itself; the user has to run their package
    /// manager.
    PackageManager,
    /// Running from a development checkout — for example an executable
    /// under a workspace `target/debug` or `target/release` directory.
    /// The updater is notify-only and never tries to update.
    Development,
    /// Which kind this is could not be determined from the evidence.
    /// The selection falls back to the install-method config.
    Unknown,
}

impl InstallationType {
    /// The exact wire string for this variant.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NpmGlobal => "npm-global",
            Self::NpmLocal => "npm-local",
            Self::Native => "native",
            Self::PackageManager => "package-manager",
            Self::Development => "development",
            Self::Unknown => "unknown",
        }
    }
}

/// Parse the wire-format string back into the enum. Returns `None`
/// for any string that is not an installation-type wire value.
///
/// Matching is case-sensitive and exact: no trimming, no case folding —
/// see [`tests::case_sensitivity`].
pub fn parse_installation_type(s: &str) -> Option<InstallationType> {
    match s {
        "npm-global" => Some(InstallationType::NpmGlobal),
        "npm-local" => Some(InstallationType::NpmLocal),
        "native" => Some(InstallationType::Native),
        "package-manager" => Some(InstallationType::PackageManager),
        "development" => Some(InstallationType::Development),
        "unknown" => Some(InstallationType::Unknown),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_each_variant() {
        for variant in [
            InstallationType::NpmGlobal,
            InstallationType::NpmLocal,
            InstallationType::Native,
            InstallationType::PackageManager,
            InstallationType::Development,
            InstallationType::Unknown,
        ] {
            assert_eq!(
                parse_installation_type(variant.as_str()),
                Some(variant),
                "round-trip failed for {variant:?}",
            );
        }
    }

    #[test]
    fn unknown_string_returns_none() {
        // The "Unknown" *enum variant* is distinct from "an unknown
        // string". Strings that aren't in the union return None;
        // strings that are the literal "unknown" return
        // Some(Unknown).
        assert_eq!(parse_installation_type("not-a-thing"), None);
        assert_eq!(
            parse_installation_type("unknown"),
            Some(InstallationType::Unknown)
        );
    }

    #[test]
    fn empty_string_returns_none() {
        assert_eq!(parse_installation_type(""), None);
    }

    #[test]
    fn case_sensitivity() {
        // The updater behavior `equals` is case-sensitive.
        assert_eq!(parse_installation_type("Npm-Global"), None);
        assert_eq!(parse_installation_type("NATIVE"), None);
        assert_eq!(parse_installation_type("Package-Manager"), None);
    }

    #[test]
    fn hyphen_position_matters() {
        // The updater behavior values use single-hyphen positions. Off-by-one
        // typos must NOT match.
        assert_eq!(parse_installation_type("npmglobal"), None);
        assert_eq!(parse_installation_type("npm_global"), None);
        assert_eq!(parse_installation_type("npm  global"), None);
        assert_eq!(parse_installation_type("packagemanager"), None);
        assert_eq!(parse_installation_type("package_manager"), None);
    }

    #[test]
    fn whitespace_padding_returns_none() {
        // No trimming — strict equality.
        assert_eq!(parse_installation_type(" native"), None);
        assert_eq!(parse_installation_type("native "), None);
        assert_eq!(parse_installation_type("\tnative\n"), None);
    }

    /// Exhaustive table — `as_str()` must produce the exact
    /// wire strings, and `parse_installation_type` must
    /// be their inverse.
    #[test]
    fn wire_strings_round_trip_table() {
        let cases: &[(InstallationType, &str)] = &[
            (InstallationType::NpmGlobal, "npm-global"),
            (InstallationType::NpmLocal, "npm-local"),
            (InstallationType::Native, "native"),
            (InstallationType::PackageManager, "package-manager"),
            (InstallationType::Development, "development"),
            (InstallationType::Unknown, "unknown"),
        ];
        for (variant, expected_str) in cases {
            assert_eq!(variant.as_str(), *expected_str);
            assert_eq!(parse_installation_type(expected_str), Some(*variant));
        }
    }
}
