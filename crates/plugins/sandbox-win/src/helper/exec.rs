//! The `exec` command.
//!
//! The whole helper exists for this one subcommand: take a command line the
//! caller has already decided is allowed to run, put the rules it named onto
//! the disk, and run it as an account that cannot reach anything outside
//! them.
//!
//! ## The order of operations, and why it is that order
//!
//! 1. **Parse.** An unknown flag is refused, never ignored — a silently
//!    dropped `--deny-read` is a rule the caller believes is in force.
//! 2. **Report degraded masks.** Before anything can fail below, so the
//!    caller learns a `--mask-file` became a denial even if the command never
//!    starts.
//! 3. **Readiness.** Accounts and credentials, checked with a sentence each
//!    rather than left to the first Win32 call that fails.
//! 4. **Reap.** Sessions that are gone lose their ACEs here, on the next
//!    command, rather than at the next reboot.
//! 5. **Record, then place.** The ledger row is written before the ACE, so a
//!    crash between them leaves a row with no ACE — recoverable — rather than
//!    an ACE with no row, which nothing on the machine knows how to remove.
//! 6. **Launch, and relay the exit code verbatim.**
//!
//! ## Refusing is a success
//!
//! Every failure path here exits 126 with a `sandbox-win: ` line, and never
//! runs the command. The one outcome that is not allowed is running it
//! unconfined: the caller has no way to tell that apart from a command that
//! ran inside the sandbox, and it is the difference between a model reading a
//! permission error and a model reading the user's credentials.

use crate::core::account::account_for;
use crate::core::acl::plan_aces;
use crate::core::cmdline::build_command_line;
use crate::core::env::EnvBlock;
use crate::core::markers::{denied, failure, op, EXIT_HELPER_FAILURE};
use crate::core::parse_exec;
use crate::core::{argv::ExecRequest, ledger::Owner};
use crate::sys::acl::SystemAceWriter;
use crate::sys::apply::{self, PrepareContext};
use crate::sys::launch::{self, LaunchRequest};
use crate::sys::ledger_store::LedgerStore;
use crate::sys::{credentials, fileid, paths, principal, process, random, SysError, SysResult};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub fn run(arguments: &[String]) -> i32 {
    let request = match parse_exec(arguments) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("{}", failure(&error.to_string()));
            return EXIT_HELPER_FAILURE;
        }
    };

    // Reported before anything can fail below it. Without a filesystem filter driver
    // the helper cannot make one path answer two processes differently, so a
    // `--mask-file` is a read denial. Saying so is the difference between the caller
    // believing the command read a fake credential and knowing it read a permission
    // error — two problems people debug in two entirely different places.
    let plan = plan_aces(&request);
    for path in &plan.degraded_masks {
        eprintln!("{}", denied(op::MASK_DEGRADED, &path.to_string_lossy()));
    }

    if let Some(reason) = readiness_problem() {
        eprintln!("{}", failure(&reason));
        return EXIT_HELPER_FAILURE;
    }

    match confine_and_run(&request, &plan.aces) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{}", failure(&error.to_string()));
            eprintln!(
                "{}",
                failure("the command was NOT run; it has not been executed unconfined either")
            );
            EXIT_HELPER_FAILURE
        }
    }
}

fn confine_and_run(request: &ExecRequest, aces: &[crate::core::acl::PlannedAce]) -> SysResult<i32> {
    let store = LedgerStore::default_location()?;
    let writer = SystemAceWriter;

    // Whatever previous sessions left behind, before this one adds to it.
    // Doing it first also means a machine that has been killed repeatedly
    // does not accumulate — the ledger is read under a lock either way, so
    // this costs one pass over a list that is normally empty.
    crate::helper::reap::reap_now(&store, &writer)?;

    // The ACEs belong to the *caller's* session, not to this process: the
    // helper exits with the command, and rows owned by it would be reapable
    // before the next command started.
    let owner = process::parent_owner()?;
    let group = principal::lookup_sid(crate::core::account::SANDBOX_GROUP)?.ok_or_else(|| {
        SysError::Invalid(format!(
            "the sandbox group {} does not exist — run `sandbox-win.exe install`",
            crate::core::account::SANDBOX_GROUP
        ))
    })?;

    if !aces.is_empty() {
        apply_rules(&store, &writer, aces, owner, &group)?;
    }

    let account = account_for(request.block_network);
    let secrets = credentials::load(&paths::credentials_path()?)?;
    let password = if request.block_network {
        secrets.user_no_network
    } else {
        secrets.user
    };

    // The base is the sandbox account's own profile, never the caller's. `apply` then
    // layers `--inherit-env`, `--env` and `--unset-env` on top, in that order.
    let mut environment = launch::profile_environment(account, &password)?;
    environment.apply(request, |name| std::env::var(name).ok());
    warn_about_unshaped_environment(&environment, request);

    if let Some(cwd) = request.cwd.as_deref() {
        // Checked here rather than left to `CreateProcessWithLogonW`, which
        // reports a missing working directory as a bare Win32 code from
        // somewhere inside process creation — indistinguishable, to the
        // person reading it, from the sandbox account being unable to reach a
        // directory that is right there.
        if !cwd.is_dir() {
            return Err(SysError::Invalid(format!(
                "the working directory {} does not exist, so the command was not run",
                cwd.display()
            )));
        }
    }

    // Window messages do not cross desktops, so this is what stops a confined command
    // posting a crafted message to a window in the user's session. Held for the whole
    // command: the desktop exists only while a handle to it is open.
    let desktop = isolated_desktop(&group);

    let outcome = launch::run(&LaunchRequest {
        account,
        password: &password,
        // Rebuilt with the `CommandLineToArgvW` inverse, never concatenated:
        // the caller's argv after `--` arrives verbatim, and a PowerShell
        // `-EncodedCommand` payload has to survive it byte for byte.
        command_line: &build_command_line(&request.command),
        working_directory: request.cwd.as_deref(),
        environment: &environment,
        desktop: desktop.as_ref().map(|desktop| desktop.lp_desktop()),
    })?;

    Ok(outcome.exit_code)
}

/// A desktop of the command's own, or a loud line saying why not.
///
/// The one mechanism here that degrades rather than refuses. The other three —
/// the downgraded account, the network filters, the ACEs — each stand alone
/// between the command and something it must not reach, and a command that ran
/// without one of them ran unconfined. A shared desktop is different: the command
/// is still a different user with no path to the user's files or network, and
/// what is lost is one layer against window-message injection.
///
/// Refusing every command on a machine whose window-station policy is unusual
/// would be the worse trade. Doing it quietly would not — the caller asked for an
/// isolated desktop and has no other way to learn it did not get one, so the
/// failure goes to stderr with the marker Rebon reads.
fn isolated_desktop(group: &str) -> Option<crate::sys::desktop::IsolatedDesktop> {
    match crate::sys::desktop::create(group) {
        Ok(desktop) => Some(desktop),
        Err(error) => {
            eprintln!(
                "{}",
                failure(&format!(
                    "the command could not be given a desktop of its own ({error}); it is \
                     still confined by its account, the network filters and the file rules, \
                     but it shares a desktop with your session"
                ))
            );
            None
        }
    }
}

/// Record every row, then write every ACE, then drop the rows that failed.
///
/// The two ledger writes are the point. Between them the ledger claims ACEs
/// that are not on the disk yet, which is the safe direction: `revoke` is
/// idempotent, so a `reap` that finds nothing simply drops the row.
fn apply_rules(
    store: &LedgerStore,
    writer: &SystemAceWriter,
    aces: &[crate::core::acl::PlannedAce],
    owner: Owner,
    group: &str,
) -> SysResult<()> {
    let context = PrepareContext {
        exists: &|path: &Path| path.exists(),
        create_directory: &|path: &Path| {
            std::fs::create_dir(path)
                .map_err(|error| SysError::io("creating a placeholder directory", error))
        },
        observe: &fileid::observe,
        new_id: &new_id,
        owner,
        trustee_sid: group.to_string(),
        now_ms: now_ms(),
    };
    let prepared = apply::prepare(aces, &context)?;

    store.update(|ledger| {
        for item in &prepared {
            ledger.record(item.entry.clone());
        }
        Ok(())
    })?;

    let report = apply::place(&prepared, writer);

    if !report.failed.is_empty() {
        let ids: Vec<String> = report.failed.iter().map(|(id, _)| id.clone()).collect();
        // Dropped, because a row for an ACE that was never written would make
        // the next revocation report a mismatch on a path nothing was done
        // to — and send someone looking for damage that does not exist.
        store.update(|ledger| {
            for id in &ids {
                ledger.remove(id);
            }
            Ok(())
        })?;
        let reasons: Vec<String> = report
            .failed
            .into_iter()
            .map(|(_, reason)| reason)
            .collect();
        return Err(SysError::Invalid(format!(
            "some of the command's rules could not be written, so it was not run: {}",
            reasons.join("; ")
        )));
    }

    Ok(())
}

/// Warn when the environment is missing something a command needs to run.
///
/// Not a failure: which variables matter depends entirely on the command, and
/// refusing to run because `PATH` is absent would be this helper deciding
/// that for the caller. But an absent `PATH` produces "the system cannot find
/// the file specified" from the child, which reads as a broken command rather
/// than a sandbox that did not pass one through.
fn warn_about_unshaped_environment(environment: &EnvBlock, request: &ExecRequest) {
    if request.command.is_empty() {
        return;
    }
    if !environment.contains("PATH") {
        eprintln!(
            "{}",
            failure(
                "the confined command's environment has no PATH — if it cannot find its \
                 executable, that is why"
            )
        );
    }
}

/// A ledger id.
///
/// Random rather than a counter: ids have to be unique across every helper
/// process on the machine, and two commands starting at once would agree on
/// what "the next number" was.
fn new_id() -> String {
    match random::bytes(16) {
        Ok(bytes) => bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        // Only reachable if the system RNG is unavailable, in which case the
        // machine has larger problems. The pid and the clock separate this
        // process from every other one; the counter is what separates two ids
        // minted inside it, because two calls in the same millisecond agree on
        // both of the other two parts — and two ledger rows sharing an id
        // means the second revocation removes the first's row and leaves the
        // first's ACE on the disk.
        Err(_) => {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            format!(
                "{}-{}-{}",
                std::process::id(),
                now_ms(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// The first thing standing between this argv and a confined process.
///
/// Checked in the order a user would fix them, and reported one at a time:
/// four simultaneous complaints about a machine where `install` has simply
/// never run is four times the noise for one action.
fn readiness_problem() -> Option<String> {
    match principal::probe_accounts() {
        Ok(accounts) if accounts.is_complete() => {}
        Ok(accounts) if accounts.is_partial() => {
            return Some(format!(
                "the sandbox accounts are only half created (missing: {}) — run \
                 `sandbox-win.exe uninstall` then `sandbox-win.exe install`",
                accounts.missing().join(", ")
            ))
        }
        Ok(_) => {
            return Some(
                "the sandbox accounts do not exist — run `sandbox-win.exe install` once \
                 (it will ask for administrator rights)"
                    .into(),
            )
        }
        Err(error) => return Some(format!("could not look up the sandbox accounts: {error}")),
    }

    match paths::credentials_path() {
        Ok(path) if credentials::are_usable(&path) => {}
        Ok(path) => {
            return Some(format!(
                "the sandbox accounts' stored credentials could not be read from {} — \
                 re-run `sandbox-win.exe install`",
                path.display()
            ))
        }
        Err(error) => return Some(error.to_string()),
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(|word| (*word).to_string()).collect()
    }

    #[test]
    fn an_unknown_flag_is_refused() {
        // Never ignored. A dropped flag is a rule the caller believes is in
        // force and is not, and the caller has no way to find out.
        assert_eq!(run(&words(&["--nonsense"])), EXIT_HELPER_FAILURE);
    }

    #[test]
    fn an_empty_argv_is_refused() {
        assert_eq!(run(&[]), EXIT_HELPER_FAILURE);
    }

    #[test]
    fn a_missing_command_is_refused() {
        assert_eq!(run(&words(&["--quiet"])), EXIT_HELPER_FAILURE);
    }

    #[test]
    fn ids_do_not_repeat() {
        // Two rows sharing an id means the second revocation removes the
        // first's row and the second ACE stays on the disk.
        let ids: std::collections::HashSet<String> = (0..64).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 64);
    }
}
