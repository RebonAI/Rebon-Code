//! Render visibility — whether each updater surface should draw at all.
//!
//! Three distinct visibility predicates, one per updater kind.
//!
//! Each predicate reduces to a small boolean expression over the typed
//! state. They are exposed as distinct functions so a renderer can ask
//! "should I draw the npm updater?" vs "should I draw the native
//! updater?" without re-implementing the predicate.
//!
//! ## Why three distinct predicates
//!
//! The three updater surfaces have different visibility rules:
//!
//! * **npm updater** — hides completely until EITHER a result exists
//!   OR we have both versions AND are actively updating. The two-layer
//!   `if` can be reduced to the single expression:
//!   `(has_update_result) || (is_updating && has_both_versions)`.
//! * **Native updater** — adds the max-version warning banner as a
//!   third visibility trigger.
//! * **Package-manager updater** — purely gated on "is there an
//!   update available?". No versions-in-flight state.
//!
//! Collapsing them into one predicate would lose the per-kind
//! visibility contract. UI code can hold one `RenderVisibility` per
//! kind and pick between them.

/// Pre-evaluated inputs to the visibility predicates. Keeping these
/// as a struct means a renderer can build one once per frame and
/// pass it to all three functions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderInputs {
    /// True iff a completed update result that carries a version is
    /// available; an absent result, or one with no version, is false.
    pub has_update_result: bool,
    /// True iff the updater is mid-install.
    pub is_updating: bool,
    /// True iff both the current (global) version and a latest version
    /// are known, so they can be compared. The npm and native updaters
    /// build this from the same two fields, so one flag covers both.
    pub has_both_versions: bool,
    /// True iff the native updater has a max-version issue to
    /// warn about. Only meaningful for the native updater;
    /// ignored by the other two predicates.
    pub has_max_version_issue: bool,
    /// True iff the package-manager updater has detected an
    /// update. Only meaningful for the package-manager predicate;
    /// ignored by the other two.
    pub update_available: bool,
}

/// Visibility decision. `Render` means "draw the surface";
/// `Hide` means "draw nothing".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderVisibility {
    Render,
    Hide,
}

impl RenderVisibility {
    pub fn is_visible(self) -> bool {
        matches!(self, Self::Render)
    }
}

/// Dispatcher entry point that picks the right per-kind predicate.
/// Callers usually know which updater they are rendering and can call
/// the specific function directly.
pub fn decide_render(kind: UpdaterKind, inputs: &RenderInputs) -> RenderVisibility {
    match kind {
        UpdaterKind::NpmJs => decide_render_npm(inputs),
        UpdaterKind::Native => decide_render_native(inputs),
        UpdaterKind::PackageManager => decide_render_package_manager(inputs),
    }
}

/// Which concrete updater is being rendered. Distinct from
/// [`crate::updater_path::UpdaterChoice`] because the dispatcher
/// result also carries a `None` variant that means "don't mount
/// anything" — by the time you're calling `decide_render` you've
/// already committed to mounting one of the three surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpdaterKind {
    NpmJs,
    Native,
    PackageManager,
}

/// The npm visibility predicate collapsed into a single boolean:
///
/// ```text
/// has_update_result || (is_updating && has_both_versions)
/// ```
///
/// Equivalently, the hide condition is
/// `!has_update_result && (!has_both_versions || !is_updating)`:
///
/// ```text
/// if (!H && !V) hide;   // hide when !H && !V
/// if (!H && !U) hide;   // hide when !H && !U
/// otherwise render
/// ```
///
/// Let H = `has_update_result`, U = `is_updating`, V =
/// `has_both_versions`. Negating the hide condition gives
/// `H || (V && U)` — the expression above, modulo re-ordering.
pub fn decide_render_npm(inputs: &RenderInputs) -> RenderVisibility {
    if inputs.has_update_result || (inputs.is_updating && inputs.has_both_versions) {
        RenderVisibility::Render
    } else {
        RenderVisibility::Hide
    }
}

/// The native visibility predicate. Adds the max-version warning as a
/// third visibility trigger.
pub fn decide_render_native(inputs: &RenderInputs) -> RenderVisibility {
    if inputs.has_max_version_issue
        || inputs.has_update_result
        || (inputs.is_updating && inputs.has_both_versions)
    {
        RenderVisibility::Render
    } else {
        RenderVisibility::Hide
    }
}

/// The package-manager visibility predicate. Purely gated on
/// `update_available`.
pub fn decide_render_package_manager(inputs: &RenderInputs) -> RenderVisibility {
    if inputs.update_available {
        RenderVisibility::Render
    } else {
        RenderVisibility::Hide
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> RenderInputs {
        RenderInputs {
            has_update_result: false,
            is_updating: false,
            has_both_versions: false,
            has_max_version_issue: false,
            update_available: false,
        }
    }

    // -----------------------------------------------------------------
    // decide_render_npm
    // -----------------------------------------------------------------

    #[test]
    fn npm_hides_when_all_inputs_false() {
        assert_eq!(decide_render_npm(&base()), RenderVisibility::Hide);
    }

    #[test]
    fn npm_renders_when_has_update_result() {
        let i = RenderInputs {
            has_update_result: true,
            ..base()
        };
        assert_eq!(decide_render_npm(&i), RenderVisibility::Render);
    }

    #[test]
    fn npm_renders_when_updating_with_both_versions() {
        let i = RenderInputs {
            is_updating: true,
            has_both_versions: true,
            ..base()
        };
        assert_eq!(decide_render_npm(&i), RenderVisibility::Render);
    }

    #[test]
    fn npm_hides_when_updating_without_versions() {
        // Updating without both versions, and no result, hides the
        // npm surface.
        let i = RenderInputs {
            is_updating: true,
            has_both_versions: false,
            ..base()
        };
        assert_eq!(decide_render_npm(&i), RenderVisibility::Hide);
    }

    #[test]
    fn npm_hides_when_versions_present_but_not_updating_and_no_result() {
        let i = RenderInputs {
            is_updating: false,
            has_both_versions: true,
            has_update_result: false,
            ..base()
        };
        assert_eq!(decide_render_npm(&i), RenderVisibility::Hide);
    }

    #[test]
    fn npm_result_wins_over_missing_versions() {
        // Having a result is enough even without versions.
        let i = RenderInputs {
            has_update_result: true,
            has_both_versions: false,
            is_updating: false,
            ..base()
        };
        assert_eq!(decide_render_npm(&i), RenderVisibility::Render);
    }

    // -----------------------------------------------------------------
    // decide_render_native
    // -----------------------------------------------------------------

    #[test]
    fn native_hides_when_all_inputs_false() {
        assert_eq!(decide_render_native(&base()), RenderVisibility::Hide);
    }

    #[test]
    fn native_renders_when_max_version_issue() {
        let i = RenderInputs {
            has_max_version_issue: true,
            ..base()
        };
        assert_eq!(decide_render_native(&i), RenderVisibility::Render);
    }

    #[test]
    fn native_renders_when_has_update_result() {
        let i = RenderInputs {
            has_update_result: true,
            ..base()
        };
        assert_eq!(decide_render_native(&i), RenderVisibility::Render);
    }

    #[test]
    fn native_renders_when_updating_with_versions() {
        let i = RenderInputs {
            is_updating: true,
            has_both_versions: true,
            ..base()
        };
        assert_eq!(decide_render_native(&i), RenderVisibility::Render);
    }

    #[test]
    fn native_hides_when_updating_without_versions_and_no_warning() {
        let i = RenderInputs {
            is_updating: true,
            has_both_versions: false,
            ..base()
        };
        assert_eq!(decide_render_native(&i), RenderVisibility::Hide);
    }

    // -----------------------------------------------------------------
    // decide_render_package_manager
    // -----------------------------------------------------------------

    #[test]
    fn package_manager_hides_when_no_update_available() {
        assert_eq!(
            decide_render_package_manager(&base()),
            RenderVisibility::Hide
        );
    }

    #[test]
    fn package_manager_renders_when_update_available() {
        let i = RenderInputs {
            update_available: true,
            ..base()
        };
        assert_eq!(decide_render_package_manager(&i), RenderVisibility::Render);
    }

    #[test]
    fn package_manager_ignores_other_inputs() {
        // Even with all other flags set, the package-manager
        // predicate hides unless update_available is true.
        let i = RenderInputs {
            has_update_result: true,
            is_updating: true,
            has_both_versions: true,
            has_max_version_issue: true,
            update_available: false,
        };
        assert_eq!(decide_render_package_manager(&i), RenderVisibility::Hide);
    }

    // -----------------------------------------------------------------
    // decide_render dispatcher
    // -----------------------------------------------------------------

    #[test]
    fn dispatcher_routes_to_per_component_predicates() {
        let i = RenderInputs {
            has_max_version_issue: true,
            ..base()
        };
        // Max-version issue only triggers the native predicate.
        assert_eq!(
            decide_render(UpdaterKind::Native, &i),
            RenderVisibility::Render
        );
        assert_eq!(
            decide_render(UpdaterKind::NpmJs, &i),
            RenderVisibility::Hide
        );
        assert_eq!(
            decide_render(UpdaterKind::PackageManager, &i),
            RenderVisibility::Hide
        );
    }

    #[test]
    fn is_visible_convenience_method() {
        assert!(RenderVisibility::Render.is_visible());
        assert!(!RenderVisibility::Hide.is_visible());
    }

    /// Exhaustive table covering the 2×2×2 truth table of the npm
    /// predicate inputs (result, updating, versions) plus the
    /// max-version toggle for native.
    #[test]
    fn render_visibility_truth_table() {
        // (has_result, is_updating, has_versions) → npm visibility
        let npm_cases: &[(bool, bool, bool, RenderVisibility)] = &[
            (false, false, false, RenderVisibility::Hide),
            (false, false, true, RenderVisibility::Hide),
            (false, true, false, RenderVisibility::Hide),
            (false, true, true, RenderVisibility::Render),
            (true, false, false, RenderVisibility::Render),
            (true, false, true, RenderVisibility::Render),
            (true, true, false, RenderVisibility::Render),
            (true, true, true, RenderVisibility::Render),
        ];
        for (has_result, is_updating, has_versions, expected) in npm_cases {
            let i = RenderInputs {
                has_update_result: *has_result,
                is_updating: *is_updating,
                has_both_versions: *has_versions,
                ..base()
            };
            assert_eq!(
                decide_render_npm(&i),
                *expected,
                "npm table failed for ({has_result}, {is_updating}, {has_versions})",
            );
        }

        // Native adds has_max_version_issue as an OR trigger.
        for (has_result, is_updating, has_versions, npm_expected) in npm_cases {
            // Without max-version issue, native == npm.
            let i_no_warning = RenderInputs {
                has_update_result: *has_result,
                is_updating: *is_updating,
                has_both_versions: *has_versions,
                has_max_version_issue: false,
                ..base()
            };
            assert_eq!(decide_render_native(&i_no_warning), *npm_expected);

            // With max-version issue, native is always Render.
            let i_warning = RenderInputs {
                has_max_version_issue: true,
                ..i_no_warning
            };
            assert_eq!(decide_render_native(&i_warning), RenderVisibility::Render);
        }
    }
}
