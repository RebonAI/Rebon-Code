//! `rebon rc`: the Remote Control runner.
//!
//! ```text
//! RC server ◀──HTTP: register / poll / ack / heartbeat / stop──▶ rebon rc serve
//!     ▲                                                              │ one environment per machine,
//!     └──────── WS: session_message / session_state / … ───────────▶ │ one work item per session
//!                                                                    ▼
//!                       session host (rebon-session-host): start / resume / follow / command
//!                                                                    │
//!                                          background worker ──▶ ~/.rebon/jobs/<id>/
//! ```
//!
//! **The runner hosts nothing.** A work item's session runs in a Rebon
//! background worker, started or resumed through the session host the way
//! `rebon serve` does it; the runner follows that worker's owner, uplinks
//! what it does, and runs what a controller asks of it. A worker outlives
//! the runner: stopping `rebon rc serve` leaves every session where it was.
//!
//! Where things are decided:
//!
//! - `core` — every decision, with no I/O: which items to take and what
//!   ends them ([`core::work`]), who holds a session ([`core::owner`]),
//!   owner events to frames ([`core::uplink`]), controller frames to
//!   actions and answers ([`core::downlink`]), frame size
//!   ([`core::limits`]), ids ([`core::ids`]), reported state
//!   ([`core::state`]).
//! - [`ports`] — the two seams: the RC server, and the session host.
//! - [`session`] — one work item's run: the pump, the writer, the
//!   heartbeat, the follower.
//! - [`serve`] — registration, polling, dispatch, project refresh,
//!   shutdown.
//! - [`host`] / [`transport`] — the real implementations of the seams.
//! - [`files`] / [`ledger`] — what is kept under `<config home>/rc/`.
//! - [`projects`] — the projects this machine advertises.
//! - [`cli`] — `rebon rc login | serve | status`.
//!
//! An endpoint, like `rebon mcp serve`: it depends on `rebon-session-host`
//! and nothing above it, and only the binary depends on it.

pub mod cli;
pub mod core;
pub mod files;
pub mod host;
pub mod ledger;
pub mod login;
pub mod ports;
pub mod projects;
pub mod serve;
pub mod session;
pub mod transport;

#[cfg(test)]
mod layering {
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("crates/rebon-rc-runner sits two levels below the repo root")
            .to_path_buf()
    }

    fn declares(text: &str, name: &str) -> bool {
        text.lines().any(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with('#')
                && trimmed
                    .strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with(['.', ' ', '=']))
        })
    }

    /// An endpoint: only the binary may depend on it.
    #[test]
    fn only_rebon_cli_may_depend_on_the_runner() {
        let root = repo_root();
        let mut manifests = Vec::new();
        for parent in [root.join("crates"), root.join("crates").join("plugins")] {
            for entry in std::fs::read_dir(&parent).expect("read crates").flatten() {
                let manifest = entry.path().join("Cargo.toml");
                if manifest.is_file() {
                    manifests.push(manifest);
                }
            }
        }
        let dependents: Vec<String> = manifests
            .iter()
            .filter(|manifest| !manifest.starts_with(root.join("crates").join("rebon-rc-runner")))
            .filter(|manifest| {
                std::fs::read_to_string(manifest)
                    .map(|text| declares(&text, "rebon-rc-runner"))
                    .unwrap_or(false)
            })
            .map(|manifest| {
                manifest
                    .parent()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        assert!(
            dependents.iter().all(|name| name == "rebon-cli"),
            "only rebon-cli may depend on rebon-rc-runner; found {dependents:?}"
        );
    }

    /// It is a client of the session host, not a part of the runtime.
    #[test]
    fn the_runner_names_nothing_above_the_session_host() {
        let manifest = std::fs::read_to_string(
            repo_root()
                .join("crates")
                .join("rebon-rc-runner")
                .join("Cargo.toml"),
        )
        .expect("read this crate's manifest");
        for forbidden in [
            "rebon-session-runtime",
            "rebon-cli",
            "rebon-core",
            "rebon-harness",
            "rebon-tui",
            "rebon-config",
        ] {
            assert!(
                !declares(&manifest, forbidden),
                "rebon-rc-runner must not depend on {forbidden}"
            );
        }
    }
}
