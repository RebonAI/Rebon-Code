//! The `reap` command.
//!
//! ACEs are a persistent modification to the user's real disk. When Rebon is
//! killed, they stay: the user's own editor stops being able to open files in
//! their own project, and nothing on the machine explains why. This is what puts
//! them back.
//!
//! Exposed as a command of its own, rather than only running at the start of
//! `exec`, so a diagnostic can hand a user with leftovers a sentence they can
//! copy.
//!
//! The decision logic lives in [`crate::sys::reap`], where it is tested against
//! a fake writer on every platform. This module is the wiring: liveness from the
//! process table, identity from the filesystem, and the real DACL writer.

use crate::core::markers::{failure, EXIT_HELPER_FAILURE};
use crate::sys::acl::SystemAceWriter;
use crate::sys::ledger_store::LedgerStore;
use crate::sys::reap::{reap_dead_sessions, ReapContext, ReapReport};
use crate::sys::{fileid, process, SysResult};
use std::path::Path;

/// Revoke every ACE in the ledger, whatever session placed it — `uninstall`.
///
/// `reap` only touches sessions that are gone, because a live one's command is
/// still relying on its confinement. Uninstall is the opposite case: the helper
/// is being removed, so nothing is going to enforce anything, and an ACE left
/// behind would outlive the software that explains it.
pub fn revoke_everything() -> SysResult<ReapReport> {
    let store = LedgerStore::default_location()?;
    let writer = SystemAceWriter;
    let observe = fileid::observe;
    let remove_directory = |path: &Path| -> SysResult<()> {
        std::fs::remove_dir(path)
            .map_err(|error| crate::sys::SysError::io("removing a placeholder", error))
    };
    store.update(|ledger| {
        let ids: Vec<String> = ledger
            .entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        let context = ReapContext {
            writer: &writer,
            // Every row, regardless of whether its session is running.
            is_alive: &|_| false,
            observe: &observe,
            remove_directory: &remove_directory,
        };
        Ok(crate::sys::reap::revoke_entries(ledger, &ids, &context))
    })
}

/// Reap dead sessions, quietly — the step at the start of every `exec`.
///
/// Shares the store and the writer with the caller rather than opening its own:
/// `exec` is about to take the same lock to record its own rows, and two
/// `LedgerStore` handles in one process would be two paths to the same file with
/// nothing between them.
///
/// Silent on success. `exec`'s stdout belongs to the confined command and its
/// stderr is read by a model — a paragraph about ACEs from a session that ended
/// an hour ago is noise in front of the output somebody actually asked for.
/// Anything still needing a person survives to the next explicit
/// `sandbox-win.exe reap`, which is the command a diagnostic hands them.
pub fn reap_now(store: &LedgerStore, writer: &SystemAceWriter) -> SysResult<ReapReport> {
    let is_alive = process::is_alive;
    let observe = fileid::observe;
    let remove_directory = |path: &Path| -> SysResult<()> {
        std::fs::remove_dir(path)
            .map_err(|error| crate::sys::SysError::io("removing a placeholder", error))
    };
    store.update(|ledger| {
        let context = ReapContext {
            writer,
            is_alive: &is_alive,
            observe: &observe,
            remove_directory: &remove_directory,
        };
        Ok(reap_dead_sessions(ledger, &context))
    })
}

pub fn run() -> i32 {
    let store = match LedgerStore::default_location() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("{}", failure(&error.to_string()));
            return EXIT_HELPER_FAILURE;
        }
    };

    // One lock across decide-and-act, inside `reap_now`. Between reading the ledger
    // and writing it back, ACEs come off the disk; another `exec` reaping
    // concurrently would work from a list of rows that are no longer true.
    //
    // Only the reporting differs from the `exec` path: this one was asked for, so it
    // says what it found.
    let report = match reap_now(&store, &SystemAceWriter) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("{}", failure(&error.to_string()));
            return EXIT_HELPER_FAILURE;
        }
    };

    print_report(&report);
    if report.needs_attention() {
        EXIT_HELPER_FAILURE
    } else {
        0
    }
}

fn print_report(report: &ReapReport) {
    if report.is_empty() {
        println!("Nothing to reap: no sandbox ACEs are outstanding.");
        return;
    }

    for path in &report.revoked {
        println!("removed the sandbox ACE on {path}");
    }
    for (path, reason) in &report.dropped {
        println!("dropped the record for {path}: {reason}");
    }
    for directory in &report.placeholders_removed {
        println!("removed the placeholder directory {}", directory.display());
    }

    // The two that need a person go to stderr with the marker, because they are the
    // cases where something is still on the user's disk.
    for (path, reason) in &report.refused {
        eprintln!("{}", failure(&format!("{path}: {reason}")));
    }
    for (path, reason) in &report.failed {
        eprintln!(
            "{}",
            failure(&format!("{path}: could not remove the ACE — {reason}"))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_report_is_not_a_problem() {
        let report = ReapReport::default();
        assert!(report.is_empty());
        assert!(!report.needs_attention());
    }

    #[test]
    fn a_refusal_needs_attention_and_a_clean_pass_does_not() {
        // The exit code drives a diagnostic: leftovers on the user's disk have to be
        // visible as a failure, and a clean pass must not look like one.
        let clean = ReapReport {
            revoked: vec![r"C:\work\vendor".into()],
            ..Default::default()
        };
        assert!(!clean.needs_attention());

        let refused = ReapReport {
            refused: vec![(r"C:\work\vendor".into(), "the file was replaced".into())],
            ..Default::default()
        };
        assert!(refused.needs_attention());
    }

    #[test]
    fn reaping_an_empty_machine_succeeds() {
        // On a machine where the helper has never placed an ACE this has to be a clean
        // zero, because `exec` calls it before every command.
        if LedgerStore::default_location().is_ok() {
            assert_eq!(run(), 0);
        }
    }
}
