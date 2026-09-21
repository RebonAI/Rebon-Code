//! `HookCommand` — the hook transports as one enum, plus [`display_text`]
//! and [`is_hook_equal`] semantics.
//!
//! ## The variants
//!
//! Each variant is a flat struct holding that transport's fields:
//!
//! * [`BashCommandHook`] — `command`, `if`, `shell`, `timeout`,
//!   `status_message`, `once`, `async`, `async_rewake`.
//! * [`PromptHook`] — `prompt`, `if`, `timeout`, `model`,
//!   `status_message`, `once`.
//! * [`AgentHook`] — the same fields, with `prompt` as the task the
//!   subagent runs.
//! * [`HttpHook`] — `url`, `if`, `timeout`, `headers`,
//!   `allowed_env_vars`, `status_message`, `once`.
//!
//! ## `display_text`
//!
//! A non-empty `status_message` wins; otherwise the variant's own
//! string is used — `command`, `prompt` (for both the `prompt` and
//! `agent` transports), or `url`.
//!
//! ## `is_hook_equal`
//!
//! Compares only the identity fields: the command / prompt / url string,
//! the `if` predicate, and — on `command` hooks only — the shell. Two
//! hooks of different transports are never equal.
//!
//! Load-bearing invariants:
//!
//! 1. **A non-empty `status_message` overrides display text.**
//!    An empty one does not.
//! 2. **`shell` defaults to [`DEFAULT_HOOK_SHELL`]**
//!    ([`ShellKind::Bash`]). Two `command` hooks with the same string
//!    but different shells are distinct hooks.
//! 3. **`if` defaults to the empty string.** `None` and `Some("")`
//!    compare equal in the `if` dimension.
//! 4. **`timeout` is NOT part of identity**, and neither are `model`,
//!    `status_message`, `once`, `async`, `async_rewake`, `headers` or
//!    `allowed_env_vars`.
//!
//! ## Numeric type for `timeout`
//!
//! `timeout` is `u64` (seconds) instead of a float so the structs
//! can derive `Eq`/`Hash` cleanly, and because every consumer of the
//! timeout today treats it as an integer second count. Fractional
//! timeouts are not observably used. If a future consumer needs
//! sub-second precision, swap `u64` for an explicit ordered float
//! wrapper — `is_hook_equal` does NOT read `timeout`, so the change
//! would be local.
//!
//! ## What this module does NOT do
//!
//! Parsing JSON into a `HookCommand` is the **settings-store's**
//! job. This module owns the in-memory shape and the `display_text`
//! / `is_hook_equal` predicates only.

/// The shell a `command` hook runs under when it names none.
pub const DEFAULT_HOOK_SHELL: ShellKind = ShellKind::Bash;

/// Shell interpreter for command hooks.
///
/// `Node` is a rebon extension — it lets a command hook carry inline
/// JavaScript that the executor hands to `node -e`. The command
/// executor promotes it to a first-class variant so it knows which
/// interpreter to spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellKind {
    Bash,
    Powershell,
    Node,
}

impl ShellKind {
    /// OS-level interpreter name. Used by the command executor to
    /// pick the `Command::new(...)` binary.
    pub const fn interpreter(self) -> &'static str {
        match self {
            ShellKind::Bash => "bash",
            ShellKind::Powershell => "powershell",
            ShellKind::Node => "node",
        }
    }
}

/// `command`-type hook. Field order kept stable for serde
/// compatibility with the settings-store representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashCommandHook {
    pub command: String,
    /// Permission-rule syntax filter. `None` and `Some("")` are
    /// treated as equal by [`is_hook_equal`].
    pub r#if: Option<String>,
    /// Shell interpreter. `None` defaults to [`DEFAULT_HOOK_SHELL`].
    pub shell: Option<ShellKind>,
    /// Per-command timeout in seconds. NOT part of identity.
    pub timeout: Option<u64>,
    pub status_message: Option<String>,
    pub once: Option<bool>,
    pub r#async: Option<bool>,
    pub async_rewake: Option<bool>,
}

/// `prompt`-type hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHook {
    pub prompt: String,
    pub r#if: Option<String>,
    pub timeout: Option<u64>,
    pub model: Option<String>,
    pub status_message: Option<String>,
    pub once: Option<bool>,
}

/// An `agent` hook: `prompt` is the task the subagent runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHook {
    pub prompt: String,
    pub r#if: Option<String>,
    pub timeout: Option<u64>,
    pub model: Option<String>,
    pub status_message: Option<String>,
    pub once: Option<bool>,
}

/// An `http` hook: the request goes to `url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpHook {
    pub url: String,
    pub r#if: Option<String>,
    pub timeout: Option<u64>,
    pub headers: Option<Vec<(String, String)>>,
    pub allowed_env_vars: Option<Vec<String>>,
    pub status_message: Option<String>,
    pub once: Option<bool>,
}

/// One variant per `type` literal that can be persisted in
/// `settings.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookCommand {
    Command(BashCommandHook),
    Prompt(PromptHook),
    Agent(AgentHook),
    Http(HttpHook),
}

impl HookCommand {
    /// The literal `type` discriminant string, in the casing the
    /// settings shape uses.
    pub const fn type_str(&self) -> &'static str {
        match self {
            HookCommand::Command(_) => "command",
            HookCommand::Prompt(_) => "prompt",
            HookCommand::Agent(_) => "agent",
            HookCommand::Http(_) => "http",
        }
    }

    /// The status line a hook shows while it runs, whichever transport
    /// it is: the first `status_message` that is present and non-empty.
    pub fn status_message(&self) -> Option<&str> {
        let s = match self {
            HookCommand::Command(h) => h.status_message.as_deref(),
            HookCommand::Prompt(h) => h.status_message.as_deref(),
            HookCommand::Agent(h) => h.status_message.as_deref(),
            HookCommand::Http(h) => h.status_message.as_deref(),
        };
        // An empty string counts as absent.
        match s {
            Some(v) if !v.is_empty() => Some(v),
            _ => None,
        }
    }
}

/// The one-line text shown for a hook: its `status_message` when that
/// is non-empty, otherwise the command, prompt, or url.
///
/// There are no `callback` or `function` arms — those transports cannot
/// be persisted to settings.json, so this path never receives them.
pub fn display_text(hook: &HookCommand) -> &str {
    if let Some(msg) = hook.status_message() {
        return msg;
    }
    match hook {
        HookCommand::Command(h) => &h.command,
        HookCommand::Prompt(h) => &h.prompt,
        HookCommand::Agent(h) => &h.prompt,
        HookCommand::Http(h) => &h.url,
    }
}

/// Whether two hooks are the same hook: same transport, same
/// command/prompt/url content, same `if` predicate, and — on `command`
/// hooks only — the same shell.
///
/// NOT a full structural equality — `timeout`, `model`,
/// `status_message`, `once`, `async`, `async_rewake`, `headers`, and
/// `allowed_env_vars` are intentionally ignored.
pub fn is_hook_equal(a: &HookCommand, b: &HookCommand) -> bool {
    fn same_if<A: AsRef<str>, B: AsRef<str>>(a: Option<A>, b: Option<B>) -> bool {
        a.as_ref().map(AsRef::as_ref).unwrap_or("") == b.as_ref().map(AsRef::as_ref).unwrap_or("")
    }
    match (a, b) {
        (HookCommand::Command(x), HookCommand::Command(y)) => {
            x.command == y.command
                && x.shell.unwrap_or(DEFAULT_HOOK_SHELL) == y.shell.unwrap_or(DEFAULT_HOOK_SHELL)
                && same_if(x.r#if.as_ref(), y.r#if.as_ref())
        }
        (HookCommand::Prompt(x), HookCommand::Prompt(y)) => {
            x.prompt == y.prompt && same_if(x.r#if.as_ref(), y.r#if.as_ref())
        }
        (HookCommand::Agent(x), HookCommand::Agent(y)) => {
            x.prompt == y.prompt && same_if(x.r#if.as_ref(), y.r#if.as_ref())
        }
        (HookCommand::Http(x), HookCommand::Http(y)) => {
            x.url == y.url && same_if(x.r#if.as_ref(), y.r#if.as_ref())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(command: &str) -> HookCommand {
        HookCommand::Command(BashCommandHook {
            command: command.into(),
            r#if: None,
            shell: None,
            timeout: None,
            status_message: None,
            once: None,
            r#async: None,
            async_rewake: None,
        })
    }

    fn cmd_with(command: &str, r#if: Option<&str>, shell: Option<ShellKind>) -> HookCommand {
        HookCommand::Command(BashCommandHook {
            command: command.into(),
            r#if: r#if.map(Into::into),
            shell,
            timeout: None,
            status_message: None,
            once: None,
            r#async: None,
            async_rewake: None,
        })
    }

    fn prompt(prompt: &str) -> HookCommand {
        HookCommand::Prompt(PromptHook {
            prompt: prompt.into(),
            r#if: None,
            timeout: None,
            model: None,
            status_message: None,
            once: None,
        })
    }

    fn agent(prompt: &str) -> HookCommand {
        HookCommand::Agent(AgentHook {
            prompt: prompt.into(),
            r#if: None,
            timeout: None,
            model: None,
            status_message: None,
            once: None,
        })
    }

    fn http(url: &str) -> HookCommand {
        HookCommand::Http(HttpHook {
            url: url.into(),
            r#if: None,
            timeout: None,
            headers: None,
            allowed_env_vars: None,
            status_message: None,
            once: None,
        })
    }

    #[test]
    fn type_str_values_are_pinned() {
        assert_eq!(cmd("ls").type_str(), "command");
        assert_eq!(prompt("hello").type_str(), "prompt");
        assert_eq!(agent("verify").type_str(), "agent");
        assert_eq!(http("https://x").type_str(), "http");
    }

    #[test]
    fn display_text_command_default() {
        assert_eq!(display_text(&cmd("ls -la")), "ls -la");
    }

    #[test]
    fn display_text_prompt_default() {
        assert_eq!(display_text(&prompt("Summarize")), "Summarize");
    }

    #[test]
    fn display_text_agent_default() {
        assert_eq!(display_text(&agent("Verify tests")), "Verify tests");
    }

    #[test]
    fn display_text_http_default() {
        assert_eq!(
            display_text(&http("https://example.com")),
            "https://example.com"
        );
    }

    #[test]
    fn display_text_status_message_overrides_command() {
        let mut h = cmd("ls -la");
        if let HookCommand::Command(c) = &mut h {
            c.status_message = Some("Linting…".into());
        }
        assert_eq!(display_text(&h), "Linting…");
    }

    #[test]
    fn display_text_status_message_overrides_prompt() {
        let mut h = prompt("Summarize");
        if let HookCommand::Prompt(p) = &mut h {
            p.status_message = Some("Thinking…".into());
        }
        assert_eq!(display_text(&h), "Thinking…");
    }

    #[test]
    fn display_text_empty_status_message_does_not_override() {
        // An empty string counts as absent, so the underlying command
        // wins.
        let mut h = cmd("ls");
        if let HookCommand::Command(c) = &mut h {
            c.status_message = Some(String::new());
        }
        assert_eq!(display_text(&h), "ls");
    }

    #[test]
    fn is_hook_equal_same_command() {
        assert!(is_hook_equal(&cmd("ls"), &cmd("ls")));
    }

    #[test]
    fn is_hook_equal_different_command_string() {
        assert!(!is_hook_equal(&cmd("ls"), &cmd("ls -la")));
    }

    #[test]
    fn is_hook_equal_command_vs_prompt_is_false() {
        assert!(!is_hook_equal(&cmd("ls"), &prompt("ls")));
    }

    #[test]
    fn is_hook_equal_command_default_shell_vs_explicit_bash() {
        let a = cmd_with("ls", None, None);
        let b = cmd_with("ls", None, Some(ShellKind::Bash));
        assert!(is_hook_equal(&a, &b));
    }

    #[test]
    fn is_hook_equal_command_bash_vs_powershell_is_false() {
        let a = cmd_with("ls", None, Some(ShellKind::Bash));
        let b = cmd_with("ls", None, Some(ShellKind::Powershell));
        assert!(!is_hook_equal(&a, &b));
    }

    #[test]
    fn is_hook_equal_if_none_vs_empty_string() {
        // The `if` predicate is compared with an empty-string fallback,
        // so `None` matches `Some("")`.
        let a = cmd_with("ls", None, None);
        let b = cmd_with("ls", Some(""), None);
        assert!(is_hook_equal(&a, &b));
    }

    #[test]
    fn is_hook_equal_if_distinct_strings_are_distinct() {
        let a = cmd_with("setup.sh", Some("Bash(git *)"), None);
        let b = cmd_with("setup.sh", Some("Bash(npm *)"), None);
        assert!(!is_hook_equal(&a, &b));
    }

    #[test]
    fn is_hook_equal_prompt_same() {
        assert!(is_hook_equal(&prompt("Hi"), &prompt("Hi")));
    }

    #[test]
    fn is_hook_equal_prompt_different() {
        assert!(!is_hook_equal(&prompt("Hi"), &prompt("Hello")));
    }

    #[test]
    fn is_hook_equal_agent_same() {
        assert!(is_hook_equal(&agent("V"), &agent("V")));
    }

    #[test]
    fn is_hook_equal_agent_different() {
        assert!(!is_hook_equal(&agent("V"), &agent("V2")));
    }

    #[test]
    fn is_hook_equal_http_same() {
        assert!(is_hook_equal(&http("https://x"), &http("https://x")));
    }

    #[test]
    fn is_hook_equal_http_different() {
        assert!(!is_hook_equal(&http("https://x"), &http("https://y")));
    }

    #[test]
    fn is_hook_equal_ignores_timeout() {
        // `timeout` is NOT part of identity; only the command string,
        // the `if` predicate and the shell are compared.
        let mut a = cmd("ls");
        let mut b = cmd("ls");
        if let HookCommand::Command(c) = &mut a {
            c.timeout = Some(5);
        }
        if let HookCommand::Command(c) = &mut b {
            c.timeout = Some(60);
        }
        assert!(is_hook_equal(&a, &b));
    }

    #[test]
    fn is_hook_equal_ignores_status_message() {
        let mut a = cmd("ls");
        let mut b = cmd("ls");
        if let HookCommand::Command(c) = &mut a {
            c.status_message = Some("A".into());
        }
        if let HookCommand::Command(c) = &mut b {
            c.status_message = Some("B".into());
        }
        assert!(is_hook_equal(&a, &b));
    }

    #[test]
    fn default_hook_shell_is_bash() {
        // The default is pinned by the settings shape; preserve it.
        assert_eq!(DEFAULT_HOOK_SHELL, ShellKind::Bash);
    }

    /// Table of `display_text` results across all four transports,
    /// covering the `status_message` priority and the empty-string edge
    /// case.
    #[test]
    fn display_text_table() {
        let cases: Vec<(HookCommand, &str)> = vec![
            (cmd("ls"), "ls"),
            (prompt("Summarize"), "Summarize"),
            (agent("Verify"), "Verify"),
            (http("https://api"), "https://api"),
            (
                {
                    let mut h = cmd("ls");
                    if let HookCommand::Command(c) = &mut h {
                        c.status_message = Some("Status".into());
                    }
                    h
                },
                "Status",
            ),
            (
                {
                    let mut h = http("https://api");
                    if let HookCommand::Http(c) = &mut h {
                        c.status_message = Some("Pinging…".into());
                    }
                    h
                },
                "Pinging…",
            ),
        ];
        for (hook, expected) in cases {
            assert_eq!(display_text(&hook), expected);
        }
    }
}
