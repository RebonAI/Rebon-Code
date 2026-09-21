//! Static shell and git classification for permission decisions.
//!
//! Everything here answers one question about a command string without
//! running it: is this call sensitive enough that auto mode has to stop and
//! ask? The answers gate `Bash` and `PowerShell` in every surface, so a
//! predicate that gets more permissive is a security regression while one
//! that gets more conservative is only a extra prompt.
//!
//! The crate owns the workspace's only shell tokenisers ([`lexer`]). Nothing
//! else may hand-roll one: a second lexer is a second set of
//! quoting rules, and the gap between them is where an evasion lives.

pub mod lexer;
pub mod powershell_shape;

use crate::lexer::{shell_tokens, workflow_push_ansi_c_escape, workflow_shell_tokens};
use rebon_tools_core::{canonicalize_scope_path, scope_path_starts_with};
use serde_json::Value;

fn shell_command_input<'a>(tool_name: &str, input: &'a Value) -> Option<&'a str> {
    if !matches!(
        tool_name,
        "Bash" | "BashTool" | "PowerShell" | "PowerShellTool"
    ) {
        return None;
    }
    input.get("command").and_then(Value::as_str)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoModeInputDisposition {
    NoSpecialRisk,
    EmbeddedScriptReview(EmbeddedInterpreterScript),
    HardSensitive(HardSensitiveReason),
}

/// Closed set of reasons the deterministic layer refuses to auto-run a
/// command. Deliberately an enum rather than free text: these strings
/// reach the model (and `/permissions`) as denial reasons, so they must
/// stay stable, reviewable, and free of untrusted content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardSensitiveReason {
    /// Deletes, trashes, or truncates files or directories.
    FileDeletion,
    /// Destroys git stash state (`git stash drop` / `clear` / `pop`).
    GitStashMutation,
    /// Removes or prunes git worktrees.
    GitWorktreeMutation,
    /// Overwrites files with content from git history — a disguised
    /// restore or reset.
    GitContentRestore,
    /// Interpreter code (inline `-c` / heredoc body) deletes files.
    InterpreterFileDeletion,
    /// Launches a script file or encoded command whose contents are not
    /// visible in the invocation.
    ScriptFileExecution,
    /// The interpreter invocation cannot be statically analyzed
    /// (dynamic interpreter, malformed heredoc, oversized body, …).
    UnanalyzableScriptInvocation,
    /// Names a file carrying a live session's loopback endpoint and token —
    /// `<config>/jobs/**/state.json` or `**/*.owner.json`.
    SessionCredentialAccess,
}

impl HardSensitiveReason {
    /// One-line, model-facing description of the flagged behavior.
    pub fn describe(self) -> &'static str {
        match self {
            Self::FileDeletion => "the command deletes, trashes, or truncates files",
            Self::GitStashMutation => "the command destroys git stash state",
            Self::GitWorktreeMutation => "the command removes a git worktree",
            Self::GitContentRestore => "the command overwrites files with content from git history",
            Self::InterpreterFileDeletion => "the embedded interpreter code deletes files",
            Self::ScriptFileExecution => {
                "the command executes script content that is not visible in the invocation"
            }
            Self::UnanalyzableScriptInvocation => {
                "the interpreter invocation cannot be statically analyzed"
            }
            Self::SessionCredentialAccess => {
                "the command names a file holding a live session's endpoint and token"
            }
        }
    }
}

/// Does this command name one of the files that command a live session?
///
/// The path-taking file tools refuse these paths outright, but a shell command
/// is opaque to that check — `cat`, `type`, `Get-Content` and a
/// hundred other spellings all read a file without ever presenting a path
/// parameter. So the shell gets the weaker guarantee that fits it: not a
/// refusal, but a stop for the user to look at, which is what
/// [`AutoModeInputDisposition::HardSensitive`] means.
///
/// Deliberately a text match rather than a resolved path. This crate is kept
/// dependency-light and does not know where the config home is, and a command
/// line is not a path anyway — it may hold globs, variables, or a `cd` two
/// commands earlier. The trade is the right way round: a false positive costs
/// one confirmation on a command that mentions `state.json` next to `jobs/`,
/// while a miss hands over a session.
pub fn is_session_credential_access_command(tool_name: &str, input: &Value) -> bool {
    let Some(command) = shell_command_input(tool_name, input) else {
        return false;
    };
    let command = command.to_ascii_lowercase();
    if command.contains(".owner.json") {
        return true;
    }
    // `state.json` on its own is an ordinary name in plenty of projects, so it
    // counts only beside something that says whose it is.
    command.contains("state.json")
        && (command.contains(".rebon")
            || command.contains("jobs/")
            || command.contains("jobs\\")
            || command.contains("rebon_config_dir")
            || command.contains("rebon-config-dir"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddedInterpreter {
    Python,
    Node,
    Bash,
    PowerShell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedInterpreterScript {
    pub interpreter: EmbeddedInterpreter,
    pub invocation: String,
    pub source: String,
}

/// Deterministic risk floor for a script body on its way to the
/// auto-mode classifier: re-classify the body as if it had been typed
/// as a shell command of its own.
///
/// Only shell bodies are checked. Python and Node bodies are already
/// covered upstream — [`classify_auto_mode_tool_input`] only yields
/// `EmbeddedScriptReview` after `interpreter_code_destroys_files` has
/// cleared the body, so re-running that check here would always answer
/// `false`. Shell bodies get no such upstream check, which is exactly
/// what this supplies for them.
pub fn embedded_interpreter_script_hard_sensitive_effects(
    script: &EmbeddedInterpreterScript,
) -> Option<HardSensitiveReason> {
    let tool_name = match script.interpreter {
        EmbeddedInterpreter::Python | EmbeddedInterpreter::Node => return None,
        EmbeddedInterpreter::Bash => "Bash",
        EmbeddedInterpreter::PowerShell => "PowerShell",
    };
    match classify_auto_mode_tool_input(tool_name, &serde_json::json!({ "command": script.source }))
    {
        AutoModeInputDisposition::HardSensitive(reason) => Some(reason),
        _ => None,
    }
}

pub fn classify_auto_mode_tool_input(tool_name: &str, input: &Value) -> AutoModeInputDisposition {
    classify_auto_mode_tool_input_with_options(tool_name, input, None, false)
}

pub fn classify_auto_mode_tool_input_for_continuity(
    tool_name: &str,
    input: &Value,
) -> AutoModeInputDisposition {
    classify_auto_mode_tool_input_with_options(tool_name, input, None, true)
}

/// Variant of [`classify_auto_mode_tool_input`] that stops treating
/// file deletion as sensitive when every deletion target statically
/// resolves to a path confined to `exempt_deletion_root` (typically the session
/// scratchpad directory). Every other sensitivity class is unaffected:
/// a command that deletes inside the root but also runs `git stash
/// drop` is still hard-sensitive.
pub fn classify_auto_mode_tool_input_with_exempt_deletion_root(
    tool_name: &str,
    input: &Value,
    exempt_deletion_root: Option<&std::path::Path>,
) -> AutoModeInputDisposition {
    classify_auto_mode_tool_input_with_options(tool_name, input, exempt_deletion_root, false)
}

fn classify_auto_mode_tool_input_with_options(
    tool_name: &str,
    input: &Value,
    exempt_deletion_root: Option<&std::path::Path>,
    continuity_script_review: bool,
) -> AutoModeInputDisposition {
    // A heredoc that only feeds a literal data sink (`cat`/`tee`
    // writing literal paths) is inert data, not hidden execution: the
    // write is equivalent to the always-approved `Write` tool, and
    // agents lean on this shape constantly, so classify the command
    // as if the block were not there. This also keeps the body text
    // out of the token scans below, where it could only false-positive.
    if matches!(tool_name, "Bash" | "BashTool") {
        if let Some(residual) =
            shell_command_input(tool_name, input).and_then(strip_literal_data_sink_heredocs)
        {
            let mut residual_input = input.clone();
            if let Some(object) = residual_input.as_object_mut() {
                object.insert("command".into(), Value::String(residual));
            }
            return classify_auto_mode_tool_input_with_options(
                tool_name,
                &residual_input,
                exempt_deletion_root,
                continuity_script_review,
            );
        }
    }
    let deletion_is_sensitive = is_sensitive_content_deletion_input(tool_name, input)
        && !exempt_deletion_root.is_some_and(|root| {
            shell_command_input(tool_name, input)
                .is_some_and(|command| shell_command_deletions_confined_to_root(command, root))
        });
    if is_session_credential_access_command(tool_name, input) {
        return AutoModeInputDisposition::HardSensitive(
            HardSensitiveReason::SessionCredentialAccess,
        );
    }
    if is_sensitive_git_stash_command(tool_name, input) {
        return AutoModeInputDisposition::HardSensitive(HardSensitiveReason::GitStashMutation);
    }
    if is_sensitive_git_worktree_command(tool_name, input) {
        return AutoModeInputDisposition::HardSensitive(HardSensitiveReason::GitWorktreeMutation);
    }
    if deletion_is_sensitive {
        return AutoModeInputDisposition::HardSensitive(HardSensitiveReason::FileDeletion);
    }
    if is_sensitive_git_content_restore_command(tool_name, input) {
        return AutoModeInputDisposition::HardSensitive(HardSensitiveReason::GitContentRestore);
    }
    if is_sensitive_interpreter_file_deletion_command(tool_name, input) {
        return AutoModeInputDisposition::HardSensitive(
            HardSensitiveReason::InterpreterFileDeletion,
        );
    }
    if is_sensitive_powershell_file_deletion_command(tool_name, input) {
        return AutoModeInputDisposition::HardSensitive(HardSensitiveReason::FileDeletion);
    }

    let Some(command) = shell_command_input(tool_name, input) else {
        return AutoModeInputDisposition::NoSpecialRisk;
    };

    if matches!(tool_name, "Bash" | "BashTool") {
        match extract_embedded_interpreter_script(command, continuity_script_review) {
            EmbeddedScriptExtraction::Review(script) => {
                if interpreter_code_destroys_files(&script.source) {
                    return AutoModeInputDisposition::HardSensitive(
                        HardSensitiveReason::InterpreterFileDeletion,
                    );
                }
                return AutoModeInputDisposition::EmbeddedScriptReview(script);
            }
            EmbeddedScriptExtraction::Invalid => {
                return AutoModeInputDisposition::HardSensitive(
                    HardSensitiveReason::UnanalyzableScriptInvocation,
                );
            }
            EmbeddedScriptExtraction::NotPresent => {}
        }
    }

    if shell_command_has_sensitive_script_execution(command) {
        AutoModeInputDisposition::HardSensitive(HardSensitiveReason::ScriptFileExecution)
    } else {
        AutoModeInputDisposition::NoSpecialRisk
    }
}

pub fn is_auto_mode_sensitive_tool_input(tool_name: &str, input: &Value) -> bool {
    !matches!(
        classify_auto_mode_tool_input(tool_name, input),
        AutoModeInputDisposition::NoSpecialRisk
    )
}

/// See [`classify_auto_mode_tool_input_with_exempt_deletion_root`].
pub fn is_auto_mode_sensitive_tool_input_with_exempt_deletion_root(
    tool_name: &str,
    input: &Value,
    exempt_deletion_root: Option<&std::path::Path>,
) -> bool {
    !matches!(
        classify_auto_mode_tool_input_with_exempt_deletion_root(
            tool_name,
            input,
            exempt_deletion_root
        ),
        AutoModeInputDisposition::NoSpecialRisk
    )
}

/// True when the input would stay sensitive even if every file
/// deletion in it were excused — it carries a sensitive class *other
/// than* plain deletion: git stash/worktree/content-restore, script
/// execution, or an embedded interpreter script (whose body is exactly
/// what a reviewer has not read yet, deletion or not).
///
/// This scopes "a background deletion may float up as a question": a
/// compound command like `rm -rf x && git stash drop` must not ride
/// its deletion into an interactive prompt for something a background
/// worker is required to hard-deny.
pub fn is_sensitive_beyond_deletions(tool_name: &str, input: &Value) -> bool {
    // Same inert-heredoc peeling as the classifier, so both agree on
    // what the command "is" before naming its risk classes.
    if matches!(tool_name, "Bash" | "BashTool") {
        if let Some(residual) =
            shell_command_input(tool_name, input).and_then(strip_literal_data_sink_heredocs)
        {
            let mut residual_input = input.clone();
            if let Some(object) = residual_input.as_object_mut() {
                object.insert("command".into(), Value::String(residual));
            }
            return is_sensitive_beyond_deletions(tool_name, &residual_input);
        }
    }
    if is_sensitive_git_stash_command(tool_name, input)
        || is_sensitive_git_worktree_command(tool_name, input)
        || is_sensitive_git_content_restore_command(tool_name, input)
    {
        return true;
    }
    let Some(command) = shell_command_input(tool_name, input) else {
        return false;
    };
    if matches!(tool_name, "Bash" | "BashTool")
        && !matches!(
            extract_embedded_interpreter_script(command, false),
            EmbeddedScriptExtraction::NotPresent
        )
    {
        return true;
    }
    shell_command_has_sensitive_script_execution(command)
}

const MAX_EMBEDDED_SCRIPT_BYTES: usize = 32 * 1024;

enum EmbeddedScriptExtraction {
    NotPresent,
    Review(EmbeddedInterpreterScript),
    Invalid,
}

fn extract_embedded_interpreter_script(
    command: &str,
    continuity_script_review: bool,
) -> EmbeddedScriptExtraction {
    let Some((header, body)) = command.split_once('\n') else {
        return if command.contains("<<") {
            EmbeddedScriptExtraction::Invalid
        } else {
            EmbeddedScriptExtraction::NotPresent
        };
    };
    let header = header.trim_end_matches('\r');
    let Some(operator_offset) = header.find("<<") else {
        return if body.contains("<<") {
            EmbeddedScriptExtraction::Invalid
        } else {
            EmbeddedScriptExtraction::NotPresent
        };
    };
    let invocation = header[..operator_offset].trim_end();
    let Some(interpreter) =
        embedded_interpreter_from_invocation(invocation, continuity_script_review)
    else {
        return EmbeddedScriptExtraction::Invalid;
    };
    if header[operator_offset + 2..].contains("<<") {
        return EmbeddedScriptExtraction::Invalid;
    }

    let delimiter_expression = header[operator_offset + 2..].trim();
    let Some(quote) = delimiter_expression.chars().next() else {
        return EmbeddedScriptExtraction::Invalid;
    };
    if !matches!(quote, '\'' | '"') || !delimiter_expression.ends_with(quote) {
        return EmbeddedScriptExtraction::Invalid;
    }
    let delimiter =
        &delimiter_expression[quote.len_utf8()..delimiter_expression.len() - quote.len_utf8()];
    if delimiter.is_empty()
        || !delimiter
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return EmbeddedScriptExtraction::Invalid;
    }

    let Some((source, tail)) = split_heredoc_body(body, delimiter) else {
        return EmbeddedScriptExtraction::Invalid;
    };
    if source.len() > MAX_EMBEDDED_SCRIPT_BYTES || !tail.trim().is_empty() {
        return EmbeddedScriptExtraction::Invalid;
    }

    EmbeddedScriptExtraction::Review(EmbeddedInterpreterScript {
        interpreter,
        invocation: invocation.to_owned(),
        source: source.to_owned(),
    })
}

fn heredoc_invocation_has_dynamic_shell_syntax(invocation: &str) -> bool {
    invocation.chars().any(|ch| {
        matches!(
            ch,
            '$' | '`' | '\\' | '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')'
        )
    })
}

fn embedded_interpreter_from_invocation(
    invocation: &str,
    continuity_script_review: bool,
) -> Option<EmbeddedInterpreter> {
    if invocation
        .chars()
        .any(|ch| matches!(ch, ';' | '|' | '&' | '<' | '>' | '\n' | '\r'))
        || heredoc_invocation_has_dynamic_shell_syntax(invocation)
    {
        return None;
    }
    let tokens = shell_tokens(invocation);
    let command_index = direct_command_name_index(&tokens)?;
    let command_name = normalized_command_name(&tokens[command_index]);
    let args = &tokens[command_index + 1..];
    if is_python_interpreter_name(&command_name) {
        return python_interpreter_input(args)
            .reads_stdin()
            .then_some(EmbeddedInterpreter::Python);
    }
    if matches!(command_name.as_str(), "node" | "nodejs") && (args.is_empty() || args == ["-"]) {
        return Some(EmbeddedInterpreter::Node);
    }
    if continuity_script_review && matches!(command_name.as_str(), "bash" | "sh") {
        return (args.is_empty() || args == ["-s"] || args == ["--"])
            .then_some(EmbeddedInterpreter::Bash);
    }
    if continuity_script_review && matches!(command_name.as_str(), "pwsh" | "powershell") {
        return powershell_interpreter_input(args)
            .reads_stdin()
            .then_some(EmbeddedInterpreter::PowerShell);
    }
    None
}

fn direct_command_name_index(tokens: &[String]) -> Option<usize> {
    let mut index = 0;
    if tokens
        .first()
        .is_some_and(|token| token.eq_ignore_ascii_case("env"))
    {
        index = 1;
        while tokens
            .get(index)
            .is_some_and(|token| token.starts_with('-') || is_env_assignment(token))
        {
            index += 1;
        }
    } else {
        while tokens
            .get(index)
            .is_some_and(|token| is_env_assignment(token))
        {
            index += 1;
        }
    }
    (index < tokens.len()).then_some(index)
}

fn split_heredoc_body<'a>(body: &'a str, delimiter: &str) -> Option<(&'a str, &'a str)> {
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        if content == delimiter {
            return Some((&body[..offset], &body[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

struct StrippableHeredocHeader {
    delimiter: String,
    /// Unquoted delimiters let the shell expand `$…` and backticks
    /// inside the body, so such a body is only inert when it contains
    /// neither.
    expands_body: bool,
    literal_data_sink: bool,
}

/// Parse a line that opens a plain heredoc (`… << 'DELIM' …`).
/// Returns `None` for here-strings, `<<-` variants, stacked `<<`
/// operators, and malformed delimiters — those lines are left in
/// place for the fail-closed paths downstream.
fn parse_strippable_heredoc_header(line: &str) -> Option<StrippableHeredocHeader> {
    let operator_offset = line.find("<<")?;
    let after_operator = &line[operator_offset + 2..];
    if after_operator.starts_with('<')
        || after_operator.starts_with('-')
        || after_operator.contains("<<")
    {
        return None;
    }
    let delimiter_expression = after_operator.trim_start();
    let first = delimiter_expression.chars().next()?;
    let (delimiter, trailing, quoted) = if matches!(first, '\'' | '"') {
        let rest = &delimiter_expression[1..];
        let close = rest.find(first)?;
        (&rest[..close], &rest[close + 1..], true)
    } else {
        let end = delimiter_expression
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
            .unwrap_or(delimiter_expression.len());
        (
            &delimiter_expression[..end],
            &delimiter_expression[end..],
            false,
        )
    };
    if delimiter.is_empty()
        || !delimiter
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some(StrippableHeredocHeader {
        delimiter: delimiter.to_owned(),
        expands_body: !quoted,
        literal_data_sink: heredoc_invocation_is_literal_data_sink(
            &line[..operator_offset],
            trailing,
        ),
    })
}

/// True when the heredoc invocation around the `<<` operator is a pure
/// data sink: `cat` or `tee` whose only effect is writing the heredoc
/// body to literal file paths (or stdout). The body of such a heredoc
/// never executes, so the write is no more dangerous than the `Write`
/// tool, which auto mode already approves.
fn heredoc_invocation_is_literal_data_sink(before_operator: &str, after_delimiter: &str) -> bool {
    let mut words: Vec<String> = Vec::new();
    let mut redirect_targets: Vec<String> = Vec::new();
    for part in [before_operator, after_delimiter] {
        if part
            .chars()
            .any(|ch| matches!(ch, ';' | '|' | '&' | '<' | '\'' | '"' | '\n' | '\r'))
            || heredoc_invocation_has_dynamic_shell_syntax(part)
        {
            return false;
        }
        if !collect_literal_sink_tokens(part, &mut words, &mut redirect_targets) {
            return false;
        }
    }
    let mut command_index = 0;
    while words
        .get(command_index)
        .is_some_and(|word| is_env_assignment(word))
    {
        command_index += 1;
    }
    let Some(command_word) = words.get(command_index) else {
        return false;
    };
    let args = &words[command_index + 1..];
    match normalized_command_name(command_word).as_str() {
        "cat" => {
            args.iter().all(|arg| arg == "-")
                && redirect_targets.len() <= 1
                && redirect_targets
                    .iter()
                    .all(|target| is_literal_sink_write_target(target))
        }
        "tee" => {
            args.iter().all(|arg| {
                matches!(arg.as_str(), "-a" | "--append") || is_literal_sink_write_target(arg)
            }) && redirect_targets
                .iter()
                .all(|target| is_literal_sink_write_target(target))
        }
        _ => false,
    }
}

/// Split a heredoc invocation fragment into command words and stdout
/// redirect targets. Anything but plain words and `>`/`>>` redirects
/// (fd redirects like `2>`, glued `a>b` forms) disqualifies the sink.
fn collect_literal_sink_tokens(
    text: &str,
    words: &mut Vec<String>,
    redirect_targets: &mut Vec<String>,
) -> bool {
    let mut tokens = text.split_whitespace();
    while let Some(token) = tokens.next() {
        if let Some(rest) = token.strip_prefix(">>").or_else(|| token.strip_prefix('>')) {
            let target = if rest.is_empty() {
                let Some(next) = tokens.next() else {
                    return false;
                };
                next
            } else {
                rest
            };
            if target.contains('>') {
                return false;
            }
            redirect_targets.push(target.to_owned());
        } else if token.contains('>') {
            return false;
        } else {
            words.push(token.to_owned());
        }
    }
    true
}

fn is_literal_sink_write_target(path: &str) -> bool {
    if path == "/dev/null" {
        return true;
    }
    if path.is_empty() || path.starts_with('-') || path.starts_with('~') {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    !(lower.starts_with("/dev/") || lower.starts_with("/proc/") || lower.starts_with("/sys/"))
}

/// Rewrite a shell command with every literal-data-sink heredoc
/// removed, so the residual command can be classified on its own.
/// Non-sink heredocs with a parseable delimiter are kept verbatim and
/// their bodies skipped opaquely — a sink-shaped line inside another
/// heredoc's body is data, not a command, and must never be stripped.
/// Returns `None` when nothing was stripped; unterminated or malformed
/// blocks are left in place so the fail-closed paths still see them.
fn strip_literal_data_sink_heredocs(command: &str) -> Option<String> {
    if !command.contains("<<") {
        return None;
    }
    let lines: Vec<&str> = command.split_inclusive('\n').collect();
    let mut output = String::with_capacity(command.len());
    let mut stripped_any = false;
    let mut index = 0;
    while index < lines.len() {
        let raw_line = lines[index];
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(header) = parse_strippable_heredoc_header(line) else {
            output.push_str(raw_line);
            index += 1;
            continue;
        };
        let terminator = (index + 1..lines.len()).find(|&candidate| {
            let content = lines[candidate]
                .strip_suffix('\n')
                .unwrap_or(lines[candidate]);
            let content = content.strip_suffix('\r').unwrap_or(content);
            content == header.delimiter
        });
        let Some(terminator) = terminator else {
            for rest in &lines[index..] {
                output.push_str(rest);
            }
            break;
        };
        let body_is_inert = !header.expands_body
            || lines[index + 1..terminator]
                .iter()
                .all(|body_line| !body_line.contains('$') && !body_line.contains('`'));
        if header.literal_data_sink && body_is_inert {
            stripped_any = true;
        } else {
            for kept in &lines[index..=terminator] {
                output.push_str(kept);
            }
        }
        index = terminator + 1;
    }
    stripped_any.then_some(output)
}

pub fn is_sensitive_content_deletion_input(tool_name: &str, input: &Value) -> bool {
    shell_command_input(tool_name, input).is_some_and(shell_command_has_sensitive_file_deletion)
}

pub fn is_sensitive_git_stash_command(tool_name: &str, input: &Value) -> bool {
    shell_command_input(tool_name, input).is_some_and(shell_command_has_sensitive_git_stash)
}

pub fn shell_command_has_sensitive_git_stash(command: &str) -> bool {
    let mut segment = Vec::new();
    for token in shell_tokens(command) {
        if is_shell_separator(&token) {
            if git_segment_has_sensitive_stash(&segment) {
                return true;
            }
            segment.clear();
        } else {
            segment.push(token);
        }
    }
    git_segment_has_sensitive_stash(&segment)
}

pub fn is_sensitive_git_worktree_command(tool_name: &str, input: &Value) -> bool {
    shell_command_input(tool_name, input).is_some_and(shell_command_has_sensitive_git_worktree)
}

pub fn shell_command_has_sensitive_git_worktree(command: &str) -> bool {
    let mut segment = Vec::new();
    for token in shell_tokens(command) {
        if is_shell_separator(&token) {
            if git_segment_has_sensitive_worktree_change(&segment) {
                return true;
            }
            segment.clear();
        } else {
            segment.push(token);
        }
    }
    git_segment_has_sensitive_worktree_change(&segment)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowShellHost {
    Unknown,
    Posix,
    PowerShell,
}

impl WorkflowShellHost {
    fn for_tool(tool_name: &str) -> Self {
        match tool_name {
            "Bash" | "BashTool" => Self::Posix,
            "PowerShell" | "PowerShellTool" => Self::PowerShell,
            _ => Self::Unknown,
        }
    }
}

pub fn is_workflow_sensitive_git_command(tool_name: &str, input: &Value) -> bool {
    let host = WorkflowShellHost::for_tool(tool_name);
    shell_command_input(tool_name, input).is_some_and(|command| {
        shell_command_has_workflow_sensitive_git_operation_for_host(command, host)
    })
}

pub fn shell_command_has_workflow_sensitive_git_operation(command: &str) -> bool {
    shell_command_has_workflow_sensitive_git_operation_for_host(command, WorkflowShellHost::Unknown)
}

fn shell_command_has_workflow_sensitive_git_operation_for_host(
    command: &str,
    host: WorkflowShellHost,
) -> bool {
    let command = shell_command_without_literal_data_blocks(command);
    let command = workflow_command_without_comments(&command);
    if workflow_command_text_has_sensitive_git_operation(&command, host) {
        return true;
    }
    workflow_embedded_command_payloads(&command)
        .into_iter()
        .any(|payload| shell_command_has_workflow_sensitive_git_operation_for_host(&payload, host))
}

fn workflow_command_text_has_sensitive_git_operation(
    command: &str,
    host: WorkflowShellHost,
) -> bool {
    if workflow_static_shell_pipeline_is_sensitive(command, host) {
        return true;
    }
    let mut segment = Vec::new();
    for token in workflow_shell_tokens(command) {
        if is_workflow_shell_separator(&token) {
            if git_segment_has_workflow_sensitive_operation(&segment, host) {
                return true;
            }
            segment.clear();
        } else {
            segment.push(token);
        }
    }
    git_segment_has_workflow_sensitive_operation(&segment, host)
}

fn shell_command_without_literal_data_blocks(command: &str) -> String {
    enum LiteralBlock {
        Heredoc {
            delimiter: String,
            strip_tabs: bool,
            expands: bool,
        },
        PowerShellHereString {
            terminator: &'static str,
            expands: bool,
            prefix_executes_body: bool,
            body: String,
        },
    }

    let mut output = String::with_capacity(command.len());
    let mut block: Option<LiteralBlock> = None;
    for line_with_ending in command.split_inclusive('\n') {
        let has_newline = line_with_ending.ends_with('\n');
        let line = line_with_ending
            .strip_suffix('\n')
            .unwrap_or(line_with_ending);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(active) = block.as_mut() {
            let closes = match active {
                LiteralBlock::Heredoc {
                    delimiter,
                    strip_tabs,
                    expands,
                } => {
                    let candidate = if *strip_tabs {
                        line.trim_start_matches('\t')
                    } else {
                        line
                    };
                    let closes = candidate == delimiter;
                    if !closes && *expands {
                        for payload in workflow_embedded_command_payloads(line) {
                            output.push_str(&payload);
                            output.push(';');
                        }
                    }
                    closes
                }
                LiteralBlock::PowerShellHereString {
                    terminator,
                    expands,
                    prefix_executes_body,
                    body,
                } => {
                    let close_suffix = line.trim_start().strip_prefix(*terminator);
                    if let Some(suffix) = close_suffix {
                        if *prefix_executes_body
                            || powershell_here_string_suffix_executes_body(suffix)
                        {
                            output.push_str(body);
                            output.push(';');
                        }
                        output.push_str(suffix);
                        true
                    } else {
                        body.push_str(line);
                        body.push('\n');
                        if *expands {
                            for payload in workflow_embedded_command_payloads(line) {
                                output.push_str(&payload);
                                output.push(';');
                            }
                        }
                        false
                    }
                }
            };
            if closes {
                block = None;
            }
            if has_newline {
                output.push('\n');
            }
            continue;
        }

        let output_start = output.len();
        output.push_str(line_with_ending);
        if let Some((delimiter, strip_tabs, marker_index, delimiter_end, expands)) =
            bash_heredoc_delimiter(line)
        {
            if !heredoc_body_is_executable(line) {
                output.truncate(output_start);
                output.push_str(&line[..marker_index]);
                output.push_str(&line[delimiter_end..]);
                if has_newline {
                    output.push('\n');
                }
                block = Some(LiteralBlock::Heredoc {
                    delimiter,
                    strip_tabs,
                    expands,
                });
            }
        } else if let Some((terminator, expands, marker_index)) =
            powershell_here_string_terminator(line)
        {
            output.truncate(output_start);
            output.push_str(&line[..marker_index]);
            if has_newline {
                output.push('\n');
            }
            block = Some(LiteralBlock::PowerShellHereString {
                terminator,
                expands,
                prefix_executes_body: powershell_here_string_prefix_executes_body(
                    &line[..marker_index],
                ),
                body: String::new(),
            });
        }
    }
    output
}

fn bash_heredoc_delimiter(line: &str) -> Option<(String, bool, usize, usize, bool)> {
    let line = &line[..shell_comment_start(line).unwrap_or(line.len())];
    let chars: Vec<char> = line.chars().collect();
    let mut quote = None;
    let mut escaped = false;
    let mut arithmetic_paren_depth = 0;
    let mut index = 0;
    while index + 1 < chars.len() {
        let ch = chars[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            index += 1;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
            index += 1;
            continue;
        }
        if arithmetic_paren_depth > 0 {
            match ch {
                '(' => arithmetic_paren_depth += 1,
                ')' => arithmetic_paren_depth -= 1,
                _ => {}
            }
            index += 1;
            continue;
        }
        if ch == '$' && chars.get(index + 1) == Some(&'(') && chars.get(index + 2) == Some(&'(') {
            arithmetic_paren_depth = 2;
            index += 3;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            quote = Some(ch);
            index += 1;
            continue;
        }
        if ch != '<' || chars[index + 1] != '<' || chars.get(index + 2) == Some(&'<') {
            index += 1;
            continue;
        }

        let marker_index = index;
        index += 2;
        let strip_tabs = chars.get(index) == Some(&'-');
        if strip_tabs {
            index += 1;
        }
        while chars.get(index).is_some_and(|ch| ch.is_whitespace()) {
            index += 1;
        }

        let mut delimiter = String::new();
        let mut delimiter_quote = None;
        let mut delimiter_ansi_c = false;
        let mut delimiter_was_quoted = false;
        while let Some(ch) = chars.get(index).copied() {
            if let Some(active) = delimiter_quote {
                if ch == active {
                    delimiter_quote = None;
                    delimiter_ansi_c = false;
                    index += 1;
                } else if ch == '\\' && delimiter_ansi_c {
                    return None;
                } else if ch == '\\' && active == '"' {
                    delimiter_was_quoted = true;
                    index += 1;
                    delimiter.push(*chars.get(index)?);
                    index += 1;
                } else {
                    delimiter.push(ch);
                    index += 1;
                }
                continue;
            }

            if ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '<' | '>' | '(' | ')') {
                break;
            }
            if ch == '$' && matches!(chars.get(index + 1), Some('\'' | '"')) {
                delimiter_was_quoted = true;
                delimiter_ansi_c = chars[index + 1] == '\'';
                delimiter_quote = Some(chars[index + 1]);
                index += 2;
                continue;
            }
            if matches!(ch, '\'' | '"') {
                delimiter_was_quoted = true;
                delimiter_quote = Some(ch);
                index += 1;
                continue;
            }
            if ch == '\\' {
                delimiter_was_quoted = true;
                index += 1;
                delimiter.push(*chars.get(index)?);
                index += 1;
                continue;
            }
            delimiter.push(ch);
            index += 1;
        }
        if delimiter.is_empty() || delimiter_quote.is_some() {
            return None;
        }

        let byte_index = |char_index: usize| {
            line.char_indices()
                .nth(char_index)
                .map(|(byte_index, _)| byte_index)
                .unwrap_or(line.len())
        };
        return Some((
            delimiter,
            strip_tabs,
            byte_index(marker_index),
            byte_index(index),
            !delimiter_was_quoted,
        ));
    }
    None
}

fn shell_comment_start(line: &str) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    let mut previous = None;
    for (index, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            previous = Some(ch);
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            } else if (ch == '\\' && active == '"') || ch == '`' {
                escaped = true;
            }
            previous = Some(ch);
            continue;
        }
        if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if matches!(ch, '\\' | '`') {
            escaped = true;
        } else if ch == '#'
            && previous.map_or(true, |previous| {
                previous.is_whitespace()
                    || matches!(previous, ';' | '|' | '&' | '(' | ')' | '{' | '}')
            })
        {
            return Some(index);
        }
        previous = Some(ch);
    }
    None
}

fn workflow_command_without_comments(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    let mut output = String::with_capacity(command.len());
    let mut quote = None;
    let mut escaped = false;
    let mut block_comment = false;
    let mut line_comment = false;
    let mut previous = None;
    let mut index = 0;

    while index < chars.len() {
        let ch = chars[index];
        if block_comment {
            if ch == '#' && chars.get(index + 1) == Some(&'>') {
                block_comment = false;
                index += 2;
            } else {
                if ch == '\n' {
                    output.push(ch);
                    previous = None;
                }
                index += 1;
            }
            continue;
        }
        if line_comment {
            if ch == '\n' {
                output.push(ch);
                line_comment = false;
                previous = None;
            }
            index += 1;
            continue;
        }
        if escaped {
            output.push(ch);
            escaped = false;
            previous = (ch != '\n').then_some(ch);
            index += 1;
            continue;
        }
        if let Some(active) = quote {
            output.push(ch);
            if ch == active {
                quote = None;
            } else if (ch == '\\' && active == '"') || ch == '`' {
                escaped = true;
            }
            previous = (ch != '\n').then_some(ch);
            index += 1;
            continue;
        }
        if ch == '<' && chars.get(index + 1) == Some(&'#') {
            if previous.is_some_and(|previous: char| !previous.is_whitespace()) {
                output.push(' ');
            }
            block_comment = true;
            index += 2;
            continue;
        }
        if ch == '#'
            && previous.map_or(true, |previous: char| {
                previous.is_whitespace()
                    || matches!(previous, ';' | '|' | '&' | '(' | ')' | '{' | '}')
            })
        {
            line_comment = true;
            index += 1;
            continue;
        }
        output.push(ch);
        if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if matches!(ch, '\\' | '`') {
            escaped = true;
        }
        previous = (ch != '\n').then_some(ch);
        index += 1;
    }
    output
}

fn heredoc_body_is_executable(opener: &str) -> bool {
    let (segments, separators) = split_workflow_pipeline_segments(opener);
    let Some(source_index) = segments
        .iter()
        .position(|segment| segment.iter().any(|token| token.contains("<<")))
    else {
        return false;
    };
    if segment_command_is_shell_interpreter(&segments[source_index])
        || segment_sources_standard_input(&segments[source_index])
    {
        return true;
    }

    let mut segment_index = source_index;
    while separators
        .get(segment_index)
        .is_some_and(|separator| separator == "|")
    {
        segment_index += 1;
        if segments
            .get(segment_index)
            .is_some_and(|segment| segment_command_is_shell_interpreter(segment))
        {
            return true;
        }
    }
    false
}

fn segment_sources_standard_input(segment: &[String]) -> bool {
    let Some(index) = workflow_executed_command_index(segment) else {
        return false;
    };
    let command = workflow_unescape_static_token(&segment[index]);
    if command != "." && !command.eq_ignore_ascii_case("source") {
        return false;
    }
    segment[index + 1..].iter().any(|arg| {
        matches!(
            workflow_unescape_static_token(arg).as_str(),
            "/dev/stdin" | "/dev/fd/0" | "/proc/self/fd/0"
        )
    })
}

fn segment_command_is_shell_interpreter(segment: &[String]) -> bool {
    workflow_executed_command_index(segment).is_some_and(|index| {
        [
            "bash",
            "sh",
            "zsh",
            "dash",
            "ksh",
            "busybox",
            "pwsh",
            "powershell",
            "invoke-expression",
            "iex",
        ]
        .iter()
        .any(|name| workflow_command_name_matches(&segment[index], name))
    })
}

fn powershell_here_string_prefix_executes_body(prefix: &str) -> bool {
    let segment = workflow_shell_tokens(prefix);
    workflow_executed_command_index(&segment).is_some_and(|index| {
        ["invoke-expression", "iex"]
            .iter()
            .any(|name| workflow_command_name_matches(&segment[index], name))
    })
}

fn powershell_here_string_suffix_executes_body(suffix: &str) -> bool {
    let suffix = suffix.trim_start();
    let Some(command) = suffix.strip_prefix('|') else {
        return false;
    };
    let (segments, _) = split_workflow_pipeline_segments(command);
    segments
        .iter()
        .any(|segment| segment_command_is_shell_interpreter(segment))
}

fn powershell_here_string_terminator(line: &str) -> Option<(&'static str, bool, usize)> {
    let code = &line[..shell_comment_start(line).unwrap_or(line.len())];
    let trimmed = code.trim_end();
    if let Some(index) = trimmed.strip_suffix("@'").map(str::len) {
        Some(("'@", false, index))
    } else if let Some(index) = trimmed.strip_suffix("@\"").map(str::len) {
        Some(("\"@", true, index))
    } else {
        None
    }
}

fn workflow_embedded_command_payloads(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut payloads = Vec::new();
    let mut quote = None;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if quote == Some('\'') {
            if ch == '\'' {
                quote = None;
            }
            index += 1;
            continue;
        }
        if quote == Some('"') {
            if ch == '\\' {
                index += 2;
                continue;
            }
            if ch == '"' {
                quote = None;
                index += 1;
                continue;
            }
            if ch == '$' && chars.get(index + 1) == Some(&'(') {
                if let Some((payload, next)) = workflow_parenthesized_payload(&chars, index + 1) {
                    payloads.push(payload);
                    index = next;
                    continue;
                }
            }
            if ch == '`' {
                if let Some((payload, next)) = workflow_backtick_payload(&chars, index) {
                    payloads.push(payload);
                    index = next;
                    continue;
                }
            }
            index += 1;
            continue;
        }
        if ch == '\\' {
            index += 2;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            index += 1;
            continue;
        }
        if ch == '$' && chars.get(index + 1) == Some(&'(') {
            if let Some((payload, next)) = workflow_parenthesized_payload(&chars, index + 1) {
                payloads.push(payload);
                index = next;
                continue;
            }
        }
        if ch == '`' {
            if let Some((payload, next)) = workflow_backtick_payload(&chars, index) {
                payloads.push(payload);
                index = next;
                continue;
            }
        }
        index += 1;
    }
    payloads
}

fn workflow_parenthesized_payload(chars: &[char], open_index: usize) -> Option<(String, usize)> {
    let mut depth = 1;
    let mut quote = None;
    let mut index = open_index + 1;
    while index < chars.len() {
        let ch = chars[index];
        if quote == Some('\'') {
            if ch == '\'' {
                quote = None;
            }
            index += 1;
            continue;
        }
        if quote == Some('"') {
            if ch == '\\' {
                index += 2;
                continue;
            }
            if ch == '"' {
                quote = None;
            }
            index += 1;
            continue;
        }
        if ch == '\\' {
            index += 2;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            index += 1;
            continue;
        }
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((chars[open_index + 1..index].iter().collect(), index + 1));
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn workflow_backtick_payload(chars: &[char], open_index: usize) -> Option<(String, usize)> {
    let mut index = open_index + 1;
    while index < chars.len() {
        if chars[index] == '\\' {
            index += 2;
            continue;
        }
        if chars[index] == '`' {
            return Some((chars[open_index + 1..index].iter().collect(), index + 1));
        }
        index += 1;
    }
    None
}

pub fn is_sensitive_git_checkout_command(tool_name: &str, input: &Value) -> bool {
    shell_command_input(tool_name, input).is_some_and(shell_command_has_sensitive_git_checkout)
}

pub fn shell_command_has_sensitive_git_checkout(command: &str) -> bool {
    let mut segment = Vec::new();
    for token in shell_tokens(command) {
        if is_shell_separator(&token) {
            if git_segment_has_sensitive_checkout(&segment) {
                return true;
            }
            segment.clear();
        } else {
            segment.push(token);
        }
    }
    git_segment_has_sensitive_checkout(&segment)
}

pub fn shell_command_has_sensitive_file_deletion(command: &str) -> bool {
    let mut segment = Vec::new();
    for token in shell_tokens(command) {
        if is_shell_separator(&token) {
            if segment_has_sensitive_file_deletion(&segment) {
                return true;
            }
            segment.clear();
        } else {
            segment.push(token);
        }
    }
    segment_has_sensitive_file_deletion(&segment)
}

/// Conservative allow-list of deletion commands whose arguments are
/// plain target paths. Everything else the deletion classifier flags
/// (`git clean`, `find -delete`, `xargs rm`, wrapper shells, control
/// flow, registry/recycle-bin cmdlets) never qualifies for the root
/// exemption, even when it happens to only touch the root.
const ROOT_CONFINABLE_DELETION_COMMANDS: &[&str] = &[
    "rm",
    "unlink",
    "rmdir",
    "del",
    "erase",
    "rd",
    "ri",
    "remove-item",
];

/// Returns true when `command` contains at least one sensitive file
/// deletion and EVERY deletion in it is a simple command from
/// [`ROOT_CONFINABLE_DELETION_COMMANDS`] whose targets statically
/// resolve to absolute paths inside `root`. A PowerShell variable is
/// accepted only when assigned a single static value earlier in the
/// same command. Simple positional `Join-Path` assignments are resolved
/// when both operands are static and the child is relative.
///
/// Used to exempt session-scratchpad cleanup from the interactive
/// deletion approval. Deliberately conservative: unresolved dynamic
/// syntax, arrays, wildcard targets, `..` traversal, option tokens with
/// payloads, and paths that canonicalize through a symlink/junction
/// outside the root all keep the command sensitive.
pub fn shell_command_deletions_confined_to_root(command: &str, root: &std::path::Path) -> bool {
    let Some(root) = normalized_root_scope(root) else {
        return false;
    };
    deletions_confined_to_scopes(command, std::slice::from_ref(&root), false)
}

/// Stricter sibling of [`shell_command_deletions_confined_to_root`]: the
/// command must *only* delete, and every target must land inside one of
/// `roots`.
///
/// The difference is the segments that delete nothing. Excusing a
/// deletion inside a command someone is going to be asked about anyway
/// can ignore them; authorising the whole command outright cannot, or
/// `rm <root>/x; curl evil` would be waved through by the half of it
/// that only tidies up. A root that is not an absolute path authorises
/// nothing and is dropped; no roots at all means no authorisation.
///
/// Every other conservatism is the one already documented above:
/// wildcards, `..`, dynamic targets, option tokens with payloads, and
/// paths that canonicalise through a link out of the root all keep the
/// command on the interactive path.
pub fn shell_command_only_deletes_inside_roots(
    command: &str,
    roots: &[std::path::PathBuf],
) -> bool {
    let scopes = roots
        .iter()
        .filter_map(|root| normalized_root_scope(root))
        .collect::<Vec<_>>();
    if scopes.is_empty() {
        return false;
    }
    deletions_confined_to_scopes(command, &scopes, true)
}

fn deletions_confined_to_scopes(
    command: &str,
    roots: &[NormalizedRootScope],
    every_segment_must_delete: bool,
) -> bool {
    let mut variables = std::collections::HashMap::new();
    let mut segment: Vec<String> = Vec::new();
    let mut saw_deletion_segment = false;
    for token in shell_tokens(command) {
        if is_shell_separator(&token) {
            record_static_powershell_assignment(command, &segment, &mut variables);
            if !deletion_segment_confined_to_roots(
                &segment,
                roots,
                &variables,
                every_segment_must_delete,
                &mut saw_deletion_segment,
            ) {
                return false;
            }
            segment.clear();
        } else {
            segment.push(token);
        }
    }
    record_static_powershell_assignment(command, &segment, &mut variables);
    if !deletion_segment_confined_to_roots(
        &segment,
        roots,
        &variables,
        every_segment_must_delete,
        &mut saw_deletion_segment,
    ) {
        return false;
    }
    saw_deletion_segment
}

struct NormalizedRootScope {
    logical: String,
    canonical: Option<std::path::PathBuf>,
}

fn normalized_root_scope(root: &std::path::Path) -> Option<NormalizedRootScope> {
    if !root.is_absolute() {
        return None;
    }
    let mut logical = root.display().to_string().replace('\\', "/");
    while logical.ends_with('/') {
        logical.pop();
    }
    if logical.is_empty() {
        return None;
    }
    Some(NormalizedRootScope {
        logical,
        canonical: canonicalize_scope_path(root),
    })
}

fn record_static_powershell_assignment(
    command: &str,
    segment: &[String],
    variables: &mut std::collections::HashMap<String, String>,
) {
    if segment.iter().any(|token| {
        token.starts_with("${")
            || token.contains("variable:")
            || matches!(
                normalized_command_name(token).as_str(),
                "set-variable" | "sv" | "new-variable" | "nv" | "remove-variable" | "rv"
            )
    }) {
        variables.clear();
        return;
    }

    record_leading_static_powershell_assignment(command, segment, variables);
    invalidate_embedded_powershell_assignments(segment, variables);
}

fn record_leading_static_powershell_assignment(
    command: &str,
    segment: &[String],
    variables: &mut std::collections::HashMap<String, String>,
) {
    let Some(first) = segment.first() else {
        return;
    };
    let Some(variable) = powershell_variable_prefix(first) else {
        return;
    };
    let variable_key = variable.to_ascii_lowercase();
    let suffix = &first[variable.len()..];
    let rhs = if suffix.is_empty() {
        let Some(operator) = segment.get(1).map(String::as_str) else {
            return;
        };
        if !is_powershell_assignment_operator(operator) {
            return;
        }
        if operator != "=" {
            variables.remove(&variable_key);
            return;
        }
        segment[2..].iter().map(String::as_str).collect::<Vec<_>>()
    } else if let Some(value) = suffix.strip_prefix('=') {
        std::iter::once(value)
            .chain(segment[1..].iter().map(String::as_str))
            .collect::<Vec<_>>()
    } else if powershell_assignment_operator_prefix(suffix).is_some() {
        variables.remove(&variable_key);
        return;
    } else {
        return;
    };

    let value = resolve_static_powershell_assignment_value(command, &rhs, variables);
    variables.remove(&variable_key);
    if let Some(value) = value {
        variables.insert(variable_key, value);
    }
}

fn resolve_static_powershell_assignment_value(
    command_text: &str,
    rhs: &[&str],
    variables: &std::collections::HashMap<String, String>,
) -> Option<String> {
    if let [value] = rhs {
        return resolve_static_powershell_token(command_text, value, variables);
    }
    let [command, parent, child] = rhs else {
        return None;
    };
    if normalized_command_name(command) != "join-path" {
        return None;
    }

    let parent = resolve_static_powershell_token(command_text, parent, variables)?;
    let child = resolve_static_powershell_token(command_text, child, variables)?;
    if child.starts_with(['/', '\\'])
        || child.contains(':')
        || child.split(['/', '\\']).any(|component| component == "..")
    {
        return None;
    }

    Some(format!(
        "{}/{}",
        parent.trim_end_matches(['/', '\\']),
        child
    ))
}

fn resolve_static_powershell_token(
    command_text: &str,
    token: &str,
    variables: &std::collections::HashMap<String, String>,
) -> Option<String> {
    if token.starts_with('$') {
        return variables.get(&token.to_ascii_lowercase()).cloned();
    }
    let token = if let Some(inner) = token
        .strip_prefix('(')
        .and_then(|token| token.strip_suffix(')'))
    {
        if inner.contains(['\'', '"'])
            || command_text.contains(&format!("({inner})"))
            || !command_text.contains(&format!("('{inner}')"))
                && !command_text.contains(&format!("(\"{inner}\")"))
        {
            return None;
        }
        inner
    } else {
        token
    };
    if token.is_empty()
        || token.chars().any(|ch| {
            matches!(
                ch,
                '$' | '`' | '%' | '{' | '}' | ',' | '(' | ')' | '*' | '?' | '[' | ']'
            )
        })
    {
        return None;
    }
    Some(token.to_string())
}

fn invalidate_embedded_powershell_assignments(
    segment: &[String],
    variables: &mut std::collections::HashMap<String, String>,
) {
    for (index, token) in segment.iter().enumerate().skip(1) {
        let Some(variable) = powershell_variable_prefix(token) else {
            continue;
        };
        let suffix = &token[variable.len()..];
        let mutates = powershell_assignment_operator_prefix(suffix).is_some()
            || (suffix.is_empty()
                && segment
                    .get(index + 1)
                    .is_some_and(|operator| is_powershell_assignment_operator(operator)));
        if mutates {
            variables.remove(&variable.to_ascii_lowercase());
        }
    }
}

fn is_powershell_assignment_operator(token: &str) -> bool {
    matches!(token, "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "++" | "--")
}

fn powershell_assignment_operator_prefix(token: &str) -> Option<&'static str> {
    ["+=", "-=", "*=", "/=", "%=", "++", "--", "="]
        .into_iter()
        .find(|operator| token.starts_with(operator))
}

fn powershell_variable_prefix(token: &str) -> Option<&str> {
    let name = token.strip_prefix('$')?;
    let name_len = name
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_alphanumeric() || *ch == '_')
        .map(|(index, ch)| index + ch.len_utf8())
        .last()?;
    Some(&token[..1 + name_len])
}

fn deletion_segment_confined_to_roots(
    segment: &[String],
    roots: &[NormalizedRootScope],
    variables: &std::collections::HashMap<String, String>,
    every_segment_must_delete: bool,
    saw_deletion_segment: &mut bool,
) -> bool {
    if segment.is_empty() {
        return true;
    }
    if !segment_has_sensitive_file_deletion(segment) {
        // A segment that deletes nothing is none of this function's
        // business when the caller only excuses deletions inside a
        // command it is asking about anyway. It is very much its
        // business when the caller is authorising the whole command.
        return !every_segment_must_delete;
    }
    *saw_deletion_segment = true;
    let Some(command_index) = command_name_index(segment) else {
        return false;
    };
    let command_name = normalized_command_name(&segment[command_index]);
    if ROOT_CONFINABLE_DELETION_COMMANDS.contains(&command_name.as_str()) {
        return deletion_arguments_confined_to_roots(
            &segment[command_index + 1..],
            roots,
            variables,
        );
    }
    if is_deletion_control_flow_command(&command_name) {
        return control_flow_deletions_confined_to_roots(
            &segment[command_index + 1..],
            roots,
            variables,
        );
    }
    false
}

fn is_deletion_control_flow_command(command_name: &str) -> bool {
    matches!(
        command_name,
        "if" | "for"
            | "foreach"
            | "foreach-object"
            | "%"
            | "while"
            | "do"
            | "switch"
            | "try"
            | "catch"
            | "finally"
    )
}

fn control_flow_deletions_confined_to_roots(
    tokens: &[String],
    roots: &[NormalizedRootScope],
    variables: &std::collections::HashMap<String, String>,
) -> bool {
    let mut saw_deletion = false;
    let mut index = 0;
    while index < tokens.len() {
        let command_name = normalized_command_name(&tokens[index]);
        if !ROOT_CONFINABLE_DELETION_COMMANDS.contains(&command_name.as_str()) {
            index += 1;
            continue;
        }
        saw_deletion = true;
        let start = index + 1;
        let mut end = start;
        while end < tokens.len() && !matches!(tokens[end].as_str(), "{" | "}" | "else" | "elseif") {
            end += 1;
        }
        if !deletion_arguments_confined_to_roots(&tokens[start..end], roots, variables) {
            return false;
        }
        index = end.saturating_add(1);
    }
    saw_deletion
}

fn deletion_arguments_confined_to_roots(
    arguments: &[String],
    roots: &[NormalizedRootScope],
    variables: &std::collections::HashMap<String, String>,
) -> bool {
    let mut checked_target = false;
    for token in arguments {
        if token.starts_with('-') {
            if token.contains('/')
                || token.contains('\\')
                || token.contains(':')
                || token.contains('=')
                || token.contains('$')
                || token.contains('%')
            {
                return false;
            }
            continue;
        }
        let resolved = token
            .strip_prefix('$')
            .and_then(|_| variables.get(&token.to_ascii_lowercase()))
            .map(String::as_str)
            .unwrap_or(token);
        if !token_is_static_path_under_roots(resolved, roots) {
            return false;
        }
        checked_target = true;
    }
    checked_target
}

/// A target has to land inside *one* root, not every root: the roots are
/// alternatives (a scratchpad, a mandated report file), not a conjunction.
fn token_is_static_path_under_roots(token: &str, roots: &[NormalizedRootScope]) -> bool {
    if token.is_empty()
        || token.starts_with('~')
        || token.chars().any(|ch| {
            matches!(
                ch,
                '$' | '`' | '%' | '{' | '}' | ',' | '(' | ')' | '*' | '?' | '[' | ']'
            )
        })
    {
        return false;
    }
    let normalized = token.replace('\\', "/");
    if normalized.split('/').any(|component| component == "..") {
        return false;
    }
    roots
        .iter()
        .any(|root| token_is_static_path_under_root(token, &normalized, root))
}

fn token_is_static_path_under_root(
    token: &str,
    normalized: &str,
    root: &NormalizedRootScope,
) -> bool {
    if !normalized_path_starts_with(normalized, &root.logical) {
        return false;
    }
    match root.canonical.as_deref() {
        Some(canonical_root) => canonicalize_scope_path(std::path::Path::new(token))
            .is_some_and(|path| scope_path_starts_with(&path, canonical_root)),
        None => true,
    }
}

fn normalized_path_starts_with(path: &str, root: &str) -> bool {
    if path.len() < root.len() || !path.is_char_boundary(root.len()) {
        return false;
    }
    let (prefix, remainder) = path.split_at(root.len());
    let prefix_matches = if cfg!(windows) {
        prefix.eq_ignore_ascii_case(root)
    } else {
        prefix == root
    };
    prefix_matches && (remainder.is_empty() || remainder.starts_with('/'))
}

/// `git show REV:path`/`git cat-file` output redirected — or piped
/// into a write sink — resurrects committed content over the working
/// tree without going through Edit/Write, the same destructive effect
/// as `git checkout` (already sensitive). Plain `git show` inspection
/// with no redirect stays non-sensitive.
pub fn is_sensitive_git_content_restore_command(tool_name: &str, input: &Value) -> bool {
    shell_command_input(tool_name, input)
        .is_some_and(shell_command_has_sensitive_git_content_restore)
}

pub fn shell_command_has_sensitive_git_content_restore(command: &str) -> bool {
    let (segments, separators) = split_pipeline_segments(command);
    let mut emits_committed_content = false;
    for (index, segment) in segments.iter().enumerate() {
        if segment_wrapper_payload_matches(segment, shell_command_has_sensitive_git_content_restore)
        {
            return true;
        }
        if !git_segment_emits_committed_content(segment) {
            continue;
        }
        emits_committed_content = true;
        if segment_has_file_redirect(segment) {
            return true;
        }
        let piped_into_next = separators.get(index).is_some_and(|sep| sep == "|");
        if piped_into_next
            && segments
                .get(index + 1)
                .is_some_and(|next| segment_is_write_sink(next))
        {
            return true;
        }
    }
    // Committed content captured anywhere in the command (for example
    // `$x = & git show HEAD:path`) combined with any file write in
    // another segment is a working-tree restore in disguise — the
    // laundering shape from the auto-mode bypass incident. Plain
    // writes with no committed-content source stay non-sensitive.
    emits_committed_content
        && segments
            .iter()
            .any(|segment| segment_performs_file_write(segment))
}

fn segment_performs_file_write(segment: &[String]) -> bool {
    if segment_has_file_redirect(segment) || segment_is_write_sink(segment) {
        return true;
    }
    if let Some(command_index) = command_name_index(segment) {
        if matches!(
            normalized_command_name(&segment[command_index]).as_str(),
            "copy-item" | "move-item" | "new-item"
        ) && !segment_has_noop_flag(segment)
        {
            return true;
        }
    }
    segment.iter().any(|token| {
        let lower = token.to_ascii_lowercase();
        powershell_static_file_write_method(&lower)
            || [
                ".openwrite(",
                ".create(",
                ".createtext(",
                ".copyto(",
                ".moveto(",
            ]
            .iter()
            .any(|method| lower.contains(method))
    })
}

fn powershell_static_file_write_method(lower: &str) -> bool {
    let Some((class_offset, class_name)) = powershell_static_file_class_offset(lower) else {
        return false;
    };
    let Some(method) = lower[class_offset + class_name.len()..].split('(').next() else {
        return false;
    };
    matches!(
        method,
        "writealltext"
            | "writealllines"
            | "writeallbytes"
            | "appendalltext"
            | "appendalllines"
            | "copy"
            | "move"
            | "replace"
            | "create"
            | "createdirectory"
            | "createtext"
            | "copyfile"
            | "copydirectory"
            | "movefile"
            | "movedirectory"
            | "open"
            | "openwrite"
    )
}

/// Inline interpreter code (`python -c …`, `node -e …`) calling
/// file-deleting APIs bypasses the shell-token deletion scan;
/// classify it like the equivalent shell command. The observed
/// laundering pattern is a blocked `rm` retried as
/// `python -c "shutil.rmtree(...)"`. Ordinary writes, copies, and
/// renames are deliberately NOT flagged — auto mode only guards
/// deletion, resets, and other unrecoverable destruction.
pub fn is_sensitive_interpreter_file_deletion_command(tool_name: &str, input: &Value) -> bool {
    shell_command_input(tool_name, input)
        .is_some_and(shell_command_has_sensitive_interpreter_file_deletion)
}

pub fn shell_command_has_sensitive_interpreter_file_deletion(command: &str) -> bool {
    let (segments, _) = split_pipeline_segments(command);
    segments
        .iter()
        .any(|segment| segment_has_sensitive_interpreter_file_deletion(segment))
}

pub fn is_sensitive_script_execution_command(tool_name: &str, input: &Value) -> bool {
    shell_command_input(tool_name, input).is_some_and(shell_command_has_sensitive_script_execution)
}

pub fn shell_command_has_sensitive_script_execution(command: &str) -> bool {
    let (segments, separators) = split_pipeline_segments(command);
    segments.iter().enumerate().any(|(index, segment)| {
        segment_wrapper_payload_matches(segment, shell_command_has_sensitive_script_execution)
            || segment_executes_uninspected_script(segment)
            || index > 0
                && separators
                    .get(index - 1)
                    .is_some_and(|separator| separator == "|")
                && segment_executes_piped_script(segment)
            || index > 0
                && separators
                    .get(index - 1)
                    .is_some_and(|separator| separator == "&")
                && segment_executes_dynamic_powershell_target(segment)
    })
}

fn segment_executes_piped_script(segment: &[String]) -> bool {
    let Some(command_index) = command_name_index(segment) else {
        return false;
    };
    let command_name = normalized_command_name(&segment[command_index]);
    let args = &segment[command_index + 1..];
    if is_python_interpreter_name(&command_name) {
        return python_interpreter_input(args).reads_stdin();
    }
    if matches!(command_name.as_str(), "pwsh" | "powershell") {
        return powershell_interpreter_input(args).reads_stdin();
    }
    false
}

fn segment_executes_dynamic_powershell_target(segment: &[String]) -> bool {
    command_name_index(segment).is_some_and(|command_index| {
        segment[command_index]
            .chars()
            .next()
            .is_some_and(|ch| matches!(ch, '$' | '(' | '{' | '['))
    })
}

fn segment_executes_uninspected_script(segment: &[String]) -> bool {
    let Some(command_index) = command_name_index(segment) else {
        return false;
    };
    let command_name = normalized_command_name(&segment[command_index]);
    if token_has_script_extension(&segment[command_index], &["py", "pyw", "pyz", "ps1"])
        || matches!(command_name.as_str(), "." | "source")
            && segment[command_index + 1..]
                .iter()
                .any(|token| token_has_script_extension(token, &["py", "pyw", "pyz", "ps1"]))
    {
        return true;
    }
    if is_python_interpreter_name(&command_name) {
        return python_interpreter_executes_uninspected_code(&segment[command_index + 1..]);
    }
    if matches!(command_name.as_str(), "pwsh" | "powershell") {
        return powershell_executes_uninspected_code(&segment[command_index + 1..]);
    }
    if matches!(
        command_name.as_str(),
        "invoke-command" | "start-job" | "start-threadjob"
    ) {
        return segment[command_index + 1..].iter().any(|arg| {
            matches!(
                arg.to_ascii_lowercase().as_str(),
                "-scriptblock" | "-filepath"
            ) || token_has_script_extension(arg, &["ps1"])
        });
    }
    matches!(command_name.as_str(), "invoke-expression" | "iex")
}

fn is_python_interpreter_name(command_name: &str) -> bool {
    if matches!(command_name, "py" | "python" | "pythonw" | "pypy") {
        return true;
    }
    ["python", "pythonw", "pypy"].iter().any(|prefix| {
        command_name
            .strip_prefix(prefix)
            .is_some_and(is_numeric_version_suffix)
    })
}

fn is_numeric_version_suffix(suffix: &str) -> bool {
    !suffix.is_empty()
        && suffix.split('.').all(|component| {
            !component.is_empty() && component.chars().all(|ch| ch.is_ascii_digit())
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterpreterInput {
    InfoOnly,
    Inline,
    External,
    ExplicitStdin,
    ImplicitStdin,
}

impl InterpreterInput {
    fn reads_stdin(self) -> bool {
        matches!(self, Self::ExplicitStdin | Self::ImplicitStdin)
    }

    fn is_uninspected(self) -> bool {
        matches!(self, Self::External | Self::ExplicitStdin)
    }
}

fn python_interpreter_input(args: &[String]) -> InterpreterInput {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let lower = arg.to_ascii_lowercase();
        if matches!(arg.as_str(), "-V" | "-VV")
            || matches!(lower.as_str(), "-h" | "--help" | "--version")
        {
            return InterpreterInput::InfoOnly;
        }
        if arg == "-c" || arg.starts_with("-c") && arg.len() > 2 {
            return InterpreterInput::Inline;
        }
        if arg == "-m" || arg.starts_with("-m") && arg.len() > 2 {
            return InterpreterInput::External;
        }
        if arg == "-" {
            return InterpreterInput::ExplicitStdin;
        }
        if arg == "--" {
            return match args.get(index + 1).map(String::as_str) {
                None => InterpreterInput::ImplicitStdin,
                Some("-") => InterpreterInput::ExplicitStdin,
                Some(_) => InterpreterInput::External,
            };
        }
        if matches!(arg.as_str(), "-W" | "-X" | "-Q") || lower == "--check-hash-based-pycs" {
            index += 2;
            continue;
        }
        if !arg.starts_with('-') {
            return InterpreterInput::External;
        }
        index += 1;
    }
    InterpreterInput::ImplicitStdin
}

fn powershell_interpreter_input(args: &[String]) -> InterpreterInput {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let lower = arg.to_ascii_lowercase();
        if matches!(lower.as_str(), "-?" | "-h" | "--help" | "-help") {
            return InterpreterInput::InfoOnly;
        }
        if matches!(
            lower.as_str(),
            "-encodedcommand" | "-enc" | "-e" | "-encodedarguments"
        ) {
            return InterpreterInput::External;
        }
        if matches!(lower.as_str(), "-file" | "-f") {
            return match args.get(index + 1).map(String::as_str) {
                Some("-") => InterpreterInput::ExplicitStdin,
                Some(_) | None => InterpreterInput::External,
            };
        }
        if matches!(lower.as_str(), "-command" | "-c" | "-commandwithargs") {
            return match args.get(index + 1).map(String::as_str) {
                None | Some("-") => InterpreterInput::ExplicitStdin,
                Some(payload)
                    if payload
                        .chars()
                        .next()
                        .is_some_and(|ch| matches!(ch, '$' | '(' | '{' | '[')) =>
                {
                    InterpreterInput::External
                }
                Some(_) => InterpreterInput::Inline,
            };
        }
        if matches!(
            lower.as_str(),
            "-configurationname"
                | "-custompipename"
                | "-executionpolicy"
                | "-ep"
                | "-inputformat"
                | "-outputformat"
                | "-psconsolefile"
                | "-settingsfile"
                | "-version"
                | "-windowstyle"
                | "-workingdirectory"
        ) {
            index += 2;
            continue;
        }
        if arg == "-" {
            return InterpreterInput::ExplicitStdin;
        }
        if !arg.starts_with('-') {
            return InterpreterInput::External;
        }
        index += 1;
    }
    InterpreterInput::ImplicitStdin
}

fn python_interpreter_executes_uninspected_code(args: &[String]) -> bool {
    python_interpreter_input(args).is_uninspected()
}

fn powershell_executes_uninspected_code(args: &[String]) -> bool {
    powershell_interpreter_input(args).is_uninspected()
}

fn token_has_script_extension(token: &str, extensions: &[&str]) -> bool {
    let path = token
        .trim_matches(|ch: char| matches!(ch, '\'' | '"' | '(' | ')' | '{' | '}' | '&'))
        .split(['?', '#'])
        .next()
        .unwrap_or(token);
    let Some((_, extension)) = path.rsplit_once('.') else {
        return false;
    };
    extensions
        .iter()
        .any(|expected| extension.eq_ignore_ascii_case(expected))
}

/// PowerShell file APIs that *delete* content (`[IO.File]::Delete`,
/// `$item.Delete()`) bypass the cmdlet-level deletion scan. Write,
/// copy, and move APIs are deliberately NOT flagged here — ordinary
/// edits are allowed in auto mode; only deletion and other
/// unrecoverable destruction require approval. (Writes that restore
/// committed content are caught separately by the git content-restore
/// classifier.)
pub fn is_sensitive_powershell_file_deletion_command(tool_name: &str, input: &Value) -> bool {
    shell_command_input(tool_name, input)
        .is_some_and(shell_command_has_sensitive_powershell_file_deletion)
}

pub fn shell_command_has_sensitive_powershell_file_deletion(command: &str) -> bool {
    let (segments, _) = split_pipeline_segments(command);
    segments.iter().any(|segment| {
        segment_wrapper_payload_matches(
            segment,
            shell_command_has_sensitive_powershell_file_deletion,
        ) || segment_has_sensitive_powershell_file_deletion(segment)
    })
}

fn segment_has_sensitive_powershell_file_deletion(segment: &[String]) -> bool {
    let Some(command_index) = command_name_index(segment) else {
        return false;
    };
    segment_invokes_powershell_static_file_deletion(segment, command_index)
}

fn segment_invokes_powershell_static_file_deletion(
    segment: &[String],
    command_index: usize,
) -> bool {
    let expression_context = segment[command_index]
        .chars()
        .next()
        .is_some_and(|ch| matches!(ch, '$' | '(' | '{' | '['))
        || matches!(
            normalized_command_name(&segment[command_index]).as_str(),
            "if" | "for"
                | "foreach"
                | "foreach-object"
                | "%"
                | "while"
                | "do"
                | "switch"
                | "try"
                | "catch"
                | "finally"
        );
    segment
        .iter()
        .enumerate()
        .skip(command_index)
        .any(|(index, token)| {
            if powershell_instance_file_deletion(token)
                && (index == command_index || expression_context)
            {
                return true;
            }
            let Some(class_offset) = powershell_static_file_deletion_offset(token) else {
                return false;
            };
            let direct_invocation = index == command_index
                && token[..class_offset]
                    .chars()
                    .all(|ch| matches!(ch, '$' | '(' | '{'));
            if direct_invocation {
                return true;
            }
            let assignment_starts_at_command = segment[command_index].starts_with('$')
                && (segment[command_index..index]
                    .iter()
                    .any(|prefix| prefix == "=")
                    || (index == command_index && token[..class_offset].contains('=')));
            assignment_starts_at_command || expression_context
        })
}

fn powershell_static_file_deletion_offset(token: &str) -> Option<usize> {
    let lower = token.to_ascii_lowercase();
    let (class_offset, class_name) = powershell_static_file_class_offset(&lower)?;
    let method = lower[class_offset + class_name.len()..].split('(').next()?;
    matches!(method, "delete" | "deletefile" | "deletedirectory").then_some(class_offset)
}

fn powershell_static_file_class_offset(lower: &str) -> Option<(usize, &'static str)> {
    [
        "[system.io.file]::",
        "[io.file]::",
        "[system.io.directory]::",
        "[io.directory]::",
        "[microsoft.visualbasic.fileio.filesystem]::",
    ]
    .iter()
    .find_map(|class_name| lower.find(class_name).map(|offset| (offset, *class_name)))
}

fn powershell_instance_file_deletion(token: &str) -> bool {
    token.to_ascii_lowercase().contains(".delete(")
}

fn segment_has_sensitive_interpreter_file_deletion(segment: &[String]) -> bool {
    let Some(command_index) = command_name_index(segment) else {
        return false;
    };
    if wrapper_payload_matches(
        segment,
        command_index,
        shell_command_has_sensitive_interpreter_file_deletion,
    ) {
        return true;
    }
    if !is_inline_code_interpreter(&segment[command_index]) {
        return false;
    }
    segment
        .iter()
        .skip(command_index + 1)
        .any(|token| interpreter_code_destroys_files(token))
}

fn is_inline_code_interpreter(token: &str) -> bool {
    let command_name = normalized_command_name(token);
    is_python_interpreter_name(&command_name)
        || matches!(
            command_name.as_str(),
            "node" | "nodejs" | "deno" | "bun" | "perl" | "ruby" | "php"
        )
}

/// Deletion, truncation, and dynamic-execution APIs only. Write,
/// copy, rename, mkdir, and permission APIs are deliberately absent:
/// ordinary edits are allowed in auto mode; only unrecoverable
/// destruction (and code loading that could hide it) needs approval.
fn interpreter_code_destroys_files(token: &str) -> bool {
    let code = token.to_ascii_lowercase();
    const DESTRUCTIVE_APIS: &[&str] = &[
        // Python deletion
        "os.remove",
        "os.unlink",
        "os.rmdir",
        "os.removedirs",
        "shutil.rmtree",
        "send2trash(",
        ".unlink(",
        ".rmdir(",
        // Python data loss
        "os.truncate",
        "os.ftruncate",
        // Python dynamic execution
        "runpy.run_path(",
        "runpy.run_module(",
        "exec(",
        // Node / Deno / Bun
        "fs.unlink",
        "fs.rm",
        "fs.truncate",
        ".unlinksync(",
        ".rmsync(",
        ".rmdirsync(",
        ".truncatesync(",
        "promises.rm",
        "promises.unlink",
        "deno.remove",
        // Ruby / PHP / Perl
        "file.delete",
        "fileutils.rm",
        "unlink(",
    ];
    if DESTRUCTIVE_APIS.iter().any(|needle| code.contains(needle)) {
        return true;
    }
    interpreter_code_launches_deletion(&code)
}

fn interpreter_code_launches_deletion(code: &str) -> bool {
    const PROCESS_APIS: &[&str] = &[
        "os.system(",
        "os.popen(",
        "subprocess.run(",
        "subprocess.call(",
        "subprocess.check_call(",
        "subprocess.check_output(",
        "subprocess.popen(",
    ];
    const DELETION_COMMANDS: &[&str] = &[
        "rm ",
        "rm'",
        "rm\"",
        "del ",
        "del'",
        "del\"",
        "erase ",
        "rmdir ",
        "rd ",
        "unlink ",
        "remove-item ",
    ];
    PROCESS_APIS.iter().any(|api| code.contains(api))
        && DELETION_COMMANDS
            .iter()
            .any(|command| code.contains(command))
}

fn split_workflow_pipeline_segments(command: &str) -> (Vec<Vec<String>>, Vec<String>) {
    let mut segments = Vec::new();
    let mut separators: Vec<String> = Vec::new();
    let mut segment = Vec::new();
    for token in workflow_shell_tokens(command) {
        if token == "&"
            && segment.is_empty()
            && separators
                .last()
                .is_some_and(|separator| workflow_pipeline_separator(separator))
        {
            continue;
        }
        if is_shell_separator(&token) {
            segments.push(std::mem::take(&mut segment));
            separators.push(token);
        } else {
            segment.push(token);
        }
    }
    segments.push(segment);
    (segments, separators)
}

fn workflow_pipeline_separator(token: &str) -> bool {
    matches!(token, "|" | "|&")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowEchoSemantics {
    Posix,
    PowerShell,
    Both,
}

fn workflow_shell_host_for_segment(segment: &[String]) -> WorkflowShellHost {
    let Some(index) = workflow_executed_command_index(segment) else {
        return WorkflowShellHost::Unknown;
    };
    if ["pwsh", "powershell", "invoke-expression", "iex"]
        .iter()
        .any(|name| workflow_command_name_matches(&segment[index], name))
    {
        WorkflowShellHost::PowerShell
    } else if ["bash", "sh", "zsh", "dash", "ksh", "busybox"]
        .iter()
        .any(|name| workflow_command_name_matches(&segment[index], name))
    {
        WorkflowShellHost::Posix
    } else {
        WorkflowShellHost::Unknown
    }
}

fn workflow_static_shell_pipeline_is_sensitive(command: &str, host: WorkflowShellHost) -> bool {
    let (segments, separators) = split_workflow_pipeline_segments(command);
    separators.iter().enumerate().any(|(index, separator)| {
        workflow_pipeline_separator(separator)
            && segments
                .get(index + 1)
                .is_some_and(|segment| segment_command_is_shell_interpreter(segment))
            && workflow_static_pipeline_input_payloads(&segments, &separators, index, host)
                .into_iter()
                .any(|payload| {
                    let payload_host = segments
                        .get(index + 1)
                        .map_or(WorkflowShellHost::Unknown, |segment| {
                            workflow_shell_host_for_segment(segment)
                        });
                    shell_command_has_workflow_sensitive_git_operation_for_host(
                        &payload,
                        payload_host,
                    )
                })
    })
}

fn workflow_static_pipeline_input_payloads(
    segments: &[Vec<String>],
    separators: &[String],
    mut segment_index: usize,
    host: WorkflowShellHost,
) -> Vec<String> {
    let echo_semantics = match host {
        WorkflowShellHost::Posix => WorkflowEchoSemantics::Posix,
        WorkflowShellHost::PowerShell => WorkflowEchoSemantics::PowerShell,
        WorkflowShellHost::Unknown => {
            let sink_is_invoke_expression = segments
                .get(segment_index + 1)
                .and_then(|segment| {
                    workflow_executed_command_index(segment).map(|index| (&segment[index], index))
                })
                .is_some_and(|(command, _)| {
                    ["invoke-expression", "iex"]
                        .iter()
                        .any(|name| workflow_command_name_matches(command, name))
                });
            if sink_is_invoke_expression {
                WorkflowEchoSemantics::Both
            } else {
                WorkflowEchoSemantics::Posix
            }
        }
    };
    loop {
        let Some(segment) = segments.get(segment_index) else {
            return Vec::new();
        };
        let payloads = workflow_static_pipeline_payloads(segment, echo_semantics);
        if !payloads.is_empty() {
            return payloads;
        }
        if segment_index == 0
            || !separators
                .get(segment_index - 1)
                .is_some_and(|separator| workflow_pipeline_separator(separator))
            || !workflow_static_pipeline_segment_is_transparent(segment)
        {
            return Vec::new();
        }
        segment_index -= 1;
    }
}

fn workflow_static_pipeline_segment_is_transparent(segment: &[String]) -> bool {
    let Some(command_index) = workflow_executed_command_index(segment) else {
        return false;
    };
    if !workflow_command_name_matches(&segment[command_index], "cat") {
        return false;
    }
    let Some(args) = segment.get(command_index + 1..) else {
        return false;
    };
    let mut options = true;
    for raw_arg in args {
        let arg = workflow_unescape_static_token(raw_arg);
        if options && arg == "--" {
            options = false;
            continue;
        }
        if arg == "-" {
            continue;
        }
        if options
            && arg
                .strip_prefix('-')
                .is_some_and(|cluster| !cluster.is_empty() && cluster.chars().all(|ch| ch == 'u'))
        {
            continue;
        }
        return false;
    }
    true
}

fn workflow_static_pipeline_payloads(
    segment: &[String],
    echo_semantics: WorkflowEchoSemantics,
) -> Vec<String> {
    let Some(command_index) = workflow_executed_command_index(segment) else {
        return Vec::new();
    };
    let args = &segment[command_index + 1..];
    if workflow_command_name_matches(&segment[command_index], "echo") {
        return match echo_semantics {
            WorkflowEchoSemantics::Posix => vec![workflow_static_echo_output(args)],
            WorkflowEchoSemantics::PowerShell => workflow_static_write_output_records(args),
            WorkflowEchoSemantics::Both => {
                let mut payloads = vec![workflow_static_echo_output(args)];
                payloads.extend(workflow_static_write_output_records(args));
                payloads
            }
        };
    }
    if workflow_command_name_matches(&segment[command_index], "write-output")
        || workflow_command_name_matches(&segment[command_index], "write")
    {
        return workflow_static_write_output_records(args);
    }
    if workflow_command_name_matches(&segment[command_index], "printf") {
        return workflow_static_printf_output(args).into_iter().collect();
    }
    if workflow_command_name_matches(&segment[command_index], "cat") {
        return workflow_here_string_payload(args).into_iter().collect();
    }
    Vec::new()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkflowWriteOutputParameter {
    InputObject(Option<String>),
    NoEnumerate(bool),
}

fn workflow_static_write_output_records(args: &[String]) -> Vec<String> {
    let mut output = Vec::new();
    let mut no_enumerate = false;
    let mut has_comma = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if let Some(consumes_next) = workflow_powershell_common_parameter(arg) {
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        match workflow_write_output_parameter(arg) {
            Some(WorkflowWriteOutputParameter::NoEnumerate(enabled)) => {
                no_enumerate = enabled;
            }
            Some(WorkflowWriteOutputParameter::InputObject(Some(attached_value))) => {
                output.push(attached_value);
            }
            Some(WorkflowWriteOutputParameter::InputObject(None)) => {}
            None if arg == "," => has_comma = true,
            None if workflow_unescape_static_token(arg).starts_with('-') => return Vec::new(),
            None => output.push(arg.clone()),
        }
        index += 1;
    }
    if no_enumerate && (has_comma || output.len() > 1) {
        Vec::new()
    } else {
        output
    }
}

const WORKFLOW_POWERSHELL_COMMON_VALUE_PARAMETERS: &[&str] = &[
    "erroraction",
    "errorvariable",
    "warningaction",
    "warningvariable",
    "informationaction",
    "informationvariable",
    "progressaction",
    "outvariable",
    "outbuffer",
    "pipelinevariable",
];
const WORKFLOW_POWERSHELL_COMMON_SWITCH_PARAMETERS: &[&str] = &["verbose", "debug"];
const WORKFLOW_POWERSHELL_WRITE_OUTPUT_PARAMETERS: &[&str] = &["inputobject", "noenumerate"];

fn workflow_powershell_unique_parameter<'a>(
    name: &str,
    command_parameters: &'a [&'a str],
) -> Option<&'a str> {
    if name.is_empty() {
        return None;
    }
    let mut matched = None;
    for candidate in command_parameters
        .iter()
        .chain(WORKFLOW_POWERSHELL_COMMON_VALUE_PARAMETERS)
        .chain(WORKFLOW_POWERSHELL_COMMON_SWITCH_PARAMETERS)
        .copied()
        .filter(|candidate| candidate.starts_with(name))
    {
        if matched.is_some() {
            return None;
        }
        matched = Some(candidate);
    }
    matched
}

fn workflow_write_output_parameter(arg: &str) -> Option<WorkflowWriteOutputParameter> {
    let parameter = workflow_unescape_static_token(arg);
    let (raw_name, attached_value) = parameter
        .split_once(':')
        .map_or((parameter.as_str(), None), |(name, value)| {
            (name, Some(value.to_owned()))
        });
    let name = raw_name.strip_prefix('-')?.to_ascii_lowercase();
    match workflow_powershell_unique_parameter(&name, WORKFLOW_POWERSHELL_WRITE_OUTPUT_PARAMETERS)?
    {
        "noenumerate" => Some(WorkflowWriteOutputParameter::NoEnumerate(
            match attached_value.as_deref() {
                None => true,
                Some(value) => value.eq_ignore_ascii_case("$true"),
            },
        )),
        "inputobject" => Some(WorkflowWriteOutputParameter::InputObject(attached_value)),
        _ => None,
    }
}

fn workflow_powershell_common_parameter(arg: &str) -> Option<bool> {
    let parameter = workflow_unescape_static_token(arg);
    let (raw_name, has_attached_value) = parameter
        .split_once(':')
        .map_or((parameter.as_str(), false), |(name, _)| (name, true));
    let name = raw_name.strip_prefix('-')?.to_ascii_lowercase();
    let alias_takes_value = match name.as_str() {
        "ea" | "ev" | "wa" | "wv" | "infa" | "iv" | "proga" | "ov" | "ob" | "pv" => Some(true),
        "vb" | "db" => Some(false),
        _ => None,
    };
    let takes_value = alias_takes_value.or_else(|| {
        workflow_powershell_unique_parameter(&name, &[])
            .map(|parameter| WORKFLOW_POWERSHELL_COMMON_VALUE_PARAMETERS.contains(&parameter))
    })?;
    Some(takes_value && !has_attached_value)
}

fn workflow_here_string_payload(args: &[String]) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        if arg == "<<<" {
            return args.get(index + 1..).map(|payload| payload.join(" "));
        }
        if let Some(payload_start) = arg.strip_prefix("<<<").filter(|value| !value.is_empty()) {
            let mut payload = payload_start.to_owned();
            if let Some(rest) = args.get(index + 1..).filter(|rest| !rest.is_empty()) {
                payload.push(' ');
                payload.push_str(&rest.join(" "));
            }
            return Some(payload);
        }
    }
    None
}

fn workflow_static_echo_output(args: &[String]) -> String {
    let mut index = 0;
    let mut decode_escapes = false;
    while let Some(option) = args.get(index).and_then(|arg| arg.strip_prefix('-')) {
        if option.is_empty() || !option.chars().all(|ch| matches!(ch, 'n' | 'e' | 'E')) {
            break;
        }
        for flag in option.chars() {
            if flag == 'e' {
                decode_escapes = true;
            } else if flag == 'E' {
                decode_escapes = false;
            }
        }
        index += 1;
    }
    let output = args[index..].join(" ");
    if decode_escapes {
        workflow_decode_static_backslash_escapes(&output)
    } else {
        output
    }
}

fn workflow_static_printf_output(args: &[String]) -> Option<String> {
    let args = if args
        .first()
        .is_some_and(|arg| workflow_unescape_static_token(arg) == "--")
    {
        &args[1..]
    } else {
        args
    };
    let format = workflow_decode_static_backslash_escapes(args.first()?);
    let values = &args[1..];
    let mut output = String::new();
    let mut value_index = 0;
    loop {
        let (rendered, consumed) =
            workflow_render_static_printf_format(&format, &values[value_index..])?;
        output.push_str(&rendered);
        value_index += consumed;
        if value_index >= values.len() || consumed == 0 {
            break;
        }
    }
    Some(output)
}

fn workflow_render_static_printf_format(
    format: &str,
    values: &[String],
) -> Option<(String, usize)> {
    let mut output = String::new();
    let mut chars = format.chars();
    let mut consumed = 0;
    while let Some(ch) = chars.next() {
        if ch != '%' {
            output.push(ch);
            continue;
        }
        match chars.next()? {
            '%' => output.push('%'),
            's' => {
                if let Some(value) = values.get(consumed) {
                    output.push_str(value);
                    consumed += 1;
                }
            }
            'b' => {
                if let Some(value) = values.get(consumed) {
                    output.push_str(&workflow_decode_static_backslash_escapes(value));
                    consumed += 1;
                }
            }
            _ => return None,
        }
    }
    Some((output, consumed))
}

fn workflow_decode_static_backslash_escapes(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            workflow_push_ansi_c_escape(&mut chars, &mut output);
        } else {
            output.push(ch);
        }
    }
    output
}

fn split_pipeline_segments(command: &str) -> (Vec<Vec<String>>, Vec<String>) {
    let mut segments = Vec::new();
    let mut separators = Vec::new();
    let mut segment = Vec::new();
    for token in shell_tokens(command) {
        if is_shell_separator(&token) {
            segments.push(std::mem::take(&mut segment));
            separators.push(token);
        } else {
            segment.push(token);
        }
    }
    segments.push(segment);
    (segments, separators)
}

fn git_segment_emits_committed_content(segment: &[String]) -> bool {
    let Some(git_index) = git_command_index(segment) else {
        return false;
    };
    let Some(subcommand_index) = git_subcommand_index(segment, git_index) else {
        return false;
    };
    match segment[subcommand_index].to_ascii_lowercase().as_str() {
        "cat-file" => true,
        "show" => segment[subcommand_index + 1..]
            .iter()
            .take_while(|token| !token_contains_file_redirect(token))
            .any(|token| is_git_rev_path_spec(token)),
        _ => false,
    }
}

fn is_git_rev_path_spec(token: &str) -> bool {
    if token.starts_with('-') {
        return false;
    }
    let Some((rev, path)) = token.split_once(':') else {
        return false;
    };
    if rev.is_empty() || path.is_empty() {
        return false;
    }
    // `D:/out.txt`-style drive-letter paths are not rev:path specs.
    !(rev.len() == 1
        && rev.chars().all(|ch| ch.is_ascii_alphabetic())
        && (path.starts_with('/') || path.starts_with('\\')))
}

fn segment_has_file_redirect(segment: &[String]) -> bool {
    segment
        .iter()
        .any(|token| token_contains_file_redirect(token))
}

fn token_contains_file_redirect(token: &str) -> bool {
    // The tokenizer keeps `>` inside ordinary tokens: standalone
    // (`>`), prefixed (`>out.cs`) or fused (`HEAD:f>out.cs`).
    // `2>&1`-style FD duplication doesn't write a file.
    token.contains('>') && !token.ends_with(">&1") && !token.ends_with(">&2")
}

fn segment_is_write_sink(segment: &[String]) -> bool {
    let Some(command_index) = command_name_index(segment) else {
        return false;
    };
    matches!(
        normalized_command_name(&segment[command_index]).as_str(),
        "tee" | "tee-object" | "out-file" | "set-content" | "add-content" | "sponge" | "dd"
    )
}

/// Shell wrappers that can hide a sensitive payload inside a single
/// quoted argument (`pwsh -Command "Remove-Item x"`), invisible to
/// the token-level scans above.
fn is_shell_wrapper_command_name(token: &str) -> bool {
    matches!(
        normalized_command_name(token).as_str(),
        "pwsh"
            | "powershell"
            | "bash"
            | "sh"
            | "zsh"
            | "dash"
            | "ksh"
            | "busybox"
            | "cmd"
            | "forfiles"
            | "start"
            | "call"
    )
}

fn wrapper_payloads(segment: &[String], command_index: usize) -> Vec<String> {
    if !is_shell_wrapper_command_name(&segment[command_index]) {
        return Vec::new();
    }
    let command_name = normalized_command_name(&segment[command_index]);
    if command_name == "start" {
        let args = &segment[command_index + 1..];
        return args
            .iter()
            .enumerate()
            .filter(|(_, arg)| !arg.starts_with('/'))
            .map(|(offset, _)| args[offset..].join(" "))
            .collect();
    }

    let mut payloads = Vec::new();
    if command_name == "cmd" {
        let args = &segment[command_index + 1..];
        for (offset, arg) in args.iter().enumerate() {
            let option = workflow_unescape_static_token(arg);
            let lower = option.to_ascii_lowercase();
            if (lower.starts_with("/c") || lower.starts_with("/k")) && option.len() > 2 {
                let mut payload = option[2..].to_owned();
                if offset + 1 < args.len() {
                    payload.push(' ');
                    payload.push_str(&args[offset + 1..].join(" "));
                }
                payloads.push(payload);
            }
        }
    }
    payloads.extend(
        segment
            .iter()
            .skip(command_index + 1)
            .filter(|token| token.contains(char::is_whitespace))
            .cloned(),
    );
    if let Some(payload_start) = wrapper_payload_start(segment, command_index) {
        if payload_start < segment.len() {
            payloads.push(segment[payload_start..].join(" "));
        }
    }
    payloads
}

/// Re-scan quoted wrapper payloads with `predicate`. The tokenizer
/// strips one level of quoting per pass, so nesting terminates.
fn wrapper_payload_matches(
    segment: &[String],
    command_index: usize,
    predicate: fn(&str) -> bool,
) -> bool {
    wrapper_payloads(segment, command_index)
        .into_iter()
        .any(|payload| predicate(&payload))
}

fn wrapper_payload_start(segment: &[String], command_index: usize) -> Option<usize> {
    let command_name = normalized_command_name(&segment[command_index]);
    let args = &segment[command_index + 1..];
    let marker_offset = match command_name.as_str() {
        "cmd" => args
            .iter()
            .position(|arg| matches!(arg.to_ascii_lowercase().as_str(), "/c" | "/k")),
        "pwsh" | "powershell" => args.iter().position(|arg| {
            matches!(
                arg.to_ascii_lowercase().as_str(),
                "-command" | "-c" | "-commandwithargs"
            )
        }),
        "bash" | "sh" | "zsh" | "dash" | "ksh" | "busybox" => args
            .iter()
            .position(|arg| matches!(arg.to_ascii_lowercase().as_str(), "-c" | "--command")),
        "forfiles" => args.iter().position(|arg| arg.eq_ignore_ascii_case("/c")),
        "call" => return Some(command_index + 1),
        _ => None,
    }?;
    Some(command_index + 2 + marker_offset)
}

fn segment_wrapper_payload_matches(segment: &[String], predicate: fn(&str) -> bool) -> bool {
    command_name_index(segment)
        .is_some_and(|index| wrapper_payload_matches(segment, index, predicate))
}

fn is_workflow_shell_separator(token: &str) -> bool {
    is_shell_separator(token) || matches!(token, "(" | ")" | "{" | "}")
}

fn is_shell_separator(token: &str) -> bool {
    matches!(token, "&&" | "||" | "|" | "|&" | ";" | "\n" | "&")
}

fn segment_has_sensitive_file_deletion(segment: &[String]) -> bool {
    let Some(command_index) = command_name_index(segment) else {
        return false;
    };
    if wrapper_payload_matches(
        segment,
        command_index,
        shell_command_has_sensitive_file_deletion,
    ) {
        return true;
    }
    match normalized_command_name(&segment[command_index]).as_str() {
        "rm"
        | "unlink"
        | "rmdir"
        | "del"
        | "erase"
        | "deltree"
        | "rd"
        | "ri"
        | "remove-item"
        | "remove-itemproperty"
        | "remove-content"
        | "clear-content"
        | "clear-recyclebin"
        | "sdelete"
        | "sdelete64" => !segment_has_noop_flag(segment),
        "git" => git_segment_has_sensitive_file_deletion(segment, command_index),
        "find" => find_segment_has_file_deletion(segment, command_index),
        "xargs" => xargs_segment_has_file_deletion(segment, command_index),
        "pwsh" | "powershell" => false,
        "if" | "for" | "foreach" | "foreach-object" | "%" | "while" | "do" | "switch" | "try"
        | "catch" | "finally" => control_flow_segment_has_file_deletion(segment, command_index),
        "robocopy" => robocopy_segment_deletes_destination_entries(segment, command_index),
        _ => false,
    }
}

fn control_flow_segment_has_file_deletion(segment: &[String], command_index: usize) -> bool {
    if segment_has_noop_flag(segment) {
        return false;
    }
    segment
        .iter()
        .skip(command_index + 1)
        .any(|token| is_file_deletion_command_name(token))
        || segment
            .iter()
            .skip(command_index + 1)
            .any(|token| powershell_instance_file_deletion(token))
}

fn robocopy_segment_deletes_destination_entries(segment: &[String], command_index: usize) -> bool {
    let args = &segment[command_index + 1..];
    !args.iter().any(|arg| arg.eq_ignore_ascii_case("/l"))
        && args.iter().any(|arg| {
            matches!(
                arg.to_ascii_lowercase().as_str(),
                "/mir" | "/purge" | "/mov" | "/move"
            )
        })
}

fn command_name_index(segment: &[String]) -> Option<usize> {
    let mut index = 0;
    while let Some(token) = segment.get(index) {
        let lower = token.to_ascii_lowercase();
        if is_env_assignment(token)
            || matches!(lower.as_str(), "sudo" | "command" | "builtin" | "time")
        {
            index += 1;
            continue;
        }
        if lower == "env" {
            index += 1;
            while segment
                .get(index)
                .is_some_and(|token| token.starts_with('-') || is_env_assignment(token))
            {
                index += 1;
            }
            continue;
        }
        return Some(index);
    }
    None
}

const WORKFLOW_ENV_LONG_OPTIONS: &[&str] = &[
    "argv0",
    "ignore-environment",
    "null",
    "unset",
    "chdir",
    "split-string",
    "block-signal",
    "default-signal",
    "ignore-signal",
    "list-signal-handling",
    "debug",
    "help",
    "version",
];

fn workflow_env_unique_long_option(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return None;
    }
    let mut matches = WORKFLOW_ENV_LONG_OPTIONS
        .iter()
        .copied()
        .filter(|candidate| candidate.starts_with(name));
    let matched = matches.next()?;
    matches.next().is_none().then_some(matched)
}

fn workflow_env_long_option_takes_value(option: &str) -> bool {
    let Some(name) = option.strip_prefix("--") else {
        return false;
    };
    matches!(
        workflow_env_unique_long_option(name),
        Some("argv0" | "unset" | "chdir" | "split-string")
    )
}

fn command_prefix_option_takes_value(prefix: &str, option: &str) -> bool {
    let unescaped = workflow_unescape_static_token(option);
    let option = unescaped
        .split_once('=')
        .map(|(option, _)| option)
        .unwrap_or(&unescaped);
    let lower = option.to_ascii_lowercase();
    let exact_match = match prefix {
        "sudo" => {
            matches!(
                option,
                "-u" | "-g" | "-h" | "-p" | "-C" | "-T" | "-R" | "-D" | "-r" | "-t"
            ) || matches!(
                lower.as_str(),
                "--user"
                    | "--group"
                    | "--host"
                    | "--prompt"
                    | "--close-from"
                    | "--command-timeout"
                    | "--chroot"
                    | "--chdir"
                    | "--role"
                    | "--type"
            )
        }
        "time" => matches!(lower.as_str(), "-f" | "--format" | "-o" | "--output"),
        "exec" => lower == "-a",
        "env" => {
            matches!(option, "-a" | "-u" | "-C" | "-S")
                || workflow_env_long_option_takes_value(option)
        }
        "nice" => matches!(lower.as_str(), "-n" | "--adjustment"),
        "timeout" => matches!(lower.as_str(), "-k" | "--kill-after" | "-s" | "--signal"),
        _ => false,
    };
    if exact_match || option.starts_with("--") {
        return exact_match;
    }

    let value_options: &[char] = match prefix {
        "sudo" => &['u', 'g', 'h', 'p', 'C', 'T', 'R', 'D', 'r', 't'],
        "time" => &['f', 'o'],
        "exec" => &['a'],
        "env" => &['a', 'u', 'C', 'S'],
        "nice" => &['n'],
        "timeout" => &['k', 's'],
        _ => &[],
    };
    let Some(cluster) = option
        .strip_prefix('-')
        .filter(|cluster| !cluster.is_empty())
    else {
        return false;
    };
    cluster
        .char_indices()
        .find(|(_, flag)| value_options.contains(flag))
        .is_some_and(|(offset, flag)| offset + flag.len_utf8() == cluster.len())
}

fn workflow_command_prefix_is_query_only(prefix: &str, option: &str) -> bool {
    if prefix != "command" {
        return false;
    }
    let option = workflow_unescape_static_token(option);
    option
        .strip_prefix('-')
        .filter(|cluster| !cluster.starts_with('-'))
        .is_some_and(|cluster| cluster.chars().any(|ch| matches!(ch, 'v' | 'V')))
}

fn normalized_command_name(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    let file_name = lower.rsplit(['/', '\\']).next().unwrap_or(&lower);
    file_name
        .strip_suffix(".exe")
        .unwrap_or(file_name)
        .trim_start_matches("microsoft.powershell.management\\")
        .to_owned()
}

fn workflow_unescape_static_token(token: &str) -> String {
    let mut output = String::with_capacity(token.len());
    let mut chars = token.chars();
    while let Some(ch) = chars.next() {
        if matches!(ch, '\\' | '`') {
            if let Some(next) = chars.next() {
                output.push(next);
            }
        } else {
            output.push(ch);
        }
    }
    output
}

fn workflow_command_name_matches(token: &str, expected: &str) -> bool {
    normalized_command_name(token) == expected
        || normalized_command_name(&workflow_unescape_static_token(token)) == expected
}

fn segment_has_noop_flag(segment: &[String]) -> bool {
    segment.iter().any(|token| {
        let lower = token.to_ascii_lowercase();
        matches!(lower.as_str(), "--help" | "-h" | "-?" | "/?" | "-whatif")
            || matches!(
                lower.as_str(),
                "-whatif:$true" | "-whatif:true" | "-whatif:1"
            )
    })
}

fn git_segment_has_sensitive_file_deletion(segment: &[String], git_index: usize) -> bool {
    let Some(subcommand_index) = git_subcommand_index(segment, git_index) else {
        return false;
    };
    match segment[subcommand_index].to_ascii_lowercase().as_str() {
        "rm" => true,
        "clean" => git_clean_is_forced_without_dry_run(&segment[subcommand_index + 1..]),
        "checkout" | "restore" => git_worktree_wide_restore(&segment[subcommand_index + 1..]),
        _ => false,
    }
}

fn git_clean_is_forced_without_dry_run(args: &[String]) -> bool {
    let mut forced = false;
    let mut dry_run = false;
    for arg in args {
        let lower = arg.to_ascii_lowercase();
        if matches!(lower.as_str(), "-n" | "--dry-run") {
            dry_run = true;
        }
        if lower == "-f" || lower == "--force" {
            forced = true;
        }
        if lower.starts_with('-') && !lower.starts_with("--") && lower.contains('f') {
            forced = true;
        }
        if lower.starts_with('-') && !lower.starts_with("--") && lower.contains('n') {
            dry_run = true;
        }
    }
    forced && !dry_run
}

fn git_worktree_wide_restore(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "." | ":/" | "./"))
}

fn git_segment_has_sensitive_worktree_change(segment: &[String]) -> bool {
    if segment_wrapper_payload_matches(segment, shell_command_has_sensitive_git_worktree) {
        return true;
    }
    let Some(git_index) = git_command_index(segment) else {
        return false;
    };
    let Some(subcommand_index) = git_subcommand_index(segment, git_index) else {
        return false;
    };
    let args = &segment[subcommand_index + 1..];
    if git_args_are_help_only(args) {
        return false;
    }

    match segment[subcommand_index].to_ascii_lowercase().as_str() {
        "checkout" | "restore" | "switch" | "reset" | "merge" | "rebase" | "cherry-pick"
        | "revert" | "pull" | "am" => true,
        "apply" => git_apply_is_mutating(args),
        "clean" => git_clean_is_forced_without_dry_run(args),
        "rm" => true,
        _ => false,
    }
}

fn git_segment_has_workflow_sensitive_operation(
    segment: &[String],
    host: WorkflowShellHost,
) -> bool {
    if workflow_git_command_indices(segment)
        .into_iter()
        .any(|git_index| git_environment_alias_is_configured(segment, git_index))
    {
        return true;
    }
    if let Some(sensitive) = workflow_env_split_string_payload_match(
        segment,
        shell_command_has_workflow_sensitive_git_operation,
    ) {
        return sensitive;
    }
    if workflow_segment_wrapper_payload_is_sensitive(segment)
        || workflow_dynamic_shell_payload_is_sensitive(segment, host)
        || workflow_shell_stdin_payload_is_sensitive(segment)
        || workflow_argv_executor_payload_is_sensitive(segment, host)
    {
        return true;
    }
    workflow_git_command_indices(segment)
        .into_iter()
        .any(|git_index| git_invocation_has_workflow_sensitive_operation(segment, git_index))
}

fn workflow_env_split_string_payload_match(
    segment: &[String],
    predicate: fn(&str) -> bool,
) -> Option<bool> {
    segment.iter().enumerate().find_map(|(env_index, token)| {
        if !workflow_command_name_matches(token, "env") {
            return None;
        }
        let mut prefix_probe = segment[..=env_index].to_vec();
        prefix_probe[env_index] = "env".to_owned();
        prefix_probe.push("__workflow_command__".to_owned());
        if workflow_executed_command_index(&prefix_probe) != Some(env_index + 1) {
            return None;
        }
        workflow_env_split_string_payload(&segment[env_index + 1..])
            .map(|payload| predicate(&payload))
    })
}

const WORKFLOW_ENV_SPLIT_MAX_DEPTH: usize = 16;

fn workflow_env_split_string_payload(args: &[String]) -> Option<String> {
    workflow_env_command_payload(args, 0)
}

fn workflow_env_command_payload(args: &[String], split_depth: usize) -> Option<String> {
    let mut index = 0;
    while let Some(raw_option) = args.get(index) {
        let option = workflow_unescape_static_token(raw_option);
        if is_env_assignment(&option) {
            index += 1;
            continue;
        }
        if option == "--" {
            return workflow_join_static_command_args(&args[index + 1..]);
        }
        if option == "-" {
            index += 1;
            continue;
        }
        if !option.starts_with('-') {
            return workflow_join_static_command_args(&args[index..]);
        }
        if let Some(long_option) = option.strip_prefix("--") {
            let (name, attached_value) = long_option
                .split_once('=')
                .map_or((long_option, None), |(name, value)| (name, Some(value)));
            let normalized_option = workflow_env_unique_long_option(name);
            if matches!(normalized_option, Some("help" | "version")) {
                return Some(String::new());
            }
            if normalized_option == Some("split-string") {
                if split_depth >= WORKFLOW_ENV_SPLIT_MAX_DEPTH {
                    return None;
                }
                return if let Some(payload) = attached_value {
                    workflow_join_env_split_command(payload, &args[index + 1..], split_depth + 1)
                } else {
                    let payload = args.get(index + 1)?;
                    workflow_join_env_split_command(payload, &args[index + 2..], split_depth + 1)
                };
            }
        } else if let Some(cluster) = option.strip_prefix('-') {
            for (offset, flag) in cluster.char_indices() {
                if flag == 'S' {
                    if split_depth >= WORKFLOW_ENV_SPLIT_MAX_DEPTH {
                        return None;
                    }
                    let value_start = offset + flag.len_utf8();
                    let attached_value = &cluster[value_start..];
                    return if attached_value.is_empty() {
                        let payload = args.get(index + 1)?;
                        workflow_join_env_split_command(
                            payload,
                            &args[index + 2..],
                            split_depth + 1,
                        )
                    } else {
                        workflow_join_env_split_command(
                            attached_value,
                            &args[index + 1..],
                            split_depth + 1,
                        )
                    };
                }
                if matches!(flag, 'a' | 'u' | 'C') {
                    break;
                }
            }
        }
        let takes_value = command_prefix_option_takes_value("env", &option);
        index += 1;
        if takes_value && !option.contains('=') {
            index += 1;
        }
    }
    None
}

fn workflow_join_env_split_command(
    command: &str,
    args: &[String],
    split_depth: usize,
) -> Option<String> {
    let mut split_args = workflow_env_split_string_args(command)?;
    split_args.extend(args.iter().cloned());
    workflow_env_command_payload(&split_args, split_depth)
}

fn workflow_join_static_command_args(args: &[String]) -> Option<String> {
    (!args.is_empty()).then(|| {
        args.iter()
            .map(|arg| workflow_quote_static_arg(arg))
            .collect::<Vec<_>>()
            .join(" ")
    })
}

fn workflow_env_split_string_args(command: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut quote = None;
    let mut started = false;

    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    current.push(ch);
                }
                started = true;
            }
            Some('"') => {
                if ch == '"' {
                    quote = None;
                } else if ch == '\\' {
                    let next = chars.next()?;
                    if next == '_' {
                        current.push(' ');
                    } else if !workflow_push_env_split_escape(next, &mut current) {
                        current.push('\\');
                        current.push(next);
                    }
                } else {
                    current.push(ch);
                }
                started = true;
            }
            Some(_) => unreachable!("env split-string quote is single or double"),
            None => match ch {
                '#' if !started => break,
                '\'' | '"' => {
                    quote = Some(ch);
                    started = true;
                }
                '\\' => {
                    let next = chars.next()?;
                    if next == '_' {
                        if started {
                            args.push(std::mem::take(&mut current));
                            started = false;
                        }
                    } else if next == 'c' {
                        break;
                    } else {
                        if !workflow_push_env_split_escape(next, &mut current) {
                            current.push('\\');
                            current.push(next);
                        }
                        started = true;
                    }
                }
                ch if ch.is_whitespace() => {
                    if started {
                        args.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                _ => {
                    current.push(ch);
                    started = true;
                }
            },
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        args.push(current);
    }
    Some(args)
}

fn workflow_push_env_split_escape(escaped: char, output: &mut String) -> bool {
    let value = match escaped {
        'a' => '\x07',
        'b' => '\x08',
        'f' => '\x0c',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\x0b',
        '#' => '#',
        '$' => '$',
        '\'' => '\'',
        '"' => '"',
        '\\' => '\\',
        _ => return false,
    };
    output.push(value);
    true
}

fn workflow_quote_static_arg(arg: &str) -> String {
    let escaped = arg
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    format!("\"{escaped}\"")
}

fn workflow_segment_wrapper_payload_is_sensitive(segment: &[String]) -> bool {
    let Some(index) = workflow_executed_command_index(segment) else {
        return false;
    };
    let Some(command_name) = workflow_shell_wrapper_name(&segment[index]) else {
        return false;
    };
    let payload_host = match command_name {
        "pwsh" | "powershell" => WorkflowShellHost::PowerShell,
        "bash" | "sh" | "zsh" | "dash" | "ksh" | "busybox" => WorkflowShellHost::Posix,
        _ => WorkflowShellHost::Unknown,
    };
    let mut normalized_segment = segment.to_vec();
    normalized_segment[index] = command_name.to_owned();
    wrapper_payloads(&normalized_segment, index)
        .into_iter()
        .any(|payload| {
            shell_command_has_workflow_sensitive_git_operation_for_host(&payload, payload_host)
        })
}

fn workflow_shell_wrapper_name(token: &str) -> Option<&'static str> {
    [
        "pwsh",
        "powershell",
        "bash",
        "sh",
        "zsh",
        "dash",
        "ksh",
        "busybox",
        "cmd",
        "forfiles",
        "start",
        "call",
    ]
    .into_iter()
    .find(|name| workflow_command_name_matches(token, name))
}

fn workflow_shell_stdin_payload_is_sensitive(segment: &[String]) -> bool {
    let Some(command_index) = workflow_executed_command_index(segment) else {
        return false;
    };
    let is_stdin_shell = ["bash", "sh", "zsh", "dash", "ksh", "busybox"]
        .iter()
        .any(|name| workflow_command_name_matches(&segment[command_index], name));
    is_stdin_shell
        && workflow_here_string_payload(&segment[command_index + 1..]).is_some_and(|payload| {
            shell_command_has_workflow_sensitive_git_operation_for_host(
                &payload,
                WorkflowShellHost::Posix,
            )
        })
}

fn workflow_dynamic_shell_payload_is_sensitive(
    segment: &[String],
    host: WorkflowShellHost,
) -> bool {
    let Some(command_index) = workflow_executed_command_index(segment) else {
        return false;
    };
    let is_invoke_expression = ["invoke-expression", "iex"]
        .iter()
        .any(|name| workflow_command_name_matches(&segment[command_index], name));
    if !is_invoke_expression && !workflow_command_name_matches(&segment[command_index], "eval") {
        return false;
    }
    let args = &segment[command_index + 1..];
    let payload = if is_invoke_expression {
        workflow_invoke_expression_payload(args)
    } else {
        args.join(" ")
    };
    let payload_host = if is_invoke_expression {
        WorkflowShellHost::PowerShell
    } else {
        host
    };
    !payload.is_empty()
        && shell_command_has_workflow_sensitive_git_operation_for_host(&payload, payload_host)
}

fn workflow_argv_executor_payload_is_sensitive(
    segment: &[String],
    host: WorkflowShellHost,
) -> bool {
    let Some(command_index) = workflow_executed_command_index(segment) else {
        return false;
    };
    if workflow_command_name_matches(&segment[command_index], "xargs") {
        return workflow_xargs_command(segment, command_index).is_some_and(
            |(index, replacement)| {
                let child = &segment[index..];
                workflow_substituted_argv_is_sensitive(child, replacement.as_deref(), host)
                    || workflow_xargs_appended_input_can_make_git_sensitive(child)
            },
        );
    }
    if workflow_command_name_matches(&segment[command_index], "find") {
        let mut index = command_index + 1;
        while index < segment.len() {
            let token = workflow_unescape_static_token(&segment[index]);
            if !matches!(token.as_str(), "-exec" | "-execdir") {
                index += 1;
                continue;
            }
            let command_start = index + 1;
            let command_end = segment[command_start..]
                .iter()
                .position(|token| {
                    matches!(workflow_unescape_static_token(token).as_str(), ";" | "+")
                })
                .map_or(segment.len(), |offset| command_start + offset);
            if command_start < command_end
                && workflow_substituted_argv_is_sensitive(
                    &segment[command_start..command_end],
                    Some("{}"),
                    host,
                )
            {
                return true;
            }
            index = command_end.saturating_add(1);
        }
    }
    false
}

fn workflow_substituted_argv_is_sensitive(
    child: &[String],
    replacement: Option<&str>,
    host: WorkflowShellHost,
) -> bool {
    if git_segment_has_workflow_sensitive_operation(child, host) {
        return true;
    }
    let Some(replacement) = replacement.filter(|replacement| !replacement.is_empty()) else {
        return false;
    };
    let Some(command_index) = workflow_executed_command_index(child) else {
        return false;
    };
    if workflow_unescape_static_token(&child[command_index]).contains(replacement) {
        return true;
    }
    let dynamic_arg = workflow_argv_contains_replacement(&child[command_index + 1..], replacement);
    if !dynamic_arg {
        return false;
    }
    if workflow_command_name_matches(&child[command_index], "git") {
        return workflow_git_replacement_can_be_sensitive(child, command_index, replacement);
    }
    workflow_shell_wrapper_name(&child[command_index]).is_some()
        || ["eval", "iex", "invoke-expression", "xargs", "find"]
            .iter()
            .any(|name| workflow_command_name_matches(&child[command_index], name))
}

fn workflow_argv_contains_replacement(args: &[String], replacement: &str) -> bool {
    args.iter()
        .any(|arg| workflow_unescape_static_token(arg).contains(replacement))
        || (replacement == "{}"
            && args.windows(2).any(|pair| {
                workflow_unescape_static_token(&pair[0]) == "{"
                    && workflow_unescape_static_token(&pair[1]) == "}"
            }))
}

fn workflow_git_replacement_can_be_sensitive(
    child: &[String],
    git_index: usize,
    replacement: &str,
) -> bool {
    let Some(subcommand_index) = git_subcommand_index(child, git_index) else {
        return true;
    };
    let subcommand = workflow_unescape_static_token(&child[subcommand_index]);
    subcommand.contains(replacement)
        || (workflow_git_subcommand_can_be_sensitive_with_dynamic_args(&subcommand)
            && workflow_argv_contains_replacement(&child[subcommand_index + 1..], replacement))
}

fn workflow_xargs_appended_input_can_make_git_sensitive(child: &[String]) -> bool {
    let Some(git_index) = workflow_executed_command_index(child) else {
        return false;
    };
    if !workflow_command_name_matches(&child[git_index], "git") {
        return false;
    }
    let Some(subcommand_index) = git_subcommand_index(child, git_index) else {
        return true;
    };
    workflow_git_subcommand_can_be_sensitive_with_dynamic_args(&child[subcommand_index])
}

fn workflow_git_subcommand_can_be_sensitive_with_dynamic_args(subcommand: &str) -> bool {
    matches!(
        workflow_unescape_static_token(subcommand)
            .to_ascii_lowercase()
            .as_str(),
        "apply"
            | "clean"
            | "rm"
            | "stash"
            | "push"
            | "branch"
            | "tag"
            | "fetch"
            | "remote"
            | "replace"
            | "notes"
            | "symbolic-ref"
            | "worktree"
    )
}

const WORKFLOW_XARGS_LONG_OPTIONS: &[&str] = &[
    "null",
    "arg-file",
    "delimiter",
    "eof",
    "replace",
    "max-lines",
    "max-args",
    "open-tty",
    "max-procs",
    "interactive",
    "process-slot-var",
    "no-run-if-empty",
    "max-chars",
    "show-limits",
    "verbose",
    "exit",
    "help",
    "version",
];

fn workflow_xargs_unique_long_option(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return None;
    }
    let mut matches = WORKFLOW_XARGS_LONG_OPTIONS
        .iter()
        .copied()
        .filter(|candidate| candidate.starts_with(name));
    let matched = matches.next()?;
    matches.next().is_none().then_some(matched)
}

fn workflow_xargs_command(
    segment: &[String],
    command_index: usize,
) -> Option<(usize, Option<String>)> {
    let mut index = command_index + 1;
    let mut replacement = None;
    while let Some(raw_option) = segment.get(index) {
        let option = workflow_unescape_static_token(raw_option);
        if option == "--" {
            return (index + 1 < segment.len()).then_some((index + 1, replacement));
        }
        if !option.starts_with('-') || option == "-" {
            return Some((index, replacement));
        }
        if let Some(long) = option.strip_prefix("--") {
            let (name, attached_value) = long
                .split_once('=')
                .map_or((long, None), |(name, value)| (name, Some(value)));
            let normalized = workflow_xargs_unique_long_option(name)?;
            if matches!(normalized, "help" | "version") {
                return None;
            }
            if normalized == "replace" {
                replacement = Some(attached_value.unwrap_or("{}").to_owned());
            }
            index += 1;
            if normalized == "replace"
                && replacement.as_deref().is_some_and(str::is_empty)
                && segment.get(index).is_some_and(|token| token == "{")
                && segment.get(index + 1).is_some_and(|token| token == "}")
            {
                replacement = Some("{}".to_owned());
                index += 2;
            }
            if attached_value.is_none()
                && matches!(
                    normalized,
                    "arg-file"
                        | "delimiter"
                        | "max-args"
                        | "max-chars"
                        | "max-procs"
                        | "process-slot-var"
                )
            {
                segment.get(index)?;
                index += 1;
            }
            continue;
        }

        let cluster = option.strip_prefix('-')?;
        let mut consumed = false;
        for (offset, flag) in cluster.char_indices() {
            let value_start = offset + flag.len_utf8();
            let attached_value = &cluster[value_start..];
            if matches!(
                flag,
                'a' | 'd' | 'E' | 'I' | 'J' | 'L' | 'n' | 'P' | 'R' | 'S' | 's'
            ) {
                let value = if attached_value.is_empty() {
                    index += 1;
                    if flag == 'I'
                        && segment.get(index).is_some_and(|token| token == "{")
                        && segment.get(index + 1).is_some_and(|token| token == "}")
                    {
                        index += 2;
                        "{}".to_owned()
                    } else {
                        let value = workflow_unescape_static_token(segment.get(index)?);
                        index += 1;
                        value
                    }
                } else {
                    index += 1;
                    attached_value.to_owned()
                };
                if flag == 'I' {
                    replacement = Some(value);
                }
                consumed = true;
                break;
            }
            if matches!(flag, 'e' | 'i' | 'l') {
                let split_replacement = flag == 'i'
                    && attached_value.is_empty()
                    && segment.get(index + 1).is_some_and(|token| token == "{")
                    && segment.get(index + 2).is_some_and(|token| token == "}");
                if flag == 'i' {
                    replacement = Some(if attached_value.is_empty() {
                        "{}".to_owned()
                    } else {
                        attached_value.to_owned()
                    });
                }
                index += if split_replacement { 3 } else { 1 };
                consumed = true;
                break;
            }
            if !matches!(flag, '0' | 'o' | 'p' | 'r' | 't' | 'x') {
                return None;
            }
        }
        if !consumed {
            index += 1;
        }
    }
    None
}

fn workflow_invoke_expression_payload(args: &[String]) -> String {
    let mut payload = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if let Some(consumes_next) = workflow_powershell_common_parameter(arg) {
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        let parameter = workflow_unescape_static_token(arg);
        let (raw_name, attached_value) = parameter
            .split_once(':')
            .map_or((parameter.as_str(), None), |(name, value)| {
                (name, Some(value))
            });
        let name = raw_name.strip_prefix('-').map(str::to_ascii_lowercase);
        let is_command_parameter = name.as_deref().is_some_and(|name| {
            workflow_powershell_unique_parameter(name, &["command"]) == Some("command")
        });
        if is_command_parameter {
            if let Some(value) = attached_value {
                payload.push(value.to_owned());
                index += 1;
            } else if let Some(value) = args.get(index + 1) {
                payload.push(value.clone());
                index += 2;
            } else {
                break;
            }
            continue;
        }
        payload.push(arg.clone());
        index += 1;
    }
    payload.join(" ")
}

fn git_invocation_has_workflow_sensitive_operation(segment: &[String], git_index: usize) -> bool {
    let Some(subcommand_index) = git_subcommand_index(segment, git_index) else {
        return false;
    };
    if git_inline_alias_is_configured(segment, git_index, subcommand_index)
        || git_environment_alias_is_configured(segment, git_index)
    {
        return true;
    }
    let args = &segment[subcommand_index + 1..];
    let subcommand =
        workflow_unescape_static_token(&segment[subcommand_index]).to_ascii_lowercase();
    if workflow_git_args_are_help_only(&subcommand, args) {
        return false;
    }

    match subcommand.as_str() {
        "restore" | "reset" | "merge" | "rebase" | "cherry-pick" | "revert" | "am" => true,
        "checkout" => git_checkout_is_workflow_sensitive(args),
        "switch" => git_switch_is_workflow_sensitive(args),
        "pull" => git_pull_is_workflow_sensitive(args),
        "apply" => git_apply_is_mutating(args),
        "clean" => !git_clean_args_are_dry_run(args),
        "rm" => !git_rm_args_are_dry_run(args),
        "stash" => !git_stash_args_are_read_only(args),
        "push" => {
            !git_push_args_are_dry_run(args)
                && (git_push_is_forced(args) || git_push_deletes_remote_ref(args))
        }
        "branch" => {
            git_branch_is_delete(args) || git_branch_is_forced(args) || git_branch_is_move(args)
        }
        "tag" => git_tag_is_delete(args) || git_tag_is_forced(args),
        "fetch" => {
            !git_fetch_args_are_dry_run(args)
                && (git_fetch_prunes_refs(args) || git_fetch_is_forced(args))
        }
        "remote" => git_remote_mutates_refs(args),
        "replace" => git_replace_args_are_mutating(args),
        "notes" => git_notes_mutates_refs(args),
        "checkout-index" => true,
        "update-ref" => true,
        "symbolic-ref" => git_symbolic_ref_is_mutating(args),
        "worktree" => git_worktree_args_are_mutating(args),
        _ => false,
    }
}

/// `git checkout` can overwrite tracked content in every form except
/// pure branch creation: `checkout -b <new> [start]` never touches the
/// working tree. Anything else — plain branch switches (ambiguous with
/// pathspec restores), `--`/pathspec forms, `-B`, force/merge/patch
/// variants — keeps the shared-worktree gate.
fn git_checkout_is_workflow_sensitive(args: &[String]) -> bool {
    let mut creates_new_branch = false;
    for arg in args {
        let arg = workflow_unescape_static_token(arg);
        let lower = arg.to_ascii_lowercase();
        if lower == "--" {
            return true;
        }
        if matches!(
            lower.as_str(),
            "--force" | "--merge" | "--ours" | "--theirs" | "--patch" | "--detach"
        ) || lower.starts_with("--pathspec-from-file")
        {
            return true;
        }
        if let Some(cluster) = arg.strip_prefix('-').filter(|rest| !rest.starts_with('-')) {
            // Short options may carry an attached value (`-bfeature`),
            // so only the first character names the option.
            match cluster.chars().next() {
                Some('b') => creates_new_branch = true,
                Some('B' | 'f' | 'm' | 'p') => return true,
                _ => {}
            }
        }
    }
    !creates_new_branch
}

/// `git switch` never takes pathspecs, and a plain branch switch refuses
/// to clobber local changes — only the force/discard/merge forms can
/// destroy shared work.
fn git_switch_is_workflow_sensitive(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| {
            let arg = workflow_unescape_static_token(arg);
            let lower = arg.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "--force" | "--discard-changes" | "--force-create" | "--merge"
            ) {
                return true;
            }
            arg.strip_prefix('-')
                .filter(|rest| !rest.starts_with('-'))
                // Short options may carry an attached value (`-cfeature`),
                // so only the first character names the option.
                .and_then(|cluster| cluster.chars().next())
                .is_some_and(|option| matches!(option, 'C' | 'f' | 'm'))
        })
}

/// Plain `git pull` aborts rather than clobbering local work; the
/// rebase/autostash/force forms rewrite local commits or move dirty
/// state, so those keep the shared-worktree gate.
fn git_pull_is_workflow_sensitive(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| {
            let arg = workflow_unescape_static_token(arg);
            let lower = arg.to_ascii_lowercase();
            if lower == "--rebase"
                || lower.starts_with("--rebase=")
                || lower == "--autostash"
                || lower == "--force"
            {
                return true;
            }
            if !arg.starts_with('-') && arg.starts_with('+') {
                // Forced refspec (`git pull origin +main`).
                return true;
            }
            arg.strip_prefix('-')
                .filter(|rest| !rest.starts_with('-'))
                .and_then(|cluster| cluster.chars().next())
                .is_some_and(|option| matches!(option, 'r' | 'f'))
        })
}

fn workflow_git_command_indices(segment: &[String]) -> Vec<usize> {
    let Some(command_index) = workflow_command_name_index(segment) else {
        return Vec::new();
    };
    let mut indices = Vec::new();
    if workflow_command_name_matches(&segment[command_index], "git") {
        indices.push(command_index);
    }
    if workflow_control_flow_marker(&segment[command_index]) {
        indices.extend(
            segment
                .iter()
                .enumerate()
                .skip(command_index + 1)
                .filter_map(|(index, token)| {
                    (workflow_command_name_matches(token, "git")
                        && workflow_git_follows_control_marker(segment, command_index, index))
                    .then_some(index)
                }),
        );
    }
    indices
}

fn workflow_command_name_index(segment: &[String]) -> Option<usize> {
    let mut index = 0;
    while let Some(token) = segment.get(index) {
        if matches!(token.as_str(), "(" | ")" | "{" | "}") || is_env_assignment(token) {
            index += 1;
            continue;
        }
        let lower = workflow_unescape_static_token(token).to_ascii_lowercase();
        if lower == "!" {
            index += 1;
            continue;
        }
        if matches!(
            lower.as_str(),
            "sudo" | "command" | "builtin" | "time" | "exec" | "nohup" | "env" | "nice" | "timeout"
        ) {
            let prefix = lower;
            index += 1;
            while let Some(raw_option) = segment.get(index) {
                let option = workflow_unescape_static_token(raw_option);
                if workflow_command_prefix_is_query_only(&prefix, &option) {
                    return None;
                }
                if option == "--" {
                    index += 1;
                    break;
                }
                if prefix == "env" && is_env_assignment(&option) {
                    index += 1;
                    continue;
                }
                if !option.starts_with('-') || option == "-" {
                    break;
                }
                let takes_value = command_prefix_option_takes_value(&prefix, &option);
                index += 1;
                if takes_value && !option.contains('=') {
                    index += 1;
                }
            }
            if prefix == "timeout" && segment.get(index).is_some() {
                index += 1;
            }
            continue;
        }
        return Some(index);
    }
    None
}

fn workflow_executed_command_index(segment: &[String]) -> Option<usize> {
    let mut offset = 0;
    loop {
        let index = offset + workflow_command_name_index(&segment[offset..])?;
        if !workflow_control_flow_marker(&segment[index]) {
            return Some(index);
        }
        offset = index + 1;
    }
}

fn workflow_git_follows_control_marker(
    segment: &[String],
    command_index: usize,
    git_index: usize,
) -> bool {
    let command_start = command_index + 1;
    workflow_command_name_index(&segment[command_start..])
        .is_some_and(|relative_index| command_start + relative_index == git_index)
}

fn workflow_control_flow_marker(token: &str) -> bool {
    matches!(
        token
            .trim_matches(|ch| matches!(ch, '{' | '}' | '(' | ')'))
            .to_ascii_lowercase()
            .as_str(),
        "if" | "then"
            | "else"
            | "elseif"
            | "elif"
            | "for"
            | "foreach"
            | "while"
            | "until"
            | "do"
            | "case"
            | "switch"
            | "try"
            | "catch"
            | "finally"
            | "begin"
            | "process"
            | "end"
            | ""
    )
}

fn git_inline_alias_is_configured(
    segment: &[String],
    git_index: usize,
    subcommand_index: usize,
) -> bool {
    let options = &segment[git_index + 1..subcommand_index];
    let mut index = 0;
    while index < options.len() {
        let lower = workflow_unescape_static_token(&options[index]).to_ascii_lowercase();
        if lower == "-c" {
            if options.get(index + 1).is_some_and(|value| {
                workflow_unescape_static_token(value)
                    .to_ascii_lowercase()
                    .starts_with("alias.")
            }) {
                return true;
            }
            index += 2;
            continue;
        }
        if lower.starts_with("-calias.")
            || lower.starts_with("--config-env=alias.")
            || (lower == "--config-env"
                && options.get(index + 1).is_some_and(|value| {
                    workflow_unescape_static_token(value)
                        .to_ascii_lowercase()
                        .starts_with("alias.")
                }))
        {
            return true;
        }
        index += 1;
    }
    false
}

fn git_environment_alias_is_configured(segment: &[String], git_index: usize) -> bool {
    let assignments = &segment[..git_index];
    let Some(count) = workflow_static_environment_value(assignments, "GIT_CONFIG_COUNT")
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return false;
    };
    assignments.iter().any(|assignment| {
        let assignment = workflow_unescape_static_token(assignment);
        let Some((name, key)) = assignment.split_once('=') else {
            return false;
        };
        let lower_name = name.to_ascii_lowercase();
        let Some(index) = lower_name
            .strip_prefix("git_config_key_")
            .and_then(|index| index.parse::<usize>().ok())
        else {
            return false;
        };
        index < count
            && key.to_ascii_lowercase().starts_with("alias.")
            && workflow_static_environment_value(assignments, &format!("GIT_CONFIG_VALUE_{index}"))
                .is_some()
    })
}

fn workflow_static_environment_value(tokens: &[String], name: &str) -> Option<String> {
    tokens.iter().rev().find_map(|token| {
        let assignment = workflow_unescape_static_token(token);
        let (candidate, value) = assignment.split_once('=')?;
        candidate
            .eq_ignore_ascii_case(name)
            .then(|| value.to_owned())
    })
}

fn git_fetch_args_are_dry_run(args: &[String]) -> bool {
    git_boolean_option_is_enabled(
        args,
        Some('n'),
        "--dry-run",
        workflow_git_value_options("fetch"),
    )
}

fn git_args_are_dry_run(args: &[String]) -> bool {
    git_boolean_option_is_enabled(args, Some('n'), "--dry-run", &[])
}

fn git_push_args_are_dry_run(args: &[String]) -> bool {
    git_boolean_option_is_enabled(
        args,
        Some('n'),
        "--dry-run",
        &["-o", "--push-option", "--receive-pack", "--exec", "--repo"],
    )
}

fn git_rm_args_are_dry_run(args: &[String]) -> bool {
    git_boolean_option_is_enabled(args, Some('n'), "--dry-run", &["--pathspec-from-file"])
}

fn git_clean_args_are_dry_run(args: &[String]) -> bool {
    git_boolean_option_is_enabled(args, Some('n'), "--dry-run", &["-e", "--exclude"])
}

fn git_option_is_value_option(option: &str, value_options: &[&str]) -> bool {
    value_options.iter().any(|candidate| {
        if option.starts_with("--") || candidate.starts_with("--") {
            option.eq_ignore_ascii_case(candidate)
        } else {
            option == *candidate
        }
    })
}

fn git_boolean_option_is_enabled(
    args: &[String],
    target_short: Option<char>,
    target_long: &str,
    value_options: &[&str],
) -> bool {
    let is_value_option = |option: &str| git_option_is_value_option(option, value_options);
    let negated_long = target_long
        .strip_prefix("--")
        .map(|name| format!("--no-{name}"));
    let mut enabled = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let unescaped = workflow_unescape_static_token(arg);
        let lower = unescaped.to_ascii_lowercase();
        if lower == "--" {
            break;
        }
        if lower == target_long {
            enabled = true;
            index += 1;
            continue;
        }
        if negated_long.as_deref() == Some(lower.as_str()) {
            enabled = false;
            index += 1;
            continue;
        }
        if let Some((option, _)) = unescaped.split_once('=') {
            if option.eq_ignore_ascii_case(target_long) {
                enabled = true;
                index += 1;
                continue;
            }
            if negated_long
                .as_deref()
                .is_some_and(|negated| option.eq_ignore_ascii_case(negated))
            {
                enabled = false;
                index += 1;
                continue;
            }
            if is_value_option(option) {
                index += 1;
                continue;
            }
        }
        if unescaped.starts_with("--") {
            let consumes_next = is_value_option(&unescaped);
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        if let Some(cluster) = unescaped
            .strip_prefix('-')
            .filter(|value| !value.is_empty())
        {
            let options: Vec<char> = cluster.chars().collect();
            let mut consumes_next = false;
            for (offset, option) in options.iter().enumerate() {
                if Some(*option) == target_short {
                    enabled = true;
                }
                let short_option = format!("-{option}");
                if is_value_option(&short_option) {
                    consumes_next = offset + 1 == options.len();
                    break;
                }
            }
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        index += 1;
    }
    enabled
}

fn git_args_have_option(
    args: &[String],
    target_short: Option<char>,
    target_long: &str,
    value_options: &[&str],
) -> bool {
    let is_value_option = |option: &str| git_option_is_value_option(option, value_options);
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let unescaped = workflow_unescape_static_token(arg);
        let lower = unescaped.to_ascii_lowercase();
        if lower == "--" {
            break;
        }
        if lower == target_long {
            return true;
        }
        if let Some((option, _)) = unescaped.split_once('=') {
            if option.eq_ignore_ascii_case(target_long) {
                return true;
            }
            if is_value_option(option) {
                index += 1;
                continue;
            }
        }
        if unescaped.starts_with("--") {
            let consumes_next = is_value_option(&unescaped);
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        if let Some(cluster) = unescaped
            .strip_prefix('-')
            .filter(|value| !value.is_empty())
        {
            let options: Vec<char> = cluster.chars().collect();
            let mut consumes_next = false;
            for (offset, option) in options.iter().enumerate() {
                if Some(*option) == target_short {
                    return true;
                }
                let short_option = format!("-{option}");
                if is_value_option(&short_option) {
                    consumes_next = offset + 1 == options.len();
                    break;
                }
            }
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        index += 1;
    }
    false
}

fn git_first_positional_arg(args: &[String], value_options: &[&str]) -> Option<(usize, String)> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let unescaped = workflow_unescape_static_token(arg);
        if unescaped == "--" {
            return None;
        }
        if unescaped.starts_with("--") {
            let (option, has_attached_value) = unescaped
                .split_once('=')
                .map_or((unescaped.as_str(), false), |(option, _)| (option, true));
            let consumes_next =
                git_option_is_value_option(option, value_options) && !has_attached_value;
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        if let Some(cluster) = unescaped
            .strip_prefix('-')
            .filter(|cluster| !cluster.is_empty())
        {
            let options: Vec<char> = cluster.chars().collect();
            let consumes_next = options.iter().enumerate().any(|(offset, option)| {
                git_option_is_value_option(&format!("-{option}"), value_options)
                    && offset + 1 == options.len()
            });
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        return Some((index, unescaped));
    }
    None
}

fn git_args_have_matching_positional(
    args: &[String],
    value_options: &[&str],
    predicate: impl Fn(&str) -> bool,
) -> bool {
    let mut index = 0;
    let mut options = true;
    while let Some(arg) = args.get(index) {
        let unescaped = workflow_unescape_static_token(arg);
        if options && unescaped == "--" {
            options = false;
            index += 1;
            continue;
        }
        if options && unescaped.starts_with("--") {
            let (option, has_attached_value) = unescaped
                .split_once('=')
                .map_or((unescaped.as_str(), false), |(option, _)| (option, true));
            let consumes_next =
                git_option_is_value_option(option, value_options) && !has_attached_value;
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        if options {
            if let Some(cluster) = unescaped
                .strip_prefix('-')
                .filter(|cluster| !cluster.is_empty())
            {
                let options: Vec<char> = cluster.chars().collect();
                let consumes_next = options.iter().enumerate().any(|(offset, option)| {
                    git_option_is_value_option(&format!("-{option}"), value_options)
                        && offset + 1 == options.len()
                });
                index += if consumes_next { 2 } else { 1 };
                continue;
            }
        }
        if predicate(&unescaped) {
            return true;
        }
        index += 1;
    }
    false
}

fn git_args_positional_count(args: &[String], value_options: &[&str]) -> usize {
    let mut index = 0;
    let mut options = true;
    let mut count = 0;
    while let Some(arg) = args.get(index) {
        let unescaped = workflow_unescape_static_token(arg);
        if options && unescaped == "--" {
            options = false;
            index += 1;
            continue;
        }
        if options && unescaped.starts_with("--") {
            let (option, has_attached_value) = unescaped
                .split_once('=')
                .map_or((unescaped.as_str(), false), |(option, _)| (option, true));
            let consumes_next =
                git_option_is_value_option(option, value_options) && !has_attached_value;
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        if options {
            if let Some(cluster) = unescaped
                .strip_prefix('-')
                .filter(|cluster| !cluster.is_empty())
            {
                let options: Vec<char> = cluster.chars().collect();
                let consumes_next = options.iter().enumerate().any(|(offset, option)| {
                    git_option_is_value_option(&format!("-{option}"), value_options)
                        && offset + 1 == options.len()
                });
                index += if consumes_next { 2 } else { 1 };
                continue;
            }
        }
        count += 1;
        index += 1;
    }
    count
}

fn git_args_have_matching_option_value(
    args: &[String],
    target_long: &str,
    value_options: &[&str],
    predicate: impl Fn(&str) -> bool,
) -> bool {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let unescaped = workflow_unescape_static_token(arg);
        if unescaped == "--" {
            break;
        }
        if unescaped.starts_with("--") {
            let (option, attached_value) = unescaped
                .split_once('=')
                .map_or((unescaped.as_str(), None), |(option, value)| {
                    (option, Some(value))
                });
            if option.eq_ignore_ascii_case(target_long) {
                let matches = attached_value.map_or_else(
                    || {
                        args.get(index + 1)
                            .is_some_and(|value| predicate(&workflow_unescape_static_token(value)))
                    },
                    &predicate,
                );
                if matches {
                    return true;
                }
            }
            let consumes_next =
                git_option_is_value_option(option, value_options) && attached_value.is_none();
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        if let Some(cluster) = unescaped
            .strip_prefix('-')
            .filter(|cluster| !cluster.is_empty())
        {
            let options: Vec<char> = cluster.chars().collect();
            let consumes_next = options.iter().enumerate().any(|(offset, option)| {
                git_option_is_value_option(&format!("-{option}"), value_options)
                    && offset + 1 == options.len()
            });
            index += if consumes_next { 2 } else { 1 };
            continue;
        }
        index += 1;
    }
    false
}

fn git_stash_args_are_read_only(args: &[String]) -> bool {
    args.first().is_some_and(|subcommand| {
        matches!(
            workflow_unescape_static_token(subcommand)
                .to_ascii_lowercase()
                .as_str(),
            "list" | "show"
        )
    })
}

fn git_push_is_forced(args: &[String]) -> bool {
    let value_options = workflow_git_value_options("push");
    git_boolean_option_is_enabled(args, Some('f'), "--force", value_options)
        || git_boolean_option_is_enabled(args, None, "--force-with-lease", value_options)
        || git_args_have_matching_positional(args, value_options, |arg| arg.starts_with('+'))
}

fn git_push_deletes_remote_ref(args: &[String]) -> bool {
    let value_options = workflow_git_value_options("push");
    git_boolean_option_is_enabled(args, Some('d'), "--delete", value_options)
        || git_boolean_option_is_enabled(args, None, "--mirror", value_options)
        || git_boolean_option_is_enabled(args, None, "--prune", value_options)
        || git_push_has_delete_refspec(args, value_options)
}

fn git_push_has_delete_refspec(args: &[String], value_options: &[&str]) -> bool {
    git_args_have_matching_positional(args, value_options, |arg| {
        arg.trim_start_matches('+').starts_with(':')
    })
}

fn git_branch_is_delete(args: &[String]) -> bool {
    git_args_have_option(args, Some('d'), "--delete", &["--format", "--sort"])
        || args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .any(|arg| {
                let arg = workflow_unescape_static_token(arg);
                arg.eq_ignore_ascii_case("--delete-force")
                    || arg
                        .strip_prefix('-')
                        .filter(|cluster| !cluster.starts_with('-'))
                        .is_some_and(|cluster| cluster.contains('D'))
            })
}

fn git_branch_is_forced(args: &[String]) -> bool {
    git_args_have_option(
        args,
        Some('f'),
        "--force",
        &["--format", "--sort", "--contains", "--no-contains"],
    ) || args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| {
            workflow_unescape_static_token(arg)
                .strip_prefix('-')
                .filter(|cluster| !cluster.starts_with('-'))
                .is_some_and(|cluster| cluster.chars().any(|option| matches!(option, 'M' | 'C')))
        })
}

fn git_branch_is_move(args: &[String]) -> bool {
    git_args_have_option(
        args,
        Some('m'),
        "--move",
        workflow_git_value_options("branch"),
    )
}

fn git_tag_is_delete(args: &[String]) -> bool {
    git_args_have_option(
        args,
        Some('d'),
        "--delete",
        &["-m", "--message", "-F", "--file"],
    )
}

fn git_tag_is_forced(args: &[String]) -> bool {
    git_args_have_option(
        args,
        Some('f'),
        "--force",
        &["-m", "--message", "-F", "--file"],
    )
}

fn git_fetch_prunes_refs(args: &[String]) -> bool {
    let value_options = workflow_git_value_options("fetch");
    git_boolean_option_is_enabled(args, Some('p'), "--prune", value_options)
        || git_boolean_option_is_enabled(args, Some('P'), "--prune-tags", value_options)
}

fn git_fetch_is_forced(args: &[String]) -> bool {
    let value_options = workflow_git_value_options("fetch");
    git_boolean_option_is_enabled(args, Some('f'), "--force", value_options)
        || git_args_have_matching_positional(args, value_options, |arg| arg.starts_with('+'))
        || git_args_have_matching_option_value(args, "--refmap", value_options, |value| {
            value.starts_with('+')
        })
}

fn git_remote_mutates_refs(args: &[String]) -> bool {
    let subcommand = args.iter().find_map(|token| {
        let token = workflow_unescape_static_token(token);
        (token != "--" && !token.starts_with('-')).then(|| token.to_ascii_lowercase())
    });
    match subcommand.as_deref() {
        Some("remove" | "rm" | "rename") => true,
        Some("prune") => !git_args_are_dry_run(args),
        Some("update") => git_args_have_option(args, Some('p'), "--prune", &[]),
        Some("set-head") => true,
        _ => false,
    }
}

fn git_notes_mutates_refs(args: &[String]) -> bool {
    let value_options = workflow_git_value_options("notes");
    let Some((subcommand_index, subcommand)) = git_first_positional_arg(args, value_options) else {
        return false;
    };
    let operation_args = &args[subcommand_index + 1..];
    match subcommand.to_ascii_lowercase().as_str() {
        "remove" => true,
        "prune" => {
            !git_boolean_option_is_enabled(operation_args, Some('n'), "--dry-run", value_options)
        }
        "add" | "copy" => git_args_have_option(operation_args, Some('f'), "--force", value_options),
        _ => false,
    }
}

fn git_replace_args_are_mutating(args: &[String]) -> bool {
    let value_options = workflow_git_value_options("replace");
    if git_args_have_option(args, Some('d'), "--delete", value_options)
        || git_args_have_option(args, Some('f'), "--force", value_options)
        || git_args_have_option(args, None, "--graft", value_options)
        || git_args_have_option(args, None, "--convert-graft-file", value_options)
    {
        return true;
    }
    if args.iter().any(|arg| {
        matches!(
            workflow_unescape_static_token(arg)
                .to_ascii_lowercase()
                .as_str(),
            "-l" | "--list"
        )
    }) {
        return false;
    }
    args.iter()
        .any(|arg| !workflow_unescape_static_token(arg).starts_with('-'))
}

fn git_symbolic_ref_is_mutating(args: &[String]) -> bool {
    git_args_have_option(args, Some('d'), "--delete", &["-m"])
        || git_args_positional_count(args, &["-m"]) >= 2
}

fn git_worktree_args_are_mutating(args: &[String]) -> bool {
    let subcommand = args.iter().find_map(|token| {
        let token = workflow_unescape_static_token(token);
        (token != "--" && !token.starts_with('-')).then(|| token.to_ascii_lowercase())
    });
    match subcommand.as_deref() {
        Some("list") | None => false,
        Some("prune")
            if git_boolean_option_is_enabled(
                args,
                Some('n'),
                "--dry-run",
                workflow_git_value_options("worktree"),
            ) =>
        {
            false
        }
        _ => true,
    }
}

fn git_apply_is_mutating(args: &[String]) -> bool {
    let mut applies = false;
    let mut read_only = false;
    for arg in args {
        match arg.to_ascii_lowercase().as_str() {
            "--apply" => applies = true,
            "--check" | "--stat" | "--numstat" | "--summary" => read_only = true,
            _ => {}
        }
    }
    applies || !read_only
}

fn find_segment_has_file_deletion(segment: &[String], command_index: usize) -> bool {
    let args = &segment[command_index + 1..];
    args.iter()
        .any(|token| token.eq_ignore_ascii_case("-delete"))
        || args.windows(2).any(|window| {
            matches!(
                window[0].to_ascii_lowercase().as_str(),
                "-exec" | "-execdir"
            ) && is_file_deletion_command_name(&window[1])
        })
}

fn xargs_segment_has_file_deletion(segment: &[String], command_index: usize) -> bool {
    segment
        .iter()
        .skip(command_index + 1)
        .any(|token| is_file_deletion_command_name(token))
}

fn is_file_deletion_command_name(token: &str) -> bool {
    matches!(
        normalized_command_name(token).as_str(),
        "rm" | "unlink"
            | "rmdir"
            | "del"
            | "erase"
            | "deltree"
            | "rd"
            | "ri"
            | "remove-item"
            | "remove-itemproperty"
            | "remove-content"
            | "clear-content"
            | "clear-recyclebin"
            | "sdelete"
            | "sdelete64"
    )
}

fn git_segment_has_sensitive_checkout(segment: &[String]) -> bool {
    let Some(git_index) = git_command_index(segment) else {
        return false;
    };
    let Some(checkout_index) = git_subcommand_index(segment, git_index) else {
        return false;
    };
    if !segment[checkout_index].eq_ignore_ascii_case("checkout") {
        return false;
    }
    !git_checkout_args_are_help_only(&segment[checkout_index + 1..])
}

fn git_checkout_args_are_help_only(args: &[String]) -> bool {
    git_args_are_help_only(args)
}

fn workflow_git_value_options(subcommand: &str) -> &'static [&'static str] {
    match subcommand {
        "clean" => &["-e", "--exclude"],
        "push" => &["-o", "--push-option", "--receive-pack", "--exec", "--repo"],
        "fetch" => &[
            "-o",
            "--server-option",
            "--upload-pack",
            "--depth",
            "--deepen",
            "--shallow-since",
            "--shallow-exclude",
            "--negotiation-tip",
            "--filter",
            "--refmap",
            "--jobs",
        ],
        "tag" => &[
            "-m",
            "--message",
            "-F",
            "--file",
            "-u",
            "--local-user",
            "--cleanup",
            "--format",
            "--sort",
            "--contains",
            "--no-contains",
            "--points-at",
            "--merged",
            "--no-merged",
        ],
        "branch" => &[
            "--format",
            "--sort",
            "--contains",
            "--no-contains",
            "--points-at",
            "--merged",
            "--no-merged",
        ],
        "stash" => &["-m", "--message", "--pathspec-from-file"],
        "notes" => &[
            "--ref",
            "-m",
            "--message",
            "-F",
            "--file",
            "-C",
            "--reuse-message",
            "-c",
            "--reedit-message",
        ],
        "symbolic-ref" | "update-ref" => &["-m"],
        "worktree" => &["-b", "-B", "--orphan", "--reason", "--expire"],
        "replace" => &["--format"],
        "checkout-index" => &["--prefix"],
        "checkout" => &["-b", "-B", "--orphan", "--conflict", "--pathspec-from-file"],
        "reset" | "restore" | "rm" => &["--pathspec-from-file"],
        _ => &[],
    }
}

fn workflow_git_args_are_help_only(subcommand: &str, args: &[String]) -> bool {
    git_args_have_option(
        args,
        Some('h'),
        "--help",
        workflow_git_value_options(subcommand),
    )
}

fn git_args_are_help_only(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.to_ascii_lowercase().as_str(), "--help" | "-h"))
}

fn git_segment_has_sensitive_stash(segment: &[String]) -> bool {
    if segment_wrapper_payload_matches(segment, shell_command_has_sensitive_git_stash) {
        return true;
    }
    let Some(git_index) = git_command_index(segment) else {
        return false;
    };
    let Some(stash_index) = git_subcommand_index(segment, git_index) else {
        return false;
    };
    if !segment[stash_index].eq_ignore_ascii_case("stash") {
        return false;
    }

    let subcommand = segment
        .iter()
        .skip(stash_index + 1)
        .find(|token| token.as_str() != "--" && !token.starts_with('-'))
        .map(|token| token.to_ascii_lowercase());
    !matches!(subcommand.as_deref(), Some("list" | "show"))
}

fn git_command_index(segment: &[String]) -> Option<usize> {
    let mut index = 0;
    while let Some(token) = segment.get(index) {
        let lower = token.to_ascii_lowercase();
        if is_env_assignment(token)
            || matches!(lower.as_str(), "sudo" | "command" | "builtin" | "time")
        {
            index += 1;
            continue;
        }
        if lower == "env" {
            index += 1;
            while segment
                .get(index)
                .is_some_and(|token| token.starts_with('-') || is_env_assignment(token))
            {
                index += 1;
            }
            continue;
        }
        return lower.eq("git").then_some(index);
    }
    None
}

fn git_subcommand_index(segment: &[String], git_index: usize) -> Option<usize> {
    let mut index = git_index + 1;
    while let Some(raw_token) = segment.get(index) {
        let token = workflow_unescape_static_token(raw_token);
        if token == "--" {
            index += 1;
            break;
        }
        if matches!(
            token.to_ascii_lowercase().as_str(),
            "-h" | "--help" | "--version"
        ) {
            return None;
        }
        if !token.starts_with('-') {
            break;
        }
        let takes_value = git_global_option_takes_value(&token);
        index += 1;
        if takes_value && !token.contains('=') {
            index += 1;
        }
    }
    (index < segment.len()).then_some(index)
}

fn git_global_option_takes_value(token: &str) -> bool {
    let unescaped = workflow_unescape_static_token(token);
    let option = unescaped
        .split_once('=')
        .map(|(option, _)| option)
        .unwrap_or(&unescaped);
    matches!(option, "-c" | "-C")
        || matches!(
            option.to_ascii_lowercase().as_str(),
            "--config-env" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path"
        )
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file tools refuse these paths outright, but a shell reads a file
    /// without ever showing a path parameter — so the shell's guarantee is a
    /// stop for the user rather than a refusal.
    #[test]
    fn a_command_naming_a_session_credential_is_hard_sensitive() {
        for command in [
            "cat ~/.rebon/jobs/bg-1a04/state.json",
            "type C:\\Users\\me\\.rebon\\jobs\\bg-1a04\\state.json",
            "grep -o 'ipcToken' ~/.rebon/projects/f--repo/sess-1.owner.json",
            "python -c \"print(open('/home/me/.rebon/jobs/bg-1/state.json').read())\"",
            "jq .ipcPort jobs/bg-1/state.json",
        ] {
            assert_eq!(
                classify_auto_mode_tool_input("Bash", &serde_json::json!({ "command": command })),
                AutoModeInputDisposition::HardSensitive(
                    HardSensitiveReason::SessionCredentialAccess
                ),
                "expected a stop for {command:?}"
            );
        }
        assert_eq!(
            classify_auto_mode_tool_input(
                "PowerShell",
                &serde_json::json!({ "command": "Get-Content $env:USERPROFILE\\.rebon\\projects\\p\\sess-1.owner.json" })
            ),
            AutoModeInputDisposition::HardSensitive(HardSensitiveReason::SessionCredentialAccess),
            "the same holds for the PowerShell tool"
        );
    }

    /// `state.json` is an ordinary name. Flagging every command that mentions
    /// one would train the user to click through the stop that matters.
    #[test]
    fn an_ordinary_state_file_is_not_a_session_credential() {
        for command in [
            "cat state.json",
            "cat src/state.json",
            "node -e \"require('./state.json')\"",
            "cat ~/.rebon/projects/f--repo/sess-1.jsonl",
            "ls ~/.rebon/jobs",
        ] {
            assert_eq!(
                classify_auto_mode_tool_input("Bash", &serde_json::json!({ "command": command })),
                AutoModeInputDisposition::NoSpecialRisk,
                "expected no stop for {command:?}"
            );
        }
    }

    /// The classifier only reads shell tools; a path-taking tool is covered by
    /// the file-tool path guard, which refuses rather than asks.
    #[test]
    fn session_credential_detection_only_applies_to_shell_tools() {
        assert_eq!(
            classify_auto_mode_tool_input(
                "Read",
                &serde_json::json!({ "file_path": "~/.rebon/jobs/bg-1/state.json" })
            ),
            AutoModeInputDisposition::NoSpecialRisk
        );
    }

    #[test]
    fn sensitive_git_stash_detection_flags_mutating_stash_commands() {
        for command in [
            "git stash",
            "git stash pop",
            "git stash apply stash@{0}",
            "git -C repo stash pop",
            "FOO=bar git stash push -m savepoint",
            "cd repo && git stash pop",
            "git status; git stash apply",
            "sudo git stash clear",
            "env GIT_DIR=.git git stash drop",
        ] {
            assert!(
                shell_command_has_sensitive_git_stash(command),
                "expected sensitive git stash detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_git_stash_detection_allows_read_only_stash_commands() {
        for command in [
            "git status",
            "git stash list",
            "git stash show",
            "git stash show -p stash@{0}",
            "echo 'git stash pop'",
        ] {
            assert!(
                !shell_command_has_sensitive_git_stash(command),
                "expected no sensitive git stash detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_git_checkout_detection_flags_checkout_commands() {
        for command in [
            "git checkout -- src/lib.rs",
            "git checkout -- .",
            "git checkout main",
            "git checkout -b topic",
            "git -C repo checkout -- README.md",
            "FOO=bar git checkout -- Cargo.toml",
            "cd repo && git checkout -- src/lib.rs",
            "git status; git checkout -- src/lib.rs",
            "sudo git checkout -- src/lib.rs",
            "env GIT_DIR=.git git checkout -- src/lib.rs",
        ] {
            assert!(
                shell_command_has_sensitive_git_checkout(command),
                "expected sensitive git checkout detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_git_checkout_detection_allows_non_checkout_commands() {
        for command in [
            "git status",
            "git branch --show-current",
            "git checkout --help",
            "git checkout -h",
            "echo 'git checkout -- src/lib.rs'",
        ] {
            assert!(
                !shell_command_has_sensitive_git_checkout(command),
                "expected no sensitive git checkout detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_git_worktree_detection_flags_mutating_commands() {
        for command in [
            "git restore src/lib.rs",
            "git restore --staged src/lib.rs",
            "git reset --hard HEAD~1",
            "git reset --merge origin/main",
            "git reset --keep origin/main",
            "git switch main",
            "git merge feature",
            "git rebase main",
            "git cherry-pick abc123",
            "git revert abc123",
            "git pull --rebase",
            "git apply fix.patch",
            "git apply --apply fix.patch",
            "git status && git restore src/lib.rs",
            "sudo git -C repo reset --hard",
        ] {
            assert!(
                shell_command_has_sensitive_git_worktree(command),
                "expected sensitive git worktree detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_git_worktree_detection_allows_read_only_commands() {
        for command in [
            "git status",
            "git diff",
            "git log --oneline",
            "git restore --help",
            "git reset -h",
            "git apply --check fix.patch",
            "git apply --stat fix.patch",
            "echo 'git reset --hard'",
        ] {
            assert!(
                !shell_command_has_sensitive_git_worktree(command),
                "expected no sensitive git worktree detection for {command:?}"
            );
        }
    }

    #[test]
    fn workflow_sensitive_git_detection_flags_shared_worktree_risks() {
        for command in [
            "git reset",
            "git reset --soft HEAD~1",
            "git checkout -- src/lib.rs",
            "git checkout -Bhelp",
            "git checkout main",
            "git checkout --detach main",
            "git switch -C main",
            "git switch -f main",
            "git switch --discard-changes main",
            "git switch --merge main",
            "git pull --rebase",
            "git pull -r origin main",
            "git pull --autostash origin main",
            "git pull origin +main",
            "git restore --staged src/lib.rs",
            "git clean",
            "git clean -d",
            "git clean -fen",
            "git clean -f -e -n",
            "git stash push -m temporary",
            "git stash push -m -h",
            "git stash -m list",
            "git stash -- list",
            "git stash -q list",
            "git stash --message=x list",
            "git revert abc123",
            "git push --force origin main",
            "git push -fu origin main",
            "git push --force -o -n origin main",
            "git push --force-with-lease=main origin main",
            "git push --dry-run --no-dry-run --force origin main",
            "git push -n --no-dry-run -f origin main",
            "git push origin +main",
            "git push --delete origin topic",
            r"git push -\d origin topic",
            r"git push origin \:topic",
            "git push --prune origin refs/heads/*:refs/heads/*",
            "git push origin :topic",
            "git branch -D topic",
            "git branch -dr origin/topic",
            "git branch -Dr origin/topic",
            "git branch --delete topic",
            "git branch -f topic HEAD~1",
            "git branch -M topic",
            "git branch -C topic copy",
            "git branch -m old new",
            "git branch --move old new",
            "git tag -d v1",
            "git tag -f v1 HEAD",
            "git fetch --prune origin",
            "git fetch -P origin",
            "git fetch --prune-tags origin",
            "git fetch --force origin main:topic",
            "git fetch --dry-run --no-dry-run --force origin main:topic",
            "git fetch -o --dry-run --force origin main:topic",
            "git fetch origin +main:topic",
            "git fetch --refmap=+refs/heads/main:refs/remotes/origin/main origin refs/heads/main",
            "git remote prune origin",
            "git remote remove origin",
            "git remote rename origin upstream",
            "git remote set-head origin -d",
            "git remote set-head origin -a",
            "git remote set-head origin main",
            "git remote update --prune origin",
            "git replace -f HEAD HEAD~1",
            "git replace -d HEAD",
            "git replace --convert-graft-file",
            "git notes remove HEAD",
            "git notes add -f -m x HEAD",
            "git notes copy -f HEAD HEAD~1",
            "git notes prune",
            "git notes prune --dry-run --no-dry-run",
            "git checkout-index -f -a",
            "git update-ref -d refs/heads/topic",
            "git symbolic-ref -d refs/heads/topic",
            "git symbolic-ref -dq refs/heads/topic",
            "git symbolic-ref -m -h -d refs/remotes/origin/HEAD",
            "git symbolic-ref HEAD refs/heads/topic",
            "git restore -- --help",
            "git reset -- --help",
            "git reset --pathspec-from-file --help",
            "git checkout --pathspec-from-file --help",
            "git clean -f -- -n",
            "git rm -- -n",
            "git worktree remove ../worker",
            "git worktree prune --dry-run --no-dry-run",
            "git.exe reset --hard HEAD~1",
            "git -C ${dir} reset --hard HEAD~1",
            r"git -\C repo reset --hard HEAD~1",
            r#""C:\Program Files\Git\cmd\git.exe" restore src/lib.rs"#,
            "if true; then git reset --hard HEAD~1; fi",
            "if ($true) { git restore src/lib.rs }",
            "if($true){git reset --hard HEAD~1}",
            "f(){ git reset --hard HEAD~1; }; f",
            "! git reset --hard HEAD~1",
            "exec git restore src/lib.rs",
            "exec -- git reset --hard HEAD~1",
            "command -- git restore src/lib.rs",
            "sudo -- git reset --hard HEAD~1",
            "sudo -H git reset --hard HEAD~1",
            "sudo -r sysadm_r git reset --hard HEAD~1",
            "sudo --type sysadm_t git restore src/lib.rs",
            "time -p git reset --hard HEAD~1",
            "nohup git restore src/lib.rs",
            "nice git reset --hard HEAD~1",
            "nice -n 5 git restore src/lib.rs",
            "env -S 'git reset --hard HEAD~1'",
            "env -S '-- git reset --hard HEAD~1'",
            "env -iS '-- git restore src/lib.rs'",
            "env -S '-i -- git reset --hard HEAD~1'",
            "env -S '-S \"git restore src/lib.rs\"'",
            "env - /usr/bin/git reset --hard HEAD~1",
            "env - git restore src/lib.rs",
            "env -S '#' git reset --hard HEAD~1",
            "env -S '# ignored' git restore src/lib.rs",
            "/usr/bin/env -S 'git reset --hard HEAD~1'",
            r#""C:\Program Files\Git\usr\bin\env.exe" -S 'git restore src/lib.rs'"#,
            "env --split-string='git restore src/lib.rs'",
            "env -iS 'git reset --hard HEAD~1'",
            "env -iS'git restore src/lib.rs'",
            "env --split 'git reset --hard HEAD~1'",
            "env --split='git restore src/lib.rs'",
            "env -S git reset --hard HEAD~1",
            "env --split-string=git restore src/lib.rs",
            r"env -S 'git\_reset --hard HEAD~1'",
            "if env -S git reset --hard HEAD~1; then :; fi",
            "timeout 5 git reset --hard HEAD~1",
            "timeout -s KILL 5 git restore src/lib.rs",
            r"timeout -\s KILL 5 git reset --hard HEAD~1",
            r"timeout \-s KILL 5 git restore src/lib.rs",
            "timeout -vs KILL 5 git restore src/lib.rs",
            "if true; then exec git reset --hard HEAD~1; fi",
            "if true; then time -p git reset --hard HEAD~1; fi",
            "if eval 'git reset --hard HEAD~1'; then :; fi",
            "if bash -c 'git restore src/lib.rs'; then :; fi",
            "echo HEAD~0 | xargs -I{} git reset --hard {}",
            "printf 'reset\\n' | xargs -I{} git {} --hard HEAD~1",
            "printf '' | xargs --max-lines git reset --hard HEAD~1",
            "printf '' | xargs --max-p 1 git reset --hard HEAD~1",
            "printf 'reset --hard HEAD~1\\n' | xargs git",
            "printf '%s\\n' --force origin main | xargs git push",
            "printf '%s\\0' src/lib.rs | xargs -0 -n 1 git restore --",
            "xargs --replace={} env git reset --hard {}",
            "find . -exec git reset --hard HEAD~1 {} \\;",
            "find reset -maxdepth 0 -exec git {} --hard HEAD~1 \\;",
            "find . -execdir env git restore -- {} +",
            "find . -exec sh -c 'git reset --hard \"$1\"' sh {} \\;",
            "iex \"echo 0,'git reset --hard HEAD~1' | bash\"",
            "pwsh -c \"echo 0,'git reset --hard HEAD~1' | bash\"",
            concat!("git \\", "\n", "reset --hard HEAD~1"),
            concat!("git `", "\n", "reset --hard HEAD~1"),
            "g\\it restore src/lib.rs",
            "git re\\set --hard HEAD~1",
            "git re`set --hard HEAD~1",
            "git $'reset' --hard HEAD~1",
            r"git $'re\x73et' --hard HEAD~1",
            "bash -c $'git restore src/lib.rs'",
            "b\\ash -c 'git reset --hard HEAD~1'",
            "echo $(git reset --hard HEAD~1)",
            "echo \"$(git restore src/lib.rs)\"",
            "echo `git stash pop`",
            "eval 'git reset --hard HEAD~1'",
            "Invoke-Expression 'git restore src/lib.rs'",
            "Invoke-Expression -Command 'git reset --hard HEAD~1'",
            "Invoke-Expression -Verbose -Command 'git reset --hard HEAD~1'",
            "Invoke-Expression -ErrorAction Stop -Command 'git restore src/lib.rs'",
            "Invoke-Expression -ErrorA Stop 'git reset --hard HEAD~1'",
            "Invoke-Expression -ErrorA:Stop 'git restore src/lib.rs'",
            "git -c alias.wipe='reset --hard' wipe HEAD~1",
            r"git -\c alias.wipe='reset --hard' wipe HEAD~1",
            "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.wipe GIT_CONFIG_VALUE_0='!git reset --hard HEAD~1' git wipe",
            "env GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.wipe GIT_CONFIG_VALUE_0='reset --hard' git wipe HEAD~1",
            "bash <<'EOF'\ngit reset --hard HEAD~1\nEOF",
            "if bash <<'EOF'\ngit reset --hard HEAD~1\nEOF\nthen :; fi",
            "source /dev/stdin <<'EOF'\ngit reset --hard HEAD~1\nEOF",
            ". /dev/stdin <<'EOF'\ngit restore src/lib.rs\nEOF",
            "cat <<'EOF' | bash\ngit reset --hard HEAD~1\nEOF",
            "cat <<'EOF' | (bash)\ngit reset --hard HEAD~1\nEOF",
            "cat <<'EOF' | exec bash\ngit reset --hard HEAD~1\nEOF",
            "cat <<EOF | sh\ngit restore src/lib.rs\nEOF",
            "cat <<E'OF'\nplain\nEOF\ngit reset --hard HEAD~1",
            "cat <<E\\OF\nplain\nEOF\ngit restore src/lib.rs",
            "cat <<$'EOF'\nplain\nEOF\ngit reset --hard HEAD~1",
            "echo $((1 << 2))\ngit reset --hard HEAD~1",
            "# <<EOF\ngit reset --hard HEAD~1",
            "# @'\ngit restore src/lib.rs",
            "cat <<EOF && git reset --hard HEAD~1\nbody\nEOF",
            "cat <<'EOF'; git restore src/lib.rs\nbody\nEOF",
            "cat <<EOF\n$(git reset --hard HEAD~1)\nEOF",
            "$text = @\"\n$(git restore src/lib.rs)\n\"@\nWrite-Output $text",
            "@'\ngit reset --hard HEAD~1\n'@ | Invoke-Expression",
            "Invoke-Expression @'\ngit restore src/lib.rs\n'@",
            "printf 'git reset --hard HEAD~1\\n' | bash",
            r"printf 'git\x20reset --hard HEAD~1\n' | bash",
            "printf '%s ' git reset --hard HEAD~1 | bash",
            "printf -- 'git reset --hard HEAD~1' | bash",
            "printf '%s' 'git reset --hard HEAD~1' | cat | bash",
            "printf '%s' 'git reset --hard HEAD~1' | cat - | bash",
            "printf '%s' 'git reset --hard HEAD~1' | cat -- | bash",
            "printf '%s' 'git reset --hard HEAD~1' | cat -u | bash",
            "printf '%s' 'git reset --hard HEAD~1' | cat |& bash",
            "echo -n 'git reset --hard HEAD~1' | bash",
            "Write-Output -NoEnumerate 'git reset --hard HEAD~1' | Invoke-Expression",
            "Write-Output -NoE 'git restore src/lib.rs' | Invoke-Expression",
            "Write-Output -InputObject 'git restore src/lib.rs' | Invoke-Expression",
            "Write-Output -Input 'git reset --hard HEAD~1' | Invoke-Expression",
            "Write-Output -Inp 'git restore src/lib.rs' | Invoke-Expression",
            "Write-Output -Input:'git reset --hard HEAD~1' | Invoke-Expression",
            "Write-Output -Verbose 'git reset --hard HEAD~1' | Invoke-Expression",
            "Write-Output -ErrorAction Stop 'git restore src/lib.rs' | Invoke-Expression",
            "Write-Output -ErrorA Stop 'git reset --hard HEAD~1' | Invoke-Expression",
            "echo -NoE 'git reset --hard HEAD~1' | iex",
            "echo -EA Stop 'git restore src/lib.rs' | iex",
            "write 'git reset --hard HEAD~1' | iex",
            "Write-Output '1' 'git reset --hard HEAD~1' | Invoke-Expression",
            "Write-Output 0,'git reset --hard HEAD~1' | iex",
            "echo 0,'git restore src/lib.rs' | iex",
            "write 0,'git reset --hard HEAD~1' | iex",
            "Write-Output 'git reset --hard HEAD~1' | & iex",
            "echo 'git restore src/lib.rs' |& bash",
            "echo '1' 'git restore src/lib.rs' | iex",
            "write '1' 'git reset --hard HEAD~1' | iex",
            "cat <<< 'git restore src/lib.rs' | bash",
            "cat <<<'git reset --hard HEAD~1' | bash",
            "bash <<<'git reset --hard HEAD~1'",
            "if bash <<<'git reset --hard HEAD~1'; then :; fi",
            "$text = @'\nplain text\n'@; git restore src/lib.rs",
            "cd repo && git reset --hard HEAD~1",
            "Set-Location repo; git restore src/lib.rs",
            "cmd.exe /c\"git reset --hard HEAD~1\"",
            r#"powershell -Command "git stash pop""#,
        ] {
            assert!(
                shell_command_has_workflow_sensitive_git_operation(command),
                "expected workflow-sensitive Git detection for {command:?}"
            );
        }
    }

    #[test]
    fn workflow_sensitive_git_detection_allows_read_only_inspection() {
        for command in [
            "git status --short",
            "git diff -- src/lib.rs",
            "git log --oneline -5",
            "git show HEAD:src/lib.rs",
            "git rev-parse HEAD",
            "git ls-files src",
            "git checkout -b feature",
            "git checkout -bhelp",
            "git checkout -b feature origin/main",
            "git switch main",
            "git switch -c feature",
            "git switch -cfeature",
            "git switch --detach HEAD~1",
            "git pull",
            "git pull origin main",
            "git pull --ff-only origin main",
            "git pull --no-rebase origin main",
            "git stash list",
            "git stash show -p stash@{0}",
            "git branch --list",
            "git symbolic-ref HEAD",
            "git stash push -m -h --help",
            "git clean -nd",
            "git worktree list",
            "git worktree prune --dry-run",
            "git worktree prune --no-dry-run --dry-run",
            "git push --force --dry-run origin main",
            "git push --no-dry-run --dry-run --force origin main",
            "git push --force --no-force origin main",
            "git push --force-with-lease=main --no-force-with-lease origin main",
            "git push --delete --dry-run origin topic",
            "git push --prune --dry-run origin refs/heads/*:refs/heads/*",
            "git push -o --delete origin main",
            "git push -o --force-with-lease origin main",
            "git fetch --prune --dry-run origin",
            "git fetch --prune-tags --dry-run origin",
            "git fetch -o +main origin main:topic",
            "git fetch -o --refmap=+main origin main:topic",
            "git fetch --refmap=refs/heads/main:refs/remotes/origin/main origin refs/heads/main",
            "git fetch --force --dry-run origin main:topic",
            "git fetch --force --no-force origin main:topic",
            "git fetch --prune --no-prune origin",
            "git remote prune --dry-run origin",
            "git remote update origin",
            "git replace -l",
            "git replace --format --convert-graft-file -l",
            "git notes list",
            "git notes show HEAD",
            "git notes prune --dry-run",
            "git notes prune --no-dry-run --dry-run",
            "git tag -F message.txt v1 HEAD",
            "git rm -n src/file.rs",
            "git reset --help",
            "git checkout -Bhelp --help",
            "git -h reset --hard HEAD~1",
            "git --help reset --hard HEAD~1",
            "git update-ref --help",
            r"git -\C repo status --short",
            "git $'status' --short",
            "bash -c $'git status --short'",
            "env -S 'git status --short'",
            "env -S '-- git status --short'",
            "env -S '-i -- git status --short'",
            "env -S '-S \"git status --short\"'",
            "env - /usr/bin/git status --short",
            "env --help /usr/bin/git reset --hard HEAD~1",
            "env --version git restore src/lib.rs",
            "/usr/bin/env --help git reset --hard HEAD~1",
            "/usr/bin/env -S 'git status --short'",
            r#""C:\Program Files\Git\usr\bin\env.exe" -S 'git status --short'"#,
            r"env -S '\#' git reset --hard HEAD~1",
            r##"env -S '"#"' git reset --hard HEAD~1"##,
            "env -S 'prefix#suffix' git reset --hard HEAD~1",
            "env -iS 'git status --short'",
            "env --split='git status --short'",
            "env -S git status --short",
            "env --split-string=git status --short",
            r#"env -S '"git\_reset" --version'"#,
            "echo 'git reset --hard'",
            "echo git reset --hard",
            "echo ok # ; git reset --hard HEAD~1",
            "echo ok # $(git reset --hard HEAD~1)",
            "<#\ngit reset --hard HEAD~1\n#>\nWrite-Output ok",
            "command -v git reset --hard HEAD~1",
            "command -V git restore src/lib.rs",
            "xargs --help git reset --hard HEAD~1",
            "xargs --max 1 git reset --hard HEAD~1",
            "xargs -a git echo reset --hard HEAD~1",
            "xargs -I git echo reset --hard HEAD~1",
            "xargs -I{} echo {}",
            "xargs --max-lines git status --short",
            "xargs --max-p 1 git status --short",
            "echo HEAD | xargs echo git reset --hard",
            "find . -name git -print",
            "find . -exec git status --short {} \\;",
            "find . -execdir git diff -- {} +",
            "find . -exec echo git reset --hard {} \\;",
            "find reset -maxdepth 0 -exec echo git {} --hard HEAD~1 \\;",
            "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=color.ui GIT_CONFIG_VALUE_0=always git status --short",
            "GIT_CONFIG_COUNT=0 GIT_CONFIG_KEY_0=alias.wipe GIT_CONFIG_VALUE_0='!git reset --hard HEAD~1' git wipe",
            "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.wipe git wipe",
            "cmd.exe /c\"git status --short\"",
            "sudo -h git reset --hard HEAD~1",
            "echo '$(git reset --hard)'",
            "echo \"$(echo git reset --hard)\"",
            "printf '%s ' git status --short | bash",
            "printf -- 'git status --short' | bash",
            "printf '%s' 'git status --short' | cat | bash",
            "printf '%s' 'git status --short' | cat - | bash",
            "printf '%s' 'git status --short' | cat -- | bash",
            "printf '%s' 'git status --short' | cat -u | bash",
            "echo -n 'git status --short' | bash",
            "Write-Output -NoEnumerate 'git status --short' | Invoke-Expression",
            "Write-Output -NoE 'git status --short' | Invoke-Expression",
            "Write-Output -Input 'git status --short' | Invoke-Expression",
            "Write-Output -Inp 'git status --short' | Invoke-Expression",
            "Write-Output -Input:'git status --short' | Invoke-Expression",
            "Write-Output -Verbose 'git status --short' | Invoke-Expression",
            "Write-Output -ErrorAction Stop 'git status --short' | Invoke-Expression",
            "Write-Output -ErrorA Stop 'git status --short' | Invoke-Expression",
            "echo -NoE 'git status --short' | iex",
            "echo -EA Stop 'git status --short' | iex",
            "write 'git status --short' | iex",
            "Write-Output 0,'git status --short' | iex",
            "Write-Output -NoEnumerate 0,'git reset --hard HEAD~1' | iex",
            "echo -NoE 0,'git reset --hard HEAD~1' | iex",
            "Write-Output '0,git reset --hard HEAD~1' | iex",
            "Write-Output 'git status --short' | & iex",
            "echo 'git status --short' |& bash",
            "echo 0,'git reset --hard HEAD~1' | bash",
            "echo 1 'git reset --hard HEAD~1' | bash",
            "Write-Output -I 'git reset --hard HEAD~1' | Invoke-Expression",
            "bash <<<'git status --short'",
            "if bash <<<'git status --short'; then :; fi",
            "if echo git reset --hard; then true; fi",
            "if cat <<'EOF'\ngit reset --hard\nEOF\nthen :; fi",
            "cat <<'EOF'\ngit reset --hard\nEOF",
            "cat <<E'OF'\ngit reset --hard\nEOF",
            "cat <<E\\OF\ngit restore src/lib.rs\nEOF",
            "cat <<EOF\ngit reset --hard\nEOF",
            "$text = @'\ngit restore src/lib.rs\n'@\nWrite-Output $text",
            "$text = @\"\ngit restore src/lib.rs\n\"@\nWrite-Output $text",
            "Invoke-Expression @'\ngit status --short\n'@",
            "Invoke-Expression -Command 'git status --short'",
            "Invoke-Expression -Verbose -Command 'git status --short'",
            "Invoke-Expression -ErrorAction Stop -Command 'git status --short'",
            "Invoke-Expression -ErrorA Stop 'git status --short'",
            "Invoke-Expression -Error Stop 'git reset --hard HEAD~1'",
        ] {
            assert!(
                !shell_command_has_workflow_sensitive_git_operation(command),
                "expected read-only Git command for {command:?}"
            );
        }
    }

    #[test]
    fn workflow_sensitive_git_detection_only_applies_to_shell_tools() {
        let input = serde_json::json!({ "command": "git reset --hard HEAD~1" });
        assert!(is_workflow_sensitive_git_command("Bash", &input));
        assert!(is_workflow_sensitive_git_command("PowerShell", &input));
        assert!(!is_workflow_sensitive_git_command("Read", &input));

        let host_specific_pipeline =
            serde_json::json!({ "command": "echo 0,'git reset --hard HEAD~1' | bash" });
        assert!(!is_workflow_sensitive_git_command(
            "Bash",
            &host_specific_pipeline
        ));
        assert!(is_workflow_sensitive_git_command(
            "PowerShell",
            &host_specific_pipeline
        ));

        let invoke_expression = serde_json::json!({
            "command": "iex \"echo 0,'git reset --hard HEAD~1' | cat | bash\""
        });
        assert!(is_workflow_sensitive_git_command(
            "PowerShell",
            &invoke_expression
        ));

        let powershell_wrapper = serde_json::json!({
            "command": "pwsh -c \"echo 0,'git reset --hard HEAD~1' | bash\""
        });
        assert!(is_workflow_sensitive_git_command(
            "Bash",
            &powershell_wrapper
        ));
        assert!(is_workflow_sensitive_git_command(
            "PowerShell",
            &powershell_wrapper
        ));

        let posix_wrapper = serde_json::json!({
            "command": "bash -c \"echo 0,'git reset --hard HEAD~1' | bash\""
        });
        assert!(!is_workflow_sensitive_git_command(
            "PowerShell",
            &posix_wrapper
        ));

        let posix_eval = serde_json::json!({
            "command": "eval \"echo 0,'git reset --hard HEAD~1' | bash\""
        });
        assert!(!is_workflow_sensitive_git_command("Bash", &posix_eval));
    }

    #[test]
    fn sensitive_git_checkout_detection_only_applies_to_shell_tools() {
        let input = serde_json::json!({ "command": "git checkout -- src/lib.rs" });
        assert!(is_sensitive_git_checkout_command("Bash", &input));
        assert!(is_sensitive_git_checkout_command("PowerShell", &input));
        assert!(!is_sensitive_git_checkout_command("Read", &input));
    }

    #[test]
    fn sensitive_git_stash_detection_only_applies_to_shell_tools() {
        let input = serde_json::json!({ "command": "git stash pop" });
        assert!(is_sensitive_git_stash_command("Bash", &input));
        assert!(is_sensitive_git_stash_command("PowerShell", &input));
        assert!(!is_sensitive_git_stash_command("Read", &input));
    }

    #[test]
    fn sensitive_file_deletion_detection_flags_shell_deletion_commands() {
        for command in [
            "rm old.log",
            "rm -rf target/tmp",
            "unlink stale.sock",
            "rmdir empty-dir",
            "git rm src/old.rs",
            "git clean -fd",
            "find . -name '*.tmp' -delete",
            "find . -name '*.tmp' -exec rm {} ;",
            "xargs rm -f",
            "Remove-Item .\\old.txt",
            "Microsoft.PowerShell.Management\\Remove-Item .\\old.txt",
            "Remove-ItemProperty HKCU:\\Software\\Temp -Name stale",
            "Clear-RecycleBin -Force",
            "Get-ChildItem cache | ForEach-Object { Remove-Item $_.FullName -Force }",
            "if (Test-Path cache) { Remove-Item cache -Recurse -Force }",
            "del old.txt",
            "deltree /y old-dir",
            "sdelete64 -p 1 secret.txt",
            "powershell -Command Remove-Item old.txt",
            "cmd.exe /d /s /c del /f /q old.txt",
            r#""C:\Windows\System32\cmd.exe" /d /c del /q old.txt"#,
            "cmd /c if exist old.txt del /q old.txt",
            r#"cmd /c for %F in (*.tmp) do erase "%F""#,
            r#"forfiles /p . /m *.tmp /c "cmd /c del /q @path""#,
            "start /wait cmd.exe /c rd /s /q cache",
            "robocopy empty target /MIR",
            "robocopy source target /MOVE",
        ] {
            assert!(
                shell_command_has_sensitive_file_deletion(command),
                "expected sensitive deletion detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_file_deletion_detection_allows_non_deleting_commands() {
        for command in [
            "ls old.log",
            "grep rm README.md",
            "git clean -nd",
            "git status",
            "echo 'rm old.log'",
            "Remove-Item -WhatIf old.txt",
            "if (Test-Path cache) { Remove-Item cache -WhatIf:$true }",
            "cmd /c echo del old.txt",
            "powershell -Command Write-Output Remove-Item",
            r#"cmd /c start "report" /wait notepad.exe report.txt"#,
            "robocopy source target /E",
            "robocopy source target /MIR /L",
        ] {
            assert!(
                !shell_command_has_sensitive_file_deletion(command),
                "expected no deletion detection for {command:?}"
            );
        }
    }

    fn test_scratchpad_root() -> std::path::PathBuf {
        if cfg!(windows) {
            std::path::PathBuf::from(r"C:\Users\u\AppData\Local\Temp\rebon\proj\sess\scratchpad")
        } else {
            std::path::PathBuf::from("/tmp/rebon/proj/sess/scratchpad")
        }
    }

    #[test]
    fn deletion_confined_to_root_allows_literal_scratchpad_targets() {
        let root = test_scratchpad_root();
        let d = root.display().to_string();
        for command in [
            format!("rm -rf {d}/pkg-bundle"),
            format!("rm {d}/a.txt {d}/b.txt"),
            format!("rm -rf {d}"),
            format!("rm -- {d}/dashed-file"),
            format!("rmdir {d}/empty"),
            format!("Remove-Item -Recurse -Force {d}/bundle"),
            format!("cd /somewhere && rm -rf {d}/out"),
            format!("mkdir -p {d}/x; rm -rf {d}/x"),
        ] {
            assert!(
                shell_command_deletions_confined_to_root(&command, &root),
                "expected confined deletion for {command:?}"
            );
        }
    }

    #[test]
    fn deletion_confined_to_root_allows_guarded_literal_powershell_variable() {
        let root = test_scratchpad_root();
        let destination = root.join("verifycopy");
        let command = format!(
            "$dest = '{}'; if (Test-Path $dest) {{ Remove-Item -Recurse -Force $dest }}; Copy-Item project $dest",
            destination.display()
        );

        assert!(shell_command_deletions_confined_to_root(&command, &root));
        assert!(shell_command_deletions_confined_to_root(
            &format!("$Dest = '{}'; Remove-Item $DEST", destination.display()),
            &root
        ));
    }

    #[test]
    fn deletion_confined_to_root_allows_static_powershell_join_path() {
        let root = test_scratchpad_root();
        let d = root.display();
        for command in [
            format!(
                "$scratch = '{d}'; $dest = Join-Path $scratch child; Remove-Item -Recurse -Force $dest"
            ),
            format!(
                "$scratch='{d}'; $dest=Join-Path $scratch ('nested/bundle'); Remove-Item $dest"
            ),
            format!(
                "$scratch='{d}'; $child='bundle'; $dest=join-path $scratch $child; Remove-Item $dest"
            ),
        ] {
            assert!(
                shell_command_deletions_confined_to_root(&command, &root),
                "expected static Join-Path deletion to be confined: {command:?}"
            );
        }
    }

    #[test]
    fn deletion_confined_to_root_rejects_arrays_and_unsafe_variable_values() {
        let root = test_scratchpad_root();
        let d = root.display();
        for command in [
            format!(r#"Remove-Item -LiteralPath "{d}/inside","/outside""#),
            "$dest = '/outside'; Remove-Item -Recurse -Force $dest".to_string(),
            format!("$dest = '{d}/safe'; $dest='/outside'; Remove-Item $dest"),
            format!("$dest = '{d}/safe'; $DEST='/outside'; Remove-Item $dest"),
            format!("$dest = '{d}/safe'; if ($true) {{ $dest = '/outside' }}; Remove-Item $dest"),
            format!("$dest = Join-Path '{d}' ../outside; Remove-Item $dest"),
            format!("$dest = Join-Path '{d}' /outside; Remove-Item $dest"),
            format!("$dest = Join-Path '{d}' (Get-Evil); Remove-Item $dest"),
            format!("$dest = Join-Path '{d}' $dynamic; Remove-Item $dest"),
            format!("$dest = Join-Path '{d}' child extra; Remove-Item $dest"),
            format!("Remove-Item -Path:$dest {d}/safe"),
        ] {
            assert!(
                !shell_command_deletions_confined_to_root(&command, &root),
                "expected unsafe PowerShell deletion to remain sensitive: {command:?}"
            );
        }
    }

    #[test]
    fn deletion_confined_to_root_rejects_existing_link_escape() {
        let base = tempfile::Builder::new()
            .prefix("rebon-tools-core-deletion-scope-")
            .tempdir()
            .unwrap();
        let root = base.path().join("scratchpad");
        let outside = base.path().join("outside");
        let link = root.join("link-outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        #[cfg(unix)]
        let link_created = std::os::unix::fs::symlink(&outside, &link).is_ok();
        #[cfg(windows)]
        let link_created = std::os::windows::fs::symlink_dir(&outside, &link).is_ok();

        if link_created {
            let command = format!("Remove-Item -Recurse -Force {}/payload", link.display());
            assert!(!shell_command_deletions_confined_to_root(&command, &root));
        }
    }

    #[test]
    fn deletion_confined_to_root_rejects_escapes_and_dynamic_targets() {
        let root = test_scratchpad_root();
        let d = root.display().to_string();
        for command in [
            "rm -rf /etc/passwd".to_string(),
            format!("rm -rf {d}/../sibling"),
            format!("rm -rf {d}/ok /other/path"),
            format!("rm -rf {d}/ok; rm -rf /other/path"),
            format!("rm -rf $SCRATCH/x"),
            format!("rm -rf {d}/nested/*"),
            format!("rm -rf {d}/{{a,b}}"),
            format!("rm -rf ~/{d}"),
            format!("find {d} -name '*.tmp' -delete"),
            format!("bash -c 'rm -rf {d}/x'"),
            format!("git clean -fd {d}"),
            format!("Remove-Item -Path:{d}/bundle -Recurse"),
            format!("rm -rf {d}-sibling"),
            "rm -rf".to_string(),
            "ls no-deletion-here".to_string(),
            format!("rm -rf relative/scratchpad"),
        ] {
            assert!(
                !shell_command_deletions_confined_to_root(&command, &root),
                "expected NOT confined for {command:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn deletion_confined_to_root_matches_windows_paths_case_insensitively() {
        let root = test_scratchpad_root();
        let d = root.display().to_string();
        let backslashed = d.replace('/', "\\");
        for command in [
            format!("Remove-Item -Recurse -Force {backslashed}\\bundle"),
            format!("rm -rf {}", d.to_ascii_lowercase()),
        ] {
            assert!(
                shell_command_deletions_confined_to_root(&command, &root),
                "expected confined deletion for {command:?}"
            );
        }
    }

    #[test]
    fn exempt_deletion_root_declassifies_confined_deletions_only() {
        let root = test_scratchpad_root();
        let d = root.display().to_string();
        let confined = serde_json::json!({ "command": format!("rm -rf {d}/bundle") });
        assert!(is_auto_mode_sensitive_tool_input("Bash", &confined));
        assert!(
            !is_auto_mode_sensitive_tool_input_with_exempt_deletion_root(
                "Bash",
                &confined,
                Some(&root)
            )
        );

        let powershell_join = serde_json::json!({
            "command": format!(
                "$ErrorActionPreference='Stop'; $scratch='{d}'; $dir=Join-Path $scratch ('verification-harness'); if (Test-Path $dir) {{ Remove-Item -Recurse -Force $dir }}"
            )
        });
        assert!(is_auto_mode_sensitive_tool_input(
            "PowerShell",
            &powershell_join
        ));
        assert!(
            !is_auto_mode_sensitive_tool_input_with_exempt_deletion_root(
                "PowerShell",
                &powershell_join,
                Some(&root)
            )
        );

        let escaping = serde_json::json!({ "command": "rm -rf /other/place" });
        assert!(is_auto_mode_sensitive_tool_input_with_exempt_deletion_root(
            "Bash",
            &escaping,
            Some(&root)
        ));

        // A confined deletion combined with another sensitive class
        // stays sensitive.
        let stash = serde_json::json!({
            "command": format!("rm -rf {d}/bundle && git stash drop")
        });
        assert!(is_auto_mode_sensitive_tool_input_with_exempt_deletion_root(
            "Bash",
            &stash,
            Some(&root)
        ));
    }

    #[test]
    fn sensitive_git_content_restore_detection_flags_redirected_output() {
        for command in [
            "git show HEAD:File.cs > File.cs",
            r#"git -C "D:/work/example-project" show HEAD:File.cs > "D:/work/example-project/.baseline_File.cs""#,
            "git show HEAD:src/lib.rs >src/lib.rs",
            "git cat-file -p abc123 > src/lib.rs",
            "git show HEAD:src/lib.rs | tee src/lib.rs",
            "git show HEAD:src/lib.rs | Out-File src/lib.rs",
            "git show HEAD:src/lib.rs | Set-Content src/lib.rs",
            r#"bash -c "git show HEAD:src/lib.rs > src/lib.rs""#,
            r#"cmd.exe /d /c "git show HEAD:src/tui/mod.rs > C:\projects\example\src\tui\mod.rs""#,
        ] {
            assert!(
                shell_command_has_sensitive_git_content_restore(command),
                "expected content-restore detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_git_content_restore_detection_allows_inspection() {
        for command in [
            "git show HEAD:src/lib.rs",
            "git show HEAD:src/lib.rs | findstr /n SafeTapEvent",
            "git show HEAD:src/lib.rs | grep -n unlink",
            "git show --stat HEAD",
            "git show HEAD --stat > stats.txt",
            "git log --oneline > log.txt",
            "git diff HEAD~1 -- src/lib.rs",
        ] {
            assert!(
                !shell_command_has_sensitive_git_content_restore(command),
                "expected no content-restore detection for {command:?}"
            );
        }
    }

    #[test]
    fn git_content_restore_detects_capture_then_write_laundering() {
        for command in [
            r#"$lines = & git show HEAD:src/lib.rs; [System.IO.File]::WriteAllText("src/lib.rs", $text)"#,
            "$c = & git show HEAD:src/lib.rs\nSet-Content -Path src/lib.rs -Value $c",
            r#"$c = & git cat-file blob HEAD:src/lib.rs; $c | Out-File src/lib.rs"#,
            r#"$c = & git show HEAD:src/lib.rs; Copy-Item baseline.rs src/lib.rs"#,
        ] {
            assert!(
                shell_command_has_sensitive_git_content_restore(command),
                "expected committed-content restore detection for {command:?}"
            );
        }
        for command in [
            r#"[System.IO.File]::WriteAllText("src/lib.rs", $text)"#,
            r#"Set-Content -Path src/lib.rs -Value $text"#,
            "git show HEAD:src/lib.rs",
            "$c = & git show HEAD:src/lib.rs; Write-Output $c",
            "git log --oneline -5; Set-Content -Path notes.txt -Value $text",
        ] {
            assert!(
                !shell_command_has_sensitive_git_content_restore(command),
                "expected no restore detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_powershell_file_deletion_detection_flags_deletions() {
        for command in [
            r#"[System.IO.File]::Delete("old.txt")"#,
            r#"[IO.Directory]::Delete("cache", $true)"#,
            r#"[System.IO.FileInfo]::new("old.txt").Delete()"#,
            r#"[Microsoft.VisualBasic.FileIO.FileSystem]::DeleteDirectory("cache", 'DeleteAllContents')"#,
            r#"$item = Get-Item "old.txt"; $item.Delete()"#,
            r#"(Get-Item "old.txt").Delete()"#,
            r#"Get-ChildItem cache | ForEach-Object { $_.Delete() }"#,
            r#"foreach ($path in $paths) { [IO.File]::Delete($path) }"#,
            r#"pwsh -Command "[IO.File]::Delete('old.txt')""#,
        ] {
            assert!(
                shell_command_has_sensitive_powershell_file_deletion(command),
                "expected PowerShell file-deletion detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_powershell_file_deletion_detection_allows_writes_and_read_only() {
        for command in [
            r#"[System.IO.File]::ReadAllText("src/lib.rs")"#,
            r#"Get-Content "src/lib.rs""#,
            r#"Write-Output '[System.IO.File]::WriteAllText("src/lib.rs", "text")'"#,
            r#"Write-Output '$item.Delete()'"#,
            r#"[IO.Directory]::Exists("cache")"#,
            r#"Set-Content -WhatIf -Path "src/lib.rs" -Value $text"#,
            // Ordinary writes, copies, and moves are edits, not
            // destruction — auto mode approves them.
            r#"[System.IO.File]::WriteAllText("src/lib.rs", $text)"#,
            r#"$result = [IO.File]::Copy("baseline.rs", "src/lib.rs", $true)"#,
            r#"$result=[IO.File]::Move("old.rs", "src/lib.rs", $true)"#,
            r#"Set-Content -Path "src/lib.rs" -Value $text"#,
            r#"$text | Out-File -FilePath "src/lib.rs" -Encoding utf8"#,
            r#"pwsh -Command "[IO.File]::WriteAllBytes('src/data.bin', $bytes)""#,
        ] {
            assert!(
                !shell_command_has_sensitive_powershell_file_deletion(command),
                "expected no PowerShell file-deletion detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_interpreter_deletion_detection_flags_inline_destructive_apis() {
        for command in [
            r#"python -c "import os; [os.remove(f) for f in files]""#,
            r#"python3 -c "from pathlib import Path; Path('x.cs').unlink()""#,
            r#"python3 -c "from pathlib import Path; Path('cache').rmdir()""#,
            r#"python -c "import shutil; shutil.rmtree('cache')""#,
            r#"python -c "import os; os.ftruncate(fd, 0)""#,
            r#"python -c "import os; os.system('del /q old.txt')""#,
            r#"python -c "import subprocess; subprocess.run(['rm', '-rf', 'cache'])""#,
            r#"python -c "import runpy; runpy.run_path('cleanup.py')""#,
            r#"python -c "exec(open('cleanup.py').read())""#,
            r#"node -e "const fs=require('fs'); fs.unlinkSync('x')""#,
            r#"node -e "require('fs').rmSync('x', {recursive: true})""#,
            r#"deno eval "await Deno.remove('x')""#,
            r#"perl -e "unlink('x')""#,
            r#"pwsh -Command "python -c 'import shutil; shutil.rmtree(d)'""#,
        ] {
            assert!(
                shell_command_has_sensitive_interpreter_file_deletion(command),
                "expected interpreter deletion detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_interpreter_deletion_detection_allows_reads_and_writes() {
        for command in [
            r#"python -c "import json; [json.load(open(p, encoding='utf-8')) for p in paths]""#,
            r#"python -c "print(open('x.cs').read())""#,
            r#"python -c "import sys; print(sys.version)""#,
            r#"node -e "console.log(require('fs').readFileSync('x','utf8'))""#,
            "python --version",
            "python -V",
            "node scripts/lint.js",
            // Writes, copies, and renames are edits, not destruction.
            r#"python -c "import os, shutil; shutil.copyfile('a.cs','b.cs')""#,
            r#"python -c "import os, subprocess; [(open(os.path.join(r,f),'wb').write(data)) for f in files]""#,
            r#"python3 -c "Path('x.cs').write_text(content)""#,
            r#"node -e "require('fs').writeFileSync('x', 'data')""#,
            r#"php -r "file_put_contents('x', $data);""#,
        ] {
            assert!(
                !shell_command_has_sensitive_interpreter_file_deletion(command),
                "expected no interpreter deletion detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_script_execution_detection_flags_uninspected_python_and_powershell_code() {
        for command in [
            "python cleanup.py",
            r#""C:\Python312\python.exe" scripts\cleanup.py"#,
            "py -3 scripts/cleanup.py --force",
            "python -m cleanup_cache",
            "python -",
            "./cleanup.py",
            "pwsh -File scripts/cleanup.ps1",
            "powershell -NoProfile ./cleanup.ps1",
            "powershell -EncodedCommand ZABlAGwAIAB4AA==",
            "./cleanup.ps1",
            ". ./cleanup.ps1",
            "Invoke-Expression $script",
            "& $script",
            "pwsh -Command $script",
            "Invoke-Command -FilePath cleanup.ps1",
            "Get-Content cleanup.py | python",
            "Get-Content cleanup.py | python -W ignore",
            "Get-Content cleanup.ps1 | pwsh -ExecutionPolicy Bypass",
            "Get-Content cleanup.ps1 | pwsh -ExecutionPolicy Bypass -",
            "Get-Content cleanup.ps1 | pwsh -Command -",
            r#"cmd.exe /c "python cleanup.py""#,
            r#"pwsh -Command "& ./cleanup.ps1""#,
        ] {
            assert!(
                shell_command_has_sensitive_script_execution(command),
                "expected uninspected script execution detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_script_execution_detection_allows_inspection_and_inline_read_only_code() {
        for command in [
            "python --version",
            "python -V",
            "python -VV",
            "python -h",
            r#"python -c "print(open('report.txt').read())""#,
            r#"Write-Output data | python -W ignore -c "print('ok')""#,
            "pwsh -Help",
            "powershell -Command Get-Date",
            "Write-Output data | python --version",
            "Write-Output data | pwsh -ExecutionPolicy Bypass -Command Get-Date",
            "Write-Output data | pwsh -Command Get-Date",
            "Get-Content scripts/cleanup.ps1",
            "Write-Output cleanup.py",
            "node scripts/lint.js",
        ] {
            assert!(
                !shell_command_has_sensitive_script_execution(command),
                "expected no uninspected script execution detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_file_deletion_detection_sees_quoted_wrapper_payloads() {
        for command in [
            r#"powershell -Command "Remove-Item old.txt""#,
            r#"pwsh -NoProfile -Command "Remove-Item -Recurse -Force build""#,
            r#"bash -c "rm -rf target/tmp""#,
            r#"cmd /c "del old.txt""#,
            r#"cmd /c start "cleanup" /wait cmd.exe /c del /q old.txt"#,
        ] {
            assert!(
                shell_command_has_sensitive_file_deletion(command),
                "expected quoted wrapper deletion detection for {command:?}"
            );
        }
    }

    #[test]
    fn sensitive_git_detection_sees_quoted_wrapper_payloads() {
        assert!(shell_command_has_sensitive_git_worktree(
            r#"bash -c "git checkout -- src/lib.rs""#
        ));
        assert!(shell_command_has_sensitive_git_stash(
            r#"pwsh -Command "git stash pop""#
        ));
        assert!(!shell_command_has_sensitive_git_worktree(
            r#"bash -c "git status""#
        ));
    }

    #[test]
    fn auto_mode_classifies_read_only_python_heredocs_for_semantic_review() {
        let commands = [
            r#"python - <<'PY'
import io, tarfile, urllib.request
url='https://registry.npmjs.org/create-fumadocs-app/-/create-fumadocs-app-16.1.5.tgz'
with urllib.request.urlopen(url) as response:
    archive=tarfile.open(fileobj=io.BytesIO(response.read()), mode='r:gz')
for name in archive.getnames():
    if name.endswith(('.tsx','.ts','.mjs','.css','.json')):
        print(name)
PY"#,
            r#"PYTHONIOENCODING=utf-8 python - <<'PY'
import io, tarfile, urllib.request
url='https://registry.npmjs.org/fumadocs-core/-/fumadocs-core-16.11.5.tgz'
with urllib.request.urlopen(url) as response:
    archive=tarfile.open(fileobj=io.BytesIO(response.read()), mode='r:gz')
text=archive.extractfile('package/dist/search/client/orama-static.js').read().decode('utf-8')
print(text)
PY"#,
        ];

        for command in commands {
            let disposition =
                classify_auto_mode_tool_input("Bash", &serde_json::json!({ "command": command }));
            let AutoModeInputDisposition::EmbeddedScriptReview(script) = disposition else {
                panic!("expected semantic review for {command:?}, got {disposition:?}");
            };
            assert_eq!(script.interpreter, EmbeddedInterpreter::Python);
            assert!(script.source.contains("urllib.request.urlopen"));
            assert!(!script.source.contains("\nPY"));
            assert!(is_auto_mode_sensitive_tool_input(
                "Bash",
                &serde_json::json!({ "command": command })
            ));
        }
    }

    #[test]
    fn auto_mode_classifies_versioned_python_heredocs_for_semantic_review() {
        for command in [
            "python3.12 - <<'PY'\nprint(open('package.json').read())\nPY",
            "/usr/bin/python3.13 - <<'PY'\nprint(open('package.json').read())\nPY",
            "python3.12.exe - <<'PY'\nprint(open('package.json').read())\nPY",
            "pypy3.10 - <<'PY'\nprint(open('package.json').read())\nPY",
        ] {
            let disposition =
                classify_auto_mode_tool_input("Bash", &serde_json::json!({ "command": command }));
            let AutoModeInputDisposition::EmbeddedScriptReview(script) = disposition else {
                panic!("expected semantic review for {command:?}, got {disposition:?}");
            };
            assert_eq!(script.interpreter, EmbeddedInterpreter::Python);
        }
    }

    #[test]
    fn auto_mode_classifies_read_only_node_heredoc_for_semantic_review() {
        let command = r#"node - <<'NODE'
const fs = require('fs');
const crypto = require('crypto');
console.log(crypto.createHash('sha256').update(fs.readFileSync('package.tgz')).digest('hex'));
NODE"#;
        let disposition =
            classify_auto_mode_tool_input("Bash", &serde_json::json!({ "command": command }));
        let AutoModeInputDisposition::EmbeddedScriptReview(script) = disposition else {
            panic!("expected semantic review, got {disposition:?}");
        };
        assert_eq!(script.interpreter, EmbeddedInterpreter::Node);
        assert_eq!(script.invocation, "node -");
    }

    #[test]
    fn auto_mode_keeps_deleting_heredocs_hard_sensitive() {
        for command in [
            "python - <<'PY'\nimport os\nos.remove('x')\nPY",
            "python3.12 - <<'PY'\nimport shutil\nshutil.rmtree('cache')\nPY",
            "python - <<'PY'\nfrom pathlib import Path\nPath('x').unlink()\nPY",
            "python - <<'PY'\nimport os\nos.truncate('x', 0)\nPY",
            "python - <<'PY'\nexec(open('cleanup.py').read())\nPY",
            "node - <<'NODE'\nrequire('fs').rmSync('x')\nNODE",
            "node - <<'NODE'\nrequire('fs').unlinkSync('x')\nNODE",
        ] {
            assert!(
                matches!(
                    classify_auto_mode_tool_input(
                        "Bash",
                        &serde_json::json!({ "command": command })
                    ),
                    AutoModeInputDisposition::HardSensitive(_)
                ),
                "expected hard sensitivity for {command:?}"
            );
        }
    }

    #[test]
    fn auto_mode_routes_writing_heredocs_to_semantic_review() {
        // Writes are edits, not destruction: they skip the hard gate
        // and go to the semantic classifier like read-only scripts.
        for command in [
            "python - <<'PY'\nfrom pathlib import Path\nPath('x').write_text('data')\nPY",
            "python3.13.exe - <<'PY'\nopen('x', 'w').write('data')\nPY",
            "python - <<'PY'\nimport tarfile\ntarfile.open('x.tgz').extractall('.')\nPY",
            "node - <<'NODE'\nrequire('fs').writeFileSync('x', 'data')\nNODE",
        ] {
            let disposition =
                classify_auto_mode_tool_input("Bash", &serde_json::json!({ "command": command }));
            assert!(
                matches!(
                    disposition,
                    AutoModeInputDisposition::EmbeddedScriptReview(_)
                ),
                "expected semantic review for {command:?}, got {disposition:?}"
            );
        }
    }

    #[test]
    fn auto_mode_keeps_ambiguous_heredocs_hard_sensitive() {
        let oversized = format!(
            "python - <<'PY'\n{}\nPY",
            "x".repeat(MAX_EMBEDDED_SCRIPT_BYTES + 1)
        );
        let unrecognized_oversized = format!(
            "bash <<'SH'\n{}\nSH",
            "x".repeat(MAX_EMBEDDED_SCRIPT_BYTES + 1)
        );
        for command in [
            "python - <<PY\nprint('x')\nPY".to_owned(),
            "python - <<'PY'\nprint('x')".to_owned(),
            "python - <<'PY'\nprint('x')\nPY\necho done".to_owned(),
            "python - <<'PY' <<'OTHER'\nprint('x')\nPY".to_owned(),
            "sudo python - <<'PY'\nprint('x')\nPY".to_owned(),
            "p\\ython - <<'PY'\nopen('x', 'w').write('data')\nPY".to_owned(),
            "${PYTHON:-python} - <<'PY'\nopen('x', 'w').write('data')\nPY".to_owned(),
            "$PYTHON - <<'PY'\nopen('x', 'w').write('data')\nPY".to_owned(),
            "PYTHON=python\n$PYTHON - <<'PY'\nopen('x', 'w').write('data')\nPY".to_owned(),
            ":\np\\ython - <<'PY'\nopen('x', 'w').write('data')\nPY".to_owned(),
            "\n$PYTHON - <<'PY'\nopen('x', 'w').write('data')\nPY".to_owned(),
            "# choose interpreter\n$PYTHON - <<'PY'\nopen('x', 'w').write('data')\nPY".to_owned(),
            "$PYTHON - <<'PY'".to_owned(),
            "# << decoy\nPYTHON=python\n$PYTHON - <<'PY'\nopen('x', 'w').write('data')\nPY"
                .to_owned(),
            "printf '%s' '<<'\np\\ython - <<'PY'\nopen('x', 'w').write('data')\nPY".to_owned(),
            "bash <<'SH'\ntouch x\nSH".to_owned(),
            "mystery <<'X'\nopaque\nX".to_owned(),
            "bash <<'SH'\ntouch x".to_owned(),
            "bash <<'A' <<'B'\ntouch x\nA\nB".to_owned(),
            "node20 - <<'JS'\nrequire('fs').writeFileSync('x', 'data')\nJS".to_owned(),
            "ruby <<'RB'\nputs 'x'\nRB".to_owned(),
            oversized,
            unrecognized_oversized,
        ] {
            assert!(
                matches!(
                    classify_auto_mode_tool_input(
                        "Bash",
                        &serde_json::json!({ "command": command })
                    ),
                    AutoModeInputDisposition::HardSensitive(_)
                ),
                "expected hard sensitivity for {command:?}"
            );
        }
    }

    #[test]
    fn auto_mode_allows_literal_data_sink_heredocs() {
        for command in [
            // The ultrawork sub-agent probe pattern that motivated the
            // exemption: write a scratch file, nothing executes.
            "cat > /tmp/ufl_customer_layout_probe.jl <<'EOF'\nusing JuMP, HiGHS\nmodel = Model()\nEOF",
            "cat <<'EOF' > notes.txt\nplain text\nEOF",
            "cat >> log.txt <<'EOF'\nappended\nEOF",
            "cat <<'EOF'\njust printed\nEOF",
            "cat <<\"EOF\" > out.txt\ndouble quoted delimiter\nEOF",
            "cat > out.txt <<EOF\nno expansions in this body\nEOF",
            "FOO=bar cat > out.txt <<'EOF'\ndata\nEOF",
            "tee /tmp/a.txt <<'EOF'\ndata\nEOF",
            "tee -a /tmp/a.txt <<'EOF'\ndata\nEOF",
            "tee /tmp/a.txt > /dev/null <<'EOF'\ndata\nEOF",
            // The body is inert data even when it *mentions* sensitive
            // commands — it is written, never run.
            "cat > cleanup.sh <<'EOF'\nrm -rf ./build\ngit stash drop\nEOF",
            // The residual command around the heredoc is classified on
            // its own and stays benign here.
            "cat > /tmp/probe.jl <<'EOF'\nprobe\nEOF\njulia /tmp/probe.jl",
            "echo start\ncat > /tmp/x.txt <<'EOF'\ndata\nEOF\necho done",
            "cat > a.txt <<'EOF'\none\nEOF\ncat > b.txt <<'EOF'\ntwo\nEOF",
        ] {
            assert_eq!(
                classify_auto_mode_tool_input("Bash", &serde_json::json!({ "command": command })),
                AutoModeInputDisposition::NoSpecialRisk,
                "expected literal data sink heredoc to carry no special risk: {command:?}"
            );
        }
    }

    #[test]
    fn auto_mode_keeps_non_literal_sink_heredocs_hard_sensitive() {
        for command in [
            // Dynamic target — could resolve anywhere.
            "cat > $FILE <<'EOF'\ndata\nEOF",
            // Unquoted delimiter with an expanding body: the command
            // substitution runs when the shell reads the heredoc.
            "cat > /tmp/x.txt <<EOF\n$(rm -rf /)\nEOF",
            "cat > /tmp/x.txt <<EOF\n`rm -rf /`\nEOF",
            // Device / kernel pseudo-file writes.
            "cat > /dev/sda <<'EOF'\ndata\nEOF",
            "cat > /proc/sys/vm/drop_caches <<'EOF'\n1\nEOF",
            // Tilde expands to $HOME.
            "cat > ~/x.txt <<'EOF'\ndata\nEOF",
            // Wrapper command in front of the sink.
            "sudo cat > /etc/hosts <<'EOF'\ndata\nEOF",
            // Quoted or ambiguous redirect shapes.
            "cat > 'a b.txt' <<'EOF'\ndata\nEOF",
            "cat > a.txt > b.txt <<'EOF'\ndata\nEOF",
            "cat 2> err.log <<'EOF'\ndata\nEOF",
            // Reads a file besides the heredoc.
            "cat extra.txt > x.txt <<'EOF'\ndata\nEOF",
            // Unknown tee flag.
            "tee -x /tmp/a.txt <<'EOF'\ndata\nEOF",
            // Missing terminator stays fail-closed.
            "cat > x.txt <<'EOF'\nunterminated",
            // The residual command after stripping is still classified:
            // running the freshly written Python script needs approval.
            "cat > run.py <<'EOF'\nprint('x')\nEOF\npython run.py",
            // A sink-shaped line inside another heredoc's body is data
            // for that heredoc, never a strippable command.
            "python - <<'PY'\ncat > /tmp/x <<'EOF'\nimport os; os.remove('victim')\nEOF\nprint('ok')\nPY",
        ] {
            assert!(
                matches!(
                    classify_auto_mode_tool_input(
                        "Bash",
                        &serde_json::json!({ "command": command })
                    ),
                    AutoModeInputDisposition::HardSensitive(_)
                ),
                "expected hard sensitivity for {command:?}"
            );
        }
    }

    #[test]
    fn auto_mode_reviews_interpreter_heredoc_after_stripping_sink_heredoc() {
        let command = "cat > config.json <<'JSON'\n{\"key\": 1}\nJSON\npython - <<'PY'\nprint(open('config.json').read())\nPY";
        let disposition =
            classify_auto_mode_tool_input("Bash", &serde_json::json!({ "command": command }));
        let AutoModeInputDisposition::EmbeddedScriptReview(script) = disposition else {
            panic!("expected semantic review after sink strip, got {disposition:?}");
        };
        assert_eq!(script.interpreter, EmbeddedInterpreter::Python);
        assert!(script.source.contains("config.json"));
        assert!(!script.source.contains("JSON"));
    }

    #[test]
    fn continuity_mode_reviews_visible_shell_and_powershell_heredocs() {
        for (command, expected) in [
            (
                "bash <<'SH'\nprintf '%s\\n' ready\ncargo test -p rebon-core\nSH",
                EmbeddedInterpreter::Bash,
            ),
            (
                "pwsh -NoProfile -Command - <<'PS'\nGet-Date\nPS",
                EmbeddedInterpreter::PowerShell,
            ),
        ] {
            assert!(
                matches!(
                    classify_auto_mode_tool_input(
                        "Bash",
                        &serde_json::json!({ "command": command })
                    ),
                    AutoModeInputDisposition::HardSensitive(_)
                ),
                "standard auto mode must remain fail-closed for {command:?}"
            );
            let disposition = classify_auto_mode_tool_input_for_continuity(
                "Bash",
                &serde_json::json!({ "command": command }),
            );
            let AutoModeInputDisposition::EmbeddedScriptReview(script) = disposition else {
                panic!("expected continuity review for {command:?}, got {disposition:?}");
            };
            assert_eq!(script.interpreter, expected);
        }
    }

    #[test]
    fn continuity_mode_keeps_destructive_dynamic_and_file_scripts_hard_sensitive() {
        for command in [
            "bash <<'SH'\nrm -rf target/cache\nSH",
            "python $SCRIPT",
            "python scripts/check.py",
            "python scripts/check.py && rm -rf target/cache",
            "pwsh -File scripts/check.ps1",
            "pwsh -EncodedCommand ZABhAG4AZwBlAHIAbwB1AHMA",
        ] {
            assert!(
                matches!(
                    classify_auto_mode_tool_input_for_continuity(
                        "Bash",
                        &serde_json::json!({ "command": command }),
                    ),
                    AutoModeInputDisposition::HardSensitive(_)
                ),
                "expected continuity mode to stay fail-closed for {command:?}"
            );
        }
    }

    #[test]
    fn auto_mode_preserves_existing_non_script_behavior() {
        assert_eq!(
            classify_auto_mode_tool_input("Bash", &serde_json::json!({ "command": "git status" })),
            AutoModeInputDisposition::NoSpecialRisk
        );
        assert_eq!(
            classify_auto_mode_tool_input(
                "Bash",
                &serde_json::json!({ "command": "python cleanup.py" })
            ),
            AutoModeInputDisposition::HardSensitive(HardSensitiveReason::ScriptFileExecution)
        );
    }

    #[test]
    fn hard_sensitive_reasons_map_to_their_trigger_category() {
        let classify = |command: &str| {
            classify_auto_mode_tool_input("Bash", &serde_json::json!({ "command": command }))
        };
        assert_eq!(
            classify("rm -rf build"),
            AutoModeInputDisposition::HardSensitive(HardSensitiveReason::FileDeletion)
        );
        assert_eq!(
            classify("git stash drop"),
            AutoModeInputDisposition::HardSensitive(HardSensitiveReason::GitStashMutation)
        );
        assert_eq!(
            classify("python - <<'PY'\nimport os\nos.remove('x')\nPY"),
            AutoModeInputDisposition::HardSensitive(HardSensitiveReason::InterpreterFileDeletion)
        );
        assert_eq!(
            classify("mystery <<'X'\nopaque\nX"),
            AutoModeInputDisposition::HardSensitive(
                HardSensitiveReason::UnanalyzableScriptInvocation
            )
        );
        assert_eq!(
            embedded_interpreter_script_hard_sensitive_effects(&EmbeddedInterpreterScript {
                interpreter: EmbeddedInterpreter::Bash,
                invocation: "bash".into(),
                source: "rm -rf target/cache".into(),
            }),
            Some(HardSensitiveReason::FileDeletion)
        );
    }

    #[test]
    fn auto_mode_sensitive_input_flags_cmd_powershell_and_python_deletions() {
        for (tool_name, command) in [
            ("PowerShell", "cmd.exe /d /c del /f /q old.txt"),
            ("PowerShell", "cmd /c if exist cache rd /s /q cache"),
            ("PowerShell", "Remove-Item -Recurse -Force cache"),
            ("PowerShell", "[IO.Directory]::Delete('cache', $true)"),
            ("PowerShell", "$item = Get-Item old.txt; $item.Delete()"),
            (
                "Bash",
                "python -c \"import shutil; shutil.rmtree('cache')\"",
            ),
            ("Bash", "python cleanup.py"),
            ("PowerShell", "pwsh -File cleanup.ps1"),
        ] {
            assert!(
                is_auto_mode_sensitive_tool_input(
                    tool_name,
                    &serde_json::json!({ "command": command })
                ),
                "expected auto mode to require confirmation for {command:?}"
            );
        }
    }

    #[test]
    fn auto_mode_sensitive_input_flags_sensitive_shell_commands() {
        assert!(is_auto_mode_sensitive_tool_input(
            "Bash",
            &serde_json::json!({ "command": "rm old.log" })
        ));
        assert!(is_auto_mode_sensitive_tool_input(
            "Bash",
            &serde_json::json!({ "command": "git checkout -- src/lib.rs" })
        ));
        assert!(is_auto_mode_sensitive_tool_input(
            "Bash",
            &serde_json::json!({ "command": "git reset --hard HEAD" })
        ));
        assert!(is_auto_mode_sensitive_tool_input(
            "Bash",
            &serde_json::json!({ "command": "git stash pop" })
        ));
        assert!(is_auto_mode_sensitive_tool_input(
            "Bash",
            &serde_json::json!({ "command": "git show HEAD:src/lib.rs > src/lib.rs" })
        ));
        assert!(is_auto_mode_sensitive_tool_input(
            "PowerShell",
            &serde_json::json!({
                "command": "python -c \"import shutil; shutil.rmtree('cache')\""
            })
        ));
        // A plain copy is an edit, not destruction — no prompt.
        assert!(!is_auto_mode_sensitive_tool_input(
            "PowerShell",
            &serde_json::json!({
                "command": "python -c \"import shutil; shutil.copyfile('a.cs','b.cs')\""
            })
        ));
        assert!(is_auto_mode_sensitive_tool_input(
            "PowerShell",
            &serde_json::json!({
                "command": "$lines = & git show HEAD:src/tui/mod.rs; $text = ($lines -join \"`n\") + \"`n\"; [System.IO.File]::WriteAllText(\"C:/projects/example/src/tui/mod.rs\", $text, [System.Text.UTF8Encoding]::new($false))"
            })
        ));
        assert!(is_auto_mode_sensitive_tool_input(
            "PowerShell",
            &serde_json::json!({
                "command": "cmd.exe /d /c \"git show HEAD:src/tui/mod.rs > C:\\projects\\example\\src\\tui\\mod.rs\""
            })
        ));
        assert!(!is_auto_mode_sensitive_tool_input(
            "Bash",
            &serde_json::json!({
                "command": "python -c \"import json; json.load(open('x.json'))\""
            })
        ));
        assert!(!is_auto_mode_sensitive_tool_input(
            "Bash",
            &serde_json::json!({ "command": "git show HEAD:src/lib.rs" })
        ));
        assert!(!is_auto_mode_sensitive_tool_input(
            "Write",
            &serde_json::json!({ "file_path": "src/lib.rs", "content": "" })
        ));
        assert!(!is_auto_mode_sensitive_tool_input(
            "Edit",
            &serde_json::json!({
                "file_path": "src/lib.rs",
                "old_string": "line one\nline two\nline three\n",
                "new_string": ""
            })
        ));
    }
}
