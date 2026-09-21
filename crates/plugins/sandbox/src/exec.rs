//! Where a shell command meets the sandbox.
//!
//! `Bash` and `PowerShell` both end up here, and they end up here for
//! the same reason: whether a command is confined is a property of
//! *running a command on this machine*, not of which tool asked. The
//! two tools differ in how they hand their payload to a shell —
//! `sh -lc <script>` versus `pwsh … -EncodedCommand <base64>` — and
//! that difference is expressed as a
//! [`BinShell`](rebon_tool::command_sandbox::BinShell) value, not as
//! a second code path.
//!
//! ## The decision, in order
//!
//! ```text
//! policy disabled / platform has no backend → run unwrapped
//! command is on excludedCommands            → run unwrapped
//! dangerouslyDisableSandbox + Open override → run unwrapped
//! dangerouslyDisableSandbox + Closed override → refuse
//! otherwise                                 → wrap
//! ```
//!
//! The refusal on the fourth line is the point of the whole table: a
//! workspace whose policy says every command is confined must not
//! have that undone by a flag the model can set on its own tool call.
//!
//! The row above the table — *no policy at all* — is not here any more. A
//! session with the sandbox off has no `CommandSandbox` on its tool context,
//! so `Bash` builds its own passthrough and this module is never reached.
//! That is the same code path a build without this plugin takes.
//!
//! ## What "wrap" can still return
//!
//! A wrap does not necessarily produce a confined command. When the
//! session has no rules that apply to this command, the runtime takes
//! its fast path and hands back the original argv — the same process
//! that would have run with the sandbox switched off. That is why the
//! result carries
//! [`PreparedCommand::confined`](rebon_tool::command_sandbox::PreparedCommand::confined)
//! rather than a bare argv: a caller that needs to know whether the OS is
//! actually enforcing anything can ask, instead of assuming from the fact
//! that a policy exists.

use std::path::PathBuf;
use std::sync::Arc;

use rebon_tool::command_sandbox::{BinShell, CommandSandbox, PreparedCommand};
use rebon_tools_core::{ToolError, ToolId, ToolResult};

use crate::runtime::{
    CommandRequest, FsProbe, RealFs, SandboxError, SandboxRuntime, WrappedCommand,
};
use crate::view::{OverrideMode, SandboxPlatform};

/// Sandbox policy that governs how command-execution tools (Bash,
/// PowerShell) decide whether to run a command inside / outside the
/// OS-level sandbox.
///
/// Built once per session from `settings.json` and injected into the tool
/// context as the session's [`CommandSandbox`]. When nothing injects one,
/// tools behave as if sandboxing does not exist.
///
/// ## Decision rules
///
/// 1. Sandbox disabled or platform unsupported → run normally.
/// 2. Command is in `excluded_commands` → bypass sandbox.
/// 3. `dangerouslyDisableSandbox` is `true`:
///    - **Open** mode → run without sandbox (user accepted the risk).
///    - **Closed** mode → **deny** — strict mode forbids unsandboxed
///      execution.
/// 4. Otherwise → run inside sandbox: [`SandboxPolicy::prepare`] hands the
///    command to [`crate::runtime`], which wraps it with bubblewrap,
///    seatbelt, or `sandbox-win` depending on platform.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Whether sandboxing is enabled in settings.
    pub enabled: bool,
    /// Current platform.
    pub platform: SandboxPlatform,
    /// Override mode: `Open` allows unsandboxed fallback, `Closed`
    /// requires all commands to run inside the sandbox.
    pub override_mode: OverrideMode,
    /// Commands that bypass the sandbox entirely (e.g. `git`, `npm`).
    pub excluded_commands: Vec<String>,
    /// The compiled session sandbox. `None` means nothing was wired,
    /// which for an otherwise-active policy is a refusal rather than
    /// a passthrough — see [`SandboxPolicy::prepare`].
    pub runtime: Option<Arc<SandboxRuntime>>,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            platform: crate::runtime::current_platform(),
            override_mode: OverrideMode::Closed,
            excluded_commands: Vec::new(),
            runtime: None,
        }
    }
}

impl SandboxPolicy {
    /// Adopt a session sandbox built by [`crate::runtime::session`].
    ///
    /// The conversion is the single point where the runtime's
    /// vocabulary meets the tool layer's, which is why it is a
    /// function rather than a struct literal at each of the three
    /// executor call sites (TUI, ACP, headless harness). A literal
    /// per site is how one of them ends up with a field left at its
    /// default and a session that is quietly unsandboxed.
    pub fn from_session(session: crate::runtime::SessionSandbox) -> Self {
        Self {
            enabled: session.enabled,
            platform: session.platform,
            override_mode: session.override_mode,
            excluded_commands: session.excluded_commands,
            runtime: session.runtime,
        }
    }

    /// Returns `true` when sandbox enforcement is active: enabled AND
    /// this platform has a backend that can enforce it.
    ///
    /// The platform half is [`crate::runtime::has_backend`], **not**
    /// [`SandboxPlatform::is_supported`]. The two disagree
    /// about Windows and both are right about their own question:
    /// `is_supported` answers "does the `/sandbox` UI offer this
    /// platform" and is pinned to macOS-or-Linux, while this answers
    /// "can a command be wrapped here", which Windows can via
    /// `sandbox-win`. Using the UI predicate for enforcement would mean a
    /// Windows workspace with `sandbox.enabled` silently ran every
    /// command unconfined — and, worse, would accept
    /// `dangerouslyDisableSandbox` in a policy that forbids it.
    ///
    /// Whether the machine can *actually* confine anything is a
    /// separate, later question, answered by the runtime itself.
    pub fn is_active(&self) -> bool {
        self.enabled && crate::runtime::has_backend(self.platform)
    }

    /// Returns `true` when `command`'s executable (first whitespace-
    /// delimited token) exactly matches any entry in
    /// `excluded_commands`. For example, excluded `"git"` matches
    /// `"git status"` but NOT `"gitignore-gen"`.
    pub fn is_command_excluded(&self, command: &str) -> bool {
        let executable = command.trim().split_whitespace().next().unwrap_or("");
        self.excluded_commands
            .iter()
            .any(|exc| executable == exc.as_str())
    }
}

/// What the decision table says about one command, before any argv exists.
enum Verdict {
    /// Spawn what the tool built. Nothing is confined and nothing is refused.
    Unwrapped,
    /// Hand it to the runtime.
    Wrap,
    /// Strict mode, and the call asked to opt out.
    Refuse,
}

impl SandboxPolicy {
    /// The whole table, in one place, so the three entry points below
    /// cannot answer it differently.
    fn verdict(&self, command: &str, dangerously_disable_sandbox: bool) -> Verdict {
        if !self.is_active() || self.is_command_excluded(command) {
            return Verdict::Unwrapped;
        }
        if dangerously_disable_sandbox {
            return match self.override_mode {
                OverrideMode::Open => Verdict::Unwrapped,
                OverrideMode::Closed => Verdict::Refuse,
            };
        }
        Verdict::Wrap
    }
}

impl CommandSandbox for SandboxPolicy {
    /// The *policy* half of the decision — may this command run outside
    /// the sandbox at all. The wrapping half is [`SandboxPolicy::prepare`],
    /// which re-applies the same table and then builds the confined argv.
    /// This stays separate because `check_permissions` runs before `call`
    /// and needs the answer without building a command.
    ///
    /// ## Decision table
    ///
    /// | sandbox active? | excluded? | disable_sandbox | mode   | result          |
    /// |-----------------|-----------|-----------------|--------|-----------------|
    /// | no              | —         | —               | —      | Ok (passthrough)|
    /// | yes             | yes       | —               | —      | Ok (bypass)     |
    /// | yes             | no        | true            | open   | Ok (bypass)     |
    /// | yes             | no        | true            | closed | Err (denied)    |
    /// | yes             | no        | false           | —      | Ok (wrapped)    |
    fn check(
        &self,
        tool: &ToolId,
        command: &str,
        dangerously_disable_sandbox: bool,
    ) -> ToolResult<()> {
        match self.verdict(command, dangerously_disable_sandbox) {
            Verdict::Unwrapped | Verdict::Wrap => Ok(()),
            Verdict::Refuse => Err(ToolError::PermissionDenied {
                tool: tool.clone(),
                reason: STRICT_MODE_REFUSAL.into(),
            }),
        }
    }

    /// A *refused* command answers `false` here, and that is not a lie about
    /// what will happen to it: it is an error either way, and the payload
    /// shape the caller picks off the back of this never reaches a process.
    fn will_wrap(&self, command: &str, dangerously_disable_sandbox: bool) -> bool {
        matches!(
            self.verdict(command, dangerously_disable_sandbox),
            Verdict::Wrap
        )
    }

    fn prepare(
        &self,
        tool: &ToolId,
        policy_command: &str,
        payload: &str,
        shell: BinShell,
        cwd: Option<&str>,
        dangerously_disable_sandbox: bool,
    ) -> ToolResult<PreparedCommand> {
        self.prepare_with_fs(
            tool,
            policy_command,
            payload,
            shell,
            cwd,
            dangerously_disable_sandbox,
            &RealFs,
        )
    }
}

impl SandboxPolicy {
    /// [`SandboxPolicy::prepare`] against an injected filesystem.
    ///
    /// The mount plan asks the real filesystem about the paths it pins,
    /// so a test that asserts on the notices a command produces would
    /// otherwise be reading the machine it runs on: on macOS `/etc` is a
    /// symlink to `/private/etc`, which is a perfectly good reason to
    /// drop an ancestor pin and a note saying so -- and nothing to do
    /// with the command under test.
    #[allow(clippy::too_many_arguments)]
    fn prepare_with_fs(
        &self,
        tool: &ToolId,
        policy_command: &str,
        payload: &str,
        shell: BinShell,
        cwd: Option<&str>,
        dangerously_disable_sandbox: bool,
        fs: &dyn FsProbe,
    ) -> ToolResult<PreparedCommand> {
        match self.verdict(policy_command, dangerously_disable_sandbox) {
            Verdict::Unwrapped => return Ok(PreparedCommand::passthrough(&shell, payload, cwd)),
            Verdict::Refuse => {
                return Err(ToolError::PermissionDenied {
                    tool: tool.clone(),
                    reason: STRICT_MODE_REFUSAL.into(),
                })
            }
            Verdict::Wrap => {}
        }

        // Policy says confine, so a runtime has to exist. Running the
        // command anyway would be the one outcome the policy exists to
        // prevent, and it would be invisible — the command would succeed.
        let Some(runtime) = self.runtime.as_ref() else {
            return Err(ToolError::Execution {
                tool: tool.clone(),
                source: anyhow::anyhow!(
                    "the sandbox is enabled but its runtime was never initialised, so this \
                     command cannot be confined"
                ),
            });
        };

        let session = runtime.session_config();
        let mut request = CommandRequest::new(payload, shell);
        if let Some(cwd) = cwd {
            request = request.with_cwd(PathBuf::from(cwd));
        }
        // A session that names domains is a session whose commands go
        // through the proxy; one that names none has no opinion about the
        // network and must not pay for a bridge it does not need.
        request = request.with_network_restriction(session.network.is_restricted());
        // git refuses a repository owned by another uid with `dubious
        // ownership`, and inside the sandbox every writable root is
        // exactly that. Registering them keeps `git status` working
        // without touching the user's real config.
        request.git_safe_directories = cwd
            .map(PathBuf::from)
            .into_iter()
            .chain(session.filesystem.allow_write.iter().cloned())
            .collect();

        runtime
            .assert_confined(true)
            .map_err(|err| sandbox_error(tool, err))?;
        let wrapped = runtime
            .wrap_with_fs(&request, fs)
            .map_err(|err| sandbox_error(tool, err))?;
        // Asked again after the wrap: the first call proved the machine
        // can confine something, this one is the last point before spawn
        // at which a refusal still means the command never ran.
        runtime
            .assert_confined(wrapped.backend.is_confined())
            .map_err(|err| sandbox_error(tool, err))?;

        for warning in &wrapped.warnings {
            tracing::warn!(
                backend = warning.backend,
                code = warning.code,
                "sandbox rule not applied: {}",
                warning.detail
            );
        }

        Ok(prepared_from(wrapped))
    }
}

/// The message shown when strict mode refuses `dangerouslyDisableSandbox`.
pub const STRICT_MODE_REFUSAL: &str =
    "Strict sandbox mode is active — `dangerouslyDisableSandbox` is not allowed. \
     All commands must run in sandbox or be excluded via the `excludedCommands` option.";

/// Hand the wrap back in the vocabulary the tool layer spawns from.
///
/// The warnings become sentences here rather than at the call site: they reach
/// the **tool result**, not just the log. A degraded rule changes what the
/// command can do, and the model is the one that has to interpret the failure
/// — a build that cannot reach the registry looks exactly like a broken
/// network unless something says "the sandbox gave this command no network
/// because a domain rule could not be applied". Until this existed, every one
/// of those sentences went to `tracing::warn!`, which in TUI mode is a file
/// the user never opens.
fn prepared_from(wrapped: WrappedCommand) -> PreparedCommand {
    PreparedCommand {
        program: wrapped.program,
        args: wrapped.args,
        env_set: wrapped.env_set,
        env_unset: wrapped.env_unset,
        cwd: wrapped.cwd,
        confined: wrapped.backend.is_confined(),
        backend: wrapped.backend.as_str(),
        notices: wrapped
            .warnings
            .iter()
            .map(|warning| warning.to_string())
            .collect(),
    }
}

/// Map a sandbox failure onto the tool error the model will read.
///
/// A refused confinement is a *permission* error, not an execution
/// error: nothing went wrong running the command, the command was not
/// allowed to run. The distinction matters downstream — an execution
/// error reads as "your command is broken" and invites the model to
/// rewrite a command that was fine.
fn sandbox_error(tool: &ToolId, error: SandboxError) -> ToolError {
    match error {
        SandboxError::NotConfined { .. } => ToolError::PermissionDenied {
            tool: tool.clone(),
            reason: error.to_string(),
        },
        other => ToolError::Execution {
            tool: tool.clone(),
            source: anyhow::anyhow!(other.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        ConfinedProbe, ConfinedVerdict, SandboxMode, SandboxRuntimeInit, SessionSandboxConfig,
        SupportReport,
    };
    use rebon_tool::command_sandbox::{attach_notices, SANDBOX_NOTES_KEY};

    struct AlwaysConfined;
    impl ConfinedProbe for AlwaysConfined {
        fn probe(&self) -> ConfinedVerdict {
            ConfinedVerdict::confined("test")
        }
    }

    struct NeverConfined;
    impl ConfinedProbe for NeverConfined {
        fn probe(&self) -> ConfinedVerdict {
            ConfinedVerdict::unconfined("no namespaces available")
        }
    }

    fn runtime_with(
        platform: SandboxPlatform,
        mutate: impl FnOnce(&mut SessionSandboxConfig),
        probe: Arc<dyn ConfinedProbe>,
    ) -> Arc<SandboxRuntime> {
        let mut session = SessionSandboxConfig::default();
        session.runtime.bwrap_path = Some(PathBuf::from("/usr/bin/bwrap"));
        session.runtime.sandbox_win_path = Some(PathBuf::from("C:/Rebon/sandbox-win.exe"));
        mutate(&mut session);
        Arc::new(SandboxRuntime::new(SandboxRuntimeInit {
            platform,
            session,
            mode: SandboxMode::Strict,
            log_tag: "test-tag".into(),
            debug_session: false,
            support: SupportReport {
                platform,
                ..Default::default()
            },
            probe,
            session_resources: None,
        }))
    }

    fn policy(mode: OverrideMode, runtime: Option<Arc<SandboxRuntime>>) -> SandboxPolicy {
        SandboxPolicy {
            enabled: true,
            platform: runtime
                .as_ref()
                .map(|rt| rt.platform())
                .unwrap_or(SandboxPlatform::Linux),
            override_mode: mode,
            excluded_commands: vec!["git".into()],
            runtime,
        }
    }

    fn bash_shell() -> BinShell {
        BinShell::new("sh", ["-lc"])
    }

    fn tool() -> ToolId {
        ToolId::new("Bash")
    }

    fn prepare(
        policy: &SandboxPolicy,
        policy_command: &str,
        payload: &str,
        shell: BinShell,
        cwd: Option<&str>,
        disable: bool,
    ) -> ToolResult<PreparedCommand> {
        policy.prepare(&tool(), policy_command, payload, shell, cwd, disable)
    }

    #[test]
    fn a_degraded_rule_reaches_the_tool_result_not_just_the_log() {
        // The whole point. A command whose network was cut because a domain
        // rule could not be applied looks, to the model, exactly like a
        // command on a broken network — unless the result says otherwise.
        // Until this existed the sentence went to `tracing::warn!`, which in
        // TUI mode is a file the user never opens.
        let runtime = runtime_with(
            SandboxPlatform::Linux,
            |session| {
                session.network.allowed_domains = vec!["api.example.com".into()];
                // No proxy port: nothing is listening.
            },
            Arc::new(AlwaysConfined),
        );
        let prepared = prepare(
            &policy(OverrideMode::Closed, Some(runtime)),
            "curl https://api.example.com",
            "curl https://api.example.com",
            bash_shell(),
            None,
            false,
        )
        .unwrap();

        let mut result = serde_json::json!({ "stdout": "" });
        attach_notices(&mut result, &prepared);

        let notes = result[SANDBOX_NOTES_KEY].as_array().unwrap_or_else(|| {
            panic!("the degradation never reached the result: {result}");
        });
        assert!(
            notes.iter().any(|note| note
                .as_str()
                .unwrap_or_default()
                .contains("api.example.com")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_clean_command_carries_no_notes_field_at_all() {
        // Absent rather than empty: a command with nothing degraded should
        // not make the model read past a field.
        let runtime = runtime_with(
            SandboxPlatform::Linux,
            |session| session.filesystem.deny_read = vec![PathBuf::from("/secret")],
            Arc::new(AlwaysConfined),
        );
        // Against a filesystem this test describes, not the one it happens
        // to run on: the mount plan pins ancestors of the paths it binds,
        // and on macOS `/etc` is a symlink to `/private/etc`, which earns
        // a perfectly correct note that has nothing to do with `echo hi`.
        let fs = crate::runtime::fs_probe::FakeFs::new()
            .dir("/etc")
            .dir("/etc/ssh")
            .dir("/etc/ssh/ssh_config.d")
            .dir("/usr")
            .dir("/usr/bin")
            .dir("/tmp");
        let prepared = policy(OverrideMode::Closed, Some(runtime))
            .prepare_with_fs(
                &tool(),
                "echo hi",
                "echo hi",
                bash_shell(),
                None,
                false,
                &fs,
            )
            .unwrap();

        let mut result = serde_json::json!({ "stdout": "" });
        attach_notices(&mut result, &prepared);

        assert!(result.get(SANDBOX_NOTES_KEY).is_none(), "{result}");
        assert!(prepared.notices.is_empty());
    }

    #[test]
    fn a_notice_reads_as_a_sentence_with_its_backend() {
        let runtime = runtime_with(
            SandboxPlatform::Linux,
            |session| session.filesystem.allow_write = vec![PathBuf::from("/work/*/build")],
            Arc::new(AlwaysConfined),
        );
        let prepared = prepare(
            &policy(OverrideMode::Closed, Some(runtime)),
            "echo hi",
            "echo hi",
            bash_shell(),
            None,
            false,
        )
        .unwrap();

        assert!(!prepared.notices.is_empty());
        assert!(
            prepared.notices[0].starts_with("[Sandbox linux]"),
            "{:?}",
            prepared.notices
        );
    }

    #[test]
    fn a_disabled_policy_runs_the_command_unwrapped() {
        let mut policy = policy(OverrideMode::Closed, None);
        policy.enabled = false;

        let prepared = prepare(&policy, "echo hi", "echo hi", bash_shell(), None, false).unwrap();

        assert!(!prepared.confined);
        assert_eq!(prepared.backend, "passthrough");
        assert_eq!(prepared.program, "sh");
        assert_eq!(prepared.args, vec!["-lc", "echo hi"]);
    }

    #[test]
    fn an_excluded_command_bypasses_even_in_closed_mode() {
        let runtime = runtime_with(SandboxPlatform::Linux, |_| {}, Arc::new(NeverConfined));
        let policy = policy(OverrideMode::Closed, Some(runtime));

        let prepared = prepare(
            &policy,
            "git status",
            "git status",
            bash_shell(),
            None,
            false,
        )
        .unwrap();

        assert!(!policy.will_wrap("git status", false));
        assert!(!prepared.confined);
        assert_eq!(prepared.backend, "passthrough");
    }

    #[test]
    fn exclusion_matches_the_executable_not_a_prefix() {
        let runtime = runtime_with(SandboxPlatform::Linux, |_| {}, Arc::new(AlwaysConfined));
        let policy = policy(OverrideMode::Closed, Some(runtime));

        assert!(
            policy.will_wrap("gitleaks detect", false),
            "`gitleaks` must not inherit `git`'s exclusion"
        );
        prepare(
            &policy,
            "gitleaks detect",
            "gitleaks detect",
            bash_shell(),
            None,
            false,
        )
        .unwrap();
    }

    #[test]
    fn disable_flag_is_honoured_in_open_mode() {
        let runtime = runtime_with(SandboxPlatform::Linux, |_| {}, Arc::new(AlwaysConfined));
        let policy = policy(OverrideMode::Open, Some(runtime));

        let prepared = prepare(&policy, "curl x", "curl x", bash_shell(), None, true).unwrap();

        assert!(!policy.will_wrap("curl x", true));
        assert_eq!(prepared.backend, "passthrough");
        assert!(policy.check(&tool(), "curl x", true).is_ok());
    }

    #[test]
    fn disable_flag_is_refused_in_closed_mode() {
        let runtime = runtime_with(SandboxPlatform::Linux, |_| {}, Arc::new(AlwaysConfined));
        let policy = policy(OverrideMode::Closed, Some(runtime));

        // Both halves refuse, and with the same sentence: `check_permissions`
        // runs before `call`, and a user told two different things about one
        // command has to work out which one was the real reason.
        for error in [
            prepare(&policy, "curl x", "curl x", bash_shell(), None, true).unwrap_err(),
            policy.check(&tool(), "curl x", true).unwrap_err(),
        ] {
            match error {
                ToolError::PermissionDenied { tool, reason } => {
                    assert_eq!(tool.as_str(), "Bash");
                    assert!(reason.contains("dangerouslyDisableSandbox"));
                    assert!(reason.contains("Strict sandbox mode"));
                }
                other => panic!("expected PermissionDenied, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_active_policy_without_a_runtime_refuses_rather_than_running_free() {
        let policy = policy(OverrideMode::Closed, None);

        let error = prepare(&policy, "curl x", "curl x", bash_shell(), None, false).unwrap_err();

        assert!(matches!(error, ToolError::Execution { .. }));
        assert!(error.to_string().contains("never initialised"));
    }

    #[test]
    fn a_failed_confinement_probe_is_a_permission_error() {
        let runtime = runtime_with(SandboxPlatform::Linux, |_| {}, Arc::new(NeverConfined));
        let policy = policy(OverrideMode::Closed, Some(runtime));

        let error = prepare(&policy, "curl x", "curl x", bash_shell(), None, false).unwrap_err();

        match error {
            ToolError::PermissionDenied { reason, .. } => {
                assert!(reason.contains("no namespaces available"));
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
    }

    #[test]
    fn a_session_with_no_rules_still_takes_the_fast_path() {
        let runtime = runtime_with(SandboxPlatform::Linux, |_| {}, Arc::new(AlwaysConfined));
        let policy = policy(OverrideMode::Closed, Some(runtime));

        let prepared = prepare(&policy, "echo hi", "echo hi", bash_shell(), None, false).unwrap();

        assert!(policy.will_wrap("echo hi", false));
        assert_eq!(prepared.backend, "passthrough");
        assert_eq!(prepared.args, vec!["-lc", "echo hi"]);
    }

    #[test]
    fn a_session_with_write_roots_wraps_the_command() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let runtime = runtime_with(
            SandboxPlatform::Linux,
            |session| session.filesystem.allow_write = vec![root.clone()],
            Arc::new(AlwaysConfined),
        );
        let policy = policy(OverrideMode::Closed, Some(runtime));

        let prepared = prepare(&policy, "echo hi", "echo hi", bash_shell(), None, false).unwrap();

        assert_eq!(prepared.backend, "bubblewrap");
        assert_eq!(prepared.program, "/usr/bin/bwrap");
        assert!(prepared.confined);
    }

    #[test]
    fn network_rules_in_the_session_restrict_the_command() {
        let runtime = runtime_with(
            SandboxPlatform::Macos,
            |session| session.network.denied_domains = vec!["evil.test".into()],
            Arc::new(AlwaysConfined),
        );
        let policy = policy(OverrideMode::Closed, Some(runtime));

        let prepared = prepare(&policy, "curl x", "curl x", bash_shell(), None, false).unwrap();

        assert_eq!(prepared.backend, "seatbelt");
        assert!(
            !prepared.args[1].contains("(allow network*)"),
            "a session with denied domains must not emit the blanket network allow"
        );
    }

    #[test]
    fn cwd_and_write_roots_become_git_safe_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let runtime = runtime_with(
            SandboxPlatform::Macos,
            |session| session.filesystem.allow_write = vec![root.clone()],
            Arc::new(AlwaysConfined),
        );
        let policy = policy(OverrideMode::Closed, Some(runtime));

        // Not `git …`: that is on the exclusion list and would take
        // the passthrough path, where there is no environment to
        // inject into. The rule is for every command in the sandbox,
        // because any of them may shell out to git.
        let prepared = prepare(
            &policy,
            "cargo test",
            "cargo test",
            bash_shell(),
            Some("/work"),
            false,
        )
        .unwrap();

        let count = prepared
            .env_set
            .iter()
            .find(|(key, _)| key == "GIT_CONFIG_COUNT")
            .map(|(_, value)| value.clone())
            .expect("git safe directories injected");
        assert_eq!(count, "2");
        assert!(prepared
            .env_set
            .iter()
            .any(|(key, value)| key == "GIT_CONFIG_VALUE_0" && value == "/work"));
    }

    #[test]
    fn the_encoded_command_shell_reaches_the_wrapped_argv() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let runtime = runtime_with(
            SandboxPlatform::Macos,
            |session| session.filesystem.allow_write = vec![root],
            Arc::new(AlwaysConfined),
        );
        let policy = policy(OverrideMode::Closed, Some(runtime));
        let shell = BinShell::new("pwsh", ["-NoProfile", "-NonInteractive", "-EncodedCommand"]);

        let prepared =
            prepare(&policy, "ZQBjAGgAbwA=", "ZQBjAGgAbwA=", shell, None, false).unwrap();

        let args = &prepared.args;
        assert_eq!(
            &args[args.len() - 5..],
            &[
                "pwsh",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                "ZQBjAGgAbwA="
            ]
        );
    }

    /// The full table, one row at a time, asserted through the seam a tool
    /// actually calls. `check` and `prepare` have to agree on every row: they
    /// run in different phases of the same call, and a disagreement is a
    /// command that is allowed and then refused, or worse.
    #[test]
    fn the_decision_table_agrees_with_itself_on_every_row() {
        // (enabled, platform, command, disable_flag, mode, may_run)
        let table: Vec<(bool, SandboxPlatform, &str, bool, OverrideMode, bool)> = vec![
            // Sandbox inactive: enforcement never applies.
            (
                false,
                SandboxPlatform::Macos,
                "rm -rf /",
                true,
                OverrideMode::Closed,
                true,
            ),
            (
                false,
                SandboxPlatform::Macos,
                "rm -rf /",
                false,
                OverrideMode::Closed,
                true,
            ),
            (
                false,
                SandboxPlatform::Windows,
                "curl x",
                true,
                OverrideMode::Closed,
                true,
            ),
            // A platform with no backend at all. `Unknown`, not `Windows`:
            // Windows has `sandbox-win`, so a Windows policy *is* active and
            // its `Closed` override really does refuse the flag. See
            // `SandboxPolicy::is_active`.
            (
                true,
                SandboxPlatform::Unknown,
                "curl x",
                true,
                OverrideMode::Closed,
                true,
            ),
            // Excluded commands bypass in both modes.
            (
                true,
                SandboxPlatform::Macos,
                "git status",
                false,
                OverrideMode::Closed,
                true,
            ),
            (
                true,
                SandboxPlatform::Macos,
                "git push",
                true,
                OverrideMode::Closed,
                true,
            ),
            (
                true,
                SandboxPlatform::Linux,
                "npm install",
                false,
                OverrideMode::Closed,
                true,
            ),
            (
                true,
                SandboxPlatform::Macos,
                "npm run build",
                true,
                OverrideMode::Open,
                true,
            ),
            // Not excluded, no flag: wrapped, which is still "may run".
            (
                true,
                SandboxPlatform::Macos,
                "curl http://example.com",
                false,
                OverrideMode::Closed,
                true,
            ),
            (
                true,
                SandboxPlatform::Linux,
                "curl http://example.com",
                false,
                OverrideMode::Open,
                true,
            ),
            // Not excluded, flag set: the mode decides.
            (
                true,
                SandboxPlatform::Macos,
                "curl x",
                true,
                OverrideMode::Open,
                true,
            ),
            (
                true,
                SandboxPlatform::Linux,
                "curl x",
                true,
                OverrideMode::Open,
                true,
            ),
            (
                true,
                SandboxPlatform::Macos,
                "curl x",
                true,
                OverrideMode::Closed,
                false,
            ),
            (
                true,
                SandboxPlatform::Linux,
                "rm -rf /",
                true,
                OverrideMode::Closed,
                false,
            ),
            // `gitleaks` does not inherit `git`'s exclusion.
            (
                true,
                SandboxPlatform::Macos,
                "gitleaks detect",
                true,
                OverrideMode::Closed,
                false,
            ),
        ];

        for (i, (enabled, platform, command, disable, mode, may_run)) in table.iter().enumerate() {
            let policy = SandboxPolicy {
                enabled: *enabled,
                platform: *platform,
                override_mode: *mode,
                excluded_commands: vec!["git".into(), "npm".into()],
                runtime: None,
            };
            let checked = policy.check(&tool(), command, *disable);
            assert_eq!(
                checked.is_ok(),
                *may_run,
                "row {i}: en={enabled}, plat={platform:?}, cmd={command}, dis={disable}, \
                 mode={mode:?}"
            );

            // `prepare` on a row that refuses must refuse identically. On a
            // row that runs it either passes through or asks for a runtime
            // this fixture does not have — never a silent unconfined spawn.
            let prepared = prepare(&policy, command, command, bash_shell(), None, *disable);
            match (may_run, policy.will_wrap(command, *disable)) {
                (false, _) => assert!(
                    matches!(prepared, Err(ToolError::PermissionDenied { .. })),
                    "row {i}: prepare disagreed with check"
                ),
                (true, false) => assert_eq!(
                    prepared.expect("row runs unwrapped").backend,
                    "passthrough",
                    "row {i}"
                ),
                (true, true) => assert!(
                    prepared
                        .expect_err("row {i}: a wrap with no runtime must refuse")
                        .to_string()
                        .contains("never initialised"),
                    "row {i}"
                ),
            }
        }
    }
}
