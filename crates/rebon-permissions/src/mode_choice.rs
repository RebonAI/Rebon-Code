//! Permission-mode enums and pure helpers.
//!
//! Provides:
//! * The runtime mode sets — [`PERMISSION_MODES`] and the
//!   user-addressable [`EXTERNAL_PERMISSION_MODES`] — the
//!   [`ExternalPermissionMode`] enum, and a re-export of
//!   [`PermissionMode`].
//! * The mode vocabulary: [`permission_mode_title`],
//!   [`permission_mode_short_title`], [`permission_mode_symbol`],
//!   [`is_default_mode`], [`is_external_permission_mode`],
//!   [`to_external_permission_mode`], and the wire-string table behind
//!   [`PermissionMode::from_wire`].
//! * The default-permission-mode picker builder,
//!   [`default_mode_picker_options`].
//!
//! ## Security pin
//!
//! `bypassPermissions` is a member of `EXTERNAL_PERMISSION_MODES` but
//! is **excluded from the default-mode picker**. The Rust
//! implementation preserves this exclusion in
//! [`default_mode_picker_options`] and pins it with a dedicated
//! test.
//!
//! ## Canonical type source
//!
//! [`PermissionMode`] is defined in [`crate::types`], one module over.
//! This module owns the vocabulary built on top of it: the labels, the
//! symbol, the wire alias, the UI-only [`ExternalPermissionMode`]
//! subset, and the default-mode picker.
//!
//! The wire strings are the contract: [`PermissionMode::from_wire`] in
//! [`crate::types`] lowers anything it does not recognise to `Default`, so a
//! variant added to the enum and to `as_wire` but not to the parser stays
//! silently unaddressable from settings.

use core::fmt;

/// The canonical `PermissionMode`, re-exported so this module reads as
/// one vocabulary.
///
/// All seven variants (`AcceptEdits`, `BypassPermissions`, `Default`,
/// `DontAsk`, `Plan`, `Auto`, `Bubble`) and the `as_wire()` /
/// `Display` impls come from [`crate::types`]; this module adds the
/// helper functions on top.
pub use crate::types::PermissionMode;

/// External permission modes — the user-addressable subset.
///
/// Order matches `EXTERNAL_PERMISSION_MODES`.
pub const EXTERNAL_PERMISSION_MODES: &[ExternalPermissionMode] = &[
    ExternalPermissionMode::AcceptEdits,
    ExternalPermissionMode::BypassPermissions,
    ExternalPermissionMode::Default,
    ExternalPermissionMode::DontAsk,
    ExternalPermissionMode::Plan,
];

/// The runtime permission-mode set: [`EXTERNAL_PERMISSION_MODES`] plus
/// `auto`, in canonical order.
///
/// `bubble` is a *type-level* member of [`PermissionMode`] but is NOT
/// in the runtime set ([`is_external_permission_mode`] excludes it
/// alongside `auto` on an internal build, and the runtime set never
/// includes it). It is omitted here.
pub const PERMISSION_MODES: &[PermissionMode] = &[
    PermissionMode::AcceptEdits,
    PermissionMode::BypassPermissions,
    PermissionMode::Default,
    PermissionMode::DontAsk,
    PermissionMode::Plan,
    PermissionMode::Auto,
];

/// External permission modes — never include `auto` or `bubble`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExternalPermissionMode {
    AcceptEdits,
    BypassPermissions,
    Default,
    DontAsk,
    Plan,
}

impl ExternalPermissionMode {
    pub fn as_wire(&self) -> &'static str {
        match self {
            ExternalPermissionMode::AcceptEdits => "acceptEdits",
            ExternalPermissionMode::BypassPermissions => "bypassPermissions",
            ExternalPermissionMode::Default => "default",
            ExternalPermissionMode::DontAsk => "dontAsk",
            ExternalPermissionMode::Plan => "plan",
        }
    }
}

impl fmt::Display for ExternalPermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

impl From<ExternalPermissionMode> for PermissionMode {
    fn from(m: ExternalPermissionMode) -> Self {
        match m {
            ExternalPermissionMode::AcceptEdits => PermissionMode::AcceptEdits,
            ExternalPermissionMode::BypassPermissions => PermissionMode::BypassPermissions,
            ExternalPermissionMode::Default => PermissionMode::Default,
            ExternalPermissionMode::DontAsk => PermissionMode::DontAsk,
            ExternalPermissionMode::Plan => PermissionMode::Plan,
        }
    }
}

/// Whether `mode` is one the user can address directly.
///
/// `is_internal_build` selects the gate: when it is `false` the
/// predicate is always `true`; when it is `true` it returns
/// `mode != auto && mode != bubble`. Taking the gate as a parameter
/// keeps the logic process-agnostic.
pub fn is_external_permission_mode(mode: PermissionMode, is_internal_build: bool) -> bool {
    if !is_internal_build {
        return true;
    }
    !matches!(mode, PermissionMode::Auto | PermissionMode::Bubble)
}

/// Map a [`PermissionMode`] onto the user-addressable
/// [`ExternalPermissionMode`].
///
/// `auto` and `bubble` collapse to `default`; every other mode maps to
/// itself.
pub fn to_external_permission_mode(mode: PermissionMode) -> ExternalPermissionMode {
    match mode {
        PermissionMode::AcceptEdits => ExternalPermissionMode::AcceptEdits,
        PermissionMode::BypassPermissions => ExternalPermissionMode::BypassPermissions,
        PermissionMode::Default => ExternalPermissionMode::Default,
        PermissionMode::DontAsk => ExternalPermissionMode::DontAsk,
        PermissionMode::Plan => ExternalPermissionMode::Plan,
        PermissionMode::Auto => ExternalPermissionMode::Default,
        PermissionMode::Bubble => ExternalPermissionMode::Default,
    }
}

/// Display title for `mode`.
pub fn permission_mode_title(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "Default",
        PermissionMode::Plan => "Plan Mode",
        PermissionMode::AcceptEdits => "Accept edits",
        PermissionMode::BypassPermissions => "Bypass Permissions",
        PermissionMode::DontAsk => "Don't Ask",
        PermissionMode::Auto => "Auto mode",
        // No separate title for bubble → fall back to default's title.
        PermissionMode::Bubble => "Default",
    }
}

/// Short display title for `mode`.
pub fn permission_mode_short_title(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "Default",
        PermissionMode::Plan => "Plan",
        PermissionMode::AcceptEdits => "Accept",
        PermissionMode::BypassPermissions => "Bypass",
        PermissionMode::DontAsk => "DontAsk",
        PermissionMode::Auto => "Auto",
        PermissionMode::Bubble => "Default",
    }
}

/// Prompt symbol for `mode`: an empty string for `default`, a double
/// vertical line for `plan`, and `⏵⏵` for the
/// auto-accepting modes.
pub fn permission_mode_symbol(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "",
        PermissionMode::Plan => "\u{2016}",
        PermissionMode::AcceptEdits => "\u{23F5}\u{23F5}",
        PermissionMode::BypassPermissions => "\u{23F5}\u{23F5}",
        PermissionMode::DontAsk => "\u{23F5}\u{23F5}",
        PermissionMode::Auto => "\u{23F5}\u{23F5}",
        PermissionMode::Bubble => "",
    }
}

/// Whether `mode` is the default mode, or is unset.
pub fn is_default_mode(mode: Option<PermissionMode>) -> bool {
    matches!(mode, Some(PermissionMode::Default) | None)
}

/// Build the option list for the default-permission-mode enum picker.
///
/// ```text
/// 1. Start with the priority order: `default`, then `plan`.
/// 2. Append the rest of the source list in order — `PERMISSION_MODES`
///    when `internal_modes_enabled` is true, otherwise
///    `EXTERNAL_PERMISSION_MODES` cast to `PermissionMode`.
/// 3. Skip any mode already in the priority order or in the excluded
///    set. `bypassPermissions` is always excluded; `auto` is excluded
///    when `internal_modes_enabled` is true and
///    `show_auto_in_default_mode_picker` is false.
/// ```
///
/// SECURITY: `bypassPermissions` is unconditionally excluded.
pub fn default_mode_picker_options(
    internal_modes_enabled: bool,
    show_auto_in_default_mode_picker: bool,
) -> Vec<PermissionMode> {
    let priority_order: &[PermissionMode] = &[PermissionMode::Default, PermissionMode::Plan];
    let mut excluded: Vec<PermissionMode> = vec![PermissionMode::BypassPermissions];
    if internal_modes_enabled && !show_auto_in_default_mode_picker {
        excluded.push(PermissionMode::Auto);
    }
    // Source list: PERMISSION_MODES vs EXTERNAL_PERMISSION_MODES (cast).
    let all_modes_iter: Box<dyn Iterator<Item = PermissionMode>> = if internal_modes_enabled {
        Box::new(PERMISSION_MODES.iter().copied())
    } else {
        Box::new(
            EXTERNAL_PERMISSION_MODES
                .iter()
                .copied()
                .map(PermissionMode::from),
        )
    };

    let mut out: Vec<PermissionMode> = priority_order.to_vec();
    for m in all_modes_iter {
        if priority_order.contains(&m) {
            continue;
        }
        if excluded.contains(&m) {
            continue;
        }
        out.push(m);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_wire_table() {
        let cases: &[(&str, PermissionMode)] = &[
            ("default", PermissionMode::Default),
            ("plan", PermissionMode::Plan),
            ("acceptEdits", PermissionMode::AcceptEdits),
            ("bypassPermissions", PermissionMode::BypassPermissions),
            ("dontAsk", PermissionMode::DontAsk),
            ("auto", PermissionMode::Auto),
            // unknown → default
            ("", PermissionMode::Default),
            ("BUBBLE", PermissionMode::Default),
            ("bubble", PermissionMode::Default),
            ("Default", PermissionMode::Default),
            ("totally bogus", PermissionMode::Default),
        ];
        for (input, expected) in cases {
            assert_eq!(
                PermissionMode::from_wire(input),
                *expected,
                "input={input:?}"
            );
        }
    }

    #[test]
    fn title_table() {
        assert_eq!(permission_mode_title(PermissionMode::Default), "Default");
        assert_eq!(permission_mode_title(PermissionMode::Plan), "Plan Mode");
        assert_eq!(
            permission_mode_title(PermissionMode::AcceptEdits),
            "Accept edits"
        );
        assert_eq!(
            permission_mode_title(PermissionMode::BypassPermissions),
            "Bypass Permissions"
        );
        assert_eq!(permission_mode_title(PermissionMode::DontAsk), "Don't Ask");
        assert_eq!(permission_mode_title(PermissionMode::Auto), "Auto mode");
        // bubble has no separate title → falls back to default
        assert_eq!(permission_mode_title(PermissionMode::Bubble), "Default");
    }

    #[test]
    fn short_title_table() {
        assert_eq!(
            permission_mode_short_title(PermissionMode::Default),
            "Default"
        );
        assert_eq!(permission_mode_short_title(PermissionMode::Plan), "Plan");
        assert_eq!(
            permission_mode_short_title(PermissionMode::AcceptEdits),
            "Accept"
        );
        assert_eq!(
            permission_mode_short_title(PermissionMode::BypassPermissions),
            "Bypass"
        );
        assert_eq!(
            permission_mode_short_title(PermissionMode::DontAsk),
            "DontAsk"
        );
        assert_eq!(permission_mode_short_title(PermissionMode::Auto), "Auto");
    }

    #[test]
    fn permission_mode_symbol_default_is_empty() {
        assert_eq!(permission_mode_symbol(PermissionMode::Default), "");
    }

    #[test]
    fn permission_mode_symbol_plan_is_double_vertical_line() {
        assert_eq!(permission_mode_symbol(PermissionMode::Plan), "\u{2016}");
    }

    #[test]
    fn permission_mode_symbol_accept_modes_are_double_play() {
        for m in [
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
            PermissionMode::Auto,
        ] {
            assert_eq!(permission_mode_symbol(m), "\u{23F5}\u{23F5}", "mode={m:?}");
        }
    }

    #[test]
    fn is_default_mode_some_default_is_true() {
        assert!(is_default_mode(Some(PermissionMode::Default)));
    }

    #[test]
    fn is_default_mode_none_is_true() {
        assert!(is_default_mode(None));
    }

    #[test]
    fn is_default_mode_other_modes_are_false() {
        for m in [
            PermissionMode::Plan,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
            PermissionMode::Auto,
        ] {
            assert!(!is_default_mode(Some(m)), "mode={m:?}");
        }
    }

    #[test]
    fn is_external_permission_mode_non_internal_always_true() {
        for m in PERMISSION_MODES.iter().copied() {
            assert!(
                is_external_permission_mode(m, false),
                "non-internal mode={m:?} should be true"
            );
        }
        // Including bubble which is type-level only
        assert!(is_external_permission_mode(PermissionMode::Bubble, false));
    }

    #[test]
    fn is_external_permission_mode_internal_excludes_auto_and_bubble() {
        assert!(!is_external_permission_mode(PermissionMode::Auto, true));
        assert!(!is_external_permission_mode(PermissionMode::Bubble, true));
        for m in [
            PermissionMode::Default,
            PermissionMode::Plan,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
        ] {
            assert!(is_external_permission_mode(m, true), "mode={m:?}");
        }
    }

    #[test]
    fn to_external_permission_mode_collapses_auto_and_bubble_to_default() {
        assert_eq!(
            to_external_permission_mode(PermissionMode::Auto),
            ExternalPermissionMode::Default
        );
        assert_eq!(
            to_external_permission_mode(PermissionMode::Bubble),
            ExternalPermissionMode::Default
        );
    }

    #[test]
    fn to_external_permission_mode_passthrough_others() {
        assert_eq!(
            to_external_permission_mode(PermissionMode::AcceptEdits),
            ExternalPermissionMode::AcceptEdits
        );
        assert_eq!(
            to_external_permission_mode(PermissionMode::BypassPermissions),
            ExternalPermissionMode::BypassPermissions
        );
        assert_eq!(
            to_external_permission_mode(PermissionMode::Default),
            ExternalPermissionMode::Default
        );
        assert_eq!(
            to_external_permission_mode(PermissionMode::DontAsk),
            ExternalPermissionMode::DontAsk
        );
        assert_eq!(
            to_external_permission_mode(PermissionMode::Plan),
            ExternalPermissionMode::Plan
        );
    }

    #[test]
    fn external_permission_mode_wire_strings() {
        assert_eq!(ExternalPermissionMode::AcceptEdits.as_wire(), "acceptEdits");
        assert_eq!(
            ExternalPermissionMode::BypassPermissions.as_wire(),
            "bypassPermissions"
        );
        assert_eq!(ExternalPermissionMode::Default.as_wire(), "default");
        assert_eq!(ExternalPermissionMode::DontAsk.as_wire(), "dontAsk");
        assert_eq!(ExternalPermissionMode::Plan.as_wire(), "plan");
    }

    #[test]
    fn permission_mode_wire_strings() {
        assert_eq!(PermissionMode::AcceptEdits.as_wire(), "acceptEdits");
        assert_eq!(
            PermissionMode::BypassPermissions.as_wire(),
            "bypassPermissions"
        );
        assert_eq!(PermissionMode::Default.as_wire(), "default");
        assert_eq!(PermissionMode::DontAsk.as_wire(), "dontAsk");
        assert_eq!(PermissionMode::Plan.as_wire(), "plan");
        assert_eq!(PermissionMode::Auto.as_wire(), "auto");
        assert_eq!(PermissionMode::Bubble.as_wire(), "bubble");
    }

    // ---- default_mode_picker_options ----

    #[test]
    fn default_mode_picker_options_external_only() {
        // internal_modes_enabled = false → use EXTERNAL_PERMISSION_MODES.
        // showAuto is irrelevant here.
        let opts = default_mode_picker_options(false, false);
        assert_eq!(
            opts,
            vec![
                PermissionMode::Default,
                PermissionMode::Plan,
                PermissionMode::AcceptEdits,
                PermissionMode::DontAsk,
            ],
        );
    }

    #[test]
    fn default_mode_picker_options_internal_no_auto() {
        // internal_modes_enabled && !show_auto → exclude bypassPermissions AND auto.
        let opts = default_mode_picker_options(true, false);
        assert_eq!(
            opts,
            vec![
                PermissionMode::Default,
                PermissionMode::Plan,
                PermissionMode::AcceptEdits,
                PermissionMode::DontAsk,
            ],
        );
    }

    #[test]
    fn default_mode_picker_options_internal_with_auto() {
        // internal_modes_enabled && show_auto → include auto.
        let opts = default_mode_picker_options(true, true);
        assert_eq!(
            opts,
            vec![
                PermissionMode::Default,
                PermissionMode::Plan,
                PermissionMode::AcceptEdits,
                PermissionMode::DontAsk,
                PermissionMode::Auto,
            ],
        );
    }

    /// SECURITY-CRITICAL: bypassPermissions must NEVER appear in the
    /// default-mode picker options. This protects users from
    /// silently selecting "no prompts" from settings UI.
    #[test]
    fn default_mode_picker_excludes_bypass_in_all_combinations() {
        for &transcript in &[false, true] {
            for &show_auto in &[false, true] {
                let opts = default_mode_picker_options(transcript, show_auto);
                assert!(
                    !opts.contains(&PermissionMode::BypassPermissions),
                    "bypassPermissions leaked into picker (transcript={transcript}, show_auto={show_auto})",
                );
            }
        }
    }

    #[test]
    fn default_mode_picker_priority_order_first() {
        // Default and Plan must come first in every variant.
        for &transcript in &[false, true] {
            for &show_auto in &[false, true] {
                let opts = default_mode_picker_options(transcript, show_auto);
                assert_eq!(opts[0], PermissionMode::Default);
                assert_eq!(opts[1], PermissionMode::Plan);
            }
        }
    }

    #[test]
    fn internal_permission_modes_count_is_six() {
        // PERMISSION_MODES = EXTERNAL_PERMISSION_MODES ∪ {auto}: 5 + 1 = 6.
        assert_eq!(PERMISSION_MODES.len(), 6);
    }

    #[test]
    fn external_permission_modes_count_is_five() {
        assert_eq!(EXTERNAL_PERMISSION_MODES.len(), 5);
    }
}
