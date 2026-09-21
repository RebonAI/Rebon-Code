//! Install-method selection — local vs global vs early-out.
//!
//! Four branches:
//!
//! 1. `npm-local` → local install method.
//! 2. `npm-global` → global install method.
//! 3. `native` → early-out, the wrapper should never have routed
//!    here. Modeled as a typed error so the caller can `match` on it.
//! 4. Anything else (`development`, `package-manager`, `unknown`) →
//!    fall back to the configured install method. If that's
//!    local, use local; otherwise global.
//!
//! `package-manager` is in branch 4 here in theory but in practice
//! the dispatcher (see [`crate::updater_path::select_updater_path`])
//! routes those to a different updater, so this function never sees
//! them. It still handles them via the fallback so the function is
//! total.
//!
//! ## Why a typed choice
//!
//! [`select_install_method`] reports [`InstallMethodChoice::UnexpectedNative`] on the
//! `native` branch — the caller just clears its in-progress flag and bails.
//! Returning a typed choice rather than nothing means the caller can't
//! accidentally drop that case.

use crate::installation_type::InstallationType;

/// Which install method should run — local or global.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstallMethod {
    /// The local install path under `~/.rebon/local`.
    Local,
    /// The global `npm install -g` path.
    Global,
}

/// The install method recorded in config, as the fallback branch sees
/// it. Pass `None` for the "not recorded" case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigInstallMethod {
    /// The user has migrated to the local install.
    Local,
    /// The user is on the legacy global install path.
    Global,
    /// The user is on the native install path. The fallback branch
    /// only checks for `Local`, so `Native` here behaves the same as
    /// `Global` — fall through to global. Modeled as a separate
    /// variant so a parser can preserve the information for downstream
    /// consumers.
    Native,
}

/// Result of [`select_install_method`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethodChoice {
    /// Install via the chosen method.
    Use(InstallMethod),
    /// The dispatcher routed `native` to the npm-based updater by
    /// mistake. Caller should clear its in-progress flag and return
    /// without running an install.
    UnexpectedNative,
}

/// Pure version of the install-method selection branch.
///
/// Inputs:
///
/// * `installation_type` — the detected installation type.
/// * `config_install_method` — the install method recorded in the
///   global config. `None` if none is recorded.
///
/// Returns the typed [`InstallMethodChoice`].
pub fn select_install_method(
    installation_type: InstallationType,
    config_install_method: Option<ConfigInstallMethod>,
) -> InstallMethodChoice {
    match installation_type {
        InstallationType::NpmLocal => InstallMethodChoice::Use(InstallMethod::Local),
        InstallationType::NpmGlobal => InstallMethodChoice::Use(InstallMethod::Global),
        InstallationType::Native => InstallMethodChoice::UnexpectedNative,
        // Fallback branch — `development`, `package-manager`,
        // `unknown`. The branch picks local only for `ConfigInstallMethod::Local`;
        // anything else (Global, Native, None) takes the
        // global branch.
        InstallationType::Development
        | InstallationType::PackageManager
        | InstallationType::Unknown => {
            if matches!(config_install_method, Some(ConfigInstallMethod::Local)) {
                InstallMethodChoice::Use(InstallMethod::Local)
            } else {
                InstallMethodChoice::Use(InstallMethod::Global)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Branch 1: npm-local → Local
    // -----------------------------------------------------------------

    #[test]
    fn npm_local_uses_local_method() {
        assert_eq!(
            select_install_method(InstallationType::NpmLocal, None),
            InstallMethodChoice::Use(InstallMethod::Local)
        );
        // Config field doesn't matter for explicit npm-local.
        assert_eq!(
            select_install_method(
                InstallationType::NpmLocal,
                Some(ConfigInstallMethod::Global)
            ),
            InstallMethodChoice::Use(InstallMethod::Local)
        );
    }

    // -----------------------------------------------------------------
    // Branch 2: npm-global → Global
    // -----------------------------------------------------------------

    #[test]
    fn npm_global_uses_global_method() {
        assert_eq!(
            select_install_method(InstallationType::NpmGlobal, None),
            InstallMethodChoice::Use(InstallMethod::Global)
        );
        assert_eq!(
            select_install_method(
                InstallationType::NpmGlobal,
                Some(ConfigInstallMethod::Local)
            ),
            InstallMethodChoice::Use(InstallMethod::Global)
        );
    }

    // -----------------------------------------------------------------
    // Branch 3: native → unexpected (early-out)
    // -----------------------------------------------------------------

    #[test]
    fn native_returns_unexpected() {
        assert_eq!(
            select_install_method(InstallationType::Native, None),
            InstallMethodChoice::UnexpectedNative
        );
        assert_eq!(
            select_install_method(InstallationType::Native, Some(ConfigInstallMethod::Local)),
            InstallMethodChoice::UnexpectedNative
        );
    }

    // -----------------------------------------------------------------
    // Branch 4: fallback for development / package-manager / unknown
    // -----------------------------------------------------------------

    #[test]
    fn unknown_with_no_config_uses_global() {
        assert_eq!(
            select_install_method(InstallationType::Unknown, None),
            InstallMethodChoice::Use(InstallMethod::Global)
        );
    }

    #[test]
    fn unknown_with_config_local_uses_local() {
        assert_eq!(
            select_install_method(InstallationType::Unknown, Some(ConfigInstallMethod::Local)),
            InstallMethodChoice::Use(InstallMethod::Local)
        );
    }

    #[test]
    fn unknown_with_config_global_uses_global() {
        assert_eq!(
            select_install_method(InstallationType::Unknown, Some(ConfigInstallMethod::Global)),
            InstallMethodChoice::Use(InstallMethod::Global)
        );
    }

    #[test]
    fn unknown_with_config_native_uses_global() {
        // The branch picks local only for `ConfigInstallMethod::Local`, so anything else (including
        // 'native' in the config field) falls through to global.
        assert_eq!(
            select_install_method(InstallationType::Unknown, Some(ConfigInstallMethod::Native)),
            InstallMethodChoice::Use(InstallMethod::Global)
        );
    }

    #[test]
    fn development_falls_back_like_unknown() {
        // Development is in the fallback branch even though
        // an earlier guard upstream. That
        // guard runs BEFORE this function;
        // here we test only the install-method selection.
        assert_eq!(
            select_install_method(InstallationType::Development, None),
            InstallMethodChoice::Use(InstallMethod::Global)
        );
        assert_eq!(
            select_install_method(
                InstallationType::Development,
                Some(ConfigInstallMethod::Local)
            ),
            InstallMethodChoice::Use(InstallMethod::Local)
        );
    }

    #[test]
    fn package_manager_falls_back_like_unknown() {
        // package-manager shouldn't reach this function in production
        // (the dispatcher routes it elsewhere) but the function is
        // total — it falls back to the config-based path.
        assert_eq!(
            select_install_method(InstallationType::PackageManager, None),
            InstallMethodChoice::Use(InstallMethod::Global)
        );
        assert_eq!(
            select_install_method(
                InstallationType::PackageManager,
                Some(ConfigInstallMethod::Local)
            ),
            InstallMethodChoice::Use(InstallMethod::Local)
        );
    }

    /// Exhaustive table over the (installation_type × config) cross
    /// product.
    #[test]
    fn install_method_choice_table() {
        use ConfigInstallMethod as C;
        use InstallMethod as M;
        use InstallMethodChoice as Choice;
        use InstallationType as IT;

        let cases: &[(IT, Option<ConfigInstallMethod>, InstallMethodChoice)] = &[
            // npm-local always Local
            (IT::NpmLocal, None, Choice::Use(M::Local)),
            (IT::NpmLocal, Some(C::Local), Choice::Use(M::Local)),
            (IT::NpmLocal, Some(C::Global), Choice::Use(M::Local)),
            (IT::NpmLocal, Some(C::Native), Choice::Use(M::Local)),
            // npm-global always Global
            (IT::NpmGlobal, None, Choice::Use(M::Global)),
            (IT::NpmGlobal, Some(C::Local), Choice::Use(M::Global)),
            (IT::NpmGlobal, Some(C::Global), Choice::Use(M::Global)),
            (IT::NpmGlobal, Some(C::Native), Choice::Use(M::Global)),
            // native always UnexpectedNative
            (IT::Native, None, Choice::UnexpectedNative),
            (IT::Native, Some(C::Local), Choice::UnexpectedNative),
            (IT::Native, Some(C::Global), Choice::UnexpectedNative),
            (IT::Native, Some(C::Native), Choice::UnexpectedNative),
            // development falls back
            (IT::Development, None, Choice::Use(M::Global)),
            (IT::Development, Some(C::Local), Choice::Use(M::Local)),
            (IT::Development, Some(C::Global), Choice::Use(M::Global)),
            (IT::Development, Some(C::Native), Choice::Use(M::Global)),
            // package-manager falls back
            (IT::PackageManager, None, Choice::Use(M::Global)),
            (IT::PackageManager, Some(C::Local), Choice::Use(M::Local)),
            (IT::PackageManager, Some(C::Global), Choice::Use(M::Global)),
            (IT::PackageManager, Some(C::Native), Choice::Use(M::Global)),
            // unknown falls back
            (IT::Unknown, None, Choice::Use(M::Global)),
            (IT::Unknown, Some(C::Local), Choice::Use(M::Local)),
            (IT::Unknown, Some(C::Global), Choice::Use(M::Global)),
            (IT::Unknown, Some(C::Native), Choice::Use(M::Global)),
        ];
        for (it, config, expected) in cases {
            assert_eq!(
                select_install_method(*it, *config),
                *expected,
                "install method table failed for ({it:?}, {config:?})",
            );
        }
    }
}
