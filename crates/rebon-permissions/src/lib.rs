//! # rebon-permissions - permission dialog types, policies, and projections
//!
//! Pure decision logic for the permissions surface: nothing here renders,
//! performs I/O, or reads a clock - every module hands back owned data that
//! the caller renders or acts on.
//!
//! * tool-kind to permission-dialog routing and the timeout-notification
//!   copy.
//! * dialog chrome/title/badge projections.
//! * the decision-reason vocabulary for rules, hooks, classifiers, and
//!   working-directory blocks.
//! * file permission dialog and handler concerns - file prompt option
//!   generation, feedback-mode reducer, cycle-mode lookup, and
//!   session-allow outcomes.
//! * shell prefix extraction and the Bash/PowerShell option state.
//! * permission-rule escaping/stringification, working-dir guards, and
//!   read/write suggestion generation.
//!
//! Each module's tests pin the current behavioural contract.
//!
//! ## What is in this crate
//!
//! * [`auto_mode_denials`] - the denial record store and the
//!   `/permissions` close-out translation.
//! * [`chrome`] - title/dialog/worker chrome projection with the same
//!   default colors, padding, and team-lead copy.
//! * [`denial_sink`] - the sink and mode-provider traits the engine
//!   broker takes instead of the UI's own state.
//! * [`file_dialog`] - file prompt option builders, the feedback-mode
//!   reducer, cycle-mode lookup, and accept/reject/session handler
//!   outcomes.
//! * [`filesystem`] - `.rebon/` scope detection, working-directory
//!   membership, POSIX read-rule suggestion creation, and the
//!   `generate_suggestions` write/read session-upgrade logic.
//! * [`mode_choice`] - the permission-mode vocabulary the settings
//!   surfaces speak: titles, short titles, symbols, the wire alias, the
//!   user-addressable `ExternalPermissionMode` subset, and the
//!   default-mode picker (which excludes `bypassPermissions` on purpose).
//! * [`mode_policy`] - the single mode/capability table settings
//!   surfaces read.
//! * [`rule_value`] - permission-rule escaping, parsing, and
//!   serialization.
//! * [`shell_runtime`] - simple-command/first-word prefix extraction and
//!   the Bash/PowerShell editable-prefix state.
//! * [`verdict_cache`] - deny/allow fingerprints plus the one-shot
//!   exemptions `/permissions approve` installs.
//! * [`web_fetch`] - hostname resolution for WebFetch rules.
//!
//! ## What is out of scope
//!
//! * The heavy UI trees: question/confirmation dialogs, exit-plan
//!   and computer-use approvals, the permission rule list, and the full
//!   Bash/PowerShell/WebFetch renderers.
//! * Permission-request logging side effects into app state;
//!   this crate keeps only the pure string/log-envelope helpers those
//!   effects depend on.
//! * IDE diff lifecycle, IDE prompt UI, filesystem realpath probing, and
//!   any other live I/O or platform bindings.
//! * Actual widget rendering and input handling. This crate emits owned
//!   data/view models only; the consumer renders them.
//!
//! The only `rebon-*` dependencies are `rebon-tools-core` (the shared
//! allow/deny/ask verdict) and `rebon-shell-policy` (the shell tokenisers),
//! pinned by the allowlist test below.

pub mod auto_mode_denials;
pub mod chrome;
pub mod denial_sink;
pub mod file_dialog;
pub mod filesystem;
pub mod mode_choice;
pub mod mode_policy;
/// The PowerShell argv scanner moved to `rebon-shell-policy` with every
/// other shell tokeniser; re-exported so rule matchers keep one import path.
pub use rebon_shell_policy::powershell_shape;
pub mod rule_value;
pub mod shell_runtime;
pub mod types;
pub mod verdict_cache;
pub mod web_fetch;

pub use auto_mode_denials::{
    resolve_denials_for_permissions_close, AutoModeDenial, AutoModeDenialInput,
    AutoModeDenialStatus, AutoModeDenialStore, DenialReplayRequest, DenialsCloseOutcome,
    PermissionsCloseSelection, AUTO_MODE_DENIAL_DEFAULT_CAPACITY,
};
pub use chrome::{
    permission_dialog_view, permission_request_title_view, worker_badge_view,
    worker_pending_permission_view, PermissionDialogView, PermissionRequestTitleView,
    PermissionSubtitle, PermissionSubtitleView, WorkerBadge, WorkerBadgeView,
    WorkerPendingPermissionView,
};
pub use denial_sink::{
    AutoModeDenialSink, AutoModeHooks, NullDenialSink, PermissionModeProvider, SharedDenialSink,
};
pub use file_dialog::{
    cycle_mode_target, file_permission_options, handle_file_permission_option, FeedbackModeEvent,
    FilePermissionDialogState, FilePermissionHandleResult, FilePermissionOption, PermissionAction,
    SessionScope,
};
pub use filesystem::{
    all_working_directories, create_read_rule_suggestion, directory_for_path, generate_suggestions,
    is_in_global_rebon_folder, is_in_rebon_folder, path_in_allowed_working_path,
    path_in_working_path, to_posix_path, FILE_EDIT_TOOL_NAME, FILE_READ_TOOL_NAME,
    GLOBAL_REBON_FOLDER_PERMISSION_PATTERN, REBON_FOLDER_PERMISSION_PATTERN,
};
pub use mode_choice::{
    default_mode_picker_options, is_default_mode, is_external_permission_mode,
    permission_mode_short_title, permission_mode_symbol, permission_mode_title,
    to_external_permission_mode, ExternalPermissionMode, EXTERNAL_PERMISSION_MODES,
    PERMISSION_MODES,
};
pub use mode_policy::{permission_mode_capability_behavior, PermissionCapability};
pub use rule_value::{
    escape_rule_content, permission_rule_value_from_string, permission_rule_value_to_string,
    unescape_rule_content,
};
pub use shell_runtime::{
    bash_editable_prefix_changed, bash_editable_prefix_state, command_without_cwd_prefix,
    get_first_word_prefix, get_simple_command_prefix, powershell_editable_prefix_changed,
    powershell_editable_prefix_state, refine_bash_editable_prefix,
    refine_powershell_editable_prefix, strip_cwd_prefix, strip_powershell_cwd_prefix,
    toggle_permission_debug, BashEditablePrefixState, BashPrefixContext,
    PowerShellEditablePrefixState,
};
pub use types::{
    FileOperationType, OptionRender, PermissionBehavior, PermissionDecisionReason, PermissionMode,
    PermissionResult, PermissionRule, PermissionRuleSource, PermissionRuleValue,
    PermissionSubmitKind, PermissionUpdate, PermissionUpdateDestination, SelectOption, ThemeColor,
    ToolPermissionContext, ToolType,
};
pub use verdict_cache::AutoModeVerdictCache;
pub use web_fetch::web_fetch_hostname;

#[cfg(test)]
mod compatibility {
    /// Compatibility canary — two `rebon-*` crates, each for the same reason:
    /// the alternative is a second copy of something a permission bug would
    /// hide in.
    ///
    /// * `rebon-tools-core` — the verdict this crate produces (allow / deny /
    ///   ask) is the same value the tool pipeline acts on, so the enum is
    ///   defined once.
    /// * `rebon-shell-policy` — a permission rule matches the words a shell
    ///   would run, so the split into words has to be the one the rest of the
    ///   tree uses. A second tokeniser here is a second set of quoting rules,
    ///   and the gap between them is a bypass.
    ///
    /// Adding any other `rebon-*` dep means updating this list, deliberately.
    const ALLOWED_REBON_DEPS: &[&str] = &["rebon-tools-core", "rebon-shell-policy"];

    #[test]
    fn only_allowed_rebon_deps_in_cargo_toml() {
        let cargo = include_str!("../Cargo.toml");
        for line in cargo.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            if trimmed.starts_with("rebon-") {
                let dep_name = trimmed.split('=').next().unwrap_or("").trim();
                assert!(
                    ALLOWED_REBON_DEPS.contains(&dep_name),
                    "unexpected rebon dep in rebon-permissions; found: {line}"
                );
            }
        }
    }
}
