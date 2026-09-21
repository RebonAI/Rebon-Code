//! `install` and `uninstall`.
//!
//! The only two subcommands that need administrator rights, and the only two
//! that change anything outside this process.
//!
//! ## Uninstall is written first and is the more important one
//!
//! Everything `install` does is a persistent modification to a machine
//! somebody else owns: two local accounts, filters that survive a reboot, a
//! registry key, two profile directories. Shipping that without a way back is
//! not acceptable, so `uninstall` removes each of them and reports what it
//! found — including on a machine where nothing was ever installed, where it
//! succeeds having done nothing.
//!
//! It is also the recovery path for a half-finished install, which is why it
//! never stops at the first thing that is not there.
//!
//! ## Order
//!
//! `uninstall` runs outside-in: ACEs (the only thing touching the user's own
//! files), then filters, then profiles, then accounts, then the group, then
//! the credentials. Profiles before accounts, because a profile is deleted by
//! SID and the SID stops resolving the moment the account is gone.
//!
//! `install` runs the other way: group, accounts, membership, rights,
//! credentials, filters. The credentials are written before the filters
//! because a machine with accounts and no stored password is unusable, while
//! one with accounts and no filters merely has no network isolation — and
//! `status` distinguishes them.

use crate::core::account::{SANDBOX_USER, SANDBOX_USER_NO_NETWORK};
use crate::core::markers::{failure, EXIT_HELPER_FAILURE};
use crate::sys::credentials::Credentials;
use crate::sys::{credentials, elevation, paths, principal, provisioning, random, wfp_install};

pub fn install() -> i32 {
    if let Err(error) = elevation::require_elevation("install") {
        eprintln!("{}", failure(&error.to_string()));
        return EXIT_HELPER_FAILURE;
    }
    match run_install() {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            0
        }
        Err(error) => {
            eprintln!("{}", failure(&error.to_string()));
            eprintln!(
                "{}",
                failure(
                    "install did not finish. Run `sandbox-win.exe uninstall` to return the \
                     machine to a clean state, then try again."
                )
            );
            EXIT_HELPER_FAILURE
        }
    }
}

fn run_install() -> crate::sys::SysResult<Vec<String>> {
    let mut lines = Vec::new();

    // Generated once, here, and handed to both the accounts and the store.
    // The store is the only copy of these; generating them separately is how
    // the account and the credential end up disagreeing with nothing to say
    // so but a logon failure.
    let user_password = random::password()?;
    let no_network_password = random::password()?;

    let report = provisioning::provision(&[
        (SANDBOX_USER, user_password.as_str()),
        (SANDBOX_USER_NO_NETWORK, no_network_password.as_str()),
    ])?;
    for account in &report.accounts_created {
        lines.push(format!("created account       : {account}"));
    }
    for account in &report.accounts_reset {
        lines.push(format!("reset password for     : {account}"));
    }
    if report.group_created {
        lines.push(format!(
            "created group          : {}",
            crate::core::account::SANDBOX_GROUP
        ));
    }
    for note in &report.notes {
        lines.push(format!("note                   : {note}"));
    }

    let path = paths::credentials_path()?;
    paths::ensure_data_directory()?;
    credentials::store(
        &path,
        &Credentials {
            user: user_password,
            user_no_network: no_network_password,
        },
    )?;
    protect_credentials(&path, &mut lines);
    lines.push(format!("stored credentials     : {}", path.display()));
    protect_self(&mut lines);

    // The filters are keyed on the no-network account's SID, which only
    // exists once the account does.
    let sid = principal::lookup_sid(SANDBOX_USER_NO_NETWORK)?.ok_or_else(|| {
        crate::sys::SysError::Invalid(format!(
            "{SANDBOX_USER_NO_NETWORK} was created but its SID cannot be looked up, so the \
             network filters cannot be keyed to it"
        ))
    })?;
    let filters = wfp_install::install(&sid)?;
    lines.push(format!(
        "installed filters      : {} (provider {}, sublayer {})",
        filters.filters_added,
        if filters.provider_added {
            "new"
        } else {
            "existing"
        },
        if filters.sublayer_added {
            "new"
        } else {
            "existing"
        },
    ));

    lines.push(String::new());
    lines.push("The sandbox helper is installed. Running a command does not need".into());
    lines.push("administrator rights; only install and uninstall do.".into());
    lines.push("Your own network is unaffected: the filters match the sandbox".into());
    lines.push(format!("account's SID ({sid}) and nothing else."));
    Ok(lines)
}

/// Deny the sandbox group access to the credentials file.
///
/// The file holds both accounts' passwords. A confined command that reads it can
/// log on as `rebon-sbx` — the account with **no** network filters — and
/// `--block-network` stops meaning anything, silently.
///
/// A note rather than a failure: DPAPI user-scope encryption already stands
/// between the sandbox account and the contents, and refusing to finish the
/// install over the second of two independent protections would leave the machine
/// with neither.
fn protect_credentials(path: &std::path::Path, lines: &mut Vec<String>) {
    let Ok(Some(group)) = principal::lookup_sid(crate::core::account::SANDBOX_GROUP) else {
        lines.push("note                   : could not resolve the sandbox group to deny it the credentials file".into());
        return;
    };
    let ace = crate::core::acl::PlannedAce {
        path: path.to_path_buf(),
        kind: crate::core::acl::AceKind::DenyRead,
        origin: crate::core::acl::AceOrigin::DenyRead,
    };
    match crate::sys::acl_win32::place(path, &ace, &group) {
        Ok(()) => lines.push("denied group read      : credentials.bin".into()),
        Err(error) => lines.push(format!(
            "note                   : the credentials file is DPAPI-encrypted but its ACL \
             could not be tightened ({error})"
        )),
    }
}

/// Deny the sandbox group read and execute on the helper's own binary.
///
/// Defence in depth, and worth being exact about how deep: this is **not** what
/// stops a confined command from re-entering the helper. `exec` needs the stored
/// passwords, and those are DPAPI-encrypted to the calling user *and* sit behind
/// the deny in [`protect_credentials`]; `install` and `uninstall` refuse without
/// elevation; `reap` only revokes ACEs whose owning process is dead, and revoking
/// one needs `WRITE_DAC` on a path the sandbox account does not have it on. Every
/// one of those holds with this ACE absent.
///
/// It is also the one protection here that lapses silently: an upgrade that
/// replaces `sandbox-win.exe` writes a new file with a fresh DACL, and the ACE is
/// gone until the next `install`. Which is why nothing above is allowed to depend
/// on it.
///
/// A note rather than a failure, for the same reason as the credentials: a machine
/// with an installed sandbox and one missing defence-in-depth ACE is better than a
/// machine with a half-finished install.
fn protect_self(lines: &mut Vec<String>) {
    let Ok(binary) = std::env::current_exe() else {
        lines.push(
            "note                   : could not locate this executable to deny the sandbox \
             group execute on it"
                .into(),
        );
        return;
    };
    let Ok(Some(group)) = principal::lookup_sid(crate::core::account::SANDBOX_GROUP) else {
        lines.push(
            "note                   : could not resolve the sandbox group to deny it execute \
             on this executable"
                .into(),
        );
        return;
    };
    let ace = crate::core::acl::PlannedAce {
        path: binary.clone(),
        kind: crate::core::acl::AceKind::DenyExecute,
        origin: crate::core::acl::AceOrigin::DenyRead,
    };
    match crate::sys::acl_win32::place(&binary, &ace, &group) {
        Ok(()) => lines.push(format!(
            "denied group execute   : {}",
            binary
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| binary.display().to_string())
        )),
        Err(error) => lines.push(format!(
            "note                   : could not deny the sandbox group execute on {} \
             ({error})",
            binary.display()
        )),
    }
}

pub fn uninstall() -> i32 {
    if let Err(error) = elevation::require_elevation("uninstall") {
        eprintln!("{}", failure(&error.to_string()));
        return EXIT_HELPER_FAILURE;
    }

    let mut lines = Vec::new();
    let mut problems = Vec::new();

    // 1. The ACEs, first: they are the only thing that touched files the
    //    user owns, and they are what a person would most want back.
    match crate::helper::reap::revoke_everything() {
        Ok(report) => {
            for path in &report.revoked {
                lines.push(format!("removed ACE            : {path}"));
            }
            for (path, reason) in report.refused.iter().chain(report.failed.iter()) {
                problems.push(format!("{path}: {reason}"));
            }
        }
        Err(error) => problems.push(error.to_string()),
    }

    // 1b. The ACE on this binary, before the group is deleted — the ACE
    //     names the group's SID, and once the group is gone the SID resolves
    //     to nothing and the entry becomes an unreadable orphan on a file
    //     the user keeps.
    unprotect_self(&mut lines, &mut problems);

    // 2. Filters.
    match wfp_install::uninstall() {
        Ok(removal) => {
            // Reported only when something was there. "removed filters: 0"
            // on a machine that never had any reads as though the command
            // did something, and the whole value of the clean-machine run is
            // being able to say it did nothing.
            if removal.filters_removed > 0 || removal.sublayer_removed || removal.provider_removed {
                lines.push(format!(
                    "removed filters        : {} (sublayer {}, provider {})",
                    removal.filters_removed,
                    if removal.sublayer_removed {
                        "yes"
                    } else {
                        "absent"
                    },
                    if removal.provider_removed {
                        "yes"
                    } else {
                        "absent"
                    },
                ));
            }
            problems.extend(removal.notes);
        }
        Err(error) => problems.push(error.to_string()),
    }

    // 3. Profiles and accounts, in that order — a profile is deleted by SID,
    //    and the SID stops resolving the moment the account is gone.
    match provisioning::deprovision() {
        Ok(removal) => {
            for account in &removal.profiles_removed {
                lines.push(format!("removed profile        : {account}"));
            }
            for account in &removal.accounts_removed {
                lines.push(format!("removed account        : {account}"));
            }
            if removal.group_removed {
                lines.push(format!(
                    "removed group          : {}",
                    crate::core::account::SANDBOX_GROUP
                ));
            }
            problems.extend(removal.notes);
        }
        Err(error) => problems.push(error.to_string()),
    }

    // 4. Credentials last: while anything above could still fail and need
    //    another run, the passwords are what makes a retry possible.
    match paths::credentials_path() {
        Ok(path) if path.exists() => match std::fs::remove_file(&path) {
            Ok(()) => lines.push(format!("removed credentials    : {}", path.display())),
            Err(error) => problems.push(format!("{}: {error}", path.display())),
        },
        Ok(_) => {}
        Err(error) => problems.push(error.to_string()),
    }

    // 5. The ledger and its lock, and then the directory if nothing else is in it.
    //
    //    Step 1 empties the ledger; it does not delete it. Left alone, `uninstall`
    //    finishes by printing "the machine is back to where it started" over a
    //    directory holding two files it created — which is both untrue and the kind
    //    of residue the ledger exists to avoid.
    remove_data_files(&mut lines, &mut problems);

    if lines.is_empty() && problems.is_empty() {
        println!("Nothing to remove: the sandbox helper is not installed.");
        return 0;
    }
    for line in lines {
        println!("{line}");
    }
    if problems.is_empty() {
        println!();
        println!("The machine is back to where it started.");
        return 0;
    }
    for problem in problems {
        eprintln!("{}", failure(&problem));
    }
    eprintln!(
        "{}",
        failure("uninstall did not remove everything; run it again once the above is resolved")
    );
    EXIT_HELPER_FAILURE
}

/// Take back what [`protect_self`] wrote.
///
/// Silent when there was nothing to remove: `revoke` is idempotent, and on a
/// machine where the ACE was never placed — or where an upgrade already
/// replaced the binary out from under it — there is nothing to report.
fn unprotect_self(lines: &mut Vec<String>, problems: &mut Vec<String>) {
    let (Ok(binary), Ok(Some(group))) = (
        std::env::current_exe(),
        principal::lookup_sid(crate::core::account::SANDBOX_GROUP),
    ) else {
        return;
    };
    let kind = crate::core::acl::AceKind::DenyExecute;
    let had_it = crate::sys::acl_win32::read_aces(&binary)
        .map(|entries| {
            entries.iter().any(|entry| {
                entry.sid == group && entry.is_deny && entry.mask == kind.access_mask()
            })
        })
        .unwrap_or(false);
    match crate::sys::acl_win32::revoke(&binary, kind, &group) {
        Ok(()) if had_it => lines.push(format!(
            "removed ACE            : {} (deny execute)",
            binary.display()
        )),
        Ok(()) => {}
        Err(error) => problems.push(format!("{}: {error}", binary.display())),
    }
}

/// Delete the ledger, its lock, and the directory if it empties out.
///
/// The directory is removed **non-recursively**, and only when it is already
/// empty. It lives under the user's `AppData`, and an `uninstall` that
/// recursively deleted a path it merely believes it owns is a much worse
/// failure than one that leaves a folder behind.
///
/// A held lock is a note rather than a failure: it means another
/// `sandbox-win` process is running, everything else has already been
/// removed, and a second `uninstall` will take it.
fn remove_data_files(lines: &mut Vec<String>, problems: &mut Vec<String>) {
    let Ok(ledger) = paths::ledger_path() else {
        return;
    };
    let lock = {
        let mut path = ledger.clone().into_os_string();
        path.push(".lock");
        std::path::PathBuf::from(path)
    };

    for path in [&ledger, &lock] {
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => lines.push(format!("removed file           : {}", path.display())),
            Err(error) => problems.push(format!("{}: {error}", path.display())),
        }
    }

    if let Ok(directory) = paths::data_directory() {
        // `remove_dir` fails rather than recursing if anything is left, which
        // is the behaviour wanted here — a stray file means somebody put it
        // there and it is not ours to delete.
        if directory.exists() && std::fs::remove_dir(&directory).is_ok() {
            lines.push(format!("removed directory      : {}", directory.display()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_commands_refuse_without_elevation() {
        // The whole point of checking up front: a partially-applied install
        // is the state with no good answer, so nothing may start without the
        // rights to finish.
        if elevation::is_elevated() {
            return;
        }
        assert_eq!(install(), EXIT_HELPER_FAILURE);
        assert_eq!(uninstall(), EXIT_HELPER_FAILURE);
    }
}
