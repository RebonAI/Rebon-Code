//! Reducer for the sandbox overrides tab. The override mode is a binary
//! switch (`open` / `closed`) and selecting a value produces both a
//! settings write payload and a user-facing result message.
//!
//! ## Behaviour notes
//!
//! [`build_overrides_view`] picks one of three shapes from
//! [`OverridesInputs`]:
//!
//! ```text
//! !sandboxing_enabled  -> OverridesView::NotEnabled
//! locked_by_policy     -> OverridesView::Locked { current }
//! otherwise            -> OverridesView::Interactive { current, options }
//! ```
//!
//! `current` is derived from the `current_allow_unsandboxed` flag, and
//! `options` is the two-row list from [`build_options`] whose matching
//! row gets the `(current)` suffix appended to its base label.
//!
//! [`handle_overrides_input`] then maps a selection to
//! [`OverridesEffect::WritePolicy`] — with `allow_unsandboxed_commands`
//! true exactly when the selected mode is `open` — and a cancel to
//! [`OverridesEffect::CancelSkip`].
//!
//! ## Pinned rules
//!
//! 1. **`current_mode` is binary.** `current_allow_unsandboxed` true
//! selects `open`, false selects `closed`. No third state. Pinned by
//! [`OverrideMode::current_from_flag`].
//! 2. **The write payload is the allow-unsandboxed-commands flag,
//! true exactly when the mode is `open`.** Mapped through
//! [`OverridesEffect::WritePolicy`].
//! 3. **The success message for `open` is the literal `"✓
//! Unsandboxed fallback allowed - commands can run outside sandbox
//! when necessary"`.** Pinned by [`MESSAGE_OPEN`].
//! 4. **The success message for `closed` is the literal `"✓ Strict
//! sandbox mode - all commands must run in sandbox or be excluded
//! via the \`excludedCommands\` option"`.** Pinned by
//! [`MESSAGE_CLOSED`].
//! 5. **The current indicator is `"(current)"`.** Only the LABEL of
//! the matching option includes the suffix; the other option's label
//! is unchanged. Pinned by [`build_options`].
//! 6. **`locked_by_policy` short-circuits to a "managed" message** with
//! the literal `"Override settings are managed by a higher-priority
//! configuration and cannot be changed locally."` Pinned by
//! [`OverridesView::Locked`].
//! 7. **`!sandboxing_enabled` short-circuits to a "not enabled"
//! message** with the literal `"Sandbox is not enabled. Enable
//! sandbox to configure override settings."`
//! 8. **Cancel maps to [`OverridesEffect::CancelSkip`].**

/// Wire string for the "open" override mode (allow unsandboxed
/// fallback). Pinned literally — the string is observable to callers
/// and by the option list.
pub const MODE_WIRE_OPEN: &str = "open";

/// Wire string for the "closed" override mode (strict). Pinned
/// literally.
pub const MODE_WIRE_CLOSED: &str = "closed";

/// `"(current)"` — the current-mode marker appended to the matching
/// option's label. Kept as a plain string; the consumer decides how to
/// colour it.
pub const CURRENT_INDICATOR_PLAIN: &str = "(current)";

/// Label for the "allow unsandboxed fallback" option (no current
/// suffix).
pub const LABEL_OPEN_BASE: &str = "Allow unsandboxed fallback";

/// Label for the "strict sandbox mode" option (no current suffix).
pub const LABEL_CLOSED_BASE: &str = "Strict sandbox mode";

/// Result message when the user selects "open".
pub const MESSAGE_OPEN: &str =
    "\u{2713} Unsandboxed fallback allowed - commands can run outside sandbox when necessary";

/// Result message when the user selects "closed".
pub const MESSAGE_CLOSED: &str = "\u{2713} Strict sandbox mode - all commands must run in sandbox or be excluded via the `excludedCommands` option";

/// Short-circuit message when the sandbox is not enabled.
pub const MESSAGE_NOT_ENABLED: &str =
    "Sandbox is not enabled. Enable sandbox to configure override settings.";

/// Short-circuit message when override settings are policy-locked.
pub const MESSAGE_LOCKED: &str =
    "Override settings are managed by a higher-priority configuration and cannot be changed locally.";

/// Tab title — `"Configure Overrides:"`.
pub const TAB_HEADER: &str = "Configure Overrides:";

/// Documentation link shown for the overrides tab.
pub const OVERRIDES_DOCS_URL: &str = "https://reboncode.ai";

/// Override mode — binary `open` (allow unsandboxed fallback) or
/// `closed` (strict).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideMode {
    /// `'open'` — allow unsandboxed fallback when a command fails
    /// inside the sandbox. SECURITY-RELEVANT: this is the LESS
    /// restrictive setting.
    Open,
    /// `'closed'` — strict sandbox mode. All bash commands must run
    /// inside the sandbox unless explicitly excluded.
    Closed,
}

impl OverrideMode {
    /// Wire string. Stable across releases.
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Open => MODE_WIRE_OPEN,
            Self::Closed => MODE_WIRE_CLOSED,
        }
    }

    /// Parse the wire string. `None` for unknown values.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            MODE_WIRE_OPEN => Some(Self::Open),
            MODE_WIRE_CLOSED => Some(Self::Closed),
            _ => None,
        }
    }

    /// Derive the mode from the allow-unsandboxed-commands flag: true
    /// is `open`, false is `closed`.
    pub fn current_from_flag(allow_unsandboxed_commands: bool) -> Self {
        if allow_unsandboxed_commands {
            Self::Open
        } else {
            Self::Closed
        }
    }

    /// Result message for the "user selected this" code path. Pinned
    /// to [`MESSAGE_OPEN`] / [`MESSAGE_CLOSED`].
    pub fn result_message(&self) -> &'static str {
        match self {
            Self::Open => MESSAGE_OPEN,
            Self::Closed => MESSAGE_CLOSED,
        }
    }

    /// Base label (without the current indicator suffix). Pinned to
    /// [`LABEL_OPEN_BASE`] / [`LABEL_CLOSED_BASE`].
    pub fn base_label(&self) -> &'static str {
        match self {
            Self::Open => LABEL_OPEN_BASE,
            Self::Closed => LABEL_CLOSED_BASE,
        }
    }
}

/// Single option row offered by the tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideOption {
    pub label: String,
    pub value: &'static str,
    /// `true` when this option is the currently-active mode.
    pub is_current: bool,
}

/// Build the two options for the tab. The matching option
/// gets the `(current)` suffix appended to its label.
pub fn build_options(current: OverrideMode) -> Vec<OverrideOption> {
    let mut options = Vec::with_capacity(2);
    for mode in [OverrideMode::Open, OverrideMode::Closed] {
        let is_current = mode == current;
        let label = if is_current {
            format!("{} {}", mode.base_label(), CURRENT_INDICATOR_PLAIN)
        } else {
            mode.base_label().to_string()
        };
        options.push(OverrideOption {
            label,
            value: mode.as_wire(),
            is_current,
        });
    }
    options
}

/// Render decision for the tab — one of the three branches below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverridesView {
    /// Sandboxing is switched off — show the "Sandbox is not enabled"
    /// message.
    NotEnabled,
    /// Settings are locked by a higher-priority policy — show the
    /// "managed by higher-priority configuration" message plus the
    /// current setting.
    Locked { current: OverrideMode },
    /// Normal interactive state — show the two options.
    Interactive {
        current: OverrideMode,
        options: Vec<OverrideOption>,
    },
}

/// Inputs to the overrides view-model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverridesInputs {
    /// Whether sandboxing is switched on.
    pub sandboxing_enabled: bool,
    /// Whether a higher-priority policy locks the sandbox settings.
    pub locked_by_policy: bool,
    /// Whether unsandboxed fallback is currently allowed.
    pub current_allow_unsandboxed: bool,
}

/// Pure render decision. Returns one of the three branches without
/// performing any side effects.
pub fn build_overrides_view(inputs: &OverridesInputs) -> OverridesView {
    let current = OverrideMode::current_from_flag(inputs.current_allow_unsandboxed);
    if !inputs.sandboxing_enabled {
        return OverridesView::NotEnabled;
    }
    if inputs.locked_by_policy {
        return OverridesView::Locked { current };
    }
    OverridesView::Interactive {
        current,
        options: build_options(current),
    }
}

/// User-driven event the reducer accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverridesInput {
    /// User selected one of the two modes.
    Select(OverrideMode),
    /// User cancelled (Esc / Ctrl-C / `confirm:no`).
    Cancel,
}

/// Effect the reducer asks the consumer to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverridesEffect {
    /// Write the new policy and notify the parent of completion with
    /// the user-facing result message.
    WritePolicy {
        /// Value to write for the allow-unsandboxed-commands setting.
        allow_unsandboxed_commands: bool,
        /// User-facing result message to report back to the caller.
        message: &'static str,
    },
    /// User cancelled — report the dialog as skipped.
    CancelSkip,
}

/// Pure reducer for the overrides tab. Maps a [`OverridesInput`]
/// into the effect the consumer should perform.
pub fn handle_overrides_input(input: OverridesInput) -> OverridesEffect {
    match input {
        OverridesInput::Select(mode) => OverridesEffect::WritePolicy {
            allow_unsandboxed_commands: mode == OverrideMode::Open,
            message: mode.result_message(),
        },
        OverridesInput::Cancel => OverridesEffect::CancelSkip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trip_open() {
        assert_eq!(OverrideMode::Open.as_wire(), "open");
        assert_eq!(OverrideMode::from_wire("open"), Some(OverrideMode::Open));
    }

    #[test]
    fn wire_round_trip_closed() {
        assert_eq!(OverrideMode::Closed.as_wire(), "closed");
        assert_eq!(
            OverrideMode::from_wire("closed"),
            Some(OverrideMode::Closed)
        );
    }

    #[test]
    fn wire_unknown_is_none() {
        assert_eq!(OverrideMode::from_wire("Open"), None); // case-sensitive
        assert_eq!(OverrideMode::from_wire(""), None);
        assert_eq!(OverrideMode::from_wire("partial"), None);
    }

    #[test]
    fn current_from_flag_true_is_open() {
        assert_eq!(OverrideMode::current_from_flag(true), OverrideMode::Open);
    }

    #[test]
    fn current_from_flag_false_is_closed() {
        assert_eq!(OverrideMode::current_from_flag(false), OverrideMode::Closed);
    }

    #[test]
    fn result_messages_pinned() {
        assert_eq!(
            OverrideMode::Open.result_message(),
            "\u{2713} Unsandboxed fallback allowed - commands can run outside sandbox when necessary"
        );
        assert_eq!(
            OverrideMode::Closed.result_message(),
            "\u{2713} Strict sandbox mode - all commands must run in sandbox or be excluded via the `excludedCommands` option"
        );
    }

    #[test]
    fn base_labels_pinned() {
        assert_eq!(
            OverrideMode::Open.base_label(),
            "Allow unsandboxed fallback"
        );
        assert_eq!(OverrideMode::Closed.base_label(), "Strict sandbox mode");
    }

    #[test]
    fn options_when_open_is_current() {
        let opts = build_options(OverrideMode::Open);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].value, "open");
        assert!(opts[0].is_current);
        assert_eq!(opts[0].label, "Allow unsandboxed fallback (current)");
        assert_eq!(opts[1].value, "closed");
        assert!(!opts[1].is_current);
        assert_eq!(opts[1].label, "Strict sandbox mode");
    }

    #[test]
    fn options_when_closed_is_current() {
        let opts = build_options(OverrideMode::Closed);
        assert_eq!(opts[0].value, "open");
        assert!(!opts[0].is_current);
        assert_eq!(opts[0].label, "Allow unsandboxed fallback");
        assert_eq!(opts[1].value, "closed");
        assert!(opts[1].is_current);
        assert_eq!(opts[1].label, "Strict sandbox mode (current)");
    }

    #[test]
    fn options_order_is_open_then_closed() {
        // The existing option list is `[open, closed]` regardless of which is
        // current.
        for mode in [OverrideMode::Open, OverrideMode::Closed] {
            let opts = build_options(mode);
            assert_eq!(opts[0].value, "open");
            assert_eq!(opts[1].value, "closed");
        }
    }

    #[test]
    fn view_not_enabled_when_disabled() {
        let inputs = OverridesInputs {
            sandboxing_enabled: false,
            locked_by_policy: false,
            current_allow_unsandboxed: false,
        };
        assert_eq!(build_overrides_view(&inputs), OverridesView::NotEnabled);
    }

    #[test]
    fn view_not_enabled_takes_priority_over_locked() {
        // The not-enabled branch is checked first.
        let inputs = OverridesInputs {
            sandboxing_enabled: false,
            locked_by_policy: true,
            current_allow_unsandboxed: false,
        };
        assert_eq!(build_overrides_view(&inputs), OverridesView::NotEnabled);
    }

    #[test]
    fn view_locked_when_enabled_and_locked() {
        let inputs = OverridesInputs {
            sandboxing_enabled: true,
            locked_by_policy: true,
            current_allow_unsandboxed: true,
        };
        assert_eq!(
            build_overrides_view(&inputs),
            OverridesView::Locked {
                current: OverrideMode::Open
            }
        );
    }

    #[test]
    fn view_locked_carries_current_mode_when_strict() {
        let inputs = OverridesInputs {
            sandboxing_enabled: true,
            locked_by_policy: true,
            current_allow_unsandboxed: false,
        };
        assert_eq!(
            build_overrides_view(&inputs),
            OverridesView::Locked {
                current: OverrideMode::Closed
            }
        );
    }

    #[test]
    fn view_interactive_when_enabled_and_unlocked() {
        let inputs = OverridesInputs {
            sandboxing_enabled: true,
            locked_by_policy: false,
            current_allow_unsandboxed: false,
        };
        match build_overrides_view(&inputs) {
            OverridesView::Interactive { current, options } => {
                assert_eq!(current, OverrideMode::Closed);
                assert_eq!(options.len(), 2);
                assert!(options[1].is_current);
            }
            other => panic!("expected interactive, got {:?}", other),
        }
    }

    #[test]
    fn reducer_select_open_writes_true() {
        let eff = handle_overrides_input(OverridesInput::Select(OverrideMode::Open));
        assert_eq!(
            eff,
            OverridesEffect::WritePolicy {
                allow_unsandboxed_commands: true,
                message: MESSAGE_OPEN,
            }
        );
    }

    #[test]
    fn reducer_select_closed_writes_false() {
        let eff = handle_overrides_input(OverridesInput::Select(OverrideMode::Closed));
        assert_eq!(
            eff,
            OverridesEffect::WritePolicy {
                allow_unsandboxed_commands: false,
                message: MESSAGE_CLOSED,
            }
        );
    }

    #[test]
    fn reducer_cancel_routes_to_skip() {
        let eff = handle_overrides_input(OverridesInput::Cancel);
        assert_eq!(eff, OverridesEffect::CancelSkip);
    }

    #[test]
    fn pinned_constants() {
        assert_eq!(MODE_WIRE_OPEN, "open");
        assert_eq!(MODE_WIRE_CLOSED, "closed");
        assert_eq!(CURRENT_INDICATOR_PLAIN, "(current)");
        assert_eq!(LABEL_OPEN_BASE, "Allow unsandboxed fallback");
        assert_eq!(LABEL_CLOSED_BASE, "Strict sandbox mode");
        assert_eq!(TAB_HEADER, "Configure Overrides:");
        assert_eq!(
            MESSAGE_NOT_ENABLED,
            "Sandbox is not enabled. Enable sandbox to configure override settings."
        );
        assert_eq!(
            MESSAGE_LOCKED,
            "Override settings are managed by a higher-priority configuration and cannot be changed locally."
        );
        assert_eq!(OVERRIDES_DOCS_URL, "https://reboncode.ai");
    }

    #[test]
    fn overrides_view_decision_table() {
        // (enabled, locked, current_allow, expected_variant)
        #[derive(Debug)]
        enum Variant {
            NotEnabled,
            Locked,
            Interactive,
        }
        let table = [
            (false, false, false, Variant::NotEnabled),
            (false, false, true, Variant::NotEnabled),
            (false, true, false, Variant::NotEnabled),
            (false, true, true, Variant::NotEnabled),
            (true, true, false, Variant::Locked),
            (true, true, true, Variant::Locked),
            (true, false, false, Variant::Interactive),
            (true, false, true, Variant::Interactive),
        ];
        for (en, lo, ca, expected) in table {
            let inputs = OverridesInputs {
                sandboxing_enabled: en,
                locked_by_policy: lo,
                current_allow_unsandboxed: ca,
            };
            let v = build_overrides_view(&inputs);
            match (expected, &v) {
                (Variant::NotEnabled, OverridesView::NotEnabled)
                | (Variant::Locked, OverridesView::Locked { .. })
                | (Variant::Interactive, OverridesView::Interactive { .. }) => {}
                _ => panic!("unexpected variant for {en},{lo},{ca}: {:?}", v),
            }
        }
    }
}
