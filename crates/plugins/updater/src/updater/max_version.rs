//! Max-version cap — server-side kill switch for auto-updates.
//!
//! ## Rules
//!
//! Two pure functions consume an optional server-side max version:
//!
//! 1. [`apply_max_version_cap`] — for the npm / package-manager updaters.
//!    If there's a max version AND the candidate latest is above it, then
//!    either:
//!    * the current is already at-or-above the max → SKIP
//!      (no update);
//!    * else cap the candidate at the max version and proceed.
//!
//! 2. [`detect_native_max_version_warning`] — for the native updater,
//!    which only checks "is current ABOVE the max?" If so, it returns a
//!    warning-banner message. The native updater doesn't cap the
//!    candidate; the binary's own install path re-checks server-side.
//!
//! Both use the loose semver comparison in [`crate::semver_compare`].
//!
//! ## Why a typed decision rather than a tuple
//!
//! The cap outcome is returned as a typed [`MaxVersionDecision`] that
//! callers `match` on. This pins the possible outcomes (skip, cap,
//! proceed) in the type system — a caller can't accidentally drop one of
//! the cases.

use crate::semver_compare::{gt, gte};

/// Outcome of [`apply_max_version_cap`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaxVersionDecision {
    /// No max version is configured (or no `latest` to compare to).
    /// Proceed with the candidate `latest` unchanged.
    NoChange,
    /// The candidate `latest` is at-or-below the max — no cap
    /// needed. Proceed with the candidate unchanged.
    BelowOrAtMax,
    /// The candidate `latest` is above the max AND the current
    /// version is already at-or-above the max. Skip the update
    /// entirely — there's nothing safe to install.
    SkipAlreadyAtOrAboveMax,
    /// The candidate `latest` is above the max AND the current
    /// version is below the max. Cap the candidate to the max
    /// version and proceed with the install. The string is the
    /// max-version pin (= the new `latest_version`).
    Cap(String),
}

/// Apply the server-side max-version cap to a candidate `latest`.
///
/// Inputs:
///
/// * `current_version` — the locally-installed version.
/// * `latest_version` — the candidate version offered by the update
///   source. May be `None` if the fetch failed.
/// * `max_version` — the server-side cap. May be `None` if no cap is
///   configured.
///
/// Returns a [`MaxVersionDecision`] the caller branches on.
///
/// ## Missing-input handling
///
/// A missing max version OR a missing candidate makes the cap a no-op —
/// the candidate proceeds unchanged. Both inputs are modelled as
/// `Option`s, and `None` in either short-circuits to
/// [`MaxVersionDecision::NoChange`].
pub fn apply_max_version_cap(
    current_version: &str,
    latest_version: Option<&str>,
    max_version: Option<&str>,
) -> MaxVersionDecision {
    // Missing either input → no cap.
    let (Some(latest), Some(max)) = (latest_version, max_version) else {
        return MaxVersionDecision::NoChange;
    };

    if !gt(latest, max) {
        // Candidate is at-or-below the max — no cap needed.
        return MaxVersionDecision::BelowOrAtMax;
    }

    // Candidate is above the max → either skip or cap.
    if gte(current_version, max) {
        // Current is already at-or-above the max — nothing safe to
        // install. Skip.
        MaxVersionDecision::SkipAlreadyAtOrAboveMax
    } else {
        // Current is below the max — cap the candidate at the max
        // and let the install proceed.
        MaxVersionDecision::Cap(max.to_string())
    }
}

/// Warning-banner check for the native updater.
///
/// Returns `Some(msg)` if a warning banner should be shown, where `msg`
/// is the caller-supplied `message` or the `'affects your version'`
/// fallback when `message` is `None`. Returns `None` if no warning is
/// needed — no max version configured, or the current version is not
/// strictly above it.
///
/// `message` is fetched by the caller; this function only applies the
/// `'affects your version'` fallback.
pub fn detect_native_max_version_warning(
    current_version: &str,
    max_version: Option<&str>,
    message: Option<&str>,
) -> Option<String> {
    let max = max_version?;
    if !gt(current_version, max) {
        return None;
    }
    Some(message.unwrap_or("affects your version").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // apply_max_version_cap — missing-input guards
    // -----------------------------------------------------------------

    #[test]
    fn no_max_version_means_no_change() {
        assert_eq!(
            apply_max_version_cap("1.0.0", Some("2.0.0"), None),
            MaxVersionDecision::NoChange
        );
    }

    #[test]
    fn no_latest_version_means_no_change() {
        assert_eq!(
            apply_max_version_cap("1.0.0", None, Some("1.5.0")),
            MaxVersionDecision::NoChange
        );
    }

    #[test]
    fn no_max_and_no_latest_means_no_change() {
        assert_eq!(
            apply_max_version_cap("1.0.0", None, None),
            MaxVersionDecision::NoChange
        );
    }

    // -----------------------------------------------------------------
    // apply_max_version_cap — candidate within cap
    // -----------------------------------------------------------------

    #[test]
    fn candidate_below_max_proceeds_unchanged() {
        // latest = 1.5.0, max = 2.0.0 → no cap.
        assert_eq!(
            apply_max_version_cap("1.0.0", Some("1.5.0"), Some("2.0.0")),
            MaxVersionDecision::BelowOrAtMax
        );
    }

    #[test]
    fn candidate_equal_to_max_proceeds_unchanged() {
        // latest = 1.5.0, max = 1.5.0 → not greater, no cap.
        assert_eq!(
            apply_max_version_cap("1.0.0", Some("1.5.0"), Some("1.5.0")),
            MaxVersionDecision::BelowOrAtMax
        );
    }

    // -----------------------------------------------------------------
    // apply_max_version_cap — candidate above cap
    // -----------------------------------------------------------------

    #[test]
    fn candidate_above_max_caps_to_max_when_current_below() {
        // latest = 2.0.0, max = 1.5.0, current = 1.0.0
        // → cap latest to 1.5.0 and proceed.
        assert_eq!(
            apply_max_version_cap("1.0.0", Some("2.0.0"), Some("1.5.0")),
            MaxVersionDecision::Cap("1.5.0".to_string())
        );
    }

    #[test]
    fn candidate_above_max_skips_when_current_at_max() {
        // latest = 2.0.0, max = 1.5.0, current = 1.5.0 (== max)
        // → gte(current, max) is true → SKIP.
        assert_eq!(
            apply_max_version_cap("1.5.0", Some("2.0.0"), Some("1.5.0")),
            MaxVersionDecision::SkipAlreadyAtOrAboveMax
        );
    }

    #[test]
    fn candidate_above_max_skips_when_current_above_max() {
        // The pathological case: current is somehow above the max
        // (shouldn't happen in practice, but the function handles it).
        assert_eq!(
            apply_max_version_cap("1.7.0", Some("2.0.0"), Some("1.5.0")),
            MaxVersionDecision::SkipAlreadyAtOrAboveMax
        );
    }

    // -----------------------------------------------------------------
    // apply_max_version_cap — pre-release & build metadata edges
    // -----------------------------------------------------------------

    #[test]
    fn pre_release_candidate_below_release_max() {
        // latest = 1.5.0-rc1, max = 1.5.0, current = 1.0.0
        // → 1.5.0-rc1 < 1.5.0 (rc1 loses to release), so not gt
        //   → BelowOrAtMax.
        assert_eq!(
            apply_max_version_cap("1.0.0", Some("1.5.0-rc1"), Some("1.5.0")),
            MaxVersionDecision::BelowOrAtMax
        );
    }

    #[test]
    fn pre_release_candidate_above_release_max() {
        // latest = 1.6.0-rc1, max = 1.5.0, current = 1.0.0
        // → 1.6.0-rc1 > 1.5.0 (different majors), cap.
        assert_eq!(
            apply_max_version_cap("1.0.0", Some("1.6.0-rc1"), Some("1.5.0")),
            MaxVersionDecision::Cap("1.5.0".to_string())
        );
    }

    #[test]
    fn build_metadata_does_not_affect_cap() {
        // The +sha bits are stripped by the comparator, so they
        // shouldn't affect the cap decision either.
        assert_eq!(
            apply_max_version_cap("1.0.0+a", Some("2.0.0+b"), Some("1.5.0+c")),
            MaxVersionDecision::Cap("1.5.0+c".to_string())
        );
    }

    // -----------------------------------------------------------------
    // detect_native_max_version_warning
    // -----------------------------------------------------------------

    #[test]
    fn no_max_version_no_warning() {
        assert_eq!(detect_native_max_version_warning("1.0.0", None, None), None);
    }

    #[test]
    fn current_below_max_no_warning() {
        assert_eq!(
            detect_native_max_version_warning("1.0.0", Some("2.0.0"), None),
            None
        );
    }

    #[test]
    fn current_equal_to_max_no_warning() {
        // The check is strict greater-than, not at-or-above. Equal does
        // NOT trigger.
        assert_eq!(
            detect_native_max_version_warning("1.5.0", Some("1.5.0"), None),
            None
        );
    }

    #[test]
    fn current_above_max_with_no_message_uses_fallback() {
        assert_eq!(
            detect_native_max_version_warning("2.0.0", Some("1.5.0"), None),
            Some("affects your version".to_string())
        );
    }

    #[test]
    fn current_above_max_with_message_uses_message() {
        assert_eq!(
            detect_native_max_version_warning(
                "2.0.0",
                Some("1.5.0"),
                Some("known crash on Windows")
            ),
            Some("known crash on Windows".to_string())
        );
    }

    #[test]
    fn current_above_max_with_empty_message_still_uses_message() {
        // The `'affects your version'` fallback only fires when no message
        // is available — an empty string is a real message, not an absent
        // one. The Rust implementation preserves this: `Some("")` is taken
        // literally.
        //
        // This is a deliberate regression test — if a future refactor made
        // the fallback fire on an empty string, the test would catch it.
        assert_eq!(
            detect_native_max_version_warning("2.0.0", Some("1.5.0"), Some("")),
            Some("".to_string())
        );
    }

    /// Exhaustive table covering the main shapes of the cap decision.
    #[test]
    fn max_version_decision_table() {
        // (current, latest, max, expected)
        let cases: &[(&str, Option<&str>, Option<&str>, MaxVersionDecision)] = &[
            // Missing-input guards
            ("1.0.0", None, None, MaxVersionDecision::NoChange),
            ("1.0.0", Some("2.0.0"), None, MaxVersionDecision::NoChange),
            ("1.0.0", None, Some("1.5.0"), MaxVersionDecision::NoChange),
            // Below cap
            (
                "1.0.0",
                Some("1.4.0"),
                Some("1.5.0"),
                MaxVersionDecision::BelowOrAtMax,
            ),
            (
                "1.0.0",
                Some("1.5.0"),
                Some("1.5.0"),
                MaxVersionDecision::BelowOrAtMax,
            ),
            // Above cap, current below cap → cap
            (
                "1.0.0",
                Some("2.0.0"),
                Some("1.5.0"),
                MaxVersionDecision::Cap("1.5.0".into()),
            ),
            (
                "1.4.99",
                Some("2.0.0"),
                Some("1.5.0"),
                MaxVersionDecision::Cap("1.5.0".into()),
            ),
            // Above cap, current at-or-above cap → skip
            (
                "1.5.0",
                Some("2.0.0"),
                Some("1.5.0"),
                MaxVersionDecision::SkipAlreadyAtOrAboveMax,
            ),
            (
                "1.6.0",
                Some("2.0.0"),
                Some("1.5.0"),
                MaxVersionDecision::SkipAlreadyAtOrAboveMax,
            ),
        ];
        for (current, latest, max, expected) in cases {
            assert_eq!(
                apply_max_version_cap(current, *latest, *max),
                *expected,
                "max-version table failed for current={current:?} latest={latest:?} max={max:?}",
            );
        }
    }
}
