//! `should_skip_version` — minimum-version skip-gate.
//!
//! ## Rules
//!
//! Three pieces:
//!
//! 1. **No settings access** — the caller resolves the minimum version
//!    and passes the string in, so the function stays pure.
//! 2. **Absent or empty minimum → don't skip.** Both `None` and
//!    `Some("")` are treated as no-op inputs.
//! 3. **Skip iff target < minimum.** Expressed as `!gte(target, minimum)`
//!    using [`crate::semver_compare::gte`].
//!
//! The debug-log side effect is omitted from the pure function;
//! callers can layer it on top if they want.

use crate::semver_compare::gte;

/// Returns `true` iff the target version should be skipped because
/// `minimum_version` is higher.
///
/// Pass `None` when there is no minimum; pass `Some("")` if it is
/// explicitly the empty string — an empty minimum disables the gate, so it
/// is treated the same as `None`.
pub fn should_skip_version(target_version: &str, minimum_version: Option<&str>) -> bool {
    // Absent minimum → never skip.
    let Some(min) = minimum_version else {
        return false;
    };
    if min.is_empty() {
        // An empty minimum is no minimum.
        return false;
    }

    // Skip iff target < minimum.
    !gte(target_version, min)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Absent or empty minimum → never skip
    // -----------------------------------------------------------------

    #[test]
    fn no_minimum_version_does_not_skip() {
        assert!(!should_skip_version("1.0.0", None));
    }

    #[test]
    fn empty_minimum_version_does_not_skip() {
        // An empty minimum disables the gate, so the early-return fires.
        assert!(!should_skip_version("1.0.0", Some("")));
    }

    // -----------------------------------------------------------------
    // Real comparison
    // -----------------------------------------------------------------

    #[test]
    fn target_below_minimum_skips() {
        assert!(should_skip_version("1.0.0", Some("1.5.0")));
        assert!(should_skip_version("0.9.0", Some("1.0.0")));
    }

    #[test]
    fn target_equal_to_minimum_does_not_skip() {
        // gte returns true → !gte = false → don't skip.
        assert!(!should_skip_version("1.5.0", Some("1.5.0")));
    }

    #[test]
    fn target_above_minimum_does_not_skip() {
        assert!(!should_skip_version("2.0.0", Some("1.5.0")));
    }

    // -----------------------------------------------------------------
    // Pre-release & build metadata edges
    // -----------------------------------------------------------------

    #[test]
    fn pre_release_target_skipped_when_minimum_is_release() {
        // 1.5.0-rc1 < 1.5.0, so target should be skipped.
        assert!(should_skip_version("1.5.0-rc1", Some("1.5.0")));
    }

    #[test]
    fn release_target_not_skipped_when_minimum_is_pre_release() {
        // 1.5.0 > 1.5.0-rc1, so target should not be skipped.
        assert!(!should_skip_version("1.5.0", Some("1.5.0-rc1")));
    }

    #[test]
    fn build_metadata_does_not_affect_skip() {
        // 1.5.0+abc == 1.5.0+def, so target = minimum, !gte = false.
        assert!(!should_skip_version("1.5.0+abc", Some("1.5.0+def")));
    }

    #[test]
    fn stable_channel_switch_skips_downgrade() {
        // Intended for switching to the stable channel: the user can
        // stay on their current version until stable catches up,
        // preventing downgrades.
        //
        // Concrete scenario: user is on 1.5.0 (latest channel), and
        // switches to stable. Stable's latest is 1.4.0. User has
        // minimumVersion=1.5.0 set. The updater should skip the
        // 1.4.0 candidate.
        assert!(should_skip_version("1.4.0", Some("1.5.0")));
    }

    /// Exhaustive table.
    #[test]
    fn should_skip_table() {
        // (target, minimum, expected)
        let cases: &[(&str, Option<&str>, bool)] = &[
            // Absent or empty minimum → never skip
            ("1.0.0", None, false),
            ("1.0.0", Some(""), false),
            // Target < minimum → skip
            ("1.0.0", Some("1.5.0"), true),
            ("0.9.0", Some("1.0.0"), true),
            ("1.4.0", Some("1.5.0"), true),
            // Target == minimum → don't skip
            ("1.5.0", Some("1.5.0"), false),
            // Target > minimum → don't skip
            ("2.0.0", Some("1.5.0"), false),
            ("1.5.1", Some("1.5.0"), false),
            // Pre-release / build-metadata
            ("1.5.0-rc1", Some("1.5.0"), true),
            ("1.5.0", Some("1.5.0-rc1"), false),
            ("1.5.0+abc", Some("1.5.0"), false),
            ("1.5.0", Some("1.5.0+abc"), false),
        ];
        for (target, minimum, expected) in cases {
            assert_eq!(
                should_skip_version(target, *minimum),
                *expected,
                "should_skip table failed for target={target:?} minimum={minimum:?}",
            );
        }
    }
}
