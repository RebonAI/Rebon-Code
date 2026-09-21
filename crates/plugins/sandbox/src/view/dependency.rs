//! `SandboxDependencyCheck` shape + classification of dependency
//! errors into the four well-known buckets the UI cares about.
//!
//! ## Behavior notes
//!
//! * [`classify_errors`] — the four predicates that bucket the
//! `errors` array into `ripgrep_missing`, `bwrap_missing`,
//! `socat_missing`, and "other errors":
//!
//! ```text
//! rg_missing      = errors.iter().any(|e| e.contains("ripgrep"));
//! bwrap_missing   = errors.iter().any(|e| e.contains("bwrap"));
//! socat_missing   = errors.iter().any(|e| e.contains("socat"));
//! other_errors    = errors.iter().filter(|e| !e.contains("ripgrep")
//!                       && !e.contains("bwrap") && !e.contains("socat"));
//! seccomp_missing = warnings.len() > 0;
//! ```
//!
//! * [`ripgrep_install_hint`] — the install hint switch:
//!
//! ```text
//! if platform.is_mac() { "brew install ripgrep" } else { "apt install ripgrep" }
//! ```
//!
//! * Warnings rendering — the warnings list is mapped 1:1 into
//! `Text` rows; no filtering.
//!
//! * [`SandboxDependencyCheck::has_errors`] /
//! [`SandboxDependencyCheck::has_warnings`] — the doctor verdict
//! (split into the [`crate::view::doctor`] module).
//!
//! * [`SandboxDependencyCheck`] — the
//! return shape:
//!
//! ```text
//! { errors: Vec<String>, warnings: Vec<String> }
//! ```
//!
//! ## Pinned rules
//!
//! 1. **Bucketing is `String::contains`, NOT word-boundary or regex.**
//! `e.contains("ripgrep")`, `e.contains("bwrap")`, `e.contains("socat")`.
//! A message like `"failed to load bwrap-helper"` IS classified as
//! `bwrap_missing` even though `bwrap-helper` is a different binary.
//! A message like `"BWRAP missing"` is NOT classified — case
//! sensitive. Pinned by [`classify_errors`].
//! 2. **The `other_errors` bucket is "all errors that don't match any
//! of the three known buckets".** A single error containing both
//! `"bwrap"` and `"socat"` will be classified into BOTH known
//! buckets AND will NOT show up in `other_errors`. Pinned by
//! `classify_errors_message_in_two_known_buckets`.
//! 3. **Seccomp-missing is signalled by a non-empty `warnings`,
//! NOT by warning content.** Any non-empty warning list is
//! interpreted as "seccomp filter not installed". This is a
//! deliberate over-approximation. Pinned by
//! [`SandboxDependencyCheck::is_seccomp_missing`].
//! 4. **Install hint switches on platform.** macOS gets `brew`,
//! everything else gets `apt`. There is no `dnf` / `pacman` /
//! `pkg` branch. Pinned by
//! [`ripgrep_install_hint`].
//! 5. **Unsupported-platform error message is `"Unsupported
//! platform"`.** Pinned by [`UNSUPPORTED_PLATFORM_ERROR`].

use crate::view::platform::SandboxPlatform;

/// `"Unsupported platform"` — the literal error string returned when
/// the platform is unsupported.
pub const UNSUPPORTED_PLATFORM_ERROR: &str = "Unsupported platform";

/// `"ripgrep"` — substring used to bucket the rg-missing error.
pub const RIPGREP_ERROR_TOKEN: &str = "ripgrep";

/// `"bwrap"` — substring used to bucket the bwrap-missing error.
pub const BWRAP_ERROR_TOKEN: &str = "bwrap";

/// `"socat"` — substring used to bucket the socat-missing error.
pub const SOCAT_ERROR_TOKEN: &str = "socat";

/// `"brew install ripgrep"` — macOS install hint.
pub const RIPGREP_INSTALL_HINT_MAC: &str = "brew install ripgrep";

/// `"apt install ripgrep"` — non-mac install hint. Pinned literally.
pub const RIPGREP_INSTALL_HINT_NON_MAC: &str = "apt install ripgrep";

/// `"apt install bubblewrap"` — non-mac bwrap install hint, pinned
/// literally.
pub const BWRAP_INSTALL_HINT: &str = "apt install bubblewrap";

/// `"apt install socat"` — non-mac socat install hint, pinned
/// literally.
pub const SOCAT_INSTALL_HINT: &str = "apt install socat";

/// `SandboxDependencyCheck` shape: the two free-form message lists
/// the dependency check returns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxDependencyCheck {
    /// Each entry is a free-form error message; the UI
    /// classifies them by `String::contains`.
    pub errors: Vec<String>,
    /// Each entry is a free-form warning message. The
    /// presence of ANY warning is interpreted as "seccomp filter not
    /// installed".
    pub warnings: Vec<String>,
}

impl SandboxDependencyCheck {
    /// True when the check reported at least one error.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// True when the check reported at least one warning.
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// `seccomp_missing` — the seccomp filter counts as missing when
    /// `warnings.len() > 0`. Delegates to [`Self::has_warnings`].
    pub fn is_seccomp_missing(&self) -> bool {
        self.has_warnings()
    }

    /// Build a check whose only error is the unsupported-platform
    /// sentinel.
    pub fn unsupported_platform() -> Self {
        Self {
            errors: vec![UNSUPPORTED_PLATFORM_ERROR.to_string()],
            warnings: Vec::new(),
        }
    }
}

/// Result of bucketing the `errors` array into the four
/// well-known buckets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DependencyClassification {
    /// Set when any error message contains `"ripgrep"`.
    pub ripgrep_missing: bool,
    /// Set when any error message contains `"bwrap"`.
    pub bwrap_missing: bool,
    /// Set when any error message contains `"socat"`.
    pub socat_missing: bool,
    /// Set when `warnings.len() > 0`.
    pub seccomp_missing: bool,
    /// `other_errors` — every error that does NOT contain `"ripgrep"`,
    /// `"bwrap"`, or `"socat"`. The vec is in the same order as the
    /// input `errors` list.
    pub other_errors: Vec<String>,
}

/// Bucket a [`SandboxDependencyCheck`] with the three `String::contains`
/// predicates above. Pure function; takes
/// the check by reference and clones the `other_errors` strings.
pub fn classify_errors(check: &SandboxDependencyCheck) -> DependencyClassification {
    let mut other = Vec::new();
    let mut rg = false;
    let mut bwrap = false;
    let mut socat = false;
    for e in &check.errors {
        let is_rg = e.contains(RIPGREP_ERROR_TOKEN);
        let is_bwrap = e.contains(BWRAP_ERROR_TOKEN);
        let is_socat = e.contains(SOCAT_ERROR_TOKEN);
        rg |= is_rg;
        bwrap |= is_bwrap;
        socat |= is_socat;
        if !is_rg && !is_bwrap && !is_socat {
            other.push(e.clone());
        }
    }
    DependencyClassification {
        ripgrep_missing: rg,
        bwrap_missing: bwrap,
        socat_missing: socat,
        seccomp_missing: check.is_seccomp_missing(),
        other_errors: other,
    }
}

/// Pick the ripgrep install hint based on platform.
pub fn ripgrep_install_hint(platform: SandboxPlatform) -> &'static str {
    if platform.is_mac() {
        RIPGREP_INSTALL_HINT_MAC
    } else {
        RIPGREP_INSTALL_HINT_NON_MAC
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(errors: &[&str], warnings: &[&str]) -> SandboxDependencyCheck {
        SandboxDependencyCheck {
            errors: errors.iter().map(|s| s.to_string()).collect(),
            warnings: warnings.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn empty_check_classifies_to_all_false() {
        let c = classify_errors(&check(&[], &[]));
        assert_eq!(c, DependencyClassification::default());
    }

    #[test]
    fn ripgrep_only() {
        let c = classify_errors(&check(&["ripgrep (rg) not found"], &[]));
        assert!(c.ripgrep_missing);
        assert!(!c.bwrap_missing);
        assert!(!c.socat_missing);
        assert!(!c.seccomp_missing);
        assert!(c.other_errors.is_empty());
    }

    #[test]
    fn bwrap_only() {
        let c = classify_errors(&check(&["bubblewrap (bwrap) not found"], &[]));
        assert!(!c.ripgrep_missing);
        assert!(c.bwrap_missing);
        assert!(!c.socat_missing);
    }

    #[test]
    fn socat_only() {
        let c = classify_errors(&check(&["socat not found"], &[]));
        assert!(c.socat_missing);
        assert!(!c.bwrap_missing);
    }

    #[test]
    fn unknown_error_is_other() {
        let c = classify_errors(&check(&["selinux denied"], &[]));
        assert!(!c.ripgrep_missing);
        assert!(!c.bwrap_missing);
        assert!(!c.socat_missing);
        assert_eq!(c.other_errors, vec!["selinux denied".to_string()]);
    }

    #[test]
    fn unsupported_platform_is_other_error() {
        // The `UNSUPPORTED_PLATFORM_ERROR` sentinel
        // does NOT contain ripgrep/bwrap/socat, so it falls into other.
        let c = classify_errors(&SandboxDependencyCheck::unsupported_platform());
        assert!(!c.ripgrep_missing);
        assert!(!c.bwrap_missing);
        assert!(!c.socat_missing);
        assert_eq!(c.other_errors, vec!["Unsupported platform".to_string()]);
    }

    #[test]
    fn warnings_present_means_seccomp_missing() {
        let c = classify_errors(&check(&[], &["seccomp filter missing"]));
        assert!(c.seccomp_missing);
        assert!(c.other_errors.is_empty());
    }

    #[test]
    fn warnings_only_no_errors() {
        let chk = check(&[], &["seccomp filter missing"]);
        assert!(chk.is_seccomp_missing());
        assert!(!chk.has_errors());
        assert!(chk.has_warnings());
    }

    #[test]
    fn classify_errors_message_in_two_known_buckets() {
        // SECURITY-RELEVANT: a single error containing both `bwrap`
        // and `socat` is reported in BOTH buckets and is NOT in
        // `other_errors`, mirroring the single-pass `String::contains`
        // scan in `classify_errors`.
        let c = classify_errors(&check(&["bwrap and socat both missing"], &[]));
        assert!(c.bwrap_missing);
        assert!(c.socat_missing);
        assert!(c.other_errors.is_empty());
    }

    #[test]
    fn classify_errors_substring_match_is_loose() {
        // `bwrap-helper` ALSO matches `bwrap`. The matcher uses
        // `String::contains`, which is substring not word-boundary.
        let c = classify_errors(&check(&["bwrap-helper failed to spawn"], &[]));
        assert!(c.bwrap_missing);
        assert!(c.other_errors.is_empty());
    }

    #[test]
    fn classify_errors_is_case_sensitive() {
        let c = classify_errors(&check(&["BWRAP missing"], &[]));
        // `String::contains` is case-sensitive.
        assert!(!c.bwrap_missing);
        assert_eq!(c.other_errors, vec!["BWRAP missing".to_string()]);
    }

    #[test]
    fn classify_errors_preserves_other_errors_order() {
        let c = classify_errors(&check(
            &["aaa", "ripgrep missing", "bbb", "bwrap missing", "ccc"],
            &[],
        ));
        assert!(c.ripgrep_missing);
        assert!(c.bwrap_missing);
        assert_eq!(
            c.other_errors,
            vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string()]
        );
    }

    #[test]
    fn install_hint_mac_uses_brew() {
        assert_eq!(
            ripgrep_install_hint(SandboxPlatform::Macos),
            "brew install ripgrep"
        );
    }

    #[test]
    fn install_hint_linux_uses_apt() {
        assert_eq!(
            ripgrep_install_hint(SandboxPlatform::Linux),
            "apt install ripgrep"
        );
    }

    #[test]
    fn install_hint_windows_uses_apt() {
        // macOS gets brew and everything else apt; non-mac all share
        // the apt hint, even on platforms that don't have apt.
        assert_eq!(
            ripgrep_install_hint(SandboxPlatform::Windows),
            "apt install ripgrep"
        );
    }

    #[test]
    fn install_hint_unknown_uses_apt() {
        assert_eq!(
            ripgrep_install_hint(SandboxPlatform::Unknown),
            "apt install ripgrep"
        );
    }

    #[test]
    fn unsupported_platform_constant_is_pinned() {
        assert_eq!(UNSUPPORTED_PLATFORM_ERROR, "Unsupported platform");
    }

    #[test]
    fn install_hint_constants_pinned() {
        assert_eq!(RIPGREP_INSTALL_HINT_MAC, "brew install ripgrep");
        assert_eq!(RIPGREP_INSTALL_HINT_NON_MAC, "apt install ripgrep");
        assert_eq!(BWRAP_INSTALL_HINT, "apt install bubblewrap");
        assert_eq!(SOCAT_INSTALL_HINT, "apt install socat");
    }

    #[test]
    fn classify_error_buckets_table() {
        // (errors, warnings, expected_rg, expected_bwrap, expected_socat,
        // expected_seccomp, expected_other)
        let table: Vec<(Vec<&str>, Vec<&str>, bool, bool, bool, bool, Vec<&str>)> = vec![
            // Empty
            (vec![], vec![], false, false, false, false, vec![]),
            // Just rg
            (
                vec!["ripgrep (rg) not found"],
                vec![],
                true,
                false,
                false,
                false,
                vec![],
            ),
            // Just bwrap
            (
                vec!["bubblewrap (bwrap) not found"],
                vec![],
                false,
                true,
                false,
                false,
                vec![],
            ),
            // Just socat
            (
                vec!["socat not found"],
                vec![],
                false,
                false,
                true,
                false,
                vec![],
            ),
            // Just seccomp warning
            (
                vec![],
                vec!["seccomp filter not installed"],
                false,
                false,
                false,
                true,
                vec![],
            ),
            // All four buckets
            (
                vec![
                    "ripgrep missing",
                    "bwrap missing",
                    "socat missing",
                    "selinux denied",
                ],
                vec!["seccomp warning"],
                true,
                true,
                true,
                true,
                vec!["selinux denied"],
            ),
            // Other only
            (
                vec!["unrelated error"],
                vec![],
                false,
                false,
                false,
                false,
                vec!["unrelated error"],
            ),
            // Unsupported platform
            (
                vec!["Unsupported platform"],
                vec![],
                false,
                false,
                false,
                false,
                vec!["Unsupported platform"],
            ),
        ];
        for (errors, warnings, rg, bw, sc, sec, other) in table {
            let chk = check(&errors, &warnings);
            let c = classify_errors(&chk);
            assert_eq!(c.ripgrep_missing, rg, "rg for {:?}", chk);
            assert_eq!(c.bwrap_missing, bw, "bwrap for {:?}", chk);
            assert_eq!(c.socat_missing, sc, "socat for {:?}", chk);
            assert_eq!(c.seccomp_missing, sec, "seccomp for {:?}", chk);
            assert_eq!(
                c.other_errors,
                other.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "other for {:?}",
                chk
            );
        }
    }
}
