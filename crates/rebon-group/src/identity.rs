//! Which session a group server belongs to.
//!
//! An agent starts its MCP servers as child processes, and most agents put
//! their session id in the environment those children inherit. That is the
//! first answer and usually the only one needed (RFC-0009 §5):
//!
//! - Rebon sets `REBON_SESSION_ID` when a session starts or resumes.
//! - Claude Code sets `CLAUDE_CODE_SESSION_ID` (and `CLAUDE_PID`, its pid).
//! - Grok Build sets `GROK_SESSION_ID`.
//!
//! Codex scrubs its servers' environment instead and names the thread on
//! each call ([`from_call_meta`]). DeepSeek Harness strips every `DSH_*`
//! variable from its MCP children and sends nothing on the call, so a dsh
//! session says who it is on `group_join`.
//!
//! A process can carry more than one of them: Rebon started from Claude
//! Code's shell inherits Claude Code's variables and adds its own, and the
//! other way round. The server's parent process decides then — it is the
//! agent that started this server. When that cannot be told either, the
//! agent says who it is by passing its session id to `group_join`.
//!
//! One known gap: a server started before the agent switched sessions
//! (`/clear`, a resume inside the same process) still carries the old id.

use std::collections::HashMap;

/// The agent programs a member can be. Open-ended: an agent this table does
/// not know joins under the name it gives.
pub struct AgentKind;

impl AgentKind {
    pub const REBON: &'static str = "rebon";
    pub const CLAUDE_CODE: &'static str = "claude-code";
    pub const CODEX: &'static str = "codex";
    pub const GROK: &'static str = "grok";
    pub const OPENCODE: &'static str = "opencode";
    /// DeepSeek Harness.
    pub const DSH: &'static str = "dsh";
}

/// The session a server serves: which program, and its session id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    pub agent: String,
    pub session_id: String,
}

impl Caller {
    pub fn key(&self) -> crate::model::MemberKey {
        crate::model::MemberKey {
            agent: self.agent.clone(),
            session_id: self.session_id.clone(),
        }
    }
}

/// An environment variable that names the session of the agent that set it.
struct SessionVar {
    agent: &'static str,
    var: &'static str,
    /// Lowercase fragments of the agent's executable name, for telling it
    /// apart from another agent in the ancestry.
    image: &'static [&'static str],
}

/// The agents whose session variable is known. Order breaks a tie nothing
/// else can: the first listed wins.
const SESSION_VARS: &[SessionVar] = &[
    SessionVar {
        agent: AgentKind::REBON,
        var: "REBON_SESSION_ID",
        image: &["rebon"],
    },
    SessionVar {
        agent: AgentKind::CLAUDE_CODE,
        var: "CLAUDE_CODE_SESSION_ID",
        image: &["claude"],
    },
    // Grok Build gives its stdio MCP servers the session id and strips a
    // spoofed one (xai-grok-mcp `servers.rs`).
    SessionVar {
        agent: AgentKind::GROK,
        var: "GROK_SESSION_ID",
        image: &["grok"],
    },
];

/// The session a tool call says it comes from, when the agent puts it on the
/// call rather than in the environment. Codex scrubs its MCP servers'
/// environment but sends `_meta["x-codex-turn-metadata"]` with the thread
/// on every `tools/call` (codex-rs `core/src/mcp_tool_call.rs`).
pub fn from_call_meta(meta: &serde_json::Value) -> Option<Caller> {
    let codex = meta.get("x-codex-turn-metadata")?;
    let id = ["thread_id", "session_id"]
        .iter()
        .find_map(|key| codex.get(*key).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    Some(Caller {
        agent: AgentKind::CODEX.to_string(),
        session_id: id.to_string(),
    })
}

/// The process that started this one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parent {
    pub pid: u32,
    /// The executable's file name, e.g. `claude.exe`.
    pub image: String,
}

/// Who this process serves, from its own environment and parent.
pub fn detect() -> Option<Caller> {
    let env: HashMap<String, String> = std::env::vars().collect();
    resolve(&env, parent_process().as_ref())
}

/// [`detect`] over a given environment and parent, for tests and for hosts
/// that know better than the process environment.
pub fn resolve(env: &HashMap<String, String>, parent: Option<&Parent>) -> Option<Caller> {
    let present: Vec<(&SessionVar, &str)> = SESSION_VARS
        .iter()
        .filter_map(|known| {
            env.get(known.var)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(|value| (known, value))
        })
        .collect();
    let pick = match present.as_slice() {
        [] => return None,
        [only] => *only,
        several => parent
            .and_then(|parent| by_parent(several, parent, env))
            .unwrap_or(several[0]),
    };
    Some(Caller {
        agent: pick.0.agent.to_string(),
        session_id: pick.1.to_string(),
    })
}

/// The candidate whose program is the parent: by executable name, else — for
/// Claude Code, which records its pid — by pid.
fn by_parent<'a>(
    candidates: &[(&'a SessionVar, &'a str)],
    parent: &Parent,
    env: &HashMap<String, String>,
) -> Option<(&'a SessionVar, &'a str)> {
    let image = parent.image.to_lowercase();
    candidates
        .iter()
        .find(|(known, _)| known.image.iter().any(|fragment| image.contains(fragment)))
        .or_else(|| {
            let claude_pid = env.get("CLAUDE_PID")?.trim().parse::<u32>().ok()?;
            (claude_pid == parent.pid)
                .then(|| {
                    candidates
                        .iter()
                        .find(|(known, _)| known.agent == AgentKind::CLAUDE_CODE)
                })
                .flatten()
        })
        .copied()
}

/// This process's parent, when the platform will say.
pub fn parent_process() -> Option<Parent> {
    platform::parent_process()
}

#[cfg(unix)]
mod platform {
    use super::Parent;

    pub(super) fn parent_process() -> Option<Parent> {
        let pid = std::os::unix::process::parent_id();
        let image = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|name| name.trim().to_string())
            .unwrap_or_default();
        Some(Parent { pid, image })
    }
}

#[cfg(windows)]
mod platform {
    use super::Parent;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    /// Walks a process snapshot twice over: once for this process's entry,
    /// which names the parent, and the parent's, which names its image.
    pub(super) fn parent_process() -> Option<Parent> {
        let processes = snapshot()?;
        let me = std::process::id();
        let parent_pid = processes.iter().find(|(pid, _, _)| *pid == me)?.1;
        let image = processes
            .iter()
            .find(|(pid, _, _)| *pid == parent_pid)
            .map(|(_, _, image)| image.clone())
            .unwrap_or_default();
        Some(Parent {
            pid: parent_pid,
            image,
        })
    }

    /// (pid, parent pid, image name) for every process.
    fn snapshot() -> Option<Vec<(u32, u32, String)>> {
        // SAFETY: a process snapshot takes no pointers; the handle is checked
        // and closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: PROCESSENTRY32W is plain data; dwSize tells the API how
        // much of it may be written, and the handle stays open for the walk.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut processes = Vec::new();
        let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while has_entry {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            processes.push((
                entry.th32ProcessID,
                entry.th32ParentProcessID,
                String::from_utf16_lossy(&entry.szExeFile[..len]),
            ));
            has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        // SAFETY: the handle came from CreateToolhelp32Snapshot above.
        unsafe {
            CloseHandle(snapshot);
        }
        Some(processes)
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    pub(super) fn parent_process() -> Option<super::Parent> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn parent(pid: u32, image: &str) -> Parent {
        Parent {
            pid,
            image: image.to_string(),
        }
    }

    #[test]
    fn one_session_variable_names_the_caller() {
        let caller = resolve(&env(&[("CLAUDE_CODE_SESSION_ID", "aa38901a")]), None).unwrap();
        assert_eq!(caller.agent, AgentKind::CLAUDE_CODE);
        assert_eq!(caller.session_id, "aa38901a");
        let caller = resolve(&env(&[("REBON_SESSION_ID", "k7m2q-4xr9t")]), None).unwrap();
        assert_eq!(caller.agent, AgentKind::REBON);
    }

    #[test]
    fn no_session_variable_is_no_caller() {
        assert_eq!(resolve(&env(&[("PATH", "/bin")]), None), None);
        assert_eq!(resolve(&env(&[("REBON_SESSION_ID", "  ")]), None), None);
    }

    /// Claude Code started from Rebon's shell inherits Rebon's variable; its
    /// MCP server's parent is Claude Code, so Claude Code it is.
    #[test]
    fn the_parent_decides_between_inherited_variables() {
        let both = env(&[
            ("REBON_SESSION_ID", "k7m2q-4xr9t"),
            ("CLAUDE_CODE_SESSION_ID", "aa38901a"),
        ]);
        let caller = resolve(&both, Some(&parent(10, "claude.exe"))).unwrap();
        assert_eq!(caller.agent, AgentKind::CLAUDE_CODE);
        let caller = resolve(&both, Some(&parent(10, "rebon-cli.exe"))).unwrap();
        assert_eq!(caller.agent, AgentKind::REBON);
    }

    /// Claude Code installed through npm runs as `node`: its pid, which it
    /// records in CLAUDE_PID, still identifies it.
    #[test]
    fn claude_codes_pid_identifies_it_when_its_image_does_not() {
        let both = env(&[
            ("REBON_SESSION_ID", "k7m2q-4xr9t"),
            ("CLAUDE_CODE_SESSION_ID", "aa38901a"),
            ("CLAUDE_PID", "4242"),
        ]);
        let caller = resolve(&both, Some(&parent(4242, "node.exe"))).unwrap();
        assert_eq!(caller.agent, AgentKind::CLAUDE_CODE);
        // Neither the image nor the pid: the table's order.
        let caller = resolve(&both, Some(&parent(7, "node.exe"))).unwrap();
        assert_eq!(caller.agent, AgentKind::REBON);
    }

    #[test]
    fn grok_builds_variable_names_a_grok_session() {
        let caller = resolve(&env(&[("GROK_SESSION_ID", "g-1")]), None).unwrap();
        assert_eq!(caller.agent, AgentKind::GROK);
    }

    #[test]
    fn codex_names_its_thread_on_the_call() {
        let meta = serde_json::json!({
            "x-codex-turn-metadata": { "session_id": "s-9", "thread_id": "019a-thread", "turn_id": "t" }
        });
        let caller = from_call_meta(&meta).unwrap();
        assert_eq!(caller.agent, AgentKind::CODEX);
        assert_eq!(caller.session_id, "019a-thread");
        assert_eq!(
            from_call_meta(&serde_json::json!({ "progressToken": 1 })),
            None
        );
    }

    #[test]
    fn this_process_has_a_parent() {
        assert!(parent_process().is_some_and(|parent| parent.pid != 0));
    }
}
