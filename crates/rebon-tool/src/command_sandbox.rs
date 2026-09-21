//! The seam between a shell command and whatever confines it.
//!
//! `Bash` and `PowerShell` both end up here, and they end up here for the same
//! reason: whether a command is confined is a property of *running a command
//! on this machine*, not of which tool asked. The two differ in how they hand
//! their payload to a shell — `sh -lc <script>` versus
//! `pwsh … -EncodedCommand <base64>` — and that difference is expressed as a
//! [`BinShell`] value, not as a second code path.
//!
//! ## Why the words here are so plain
//!
//! Nothing in this module mentions bubblewrap, seatbelt or `sandbox-win`, and
//! nothing mentions the settings block that turns them on. It has two nouns —
//! a command and a process — and one question: *is this command allowed to run
//! as written, and what should be spawned instead if it is not?*
//!
//! The answers live in the sandbox plugin, behind
//! [`SessionSandboxService`]. That is what makes `plugins.sandbox.enabled =
//! false` mean something: with no provider on the seat, nothing here has an
//! opinion, and `Bash` spawns exactly the argv it would have spawned on a
//! machine where the feature was never written.
//!
//! ## What the two halves are for
//!
//! [`CommandSandbox::check`] answers before there is a command to build —
//! `check_permissions` runs ahead of `call` and needs the policy verdict
//! without paying for an argv. [`CommandSandbox::prepare`] answers the same
//! question *and* builds the process, so the two cannot disagree; the tool
//! calls exactly one of them per phase and never re-derives the decision.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use rebon_tools_core::{ToolError, ToolId, ToolResult};
use tokio::process::Command;

/// `CREATE_NO_WINDOW` — keeps a confined child from flashing a console window
/// on Windows, exactly as the unconfined path does.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A shell and the arguments that precede the payload.
///
/// `sh -lc`, `bash -c`, `pwsh -NoProfile -NonInteractive -Command` and
/// `pwsh … -EncodedCommand` are all the same shape, so a tool's passing modes
/// are different `BinShell` values rather than different code paths. Keeping
/// the shell and the payload as *separate* values is what makes a command
/// wrappable at all: a wrapper on Windows constructs an argv, not a command
/// string, and cannot take one apart again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinShell {
    pub program: String,
    /// Arguments that precede the command payload.
    pub args: Vec<String>,
}

impl BinShell {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// `sh -lc` — the Unix default.
    pub fn posix() -> Self {
        Self::new("sh", ["-lc"])
    }

    /// The full argv for running `command` outside any sandbox.
    pub fn argv(&self, command: &str) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.args.len() + 2);
        argv.push(self.program.clone());
        argv.extend(self.args.iter().cloned());
        argv.push(command.to_owned());
        argv
    }
}

/// A command that is ready to spawn.
///
/// The five spawn fields are what a spawner needs and nothing else. The other
/// three are what a *caller* needs in order to say something truthful about
/// what it is about to run.
#[derive(Debug, Clone)]
pub struct PreparedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub env_unset: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Whether the operating system is actually enforcing anything here.
    ///
    /// Not derivable from the fact that a sandbox exists: a session with no
    /// rules that apply to this command hands back the original argv, which is
    /// the same process that would have run with the feature switched off. A
    /// caller that needs to know can ask instead of assuming.
    pub confined: bool,
    /// What produced this command, for logs and diagnostics.
    pub backend: &'static str,
    /// Rules that could not be applied as written, as sentences.
    ///
    /// These reach the **tool result**, not just the log. A degraded rule
    /// changes what the command can do, and the model is the one that has to
    /// interpret the failure: a build that cannot reach the registry looks
    /// exactly like a broken network unless something says why.
    pub notices: Vec<String>,
}

impl PreparedCommand {
    /// The command as the tool built it, with nothing wrapped around it.
    ///
    /// The `None` branch of every caller: no provider on the seat means no
    /// opinion, and this is the argv a machine without the feature runs.
    pub fn passthrough(shell: &BinShell, payload: &str, cwd: Option<&str>) -> Self {
        let argv = shell.argv(payload);
        Self {
            program: argv[0].clone(),
            args: argv[1..].to_vec(),
            env_set: Vec::new(),
            env_unset: Vec::new(),
            cwd: cwd.map(PathBuf::from),
            confined: false,
            backend: PASSTHROUGH_BACKEND,
            notices: Vec::new(),
        }
    }
}

/// The `backend` of a command nothing wrapped.
pub const PASSTHROUGH_BACKEND: &str = "passthrough";

/// Turn a prepared command into a spawnable process.
///
/// Every child gets the same stdio shape whether or not anything confined it —
/// null stdin (the model has no input channel), piped output, killed on drop —
/// so both are read back by the same code.
pub fn to_process(prepared: &PreparedCommand) -> Command {
    let mut process = Command::new(&prepared.program);
    process
        .args(&prepared.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in &prepared.env_set {
        process.env(key, value);
    }
    for key in &prepared.env_unset {
        process.env_remove(key);
    }
    if let Some(cwd) = &prepared.cwd {
        process.current_dir(cwd);
    }
    #[cfg(windows)]
    process.creation_flags(CREATE_NO_WINDOW);
    process
}

/// The tool-result key carrying [`PreparedCommand::notices`].
///
/// One constant so `Bash` and `PowerShell` cannot spell it differently and
/// leave a consumer reading one of them.
pub const SANDBOX_NOTES_KEY: &str = "sandboxNotes";

/// Attach the notices to a tool result, if there are any.
///
/// Absent rather than empty in the common case: a command with nothing
/// degraded should not carry a field the model has to read past.
pub fn attach_notices(result: &mut serde_json::Value, prepared: &PreparedCommand) {
    if prepared.notices.is_empty() {
        return;
    }
    if let Some(object) = result.as_object_mut() {
        object.insert(
            SANDBOX_NOTES_KEY.to_string(),
            serde_json::Value::Array(
                prepared
                    .notices
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }
}

/// Whatever decides how a shell command runs on this machine.
///
/// One implementation ships with Rebon (the sandbox plugin); the trait
/// is here because its callers are here and because a tool must be able to
/// hold *nothing* — the overwhelmingly common case — without knowing that the
/// alternative exists.
pub trait CommandSandbox: Send + Sync + fmt::Debug {
    /// May this command run as asked? Answered before an argv exists.
    ///
    /// `Ok(())` covers both "run it unchanged" and "run it wrapped"; the
    /// difference does not matter to a permission prompt, and collapsing it
    /// here is what keeps `check_permissions` from having to build a command
    /// it may never spawn.
    fn check(
        &self,
        tool: &ToolId,
        command: &str,
        dangerously_disable_sandbox: bool,
    ) -> ToolResult<()>;

    /// Whether this command will be handed to the sandbox at all.
    ///
    /// Only PowerShell needs this, and it needs it *before* [`prepare`]:
    /// a wrapped command crosses at least one more argv layer, so its payload
    /// has to be base64 of UTF-16LE, and the shell prefix that carries it is
    /// therefore a different `BinShell`. Asking afterwards would mean
    /// discovering the payload was the wrong shape and wrapping twice.
    ///
    /// [`prepare`]: CommandSandbox::prepare
    fn will_wrap(&self, command: &str, dangerously_disable_sandbox: bool) -> bool;

    /// Decide *and* build, in one call.
    ///
    /// Two command strings, and conflating them is a real hole rather than a
    /// naming preference:
    ///
    /// * `policy_command` is **what the user asked to run** — the text an
    ///   exclusion list is matched against. It is the only string with an
    ///   "executable" in it in the sense a policy means.
    /// * `payload` is **what `shell` receives**. For Bash the two are the
    ///   same string. For PowerShell the payload already carries the encoding
    ///   prologue and the exit-code epilogue, and under a sandbox it is
    ///   base64 — so an exclusion for `git` would match no PowerShell command
    ///   ever, and every command in an exclusion-only configuration would be
    ///   silently wrapped.
    fn prepare(
        &self,
        tool: &ToolId,
        policy_command: &str,
        payload: &str,
        shell: BinShell,
        cwd: Option<&str>,
        dangerously_disable_sandbox: bool,
    ) -> ToolResult<PreparedCommand>;
}

/// A sandbox that refuses everything, with the reason it was asked to.
///
/// The fail-closed answer to a configuration that asks for two contradictory
/// things: confinement is switched on, and the thing that provides it is
/// switched off. Running the commands anyway would be the one outcome the
/// setting exists to prevent, and it would be invisible — every command would
/// succeed.
#[derive(Debug, Clone)]
pub struct RefusingSandbox {
    reason: String,
}

impl RefusingSandbox {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    fn refuse<T>(&self, tool: &ToolId) -> ToolResult<T> {
        Err(ToolError::PermissionDenied {
            tool: tool.clone(),
            reason: self.reason.clone(),
        })
    }
}

impl CommandSandbox for RefusingSandbox {
    fn check(&self, tool: &ToolId, _command: &str, _disable: bool) -> ToolResult<()> {
        self.refuse(tool)
    }

    /// Nothing is wrapped, because nothing runs. The payload shape a caller
    /// picks off the back of this never reaches a process.
    fn will_wrap(&self, _command: &str, _disable: bool) -> bool {
        false
    }

    fn prepare(
        &self,
        tool: &ToolId,
        _policy_command: &str,
        _payload: &str,
        _shell: BinShell,
        _cwd: Option<&str>,
        _disable: bool,
    ) -> ToolResult<PreparedCommand> {
        self.refuse(tool)
    }
}

/// Stable typed name of the seat a front end resolves a session's sandbox off.
pub const SESSION_SANDBOX_SERVICE: &str = "session-sandbox";

/// The provider behind the seat.
///
/// One provider, not a chain. Two things claiming to confine the same command
/// is not a merge, it is a question about which one wins, and a security
/// decision must not have an answer that depends on registration order.
pub trait SessionSandboxSource: Send + Sync {
    /// The sandbox this workspace's commands run under, or `None` when the
    /// settings ask for none.
    ///
    /// Called once per session, on the path that builds the executor: the
    /// result is a session-scoped object (a compiled rule set, and on Linux a
    /// running proxy) whose lifetime is the session's.
    fn for_session(&self, cwd: &Path) -> Option<Arc<dyn CommandSandbox>>;

    /// What `/doctor` should show about this machine's sandbox.
    ///
    /// `settings_overrides` are the `--settings` arguments, in the order they
    /// were given: each is either a JSON object or a path to one, and both
    /// layer over the files on disk exactly as they do for a real session.
    fn doctor(&self, cwd: &Path, settings_overrides: &[String]) -> SandboxDoctor;
}

/// Typed definition for the kernel's `session-sandbox` seat.
pub struct SessionSandboxService;

impl rebon_kernel::Service for SessionSandboxService {
    type Interface = dyn SessionSandboxSource;
    const NAME: &'static str = SESSION_SANDBOX_SERVICE;
}

/// How bad one diagnostic line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorLevel {
    Ok,
    Warning,
    Error,
}

/// One line of `/doctor`'s sandbox section.
///
/// Deliberately not the front end's own row type: the plugin knows what is
/// wrong with this machine, and the front end knows how a row is drawn. A
/// shared struct in the middle would make one of them depend on the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorLine {
    pub level: DoctorLevel,
    pub label: String,
    pub message: String,
}

impl DoctorLine {
    pub fn ok(label: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at(DoctorLevel::Ok, label, message)
    }

    pub fn warning(label: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at(DoctorLevel::Warning, label, message)
    }

    pub fn error(label: impl Into<String>, message: impl Into<String>) -> Self {
        Self::at(DoctorLevel::Error, label, message)
    }

    fn at(level: DoctorLevel, label: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level,
            label: label.into(),
            message: message.into(),
        }
    }
}

/// The sandbox section of `/doctor`, plus the one fact another section needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxDoctor {
    /// Whether `sandbox.enabled` came out true after the whole settings chain.
    ///
    /// Out here rather than parsed back off a line because the "Shell
    /// commands" section needs it too: `bwrap` and `socat` are required
    /// dependencies on Linux only when the sandbox is on, and re-reading the
    /// settings to find that out is how the two sections come to disagree.
    pub enabled_in_settings: bool,
    pub lines: Vec<DoctorLine>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_passthrough_is_the_shell_argv_with_the_payload_appended() {
        let prepared = PreparedCommand::passthrough(&BinShell::posix(), "echo hi", Some("/work"));

        assert_eq!(prepared.program, "sh");
        assert_eq!(prepared.args, vec!["-lc", "echo hi"]);
        assert_eq!(prepared.cwd, Some(PathBuf::from("/work")));
        assert!(!prepared.confined);
        assert_eq!(prepared.backend, PASSTHROUGH_BACKEND);
        assert!(prepared.notices.is_empty());
        assert!(prepared.env_set.is_empty());
    }

    /// Both halves refuse, and both refuse as a *permission* error: nothing
    /// went wrong running the command, the command was not allowed to run.
    #[test]
    fn a_refusing_sandbox_denies_both_halves_with_its_reason() {
        let sandbox = RefusingSandbox::new("the plugin is disabled");
        let tool = ToolId::new("Bash");

        for error in [
            sandbox.check(&tool, "echo hi", false).unwrap_err(),
            sandbox
                .prepare(&tool, "echo hi", "echo hi", BinShell::posix(), None, false)
                .unwrap_err(),
        ] {
            match error {
                ToolError::PermissionDenied { reason, .. } => {
                    assert_eq!(reason, "the plugin is disabled");
                }
                other => panic!("expected PermissionDenied, got {other:?}"),
            }
        }
        assert!(!sandbox.will_wrap("echo hi", false));
    }
}
