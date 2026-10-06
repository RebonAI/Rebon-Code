//! Windows backend — the `sandbox-win` helper. RFC §6.
//!
//! Windows has no single primitive that does what bubblewrap or
//! seatbelt do, so confinement is assembled from four unrelated
//! mechanisms — a Job Object for the process tree, a downgraded user
//! account for the security context, WFP filters keyed on that
//! account's SID for the network, and deny ACEs for the filesystem.
//! Three of those four need administrator rights to set up and none
//! of them can be applied from inside the process being confined.
//!
//! That is why this module builds an **argv for a helper binary**
//! rather than calling any API: `sandbox-win.exe` owns the privileged
//! half, and Rebon's half is deciding what to ask it for. Everything
//! here is therefore pure argv construction and validation — which is
//! also what makes it testable on a machine that has never seen
//! `sandbox-win.exe`.
//!
//! Two limits leak into the caller and cannot be hidden:
//!
//! * **ACLs are session-scoped.** They are set on real directories on
//!   a real disk, so a per-command read/write allowance is not
//!   expressible — RFC §6.4. Asking for one is an error, never a
//!   silently dropped rule. The one exception is a read allowance, once
//!   the helper advertises `allow-read`: it becomes a `--allow-read`
//!   grant that lives for the session, which is the honest reading of
//!   "per command" on a real disk.
//! * **The command line is finite.** `CreateProcessW` stops at 32767
//!   UTF-16 units, and a sandbox that truncates its own deny list
//!   would be worse than one that refuses — RFC §6.2.

use crate::runtime::config::EffectiveConfig;
use crate::runtime::env::EnvPlan;
use crate::runtime::error::{SandboxError, Warning};
use std::path::{Path, PathBuf};

pub const BACKEND: &str = "windows";

/// `CreateProcessW` accepts 32767 UTF-16 units. The refusal point is
/// lower because the OS re-quotes arguments that contain spaces or
/// quotes on the way through, and a command line that fits here can
/// still overflow there.
pub const MAX_COMMAND_LINE: usize = 30_000;

/// Variables the child keeps from the parent environment.
///
/// The sandbox user is a different account with a different profile,
/// so it does not inherit a usable environment on its own — but
/// copying the whole parent environment across would carry every
/// credential the agent process holds into the sandbox. These two are
/// the minimum that makes a shell able to find programs at all.
pub const PRESERVED_ENV_VARS: &[&str] = &["PATH", "PATHEXT"];

/// The `status` contract version this build of Rebon speaks.
///
/// Bumped together with the helper's `core::status::STATUS_VERSION`
/// (`crates/plugins/sandbox-win`). The two
/// halves ship separately — the helper is installed once and Rebon updates
/// through npm — so "these are the same version" is a runtime fact, not a
/// build-time one.
pub const SUPPORTED_STATUS_VERSION: u32 = 1;

/// The `features=` token for `exec --pipe-stdin`. Mirrors the helper's
/// `core::status::FEATURE_PIPE_STDIN`.
pub const FEATURE_PIPE_STDIN: &str = "pipe-stdin";
/// The `features=` token for `exec --allow-read`. Mirrors the helper's
/// `core::status::FEATURE_ALLOW_READ`.
pub const FEATURE_ALLOW_READ: &str = "allow-read";

/// The opt-in `exec` capabilities a helper advertised on its `status`
/// `features=` line.
///
/// Separate from [`SandboxWinStatus`] rather than two more fields on it: the
/// features are not part of readiness — a helper without them is a fully
/// working helper for one-shot commands — and the status struct's shape is
/// pinned by the callers that build it field by field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SandboxWinFeatures {
    /// `exec --pipe-stdin` is understood.
    pub supports_pipe_stdin: bool,
    /// `exec --allow-read <path>` is understood.
    pub supports_allow_read: bool,
}

/// Read the `features=` line out of `sandbox-win.exe status` stdout.
///
/// A helper that predates the line has none, which reads as "no features" —
/// the safe answer, because an older helper refuses the flags as unknown.
/// Tokens this build does not know are ignored; a token only counts when it
/// matches exactly.
pub fn parse_status_features(status_stdout: &str) -> SandboxWinFeatures {
    let tokens: Vec<&str> = status_stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("features="))
        .map(|value| value.split(',').map(str::trim).collect())
        .unwrap_or_default();
    SandboxWinFeatures {
        supports_pipe_stdin: tokens.contains(&FEATURE_PIPE_STDIN),
        supports_allow_read: tokens.contains(&FEATURE_ALLOW_READ),
    }
}

/// What a caller may ask of one `sandbox-win.exe exec` beyond the session's
/// rules.
///
/// `Default` asks for nothing and makes [`build_sandbox_win_argv_with`]
/// produce exactly what [`build_sandbox_win_argv`] always has — the one-shot
/// Bash / PowerShell shape, with stdin NUL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxWinExecOptions {
    /// Give the confined process a real stdin, relayed from the helper's own,
    /// instead of NUL (`--pipe-stdin`). For a long-lived host speaking NDJSON
    /// over stdin/stdout; the caller then writes to the helper's stdin.
    pub pipe_stdin: bool,
    /// Read roots for the confined accounts (`--allow-read`): read, execute
    /// and traverse, inherited by everything below, never write. Must be
    /// absolute and normalised — the helper refuses anything else.
    ///
    /// Like every ACE the helper places, a grant is owned by the calling
    /// process's session and stays until that session is reaped, not until the
    /// command exits; it applies to the sandbox group, so other confined
    /// commands running meanwhile can read the same roots.
    pub allow_read: Vec<PathBuf>,
    /// What the helper advertised, from [`parse_status_features`]. Asking for
    /// a capability it lacks is refused here rather than at spawn.
    pub features: SandboxWinFeatures,
}

/// What `sandbox-win.exe status` reports back.
///
/// All four must hold. They are separate fields rather than one
/// boolean because each has a different fix, and "sandbox not
/// available" with no reason is the least actionable message a user
/// can get.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SandboxWinStatus {
    /// `sandbox-win.exe` was found.
    pub binary_present: bool,
    /// The `version=` line from `status` — RFC §13 item 4.
    ///
    /// Without it the three booleans below cannot tell "the helper is too
    /// old" from "the helper is not set up", and the two need opposite
    /// advice. The old-helper case is the dangerous one: a helper that
    /// predates a rule silently ignores the flag carrying it, reports
    /// success, and runs the command with one fewer restriction than the
    /// caller believes is in force.
    pub version: Option<u32>,
    /// The downgraded sandbox user account exists.
    pub user_provisioned: bool,
    /// Its stored credentials are readable, so processes can be
    /// launched as that user without prompting.
    pub credentials_present: bool,
    /// The WFP sublayer and filters keyed on the sandbox user's SID
    /// are installed.
    pub wfp_installed: bool,
}

impl SandboxWinStatus {
    /// Whether the helper speaks a `status` contract this build understands.
    pub fn version_supported(&self) -> bool {
        self.version == Some(SUPPORTED_STATUS_VERSION)
    }

    /// Every piece is in place.
    pub fn is_ready(&self) -> bool {
        self.binary_present
            && self.version_supported()
            && self.user_provisioned
            && self.credentials_present
            && self.wfp_installed
    }

    /// The remediation lines for whatever is missing, in the order a
    /// user should act on them.
    ///
    /// The WFP line carries the reassurance deliberately: the natural
    /// reading of "installing a network filter" is "this will change
    /// my machine's networking", and a user who believes that will
    /// decline. The filters are keyed on the sandbox account's SID,
    /// so they cannot match traffic from the user's own session.
    pub fn remediation(&self) -> Vec<String> {
        let mut steps = Vec::new();
        if !self.binary_present {
            steps.push(
                "sandbox-win.exe was not found — install the Rebon sandbox helper, or set \
                 REBON_SANDBOX_WIN_PATH to its location."
                    .to_string(),
            );
            // Nothing else can be checked without the binary, and
            // listing three more failures the user cannot act on
            // until this one is fixed only obscures the first step.
            return steps;
        }
        if !self.version_supported() {
            // Also returned alone, and for a stronger reason than the one
            // above: a helper on the wrong contract may be answering the
            // other three probes about a different set of mechanisms. Acting
            // on those answers would be reading a stale report as a current
            // one.
            steps.push(match self.version {
                None => "sandbox-win.exe is older than this version of Rebon — it reports no \
                         status version, which means it will silently ignore sandbox rules \
                         it does not recognise. Reinstall the helper (`sandbox-win.exe install`)."
                    .to_string(),
                Some(found) => format!(
                    "sandbox-win.exe speaks status version {found}, this Rebon speaks \
                     {SUPPORTED_STATUS_VERSION} — reinstall the helper \
                     (`sandbox-win.exe install`) so the two agree on what the rules mean."
                ),
            });
            return steps;
        }
        if !self.user_provisioned || !self.credentials_present {
            steps.push(
                "The sandbox user account is not set up — run `sandbox-win.exe install` once \
                 (it will ask for administrator rights)."
                    .to_string(),
            );
        }
        if !self.wfp_installed {
            steps.push(
                "Network filters are not installed — run `sandbox-win.exe install` once. No sign-out \
                 is needed, and your own network is unaffected: the filters are keyed to the \
                 sandbox user's SID."
                    .to_string(),
            );
        }
        steps
    }
}

/// Build the `sandbox-win.exe exec` argv — RFC §6.2.
///
/// Shape:
///
/// ```text
/// sandbox-win.exe exec [--quiet]
///   [--deny-read  <path>]...
///   [--mask-file  <real> <fake>]...
///   [--deny-write <path>]...
///   [--allow-write <path>]...
///   [--block-network]
///   [--inherit-env NAME]...
///   [--env KEY=VALUE]...
///   [--unset-env KEY]...
///   [--cwd <path>]
///   -- <binShell> <shellArgs...> <command>
/// ```
///
/// The `--` matters: everything after it is the argv of the confined
/// process, handed through without interpretation. That is what makes
/// the PowerShell RFC §7.3 Windows row work — the whole
/// `pwsh -NoProfile -NonInteractive -EncodedCommand <base64>` sequence
/// arrives as argv, so no quoting layer ever sees the payload and
/// there is nothing for a `"` inside it to break.
pub fn build_sandbox_win_argv(
    config: &EffectiveConfig,
    env: &EnvPlan,
) -> Result<(String, Vec<String>, Vec<Warning>), SandboxError> {
    build_sandbox_win_argv_with(config, env, &SandboxWinExecOptions::default())
}

/// [`build_sandbox_win_argv`], plus the opt-in capabilities in `options`.
///
/// The two new flags only ever appear when asked for, in their frozen slots —
/// `--pipe-stdin` right after `--quiet`, `--allow-read` right after the last
/// `--allow-write` — so default options give a byte-identical argv.
///
/// Per-command read overrides (`allow_read_overrides`) stop being refused once
/// the helper advertises `allow-read`, and travel as `--allow-read` after the
/// roots in `options.allow_read`. Per-command write overrides are refused
/// whatever the helper says.
pub fn build_sandbox_win_argv_with(
    config: &EffectiveConfig,
    env: &EnvPlan,
    options: &SandboxWinExecOptions,
) -> Result<(String, Vec<String>, Vec<Warning>), SandboxError> {
    let features = options.features;
    // Refuse the inexpressible rules first, before any work: a caller
    // that gets an argv back must be able to trust that every rule it
    // asked for is in it.
    let refused_read = if features.supports_allow_read {
        None
    } else {
        config.allow_read_overrides.first()
    };
    if let Some(path) = refused_read.or_else(|| config.allow_write_overrides.first()) {
        return Err(SandboxError::PerExecAclUnsupported { path: path.clone() });
    }
    if options.pipe_stdin && !features.supports_pipe_stdin {
        return Err(helper_lacks(FEATURE_PIPE_STDIN));
    }
    if !options.allow_read.is_empty() && !features.supports_allow_read {
        return Err(helper_lacks(FEATURE_ALLOW_READ));
    }
    let mut read_roots: Vec<&PathBuf> = Vec::new();
    for root in options
        .allow_read
        .iter()
        .chain(&config.allow_read_overrides)
    {
        if !read_roots.contains(&root) {
            read_roots.push(root);
        }
    }

    let sandbox_win =
        config
            .runtime
            .sandbox_win_path
            .clone()
            .ok_or_else(|| SandboxError::MissingDependency {
                dependency: "sandbox-win",
                detail: "the Rebon sandbox helper (sandbox-win.exe) was not found; \
                     run `sandbox-win.exe install` or set REBON_SANDBOX_WIN_PATH"
                    .into(),
            })?;

    if config.bin_shell.program.is_empty() {
        return Err(SandboxError::ShellUnavailable {
            detail: "no shell was configured for the sandboxed command".into(),
        });
    }

    let mut warnings = Vec::new();
    let mut args = vec!["exec".to_string(), "--quiet".to_string()];
    if options.pipe_stdin {
        args.push("--pipe-stdin".into());
    }

    for path in &config.read_rules.deny_only {
        args.push("--deny-read".into());
        args.push(windows_path(path));
    }
    // A mask is a deny here for the same reason as on macOS: the ACL
    // layer can refuse an open, not answer it with different bytes.
    // Making one path return different contents to different processes
    // needs a filesystem filter driver, and RFC §1.2 rules that out —
    // it is another order of engineering and it needs WHQL signing.
    //
    // Both paths are still sent, so the helper's own stderr can name the
    // substitution that did not happen. The warning is the same
    // degradation macOS reports, and it is not optional: a caller that
    // believes the command saw a fake credential and a caller that knows
    // it saw a permission error debug two entirely different things.
    for bind in &config.masked_files {
        args.push("--mask-file".into());
        args.push(windows_path(&bind.real));
        args.push(windows_path(&bind.fake));
        // Two different things can have happened, and the caller debugs
        // them in two different places: either the fake is reachable through
        // an environment variable the tool honours, or the command simply
        // gets a permission error.
        let detail = match crate::runtime::env::mask_redirect_variable(bind) {
            Some(variable) => format!(
                "cannot fake the contents of {} — the rule became a read denial, and \
                 {variable} points at the fake file so a tool that honours it reads that \
                 instead",
                bind.real.display()
            ),
            None => format!(
                "cannot fake the contents of {} — the rule became a plain read denial",
                bind.real.display()
            ),
        };
        warnings.push(Warning::new(
            BACKEND,
            crate::runtime::error::warning_code::MASK_DOWNGRADED_TO_DENY,
            detail,
        ));
    }
    for path in &config.write_rules.deny_within_allow {
        args.push("--deny-write".into());
        args.push(windows_path(path));
    }

    let (write_roots, globs) = config.concrete_write_roots();
    for glob in globs {
        warnings.push(Warning::new(
            BACKEND,
            crate::runtime::error::warning_code::GLOB_WRITE_PATTERN,
            format!(
                "skipping glob write pattern {} — an ACL needs a concrete path",
                glob.display()
            ),
        ));
    }
    for root in &write_roots {
        args.push("--allow-write".into());
        args.push(windows_path(root));
    }
    for root in read_roots {
        args.push("--allow-read".into());
        args.push(windows_path(root));
    }

    if config.network_restricted {
        args.push("--block-network".into());
    }

    for var in PRESERVED_ENV_VARS {
        args.push("--inherit-env".into());
        args.push((*var).to_string());
    }
    for (key, value) in &env.set {
        args.push("--env".into());
        args.push(format!("{key}={value}"));
    }
    for key in &env.unset {
        args.push("--unset-env".into());
        args.push(key.clone());
    }

    if let Some(cwd) = &config.cwd {
        args.push("--cwd".into());
        args.push(windows_path(cwd));
    }

    args.push("--".into());
    args.extend(config.bin_shell.argv(&config.command));

    let length = command_line_length(&sandbox_win, &args);
    if length > MAX_COMMAND_LINE {
        return Err(SandboxError::ArgvTooLong {
            length,
            limit: MAX_COMMAND_LINE,
        });
    }

    Ok((sandbox_win.to_string_lossy().into_owned(), args, warnings))
}

/// The refusal for a capability the installed helper did not advertise.
///
/// An older helper would refuse the flag itself as unknown, so nothing would run
/// unconfined either way; refusing here names the remedy instead of leaving the
/// caller to decode the helper's stderr.
fn helper_lacks(feature: &str) -> SandboxError {
    SandboxError::MissingDependency {
        dependency: "sandbox-win",
        detail: format!(
            "the installed sandbox-win.exe does not support `{feature}` (it is not on its \
             `status` features line); reinstall the helper (`sandbox-win.exe install`)"
        ),
    }
}

/// The length `CreateProcessW` will see.
///
/// Counted in UTF-16 units, not bytes and not `char`s, because that
/// is the unit the limit is expressed in — a command full of CJK text
/// is half as long in UTF-16 as it is in UTF-8, and an emoji is two
/// units where `char` counts one. Each argument also costs a
/// separating space plus the two quotes the OS adds when it contains
/// one.
pub fn command_line_length(program: &Path, args: &[String]) -> usize {
    let mut length = program.to_string_lossy().encode_utf16().count() + 2;
    for arg in args {
        length += 1 + arg.encode_utf16().count();
        if arg.contains(' ') || arg.contains('"') {
            length += 2;
        }
    }
    length
}

/// Normalise a path for the helper.
///
/// Forward slashes are legal in most Windows APIs but not all — and
/// the ACL layer compares paths as strings when it looks up a
/// previously placed ACE, so a rule written with `/` and looked up
/// with `\` would not match its own entry.
fn windows_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

/// Extract the meaning out of `sandbox-win`'s stderr — RFC §7.4 step 8.
///
/// A sandbox failure surfaces to the model as whatever the confined
/// program printed, which for an ACL denial is usually a generic
/// `Access is denied` from deep inside a tool that has no idea a
/// sandbox exists. Recognising the helper's own markers lets the tool
/// result say which rule refused, instead of leaving the model to
/// guess at a permissions bug in the user's project.
pub fn extract_sandbox_error(stderr: &str) -> Option<String> {
    for line in stderr.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("sandbox-win: ") {
            return Some(rest.to_string());
        }
        if trimmed.contains("SANDBOX_WIN_DENIED") {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// The helper's file name.
pub const SANDBOX_WIN_BINARY: &str = "sandbox-win.exe";

/// Where to look for the helper, in order.
///
/// `executable` is the running `rebon.exe`, whose directory is checked
/// **first** among the product locations. An npm install puts the helper in
/// the platform package's `payload/`, right beside the binary, so an ordering
/// that started at `%ProgramFiles%` would leave an installed Rebon unable to
/// find its own helper — RFC §13 item 1. Same shape as
/// `rebon_boa_runner::helper_candidates_from_executable`, and for the same
/// reason it deliberately does **not** search `PATH`: accepting an unrelated
/// binary named `sandbox-win.exe` would weaken the product boundary, and here
/// that binary is the thing claiming to confine the user's commands.
///
/// `explicit` comes from `REBON_SANDBOX_WIN_PATH` and is only ever passed in
/// relaxed mode — see [`crate::runtime::session`] and RFC §11.3.
pub fn sandbox_win_candidates(explicit: Option<&str>, executable: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut push = |candidate: PathBuf| {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    };

    if let Some(explicit) = explicit {
        if !explicit.is_empty() {
            push(PathBuf::from(explicit));
        }
    }
    // The product locations beside the running executable, and the
    // empty-parent guard that keeps a bare `sandbox-win.exe` — which
    // `CreateProcess` would resolve off PATH — out of the list, are the shared
    // rule in `rebon_types::sibling_binary`. Only the machine-level
    // locations below are this helper's
    // own.
    if let Some(executable) = executable {
        for candidate in
            rebon_types::sibling_binary::candidates_for_file_name(SANDBOX_WIN_BINARY, executable)
        {
            push(candidate);
        }
    }
    if let Ok(program_files) = std::env::var("ProgramFiles") {
        push(
            PathBuf::from(program_files)
                .join("Rebon")
                .join(SANDBOX_WIN_BINARY),
        );
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        push(PathBuf::from(local).join("Rebon").join(SANDBOX_WIN_BINARY));
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::config::{
        BinShell, CommandRequest, CredentialFileRule, EffectiveConfig, SessionSandboxConfig,
    };
    use crate::runtime::env::build_env_plan;

    fn effective(
        mutate: impl FnOnce(&mut SessionSandboxConfig),
        request: CommandRequest,
    ) -> EffectiveConfig {
        let mut session = SessionSandboxConfig::default();
        session.runtime.sandbox_win_path = Some(PathBuf::from(r"C:\Rebon\sandbox-win.exe"));
        mutate(&mut session);
        EffectiveConfig::merge(&session, &request)
    }

    fn request() -> CommandRequest {
        CommandRequest::new("echo hi", BinShell::new("bash.exe", ["-c"]))
    }

    /// A path built with the host's own separator.
    ///
    /// `C:\npm\payload\rebon.exe` is a *single component* anywhere but
    /// Windows — nothing in it separates anything — so `parent()` yields an
    /// empty path and the ranking these tests are about cannot be observed
    /// at all. The ranking has no platform in it, so the fixtures are built
    /// natively and the tests run everywhere instead of passing on one OS
    /// and failing on the two the code is developed on.
    fn native(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    fn build(config: &EffectiveConfig) -> (String, Vec<String>, Vec<Warning>) {
        let env = build_env_plan(config);
        build_sandbox_win_argv(config, &env).unwrap()
    }

    #[test]
    fn argv_starts_with_exec_and_ends_with_the_shell_argv() {
        let (program, args, _) = build(&effective(|_| {}, request()));

        assert_eq!(program, r"C:\Rebon\sandbox-win.exe");
        assert_eq!(args[0], "exec");
        let separator = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(&args[separator + 1..], &["bash.exe", "-c", "echo hi"]);
    }

    #[test]
    fn encoded_command_shell_survives_as_argv() {
        let config = effective(
            |_| {},
            CommandRequest::new(
                "ZQBjAGgAbwA=",
                BinShell::new(
                    "pwsh.exe",
                    ["-NoProfile", "-NonInteractive", "-EncodedCommand"],
                ),
            ),
        );

        let (_, args, _) = build(&config);
        let separator = args.iter().position(|a| a == "--").unwrap();

        assert_eq!(
            &args[separator + 1..],
            &[
                "pwsh.exe",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                "ZQBjAGgAbwA="
            ]
        );
    }

    #[test]
    fn per_exec_allow_read_is_refused_before_anything_else() {
        let mut req = request();
        req.allow_read = vec![PathBuf::from(r"C:\secret")];
        // No sandbox-win path either: the ACL refusal must win, because
        // it is the one the caller can actually act on.
        let mut session = SessionSandboxConfig::default();
        session.filesystem.allow_write = vec![PathBuf::from(r"C:\work")];
        let config = EffectiveConfig::merge(&session, &req);
        let env = build_env_plan(&config);

        assert!(matches!(
            build_sandbox_win_argv(&config, &env),
            Err(SandboxError::PerExecAclUnsupported { .. })
        ));
    }

    #[test]
    fn per_exec_allow_write_is_refused_too() {
        let mut req = request();
        req.allow_write = vec![PathBuf::from(r"C:\other")];
        let config = effective(|_| {}, req);
        let env = build_env_plan(&config);

        assert!(matches!(
            build_sandbox_win_argv(&config, &env),
            Err(SandboxError::PerExecAclUnsupported { .. })
        ));
    }

    #[test]
    fn missing_helper_is_an_error_not_a_passthrough() {
        let mut session = SessionSandboxConfig::default();
        session.filesystem.deny_read = vec![PathBuf::from(r"C:\secret")];
        let config = EffectiveConfig::merge(&session, &request());
        let env = build_env_plan(&config);

        assert!(matches!(
            build_sandbox_win_argv(&config, &env),
            Err(SandboxError::MissingDependency {
                dependency: "sandbox-win",
                ..
            })
        ));
    }

    #[test]
    fn deny_rules_reach_the_helper_with_backslash_paths() {
        let config = effective(
            |session| {
                session.filesystem.deny_read = vec![PathBuf::from("C:/secret")];
                session.filesystem.allow_write = vec![PathBuf::from("C:/work")];
                session.filesystem.deny_write = vec![PathBuf::from("C:/work/vendor")];
            },
            request(),
        );

        let (_, args, _) = build(&config);
        let joined = args.join(" ");

        assert!(joined.contains(r"--deny-read C:\secret"));
        assert!(joined.contains(r"--allow-write C:\work"));
        assert!(joined.contains(r"--deny-write C:\work\vendor"));
    }

    #[test]
    fn credential_mask_passes_both_paths_to_the_helper() {
        let config = effective(
            |session| {
                session.credentials.files = vec![(
                    PathBuf::from(r"C:\Users\u\.npmrc"),
                    CredentialFileRule::Mask {
                        fake: PathBuf::from(r"C:\tmp\fake"),
                    },
                )];
            },
            request(),
        );

        let (_, args, _) = build(&config);
        let index = args.iter().position(|a| a == "--mask-file").unwrap();

        assert_eq!(args[index + 1], r"C:\Users\u\.npmrc");
        assert_eq!(args[index + 2], r"C:\tmp\fake");
    }

    #[test]
    fn network_restriction_becomes_a_flag() {
        let unrestricted = build(&effective(|_| {}, request()));
        assert!(!unrestricted.1.contains(&"--block-network".to_string()));

        let restricted = build(&effective(|_| {}, request().with_network_restriction(true)));
        assert!(restricted.1.contains(&"--block-network".to_string()));
    }

    #[test]
    fn path_and_pathext_are_the_only_inherited_variables() {
        let (_, args, _) = build(&effective(|_| {}, request()));
        let inherited: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(index, _)| *index > 0 && args[index - 1] == "--inherit-env")
            .map(|(_, value)| value)
            .collect();

        assert_eq!(inherited, vec!["PATH", "PATHEXT"]);
    }

    #[test]
    fn proxy_bypass_variables_are_unset_under_restriction() {
        let (_, args, _) = build(&effective(|_| {}, request().with_network_restriction(true)));
        let joined = args.join(" ");

        assert!(joined.contains("--unset-env no_proxy"));
        assert!(joined.contains("--unset-env NO_PROXY"));
    }

    #[test]
    fn git_safe_directories_are_passed_as_env_pairs() {
        let config = effective(
            |_| {},
            request().with_git_safe_directories([PathBuf::from(r"C:\work")]),
        );

        let (_, args, _) = build(&config);
        let joined = args.join(" ");

        assert!(joined.contains("--env GIT_CONFIG_COUNT=1"));
        assert!(joined.contains("--env GIT_CONFIG_KEY_0=safe.directory"));
        assert!(joined.contains(r"--env GIT_CONFIG_VALUE_0=C:\work"));
    }

    #[test]
    fn cwd_is_passed_through() {
        let config = effective(|_| {}, request().with_cwd(r"C:\work"));
        let (_, args, _) = build(&config);
        let index = args.iter().position(|a| a == "--cwd").unwrap();
        assert_eq!(args[index + 1], r"C:\work");
    }

    #[test]
    fn an_over_long_command_line_is_refused() {
        let long = "x".repeat(MAX_COMMAND_LINE + 1);
        let config = effective(
            |_| {},
            CommandRequest::new(long, BinShell::new("bash.exe", ["-c"])),
        );
        let env = build_env_plan(&config);

        match build_sandbox_win_argv(&config, &env) {
            Err(SandboxError::ArgvTooLong { length, limit }) => {
                assert!(length > limit);
                assert_eq!(limit, MAX_COMMAND_LINE);
            }
            other => panic!("expected ArgvTooLong, got {other:?}"),
        }
    }

    #[test]
    fn a_command_just_under_the_limit_is_accepted() {
        let config = effective(
            |_| {},
            CommandRequest::new("x".repeat(1000), BinShell::new("bash.exe", ["-c"])),
        );
        let env = build_env_plan(&config);
        assert!(build_sandbox_win_argv(&config, &env).is_ok());
    }

    #[test]
    fn command_line_length_counts_utf16_units_not_chars() {
        // An astral-plane character is one `char` and two UTF-16
        // units; counting `char`s would under-report by half here.
        let args = vec!["\u{1F600}".to_string()];
        let length = command_line_length(Path::new("a"), &args);
        assert_eq!(length, 1 + 2 + 1 + 2);
    }

    #[test]
    fn command_line_length_charges_for_quoting() {
        let plain = command_line_length(Path::new("a"), &["bc".to_string()]);
        let spaced = command_line_length(Path::new("a"), &["b c".to_string()]);
        assert_eq!(spaced, plain + 1 + 2);
    }

    #[test]
    fn glob_write_root_warns_and_is_dropped() {
        let config = effective(
            |session| session.filesystem.allow_write = vec![PathBuf::from(r"C:\work\*\build")],
            request(),
        );

        let (_, args, warnings) = build(&config);

        assert_eq!(warnings.len(), 1);
        assert!(!args.iter().any(|a| a.contains('*')));
    }

    fn ready_status() -> SandboxWinStatus {
        SandboxWinStatus {
            binary_present: true,
            version: Some(SUPPORTED_STATUS_VERSION),
            user_provisioned: true,
            credentials_present: true,
            wfp_installed: true,
        }
    }

    #[test]
    fn status_is_ready_only_when_every_piece_is_present() {
        let ready = ready_status();
        assert!(ready.is_ready());
        assert!(ready.remediation().is_empty());

        for missing in 0..5 {
            let mut status = ready;
            match missing {
                0 => status.binary_present = false,
                1 => status.version = None,
                2 => status.user_provisioned = false,
                3 => status.credentials_present = false,
                _ => status.wfp_installed = false,
            }
            assert!(!status.is_ready(), "case {missing}");
            assert!(!status.remediation().is_empty(), "case {missing}");
        }
    }

    #[test]
    fn a_helper_with_no_version_line_is_reported_as_too_old() {
        // RFC §13 item 4. This is the dangerous state, not merely an
        // unconfigured one: an older helper ignores flags it does not
        // recognise and exits zero, so the caller believes rules are in force
        // that the helper never applied.
        let status = SandboxWinStatus {
            version: None,
            ..ready_status()
        };
        let steps = status.remediation();

        assert!(!status.is_ready());
        assert_eq!(steps.len(), 1, "no other step is actionable first");
        assert!(
            steps[0].contains("older than this version of Rebon"),
            "{steps:?}"
        );
        assert!(steps[0].contains("silently ignore"), "{steps:?}");
    }

    #[test]
    fn a_helper_on_a_different_contract_version_names_both_numbers() {
        let status = SandboxWinStatus {
            version: Some(99),
            ..ready_status()
        };
        let steps = status.remediation();

        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("version 99"), "{steps:?}");
        assert!(
            steps[0].contains(&SUPPORTED_STATUS_VERSION.to_string()),
            "{steps:?}"
        );
    }

    #[test]
    fn a_version_problem_outranks_the_setup_steps() {
        // The other three probes come from a helper that may mean something
        // different by them. Reporting "run install to create the account"
        // when the real problem is a mismatched contract sends the user to
        // fix something that is not broken.
        let status = SandboxWinStatus {
            binary_present: true,
            version: None,
            user_provisioned: false,
            credentials_present: false,
            wfp_installed: false,
        };
        let steps = status.remediation();

        assert_eq!(steps.len(), 1);
        assert!(!steps[0].contains("Network filters"), "{steps:?}");
    }

    #[test]
    fn a_missing_binary_outranks_even_the_version_step() {
        let steps = SandboxWinStatus::default().remediation();
        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("was not found"));
    }

    #[test]
    fn a_missing_binary_reports_only_the_step_that_unblocks_the_rest() {
        let status = SandboxWinStatus::default();
        let steps = status.remediation();
        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("sandbox-win.exe was not found"));
    }

    #[test]
    fn the_wfp_step_says_the_users_own_network_is_untouched() {
        let status = SandboxWinStatus {
            wfp_installed: false,
            ..ready_status()
        };
        let steps = status.remediation();
        assert_eq!(steps.len(), 1);
        assert!(steps[0].contains("your own network is unaffected"));
        assert!(steps[0].contains("No sign-out is needed"));
    }

    #[test]
    fn helper_stderr_markers_are_recognised() {
        assert_eq!(
            extract_sandbox_error("sandbox-win: write denied: C:\\secret\n"),
            Some("write denied: C:\\secret".to_string())
        );
        assert_eq!(
            extract_sandbox_error("blah\n  SANDBOX_WIN_DENIED network\n"),
            Some("SANDBOX_WIN_DENIED network".to_string())
        );
        assert_eq!(extract_sandbox_error("error: file not found"), None);
    }

    #[test]
    fn explicit_candidate_comes_first() {
        let candidates = sandbox_win_candidates(Some(r"D:\build\sandbox-win.exe"), None);
        assert_eq!(candidates[0], PathBuf::from(r"D:\build\sandbox-win.exe"));
    }

    #[test]
    fn an_empty_explicit_override_is_ignored() {
        let candidates = sandbox_win_candidates(Some(""), None);
        assert!(!candidates.iter().any(|c| c.as_os_str().is_empty()));
    }

    #[test]
    fn the_directory_beside_the_executable_is_searched() {
        // RFC §13 item 1: an npm install puts the helper in the platform
        // package's `payload/`, right next to `rebon.exe`. Without this the
        // one installation path most users take cannot find its own helper.
        let executable = native(&[
            "Users",
            "u",
            "AppData",
            "Roaming",
            "npm",
            "payload",
            "rebon.exe",
        ]);
        let candidates = sandbox_win_candidates(None, Some(&executable));
        assert_eq!(
            candidates[0],
            native(&[
                "Users",
                "u",
                "AppData",
                "Roaming",
                "npm",
                "payload",
                "sandbox-win.exe"
            ])
        );
    }

    #[test]
    fn the_executables_directory_outranks_program_files() {
        // A machine can have both a system-wide install and an npm one. The
        // helper that ships with the running binary is the one that matches
        // its contract version, so it has to win.
        let executable = native(&["npm", "payload", "rebon.exe"]);
        let candidates = sandbox_win_candidates(None, Some(&executable));
        let beside = candidates
            .iter()
            .position(|c| c == &native(&["npm", "payload", "sandbox-win.exe"]))
            .expect("the executable's directory must be a candidate");
        for (index, candidate) in candidates.iter().enumerate() {
            if candidate.to_string_lossy().contains(r"\Rebon\") {
                assert!(index > beside, "{candidate:?} outranked the bundled helper");
            }
        }
    }

    #[test]
    fn an_explicit_override_still_outranks_the_bundled_helper() {
        // The override is taken verbatim, so it stays a Windows literal; the
        // bundled helper is derived from a parent directory, so it does not.
        let executable = native(&["npm", "payload", "rebon.exe"]);
        let candidates =
            sandbox_win_candidates(Some(r"D:\build\sandbox-win.exe"), Some(&executable));
        assert_eq!(candidates[0], PathBuf::from(r"D:\build\sandbox-win.exe"));
        assert_eq!(
            candidates[1],
            native(&["npm", "payload", "sandbox-win.exe"])
        );
    }

    #[test]
    fn the_same_location_is_never_probed_twice() {
        let candidates = sandbox_win_candidates(
            Some(r"D:\npm\payload\sandbox-win.exe"),
            Some(Path::new(r"D:\npm\payload\rebon.exe")),
        );
        assert_eq!(
            candidates
                .iter()
                .filter(|c| *c == Path::new(r"D:\npm\payload\sandbox-win.exe"))
                .count(),
            1
        );
    }

    #[test]
    fn path_is_deliberately_not_searched() {
        // Same rule as `rebon_boa_runner::helper_candidates_from_executable`,
        // and it matters more here: accepting whatever `sandbox-win.exe` happens
        // to be on PATH means accepting an arbitrary binary's claim to be
        // confining the user's commands.
        let executable = native(&["npm", "payload", "rebon.exe"]);
        let candidates = sandbox_win_candidates(None, Some(&executable));
        assert!(!candidates.contains(&PathBuf::from(SANDBOX_WIN_BINARY)));

        // The case that produces a bare name: an executable path with no
        // directory in it at all. Without the empty-parent guard this is
        // exactly `sandbox-win.exe`, and PATH decides what confines the
        // user's commands.
        let bare = sandbox_win_candidates(None, Some(Path::new("rebon.exe")));
        assert!(!bare.contains(&PathBuf::from(SANDBOX_WIN_BINARY)));
    }

    fn every_feature() -> SandboxWinFeatures {
        SandboxWinFeatures {
            supports_pipe_stdin: true,
            supports_allow_read: true,
        }
    }

    /// A session with every rule kind the one-shot builder emits.
    fn populated(request: CommandRequest) -> EffectiveConfig {
        effective(
            |session| {
                session.filesystem.deny_read = vec![PathBuf::from(r"C:\secret")];
                session.filesystem.allow_write = vec![PathBuf::from(r"C:\work")];
                session.filesystem.deny_write = vec![PathBuf::from(r"C:\work\vendor")];
                session.credentials.files = vec![(
                    PathBuf::from(r"C:\Users\u\.npmrc"),
                    CredentialFileRule::Mask {
                        fake: PathBuf::from(r"C:\tmp\fake"),
                    },
                )];
            },
            request.with_cwd(r"C:\work"),
        )
    }

    fn build_with(
        config: &EffectiveConfig,
        options: &SandboxWinExecOptions,
    ) -> Result<(String, Vec<String>, Vec<Warning>), SandboxError> {
        build_sandbox_win_argv_with(config, &build_env_plan(config), options)
    }

    #[test]
    fn the_one_shot_argv_is_pinned_byte_for_byte() {
        // The shape Bash / PowerShell commands have always produced. Default options
        // must not move a single argument.
        let config = populated(request());
        let (program, args, _) = build(&config);

        assert_eq!(program, r"C:\Rebon\sandbox-win.exe");
        assert_eq!(args, GOLDEN_ONE_SHOT_ARGV);
        let (_, with_defaults, _) = build_with(&config, &Default::default()).unwrap();
        assert_eq!(with_defaults, GOLDEN_ONE_SHOT_ARGV);
    }

    /// What the builder produced for [`populated`] before `--pipe-stdin` and
    /// `--allow-read` existed. `\etc\ssh\ssh_config.d` is the constant
    /// `ALWAYS_DENY_READ` entry every restricted command carries.
    const GOLDEN_ONE_SHOT_ARGV: &[&str] = &[
        "exec",
        "--quiet",
        "--deny-read",
        r"C:\secret",
        "--deny-read",
        r"\etc\ssh\ssh_config.d",
        "--mask-file",
        r"C:\Users\u\.npmrc",
        r"C:\tmp\fake",
        "--deny-write",
        r"C:\work\vendor",
        "--allow-write",
        r"C:\work",
        "--inherit-env",
        "PATH",
        "--inherit-env",
        "PATHEXT",
        "--cwd",
        r"C:\work",
        "--",
        "bash.exe",
        "-c",
        "echo hi",
    ];

    #[test]
    fn default_options_match_the_plain_builder_for_every_shape() {
        for config in [
            effective(|_| {}, request()),
            populated(request()),
            populated(request().with_network_restriction(true)),
            populated(request().with_git_safe_directories([PathBuf::from(r"C:\work")])),
        ] {
            let plain = build(&config);
            let defaulted = build_with(&config, &SandboxWinExecOptions::default()).unwrap();
            assert_eq!(plain, defaulted);
            assert!(!plain
                .1
                .iter()
                .any(|a| a == "--pipe-stdin" || a == "--allow-read"));
        }
    }

    #[test]
    fn advertised_features_alone_change_nothing() {
        // A new helper running an old-shaped command: nothing asked for, nothing sent.
        let config = populated(request());
        let options = SandboxWinExecOptions {
            features: every_feature(),
            ..Default::default()
        };
        assert_eq!(build_with(&config, &options).unwrap(), build(&config));
    }

    #[test]
    fn pipe_stdin_goes_right_after_quiet_and_moves_nothing_else() {
        let config = populated(request());
        let options = SandboxWinExecOptions {
            pipe_stdin: true,
            features: every_feature(),
            ..Default::default()
        };

        let (_, args, _) = build_with(&config, &options).unwrap();
        let (_, plain, _) = build(&config);

        assert_eq!(&args[..3], &["exec", "--quiet", "--pipe-stdin"]);
        let mut without: Vec<String> = args.clone();
        without.remove(2);
        assert_eq!(without, plain);
    }

    #[test]
    fn pipe_stdin_is_refused_when_the_helper_does_not_advertise_it() {
        let config = populated(request());
        let options = SandboxWinExecOptions {
            pipe_stdin: true,
            features: SandboxWinFeatures {
                supports_pipe_stdin: false,
                supports_allow_read: true,
            },
            ..Default::default()
        };

        match build_with(&config, &options) {
            Err(SandboxError::MissingDependency { dependency, detail }) => {
                assert_eq!(dependency, "sandbox-win");
                assert!(detail.contains("pipe-stdin"), "{detail}");
            }
            other => panic!("expected MissingDependency, got {other:?}"),
        }
    }

    #[test]
    fn read_roots_follow_the_write_roots_with_backslashes() {
        let config = populated(request());
        let options = SandboxWinExecOptions {
            allow_read: vec![
                PathBuf::from("C:/Users/u/AppData/Roaming/rebon/plugins"),
                PathBuf::from(r"C:\Program Files\nodejs"),
            ],
            features: every_feature(),
            ..Default::default()
        };

        let (_, args, _) = build_with(&config, &options).unwrap();
        let (_, plain, _) = build(&config);

        let last_write = plain.iter().rposition(|a| a == "--allow-write").unwrap();
        let inserted = &args[last_write + 2..last_write + 6];
        assert_eq!(
            inserted,
            &[
                "--allow-read",
                r"C:\Users\u\AppData\Roaming\rebon\plugins",
                "--allow-read",
                r"C:\Program Files\nodejs",
            ]
        );
        let mut without = args.clone();
        without.drain(last_write + 2..last_write + 6);
        assert_eq!(without, plain);
    }

    #[test]
    fn read_roots_are_refused_when_the_helper_does_not_advertise_them() {
        let config = populated(request());
        let options = SandboxWinExecOptions {
            allow_read: vec![PathBuf::from(r"C:\plugins")],
            features: SandboxWinFeatures {
                supports_pipe_stdin: true,
                supports_allow_read: false,
            },
            ..Default::default()
        };

        match build_with(&config, &options) {
            Err(SandboxError::MissingDependency { detail, .. }) => {
                assert!(detail.contains("allow-read"), "{detail}");
            }
            other => panic!("expected MissingDependency, got {other:?}"),
        }
    }

    #[test]
    fn per_exec_allow_read_becomes_a_read_root_once_the_helper_supports_it() {
        let mut req = request();
        req.allow_read = vec![PathBuf::from(r"C:\data"), PathBuf::from(r"C:\plugins")];
        let config = effective(|_| {}, req);
        let options = SandboxWinExecOptions {
            allow_read: vec![PathBuf::from(r"C:\plugins")],
            features: every_feature(),
            ..Default::default()
        };

        let (_, args, _) = build_with(&config, &options).unwrap();
        let roots: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(index, _)| *index > 0 && args[index - 1] == "--allow-read")
            .map(|(_, value)| value)
            .collect();

        // The explicit roots first, then the overrides, each path once.
        assert_eq!(roots, vec![r"C:\plugins", r"C:\data"]);
    }

    #[test]
    fn per_exec_allow_read_is_still_refused_by_a_helper_without_the_feature() {
        let mut req = request();
        req.allow_read = vec![PathBuf::from(r"C:\data")];
        let config = effective(|_| {}, req);
        let options = SandboxWinExecOptions {
            features: SandboxWinFeatures {
                supports_pipe_stdin: true,
                supports_allow_read: false,
            },
            ..Default::default()
        };

        assert!(matches!(
            build_with(&config, &options),
            Err(SandboxError::PerExecAclUnsupported { path }) if path == Path::new(r"C:\data")
        ));
    }

    #[test]
    fn per_exec_allow_write_is_refused_whatever_the_helper_advertises() {
        let mut req = request();
        req.allow_write = vec![PathBuf::from(r"C:\other")];
        let config = effective(|_| {}, req);
        let options = SandboxWinExecOptions {
            pipe_stdin: true,
            allow_read: vec![PathBuf::from(r"C:\plugins")],
            features: every_feature(),
        };

        assert!(matches!(
            build_with(&config, &options),
            Err(SandboxError::PerExecAclUnsupported { path }) if path == Path::new(r"C:\other")
        ));
    }

    #[test]
    fn both_capabilities_together_keep_the_frozen_order() {
        let config = populated(request().with_network_restriction(true));
        let options = SandboxWinExecOptions {
            pipe_stdin: true,
            allow_read: vec![PathBuf::from(r"C:\plugins")],
            features: every_feature(),
        };

        let (_, args, _) = build_with(&config, &options).unwrap();
        let position = |flag: &str| args.iter().position(|a| a == flag).unwrap();

        assert!(position("--quiet") < position("--pipe-stdin"));
        assert!(position("--pipe-stdin") < position("--deny-read"));
        assert!(position("--allow-write") < position("--allow-read"));
        assert!(position("--allow-read") < position("--block-network"));
        assert!(position("--allow-read") < position("--"));
    }

    #[test]
    fn the_features_line_is_read_when_present() {
        let text = "version=1\nuser=ok\ncredentials=ok\nwfp=ok\nfeatures=pipe-stdin,allow-read\n";
        assert_eq!(parse_status_features(text), every_feature());
    }

    #[test]
    fn a_helper_without_a_features_line_has_no_features() {
        // Every helper shipped before this line existed.
        let text = "version=1\nuser=ok\ncredentials=ok\nwfp=ok\n";
        assert_eq!(parse_status_features(text), SandboxWinFeatures::default());
        assert_eq!(parse_status_features(""), SandboxWinFeatures::default());
    }

    #[test]
    fn unknown_feature_tokens_are_ignored_and_only_exact_tokens_count() {
        let text = "version=1\r\nfeatures= time-travel , allow-read,pipe-stdin-v2 \r\n# note\n";
        assert_eq!(
            parse_status_features(text),
            SandboxWinFeatures {
                supports_pipe_stdin: false,
                supports_allow_read: true,
            }
        );
        assert_eq!(
            parse_status_features("features=\n"),
            SandboxWinFeatures::default()
        );
        // A diagnostic that merely mentions the word is not the line.
        assert_eq!(
            parse_status_features("# features=pipe-stdin is new\n"),
            SandboxWinFeatures::default()
        );
    }

    #[test]
    fn the_features_line_does_not_disturb_the_status_contract() {
        // The existing probes are `contains` tests and a `version=` line; the new line
        // must not light up any of them.
        let text = "version=1\nuser=missing\ncredentials=missing\nwfp=missing\nfeatures=pipe-stdin,allow-read\n";
        for probe in ["user=ok", "credentials=ok", "wfp=ok"] {
            assert!(!text.contains(probe));
        }
        let version: Option<u32> = text.lines().find_map(|line| {
            line.trim()
                .strip_prefix("version=")
                .and_then(|value| value.trim().parse().ok())
        });
        assert_eq!(version, Some(SUPPORTED_STATUS_VERSION));
    }

    #[test]
    fn a_mask_rule_warns_that_the_contents_are_not_faked() {
        // RFC §13 item 3, §6.4. macOS already reports this degradation;
        // Windows reporting it silently as success would tell the caller the
        // command read a fake credential when it read a permission error.
        let config = effective(
            |session| {
                session.credentials.files = vec![(
                    PathBuf::from(r"C:\Users\u\.npmrc"),
                    CredentialFileRule::Mask {
                        fake: PathBuf::from(r"C:\tmp\fake"),
                    },
                )];
            },
            request(),
        );

        let (_, args, warnings) = build(&config);

        assert!(args.iter().any(|a| a == "--mask-file"));
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].code,
            crate::runtime::error::warning_code::MASK_DOWNGRADED_TO_DENY
        );
        assert_eq!(warnings[0].backend, BACKEND);
        assert!(warnings[0].detail.contains(r"C:\Users\u\.npmrc"));
    }

    #[test]
    fn masking_warns_once_per_rule() {
        let config = effective(
            |session| {
                session.credentials.files = vec![
                    (
                        PathBuf::from(r"C:\a"),
                        CredentialFileRule::Mask {
                            fake: PathBuf::from(r"C:\fake-a"),
                        },
                    ),
                    (
                        PathBuf::from(r"C:\b"),
                        CredentialFileRule::Mask {
                            fake: PathBuf::from(r"C:\fake-b"),
                        },
                    ),
                ];
            },
            request(),
        );

        let (_, _, warnings) = build(&config);

        assert_eq!(warnings.len(), 2);
    }
}
