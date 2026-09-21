//! Pure decision logic for the sandbox doctor section. The platform,
//! settings and filesystem probes live elsewhere (see `crate::doctor`)
//! and hand their results in as pre-built inputs; this module owns the
//! verdict.
//!
//! ## Behaviour
//!
//! [`build_doctor_view`] is a three-step filter followed by a status
//! classification:
//!
//! 1. `supported_platform == false` → `None`.
//! 2. `sandbox_enabled_in_settings == false` → `None`.
//! 3. no errors AND no warnings → `None`.
//!
//! Anything else yields a [`DoctorView`]. Its [`DoctorStatus`] is
//! [`DoctorStatus::Error`] as soon as the dependency check reports at
//! least one error, and [`DoctorStatus::Warning`] otherwise. The
//! check's error and warning strings are carried through verbatim and
//! in input order, and `show_install_hint` is set when there is at
//! least one error.
//!
//! ## Pinned rules
//!
//! 1. **Verdict short-circuits on UNSUPPORTED platform.** Even if
//! sandboxing is enabled in settings, `supported_platform == false`
//! yields `None`. SECURITY-RELEVANT: Windows users should not
//! see "sandbox available" messages.
//! 2. **Verdict short-circuits on `!sandbox_enabled_in_settings`.**
//! Even on a supported platform, if the user has not opted in,
//! the doctor section is hidden.
//! 3. **Verdict short-circuits on no errors AND no warnings.** A
//! clean check renders nothing. The doctor only surfaces problems.
//! 4. **An error outranks a warning for the status color.** The
//! status is `error` if there is at least one error, otherwise
//! `warning`. Pinned by [`status_color`].
//! 5. **An error outranks a warning for the status text.**
//! `"Missing dependencies"` vs `"Available (with warnings)"`.
//! Pinned by [`status_text`].
//! 6. **The install hint is `"└ Run /sandbox for install
//! instructions"`** and is flagged only when there is at least one
//! error. The leading `└` is U+2514 BOX DRAWINGS LIGHT UP AND RIGHT;
//! the renderer trims it before display.

use crate::view::dependency::SandboxDependencyCheck;

/// `"Missing dependencies"` — status text when there is at least one
/// error in the dep check.
pub const STATUS_TEXT_ERROR: &str = "Missing dependencies";

/// `"Available (with warnings)"` — status text when there are no
/// errors but at least one warning.
pub const STATUS_TEXT_WARNING: &str = "Available (with warnings)";

/// `"error"` — design-system color name when status is error.
pub const STATUS_COLOR_ERROR: &str = "error";

/// `"warning"` — design-system color name when status is warning.
pub const STATUS_COLOR_WARNING: &str = "warning";

/// `"└ Run /sandbox for install instructions"` — install hint row.
/// The leading `└` is U+2514 BOX DRAWINGS LIGHT UP AND RIGHT.
pub const RUN_SANDBOX_HINT: &str = "└ Run /sandbox for install instructions";

/// Inputs to the doctor verdict. The consumer fills these in from
/// the real adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorInputs {
    /// Whether the host platform has a sandbox backend.
    pub supported_platform: bool,
    /// Whether sandboxing is switched on in the user's settings.
    pub sandbox_enabled_in_settings: bool,
    /// Dependency check result for this machine.
    pub dep_check: SandboxDependencyCheck,
}

/// Compact status verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorStatus {
    /// At least one dependency error — display as "Missing dependencies"
    /// in the error color.
    Error,
    /// No errors but at least one warning — display as
    /// "Available (with warnings)" in the warning color.
    Warning,
}

impl DoctorStatus {
    /// Design-system color name for this status.
    pub fn color(&self) -> &'static str {
        match self {
            Self::Error => STATUS_COLOR_ERROR,
            Self::Warning => STATUS_COLOR_WARNING,
        }
    }

    /// Status row text for this status.
    pub fn text(&self) -> &'static str {
        match self {
            Self::Error => STATUS_TEXT_ERROR,
            Self::Warning => STATUS_TEXT_WARNING,
        }
    }
}

/// Render output. `None` when the section is suppressed; `Some`
/// carries the structured rows the renderer needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorView {
    /// Verdict status (color + text).
    pub status: DoctorStatus,
    /// Errors from the dep check, in input order.
    pub errors: Vec<String>,
    /// Warnings from the dep check, in input order.
    pub warnings: Vec<String>,
    /// Whether to show the `"Run /sandbox for install instructions"`
    /// row. True iff the dependency check reported at least one error.
    pub show_install_hint: bool,
}

/// Look up the status color shorthand. Pure helper around
/// `DoctorStatus::color`.
pub fn status_color(has_errors: bool) -> &'static str {
    if has_errors {
        STATUS_COLOR_ERROR
    } else {
        STATUS_COLOR_WARNING
    }
}

/// Look up the status text. Pure helper around `DoctorStatus::text`.
pub fn status_text(has_errors: bool) -> &'static str {
    if has_errors {
        STATUS_TEXT_ERROR
    } else {
        STATUS_TEXT_WARNING
    }
}

/// Compute the doctor verdict. Returns `None` for the early-return
/// cases (unsupported platform, sandbox disabled in settings, no
/// errors and no warnings) and `Some(view)` otherwise.
pub fn build_doctor_view(inputs: &DoctorInputs) -> Option<DoctorView> {
    if !inputs.supported_platform {
        return None;
    }
    if !inputs.sandbox_enabled_in_settings {
        return None;
    }
    let has_errors = inputs.dep_check.has_errors();
    let has_warnings = inputs.dep_check.has_warnings();
    if !has_errors && !has_warnings {
        return None;
    }
    let status = if has_errors {
        DoctorStatus::Error
    } else {
        DoctorStatus::Warning
    };
    Some(DoctorView {
        status,
        errors: inputs.dep_check.errors.clone(),
        warnings: inputs.dep_check.warnings.clone(),
        show_install_hint: has_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(supported: bool, enabled: bool, errors: &[&str], warnings: &[&str]) -> DoctorInputs {
        DoctorInputs {
            supported_platform: supported,
            sandbox_enabled_in_settings: enabled,
            dep_check: SandboxDependencyCheck {
                errors: errors.iter().map(|s| s.to_string()).collect(),
                warnings: warnings.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    #[test]
    fn unsupported_platform_returns_none() {
        let inputs = make(false, true, &["bwrap missing"], &[]);
        assert!(build_doctor_view(&inputs).is_none());
    }

    #[test]
    fn unsupported_platform_returns_none_even_with_warnings() {
        let inputs = make(false, true, &[], &["seccomp"]);
        assert!(build_doctor_view(&inputs).is_none());
    }

    #[test]
    fn disabled_in_settings_returns_none() {
        let inputs = make(true, false, &["bwrap missing"], &[]);
        assert!(build_doctor_view(&inputs).is_none());
    }

    #[test]
    fn disabled_in_settings_returns_none_even_on_supported_platform() {
        let inputs = make(true, false, &[], &[]);
        assert!(build_doctor_view(&inputs).is_none());
    }

    #[test]
    fn clean_check_returns_none() {
        let inputs = make(true, true, &[], &[]);
        assert!(build_doctor_view(&inputs).is_none());
    }

    #[test]
    fn errors_only_yields_error_status() {
        let inputs = make(true, true, &["bwrap missing"], &[]);
        let v = build_doctor_view(&inputs).unwrap();
        assert_eq!(v.status, DoctorStatus::Error);
        assert_eq!(v.errors, vec!["bwrap missing".to_string()]);
        assert!(v.warnings.is_empty());
        assert!(v.show_install_hint);
    }

    #[test]
    fn warnings_only_yields_warning_status() {
        let inputs = make(true, true, &[], &["seccomp"]);
        let v = build_doctor_view(&inputs).unwrap();
        assert_eq!(v.status, DoctorStatus::Warning);
        assert!(v.errors.is_empty());
        assert_eq!(v.warnings, vec!["seccomp".to_string()]);
        assert!(!v.show_install_hint);
    }

    #[test]
    fn errors_and_warnings_yields_error_status() {
        // SECURITY-RELEVANT: errors must trump warnings for the status
        // text and color so the user sees "missing dependencies", not
        // "available (with warnings)" — the latter would suggest the
        // sandbox is functional.
        let inputs = make(true, true, &["bwrap missing"], &["seccomp"]);
        let v = build_doctor_view(&inputs).unwrap();
        assert_eq!(v.status, DoctorStatus::Error);
        assert!(v.show_install_hint);
        assert_eq!(v.errors, vec!["bwrap missing".to_string()]);
        assert_eq!(v.warnings, vec!["seccomp".to_string()]);
    }

    #[test]
    fn install_hint_only_when_errors() {
        let only_warn = make(true, true, &[], &["seccomp"]);
        let v = build_doctor_view(&only_warn).unwrap();
        assert!(!v.show_install_hint);

        let only_err = make(true, true, &["bwrap"], &[]);
        let v = build_doctor_view(&only_err).unwrap();
        assert!(v.show_install_hint);

        let both = make(true, true, &["bwrap"], &["seccomp"]);
        let v = build_doctor_view(&both).unwrap();
        assert!(v.show_install_hint);
    }

    #[test]
    fn status_color_helper_matches_status_color() {
        assert_eq!(status_color(true), DoctorStatus::Error.color());
        assert_eq!(status_color(false), DoctorStatus::Warning.color());
    }

    #[test]
    fn status_text_helper_matches_status_text() {
        assert_eq!(status_text(true), DoctorStatus::Error.text());
        assert_eq!(status_text(false), DoctorStatus::Warning.text());
    }

    #[test]
    fn pinned_constants() {
        assert_eq!(STATUS_TEXT_ERROR, "Missing dependencies");
        assert_eq!(STATUS_TEXT_WARNING, "Available (with warnings)");
        assert_eq!(STATUS_COLOR_ERROR, "error");
        assert_eq!(STATUS_COLOR_WARNING, "warning");
        assert_eq!(RUN_SANDBOX_HINT, "└ Run /sandbox for install instructions");
    }

    #[test]
    fn doctor_render_decision_table() {
        // (supported, enabled_in_settings, n_errors, n_warnings,
        // expected_render, expected_status_if_render)
        let table = [
            (false, false, 0, 0, false, None),
            (false, true, 1, 0, false, None),
            (true, false, 1, 0, false, None),
            (true, true, 0, 0, false, None),
            (true, true, 1, 0, true, Some(DoctorStatus::Error)),
            (true, true, 0, 1, true, Some(DoctorStatus::Warning)),
            (true, true, 1, 1, true, Some(DoctorStatus::Error)),
            (true, true, 5, 3, true, Some(DoctorStatus::Error)),
            (true, true, 0, 5, true, Some(DoctorStatus::Warning)),
        ];
        for (sup, en, ne, nw, render, expected_status) in table {
            let errors: Vec<&str> = (0..ne).map(|_| "e").collect();
            let warnings: Vec<&str> = (0..nw).map(|_| "w").collect();
            let inputs = make(sup, en, &errors, &warnings);
            let result = build_doctor_view(&inputs);
            assert_eq!(
                result.is_some(),
                render,
                "supported={sup} enabled={en} ne={ne} nw={nw}"
            );
            if let (Some(v), Some(exp)) = (result, expected_status) {
                assert_eq!(v.status, exp);
            }
        }
    }
}
