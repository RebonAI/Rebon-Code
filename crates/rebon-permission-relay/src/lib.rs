//! An external agent CLI's permission request, asked in the Rebon app that
//! started the CLI.
//!
//! Claude Code and Codex both run a `PermissionRequest` hook where they would
//! otherwise put a permission dialog on their own screen: the hook gets the
//! request as JSON on stdin and may answer allow or deny on stdout, or say
//! nothing and leave the dialog to the CLI. When the app runs one of them
//! with its chat on screen instead of the terminal, it adds that hook for
//! the run (`rebon permission hook --agent <agent>`), and the hook hands the
//! request to the app and waits for the user's answer:
//!
//! ```text
//! claude / codex ──PermissionRequest──▶ rebon permission hook
//!                                            │ <id>.request.json
//!                                            ▼
//!                                  the app's inbox (INBOX_ENV)
//!                                            │ <id>.answer.json
//!                                            ▼
//!                  allow / deny on stdout, or nothing: the CLI's own dialog
//! ```
//!
//! The inbox is a directory the app names in the CLI's environment, along
//! with which conversation the CLI is ([`OWNER_ENV`]) and the app's process
//! ([`HOST_PID_ENV`]); the hook inherits all three. Each file is written
//! whole and renamed into place, so neither side reads half of one.
//!
//! Every way this can fall short ends in the CLI's own dialog, never in an
//! answer nobody gave: no inbox in the environment, an app that has gone, a
//! wait past [`HOOK_TIMEOUT_SECS`], an answer of [`Answer::Defer`] (the user
//! is looking at the terminal, where the dialog belongs), or anything that
//! does not parse.
//!
//! Both halves are here because one writes what the other reads: the hook
//! ([`run_hook`]) and the app ([`pending`], [`answer`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The directory requests go to and answers come from.
pub const INBOX_ENV: &str = "REBON_PERMISSION_INBOX";

/// The app's name for the conversation the CLI is running.
pub const OWNER_ENV: &str = "REBON_PERMISSION_OWNER";

/// The app's process id: a hook whose app has gone stops waiting for it.
pub const HOST_PID_ENV: &str = "REBON_PERMISSION_HOST_PID";

/// How long the CLI gives the hook, as the app configures it. A user may take
/// a while to come back to a question, but not forever.
pub const HOOK_TIMEOUT_SECS: u64 = 3600;

/// How long the hook waits for an answer: short of [`HOOK_TIMEOUT_SECS`], so
/// it gives up — and the CLI asks on its own screen — before the CLI gives
/// up on it.
const WAIT_FOR_ANSWER: Duration = Duration::from_secs(HOOK_TIMEOUT_SECS - 60);

/// How often the hook looks for its answer.
const POLL: Duration = Duration::from_millis(100);

/// How often the hook checks the app is still there.
const HOST_CHECK: Duration = Duration::from_secs(1);

const REQUEST_SUFFIX: &str = ".request.json";
const ANSWER_SUFFIX: &str = ".answer.json";

/// What the user is told a denial was, by way of the model.
const DENIED: &str = "The user declined this in Rebon.";

/// The CLIs whose hook this relays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    ClaudeCode,
    Codex,
}

impl Agent {
    /// The agent `rebon permission hook --agent <name>` names.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "claude-code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }
}

/// A permission request, as the app reads it from the inbox.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    /// The app's conversation ([`OWNER_ENV`]).
    pub owner: String,
    pub agent: Agent,
    pub tool_name: String,
    pub tool_input: Value,
    /// Whether "always allow" can be answered: the CLI proposed rules to keep
    /// and takes them back in the answer. Claude Code does, when it has any;
    /// Codex refuses a whole answer that carries them.
    pub can_remember: bool,
}

/// The user's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Answer {
    Allow,
    /// Allow, and keep the rules the CLI proposed so it does not ask again.
    AllowAlways,
    Deny,
    /// Leave it to the CLI's own dialog.
    Defer,
}

/// The requests in `inbox` that have no answer yet, oldest name first.
/// Files that do not parse are skipped: they are the hook's to clean up.
pub fn pending(inbox: &Path) -> Vec<Request> {
    let Ok(entries) = std::fs::read_dir(inbox) else {
        return Vec::new();
    };
    let mut requests: Vec<Request> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let id = name.to_str()?.strip_suffix(REQUEST_SUFFIX)?.to_string();
            if answer_path(inbox, &id).exists() {
                return None;
            }
            let bytes = std::fs::read(entry.path()).ok()?;
            serde_json::from_slice::<Request>(&bytes)
                .ok()
                .filter(|request| request.id == id)
        })
        .collect();
    requests.sort_by(|left, right| left.id.cmp(&right.id));
    requests
}

/// Answers request `id`. The hook picks it up, removes both files, and
/// tells the CLI.
pub fn answer(inbox: &Path, id: &str, answer: Answer) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(&answer).expect("an answer serializes");
    rebon_session::write_file_atomically(&answer_path(inbox, id), &bytes)
}

fn request_path(inbox: &Path, id: &str) -> PathBuf {
    inbox.join(format!("{id}{REQUEST_SUFFIX}"))
}

fn answer_path(inbox: &Path, id: &str) -> PathBuf {
    inbox.join(format!("{id}{ANSWER_SUFFIX}"))
}

/// The hook's whole run: `input` is what the CLI wrote on stdin, `env` reads
/// the environment, `host_alive` whether a process is still running. Returns
/// what to print, or `None` to print nothing and let the CLI ask.
pub fn run_hook(
    agent: &str,
    input: &str,
    env: impl Fn(&str) -> Option<String>,
    host_alive: impl Fn(u32) -> bool,
) -> Option<Value> {
    let agent = Agent::from_name(agent)?;
    let input: Value = serde_json::from_str(input).ok()?;
    let inbox = PathBuf::from(env(INBOX_ENV).filter(|dir| !dir.is_empty())?);
    let owner = env(OWNER_ENV).filter(|owner| !owner.is_empty())?;
    let host: u32 = env(HOST_PID_ENV)?.parse().ok()?;
    let request = request_for(agent, owner, &input)?;
    let answer = ask(
        &inbox,
        &request,
        &Wait {
            timeout: WAIT_FOR_ANSWER,
            poll: POLL,
            host_alive: &|| host_alive(host),
        },
    )?;
    hook_output(agent, &input, answer)
}

/// The request the hook's `input` makes, named by this process: two hooks
/// alive at once never share a process id.
fn request_for(agent: Agent, owner: String, input: &Value) -> Option<Request> {
    let tool_name = input.get("tool_name")?.as_str()?.to_string();
    let suggestions = input
        .get("permission_suggestions")
        .and_then(Value::as_array)
        .is_some_and(|suggestions| !suggestions.is_empty());
    Some(Request {
        id: format!("hook-{}", std::process::id()),
        owner,
        agent,
        tool_name,
        tool_input: input.get("tool_input").cloned().unwrap_or(Value::Null),
        can_remember: agent == Agent::ClaudeCode && suggestions,
    })
}

/// How the hook waits.
struct Wait<'a> {
    timeout: Duration,
    poll: Duration,
    host_alive: &'a dyn Fn() -> bool,
}

/// Puts `request` in the inbox and waits for its answer. Whatever happens,
/// neither file is left behind; `None` when no answer came.
fn ask(inbox: &Path, request: &Request, wait: &Wait) -> Option<Answer> {
    let request_file = request_path(inbox, &request.id);
    let answer_file = answer_path(inbox, &request.id);
    // An earlier hook with this process id may have died with its answer
    // unread; that answer was not to this question.
    discard(&answer_file);
    let bytes = serde_json::to_vec(request).expect("a request serializes");
    rebon_session::write_file_atomically(&request_file, &bytes).ok()?;

    let started = Instant::now();
    let mut host_checked = started;
    let answer = loop {
        if let Ok(bytes) = std::fs::read(&answer_file) {
            break serde_json::from_slice::<Answer>(&bytes).ok();
        }
        if started.elapsed() >= wait.timeout {
            break None;
        }
        if host_checked.elapsed() >= HOST_CHECK {
            host_checked = Instant::now();
            if !(wait.host_alive)() {
                break None;
            }
        }
        std::thread::sleep(wait.poll);
    };
    discard(&request_file);
    discard(&answer_file);
    answer
}

/// Removes one of this hook's files. A hook may not fail, so one it cannot
/// remove stays: an unanswered request goes when the app's inbox does, and
/// an answer is cleared by the next hook to take this name.
fn discard(path: &Path) {
    std::fs::remove_file(path).ok();
}

/// What the hook prints for `answer`, in the shape both CLIs read:
/// `hookSpecificOutput.decision`. Always-allow hands Claude Code back the
/// rules it proposed; Codex is never offered it, and gets a plain allow.
pub fn hook_output(agent: Agent, input: &Value, answer: Answer) -> Option<Value> {
    let decision = match answer {
        Answer::Defer => return None,
        Answer::Allow => json!({ "behavior": "allow" }),
        Answer::AllowAlways => match (agent, input.get("permission_suggestions")) {
            (Agent::ClaudeCode, Some(Value::Array(rules))) if !rules.is_empty() => {
                json!({ "behavior": "allow", "updatedPermissions": rules })
            }
            _ => json!({ "behavior": "allow" }),
        },
        Answer::Deny => json!({ "behavior": "deny", "message": DENIED }),
    };
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": decision,
        }
    }))
}

#[cfg(test)]
mod tests;
