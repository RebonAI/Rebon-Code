//! Updater dispatcher — picks which updater path to use.
//!
//! ## Rules
//!
//! Five branches, reduced to a pure function:
//!
//! 1. **Detection skipped (auto-updates disabled + flag on)** →
//!    `UpdaterChoice::None`. No updater is mounted.
//! 2. **Installation type detected as `native`** → `UpdaterChoice::Native`.
//! 3. **Installation type detected as `package-manager`** →
//!    `UpdaterChoice::PackageManager`.
//! 4. **Anything else** (`npm-global`, `npm-local`, `development`,
//!    `unknown`) → `UpdaterChoice::NpmJs`. This is the path that mounts
//!    the npm-based updater, which handles the `npm install -g` flow
//!    plus the config-based fallback for unknown.
//! 5. **Detection hasn't completed yet** — the caller passes
//!    `installation_type: None` → `UpdaterChoice::None`.
//!
//! ## Why feature-flag pre-resolution
//!
//! The skip-detection gate is a compile-time / runtime feature flag.
//! The dispatcher accepts a [`FeatureFlags`] record with the flag
//! pre-resolved by the caller, so this module doesn't need to know how
//! feature flags are plumbed (env var? config file? bundled constant?).
//! The feature-flag code is the natural owner of that.

use crate::installation_type::InstallationType;

/// Pre-resolved feature-flag inputs to the dispatcher. The caller
/// reads its feature-flag system once and constructs this struct;
/// the dispatcher branches on the booleans without knowing how they
/// were sourced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeatureFlags {
    /// `SKIP_DETECTION_WHEN_AUTOUPDATES_DISABLED` — when on, the
    /// detection phase is skipped if auto-updates are disabled, and
    /// the dispatcher returns `None` immediately. This is an
    /// optimisation that avoids the installation-type shell-out for
    /// users who have explicitly opted out.
    pub skip_detection_when_auto_updates_disabled: bool,
}

/// Result of the dispatcher — which concrete updater the updater UI
/// should mount, or `None` to mount nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpdaterChoice {
    /// Don't render any updater. Either detection hasn't completed,
    /// or the skip-detection optimisation kicked in, or the auto-
    /// updater is disabled.
    None,
    /// The native self-updater. The bundled-mode binary
    /// self-update path.
    Native,
    /// The package-manager info-only path: an
    /// install owned by a package manager is not updated in place, the
    /// user is only told that an update exists.
    PackageManager,
    /// The npm-based updater. This
    /// covers `npm-global`, `npm-local`, `development`, and
    /// `unknown` — anything not handled by the two specialised
    /// updaters.
    NpmJs,
}

/// The dispatcher. Inputs:
///
/// * `installation_type` — `Some(detected)` once the installation type
///   has been resolved, or `None` while detection is still pending.
/// * `auto_updater_disabled` — pre-resolved auto-update config flag.
/// * `flags` — pre-resolved feature flags ([`FeatureFlags`]).
///
/// Returns the [`UpdaterChoice`] the renderer should mount.
///
/// ## Branch ordering
///
/// The branches are checked in this order:
///
/// 1. Skip-detection optimisation: if both flag-on and disabled, the
///    dispatcher short-circuits to `None`.
/// 2. Detection-not-yet-complete: if `installation_type` is `None`,
///    the result is `None`.
/// 3. PackageManager wins over Native — the package-manager flag is
///    checked before the native/npm choice.
/// 4. Native vs NpmJs comes from whether the resolved type is
///    `native`.
/// 5. Default: NpmJs.
pub fn select_updater_path(
    installation_type: Option<InstallationType>,
    auto_updater_disabled: bool,
    flags: FeatureFlags,
) -> UpdaterChoice {
    // Branch 1: skip-detection optimisation.
    if flags.skip_detection_when_auto_updates_disabled && auto_updater_disabled {
        return UpdaterChoice::None;
    }

    // Branch 2: detection not yet complete.
    let Some(install_type) = installation_type else {
        return UpdaterChoice::None;
    };

    // Branches 3-5: classify the resolved installation type.
    match install_type {
        InstallationType::PackageManager => UpdaterChoice::PackageManager,
        InstallationType::Native => UpdaterChoice::Native,
        // npm-global, npm-local, development, unknown all fall
        // through to the npm updater, which handles each of those
        // internally — the dispatcher does not need to distinguish
        // them.
        InstallationType::NpmGlobal
        | InstallationType::NpmLocal
        | InstallationType::Development
        | InstallationType::Unknown => UpdaterChoice::NpmJs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_flags() -> FeatureFlags {
        FeatureFlags::default()
    }

    fn skip_flag_on() -> FeatureFlags {
        FeatureFlags {
            skip_detection_when_auto_updates_disabled: true,
        }
    }

    // -----------------------------------------------------------------
    // Branch 1: skip-detection optimisation
    // -----------------------------------------------------------------

    #[test]
    fn skip_flag_on_and_disabled_returns_none() {
        // The optimisation kicks in: even with a fully-resolved
        // installation type, the dispatcher returns None.
        assert_eq!(
            select_updater_path(Some(InstallationType::Native), true, skip_flag_on()),
            UpdaterChoice::None
        );
        assert_eq!(
            select_updater_path(Some(InstallationType::PackageManager), true, skip_flag_on()),
            UpdaterChoice::None
        );
        assert_eq!(
            select_updater_path(Some(InstallationType::NpmGlobal), true, skip_flag_on()),
            UpdaterChoice::None
        );
    }

    #[test]
    fn skip_flag_on_but_not_disabled_does_not_short_circuit() {
        // The flag alone isn't enough — both conditions must hold.
        assert_eq!(
            select_updater_path(Some(InstallationType::Native), false, skip_flag_on()),
            UpdaterChoice::Native
        );
    }

    #[test]
    fn disabled_without_flag_does_not_short_circuit() {
        // Without the feature flag, the disabled state alone doesn't
        // skip detection. The dispatcher still returns the
        // installation-type-based choice; the update-check routine
        // handles the disabled state.
        assert_eq!(
            select_updater_path(Some(InstallationType::Native), true, no_flags()),
            UpdaterChoice::Native
        );
    }

    // -----------------------------------------------------------------
    // Branch 2: detection not yet complete
    // -----------------------------------------------------------------

    #[test]
    fn detection_not_yet_complete_returns_none() {
        assert_eq!(
            select_updater_path(None, false, no_flags()),
            UpdaterChoice::None
        );
        // Even with the disabled flag set.
        assert_eq!(
            select_updater_path(None, true, no_flags()),
            UpdaterChoice::None
        );
    }

    // -----------------------------------------------------------------
    // Branches 3-5: per-installation-type classification
    // -----------------------------------------------------------------

    #[test]
    fn native_routes_to_native_updater() {
        assert_eq!(
            select_updater_path(Some(InstallationType::Native), false, no_flags()),
            UpdaterChoice::Native
        );
    }

    #[test]
    fn package_manager_routes_to_package_manager_updater() {
        assert_eq!(
            select_updater_path(Some(InstallationType::PackageManager), false, no_flags()),
            UpdaterChoice::PackageManager
        );
    }

    #[test]
    fn npm_global_routes_to_npm_js_updater() {
        assert_eq!(
            select_updater_path(Some(InstallationType::NpmGlobal), false, no_flags()),
            UpdaterChoice::NpmJs
        );
    }

    #[test]
    fn npm_local_routes_to_npm_js_updater() {
        assert_eq!(
            select_updater_path(Some(InstallationType::NpmLocal), false, no_flags()),
            UpdaterChoice::NpmJs
        );
    }

    #[test]
    fn development_routes_to_npm_js_updater() {
        // The development case is handled inside the npm updater
        // (it early-returns). The dispatcher still routes there — the
        // updater itself decides not to update.
        assert_eq!(
            select_updater_path(Some(InstallationType::Development), false, no_flags()),
            UpdaterChoice::NpmJs
        );
    }

    #[test]
    fn unknown_routes_to_npm_js_updater() {
        // Same shape as development — the npm updater handles
        // unknown via its config-based fallback.
        assert_eq!(
            select_updater_path(Some(InstallationType::Unknown), false, no_flags()),
            UpdaterChoice::NpmJs
        );
    }

    /// Exhaustive table covering every (installation_type, disabled,
    /// flag) tuple. The dispatcher's truth table is small enough that
    /// we can pin every cell.
    #[test]
    fn dispatcher_truth_table() {
        use InstallationType as IT;
        use UpdaterChoice as UC;

        // (install_type, disabled, flag, expected)
        let cases: &[(Option<InstallationType>, bool, bool, UpdaterChoice)] = &[
            // detection not yet done — None for any flag/disabled
            (None, false, false, UC::None),
            (None, true, false, UC::None),
            (None, false, true, UC::None),
            (None, true, true, UC::None),
            // Native
            (Some(IT::Native), false, false, UC::Native),
            (Some(IT::Native), true, false, UC::Native),
            (Some(IT::Native), false, true, UC::Native),
            (Some(IT::Native), true, true, UC::None), // skip-detection wins
            // PackageManager
            (Some(IT::PackageManager), false, false, UC::PackageManager),
            (Some(IT::PackageManager), true, false, UC::PackageManager),
            (Some(IT::PackageManager), false, true, UC::PackageManager),
            (Some(IT::PackageManager), true, true, UC::None),
            // NpmGlobal
            (Some(IT::NpmGlobal), false, false, UC::NpmJs),
            (Some(IT::NpmGlobal), true, false, UC::NpmJs),
            (Some(IT::NpmGlobal), false, true, UC::NpmJs),
            (Some(IT::NpmGlobal), true, true, UC::None),
            // NpmLocal
            (Some(IT::NpmLocal), false, false, UC::NpmJs),
            (Some(IT::NpmLocal), true, false, UC::NpmJs),
            (Some(IT::NpmLocal), false, true, UC::NpmJs),
            (Some(IT::NpmLocal), true, true, UC::None),
            // Development
            (Some(IT::Development), false, false, UC::NpmJs),
            (Some(IT::Development), true, false, UC::NpmJs),
            (Some(IT::Development), false, true, UC::NpmJs),
            (Some(IT::Development), true, true, UC::None),
            // Unknown
            (Some(IT::Unknown), false, false, UC::NpmJs),
            (Some(IT::Unknown), true, false, UC::NpmJs),
            (Some(IT::Unknown), false, true, UC::NpmJs),
            (Some(IT::Unknown), true, true, UC::None),
        ];

        for (install_type, disabled, flag, expected) in cases {
            let flags = FeatureFlags {
                skip_detection_when_auto_updates_disabled: *flag,
            };
            assert_eq!(
                select_updater_path(*install_type, *disabled, flags),
                *expected,
                "dispatcher table failed for ({install_type:?}, disabled={disabled}, flag={flag})",
            );
        }
    }
}
