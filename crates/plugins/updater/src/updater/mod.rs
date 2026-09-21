//! # `updater` — pure update-decision helpers
//!
//! The half of the `updater` plugin that decides rather than does: no
//! network, no terminal, no config file. It is pure decision logic, and
//! every module below pins its behaviour with a table of cases.
//!
//! It covers:
//!
//! * The install-method branch, the max-version cap, the
//!   success/failure reporting, and the 30-minute polling interval.
//! * The updater-path dispatcher that decides which of the
//!   `Native` / `PackageManager` / `NpmJs` / `None` paths applies,
//!   based on the detected installation type.
//! * The native binary self-update path: the error-message classifier
//!   and the "max-version warning banner" gate.
//! * The package-manager info-only path: the "update available" gate.
//! * The [`InstallStatus`],
//!   [`UpdateDecision`], [`MaxVersionDecision`], and [`should_skip_version`] shapes.
//! * The `gt`/`gte`/`lt`/`lte`/`loose_order` loose
//!   comparators that the updater branches on.
//! * The `InstallationType`
//!   enum and the mapping of
//!   `InstallationType::Native` to the native updater and
//!   `InstallationType::PackageManager` to the package-manager updater.
//! * The `ReleaseChannel` enum (wire strings `"stable"` / `"latest"`).
//!
//! Every module pins its behaviour
//! with a comprehensive test table.
//!
//! ## What is included
//!
//! * [`channel`] — `ReleaseChannel` enum + `parse_channel`,
//!   `default_channel`, `npm_tag`.
//! * [`installation_type`] — `InstallationType` enum with the wire
//!   discriminant `"npm-global" | "npm-local" | "native" |
//!   "package-manager" | "development" | "unknown"`.
//! * [`updater_path`] — pure dispatcher that maps an `InstallationType`
//!   (plus the auto-updates-disabled flag) to one of `Native`,
//!   `PackageManager`, `NpmJs`, `None`.
//! * [`semver_compare`] — pure `gt`/`gte`/`lt`/`lte`/`loose_order` loose
//!   semver comparator (loose parsing, no strictness).
//! * [`max_version`] — `apply_max_version_cap`, the
//!   max-version kill-switch logic, plus the warning-banner branch.
//! * [`should_skip`] — `should_skip_version`.
//! * [`update_decision`] — `decide_update`: the pure
//!   update-needed gate (`!disabled && current &&
//!   latest && !gte(current, latest) && !should_skip_version(latest)`).
//!   Returns a typed `UpdateDecision` rather than calling out to side
//!   effects.
//! * [`installation_method`] — `select_install_method`, the branch that
//!   picks `local` vs `global`
//!   based on the detected installation type plus the configured
//!   install method as the fallback for unknown types.
//! * [`error_classification`] — `classify_error_message`, the
//!   substring walk.
//! * [`state_machine`] — pure transition function for the
//!   `(phase, current_flow, versions)` state machine. Inputs are
//!   timestamps + uuid
//!   strings supplied by the caller (same pattern as
//!   `rebon-tui::state` — see that file's "Cancel-commit semantics"
//!   doc block for the rationale).
//! * [`render_decision`] — pure visibility predicate: whether an
//!   updater should render at all. Lets a renderer ask "is this updater visible right
//!   now?" without owning the widget tree.
//!
//! ## What has been deliberately deferred
//!
//! The full updater is heavily I/O-bound: it spawns
//! subprocesses (`npm install -g`, native binary download), reads/writes config files, makes network requests for
//! the update manifest, and replaces the running executable. **None
//! of that is portable as pure logic** and all of it lives in
//! other modules:
//!
//! * **Subprocess spawning.** These command-execution concerns take the
//!   state-machine outputs of this crate (a "yes update" decision, a
//!   chosen install method, a parsed channel) as inputs and would
//!   produce the `InstallStatus` values that this crate's state machine
//!   consumes — no code outside [`state_machine`] builds one today.
//!   They belong with process-spawning code.
//! * **Network fetches.** These are
//!   transport code; they hand a `Option<String>` (latest version)
//!   to `decide_update` and a `Option<String>` (max version) to
//!   `apply_max_version_cap`. They belong with update-fetching code.

//! * **Filesystem operations** (lock acquisition, lock recovery,
//!   stale-lock TOCTOU re-check, executable atomic replacement,
//!   symlink removal). The lock-recovery state machine has
//!   non-trivial logic — but the lock file handling belongs with the
//!   filesystem code ([`update_lock`] holds the lock file itself). The
//!   signed-binary verification (`Checksum mismatch` error path) belongs
//!   there too.
//! * **UI rendering primitives** (widgets, boxes, text). The render-side of this
//!   crate is [`render_decision`] returning a typed
//!   `RenderVisibility` enum + a typed message constructor —
//!   renderers (ratatui, plain stdout, …) translate
//!   that into widgets.
//! * **Feature-flag gating.** The Rust dispatcher
//!   accepts a [`FeatureFlags`]
//!   record so the caller can pre-resolve the flags; the dispatcher
//!   itself is feature-flag-agnostic.
//! * **The polling loop.** Nothing polls on a timer yet: the startup
//!   check runs once per process, and there is no orchestrator holding
//!   a 30-minute timer. The interval is pinned here as
//!   [`POLL_INTERVAL_MS`] for the scheduler that will own that loop.
//!
//! These were deferred rather than carried as stubs because
//! *misaligned stubs have negative value*: they hint at the wrong API and force
//! consumers to either preserve the mistake or do a
//! disruptive rename. Each deferred concern has a clear owner
//! and the types this crate exports do not
//! force that future work into a specific shape.
//!
//! ## Dependency boundary
//!
//! `rebon-plugin-updater` does not depend on `rebon-render`. Markdown
//! rendering is handled elsewhere in the TUI shell
//! (the updater renders inside the same TUI shell that uses
//! Markdown for assistant text), but at the Rust crate level
//! `rebon-plugin-updater` and `rebon-render` have **disjoint imports**:
//! neither depends on the other. The compatibility test
//! `compatibility_with_rebon_render_is_disjoint` documents
//! this; nothing enforces it at compile time, so review is what
//! catches a future circular reference.
//!
//! ## Pinned constants
//!
//! * `POLL_INTERVAL_MS = 30 * 60 * 1000` — 30 minutes. Pinned in
//!   [`POLL_INTERVAL_MS`] and asserted against both that expression and
//!   the literal `1800000`.
//! * `LOCK_TIMEOUT_MS = 5 * 60 * 1000` — 5 minutes. Pinned in
//!   [`LOCK_TIMEOUT_MS`] and asserted against the literal.
//!   The fs lock itself lives in [`update_lock`], which carries no
//!   timeout logic; the constant lives here so the updater API has
//!   exactly one pinned test.

pub mod channel;
pub mod error_classification;
pub mod installation_detection;
pub mod installation_method;
pub mod installation_type;
pub mod max_version;
pub mod render_decision;
pub mod semver_compare;
pub mod should_skip;
pub mod state_machine;
pub mod update_decision;
pub mod update_lock;
pub mod update_service;
pub mod update_state;
pub mod updater_path;

pub use channel::{default_channel, npm_tag, parse_channel, ReleaseChannel};
pub use error_classification::{classify_error_message, ErrorKind};
pub use installation_detection::{
    detect_installation_from_path, InstallationDetection, InstallationDetectionOptions,
};
pub use installation_method::{select_install_method, InstallMethod, InstallMethodChoice};
pub use installation_type::{parse_installation_type, InstallationType};
pub use max_version::{apply_max_version_cap, MaxVersionDecision};
pub use render_decision::{decide_render, RenderVisibility};
pub use semver_compare::{gt, gte, loose_order, lt, lte, Ordering as SemverOrdering};
pub use should_skip::should_skip_version;
pub use state_machine::{transition, UpdateEvent, UpdateFlow, UpdateState, UpdaterPhase};
pub use update_decision::{decide_update, UpdateDecision};
pub use update_lock::{update_lock_path, UpdateLock, UpdateLockError, UPDATE_LOCK_FILE_NAME};
pub use update_service::{
    install_service, migrate_legacy_supervisor_service, service_install_spec,
    service_registration_status, service_uninstall_spec, supervisor_service_install_spec,
    supervisor_service_registration_status, supervisor_service_uninstall_spec, uninstall_service,
    RealServiceCommandRunner, RealServiceFileSystem, ServiceCommandOutput, ServiceCommandRunner,
    ServiceCommandSpec, ServiceFileSystem, ServiceInstallSpec, ServicePlatform,
    ServiceRegistrationError, ServiceRegistrationStatus, SERVICE_TASK_NAME,
    SUPERVISOR_SERVICE_TASK_NAME,
};
pub use update_state::{
    deserialize_update_state, read_update_state, serialize_update_state, update_state_path,
    write_update_state, PersistedUpdateState, UPDATE_STATE_FILE_NAME,
};
pub use updater_path::{select_updater_path, FeatureFlags, UpdaterChoice};

/// 30 minutes, in milliseconds. Asserted against both the
/// expression `30 * 60 * 1000` and the precomputed literal `1800000`,
/// so the two spellings cannot drift. Owned
/// here so the interval constant lives in exactly one
/// place.
pub const POLL_INTERVAL_MS: u64 = 30 * 60 * 1000;

/// 5 minutes, in milliseconds. Reserved for stale-lock detection: the
/// fs lock implementation ([`update_lock`]) is a plain `create_new` +
/// `remove_file` and consults no timeout yet. Asserted against
/// the literal `5 * 60 * 1000`.
pub const LOCK_TIMEOUT_MS: u64 = 5 * 60 * 1000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_is_thirty_minutes() {
        // Both spellings of the 30-minute interval must agree:
        // `30 * 60 * 1000` and the precomputed `1800000`.
        assert_eq!(POLL_INTERVAL_MS, 30 * 60 * 1000);
        assert_eq!(POLL_INTERVAL_MS, 1_800_000);
    }

    #[test]
    fn lock_timeout_is_five_minutes() {
        // 5 minutes equals 300000 ms.
        assert_eq!(LOCK_TIMEOUT_MS, 5 * 60 * 1000);
        assert_eq!(LOCK_TIMEOUT_MS, 300_000);
    }

    /// Compatibility canary — this crate must NOT take a direct dependency on
    /// `rebon-render`. Nothing here is checked at compile time: the body
    /// below is a no-op, so adding `rebon-render` to `Cargo.toml` would
    /// *not* break this test.
    ///
    /// We can't do a real "this crate isn't in deps" assertion at
    /// compile time, so this test documents the contract and review
    /// catches a violation.
    #[test]
    fn compatibility_with_rebon_render_is_disjoint() {
        // No-op assertion; the doc comment above is the contract.
        // If `rebon-render` is ever added to `Cargo.toml`'s
        // `[dependencies]`, this test should be deleted at the same
        // time as the dep is added (and a real composition test added
        // to the new combined module). The const expression below is
        // the contract: this crate exposes no symbol named
        // `rebon_render`.
        const _DISJOINT_CANARY: () = ();
    }
}
