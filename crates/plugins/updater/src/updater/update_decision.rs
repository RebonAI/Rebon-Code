//! Update-needed decision — the gate that decides whether to install.
//!
//! ## Rules
//!
//! Five conditions must all hold before an install is allowed:
//!
//! 1. auto-updates are not turned off in config;
//! 2. a current version is known — we know what's locally installed;
//! 3. a latest version is known — the fetch returned something;
//! 4. the current version is BELOW the latest version (`!gte`);
//! 5. the minimum version, when there is one, doesn't veto the candidate
//!    (see [`should_skip_version`]).
//!
//! Each failed condition corresponds to a typed reason for not updating.
//!
//! ## Why a typed enum
//!
//! Returning `bool` would lose the reason — and the reason is useful
//! (the renderer wants to display "auto-update disabled" differently
//! from "you're already up to date"). [`UpdateDecision`] carries one
//! variant per gate.

use crate::semver_compare::gte;
use crate::should_skip::should_skip_version;

/// Outcome of [`decide_update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDecision {
    /// All five gates passed. The caller should install
    /// `target_version` (which is exactly `latest_version`).
    Update { target_version: String },
    /// Auto-updates are turned off in config.
    AutoUpdatesDisabled,
    /// We don't have a local version string to compare against —
    /// the current version is absent or empty.
    NoCurrentVersion,
    /// The fetch for the latest version returned nothing.
    NoLatestVersion,
    /// `gte(current, latest)` is true — already at or above the
    /// candidate. Most common steady-state outcome.
    AlreadyAtOrAboveLatest,
    /// The minimum-version gate vetoed the candidate: the minimum
    /// version is above it.
    SkippedByMinimumVersion,
}

/// The update-needed gate.
///
/// Inputs:
///
/// * `auto_updates_disabled` — pre-resolved auto-update config flag.
/// * `current_version` — `Some(s)` if we know what's locally
///   installed, `None` otherwise. An empty string is treated as `None`.
/// * `latest_version` — `Some(s)` if the fetch returned a version,
///   `None` if it failed.
/// * `minimum_version` — the minimum version, if any. Forwarded to
///   [`should_skip_version`].
///
/// Returns the typed [`UpdateDecision`].
///
/// ## Branch ordering
///
/// The gates are checked in this order, and the first one to fail
/// determines the result:
///
/// 1. AutoUpdatesDisabled
/// 2. NoCurrentVersion
/// 3. NoLatestVersion
/// 4. AlreadyAtOrAboveLatest
/// 5. SkippedByMinimumVersion
/// 6. Update
///
/// A reordering would show up as a failure in the exhaustive decision
/// table in this module's tests.
pub fn decide_update(
    auto_updates_disabled: bool,
    current_version: Option<&str>,
    latest_version: Option<&str>,
    minimum_version: Option<&str>,
) -> UpdateDecision {
    if auto_updates_disabled {
        return UpdateDecision::AutoUpdatesDisabled;
    }

    let current = match current_version {
        Some(s) if !s.is_empty() => s,
        _ => return UpdateDecision::NoCurrentVersion,
    };

    let latest = match latest_version {
        Some(s) if !s.is_empty() => s,
        _ => return UpdateDecision::NoLatestVersion,
    };

    if gte(current, latest) {
        return UpdateDecision::AlreadyAtOrAboveLatest;
    }

    if should_skip_version(latest, minimum_version) {
        return UpdateDecision::SkippedByMinimumVersion;
    }

    UpdateDecision::Update {
        target_version: latest.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Branch 1: auto-updates disabled
    // -----------------------------------------------------------------

    #[test]
    fn auto_updates_disabled_short_circuits() {
        assert_eq!(
            decide_update(true, Some("1.0.0"), Some("2.0.0"), None),
            UpdateDecision::AutoUpdatesDisabled
        );
        // Even with all other inputs valid, the disabled gate wins.
        assert_eq!(
            decide_update(true, Some("1.0.0"), Some("2.0.0"), Some("0.5.0")),
            UpdateDecision::AutoUpdatesDisabled
        );
    }

    // -----------------------------------------------------------------
    // Branch 2: no current version
    // -----------------------------------------------------------------

    #[test]
    fn no_current_version_returns_no_current() {
        assert_eq!(
            decide_update(false, None, Some("2.0.0"), None),
            UpdateDecision::NoCurrentVersion
        );
    }

    #[test]
    fn empty_current_version_treated_as_none() {
        // An empty current version counts as no version.
        assert_eq!(
            decide_update(false, Some(""), Some("2.0.0"), None),
            UpdateDecision::NoCurrentVersion
        );
    }

    // -----------------------------------------------------------------
    // Branch 3: no latest version
    // -----------------------------------------------------------------

    #[test]
    fn no_latest_version_returns_no_latest() {
        assert_eq!(
            decide_update(false, Some("1.0.0"), None, None),
            UpdateDecision::NoLatestVersion
        );
    }

    #[test]
    fn empty_latest_version_treated_as_none() {
        assert_eq!(
            decide_update(false, Some("1.0.0"), Some(""), None),
            UpdateDecision::NoLatestVersion
        );
    }

    // -----------------------------------------------------------------
    // Branch 4: already at-or-above latest
    // -----------------------------------------------------------------

    #[test]
    fn equal_versions_already_at_or_above() {
        assert_eq!(
            decide_update(false, Some("1.0.0"), Some("1.0.0"), None),
            UpdateDecision::AlreadyAtOrAboveLatest
        );
    }

    #[test]
    fn current_higher_than_latest_already_at_or_above() {
        assert_eq!(
            decide_update(false, Some("2.0.0"), Some("1.0.0"), None),
            UpdateDecision::AlreadyAtOrAboveLatest
        );
    }

    // -----------------------------------------------------------------
    // Branch 5: minimum-version veto
    // -----------------------------------------------------------------

    #[test]
    fn minimum_version_above_latest_skips() {
        // current=1.0, latest=1.4, min=1.5 → 1.4 < 1.5 → skip.
        assert_eq!(
            decide_update(false, Some("1.0.0"), Some("1.4.0"), Some("1.5.0")),
            UpdateDecision::SkippedByMinimumVersion
        );
    }

    #[test]
    fn minimum_version_equal_to_latest_does_not_skip() {
        // latest = min → !gte = false → don't skip → Update.
        assert_eq!(
            decide_update(false, Some("1.0.0"), Some("1.5.0"), Some("1.5.0")),
            UpdateDecision::Update {
                target_version: "1.5.0".to_string()
            }
        );
    }

    // -----------------------------------------------------------------
    // Branch 6: success
    // -----------------------------------------------------------------

    #[test]
    fn happy_path_returns_update() {
        assert_eq!(
            decide_update(false, Some("1.0.0"), Some("2.0.0"), None),
            UpdateDecision::Update {
                target_version: "2.0.0".to_string()
            }
        );
    }

    #[test]
    fn happy_path_with_minimum_version_below_latest() {
        // min=1.5, latest=2.0 → 2.0 > 1.5 → don't skip → Update.
        assert_eq!(
            decide_update(false, Some("1.0.0"), Some("2.0.0"), Some("1.5.0")),
            UpdateDecision::Update {
                target_version: "2.0.0".to_string()
            }
        );
    }

    // -----------------------------------------------------------------
    // Pre-release / build metadata edges
    // -----------------------------------------------------------------

    #[test]
    fn pre_release_candidate_is_at_or_above_when_current_higher() {
        // current=1.5.0, latest=1.5.0-rc1
        // → gte(1.5.0, 1.5.0-rc1) = true → AlreadyAtOrAbove.
        assert_eq!(
            decide_update(false, Some("1.5.0"), Some("1.5.0-rc1"), None),
            UpdateDecision::AlreadyAtOrAboveLatest
        );
    }

    #[test]
    fn build_metadata_only_change_is_already_at_or_above() {
        // current=1.5.0+a, latest=1.5.0+b → metadata ignored,
        // they're equal → AlreadyAtOrAbove.
        assert_eq!(
            decide_update(false, Some("1.5.0+a"), Some("1.5.0+b"), None),
            UpdateDecision::AlreadyAtOrAboveLatest
        );
    }

    /// Exhaustive table covering each branch.
    #[test]
    fn decide_update_table() {
        // Type alias keeps clippy::type_complexity quiet without
        // hiding the shape from a reader.
        type Case = (
            bool,
            Option<&'static str>,
            Option<&'static str>,
            Option<&'static str>,
            UpdateDecision,
        );
        let cases: &[Case] = &[
            // Disabled wins everything
            (
                true,
                Some("1.0.0"),
                Some("2.0.0"),
                None,
                UpdateDecision::AutoUpdatesDisabled,
            ),
            (true, None, None, None, UpdateDecision::AutoUpdatesDisabled),
            // No current
            (
                false,
                None,
                Some("2.0.0"),
                None,
                UpdateDecision::NoCurrentVersion,
            ),
            (
                false,
                Some(""),
                Some("2.0.0"),
                None,
                UpdateDecision::NoCurrentVersion,
            ),
            // No latest
            (
                false,
                Some("1.0.0"),
                None,
                None,
                UpdateDecision::NoLatestVersion,
            ),
            (
                false,
                Some("1.0.0"),
                Some(""),
                None,
                UpdateDecision::NoLatestVersion,
            ),
            // Already at or above
            (
                false,
                Some("1.0.0"),
                Some("1.0.0"),
                None,
                UpdateDecision::AlreadyAtOrAboveLatest,
            ),
            (
                false,
                Some("2.0.0"),
                Some("1.0.0"),
                None,
                UpdateDecision::AlreadyAtOrAboveLatest,
            ),
            // Skipped by minimum
            (
                false,
                Some("1.0.0"),
                Some("1.4.0"),
                Some("1.5.0"),
                UpdateDecision::SkippedByMinimumVersion,
            ),
            // Update
            (
                false,
                Some("1.0.0"),
                Some("2.0.0"),
                None,
                UpdateDecision::Update {
                    target_version: "2.0.0".into(),
                },
            ),
            (
                false,
                Some("1.0.0"),
                Some("2.0.0"),
                Some("1.5.0"),
                UpdateDecision::Update {
                    target_version: "2.0.0".into(),
                },
            ),
        ];
        for (disabled, current, latest, minimum, expected) in cases {
            assert_eq!(
                decide_update(*disabled, *current, *latest, *minimum),
                *expected,
                "decide_update table failed for ({disabled}, {current:?}, {latest:?}, {minimum:?})",
            );
        }
    }
}
