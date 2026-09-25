use crate::command_sandbox::{self, BinShell, PreparedCommand};
use crate::edit::optional_bool;
use crate::output_truncation::{finish_shell_streams, tool_output_max_bytes};
use crate::shell_process::{ShellLineReader, ShellOutputEncoding};
use crate::{Tool, ToolContext};
use async_trait::async_trait;
use rebon_shell_policy::lexer::{self, simple_command_segments};
use rebon_tools_core::{
    require_valid_input, validation_outcome_from, PermissionDecision, PermissionRequest, ToolError,
    ToolId, ToolInputSchema, ToolProgressUpdate, ToolResult, ValidationOutcome,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{sleep, Duration};

const BASH_TOOL_NAME: &str = "Bash";
const INVALID_INPUT_CODE: i64 = 400;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

/// Bash tool. The description is computed once at construction time
/// (mirroring `AgentTool::new`) so we can concatenate the base
/// guidance, the background-usage note
/// (`BASH_BACKGROUND_USAGE_NOTE`), and the
/// commit/PR protocol
/// (`BASH_COMMIT_AND_PR_INSTRUCTIONS`) without paying
/// for string building on every `description()` call.
#[derive(Debug, Clone)]
pub struct BashTool {
    description: String,
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Base Bash tool guidance: the opening of the tool description, before the
/// background-task note and the commit/PR section are appended.
const BASH_BASE_DESCRIPTION: &str = "Executes a given bash command and returns its output.\n\
\n\
The working directory persists between commands, but shell state does not. The shell \
environment is initialized from the user's profile (bash or zsh).\n\
\n\
IMPORTANT: Avoid using this tool to run `find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, \
or `echo` commands, unless explicitly instructed or after you have verified that a dedicated \
tool cannot accomplish your task. Instead, use the appropriate dedicated tool as this will \
provide a much better experience for the user:\n\
\n\
 - File search: Use Glob (NOT find or ls)\n\
 - Content search: Use Grep (NOT grep or rg)\n\
 - Read files: Use Read (NOT cat/head/tail)\n\
 - Edit files: Use Edit (NOT sed/awk)\n\
 - Write files: Use Write (NOT echo >/cat <<EOF)\n\
 - Communication: Output text directly (NOT echo/printf)\n\
While the Bash tool can do similar things, it\u{2019}s better to use the built-in tools as \
they provide a better user experience and make it easier to review tool calls and give \
permission.\n\
\n\
# Instructions\n\
 - If your command will create new directories or files, first use this tool to run `ls` to \
verify the parent directory exists and is the correct location.\n\
 - Always quote file paths that contain spaces with double quotes in your command (e.g., \
cd \"path with spaces/file.txt\")\n\
 - Try to maintain your current working directory throughout the session by using absolute \
paths and avoiding usage of `cd`. You may use `cd` if the User explicitly requests it.\n\
 - You may specify an optional timeout in milliseconds (up to 600000ms / 10 minutes). \
Foreground commands default to 60000ms (1 minute). Background commands run until they exit, \
are stopped with ShellStop, or Rebon exits unless you specify a timeout.\n\
 - When issuing multiple commands:\n\
  - If the commands are independent and can run in parallel, make multiple Bash tool calls \
in a single message. Example: if you need to run \"git status\" and \"git diff\", send a \
single message with two Bash tool calls in parallel.\n\
  - If the commands depend on each other and must run sequentially, use a single Bash call \
with '&&' to chain them together.\n\
  - Use ';' only when you need to run commands sequentially but don't care if earlier \
commands fail.\n\
  - DO NOT use newlines to separate commands (newlines are ok in quoted strings).";

/// The background-usage note appended to the base description.
/// Describes the `run_in_background` parameter.
const BASH_BACKGROUND_USAGE_NOTE: &str = " - Use `run_in_background` for long-running commands \
you want to watch (builds, test suites, dev servers you will read output from) without appending '&'. \
The call returns a `shellId` immediately; keep working instead of polling. To block until it \
finishes, make one ShellOutput call with wait=true and a long timeout (up to 300000 ms), which \
returns when the process exits. Use ShellOutput to list shells or read incremental output you \
need, and use ShellStop to terminate the process tree. To react to a stream of events (log lines, \
status changes) or wait for an external condition, load the deferred Monitor tool with ToolSearch \
instead of polling a background shell with ShellOutput. Background shells are killed when the session ends and are not persisted across Rebon \
restarts. If a process must outlive this session — a server or daemon that will be checked after you \
finish — do NOT use `run_in_background`; start it detached from a normal foreground call instead, \
e.g. `nohup <cmd> >/dev/null 2>&1 &` (or `setsid <cmd> >/dev/null 2>&1 &`), then verify it is up.";

/// The commit/PR protocol appended to the Bash tool description. It is the
/// long inline form (no skill shortcut) and carries no commit or PR
/// attribution lines.
const BASH_COMMIT_AND_PR_INSTRUCTIONS: &str = "\n\n\
# Committing changes with git\n\
\n\
Only create commits when requested by the user. If unclear, ask first. When the user asks \
you to create a new git commit, follow these steps carefully:\n\
\n\
You can call multiple tools in a single response. When multiple independent pieces of \
information are requested and all commands are likely to succeed, run multiple tool calls \
in parallel for optimal performance. The numbered steps below indicate which commands \
should be batched in parallel.\n\
\n\
Git Safety Protocol:\n\
- NEVER update the git config\n\
- NEVER run destructive git commands (push --force, reset --hard, checkout ., restore ., \
clean -f, branch -D) unless the user explicitly requests these actions. Taking \
unauthorized destructive actions is unhelpful and can result in lost work, so it's best \
to ONLY run these commands when given direct instructions \n\
- NEVER skip hooks (--no-verify, --no-gpg-sign, etc) unless the user explicitly requests it\n\
- NEVER run force push to main/master, warn the user if they request it\n\
- CRITICAL: Always create NEW commits rather than amending, unless the user explicitly \
requests a git amend. When a pre-commit hook fails, the commit did NOT happen \u{2014} so \
--amend would modify the PREVIOUS commit, which may result in destroying work or losing \
previous changes. Instead, after hook failure, fix the issue, re-stage, and create a NEW \
commit\n\
- When staging files, prefer adding specific files by name rather than using \"git add -A\" \
or \"git add .\", which can accidentally include sensitive files (.env, credentials) or \
large binaries\n\
- NEVER commit changes unless the user explicitly asks you to. It is VERY IMPORTANT to \
only commit when explicitly asked, otherwise the user will feel that you are being too \
proactive\n\
- When you create a git worktree yourself (`git worktree add`), place it under \
`<git-root>/.rebon/worktrees/<name>` \u{2014} the runtime keeps every agent worktree there \
and cleans that directory up. Do not scatter worktrees in sibling directories like `../<name>`\n\
\n\
1. Run the following bash commands in parallel, each using the Bash tool:\n\
  - Run a git status command to see all untracked files. IMPORTANT: Never use the -uall \
flag as it can cause memory issues on large repos.\n\
  - Run a git diff command to see both staged and unstaged changes that will be committed.\n\
  - Run a git log command to see recent commit messages, so that you can follow this \
repository's commit message style.\n\
2. Analyze all staged changes (both previously staged and newly added) and draft a commit \
message:\n\
  - Summarize the nature of the changes (eg. new feature, enhancement to an existing \
feature, bug fix, refactoring, test, docs, etc.). Ensure the message accurately reflects \
the changes and their purpose (i.e. \"add\" means a wholly new feature, \"update\" means \
an enhancement to an existing feature, \"fix\" means a bug fix, etc.).\n\
  - Do not commit files that likely contain secrets (.env, credentials.json, etc). Warn \
the user if they specifically request to commit those files\n\
  - Draft a concise (1-2 sentences) commit message that focuses on the \"why\" rather than \
the \"what\"\n\
  - Ensure it accurately reflects the changes and their purpose\n\
3. Run the following commands in parallel:\n\
   - Add relevant untracked files to the staging area.\n\
   - Create the commit with a message.\n\
   - Run git status after the commit completes to verify success.\n\
   Note: git status depends on the commit completing, so run it sequentially after the \
commit.\n\
4. If the commit fails due to pre-commit hook: fix the issue and create a NEW commit\n\
\n\
Important notes:\n\
- NEVER run additional commands to read or explore code, besides git bash commands\n\
- NEVER use the TodoWrite or Agent tools\n\
- DO NOT push to the remote repository unless the user explicitly asks you to do so\n\
- IMPORTANT: Never use git commands with the -i flag (like git rebase -i or git add -i) \
since they require interactive input which is not supported.\n\
- IMPORTANT: Do not use --no-edit with git rebase commands, as the --no-edit flag is not \
a valid option for git rebase.\n\
- If there are no changes to commit (i.e., no untracked files and no modifications), do \
not create an empty commit\n\
- In order to ensure good formatting, ALWAYS pass the commit message via a HEREDOC, a la \
this example:\n\
<example>\n\
git commit -m \"$(cat <<'EOF'\n\
   Commit message here.\n\
   EOF\n\
   )\"\n\
</example>\n\
\n\
# Creating pull requests\n\
Use the gh command via the Bash tool for ALL GitHub-related tasks including working with \
issues, pull requests, checks, and releases. If given a Github URL use the gh command to \
get the information needed.\n\
\n\
IMPORTANT: When the user asks you to create a pull request, follow these steps carefully:\n\
\n\
1. Run the following bash commands in parallel using the Bash tool, in order to understand \
the current state of the branch since it diverged from the main branch:\n\
   - Run a git status command to see all untracked files (never use -uall flag)\n\
   - Run a git diff command to see both staged and unstaged changes that will be committed\n\
   - Check if the current branch tracks a remote branch and is up to date with the remote, \
so you know if you need to push to the remote\n\
   - Run a git log command and `git diff [base-branch]...HEAD` to understand the full \
commit history for the current branch (from the time it diverged from the base branch)\n\
2. Analyze all changes that will be included in the pull request, making sure to look at \
all relevant commits (NOT just the latest commit, but ALL commits that will be included \
in the pull request!!!), and draft a pull request title and summary:\n\
   - Keep the PR title short (under 70 characters)\n\
   - Use the description/body for details, not the title\n\
3. Run the following commands in parallel:\n\
   - Create new branch if needed\n\
   - Push to remote with -u flag if needed\n\
   - Create PR using gh pr create with the format below. Use a HEREDOC to pass the body \
to ensure correct formatting.\n\
<example>\n\
gh pr create --title \"the pr title\" --body \"$(cat <<'EOF'\n\
## Summary\n\
<1-3 bullet points>\n\
\n\
## Test plan\n\
[Bulleted markdown checklist of TODOs for testing the pull request...]\n\
EOF\n\
)\"\n\
</example>\n\
\n\
Important:\n\
- DO NOT use the TodoWrite or Agent tools\n\
- Return the PR URL when you're done, so the user can see it\n\
\n\
# Other common operations\n\
- View comments on a Github PR: gh api repos/foo/bar/pulls/123/comments";

const BASH_MODEL_DESCRIPTION: &str = "Executes a given bash command and returns its output.\n\
\n\
Use this for system commands and terminal operations that require shell execution. Prefer \
dedicated file/search/edit tools when available. Supports `timeout` in milliseconds (up to \
600000) and `run_in_background` for long-running commands. Background calls return a `shellId`; \
do not poll them. To block until one finishes, make one ShellOutput call with wait=true and a \
long timeout (up to 300000 ms); use ShellStop to terminate them. To react to a stream of events (log lines, status \
changes) or wait for an external condition, load the deferred Monitor tool with ToolSearch \
instead of polling a background shell with ShellOutput.";

impl BashTool {
    pub fn new() -> Self {
        // Simple prompt layout: base prose first, then the
        // background-usage hint as a trailing bullet under
        // `# Instructions`, then the git/PR protocol as two
        // top-level sections.
        let description = format!(
            "{BASH_BASE_DESCRIPTION}\n{BASH_BACKGROUND_USAGE_NOTE}{BASH_COMMIT_AND_PR_INSTRUCTIONS}"
        );
        Self { description }
    }
}

#[derive(Debug, Clone)]
struct BashInput {
    command: String,
    timeout_ms: Option<u64>,
    run_in_background: bool,
    dangerously_disable_sandbox: bool,
    /// What the caller said the command does, echoed back on the result so a
    /// surface can label the call with it. The schema has always advertised
    /// this parameter; reading it is what makes `Bash` treat it the way
    /// `PowerShell` already does instead of discarding it.
    description: Option<String>,
}

#[async_trait]
impl Tool for BashTool {
    fn id(&self) -> ToolId {
        ToolId::new(BASH_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        // `bash` is the name the Anchored Minimal bootstrap request advertises
        // (the DeepSeek Harness Minimal pair); calls come back under it.
        &["BashTool", "bash"]
    }

    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::ToolKind::Shell
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn model_description(&self) -> &str {
        BASH_MODEL_DESCRIPTION
    }

    /// `Bash` and `PowerShell` are peers, and the user picks which
    /// one(s) the model sees. See [`crate::shell_preference`].
    fn is_enabled(&self) -> bool {
        crate::shell_preference::bash_tool_enabled()
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT_MS,
                    "description": "Foreground defaults to 60000ms; background has no deadline when omitted."
                },
                "description": {
                    "type": "string",
                    "description": "Clear, concise description of what this command does in active voice, 5-10 words."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Return a shellId immediately and manage it with ShellOutput/ShellStop."
                },
                "dangerouslyDisableSandbox": { "type": "boolean" }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_destructive(&self, _input: &Value) -> bool {
        true
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        validation_outcome_from(prepare_input(self.id(), input))
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let disables_sandbox = input
            .get("dangerouslyDisableSandbox")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(shell_permission_decision(
            input,
            command,
            context,
            disables_sandbox,
            "Bash",
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let parsed = prepare_input(self.id(), &input)?;

        // --- Sandbox ---
        // One call decides the policy question and builds the argv.
        // Asking `CommandSandbox::check` here as well would duplicate the
        // decision, and two copies of a security decision drift.
        let shell = bash_bin_shell();
        let prepared = match context.command_sandbox() {
            Some(sandbox) => sandbox.prepare(
                &self.id(),
                &parsed.command,
                &parsed.command,
                shell,
                context.cwd(),
                parsed.dangerously_disable_sandbox,
            )?,
            None => PreparedCommand::passthrough(&shell, &parsed.command, context.cwd()),
        };
        let mut cmd = command_sandbox::to_process(&prepared);

        if parsed.run_in_background {
            let registry =
                context
                    .shell_process_registry()
                    .ok_or_else(|| ToolError::Execution {
                        tool: self.id(),
                        source: anyhow::anyhow!("background shell registry is unavailable"),
                    })?;
            let mut result = registry
                .spawn(
                    context,
                    cmd,
                    BASH_TOOL_NAME,
                    parsed.command,
                    parsed.timeout_ms,
                )
                .await?;
            // A backgrounded command's degraded rules are the ones most
            // worth reporting: nobody is watching its output live, so the
            // handover result is the only place the note can land.
            command_sandbox::attach_notices(&mut result, &prepared);
            return Ok(result);
        }

        let mut child = cmd.spawn().map_err(|err| ToolError::Execution {
            tool: self.id(),
            source: err.into(),
        })?;

        let stdout = child.stdout.take().ok_or_else(|| ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!("failed to capture child stdout"),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!("failed to capture child stderr"),
        })?;

        // Cancel-safe readers: the `select!` below drops whichever read loses
        // the race, and a line half-read at that moment has to survive into
        // the next poll rather than disappear.
        let mut stdout_reader = ShellLineReader::new(stdout, ShellOutputEncoding::Utf8);
        let mut stderr_reader = ShellLineReader::new(stderr, ShellOutputEncoding::Utf8);
        let timeout = sleep(Duration::from_millis(
            parsed.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
        ));
        tokio::pin!(timeout);

        // One arrival-ordered log instead of two per-stream buckets. The
        // result still reports `stdout` / `stderr` separately for the model,
        // but keeping the interleaving here is what lets a renderer put
        // cargo's stderr progress lines back ahead of its stdout results.
        let mut output_lines: Vec<(bool, String)> = Vec::new();
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut interrupted = false;

        while stdout_open || stderr_open {
            tokio::select! {
                _ = &mut timeout => {
                    interrupted = true;
                    let _ = child.kill().await;
                    break;
                }
                line = stdout_reader.next_line(), if stdout_open => {
                    match line {
                        Ok(Some(line)) => {
                            context.emit_progress(ToolProgressUpdate::new("stdout").with_message(line.clone()));
                            output_lines.push((false, line));
                        }
                        Ok(None) => stdout_open = false,
                        Err(err) => {
                            return Err(ToolError::Execution {
                                tool: self.id(),
                                source: err.into(),
                            });
                        }
                    }
                }
                line = stderr_reader.next_line(), if stderr_open => {
                    match line {
                        Ok(Some(line)) => {
                            context.emit_progress(ToolProgressUpdate::new("stderr").with_message(line.clone()));
                            output_lines.push((true, line));
                        }
                        Ok(None) => stderr_open = false,
                        Err(err) => {
                            return Err(ToolError::Execution {
                                tool: self.id(),
                                source: err.into(),
                            });
                        }
                    }
                }
            }
        }

        let status = child.wait().await.map_err(|err| ToolError::Execution {
            tool: self.id(),
            source: err.into(),
        })?;

        // Cap each stream so a single large dump doesn't flood the
        // transcript (and every later model request) with output the
        // model rarely needs in full. The live per-line progress above
        // already streamed the untruncated output to the user.
        let max_bytes = tool_output_max_bytes();
        let (stdout, stderr, stream_order) = finish_shell_streams(&output_lines, max_bytes);
        let mut result = json!({
            "stdout": stdout,
            "stderr": stderr,
            "interrupted": interrupted,
            "exitCode": status.code(),
            "timedOut": interrupted,
            "command": parsed.command,
            "dangerouslyDisableSandbox": parsed.dangerously_disable_sandbox,
        });
        if let Some(description) = parsed.description {
            result["description"] = Value::String(description);
        }
        // Present only when the two streams genuinely interleaved and
        // neither was truncated, so the common single-stream command pays
        // nothing for it.
        if let Some(sketch) = stream_order {
            result[rebon_tools_core::shell_stream_order::STREAM_ORDER_KEY] = Value::String(sketch);
        }
        command_sandbox::attach_notices(&mut result, &prepared);
        Ok(result)
    }
}

/// The shell tools' own decision on a command, before any rule or mode.
///
/// Three shapes run without asking: a command that only reads (see
/// `rebon_shell_policy::read_only` — `tool_label` is the tool's name there,
/// so `Monitor`, which starts a watcher rather than a read, never qualifies),
/// a `mkdir` and a deletion inside the roots this session already writes to.
/// A deny rule still wins over all three: the rules broker sees this `Allow`
/// before any mode does and refuses first.
///
/// None of them survives `dangerouslyDisableSandbox`. Asking to leave the
/// sandbox is asking for more than the command's own verb — a read with the
/// sandbox's read restrictions lifted is not the read the allowlist vouched
/// for — so that request keeps its prompt.
pub fn shell_permission_decision(
    input: &Value,
    command: &str,
    context: &ToolContext,
    disables_sandbox: bool,
    tool_label: &str,
) -> PermissionDecision {
    if !disables_sandbox
        && (rebon_shell_policy::read_only::is_read_only_shell_command(tool_label, input)
            || mkdir_targets_are_authorized(command, context)
            || deletion_targets_are_authorized(input, command, context))
    {
        return PermissionDecision::allow(input.clone());
    }

    let message = if command.is_empty() {
        format!("{tool_label} wants to run a command")
    } else {
        format!("{tool_label} wants to run: {command}")
    };
    PermissionDecision::ask(
        PermissionRequest::new("Run shell command", message).with_options([
            "allow_once",
            "allow_always",
            "reject_once",
        ]),
        Some(input.clone()),
    )
}

/// A command that does nothing but delete inside a root this session is
/// already allowed to write to.
///
/// The session is told to put temporary files in its scratchpad, so it
/// has to be able to clear them out again; a cleanup that stops to ask
/// is a cleanup that does not happen, and the files stay in the user's
/// tree. Scope is the same set of roots that authorises writes and
/// `mkdir` — nothing new is being trusted, only the third verb over the
/// same directories.
///
/// Two conditions, and both are narrow on purpose. The command must
/// carry no sensitive class beyond deletion, so `rm <root>/x && git
/// stash drop` is still a stash mutation. And every segment of it must
/// be a deletion whose targets statically resolve inside a root, so
/// `rm <root>/x; curl …` is not authorised by its first half. Target
/// parsing is `rebon_tools_core`'s, the same one the sub-agent broker
/// uses to excuse scratchpad cleanup — one parser, so a shape that is
/// too dynamic to read is refused identically in both places.
fn deletion_targets_are_authorized(input: &Value, command: &str, context: &ToolContext) -> bool {
    if rebon_shell_policy::is_sensitive_beyond_deletions("Bash", input) {
        return false;
    }
    let roots = match context.write_scope_roots() {
        Some(roots) => roots,
        None => context.auto_approved_write_roots(),
    };
    rebon_shell_policy::shell_command_only_deletes_inside_roots(command, roots)
}

fn mkdir_targets_are_authorized(command: &str, context: &ToolContext) -> bool {
    let Some(targets) = parse_simple_mkdir_targets(command) else {
        return false;
    };
    targets.into_iter().all(|target| {
        let resolved = if target.is_absolute() {
            target
        } else if let Some(cwd) = context.cwd() {
            crate::path_scope::resolve_context_path(&target, Path::new(cwd), context)
        } else {
            return false;
        };
        if let Some(roots) = context.write_scope_roots() {
            crate::path_scope::path_is_within_roots(&resolved, roots)
        } else {
            crate::path_scope::path_is_within_roots(&resolved, context.auto_approved_write_roots())
        }
    })
}

fn parse_simple_mkdir_targets(command: &str) -> Option<Vec<PathBuf>> {
    let tokens = tokenize_static_shell_command(command)?;
    if tokens.first().map(String::as_str) != Some("mkdir") {
        return None;
    }

    let mut targets = Vec::new();
    let mut parse_options = true;
    for token in tokens.into_iter().skip(1) {
        if parse_options && token == "--" {
            parse_options = false;
        } else if parse_options && matches!(token.as_str(), "-p" | "--parents") {
        } else if token.is_empty() || (parse_options && token.starts_with('-')) {
            return None;
        } else {
            targets.push(PathBuf::from(token));
        }
    }
    (!targets.is_empty()).then_some(targets)
}

/// The words of `command`, or `None` if any of them would be something other
/// than the literal text written.
///
/// `None` covers every form of expansion as well as every operator: the caller
/// turns these words into paths it is about to create, so a glob or a variable
/// it cannot resolve must not be mistaken for a filename that happens to
/// contain a `*`.
fn tokenize_static_shell_command(command: &str) -> Option<Vec<String>> {
    let mut segments = simple_command_segments(command, &lexer::STATIC_ARGV).ok()?;
    debug_assert_eq!(segments.len(), 1, "STATIC_ARGV never separates segments");
    Some(segments.remove(0))
}

/// Parse and vet one `Bash` request, once.
///
/// The credential refusal comes **before** the parse, and stays there: a
/// command that names a live session's token is refused for naming it,
/// whether or not the rest of the input is well-formed.
fn prepare_input(tool: ToolId, input: &Value) -> ToolResult<BashInput> {
    crate::path_scope::refuse_session_credential_command(tool.clone(), tool.as_str(), input)?;
    let parsed = parse_input(input)?;
    require_valid_input(
        tool,
        validate_parsed_input(&parsed)?,
        "Bash input is invalid",
    )?;
    Ok(parsed)
}

fn parse_input(input: &Value) -> ToolResult<BashInput> {
    let tool = ToolId::new(BASH_TOOL_NAME);
    let object = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: "Bash input must be an object".into(),
        error_code: Some(INVALID_INPUT_CODE),
    })?;

    let command = object
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool.clone(),
            reason: "Bash input requires a non-empty `command` string".into(),
            error_code: Some(INVALID_INPUT_CODE),
        })?
        .to_owned();

    let timeout_ms = match object.get("timeout") {
        Some(Value::Number(raw)) => {
            Some(raw.as_u64().filter(|value| *value >= 1).ok_or_else(|| {
                ToolError::InvalidInput {
                    tool: tool.clone(),
                    reason: "`timeout` must be an integer >= 1".into(),
                    error_code: Some(INVALID_INPUT_CODE),
                }
            })?)
        }
        Some(_) => {
            return Err(ToolError::InvalidInput {
                tool: tool.clone(),
                reason: "`timeout` must be an integer when provided".into(),
                error_code: Some(INVALID_INPUT_CODE),
            })
        }
        None => None,
    };

    Ok(BashInput {
        command,
        timeout_ms,
        run_in_background: optional_bool(
            object.get("run_in_background"),
            "run_in_background",
            &tool,
        )?
        .unwrap_or(false),
        dangerously_disable_sandbox: optional_bool(
            object.get("dangerouslyDisableSandbox"),
            "dangerouslyDisableSandbox",
            &tool,
        )?
        .unwrap_or(false),
        description: match object.get("description") {
            Some(Value::String(raw)) => Some(raw.trim().to_owned()).filter(|raw| !raw.is_empty()),
            Some(Value::Null) | None => None,
            Some(_) => {
                return Err(ToolError::InvalidInput {
                    tool,
                    reason: "`description` must be a string when provided".into(),
                    error_code: Some(INVALID_INPUT_CODE),
                })
            }
        },
    })
}

fn validate_parsed_input(input: &BashInput) -> ToolResult<ValidationOutcome> {
    if input
        .timeout_ms
        .is_some_and(|timeout| timeout > MAX_TIMEOUT_MS)
    {
        return Ok(ValidationOutcome::invalid(
            format!("`timeout` exceeds max supported timeout of {MAX_TIMEOUT_MS}ms"),
            INVALID_INPUT_CODE,
        ));
    }

    Ok(ValidationOutcome::valid())
}

/// Which interpreter [`configured_shell_command`] hands a command to.
///
/// Tools that take a raw command string without being the `Bash` tool
/// (Monitor) describe their syntax from this, so the model is not left
/// guessing between POSIX and PowerShell grammar on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandShell {
    /// `sh -lc` on Unix.
    Posix,
    /// Git-for-Windows `bash -c`.
    GitBash,
    /// `powershell.exe -Command`: Windows with no Git Bash.
    WindowsPowerShell,
}

/// The interpreter [`shell_command`] resolves to on this machine.
pub fn command_shell() -> CommandShell {
    #[cfg(windows)]
    {
        if git_bash_available() {
            CommandShell::GitBash
        } else {
            CommandShell::WindowsPowerShell
        }
    }
    #[cfg(not(windows))]
    {
        CommandShell::Posix
    }
}

/// Resolve the shell binary and arguments for the command.
///
/// On Windows, resolves Git Bash through `find_git_bash`:
///   1. `REBON_GIT_BASH_PATH` env override
///   2. Find `git.exe` → derive `<git_dir>/bin/bash.exe`
///   3. Fallback to `sh` on PATH (Git Bash ships sh.exe; WSL does not)
///   4. Last resort: `powershell.exe`
///
/// On Unix, uses `sh -lc` (login shell).
pub(crate) fn shell_command(command: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        let bash = find_git_bash();
        if !bash.is_empty() {
            return (bash, vec!["-c".into(), command.to_owned()]);
        }
        (
            "powershell.exe".into(),
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                command.to_owned(),
            ],
        )
    }
    #[cfg(not(windows))]
    {
        ("sh".into(), vec!["-lc".into(), command.to_owned()])
    }
}

pub fn configured_shell_command(command: &str, cwd: Option<&str>) -> Command {
    let (program, args) = shell_command(command);
    let mut process = Command::new(program);
    process
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }
    #[cfg(windows)]
    process.creation_flags(CREATE_NO_WINDOW);
    process
}

/// Locate Git-for-Windows bash.exe, cached for the process lifetime.
///
/// Resolution order:
///   1. `REBON_GIT_BASH_PATH` env var (explicit override)
///   2. `git.exe` on PATH → `..\..\bin\bash.exe` relative to its location
///   3. Well-known install dirs (`C:\Program Files\Git`)
///   4. `sh` on PATH (Git ships sh.exe; WSL does not put sh on PATH)
///   5. Empty string = not found (caller falls back to PowerShell)
#[cfg(windows)]
fn find_git_bash() -> String {
    use std::path::Path;
    use std::sync::OnceLock;

    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            // 1. Explicit override.
            if let Ok(p) = std::env::var("REBON_GIT_BASH_PATH") {
                if Path::new(&p).exists() {
                    return p;
                }
            }

            // 2. Derive from `git.exe` location. Walked in-process rather
            //    than through `where.exe`: this runs on the first tool
            //    snapshot of every process, and the spawn cost 100–300 ms.
            for git_path in crate::path_lookup::executables_on_path("git") {
                // git.exe is typically in <install>/cmd/git.exe →
                // bash.exe lives in <install>/bin/bash.exe
                if let Some(cmd_dir) = git_path.parent() {
                    let bash = cmd_dir.join("..").join("bin").join("bash.exe");
                    if bash.exists() {
                        return bash.to_string_lossy().into_owned();
                    }
                    // Some layouts: <install>/mingw64/bin/git.exe
                    let bash = cmd_dir.join("..").join("..").join("bin").join("bash.exe");
                    if bash.exists() {
                        return bash.to_string_lossy().into_owned();
                    }
                }
            }

            // 3. Well-known install directories.
            let well_known = [
                r"C:\Program Files\Git\bin\bash.exe",
                r"C:\Program Files (x86)\Git\bin\bash.exe",
            ];
            for p in well_known {
                if Path::new(p).exists() {
                    return p.to_string();
                }
            }

            // 4. `sh` on PATH (Git Bash ships sh.exe; WSL does not).
            if !crate::path_lookup::executables_on_path("sh").is_empty() {
                return "sh".into();
            }

            String::new()
        })
        .clone()
}

/// Whether a real POSIX shell is reachable on this Windows box.
///
/// `shell_command` falls back to `powershell.exe` when Git Bash is missing,
/// which makes `Bash` a tool whose description documents a shell it is not
/// running. [`crate::shell_preference`] uses this to hide `Bash` in that case
/// and let the real `PowerShell` tool take over.
#[cfg(windows)]
pub(crate) fn git_bash_available() -> bool {
    !find_git_bash().is_empty()
}

/// `CREATE_NO_WINDOW` — prevents a visible console on Windows.
/// Preserves the `windowsHide: true` spawn option.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The shell Bash hands its script to, as a value the sandbox can
/// build an argv around.
///
/// Splitting this out of [`configured_shell_command`] is what makes a
/// Bash command wrappable at all: the wrapper needs the shell and the
/// script as *separate* values, because on Windows it constructs an
/// argv rather than a command string. Deriving it from
/// [`shell_command`] rather than restating `sh -lc` keeps the Git
/// Bash resolution and its PowerShell fallback in one place.
pub(crate) fn bash_bin_shell() -> BinShell {
    let (program, mut args) = shell_command("");
    // `shell_command` returns argv with the command already appended;
    // a `BinShell` is only the prefix that precedes it.
    args.pop();
    BinShell::new(program, args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn tool() -> BashTool {
        BashTool::new()
    }

    #[cfg(windows)]
    fn has_bash() -> bool {
        !find_git_bash().is_empty()
    }

    fn echo_command() -> &'static str {
        #[cfg(windows)]
        {
            if has_bash() {
                return "printf 'hello\\n'";
            }
            return "Write-Output hello";
        }
        #[cfg(not(windows))]
        {
            "printf 'hello\n'"
        }
    }

    fn timeout_command() -> &'static str {
        #[cfg(windows)]
        {
            if has_bash() {
                return "sleep 1";
            }
            return "Start-Sleep -Milliseconds 300";
        }
        #[cfg(not(windows))]
        {
            "sleep 1"
        }
    }

    fn progress_command() -> &'static str {
        #[cfg(windows)]
        {
            if has_bash() {
                return "printf 'first\\nsecond\\n'";
            }
            return "Write-Output first; Write-Output second";
        }
        #[cfg(not(windows))]
        {
            "printf 'first\nsecond\n'"
        }
    }

    #[tokio::test]
    async fn check_permissions_allows_mkdir_inside_auto_approved_root() {
        let temp = tempfile::tempdir().unwrap();
        let scratchpad = temp.path().join("scratchpad");
        std::fs::create_dir(&scratchpad).unwrap();
        let target = scratchpad.join("nested dir");
        let input = json!({ "command": format!("mkdir -p \"{}\"", target.display()) });
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);

        let decision = tool().check_permissions(&input, &context).await.unwrap();

        assert_eq!(decision, PermissionDecision::allow(input));
    }

    #[tokio::test]
    async fn check_permissions_allows_relative_mkdir_inside_write_scope() {
        let temp = tempfile::tempdir().unwrap();
        let scratchpad = temp.path().join("scratchpad");
        std::fs::create_dir(&scratchpad).unwrap();
        let input = json!({ "command": "mkdir -p nested/deeper" });
        let context = ToolContext::new()
            .with_cwd(scratchpad.to_string_lossy())
            .with_write_scope_roots([scratchpad]);

        let decision = tool().check_permissions(&input, &context).await.unwrap();

        assert_eq!(decision, PermissionDecision::allow(input));
    }

    #[tokio::test]
    async fn check_permissions_allows_multiple_mkdir_targets_inside_root() {
        let temp = tempfile::tempdir().unwrap();
        let scratchpad = temp.path().join("scratchpad");
        std::fs::create_dir(&scratchpad).unwrap();
        let first = scratchpad.join("first");
        let second = scratchpad.join("second");
        let input = json!({
            "command": format!("mkdir -p \"{}\" \"{}\"", first.display(), second.display())
        });
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);

        let decision = tool().check_permissions(&input, &context).await.unwrap();

        assert_eq!(decision, PermissionDecision::allow(input));
    }

    #[tokio::test]
    async fn check_permissions_asks_for_mkdir_outside_authorized_root() {
        let temp = tempfile::tempdir().unwrap();
        let scratchpad = temp.path().join("scratchpad");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&scratchpad).unwrap();
        let input = json!({ "command": format!("mkdir -p \"{}\"", outside.display()) });
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);

        let decision = tool().check_permissions(&input, &context).await.unwrap();

        assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
    }

    #[tokio::test]
    async fn check_permissions_asks_when_any_mkdir_target_is_outside_root() {
        let temp = tempfile::tempdir().unwrap();
        let scratchpad = temp.path().join("scratchpad");
        std::fs::create_dir(&scratchpad).unwrap();
        let inside = scratchpad.join("inside");
        let outside = temp.path().join("outside");
        let input = json!({
            "command": format!("mkdir -p \"{}\" \"{}\"", inside.display(), outside.display())
        });
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);

        let decision = tool().check_permissions(&input, &context).await.unwrap();

        assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
    }

    #[tokio::test]
    async fn check_permissions_uses_explicit_write_scope_as_authoritative() {
        let temp = tempfile::tempdir().unwrap();
        let broad_root = temp.path().join("broad");
        let narrow_root = broad_root.join("scratchpad");
        std::fs::create_dir_all(&narrow_root).unwrap();
        let target = broad_root.join("outside-scratchpad");
        let input = json!({ "command": format!("mkdir -p \"{}\"", target.display()) });
        let context = ToolContext::new()
            .with_auto_approved_write_roots([broad_root])
            .with_write_scope_roots([narrow_root]);

        let decision = tool().check_permissions(&input, &context).await.unwrap();

        assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
    }

    #[tokio::test]
    async fn check_permissions_asks_for_compound_or_dynamic_mkdir_commands() {
        let temp = tempfile::tempdir().unwrap();
        let scratchpad = temp.path().join("scratchpad");
        std::fs::create_dir(&scratchpad).unwrap();
        let target = scratchpad.join("nested");
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);
        let commands = [
            format!("mkdir -p \"{}\" && echo done", target.display()),
            format!("mkdir -p \"{}\"\necho done", target.display()),
            format!("mkdir -p \"$ROOT/nested\""),
            format!("mkdir -m 700 \"{}\"", target.display()),
        ];

        for command in commands {
            let input = json!({ "command": command });
            let decision = tool().check_permissions(&input, &context).await.unwrap();
            assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
        }
    }

    /// A scratchpad root that exists on disk, next to a sibling that is
    /// not the scratchpad. Deletion confinement canonicalises, so the
    /// root has to be real.
    fn scratchpad_and_outside() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let scratchpad = temp.path().join("scratchpad");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&scratchpad).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        (temp, scratchpad, outside)
    }

    #[tokio::test]
    async fn check_permissions_allows_rm_inside_auto_approved_root() {
        let (_temp, scratchpad, _outside) = scratchpad_and_outside();
        let target = scratchpad.join("bundle");
        let input = json!({ "command": format!("rm -rf \"{}\"", target.display()) });
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);

        let decision = tool().check_permissions(&input, &context).await.unwrap();

        assert_eq!(decision, PermissionDecision::allow(input));
    }

    #[tokio::test]
    async fn check_permissions_allows_powershell_remove_item_inside_auto_approved_root() {
        let (_temp, scratchpad, _outside) = scratchpad_and_outside();
        let target = scratchpad.join("bundle");
        let input = json!({
            "command": format!("Remove-Item -Recurse -Force \"{}\"", target.display())
        });
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);

        let decision = tool().check_permissions(&input, &context).await.unwrap();

        assert_eq!(decision, PermissionDecision::allow(input));
    }

    #[tokio::test]
    async fn check_permissions_asks_for_deletion_inside_the_project() {
        let (_temp, scratchpad, outside) = scratchpad_and_outside();
        let target = outside.join("src.rs");
        let input = json!({ "command": format!("rm -rf \"{}\"", target.display()) });
        // The project is where the user's work is; the scratchpad carve-out
        // is precisely the promise that it does not reach in here.
        let context = ToolContext::new()
            .with_cwd(outside.to_string_lossy())
            .with_auto_approved_write_roots([scratchpad]);

        let decision = tool().check_permissions(&input, &context).await.unwrap();

        assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
    }

    #[tokio::test]
    async fn check_permissions_asks_for_traversal_wildcard_and_dynamic_deletion_targets() {
        let (_temp, scratchpad, _outside) = scratchpad_and_outside();
        let dir = scratchpad.display().to_string();
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);
        let commands = [
            format!("rm -rf \"{dir}/../sibling\""),
            format!("rm -rf {dir}/nested/*"),
            "rm -rf $SCRATCH/x".to_string(),
            format!("rm -rf {dir}/{{a,b}}"),
            // A relative target resolves against a cwd this check does not
            // consult, so it never qualifies.
            "rm -rf bundle".to_string(),
        ];

        for command in commands {
            let input = json!({ "command": command.clone() });
            let decision = tool().check_permissions(&input, &context).await.unwrap();
            assert_eq!(
                decision.behavior,
                rebon_tools_core::PermissionBehavior::Ask,
                "expected an ask for {command:?}"
            );
        }
    }

    #[tokio::test]
    async fn check_permissions_asks_when_a_deletion_is_chained_with_anything_else() {
        let (_temp, scratchpad, _outside) = scratchpad_and_outside();
        let target = scratchpad.join("bundle");
        let path = target.display().to_string();
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);
        let commands = [
            format!("rm -rf \"{path}\" && echo done"),
            format!("rm -rf \"{path}\"; curl http://example.invalid"),
            // Another sensitive class riding in on the deletion.
            format!("rm -rf \"{path}\" && git stash drop"),
        ];

        for command in commands {
            let input = json!({ "command": command.clone() });
            let decision = tool().check_permissions(&input, &context).await.unwrap();
            assert_eq!(
                decision.behavior,
                rebon_tools_core::PermissionBehavior::Ask,
                "expected an ask for {command:?}"
            );
        }
    }

    #[tokio::test]
    async fn check_permissions_asks_when_a_deletion_target_escapes_through_a_link() {
        let (_temp, scratchpad, outside) = scratchpad_and_outside();
        let link = scratchpad.join("link-outside");
        #[cfg(unix)]
        let link_created = std::os::unix::fs::symlink(&outside, &link).is_ok();
        #[cfg(windows)]
        let link_created = std::os::windows::fs::symlink_dir(&outside, &link).is_ok();

        // Creating a link needs a privilege the runner may not have; when
        // it does not, there is nothing to assert about.
        if !link_created {
            return;
        }
        let input = json!({
            "command": format!("rm -rf \"{}/payload\"", link.display())
        });
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);

        let decision = tool().check_permissions(&input, &context).await.unwrap();

        assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn check_permissions_allows_deletion_written_with_backslashes_or_other_case() {
        let (_temp, scratchpad, _outside) = scratchpad_and_outside();
        let forward = scratchpad.display().to_string().replace('\\', "/");
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);
        let commands = [
            format!(
                "Remove-Item -Recurse -Force {}\\bundle",
                forward.replace('/', "\\")
            ),
            format!("rm -rf {}/bundle", forward.to_ascii_lowercase()),
        ];

        for command in commands {
            let input = json!({ "command": command.clone() });
            let decision = tool().check_permissions(&input, &context).await.unwrap();
            assert_eq!(
                decision,
                PermissionDecision::allow(input),
                "expected an allow for {command:?}"
            );
        }
    }

    #[tokio::test]
    async fn check_permissions_asks_when_deletion_disables_sandbox() {
        let (_temp, scratchpad, _outside) = scratchpad_and_outside();
        let target = scratchpad.join("bundle");
        let input = json!({
            "command": format!("rm -rf \"{}\"", target.display()),
            "dangerouslyDisableSandbox": true
        });
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);

        let decision = tool().check_permissions(&input, &context).await.unwrap();

        assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
    }

    #[tokio::test]
    async fn check_permissions_asks_when_mkdir_disables_sandbox() {
        let temp = tempfile::tempdir().unwrap();
        let scratchpad = temp.path().join("scratchpad");
        std::fs::create_dir(&scratchpad).unwrap();
        let target = scratchpad.join("nested");
        let input = json!({
            "command": format!("mkdir -p \"{}\"", target.display()),
            "dangerouslyDisableSandbox": true
        });
        let context = ToolContext::new().with_auto_approved_write_roots([scratchpad]);

        let decision = tool().check_permissions(&input, &context).await.unwrap();

        assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
    }

    /// Exploring is reading, and a prompt per `ls` is what made plan mode
    /// expensive. The tool's own decision allows a read; whatever mode is on
    /// sees an `Allow` and never gets to ask.
    #[tokio::test]
    async fn check_permissions_allows_a_command_that_only_reads() {
        for command in [
            "git status",
            "git log --oneline -5 | head -3",
            "ls -la && pwd",
            "rg -n TODO crates",
        ] {
            let input = json!({ "command": command });
            let decision = tool()
                .check_permissions(&input, &ToolContext::new())
                .await
                .unwrap();
            assert_eq!(decision, PermissionDecision::allow(input), "{command:?}");
        }
    }

    #[tokio::test]
    async fn check_permissions_asks_for_a_read_that_writes_or_expands() {
        for command in [
            "git log > log.txt",
            "cat $(which rebon)",
            "ls && rm -rf target",
            "find . -delete",
            "git commit -m x",
        ] {
            let input = json!({ "command": command });
            let decision = tool()
                .check_permissions(&input, &ToolContext::new())
                .await
                .unwrap();
            assert_eq!(
                decision.behavior,
                rebon_tools_core::PermissionBehavior::Ask,
                "{command:?}"
            );
        }
    }

    /// Leaving the sandbox is more than the read the allowlist vouched for.
    #[tokio::test]
    async fn check_permissions_asks_when_a_read_disables_sandbox() {
        let input = json!({ "command": "git status", "dangerouslyDisableSandbox": true });

        let decision = tool()
            .check_permissions(&input, &ToolContext::new())
            .await
            .unwrap();

        assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
    }

    /// `Monitor` starts a watcher that outlives the call; the read-only
    /// judgment is about a command that runs and ends.
    #[test]
    fn a_monitor_command_is_never_judged_read_only() {
        let input = json!({ "command": "tail -f log.txt" });

        let decision = shell_permission_decision(
            &input,
            "tail -f log.txt",
            &ToolContext::new(),
            false,
            crate::monitor::MONITOR_TOOL_NAME,
        );

        assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
    }

    #[tokio::test]
    async fn validate_input_accepts_background_mode() {
        let result = tool()
            .validate_input(
                &json!({ "command": "echo hi", "run_in_background": true }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert!(result.is_valid());
    }

    /// Models often send a `description` with shell calls; PowerShell already
    /// accepts it, so Bash must too rather than failing the call.
    #[test]
    fn schema_accepts_command_description_like_powershell() {
        let input = json!({ "command": "echo hi", "description": "Print a greeting" });
        crate::validation::validate_schema("Bash", &tool().input_schema(), &input).unwrap();
        assert_eq!(
            tool().input_schema()["properties"]["description"],
            crate::powershell::PowerShellTool.input_schema()["properties"]["description"]
        );
    }

    #[tokio::test]
    async fn call_runs_background_command_and_returns_shell_id() {
        // Shared with every other spawning test and exclusive against
        // the `PATH`-blanking ones in `read.rs`: without it this test
        // fails with `os error 2` whenever it overlaps one of them.
        let _env = crate::test_env::hold_env();
        let registry = Arc::new(crate::ShellProcessRegistry::new());
        let context = ToolContext::new()
            .with_session_id("bash-background-test")
            .with_shell_process_registry(registry);
        let started = tool()
            .call(
                json!({
                    "command": progress_command(),
                    "run_in_background": true
                }),
                &context,
            )
            .await
            .unwrap();
        let shell_id = started["shellId"].as_str().unwrap().to_owned();
        assert_eq!(started["timeoutMs"], Value::Null);

        let mut cursor = 0;
        let mut output = String::new();
        loop {
            let update = crate::ShellOutputTool
                .call(
                    json!({
                        "shellId": shell_id,
                        "cursor": cursor,
                        "wait": true,
                        "timeout": 5_000
                    }),
                    &context,
                )
                .await
                .unwrap();
            output.push_str(update["output"].as_str().unwrap_or(""));
            cursor = update["nextCursor"].as_u64().unwrap();
            if update["completed"] == true {
                break;
            }
        }

        assert!(output.contains("first"));
        assert!(output.contains("second"));
    }

    #[tokio::test]
    async fn call_executes_command_and_captures_stdout() {
        let _env = crate::test_env::hold_env();
        let out = tool()
            .call(json!({ "command": echo_command() }), &ToolContext::new())
            .await
            .unwrap();

        assert_eq!(out["stdout"], json!("hello"));
        assert_eq!(out["interrupted"], json!(false));
    }

    #[tokio::test]
    async fn call_echoes_the_callers_description() {
        // The schema has always advertised `description`; until this was
        // wired, `Bash` accepted it and threw it away while `PowerShell`
        // echoed it, so the same call labelled itself on one shell and not
        // the other.
        let _env = crate::test_env::hold_env();
        let out = tool()
            .call(
                json!({ "command": echo_command(), "description": "  Say hello  " }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["description"], json!("Say hello"));
    }

    #[tokio::test]
    async fn call_omits_the_description_key_when_none_was_given() {
        let _env = crate::test_env::hold_env();
        let out = tool()
            .call(json!({ "command": echo_command() }), &ToolContext::new())
            .await
            .unwrap();

        assert!(out.get("description").is_none());
    }

    #[tokio::test]
    async fn a_non_string_description_is_refused() {
        let _env = crate::test_env::hold_env();
        let err = tool()
            .call(
                json!({ "command": echo_command(), "description": 7 }),
                &ToolContext::new(),
            )
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput { reason, .. } => {
                assert!(reason.contains("`description`"), "{reason}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_emits_progress_lines() {
        let _env = crate::test_env::hold_env();
        let (context, mut rx) = ToolContext::new().with_progress("bash-1");
        let out = tool()
            .call(json!({ "command": progress_command() }), &context)
            .await
            .unwrap();

        let first = rx.recv().await.expect("expected progress");
        assert!(first.kind == "stdout" || first.kind == "stderr");
        assert!(out["stdout"].as_str().unwrap().contains("first"));
    }

    #[tokio::test]
    async fn call_times_out_and_marks_interrupted() {
        let _env = crate::test_env::hold_env();
        let out = tool()
            .call(
                json!({ "command": timeout_command(), "timeout": 50 }),
                &ToolContext::new(),
            )
            .await
            .unwrap();

        assert_eq!(out["interrupted"], json!(true));
        assert_eq!(out["timedOut"], json!(true));
    }

    // -----------------------------------------------------------------
    // Description content (commit/PR + background) coverage
    // -----------------------------------------------------------------

    #[test]
    fn description_contains_base_guidance() {
        let desc = BashTool::new().description;
        assert!(desc.starts_with("Executes a given bash command"));
        assert!(desc.contains("File search: Use Glob"));
        assert!(desc.contains("Content search: Use Grep"));
    }

    #[test]
    fn description_contains_background_usage_note() {
        let desc = BashTool::new().description;
        assert!(desc.contains("run_in_background"));
        assert!(desc.contains("returns a `shellId` immediately"));
        assert!(desc.contains("without appending '&'"));
        assert!(desc.contains("ShellOutput"));
        assert!(desc.contains("ShellStop"));
    }

    #[test]
    fn description_contains_commit_protocol_headers() {
        let desc = BashTool::new().description;
        assert!(desc.contains("# Committing changes with git"));
        assert!(desc.contains("Git Safety Protocol:"));
        assert!(desc.contains("# Creating pull requests"));
        assert!(desc.contains("# Other common operations"));
    }

    #[test]
    fn description_contains_destructive_git_prohibitions() {
        let desc = BashTool::new().description;
        // Rules that would cause real damage if the model ignored them.
        assert!(desc.contains("NEVER update the git config"));
        assert!(desc.contains("NEVER run destructive git commands"));
        assert!(desc.contains("NEVER skip hooks"));
        assert!(desc.contains("NEVER run force push to main/master"));
    }

    #[test]
    fn description_contains_commit_amend_warning() {
        let desc = BashTool::new().description;
        // The "pre-commit hook failed → --amend would destroy previous
        // work" framing is what stops the model from over-amending.
        assert!(desc.contains("Always create NEW commits rather than amending"));
        assert!(desc.contains("pre-commit hook fails, the commit did NOT happen"));
        assert!(desc.contains("create a NEW commit"));
    }

    #[test]
    fn description_contains_heredoc_commit_example() {
        let desc = BashTool::new().description;
        // The HEREDOC format example is the thing that makes the model
        // stop producing malformed commit messages.
        assert!(desc.contains("ALWAYS pass the commit message via a HEREDOC"));
        assert!(desc.contains("git commit -m \"$(cat <<'EOF'"));
    }

    #[test]
    fn description_contains_pr_creation_format() {
        let desc = BashTool::new().description;
        assert!(desc.contains("gh pr create --title"));
        assert!(desc.contains("## Summary"));
        assert!(desc.contains("## Test plan"));
        assert!(desc.contains("Keep the PR title short (under 70 characters)"));
    }

    #[test]
    fn description_forbids_interactive_git_flags() {
        let desc = BashTool::new().description;
        assert!(desc.contains("Never use git commands with the -i flag"));
        assert!(desc.contains("Do not use --no-edit with git rebase"));
    }

    #[test]
    fn description_does_not_contain_anthropic_attribution_placeholders() {
        let desc = BashTool::new().description;
        assert!(!desc.contains("Co-Authored-By: Rebon"));
        assert!(!desc.contains("Generated with [Rebon]"));
    }

    #[test]
    fn description_is_stable_across_instances() {
        let a = BashTool::new().description;
        let b = BashTool::new().description;
        assert_eq!(a, b);
    }

    // ── The `CommandSandbox` seam ─────────────────────────────────
    //
    // Which commands a real sandbox confines is the sandbox plugin's own
    // decision table, tested there. What has to be true *here* is that the
    // seam is wired at all: that a context without one spawns the argv this
    // tool built, that a refusal stops the process from starting, and that
    // everything a preparation produced — environment, cwd, notices —
    // survives the trip to the spawn and to the result.

    /// A sandbox with no operating system behind it.
    #[derive(Debug)]
    struct FakeSandbox {
        refusal: Option<&'static str>,
        env_set: Vec<(String, String)>,
        env_unset: Vec<String>,
        cwd: Option<PathBuf>,
        notices: Vec<String>,
    }

    impl FakeSandbox {
        fn refusing(reason: &'static str) -> Self {
            Self {
                refusal: Some(reason),
                ..Self::wrapping()
            }
        }

        fn wrapping() -> Self {
            Self {
                refusal: None,
                env_set: Vec::new(),
                env_unset: Vec::new(),
                cwd: None,
                notices: Vec::new(),
            }
        }
    }

    impl crate::command_sandbox::CommandSandbox for FakeSandbox {
        fn check(&self, tool: &ToolId, _command: &str, _disable: bool) -> ToolResult<()> {
            match self.refusal {
                Some(reason) => Err(ToolError::PermissionDenied {
                    tool: tool.clone(),
                    reason: reason.into(),
                }),
                None => Ok(()),
            }
        }

        fn will_wrap(&self, _command: &str, _disable: bool) -> bool {
            self.refusal.is_none()
        }

        fn prepare(
            &self,
            tool: &ToolId,
            _policy_command: &str,
            payload: &str,
            shell: BinShell,
            cwd: Option<&str>,
            _disable: bool,
        ) -> ToolResult<PreparedCommand> {
            if let Some(reason) = self.refusal {
                return Err(ToolError::PermissionDenied {
                    tool: tool.clone(),
                    reason: reason.into(),
                });
            }
            let mut prepared = PreparedCommand::passthrough(&shell, payload, cwd);
            prepared.env_set = self.env_set.clone();
            prepared.env_unset = self.env_unset.clone();
            prepared.cwd = self.cwd.clone().or(prepared.cwd);
            prepared.notices = self.notices.clone();
            prepared.confined = true;
            prepared.backend = "fake";
            Ok(prepared)
        }
    }

    /// No sandbox on the context is the ordinary case, and it must be the
    /// same process a build without the plugin spawns.
    #[tokio::test]
    async fn without_a_sandbox_the_tool_spawns_the_argv_it_built() {
        let _env = crate::test_env::hold_env();
        let out = tool()
            .call(json!({ "command": echo_command() }), &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(out["stdout"], json!("hello"));

        let shell = bash_bin_shell();
        let prepared = PreparedCommand::passthrough(&shell, echo_command(), None);
        assert_eq!(prepared.program, shell.program);
        assert_eq!(prepared.args, shell.argv(echo_command())[1..]);
    }

    /// A refusal has to stop the process from *starting*, not report a
    /// failure after the fact.
    #[tokio::test]
    async fn a_refusing_sandbox_stops_the_command_before_it_spawns() {
        let context = ToolContext::new()
            .with_command_sandbox(Arc::new(FakeSandbox::refusing("nope, not today")));

        let error = tool()
            .call(json!({ "command": echo_command() }), &context)
            .await
            .expect_err("a refusing sandbox must not spawn anything");

        match error {
            ToolError::PermissionDenied { tool, reason } => {
                assert_eq!(tool.as_str(), "Bash");
                assert_eq!(reason, "nope, not today");
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
    }

    /// Everything the preparation decided reaches the spawn.
    #[test]
    fn the_preparation_environment_and_cwd_reach_the_process() {
        let temp = tempfile::tempdir().unwrap();
        let mut sandbox = FakeSandbox::wrapping();
        sandbox.env_set = vec![("FOO".into(), "bar".into())];
        sandbox.env_unset = vec!["BAZ".into()];
        sandbox.cwd = Some(temp.path().to_path_buf());

        let prepared = crate::command_sandbox::CommandSandbox::prepare(
            &sandbox,
            &ToolId::new("Bash"),
            "echo hi",
            "echo hi",
            BinShell::posix(),
            None,
            false,
        )
        .unwrap();

        let process = command_sandbox::to_process(&prepared);
        let std_command = process.as_std();
        assert_eq!(std_command.get_program(), "sh");
        let envs: Vec<_> = std_command.get_envs().collect();
        assert!(envs
            .iter()
            .any(|(key, value)| *key == "FOO" && *value == Some("bar".as_ref())));
        assert!(envs
            .iter()
            .any(|(key, value)| *key == "BAZ" && value.is_none()));
        assert_eq!(std_command.get_current_dir(), Some(temp.path()));
    }

    /// A degraded rule reaches the tool *result*, where the model reads it.
    #[tokio::test]
    async fn notices_from_the_preparation_reach_the_tool_result() {
        let _env = crate::test_env::hold_env();
        let mut sandbox = FakeSandbox::wrapping();
        sandbox.notices = vec!["the network was cut".to_string()];
        let context = ToolContext::new().with_command_sandbox(Arc::new(sandbox));

        let out = tool()
            .call(json!({ "command": echo_command() }), &context)
            .await
            .unwrap();

        let notes = out[command_sandbox::SANDBOX_NOTES_KEY]
            .as_array()
            .unwrap_or_else(|| panic!("the notice never reached the result: {out}"));
        assert_eq!(notes[0], json!("the network was cut"));
    }

    /// And a clean command carries no field at all, so the model has nothing
    /// extra to read past.
    #[tokio::test]
    async fn a_clean_command_carries_no_notices_field() {
        let _env = crate::test_env::hold_env();
        let context = ToolContext::new().with_command_sandbox(Arc::new(FakeSandbox::wrapping()));

        let out = tool()
            .call(json!({ "command": echo_command() }), &context)
            .await
            .unwrap();

        assert!(
            out.get(command_sandbox::SANDBOX_NOTES_KEY).is_none(),
            "{out}"
        );
    }

    /// Attaching to something that is not an object is a no-op rather than a
    /// panic: a tool result's shape is not this seam's to assume.
    #[test]
    fn attaching_notices_to_a_non_object_result_is_a_no_op() {
        let mut sandbox = FakeSandbox::wrapping();
        sandbox.notices = vec!["something".to_string()];
        let prepared = crate::command_sandbox::CommandSandbox::prepare(
            &sandbox,
            &ToolId::new("Bash"),
            "echo hi",
            "echo hi",
            BinShell::posix(),
            None,
            false,
        )
        .unwrap();

        let mut result = json!("not an object");
        command_sandbox::attach_notices(&mut result, &prepared);
        assert_eq!(result, json!("not an object"));
    }
}
