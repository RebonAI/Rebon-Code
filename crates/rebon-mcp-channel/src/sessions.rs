//! `sessions_list` / `session_read`: the conversations other agents have had
//! in this project on this machine, read-only.
//!
//! Two agents keep their conversations where this surface can find them by a
//! project directory and an id:
//!
//! - Rebon: `<config home>/projects/<project_dir_component(cwd)>/<id>.jsonl`,
//!   titled by its `.meta.json` sidecar or, failing that, its first prompt.
//! - Claude Code: `<CLAUDE_CONFIG_DIR, else ~/.claude>/projects/<encoded
//!   cwd>/<uuid>.jsonl`, titled by the latest `agent-name`, `ai-title` or
//!   `last-prompt` entry near the end of the file.
//!
//! Both write the same transcript shape — a `uuid` / `parentUuid` chain of
//! `user` and `assistant` entries whose `message.content` is a string or a
//! list of text / thinking / tool_use / tool_result blocks — so both are read
//! by `rebon-session`'s one parser and chain walk, and the current chain is
//! what is shown: a rewound or branched conversation reads as the branch its
//! agent is on now.
//!
//! A session is addressed by id and a directory inside this server's root,
//! never by a path, so a client cannot turn a read into a read of an arbitrary
//! file; the ids are checked before they are joined onto anything.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Context;
use rebon_session::session_storage::{
    parse_transcript_jsonl, reconstruct_chain_indices, TranscriptEntry,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::jobs::scoped_cwd;

/// How many conversations `sessions_list` returns when not told, and the
/// most it will. A hundred rows is already more than a model reads closely;
/// past that, the newest ones are what anyone wants.
const DEFAULT_LIST_LIMIT: usize = 20;
const MAX_LIST_LIMIT: usize = 100;

/// `session_read`'s budget when not told, and the range it may be given. A
/// budget is a soft ceiling on the text returned, counted in characters; the
/// top keeps one read well inside what a client will put in a tool result.
const DEFAULT_READ_CHARS: usize = 20_000;
const MIN_READ_CHARS: usize = 1_000;
const MAX_READ_CHARS: usize = 100_000;

/// How much of a Claude Code transcript's end is read for its title. Claude
/// Code appends its title entries again as a conversation goes on, so the
/// latest are always near the end, and a listing must not read every file
/// whole to name it.
const TITLE_TAIL_BYTES: u64 = 64 * 1024;

/// A listed title's length. The fallbacks are prompts, which can be pages.
const TITLE_MAX_CHARS: usize = 100;

/// A tool call's argument and a tool result, as one line each. Enough to
/// tell what was run and how it went; the conversation is the point, not
/// the tool output.
const TOOL_INPUT_MAX_CHARS: usize = 200;
const TOOL_RESULT_MAX_CHARS: usize = 300;

/// What one entry's heading costs against the budget, counted whether or not
/// it is printed. Overcounting keeps the text under the budget.
const HEADING_ALLOWANCE: usize = 16;

/// The longest id accepted. Ids are file names; anything longer is not one.
const MAX_SESSION_ID_LEN: usize = 128;

/// Said when a scoped `cwd` is refused.
const OUTSIDE_REFUSAL: &str = "only sessions inside it can be read";

/// Whose conversation it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Agent {
    Rebon,
    ClaudeCode,
}

impl Agent {
    fn as_str(self) -> &'static str {
        match self {
            Agent::Rebon => "rebon",
            Agent::ClaudeCode => "claude-code",
        }
    }
}

/// Which agents `sessions_list` looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AgentFilter {
    Rebon,
    ClaudeCode,
    All,
}

impl AgentFilter {
    fn includes(self, agent: Agent) -> bool {
        match self {
            AgentFilter::All => true,
            AgentFilter::Rebon => agent == Agent::Rebon,
            AgentFilter::ClaudeCode => agent == Agent::ClaudeCode,
        }
    }
}

// ── requests ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListRequest {
    pub cwd: Option<String>,
    pub agent: Option<AgentFilter>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadRequest {
    pub session_id: String,
    pub agent: Option<Agent>,
    pub cwd: Option<String>,
    pub after: Option<String>,
    pub max_chars: Option<usize>,
}

/// Where the conversations are. The binary fills it from its own config
/// home and environment; tests hand it temp directories.
pub(crate) struct SessionReader {
    /// This server's root, canonical. Every `cwd` is scoped to it.
    root: PathBuf,
    /// Rebon's `<config home>/projects`.
    rebon_projects_root: PathBuf,
    /// Claude Code's config directory, `None` when there is no home
    /// directory to find it under.
    claude_config_dir: Option<PathBuf>,
}

/// One conversation file, before anything has been read out of it.
struct Candidate {
    agent: Agent,
    session_id: String,
    path: PathBuf,
    size_bytes: u64,
    updated_ms: u64,
}

impl SessionReader {
    pub(crate) fn new(
        root: PathBuf,
        rebon_projects_root: PathBuf,
        claude_config_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            root,
            rebon_projects_root,
            claude_config_dir,
        }
    }

    // ── sessions_list ────────────────────────────────────────────────

    /// The project's conversations, newest first.
    ///
    /// Newest by the file's modification time, which is when the agent last
    /// wrote to it. Only as many files are opened as it takes to fill the
    /// page: a title is read per row, and a file with nothing said in it is
    /// passed over rather than counted.
    pub(crate) fn list(&self, request: ListRequest) -> anyhow::Result<Value> {
        let cwd = scoped_cwd(&self.root, request.cwd.as_deref(), OUTSIDE_REFUSAL)?;
        let cwd_text = cwd.to_string_lossy().into_owned();
        let filter = request.agent.unwrap_or(AgentFilter::All);
        let limit = request
            .limit
            .unwrap_or(DEFAULT_LIST_LIMIT)
            .clamp(1, MAX_LIST_LIMIT);

        let mut candidates = Vec::new();
        if filter.includes(Agent::Rebon) {
            candidates.extend(rebon_candidates(&self.rebon_projects_root, &cwd_text));
        }
        if filter.includes(Agent::ClaudeCode) {
            match &self.claude_config_dir {
                Some(config) => candidates.extend(claude_candidates(config, &cwd_text)),
                None if filter == AgentFilter::ClaudeCode => anyhow::bail!(NO_CLAUDE_HOME),
                None => {}
            }
        }
        candidates.sort_by(|left, right| {
            right
                .updated_ms
                .cmp(&left.updated_ms)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });

        let mut sessions = Vec::with_capacity(limit);
        let mut more = false;
        for candidate in candidates {
            let Some(title) = self.title(
                candidate.agent,
                &candidate.session_id,
                &candidate.path,
                &cwd_text,
            ) else {
                continue;
            };
            if sessions.len() == limit {
                more = true;
                break;
            }
            sessions.push(json!({
                "session_id": candidate.session_id,
                "agent": candidate.agent.as_str(),
                "title": title,
                "cwd": cwd_text,
                "updated_at": iso_ms(candidate.updated_ms),
                "size_bytes": candidate.size_bytes,
            }));
        }
        Ok(json!({ "cwd": cwd_text, "sessions": sessions, "more": more }))
    }

    /// A conversation's title, `None` when nothing has been said in it.
    fn title(&self, agent: Agent, session_id: &str, path: &Path, cwd: &str) -> Option<String> {
        let title = match agent {
            Agent::Rebon => rebon_title(&self.rebon_projects_root, cwd, session_id),
            Agent::ClaudeCode => claude_title(path),
        }?;
        Some(rebon_session_host::shorten_excerpt(&title, TITLE_MAX_CHARS))
    }

    // ── session_read ─────────────────────────────────────────────────

    /// A conversation's current chain as text: its latest part that fits,
    /// or — given the cursor a previous read returned — what came after it.
    pub(crate) fn read(&self, request: ReadRequest) -> anyhow::Result<Value> {
        let session_id = request.session_id.trim();
        if !is_valid_session_id(session_id) {
            anyhow::bail!(
                "`{session_id}` is not a session id (letters, digits, `-` and `_` only); \
                 sessions_list shows the ids there are"
            );
        }
        let cwd = scoped_cwd(&self.root, request.cwd.as_deref(), OUTSIDE_REFUSAL)?;
        let cwd_text = cwd.to_string_lossy().into_owned();
        let (agent, path) = self.locate(session_id, request.agent, &cwd_text)?;
        let bytes = std::fs::read(&path)
            .with_context(|| format!("failed to read the {} session", agent.as_str()))?;
        let entries = parse_transcript_jsonl(&bytes);
        let chain = reconstruct_chain_indices(&entries);
        let budget = request
            .max_chars
            .unwrap_or(DEFAULT_READ_CHARS)
            .clamp(MIN_READ_CHARS, MAX_READ_CHARS);
        let after = request
            .after
            .as_deref()
            .map(str::trim)
            .filter(|after| !after.is_empty());
        let page = page(&entries, &chain, after, budget);

        let title = self.title(agent, session_id, &path, &cwd_text);
        let mut result = json!({
            "session_id": session_id,
            "agent": agent.as_str(),
            "cwd": cwd_text,
            "title": title,
            "entries": page.entries,
            "truncated": page.truncated,
            "next_cursor": page.next_cursor,
            "cursor_reset": page.cursor_reset,
            "text": page.text,
        });
        if let Some(note) = page.note {
            result["note"] = json!(note);
        }
        Ok(result)
    }

    /// The file behind `session_id`, and whose it is.
    ///
    /// Without `agent`, a UUID is tried as Claude Code's first (that is the
    /// shape Claude Code names its sessions with) and then as Rebon's;
    /// anything else can only be Rebon's.
    fn locate(
        &self,
        session_id: &str,
        agent: Option<Agent>,
        cwd: &str,
    ) -> anyhow::Result<(Agent, PathBuf)> {
        let rebon = || {
            let path =
                rebon_session::transcript_file_path(&self.rebon_projects_root, cwd, session_id);
            path.is_file().then_some((Agent::Rebon, path))
        };
        let claude = || {
            let config = self.claude_config_dir.as_deref()?;
            let path = claude_project_dir(config, cwd)?.join(format!("{session_id}.jsonl"));
            path.is_file().then_some((Agent::ClaudeCode, path))
        };
        let found = match agent {
            Some(Agent::Rebon) => rebon(),
            Some(Agent::ClaudeCode) => {
                if !is_uuid(session_id) {
                    anyhow::bail!(
                        "`{session_id}` is not a Claude Code session id; Claude Code names \
                         its sessions with UUIDs"
                    );
                }
                if self.claude_config_dir.is_none() {
                    anyhow::bail!(NO_CLAUDE_HOME);
                }
                claude()
            }
            None if is_uuid(session_id) => claude().or_else(rebon),
            None => rebon(),
        };
        found.ok_or_else(|| {
            let whose = match agent {
                Some(Agent::ClaudeCode) => "Claude Code",
                None if is_uuid(session_id) => "Rebon or Claude Code",
                Some(Agent::Rebon) | None => "Rebon",
            };
            anyhow::anyhow!(
                "there is no {whose} session {session_id} in {cwd}; sessions_list shows the \
                 sessions there are"
            )
        })
    }
}

const NO_CLAUDE_HOME: &str = "there is no home directory to find Claude Code's sessions \
under; set CLAUDE_CONFIG_DIR for the server";

/// Claude Code's config directory: `CLAUDE_CONFIG_DIR` when it is set, else
/// `.claude` under the platform home. The binary's reading of the process
/// environment; everything else is handed the answer.
pub(crate) fn claude_config_dir_from_env() -> Option<PathBuf> {
    claude_config_dir_with_env(|name| std::env::var_os(name))
}

pub(crate) fn claude_config_dir_with_env(
    env: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    match env("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.to_string_lossy().trim().is_empty() => Some(PathBuf::from(dir)),
        _ => rebon_session::platform_home_with_env(env).map(|home| home.join(".claude")),
    }
}

// ── listing ──────────────────────────────────────────────────────────

/// Rebon's transcripts for `cwd`. Sidecars (`.meta.json`, `.owner.json`,
/// `.compact.json`, locks) are not `.jsonl` and fall out by extension; a
/// session hidden from Chats — an internal workflow's — is left out as the
/// app's list leaves it out, and an empty file is a session nobody spoke in.
fn rebon_candidates(projects_root: &Path, cwd: &str) -> Vec<Candidate> {
    let dir = rebon_session::project_dir_path(projects_root, cwd);
    jsonl_files(&dir, Agent::Rebon, is_valid_session_id)
        .into_iter()
        .filter(|candidate| {
            !rebon_session::load_session_hidden_from_chats(
                projects_root,
                cwd,
                &candidate.session_id,
            )
        })
        .collect()
}

fn claude_candidates(config: &Path, cwd: &str) -> Vec<Candidate> {
    match claude_project_dir(config, cwd) {
        Some(dir) => jsonl_files(&dir, Agent::ClaudeCode, is_uuid),
        None => Vec::new(),
    }
}

/// The non-empty `<id>.jsonl` files directly in `dir` whose stem `is_id`
/// accepts. A missing or unreadable directory has none.
fn jsonl_files(dir: &Path, agent: Agent, is_id: fn(&str) -> bool) -> Vec<Candidate> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                return None;
            }
            let session_id = path.file_stem()?.to_str()?.to_string();
            if !is_id(&session_id) {
                return None;
            }
            let metadata = entry.metadata().ok().filter(|meta| meta.is_file())?;
            if metadata.len() == 0 {
                return None;
            }
            let updated_ms = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |age| age.as_millis() as u64);
            Some(Candidate {
                agent,
                session_id,
                path,
                size_bytes: metadata.len(),
                updated_ms,
            })
        })
        .collect()
}

/// A Rebon session's title: the one its sidecar records (generated, or set
/// by the user), else its first prompt. `None` when it has neither.
fn rebon_title(projects_root: &Path, cwd: &str, session_id: &str) -> Option<String> {
    rebon_session::load_session_title(projects_root, cwd, session_id)
        .or_else(|| {
            first_prompt(&rebon_session::transcript_file_path(
                projects_root,
                cwd,
                session_id,
            ))
        })
        .filter(|title| !title.trim().is_empty())
}

/// The first thing the user said in a transcript. `rebon-session`'s
/// first-message helper takes the first user row as it stands, and in a
/// Rebon transcript that row is usually the runtime context the harness
/// injected ahead of the prompt; a title made of `<system-reminder>` names
/// nothing. The rows the reader leaves out are left out here too.
fn first_prompt(transcript: &Path) -> Option<String> {
    let bytes = std::fs::read(transcript).ok()?;
    parse_transcript_jsonl(&bytes)
        .iter()
        .filter(|entry| {
            entry.entry_type == "user" && !is_meta(&entry.raw) && !is_compact_summary(&entry.raw)
        })
        .find_map(|entry| user_text(&entry.raw))
}

// ── Claude Code's layout ─────────────────────────────────────────────

/// The directory name Claude Code files a project's conversations under:
/// every character that is not an ASCII letter or digit becomes `-`, case
/// kept, and a name past 200 characters is cut there and given a hash. That
/// is `sanitize_path`, which Rebon's own project key applies after folding
/// the path — Claude Code does not fold, which is why this spelling keeps the
/// raw path.
pub(crate) fn claude_project_dir_name(cwd: &str) -> String {
    rebon_session::sanitize_path(cwd)
}

/// The directory Claude Code keeps `cwd`'s conversations in, if it has one.
///
/// Looked up by name rather than joined blindly, for two reasons. Windows
/// paths compare without case, and the same folder opened as `F:\dev` and as
/// `f:\dev` is one project there. And for a name long enough to be hashed,
/// Claude Code's hash depends on its runtime, so past the cut only the
/// readable prefix can be matched; a prefix two directories share is not
/// guessed between.
pub(crate) fn claude_project_dir(config: &Path, cwd: &str) -> Option<PathBuf> {
    let projects = config.join("projects");
    let name = claude_project_dir_name(cwd);
    let exact = projects.join(&name);
    if exact.is_dir() {
        return Some(exact);
    }
    let long = name.len() > rebon_session::MAX_SANITIZED_LENGTH;
    if !cfg!(windows) && !long {
        return None;
    }
    let prefix = long.then(|| format!("{}-", &name[..rebon_session::MAX_SANITIZED_LENGTH]));
    let same = |left: &str, right: &str| {
        if cfg!(windows) {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    };
    let matches: Vec<PathBuf> = std::fs::read_dir(&projects)
        .ok()?
        .flatten()
        .filter(|entry| {
            let candidate = entry.file_name().to_string_lossy().into_owned();
            match &prefix {
                // `get`, not an index: a directory name is not ours and may
                // not be ASCII.
                Some(prefix) => {
                    candidate.len() > prefix.len()
                        && candidate
                            .get(..prefix.len())
                            .is_some_and(|head| same(head, prefix))
                }
                None => same(&candidate, &name),
            }
        })
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    match <[PathBuf; 1]>::try_from(matches) {
        Ok([only]) => Some(only),
        Err(_) => None,
    }
}

/// A Claude Code conversation's title: the name the user gave it, else
/// Claude Code's generated title, else the last prompt — the latest of each
/// kind winning. `None` when the end of the file has none of them, which is a
/// conversation where nothing has been said.
fn claude_title(transcript: &Path) -> Option<String> {
    title_from_lines(&read_tail(transcript, TITLE_TAIL_BYTES)?)
}

fn title_from_lines(lines: &str) -> Option<String> {
    let mut named = None;
    let mut generated = None;
    let mut prompt = None;
    for line in lines.lines() {
        // Most lines are messages; parsing each to find a handful of title
        // entries would be the whole cost of a listing.
        if !line.contains("\"agent-name\"")
            && !line.contains("\"ai-title\"")
            && !line.contains("\"last-prompt\"")
        {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let field = |key: &str| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("agent-name") => named = field("agentName").or(named),
            Some("ai-title") => generated = field("aiTitle").or(generated),
            Some("last-prompt") => prompt = field("lastPrompt").or(prompt),
            _ => {}
        }
    }
    named.or(generated).or(prompt)
}

/// The last `bytes` of a file as text, starting at a line: a read that
/// begins mid-file drops the line it cut rather than misread it.
fn read_tail(path: &Path, bytes: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buffer = Vec::with_capacity((length - start) as usize);
    file.read_to_end(&mut buffer).ok()?;
    let text = String::from_utf8_lossy(&buffer).into_owned();
    if start == 0 {
        return Some(text);
    }
    Some(
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default(),
    )
}

// ── ids ──────────────────────────────────────────────────────────────

/// An id that is safe to join onto a directory: letters, digits, `-` and
/// `_`, so it can name nothing but a file in that directory.
pub(crate) fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_ID_LEN
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The 8-4-4-4-12 hexadecimal shape Claude Code names its sessions with.
pub(crate) fn is_uuid(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn iso_ms(ms: u64) -> String {
    rebon_session::format_system_time_iso_ms(UNIX_EPOCH + std::time::Duration::from_millis(ms))
}

// ── rendering ────────────────────────────────────────────────────────

/// Who is speaking in a stretch of the rendered text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Voice {
    User,
    /// The assistant's words, its tool calls, and their results: a tool
    /// result is recorded as a user entry, but it is the tool answering the
    /// assistant, and reads as part of the assistant's turn.
    Assistant,
    /// A compacted conversation's summary of what came before it.
    Summary,
}

impl Voice {
    fn heading(self) -> &'static str {
        match self {
            Voice::User => "### user",
            Voice::Assistant => "### assistant",
            Voice::Summary => "### summary of the earlier conversation",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Part {
    voice: Voice,
    text: String,
}

/// One chain entry's rendering, with where on the chain it sits.
#[derive(Debug, Clone)]
struct Rendered {
    position: usize,
    parts: Vec<Part>,
}

impl Rendered {
    fn cost(&self) -> usize {
        self.parts
            .iter()
            .map(|part| part.text.chars().count() + HEADING_ALLOWANCE)
            .sum()
    }
}

/// The name of every tool the chain called, by call id, so a result can say
/// which tool it answers even when its call is on an earlier page.
fn tool_names(entries: &[TranscriptEntry], chain: &[usize]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for &index in chain {
        let entry = &entries[index];
        if entry.entry_type != "assistant" {
            continue;
        }
        for block in content_blocks(&entry.raw) {
            if is_tool_use(block) {
                if let (Some(id), Some(name)) = (
                    block.get("id").and_then(Value::as_str),
                    block.get("name").and_then(Value::as_str),
                ) {
                    names.insert(id.to_string(), name.to_string());
                }
            }
        }
    }
    names
}

fn content_blocks(raw: &Value) -> &[Value] {
    raw.pointer("/message/content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn is_tool_use(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("tool_use" | "server_tool_use")
    )
}

/// What a model reading the conversation should see of one entry, or `None`
/// for an entry that says nothing to it: attachments, system rows, progress,
/// meta prompts the harness injected, and anything of a kind not known here.
fn render_entry(entry: &TranscriptEntry, names: &HashMap<String, String>) -> Option<Vec<Part>> {
    let parts = match entry.entry_type.as_str() {
        "user" if is_meta(&entry.raw) => return None,
        "user" if is_compact_summary(&entry.raw) => user_text(&entry.raw)
            .map(|text| {
                vec![Part {
                    voice: Voice::Summary,
                    text,
                }]
            })
            .unwrap_or_default(),
        "user" => render_user(&entry.raw, names),
        "assistant" => render_assistant(&entry.raw),
        _ => return None,
    };
    (!parts.is_empty()).then_some(parts)
}

/// A prompt the harness wrote rather than the user: skill bodies, command
/// caveats, runtime context. Both agents mark it; Rebon has used both spots.
fn is_meta(raw: &Value) -> bool {
    let flag = |value: Option<&Value>| value.and_then(Value::as_bool).unwrap_or(false);
    flag(raw.get("isMeta"))
        || flag(raw.pointer("/message/isMeta"))
        || flag(raw.get("runtimeContext"))
}

fn is_compact_summary(raw: &Value) -> bool {
    raw.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
}

/// A user entry's own words, joined; system reminders riding along in a
/// block of their own are the harness's, not the user's.
fn user_text(raw: &Value) -> Option<String> {
    let content = raw.pointer("/message/content")?;
    if let Some(text) = content.as_str() {
        return spoken(text);
    }
    let texts: Vec<String> = content
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .filter_map(spoken)
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n\n"))
}

fn spoken(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty() && !text.starts_with("<system-reminder>")).then(|| text.to_string())
}

fn render_user(raw: &Value, names: &HashMap<String, String>) -> Vec<Part> {
    let mut parts = Vec::new();
    for block in content_blocks(raw) {
        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
            parts.push(Part {
                voice: Voice::Assistant,
                text: tool_result_line(block, names),
            });
        }
    }
    let images = content_blocks(raw)
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        .count();
    let mut said = user_text(raw).unwrap_or_default();
    if images > 0 {
        let note = if images == 1 {
            "[an image]".to_string()
        } else {
            format!("[{images} images]")
        };
        said = if said.is_empty() {
            note
        } else {
            format!("{said}\n{note}")
        };
    }
    if !said.is_empty() {
        parts.push(Part {
            voice: Voice::User,
            text: said,
        });
    }
    parts
}

fn render_assistant(raw: &Value) -> Vec<Part> {
    let mut lines = Vec::new();
    match raw.pointer("/message/content") {
        Some(Value::String(text)) if !text.trim().is_empty() => {
            lines.push(text.trim().to_string());
        }
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block
                            .get("text")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|text| !text.is_empty())
                        {
                            lines.push(text.to_string());
                        }
                    }
                    // Thinking is the agent's working, not what it said, and
                    // it is the bulk of many turns.
                    Some("thinking" | "redacted_thinking") => {}
                    _ if is_tool_use(block) => lines.push(tool_call_line(block)),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if lines.is_empty() {
        return Vec::new();
    }
    vec![Part {
        voice: Voice::Assistant,
        text: lines.join("\n"),
    }]
}

/// `→ Bash: cargo test`: the tool and the one argument that says what it
/// did — the one a transcript row shows beside it, as `rebon-tools-core`
/// records it — else the input itself, cut short.
fn tool_call_line(block: &Value) -> String {
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("a tool");
    let input = block.get("input").unwrap_or(&Value::Null);
    let shown = rebon_tools_core::builtin_tool_facts_for_name(name)
        .and_then(|facts| facts.primary_input)
        .and_then(|primary| input.get(primary.field))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| match input {
            Value::Null => String::new(),
            Value::Object(fields) if fields.is_empty() => String::new(),
            other => other.to_string(),
        });
    let shown = rebon_session_host::shorten_excerpt(&shown, TOOL_INPUT_MAX_CHARS);
    if shown.is_empty() {
        format!("→ {name}")
    } else {
        format!("→ {name}: {shown}")
    }
}

/// `← Bash: 12 passed` or `← Bash error: not found`, cut short.
fn tool_result_line(block: &Value, names: &HashMap<String, String>) -> String {
    let name = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .and_then(|id| names.get(id))
        .map_or("tool", String::as_str);
    let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
    let output = match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => part.get("text").and_then(Value::as_str).map(str::to_string),
                Some("image") => Some("[an image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    let output = rebon_session_host::shorten_excerpt(&output, TOOL_RESULT_MAX_CHARS);
    let output = if output.is_empty() {
        "(no output)".to_string()
    } else {
        output
    };
    if failed {
        format!("← {name} error: {output}")
    } else {
        format!("← {name}: {output}")
    }
}

// ── paging ───────────────────────────────────────────────────────────

/// One `session_read`'s worth of a conversation.
#[derive(Debug)]
struct Page {
    text: String,
    /// Rendered entries in `text`.
    entries: usize,
    truncated: bool,
    /// The uuid to pass as `after` next time: the last chain entry this page
    /// covers, skipped entries included, so a later read does not see them
    /// again either. `None` for a conversation with no chain yet.
    next_cursor: Option<String>,
    cursor_reset: bool,
    note: Option<String>,
}

/// Without a cursor — or with one no longer on the chain — the newest
/// entries that fit in `budget`. With one, the oldest entries after it that
/// fit, so a reader following along never skips anything. Either way at
/// least one entry is shown, cut to the budget if it alone is over it.
fn page(entries: &[TranscriptEntry], chain: &[usize], after: Option<&str>, budget: usize) -> Page {
    let names = tool_names(entries, chain);
    let last_uuid = chain.last().map(|&index| entries[index].uuid.clone());
    let cursor = after.map(|after| chain.iter().position(|&index| entries[index].uuid == after));
    let (start, cursor_reset) = match cursor {
        Some(Some(position)) => (Some(position + 1), false),
        Some(None) => (None, true),
        None => (None, false),
    };
    let rendered: Vec<Rendered> = chain
        .iter()
        .enumerate()
        .skip(start.unwrap_or(0))
        .filter_map(|(position, &index)| {
            render_entry(&entries[index], &names).map(|parts| Rendered { position, parts })
        })
        .collect();

    let mut note = cursor_reset.then(|| {
        "The cursor is no longer on this conversation's current chain — it was rewound or \
         branched since — so this is its latest part, as if no cursor had been given."
            .to_string()
    });
    let mut more_after = false;
    let (chosen, truncated, next_cursor) = if start.is_some() {
        let (chosen, truncated) = take_within(rendered.iter(), budget, false);
        // Stopped short: continue from just before the first entry left out,
        // so what was shown is not shown again and nothing after is skipped.
        // That entry is past the cursor, so there is always one before it.
        let next_cursor = match rendered.get(chosen.len()) {
            Some(first_left_out) => {
                more_after = true;
                Some(entries[chain[first_left_out.position - 1]].uuid.clone())
            }
            None => last_uuid.clone(),
        };
        (chosen, truncated, next_cursor)
    } else {
        let (mut chosen, truncated) = take_within(rendered.iter().rev(), budget, true);
        chosen.reverse();
        (chosen, truncated, last_uuid.clone())
    };

    let text = if chain.is_empty() {
        "(Nothing has been said in this conversation yet.)".to_string()
    } else if chosen.is_empty() {
        "(Nothing new since the cursor.)".to_string()
    } else {
        assemble(&chosen)
    };
    if truncated && start.is_none() {
        note.get_or_insert_with(|| {
            "Not all of the conversation fits in max_chars; this is its latest part, and what \
             came before it was left out."
                .to_string()
        });
    } else if more_after {
        note =
            Some("There is more after this page; call again with after = next_cursor.".to_string());
    }
    Page {
        text,
        entries: chosen.len(),
        truncated,
        next_cursor,
        cursor_reset,
        note,
    }
}

/// Entries from `candidates`, in order, while they fit in `budget`, and
/// whether any were left out. The first one is always taken; if it alone is
/// over the budget it is cut — keeping its end when reading from the newest
/// back (`keep_end`), its start when reading forward.
fn take_within<'a>(
    candidates: impl Iterator<Item = &'a Rendered>,
    budget: usize,
    keep_end: bool,
) -> (Vec<Rendered>, bool) {
    let mut chosen: Vec<Rendered> = Vec::new();
    let mut spent = 0usize;
    for candidate in candidates {
        let cost = candidate.cost();
        if spent + cost <= budget {
            spent += cost;
            chosen.push(candidate.clone());
            continue;
        }
        if chosen.is_empty() {
            chosen.push(clip_entry(candidate, budget, keep_end));
        }
        return (chosen, true);
    }
    (chosen, false)
}

/// An entry cut down to `budget`, each part given what is left of it.
fn clip_entry(entry: &Rendered, budget: usize, keep_end: bool) -> Rendered {
    let mut left = budget;
    let mut parts = Vec::new();
    for part in &entry.parts {
        let room = left.saturating_sub(HEADING_ALLOWANCE);
        let length = part.text.chars().count();
        let text = if length <= room {
            part.text.clone()
        } else {
            clip_text(&part.text, room, keep_end)
        };
        left = left.saturating_sub(text.chars().count() + HEADING_ALLOWANCE);
        parts.push(Part {
            voice: part.voice,
            text,
        });
        if left == 0 {
            break;
        }
    }
    Rendered {
        position: entry.position,
        parts,
    }
}

/// `text` in `room` characters, marker included.
fn clip_text(text: &str, room: usize, keep_end: bool) -> String {
    let length = text.chars().count();
    // The marker's own length depends on the count it states; a count with
    // as many digits as the whole text's length is never too short.
    let marker_len = format!(
        "[… {length} characters of this message left out; a larger max_chars shows more]\n"
    )
    .chars()
    .count();
    let keep = room.saturating_sub(marker_len);
    let dropped = length.saturating_sub(keep);
    let marker =
        format!("[… {dropped} characters of this message left out; a larger max_chars shows more]");
    if keep_end {
        let kept: String = text.chars().skip(dropped).collect();
        format!("{marker}\n{kept}")
    } else {
        let kept: String = text.chars().take(keep).collect();
        format!("{kept}\n{marker}")
    }
}

/// The page as text, a heading wherever the voice changes.
fn assemble(chosen: &[Rendered]) -> String {
    let mut out = String::new();
    let mut voice = None;
    for part in chosen.iter().flat_map(|entry| &entry.parts) {
        if voice != Some(part.voice) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(part.voice.heading());
            out.push('\n');
            voice = Some(part.voice);
        } else {
            out.push('\n');
        }
        out.push_str(&part.text);
    }
    out
}

#[cfg(test)]
#[path = "sessions_tests.rs"]
mod tests;
