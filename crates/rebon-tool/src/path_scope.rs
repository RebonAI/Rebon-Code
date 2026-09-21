use std::path::{Path, PathBuf};

use rebon_tools_core::{
    canonicalize_scope_path, lexically_normalize_path, scope_path_starts_with, ToolError, ToolId,
    ToolResult,
};

use crate::ToolContext;

/// Resolve `path` against `cwd` when it is relative.
///
/// Public because the code that decides a child agent's authorized roots
/// uses it, and every root on both sides of that comparison has to be
/// resolved the same way.
pub fn resolve_context_path(path: &Path, cwd: &Path, _context: &ToolContext) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Every rule a path must pass before a tool may read what is at it: the
/// session credentials nobody may read, then the roots a sub-agent is confined
/// to.
pub fn enforce_read_path_policy(
    tool: ToolId,
    context: &ToolContext,
    path: &Path,
    field: &str,
) -> ToolResult<()> {
    enforce_session_credential_denial(tool.clone(), context, path, field, "read")?;
    let roots = subagent_path_scope_roots(context);
    if roots.is_empty() {
        return Ok(());
    }
    enforce_path_scope(tool, context, path, field, &roots, "file access", false)
}

/// The same, for a tool that would change what is at the path.
pub(crate) fn enforce_write_path_policy(
    tool: ToolId,
    context: &ToolContext,
    path: &Path,
    field: &str,
) -> ToolResult<()> {
    enforce_session_credential_denial(tool.clone(), context, path, field, "write")?;
    if let Some(roots) = context.write_scope_roots() {
        return enforce_path_scope(tool, context, path, field, roots, "file mutation", true);
    }
    let roots = subagent_path_scope_roots(context);
    if roots.is_empty() {
        return Ok(());
    }
    enforce_path_scope(tool, context, path, field, &roots, "file mutation", true)
}

/// The two files that carry a live session's control credentials, named as a
/// refusal would name them.
///
/// A worker publishes its loopback port and token in exactly two places: the
/// job record it runs from (`<config>/jobs/<id>/state.json`) and the descriptor
/// beside the transcript (`<config>/projects/<project>/<sid>.owner.json`).
/// Both sit under the user's own config directory, which file tools can
/// otherwise read like anything else — so with sessions hosted in workers, one
/// `Read` is enough to drive somebody's live session: inject a prompt, answer
/// a permission request, change the permission mode. The token is not really
/// the secret; the authority it carries is.
///
/// The write side is refused for the same reason rather than a different one:
/// a descriptor an agent can edit is an endpoint an agent can point elsewhere.
///
/// The two names are matched here as lowercased suffixes rather than compared
/// against a shared constant: this crate does not depend on the session host
/// at all, and the comparison is deliberately case-insensitive, which a
/// constant equality would not be. If either file is ever renamed, this is
/// the other place to change.
fn session_credential_kind(path: &Path) -> Option<&'static str> {
    let config_home = rebon_session::config_home_with_env(|name| std::env::var_os(name))?;
    session_credential_kind_under(path, &config_home)
}

/// Split from its caller so tests can name a config home directly. Reaching it
/// through `REBON_CONFIG_DIR` instead would mean a test that mutates process
/// environment — which is already the cause of one cross-test flake in this
/// workspace.
fn session_credential_kind_under(path: &Path, config_home: &Path) -> Option<&'static str> {
    // Normalised first: the comparison has to survive `..`, a symlink, and
    // Windows' case-insensitivity, none of which change which file is opened.
    let normalized = normalize_for_scope(path);
    let name = normalized.file_name()?.to_str()?.to_ascii_lowercase();
    if name.ends_with(".owner.json") && is_within_scope(&normalized, &config_home.join("projects"))
    {
        return Some("a session owner descriptor");
    }
    if name == "state.json" && is_within_scope(&normalized, &config_home.join("jobs")) {
        return Some("a background job record");
    }
    None
}

/// Refuse a path that would hand over control of a running session.
///
/// The refusal names the file and says why, and deliberately carries none of
/// its content — a refusal that quoted the line it refused would be the leak
/// it exists to prevent.
fn enforce_session_credential_denial(
    tool: ToolId,
    context: &ToolContext,
    path: &Path,
    field: &str,
    operation: &str,
) -> ToolResult<()> {
    let path = context
        .cwd()
        .map(PathBuf::from)
        .map(|cwd| resolve_context_path(path, &cwd, context))
        .unwrap_or_else(|| path.to_path_buf());
    let Some(kind) = session_credential_kind(&path) else {
        return Ok(());
    };
    Err(ToolError::InvalidInput {
        tool,
        reason: format!(
            "`{field}` names {kind} (`{}`), which carries the loopback endpoint \
             and token that command a live session. Rebon refuses to {operation} it: \
             reading one is enough to drive somebody's session, and writing one \
             redirects where that session's commands go.",
            path.display()
        ),
        error_code: Some(400),
    })
}

/// Whether this path is one of the credential files, for a caller that has to
/// *skip* rather than refuse.
///
/// A directory search is the case this exists for: `Grep` is handed a
/// directory, so the path check above sees a directory and lets it through —
/// and then the walk opens every file underneath, one of which may be a job
/// record. Failing the whole search because a credential file happened to be
/// inside the tree would be the wrong answer; leaving it out of the walk is
/// the right one.
pub(crate) fn is_session_credential_path(path: &Path) -> bool {
    session_credential_kind(path).is_some()
}

/// The shell's version of the same refusal.
///
/// A shell command takes no path parameter, so the check above never sees it —
/// `cat`, `type`, `Get-Content` and everything like them read a file with the
/// path buried in a command string. Classifying such a command as sensitive
/// (which `rebon_tools_core` also does) only stops it where somebody is
/// watching: an unattended run auto-approves, and `rebon exec` says so in its
/// own help. A credential that is refused to `Read` and handed to `cat` is not
/// refused, so the shell tools refuse it too.
///
/// The predicate is shared with the classifier rather than written twice —
/// there should be one answer to "does this command name a session
/// credential", whichever layer is asking.
pub(crate) fn refuse_session_credential_command(
    tool: ToolId,
    tool_name: &str,
    input: &serde_json::Value,
) -> ToolResult<()> {
    if !rebon_shell_policy::is_session_credential_access_command(tool_name, input) {
        return Ok(());
    }
    Err(ToolError::InvalidInput {
        tool,
        reason: "This command names a file that carries a live session's loopback \
                 endpoint and token (a background job record or a session owner \
                 descriptor). Rebon refuses it: reading one is enough to drive \
                 somebody's session, and writing one redirects where that \
                 session's commands go."
            .to_string(),
        error_code: Some(400),
    })
}

fn enforce_path_scope(
    tool: ToolId,
    context: &ToolContext,
    path: &Path,
    field: &str,
    roots: &[PathBuf],
    operation: &str,
    mutation: bool,
) -> ToolResult<()> {
    let cwd = context.cwd().map(PathBuf::from);
    let path = cwd
        .as_deref()
        .map(|cwd| resolve_context_path(path, cwd, context))
        .unwrap_or_else(|| path.to_path_buf());

    let authorized = if mutation {
        mutation_path_is_within_roots(&path, roots)
    } else {
        path_is_within_roots(&path, roots)
    };
    if authorized {
        return Ok(());
    }

    Err(ToolError::InvalidInput {
        tool,
        reason: format!(
            "Sub-agent {operation} `{field}` is limited to authorized roots {}; `{}` is outside that scope.",
            format_roots(roots),
            path.display()
        ),
        error_code: Some(400),
    })
}

fn subagent_path_scope_roots(context: &ToolContext) -> Vec<PathBuf> {
    if context.agent_id().is_none() {
        return Vec::new();
    }
    if !context.path_scope_roots().is_empty() {
        return context.path_scope_roots().to_vec();
    }
    context
        .cwd()
        .map(PathBuf::from)
        .into_iter()
        .collect::<Vec<_>>()
}

/// Render a root list for a message the user reads. Public so the `Agent`
/// tool's permission prompt spells roots the same way every path-scope
/// denial does.
pub fn format_roots(roots: &[PathBuf]) -> String {
    let rendered = roots
        .iter()
        .map(|root| format!("`{}`", root.display()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{rendered}]")
}

pub fn path_is_within_root(path: &Path, root: &Path) -> bool {
    is_within_scope(path, root)
}

/// Directory holding coordinator worker report files.
///
/// The single source of truth for a path two crates need to agree on:
/// the coordinator writes reports here, and the Agent tool has to
/// recognise it so a caller is not prompted to authorize a directory
/// the runtime already scopes per worker.
///
/// Anchored to the resolved config home rather than the raw home directory:
/// this used to be the one Rebon-owned directory that ignored
/// `REBON_CONFIG_DIR`, so moving the data to another disk left worker reports
/// writing back to the profile drive.
pub fn worker_report_dir() -> PathBuf {
    rebon_session::config_home_with_env(|name| std::env::var_os(name))
        .unwrap_or_else(|| PathBuf::from(".rebon"))
        .join("tasks")
}

/// Is this path the worker report directory, or something inside it?
///
/// Such a root never needs the caller's authorization: the runtime
/// grants each worker exactly one file in here — its own report — and
/// grants it whether or not the caller asked. Treating the directory as
/// an external root to be approved produced a prompt per spawn that
/// bought nobody anything: approving it did not make the report
/// writable (that comes from the runtime), and refusing it did not make
/// the report optional.
pub fn is_worker_report_path(path: &Path) -> bool {
    is_within_scope(path, &worker_report_dir())
}

/// Is this path inside any of these roots?
///
/// The plural of [`path_is_within_root`], and public for the same reason: a
/// plugin that has to answer "is this inside the roots I allow" must ask the
/// one function that knows what "inside" means here — symlinks resolved,
/// `..` collapsed, Windows prefixes normalised. A second `roots.iter().any`
/// somewhere else is a second answer waiting to disagree.
pub fn path_is_within_roots(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| is_within_scope(path, root))
}

pub(crate) fn mutation_path_is_within_roots(path: &Path, roots: &[PathBuf]) -> bool {
    path_is_within_roots(path, roots) && mutation_target_has_single_link_or_is_missing(path)
}

#[cfg(unix)]
fn mutation_target_has_single_link_or_is_missing(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.nlink() <= 1,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

#[cfg(windows)]
fn mutation_target_has_single_link_or_is_missing(path: &Path) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
        Ok(metadata) if !metadata.file_type().is_file() => return true,
        Ok(_) => {}
    }
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    let succeeded = unsafe {
        GetFileInformationByHandle(file.as_raw_handle() as HANDLE, info.as_mut_ptr()) != 0
    };
    succeeded && unsafe { info.assume_init() }.nNumberOfLinks <= 1
}

#[cfg(not(any(unix, windows)))]
fn mutation_target_has_single_link_or_is_missing(_path: &Path) -> bool {
    true
}

fn is_within_scope(path: &Path, root: &Path) -> bool {
    let path = normalize_for_scope(path);
    let root = normalize_for_scope(root);
    scope_path_starts_with(&path, &root)
}

/// Canonical where it exists, anchored on the deepest existing ancestor for a
/// not-yet-created target (a `Write` creating a new file), and purely lexical
/// when nothing on the path exists — so both sides of a scope comparison
/// take the same shape.
fn normalize_for_scope(path: &Path) -> PathBuf {
    canonicalize_scope_path(path).unwrap_or_else(|| lexically_normalize_path(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The descriptor beside a transcript carries the port and token that
    /// command that session, so reading it is not a read — it is a handover.
    #[test]
    fn a_session_owner_descriptor_is_refused() {
        let home = PathBuf::from("/config");
        assert_eq!(
            session_credential_kind_under(
                Path::new("/config/projects/f--repo/sess-1.owner.json"),
                &home
            ),
            Some("a session owner descriptor")
        );
    }

    /// The job record carries the same pair for a worker-hosted session.
    #[test]
    fn a_background_job_record_is_refused() {
        let home = PathBuf::from("/config");
        assert_eq!(
            session_credential_kind_under(Path::new("/config/jobs/bg-1/state.json"), &home),
            Some("a background job record")
        );
    }

    /// Windows opens `STATE.JSON` and `state.json` as the same file, so the
    /// refusal cannot be case-sensitive where the filesystem is not.
    #[test]
    fn the_refusal_does_not_depend_on_case() {
        let home = PathBuf::from("/config");
        assert_eq!(
            session_credential_kind_under(Path::new("/config/jobs/bg-1/STATE.JSON"), &home),
            Some("a background job record")
        );
    }

    /// `..` is resolved before the comparison: a path that lands on the file is
    /// the file, however it was spelled.
    #[test]
    fn a_parent_escape_into_the_config_home_is_still_refused() {
        let home = PathBuf::from("/config");
        assert_eq!(
            session_credential_kind_under(
                Path::new("/config/projects/../jobs/bg-1/state.json"),
                &home
            ),
            Some("a background job record")
        );
    }

    /// `state.json` is an ordinary name — plenty of projects have one — and the
    /// transcript next to a descriptor is the session's actual content, which
    /// tools are meant to read. Refusing either would be the rule overreaching.
    #[test]
    fn ordinary_files_are_not_mistaken_for_credentials() {
        let home = PathBuf::from("/config");
        assert_eq!(
            session_credential_kind_under(Path::new("/repo/app/state.json"), &home),
            None,
            "a project's own state.json is not Rebon's"
        );
        assert_eq!(
            session_credential_kind_under(
                Path::new("/config/projects/f--repo/sess-1.jsonl"),
                &home
            ),
            None,
            "the transcript itself stays readable"
        );
        assert_eq!(
            session_credential_kind_under(Path::new("/config/config.json"), &home),
            None,
            "the config file is not a session credential"
        );
        assert_eq!(
            session_credential_kind_under(Path::new("/config/jobs/bg-1/events.jsonl"), &home),
            None,
            "the job's event log carries no token"
        );
    }

    /// The whole point is that this holds for the main agent too, not only for
    /// a sub-agent with scope roots: hosted sessions make any file tool a
    /// remote control if this file is readable.
    #[test]
    fn the_credential_refusal_applies_without_any_subagent_scope() {
        let credential = rebon_session::config_home_with_env(|name| std::env::var_os(name))
            .expect("a config home")
            .join("jobs")
            .join("bg-probe")
            .join("state.json");
        let context = ToolContext::new().with_cwd("/repo");

        let read =
            enforce_read_path_policy(ToolId::new("Read"), &context, &credential, "file_path")
                .expect_err("reading a job record is refused");
        let write =
            enforce_write_path_policy(ToolId::new("Write"), &context, &credential, "file_path")
                .expect_err("writing one is refused for the same reason");

        for error in [read, write] {
            let ToolError::InvalidInput { reason, .. } = error else {
                panic!("the refusal is an input error");
            };
            assert!(
                reason.contains("a background job record"),
                "the refusal says which kind of file it is: {reason}"
            );
            assert!(
                !reason.contains("token\":"),
                "and carries none of the file's content: {reason}"
            );
        }
    }

    /// A shell hides the path inside a command string, so the path check never
    /// sees it. Classifying it as sensitive only helps where somebody is
    /// watching — `rebon exec` auto-approves by design — so the shell tools
    /// refuse it outright, the same as `Read` does.
    #[test]
    fn a_shell_command_naming_a_credential_is_refused() {
        for command in [
            "cat ~/.rebon/jobs/bg-1/state.json",
            "Get-Content $env:USERPROFILE\\.rebon\\projects\\p\\sess-1.owner.json",
        ] {
            let error = refuse_session_credential_command(
                ToolId::new("Bash"),
                "Bash",
                &serde_json::json!({ "command": command }),
            )
            .expect_err("the shell may not read what Read may not read");
            let ToolError::InvalidInput { reason, .. } = error else {
                panic!("the refusal is an input error");
            };
            assert!(
                reason.contains("loopback endpoint"),
                "the refusal explains itself: {reason}"
            );
        }
    }

    /// `Grep` is handed a directory, so the path check sees a directory and
    /// lets it through — the walk underneath is where a credential would be
    /// read and returned as matched lines.
    #[test]
    fn a_directory_walk_skips_credentials_and_keeps_their_neighbours() {
        // Both the home this builds paths under and the one
        // `is_session_credential_path` resolves come from the process
        // environment, so they have to be read under the same lock the
        // tests that rewrite `REBON_CONFIG_DIR` take — otherwise one of
        // them can move between the two reads.
        let _env = crate::env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = rebon_session::config_home_with_env(|name| std::env::var_os(name))
            .expect("a config home");
        assert!(is_session_credential_path(
            &home.join("jobs").join("bg-1").join("state.json")
        ));
        assert!(is_session_credential_path(
            &home
                .join("projects")
                .join("f--repo")
                .join("sess-1.owner.json")
        ));
        assert!(
            !is_session_credential_path(&home.join("jobs").join("bg-1").join("events.jsonl")),
            "the job's own event log is not a credential"
        );
        assert!(
            !is_session_credential_path(
                &home.join("projects").join("f--repo").join("sess-1.jsonl")
            ),
            "and neither is the transcript"
        );
    }

    #[test]
    fn an_ordinary_shell_command_is_left_alone() {
        assert!(refuse_session_credential_command(
            ToolId::new("Bash"),
            "Bash",
            &serde_json::json!({ "command": "cargo test -p rebon-tool" }),
        )
        .is_ok());
    }

    #[test]
    fn lexical_scope_allows_child_paths() {
        assert!(is_within_scope(
            Path::new("/repo/crates/lib.rs"),
            Path::new("/repo")
        ));
    }

    #[test]
    fn lexical_scope_rejects_parent_escape() {
        assert!(!is_within_scope(
            Path::new("/repo/../etc/hostname"),
            Path::new("/repo")
        ));
    }

    #[test]
    fn subagent_scope_uses_explicit_roots_when_present() {
        let context = ToolContext::new()
            .with_agent_id("agent-test")
            .with_cwd("/repo")
            .with_path_scope_roots([PathBuf::from("/shared")]);

        assert!(enforce_read_path_policy(
            ToolId::new("Read"),
            &context,
            Path::new("/shared/file.txt"),
            "file_path"
        )
        .is_ok());
        assert!(enforce_read_path_policy(
            ToolId::new("Read"),
            &context,
            Path::new("/repo/file.txt"),
            "file_path"
        )
        .is_err());
    }

    #[test]
    fn explicit_write_scope_is_narrower_than_read_scope() {
        let context = ToolContext::new()
            .with_agent_id("agent-test")
            .with_cwd("/repo")
            .with_path_scope_roots([PathBuf::from("/repo"), PathBuf::from("/scratch")])
            .with_write_scope_roots([PathBuf::from("/scratch")]);

        assert!(enforce_read_path_policy(
            ToolId::new("Read"),
            &context,
            Path::new("/repo/src/lib.rs"),
            "file_path"
        )
        .is_ok());
        assert!(enforce_write_path_policy(
            ToolId::new("Write"),
            &context,
            Path::new("/scratch/result.txt"),
            "file_path"
        )
        .is_ok());
        assert!(enforce_write_path_policy(
            ToolId::new("Write"),
            &context,
            Path::new("/repo/src/lib.rs"),
            "file_path"
        )
        .is_err());
    }

    #[test]
    fn explicit_write_scope_rejects_existing_hard_link_alias() {
        let dir = tempfile::tempdir().unwrap();
        let scratchpad = dir.path().join("scratchpad");
        let outside = dir.path().join("outside.txt");
        let alias = scratchpad.join("alias.txt");
        std::fs::create_dir_all(&scratchpad).unwrap();
        std::fs::write(&outside, "outside").unwrap();
        std::fs::hard_link(&outside, &alias).unwrap();
        let context = ToolContext::new()
            .with_agent_id("verification")
            .with_path_scope_roots([scratchpad.clone()])
            .with_write_scope_roots([scratchpad]);

        assert!(
            enforce_read_path_policy(ToolId::new("Read"), &context, &alias, "file_path").is_ok()
        );
        assert!(
            enforce_write_path_policy(ToolId::new("Write"), &context, &alias, "file_path").is_err()
        );
    }

    #[test]
    fn empty_explicit_write_scope_denies_all_mutations() {
        let context = ToolContext::new()
            .with_agent_id("agent-test")
            .with_path_scope_roots([PathBuf::from("/repo")])
            .with_write_scope_roots(Vec::<PathBuf>::new());

        assert!(enforce_write_path_policy(
            ToolId::new("Edit"),
            &context,
            Path::new("/repo/src/lib.rs"),
            "file_path"
        )
        .is_err());
    }
}
