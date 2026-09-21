//! # `view` — state-machine model of sandbox behavior
//!
//! This module provides pure-logic view-models and decision helpers for
//! the terminal UI sandbox surface:
//!
//! * [`config_view`] — the "current sandbox configuration"
//! read-only tab (excluded commands, fs-read/write restrictions,
//! network host lists, unix sockets, glob-pattern warnings,
//! dep-check warnings).
//! * [`overrides`] — the binary `open` / `closed` mode
//! picker whose effect carries the `allow_unsandboxed_commands` flag.
//! * [`dependency`] — the predicates that bucket dependency errors into
//! `ripgrep_missing`, `bwrap_missing`, `socat_missing`, and "other errors",
//! plus the macOS / non-macOS install-hint switch.
//! * [`doctor`] — the
//! doctor-verdict section (`is_supported_platform && sandbox_enabled_in_settings
//! && (has_errors || has_warnings)` gate, `has_errors` wins over
//! `has_warnings` for status color/text, install-hint row only on
//! errors).
//! * [`violation_view`] — the
//! 12-hour clock, the per-violation row formatter, the
//! last-10 tail, the `⧈ Sandbox blocked N total operation(s)`
//! header, and the `… showing last K of N` footer.
//! * [`platform`] — [`SandboxPlatform`], whose wire strings are
//! `"macos"`, `"linux"`, `"windows"`, `"unknown"`.
//! * [`dependency`] — the dependency-check shape (`errors` / `warnings`) and
//! the platform-support predicate (`macos || linux`).
//! * [`fs_config`] — the
//! [`FsReadConfig`],
//! [`FsWriteConfig`], and [`NetworkConfig`]
//! shapes, plus the `"Unsupported platform"` sentinel string.
//! * [`violation`] — the [`SandboxViolationEvent`] shape.
//!
//! Every module pins its behaviour
//! with a comprehensive test table.
//!
//! ## Why this crate has a higher security bar
//!
//! `sandbox` is the surface that decides whether and how Rebon
//! executes bash commands under an OS-level sandbox primitive
//! (Apple seatbelt on macOS, bubblewrap + seccomp on Linux). **Every
//! decision rule below is a security-critical surface.** Whenever the
//! Rust deviates from the documented rules the deviation MUST
//! be intentional and pinned by an explicit test.
//!
//! Pinned sandbox-decision rules:
//!
//! 1. **The doctor verdict short-circuits on unsupported platform.**
//! Windows and `unknown` never see "sandbox available" — they see
//! nothing. A user on an unsupported platform must not be led to
//! believe sandboxing is active. See [`doctor::build_doctor_view`].
//! 2. **The doctor verdict short-circuits on `!sandbox_enabled_in_settings`.**
//! Even on a supported platform, opt-out hides the doctor. See
//! [`doctor::build_doctor_view`].
//! 3. **`has_errors` wins over `has_warnings` for status color/text.**
//! If there are any errors the status is `"Missing dependencies"`
//! in the error color. "Available (with warnings)" never renders
//! when there are errors — that would falsely suggest the sandbox
//! is functional. See [`doctor::DoctorStatus`].
//! 4. **The install hint row renders ONLY when there are errors.**
//! A warnings-only dep check does NOT show `"Run /sandbox for
//! install instructions"`. See [`doctor::DoctorView::show_install_hint`].
//! 5. **The overrides tab short-circuits on `!sandboxing_enabled`.**
//! Message is the literal `"Sandbox is not enabled. Enable sandbox
//! to configure override settings."` See
//! [`overrides::OverridesView::NotEnabled`].
//! 6. **The overrides tab short-circuits on `locked_by_policy`.**
//! A policy-locked workspace shows the managed message and cannot
//! be changed locally. See [`overrides::OverridesView::Locked`].
//! 7. **`open` mode records `allow_unsandboxed_commands: true`.**
//! This is the LESS restrictive setting — bash commands can fall
//! back to running outside the sandbox when they fail inside. The
//! wire string is `"open"`. See [`overrides::OverrideMode`].
//! 8. **The expanded-violation view is rendered ONLY when the
//! platform is NOT Linux.** Linux uses bubblewrap which surfaces
//! violations differently; the expanded view is a macOS-only
//! surface. See [`violation_view::build_violation_view`].
//! 9. **The expanded view respects `total_count` over stale local
//! state.** `total_count == 0` hides the block even if a stale
//! subscription payload still has violations in the local state.
//! 10. **Only the LAST 10 violations are kept.** The
//! footer is clamped via `min(10, total_count)`. See
//! [`violation_view::tail_last_10`].
//! 11. **Dependency-error bucketing is `String::contains`, NOT regex
//! or word-boundary.** A message like `"bwrap-helper failed to
//! spawn"` IS classified as `bwrap_missing`. Case-sensitive. A
//! single error containing both `"bwrap"` and `"socat"` lands in
//! BOTH known buckets and is NOT in `other_errors`. See
//! [`dependency::classify_errors`].
//! 12. **`seccomp_missing` is signalled by `has_warnings()`, NOT
//! by warning content.** Any non-empty warning list is interpreted
//! as "seccomp filter not installed". This is a deliberate
//! over-approximation. See
//! [`dependency::SandboxDependencyCheck::is_seccomp_missing`].
//! 13. **[`violation::format_time`] uses a 12-hour clock with am/pm.**
//! The hour is `hour % 12`, with `0` rendered as `12`; midnight is
//! `12am`, noon is `12pm`.
//! Minutes and seconds are zero-padded to 2 digits. See
//! [`violation::format_time`].
//! 14. **The violation-row command suffix treats the empty command string
//! the same as an absent one.** `Some("")` and `None` produce the SAME output —
//! `"<time> <line>"` with no colon-separated command marker. See
//! [`violation::format_violation_row`].
//! 15. **`is_supported` is `macos || linux`.** Windows and
//! `unknown` are NOT supported. See [`platform::SandboxPlatform::is_supported`].
//!
//! ## What is in this crate
//!
//! * [`platform`] — [`SandboxPlatform`] enum with wire round-trip,
//! `is_supported` / `is_mac` / `is_linux` predicates.
//! * [`dependency`] — [`SandboxDependencyCheck`] shape +
//! [`classify_errors`] bucketing into `ripgrep_missing`,
//! `bwrap_missing`, `socat_missing`, `seccomp_missing`, and
//! `other_errors`, plus the install-hint switch.
//! * [`fs_config`] — [`FsReadConfig`], [`FsWriteConfig`], and
//! [`NetworkConfig`] data shapes with their `has_*` predicates.
//! * [`config_view`] — [`build_config_view`], the pure projection of
//! the sandbox config into an ordered list of [`ConfigSection`]
//! variants, with the disabled-state short-circuit and the
//! managed-domains title switch.
//! * [`overrides`] — [`OverrideMode`] enum + [`build_overrides_view`]
//! three-branch decision (NotEnabled / Locked / Interactive) +
//! [`handle_overrides_input`] reducer mapping
//! [`OverridesInput::Select`] / [`OverridesInput::Cancel`] into
//! [`OverridesEffect`].
//! * [`doctor`] — [`build_doctor_view`] verdict that short-circuits
//! on the three render-nothing cases and surfaces the status
//! color/text + errors + warnings + install-hint row.
//! * [`violation`] — [`SandboxViolationEvent`] shape,
//! [`format_time`] 12-hour clock, and [`format_violation_row`] the
//! `"<time> [<command>: ]<line>"` formatter.
//! * [`violation_view`] — [`build_violation_view`] view-model with
//! the Linux / disabled / zero-count short-circuits, the
//! last-10 tail, the `⧈ Sandbox blocked N …` header, and the
//! `… showing last K of N` footer.
//!
//! ## Outbound seams (each modeled as a pre-built input struct)
//!
//! Every outbound seam is modeled as a pre-built input struct the
//! consumer fills in before calling the view-model, rather than as a
//! probe trait. This is a deliberate convention: the sandbox UI
//! re-reads ALL of the sandbox-manager accessors on every render
//! (there is no incremental state), so a "fill once, call once"
//! input shape is simpler than a trait object and has no
//! observational difference.
//!
//! * **`ConfigViewInputs`** — sandboxing-enabled flag,
//! dependency check, fs-read config, fs-write config,
//! network-restriction config, allowed unix sockets,
//! excluded commands, Linux glob-pattern warnings, and
//! managed-domains-only flag.
//! * **`OverridesInputs`** — sandboxing-enabled flag,
//! policy-locked flag, and
//! unsandboxed-commands-allowed flag.
//! * **`DoctorInputs`** — platform-support flag,
//! sandbox-enabled-in-settings flag, and dependency check.
//! * **`ViolationViewInputs`** — sandboxing-enabled flag,
//! platform, total violation count, and the full
//! violations list from the subscription payload.
//!
//! [`crate::panel`] fills these in, from the settings chain and the same
//! machine probe `/doctor` runs.
//!
//! ## What is still outside this crate
//!
//! Only the rendering layer.
//!
//! * **terminal UI rendering primitives** (boxes, text,
//! the selection widget, tabs, panes, links, the whole widget tree).
//! The crate produces plain `String` and struct outputs with
//! ordered section / row lists. The consumer picks whichever rendering
//! layer it wants (ratatui, raw stdout, or another terminal renderer).
//!
//! ## Why pre-built input structs instead of probe traits
//!
//! The view-models take `ConfigViewInputs` / `OverridesInputs` /
//! `DoctorInputs` / `ViolationViewInputs` structs rather than probe
//! traits because of how the consumer reads them:
//!
//! * The sandbox UI re-reads ALL of the sandbox-manager accessors on
//! EVERY render frame (there is no incremental state for the UI
//! layer to track). A "fill once, call once" input struct is
//! simpler than a trait object and has no observable difference.
//! * The config tab does not own a subscription — the whole thing is
//! a pure projection of 9 adapter fields. Wrapping that in a trait
//! would add 9 methods for no benefit.
//!
//! Future crates that re-read every frame should prefer input
//! structs; crates that poll once or carry incremental state should
//! prefer probe traits.

pub mod config_view;
pub mod dependency;
pub mod doctor;
pub mod fs_config;
pub mod overrides;
pub mod platform;
pub mod violation;
pub mod violation_view;

pub use config_view::{
    build_config_view, build_excluded_commands_value, format_glob_warnings, network_section_title,
    ConfigSection, ConfigView, ConfigViewInputs, EXCLUDED_COMMANDS_NONE_PLACEHOLDER,
    EXCLUDED_COMMANDS_TITLE, FS_READ_TITLE, FS_WRITE_TITLE, GLOB_WARNING_PREAMBLE,
    GLOB_WARNING_TITLE, NETWORK_TITLE_MANAGED, NETWORK_TITLE_UNMANAGED,
    SANDBOX_NOT_ENABLED_MESSAGE, UNIX_SOCKETS_TITLE,
};
pub use dependency::{
    classify_errors, ripgrep_install_hint, DependencyClassification, SandboxDependencyCheck,
    BWRAP_ERROR_TOKEN, BWRAP_INSTALL_HINT, RIPGREP_ERROR_TOKEN, RIPGREP_INSTALL_HINT_MAC,
    RIPGREP_INSTALL_HINT_NON_MAC, SOCAT_ERROR_TOKEN, SOCAT_INSTALL_HINT,
    UNSUPPORTED_PLATFORM_ERROR,
};
pub use doctor::{
    build_doctor_view, status_color, status_text, DoctorInputs, DoctorStatus, DoctorView,
    RUN_SANDBOX_HINT, STATUS_COLOR_ERROR, STATUS_COLOR_WARNING, STATUS_TEXT_ERROR,
    STATUS_TEXT_WARNING,
};
pub use fs_config::{FsReadConfig, FsWriteConfig, NetworkConfig};
pub use overrides::{
    build_options, build_overrides_view, handle_overrides_input, OverrideMode, OverrideOption,
    OverridesEffect, OverridesInput, OverridesInputs, OverridesView, CURRENT_INDICATOR_PLAIN,
    LABEL_CLOSED_BASE, LABEL_OPEN_BASE, MESSAGE_CLOSED, MESSAGE_LOCKED, MESSAGE_NOT_ENABLED,
    MESSAGE_OPEN, MODE_WIRE_CLOSED, MODE_WIRE_OPEN, OVERRIDES_DOCS_URL, TAB_HEADER,
};
pub use platform::SandboxPlatform;
pub use violation::{format_time, format_violation_row, SandboxViolationEvent};
pub use violation_view::{
    build_violation_view, format_footer, format_header, tail_last_10, ViolationView,
    ViolationViewInputs, OPERATION_PLURAL, OPERATION_SINGULAR, SANDBOX_BLOCKED_PREFIX,
    SHOWING_LAST_PREFIX, VIOLATION_TAIL_LIMIT,
};
