//! Pure status behavior helpers — the footer and status-row projections.
//!
//! This module was a standalone crate before the merge. Its only
//! consumers were the terminal half and the `promptinput`
//! module next door, so the crate boundary bought a Cargo entry and
//! nothing else. Nothing here names ratatui, and nothing should start.
//!
//! The one piece that did *not* come here is `effort_indicator`: 55 of
//! this crate's 64 references were to it, and seven of those sit on
//! the headless paths (`exec.rs`, `main.rs`, the background
//! runtime fields, session handover and startup, the CLI overrides).
//! The level is a domain enum, not a status row, so it sank to
//! `rebon-types` instead — a headless path must not reach through the
//! terminal crate for it. It is `rebon_types::ReasoningEffort`, and there is
//! exactly one such enum: a second five-variant copy here would be two
//! answers to what "high" means.
//!
//! This module provides independent status display projections. Each helper reads
//! pre-resolved state, decides whether to show, chooses color/text, and emits a
//! display row or command payload.
//!
//! ## Feature coverage
//!
//! * [`press_enter_to_continue`] — static "Press Enter to continue…" hint.
//! * [`tool_use_loader`] — color + glyph dispatch for the running/error/
//!   unresolved tool dot.
//! * [`dev_bar`] — slow-sync slow-op row with the `op (123ms)` formatter
//!   and the last-3 truncation.
//! * [`keybinding_warnings`] — error/warning split + header color
//!   ("Keybinding Configuration Issues").
//! * [`status_notices`] — the visibility gate: hidden when the
//!   active-notice count is zero.
//! * [`bash_mode_progress`] — `<bash-input>...</bash-input>` wrapper +
//!   progress branch shape.
//! * [`configurable_shortcut_hint`] — fall-through to the keyboard
//!   shortcut hint with action/parens/bold pass-through.
//! * [`ide_status_indicator`] — three branches: line-selection / file-name /
//!   hidden, with the `⧉ N lines selected` text.
//! * [`session_background_hint`] — double-press background trigger, tmux
//!   ctrl+b ctrl+b override, gating + visibility.
//! * [`teleport_progress`] — four-step progress reducer (validating /
//!   fetching_logs / fetching_branch / checking_out) with frame glyph.
//! * [`token_warning`] — the warning/error threshold dispatch, the
//!   auto-compact-vs-no-compact label branch, the reactive-only
//!   `100 - display_percent_left` flip, the collapse-mode label
//!   projection, the upgrade-message suffix.
//! * [`status_line`] — the [`status_line_should_display`] gate, the
//!   [`build_status_line_command_input`] payload assembly, and the
//!   padding resolution.
//!
//! ## What is in this crate
//!
//! Each module exposes plain owned data structures and pure functions.
//! The only outside type any of them names is
//! `rebon_types::ReasoningEffort`, which `status_line` renders a glyph
//! for; severity tags and the rest are owned directly by their
//! respective modules.
//!
//! ## Input seam shapes
//!
//! External state, rendering and helper seams are modelled as follows:
//!
//! 1. **The keyboard shortcut hint** → modelled as a plain
//!    [`shortcut_hint::KeyboardShortcutHint`] data shape.
//!    Consumers wire the actual rendering. We never import the design
//!    system.
//!
//! 2. **UI plumbing** — keybinding and app-state plumbing,
//!    subscriptions, blink and animation timers, and the memory-usage
//!    poller. Erased. Each module exposes a small pure function or
//!    reducer struct the consumer drives.
//!
//! 3. **Data sources and config helpers** — global config, the slow-op
//!    log, cached keybinding warnings, memory files, active notices,
//!    usage and context-window figures, session and cwd identity,
//!    model-name rendering, hook input, feature flags, and the
//!    formatters behind them. Erased. The crate consumes plain owned
//!    input structs and emits plain owned decision shapes.
//!
//! 4. **Rendering primitives** — boxes, text runs, newlines and ANSI
//!    are represented as semantic enums + strings.
//!
//! ## Dependency boundary
//!
//! `KeyboardShortcutHint` is owned here on purpose: this module owns
//! its input contract rather than borrowing the design system's, which is
//! what let it compile standalone before the merge and is still the reason
//! it names nothing but `rebon_types`. The crate-wide rule it used to have
//! its own canary for is now
//! `crate::compatibility::only_rebon_cli_may_depend_on_this_crate`.

#![deny(missing_docs)]

pub mod bash_mode_progress;
pub mod configurable_shortcut_hint;
pub mod dev_bar;
pub mod ide_status_indicator;
pub mod keybinding_warnings;
pub mod press_enter_to_continue;
pub mod session_background_hint;
pub mod shortcut_hint;
pub mod status_line;
pub mod status_notices;
pub mod teleport_progress;
pub mod token_warning;
pub mod tool_use_loader;

pub use bash_mode_progress::{bash_mode_progress_layout, BashModeProgressLayout, ShellProgress};
pub use configurable_shortcut_hint::{configurable_shortcut_hint, ConfigurableShortcutHintInputs};
pub use dev_bar::{
    dev_bar_layout, format_slow_op, recent_slow_ops, DevBarLayout, SlowOperation,
    DEV_BAR_RECENT_LIMIT, DEV_BAR_TICK_MS,
};
pub use ide_status_indicator::{
    basename, ide_status_indicator, IdeConnectionStatus, IdeSelection, IdeStatusBranch,
};
pub use keybinding_warnings::{
    keybinding_warnings_layout, KeybindingWarning, KeybindingWarningSeverity,
    KeybindingWarningsLayout, KeybindingWarningsRow,
};
pub use press_enter_to_continue::{press_enter_to_continue_text, PRESS_ENTER_TO_CONTINUE};
pub use session_background_hint::{
    resolve_background_shortcut, session_background_hint_visible,
    session_background_keybinding_active, session_background_press, BackgroundPressOutcome,
    SessionBackgroundInputs,
};
pub use shortcut_hint::KeyboardShortcutHint;
pub use status_line::{
    build_status_line_command_input, status_line_padding, status_line_should_display, AgentInfo,
    BuildStatusLineInputs, ContextWindow, CostTotals, EffortInfo, ModelInfo, OutputStyle,
    RateLimitWindow, RateLimits, RemoteInfo, StatusLineCommandInput, StatusLineConfig,
    StatusLineSettings, VimInfo, Workspace, WorktreeSession,
};
pub use status_notices::{status_notices_visible, StatusNoticesContext};
pub use teleport_progress::{
    teleport_progress_layout, TeleportProgressLayout, TeleportProgressStep, TeleportStepRow,
    TeleportStepStatus, SPINNER_FRAMES, TELEPORT_STEPS,
};
pub use token_warning::{
    calculate_token_warning_state, token_warning_layout, TokenWarningInputs, TokenWarningLayout,
    TokenWarningMode, TokenWarningRowColor, TokenWarningState, AUTOCOMPACT_BUFFER_TOKENS,
    ERROR_THRESHOLD_BUFFER_TOKENS, MANUAL_COMPACT_BUFFER_TOKENS, WARNING_THRESHOLD_BUFFER_TOKENS,
};
pub use tool_use_loader::{tool_use_loader, ToolUseLoaderInputs, ToolUseLoaderRow, BLACK_CIRCLE};
