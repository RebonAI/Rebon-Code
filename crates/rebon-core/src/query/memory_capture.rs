//! The engine's half of memory capture
//! ([`rebon_instructions::memory_capture`]).
//!
//! When a turn ends, in the background and on the small model, the
//! conversation since the last capture — the turns on disk, this one
//! included — is put to the memory store's question, and what comes back is
//! kept. At the end rather than the start, so the index the question shows
//! already holds what the turn saved itself: asked beside the turn, the pass
//! saw the index from before it and wrote near-copies of the memories the
//! model was saving at the same moment. It runs beside the turn on a forked session, as the title
//! does, so it neither waits on the turn nor joins its history; the store's
//! index changing underneath is announced to the next turn by the ordinary
//! mid-session memory update.
//!
//! Where it stops is kept beside the transcript (`<session>.memory-capture.json`)
//! rather than in this process, because a background job builds a fresh
//! engine for every turn it runs.
//!
//! A session in an agent group is also asked, at the end of each turn, what
//! the turn found out that the group should keep
//! ([`rebon_instructions::memory_capture::GroupCapture`]). That one reads the
//! agent's work as well as the person's words — a worker the router sends
//! is handed a task and talks to no one — and runs when the turn is done, so
//! a worker with one long turn is asked about it rather than never. Its
//! place is kept in `<session>.group-capture.json`.
//!
//! The router is asked its own question instead, at the end of its turns
//! ([`rebon_instructions::memory_capture::MemoryCapture::ask_router`]): most
//! of what reaches it is delivered — worker digests, events, progress checks
//! — not said by the person, and what it learns from them belongs to the
//! person's memory, its own workspace's, or a project's it dispatched into.
//! Its place is kept in `<session>.router-capture.json`, with when it last
//! asked, so a run of quiet deliveries is read together every few minutes
//! rather than once each.

use super::*;

use std::collections::HashSet;

use rebon_instructions::memory_capture::{GroupCapture, MemoryCapture};

/// What one message may bring into the question, and the whole of it. The
/// newest part is kept: an old stretch was asked about already, or is the
/// first capture of a long session, where the recent part matters most.
const MAX_MESSAGE_CHARS: usize = 2_000;
const MAX_CONVERSATION_CHARS: usize = 16_000;
/// Less said by the person than this is not worth a question.
const MIN_PERSON_CHARS: usize = 12;

/// One capture in flight per session; a turn that opens while the last one
/// is still being answered leaves its stretch to the next.
#[derive(Default)]
pub(super) struct MemoryCaptureRuns(Mutex<HashSet<String>>);

impl MemoryCaptureRuns {
    fn begin(&self, session_id: &str) -> bool {
        let began = self
            .0
            .lock()
            .expect("memory capture runs poisoned")
            .insert(session_id.to_owned());
        if began {
            IN_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        began
    }

    fn end(&self, session_id: &str) {
        let ended = self
            .0
            .lock()
            .expect("memory capture runs poisoned")
            .remove(session_id);
        if ended {
            IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            SETTLED.notify_waiters();
        }
    }
}

/// Captures in flight in this process, whichever executor started them.
static IN_FLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static SETTLED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Wait, at most `timeout`, for the captures in flight to finish. A process
/// that ends right after its turn — `rebon exec` — calls this first, since a
/// capture runs once the turn is done and would otherwise die with it.
pub async fn settle_memory_capture(timeout: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let settled = SETTLED.notified();
        if IN_FLIGHT.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            return;
        }
        if tokio::time::timeout_at(deadline, settled).await.is_err() {
            return;
        }
    }
}

pub(super) struct MemoryCaptureJob {
    pub(super) capture: Arc<dyn MemoryCapture>,
    pub(super) session: Arc<SessionHandle>,
    /// The small model the title uses.
    pub(super) model: String,
    pub(super) projects_root: PathBuf,
    pub(super) cwd: String,
    pub(super) session_id: String,
    /// The prompt that opened this turn, not on disk yet.
    pub(super) prompt_text: String,
}

/// `REBON_MEMORY_CAPTURE=0` turns capture off.
pub(super) fn memory_capture_enabled() -> bool {
    !rebon_types::env::env_defined_falsy("REBON_MEMORY_CAPTURE")
}

pub(super) fn maybe_spawn_memory_capture(job: MemoryCaptureJob, runs: &Arc<MemoryCaptureRuns>) {
    if !memory_capture_enabled() || job.model.trim().is_empty() {
        return;
    }
    if !runs.begin(&job.session_id) {
        return;
    }
    let runs = runs.clone();
    let forked = job.session.fork_for_sub_agent(None);
    tokio::spawn(async move {
        let session_id = job.session_id.clone();
        run(&job, forked.client().as_ref()).await;
        runs.end(&session_id);
    });
}

async fn run(job: &MemoryCaptureJob, client: &dyn rebon_api::ModelClient) {
    let transcript =
        rebon_session::transcript_file_path(&job.projects_root, &job.cwd, &job.session_id);
    let cursor_path = cursor_path(&transcript);
    let through = read_cursor(&cursor_path);
    let entries = match rebon_session::load_transcript_from_file(&transcript) {
        Ok(Some(loaded)) => loaded.messages,
        _ => Vec::new(),
    };
    let (slice, last_uuid) = since(&entries, through.as_deref());
    let mut messages = transcript_to_api_messages(slice);
    let prompt = job.prompt_text.trim();
    if !prompt.is_empty() && !is_machine_text(prompt) && !ends_with_person_saying(&messages, prompt)
    {
        messages.push(ApiMessage::user_text(prompt));
    }
    let (conversation, person_chars) = render_conversation(&messages);
    if person_chars < MIN_PERSON_CHARS {
        if let Some(uuid) = last_uuid {
            write_cursor(&cursor_path, &uuid);
        }
        return;
    }
    let cwd = std::path::Path::new(&job.cwd);
    let Some(ask) = job.capture.ask(cwd, &conversation) else {
        return;
    };
    let mut request =
        rebon_api::CreateMessageRequest::simple(&job.model, ask.prompt).with_system(ask.system);
    request.stream = false;
    request.max_tokens = ask.max_tokens;
    // The answer is a short JSON document; a provider set to think hard
    // would spend the budget thinking, as with titles and compaction.
    request.thinking = Some(rebon_api::ThinkingConfig::Disabled);
    let answer = match client.create_message(request).await {
        Ok(message) => message.text(),
        Err(error) => {
            tracing::debug!(%error, session_id = %job.session_id, "memory capture: model call failed");
            return;
        }
    };
    let kept = job.capture.keep(cwd, &answer);
    // One line per question asked, kept or not: how often this runs and how
    // often it finds something is what tells whether it pays.
    tracing::info!(
        session_id = %job.session_id,
        memories = ?kept,
        "memory capture asked about the conversation"
    );
    if let Some(uuid) = last_uuid {
        write_cursor(&cursor_path, &uuid);
    }
}

/// Less said by the agent than this in a turn is not worth asking the
/// group's question about.
const MIN_AGENT_CHARS: usize = 200;
/// How much of the task that opened the work stays when the newest part
/// fills the budget.
const MAX_TASK_CHARS: usize = 1_500;

pub(super) struct GroupCaptureJob {
    pub(super) capture: Arc<dyn GroupCapture>,
    pub(super) session: Arc<SessionHandle>,
    /// The small model the title uses.
    pub(super) model: String,
    pub(super) projects_root: PathBuf,
    pub(super) cwd: String,
    pub(super) session_id: String,
}

/// After a turn: ask what it found out that the session's group should
/// keep, in the background. Shares the memory capture's one-in-flight rule,
/// under a key of its own.
pub(super) fn maybe_spawn_group_capture(job: GroupCaptureJob, runs: &Arc<MemoryCaptureRuns>) {
    if !memory_capture_enabled() || job.model.trim().is_empty() {
        return;
    }
    let run_key = format!("group:{}", job.session_id);
    if !runs.begin(&run_key) {
        return;
    }
    let runs = runs.clone();
    let forked = job.session.fork_for_sub_agent(None);
    tokio::spawn(async move {
        run_group(&job, forked.client().as_ref()).await;
        runs.end(&run_key);
    });
}

async fn run_group(job: &GroupCaptureJob, client: &dyn rebon_api::ModelClient) {
    if !job.capture.in_group(&job.session_id) {
        return;
    }
    let transcript =
        rebon_session::transcript_file_path(&job.projects_root, &job.cwd, &job.session_id);
    let cursor_path = transcript.with_extension("group-capture.json");
    let through = read_cursor(&cursor_path);
    let entries = match rebon_session::load_transcript_from_file(&transcript) {
        Ok(Some(loaded)) => loaded.messages,
        _ => Vec::new(),
    };
    let (slice, last_uuid) = since(&entries, through.as_deref());
    let messages = transcript_to_api_messages(slice);
    let (work, agent_chars) = render_work(&messages);
    let ask = (agent_chars >= MIN_AGENT_CHARS)
        .then(|| job.capture.ask(&job.session_id, &work))
        .flatten();
    if let Some(ask) = ask {
        let mut request =
            rebon_api::CreateMessageRequest::simple(&job.model, ask.prompt).with_system(ask.system);
        request.stream = false;
        request.max_tokens = ask.max_tokens;
        request.thinking = Some(rebon_api::ThinkingConfig::Disabled);
        let answer = match client.create_message(request).await {
            Ok(message) => message.text(),
            Err(error) => {
                tracing::debug!(%error, session_id = %job.session_id, "group capture: model call failed");
                return;
            }
        };
        let kept = job.capture.keep(&job.session_id, &answer);
        tracing::info!(
            session_id = %job.session_id,
            facts = ?kept,
            "group capture asked about the turn"
        );
    }
    if let Some(uuid) = last_uuid {
        write_cursor(&cursor_path, &uuid);
    }
}

/// `Task:`, `Person:` and `Assistant:` lines, newest part kept, and how
/// much the agent said in it. A prompt the scheduler or the router
/// delivered is the task the work is on, so it is shown as one; when the
/// newest part fills the budget, the last task before it still opens it.
fn render_work(messages: &[ApiMessage]) -> (String, usize) {
    let mut lines: Vec<String> = Vec::new();
    let mut agent_chars = 0;
    for message in messages {
        match message.role {
            Role::User => {
                for text in message.content.iter().filter_map(|block| block.as_text()) {
                    let trimmed = text.trim_start();
                    if trimmed.starts_with(rebon_tool::cron::SCHEDULED_PROMPT_MARKER) {
                        let task = rebon_tool::cron::strip_scheduled_marker(trimmed).trim();
                        if !task.is_empty() {
                            lines.push(format!("Task: {}", clip(task, MAX_TASK_CHARS)));
                        }
                    } else if !is_machine_text(text) && !text.trim().is_empty() {
                        lines.push(format!("Person: {}", clip(text.trim(), MAX_MESSAGE_CHARS)));
                    }
                }
            }
            Role::Assistant => {
                let text = assistant_text(message);
                let text = text.trim();
                if !text.is_empty() {
                    agent_chars += text.chars().count();
                    lines.push(format!("Assistant: {}", clip(text, MAX_MESSAGE_CHARS)));
                }
            }
            _ => {}
        }
    }
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0;
    let mut first_kept = lines.len();
    for (index, line) in lines.iter().enumerate().rev() {
        let size = line.chars().count() + 1;
        if total + size > MAX_CONVERSATION_CHARS && !kept.is_empty() {
            break;
        }
        total += size;
        kept.push(line.clone());
        first_kept = index;
    }
    kept.reverse();
    if let Some(task) = lines[..first_kept]
        .iter()
        .rev()
        .find(|line| line.starts_with("Task: ") || line.starts_with("Person: "))
    {
        kept.insert(0, task.clone());
    }
    (kept.join("\n"), agent_chars)
}

/// How often the router's turns are put to the question when the person
/// said nothing in them. Deliveries alone — digests, events, progress
/// checks — come every few seconds on a busy day; what they teach keeps
/// until a later pass reads them together.
const ROUTER_QUIET_INTERVAL_MS: u64 = 5 * 60 * 1000;
/// Less than this in a stretch of the router's turns is not worth asking.
const MIN_ROUTER_CHARS: usize = 200;
/// The router's calls that say nothing worth remembering about the turn.
const ROUTER_QUIET_CALLS: &[&str] = &[
    "Recall",
    "ReadMemory",
    "ToolSearch",
    "InvokeDeferredTool",
    "ListAuthorizations",
    "ScheduleWakeup",
    "SyntheticOutput",
];

pub(super) struct RouterCaptureJob {
    pub(super) capture: Arc<dyn MemoryCapture>,
    pub(super) session: Arc<SessionHandle>,
    /// The small model the title uses.
    pub(super) model: String,
    pub(super) projects_root: PathBuf,
    /// The router's own workspace.
    pub(super) cwd: String,
    pub(super) session_id: String,
}

/// After a router turn: ask what it taught that should outlive the
/// router's conversation, in the background.
pub(super) fn maybe_spawn_router_capture(job: RouterCaptureJob, runs: &Arc<MemoryCaptureRuns>) {
    if !memory_capture_enabled() || job.model.trim().is_empty() {
        return;
    }
    let run_key = format!("router:{}", job.session_id);
    if !runs.begin(&run_key) {
        return;
    }
    let runs = runs.clone();
    let forked = job.session.fork_for_sub_agent(None);
    tokio::spawn(async move {
        run_router(&job, forked.client().as_ref()).await;
        runs.end(&run_key);
    });
}

async fn run_router(job: &RouterCaptureJob, client: &dyn rebon_api::ModelClient) {
    let transcript =
        rebon_session::transcript_file_path(&job.projects_root, &job.cwd, &job.session_id);
    let cursor_path = transcript.with_extension("router-capture.json");
    let (through, asked_at_ms) = read_router_cursor(&cursor_path);
    let entries = match rebon_session::load_transcript_from_file(&transcript) {
        Ok(Some(loaded)) => loaded.messages,
        _ => Vec::new(),
    };
    let (slice, last_uuid) = since(&entries, through.as_deref());
    let Some(last_uuid) = last_uuid else {
        return;
    };
    let turn = render_router_turn(&transcript_to_api_messages(slice));
    let now = now_ms();
    if turn.person_chars == 0
        && asked_at_ms.is_some_and(|at| now.saturating_sub(at) < ROUTER_QUIET_INTERVAL_MS)
    {
        // Left for a later pass, which reads it with what comes next.
        return;
    }
    if turn.text.chars().count() < MIN_ROUTER_CHARS {
        write_router_cursor(&cursor_path, &last_uuid, asked_at_ms);
        return;
    }
    let cwd = std::path::Path::new(&job.cwd);
    let Some(ask) = job.capture.ask_router(cwd, &turn.text, &turn.projects) else {
        write_router_cursor(&cursor_path, &last_uuid, asked_at_ms);
        return;
    };
    let mut request =
        rebon_api::CreateMessageRequest::simple(&job.model, ask.prompt).with_system(ask.system);
    request.stream = false;
    request.max_tokens = ask.max_tokens;
    request.thinking = Some(rebon_api::ThinkingConfig::Disabled);
    let answer = match client.create_message(request).await {
        Ok(message) => message.text(),
        Err(error) => {
            tracing::debug!(%error, session_id = %job.session_id, "router capture: model call failed");
            return;
        }
    };
    let kept = job.capture.keep_router(cwd, &answer, &turn.projects);
    tracing::info!(
        session_id = %job.session_id,
        memories = ?kept,
        projects = ?turn.projects,
        "router capture asked about its turns"
    );
    write_router_cursor(&cursor_path, &last_uuid, Some(now));
}

/// One stretch of the router's turns, as the question reads it.
struct RouterTurn {
    /// `Delivered:`, `Person:` and `Router …` lines, newest part kept.
    text: String,
    /// How much of it the person wrote.
    person_chars: usize,
    /// The project directories its calls named, in order.
    projects: Vec<String>,
}

fn render_router_turn(messages: &[ApiMessage]) -> RouterTurn {
    let mut lines: Vec<String> = Vec::new();
    let mut person_chars = 0;
    let mut projects: Vec<String> = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                for text in message.content.iter().filter_map(|block| block.as_text()) {
                    let trimmed = text.trim_start();
                    if trimmed.starts_with(rebon_tool::cron::SCHEDULED_PROMPT_MARKER) {
                        let delivered = rebon_tool::cron::strip_scheduled_marker(trimmed).trim();
                        if !delivered.is_empty() {
                            lines
                                .push(format!("Delivered: {}", clip(delivered, MAX_MESSAGE_CHARS)));
                        }
                    } else if !is_machine_text(text) && !text.trim().is_empty() {
                        person_chars += text.trim().chars().count();
                        lines.push(format!("Person: {}", clip(text.trim(), MAX_MESSAGE_CHARS)));
                    }
                }
            }
            Role::Assistant => {
                for block in &message.content {
                    if let Some(text) = block.as_text().map(str::trim).filter(|t| !t.is_empty()) {
                        lines.push(format!(
                            "Router (its own notes): {}",
                            clip(text, MAX_MESSAGE_CHARS)
                        ));
                    } else if let Some(call) = block.as_tool_use() {
                        collect_projects(&call.input, &mut projects);
                        if let Some(line) = describe_router_call(&call.name, &call.input) {
                            lines.push(line);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0;
    for line in lines.into_iter().rev() {
        let size = line.chars().count() + 1;
        if total + size > MAX_CONVERSATION_CHARS && !kept.is_empty() {
            break;
        }
        total += size;
        kept.push(line);
    }
    kept.reverse();
    RouterTurn {
        text: kept.join("\n"),
        person_chars,
        projects,
    }
}

/// A line for one of the router's calls, or `None` for one that only
/// looks things up.
fn describe_router_call(name: &str, input: &serde_json::Value) -> Option<String> {
    if ROUTER_QUIET_CALLS.contains(&name) {
        return None;
    }
    let field = |key: &str| {
        input
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .unwrap_or_default()
    };
    if let Some(message) = rebon_types::tell_person_message(name, input) {
        return Some(format!(
            "Router to the person: {}",
            clip(message, MAX_MESSAGE_CHARS)
        ));
    }
    let line = match name {
        "SaveMemory" => format!(
            "Router saved memory ({}): {}",
            field("scope"),
            field("title")
        ),
        "DispatchWorker" => format!(
            "Router dispatched a worker into {}: {}. Brief: {}",
            field("project"),
            field("title"),
            clip(field("brief"), 400)
        ),
        "PlanTasks" => {
            let tasks: Vec<String> = input
                .get("tasks")
                .and_then(|tasks| tasks.as_array())
                .into_iter()
                .flatten()
                .map(|task| {
                    let text = |key: &str| task.get(key).and_then(|v| v.as_str()).unwrap_or("");
                    format!("{} in {}", text("title"), text("project"))
                })
                .collect();
            format!("Router planned tasks: {}", tasks.join("; "))
        }
        _ => format!("Router called {name}: {}", clip(&input.to_string(), 300)),
    };
    Some(line)
}

/// Every `project` string anywhere in a call's input, once each.
fn collect_projects(input: &serde_json::Value, projects: &mut Vec<String>) {
    match input {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                match value.as_str().map(str::trim) {
                    Some(project) if key == "project" && !project.is_empty() => {
                        if !projects.iter().any(|known| known == project) {
                            projects.push(project.to_string());
                        }
                    }
                    _ => collect_projects(value, projects),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_projects(item, projects);
            }
        }
        _ => {}
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Where the router's pass stopped, and when it last asked.
fn read_router_cursor(path: &std::path::Path) -> (Option<String>, Option<u64>) {
    let Some(value) = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    else {
        return (None, None);
    };
    (
        value
            .get("through")
            .and_then(|through| through.as_str())
            .map(str::to_string),
        value.get("atMs").and_then(|at| at.as_u64()),
    )
}

fn write_router_cursor(path: &std::path::Path, uuid: &str, at_ms: Option<u64>) {
    let body = serde_json::json!({ "through": uuid, "atMs": at_ms }).to_string();
    if let Err(error) = std::fs::write(path, body) {
        tracing::debug!(%error, path = %path.display(), "router capture: could not record where it stopped");
    }
}

/// The entries after `through` (all of them when it is unknown: the
/// transcript was rewound past it, or this is the first capture), and the
/// uuid of the last one.
fn since<'a>(
    entries: &'a [rebon_session::TranscriptEntry],
    through: Option<&str>,
) -> (&'a [rebon_session::TranscriptEntry], Option<String>) {
    let start = through
        .and_then(|uuid| entries.iter().position(|entry| entry.uuid == uuid))
        .map_or(0, |index| index + 1);
    let slice = &entries[start.min(entries.len())..];
    (slice, entries.last().map(|entry| entry.uuid.clone()))
}

fn ends_with_person_saying(messages: &[ApiMessage], prompt: &str) -> bool {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User && !person_text(message).is_empty())
        .is_some_and(|message| person_text(message).trim() == prompt)
}

/// What the person wrote in a user message: its text, without the
/// reminders, attachments and command echoes the engine puts in the same
/// role. Tool results are not the person's either.
fn person_text(message: &ApiMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .filter(|text| !is_machine_text(text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assistant_text(message: &ApiMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Text in the person's role that the person did not write: reminders and
/// attachments, and every prompt a scheduler, the router or an automation
/// delivers (they open with the scheduler's marker line).
fn is_machine_text(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with(rebon_tool::cron::SCHEDULED_PROMPT_MARKER)
        || text.starts_with("<system-reminder>")
        || text.starts_with("<rebon-")
        || text.starts_with("<runtime_context>")
        || text.starts_with("<system-generated-history-summary>")
        || super::prompt::is_synthetic_title_source(text)
}

/// `Person:` / `Assistant:` lines, newest part kept, and how much the
/// person said in it.
fn render_conversation(messages: &[ApiMessage]) -> (String, usize) {
    let mut lines: Vec<String> = Vec::new();
    let mut person_chars = 0;
    for message in messages {
        let (label, text) = match message.role {
            Role::User => {
                let text = person_text(message);
                person_chars += text.trim().chars().count();
                ("Person", text)
            }
            Role::Assistant => ("Assistant", assistant_text(message)),
            _ => continue,
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        lines.push(format!("{label}: {}", clip(text, MAX_MESSAGE_CHARS)));
    }
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0;
    for line in lines.into_iter().rev() {
        let size = line.chars().count() + 1;
        if total + size > MAX_CONVERSATION_CHARS && !kept.is_empty() {
            break;
        }
        total += size;
        kept.push(line);
    }
    kept.reverse();
    (kept.join("\n"), person_chars)
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head} […]")
}

fn cursor_path(transcript: &std::path::Path) -> PathBuf {
    transcript.with_extension("memory-capture.json")
}

fn read_cursor(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("through")
        .and_then(|through| through.as_str())
        .map(str::to_string)
}

fn write_cursor(path: &std::path::Path, uuid: &str) {
    let body = serde_json::json!({ "through": uuid }).to_string();
    if let Err(error) = std::fs::write(path, body) {
        tracing::debug!(%error, path = %path.display(), "memory capture: could not record where it stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str, uuid: &str) -> rebon_session::TranscriptEntry {
        rebon_session::TranscriptEntry {
            entry_type: kind.to_string(),
            uuid: uuid.to_string(),
            parent_uuid: None,
            timestamp: None,
            raw: serde_json::Value::Null,
        }
    }

    #[test]
    fn a_stretch_starts_after_the_last_capture_and_ends_at_the_newest_row() {
        let entries = vec![
            entry("user", "a"),
            entry("assistant", "b"),
            entry("user", "c"),
        ];
        let (slice, last) = since(&entries, Some("b"));
        assert_eq!(slice.len(), 1);
        assert_eq!(slice[0].uuid, "c");
        assert_eq!(last.as_deref(), Some("c"));
        // A cursor the transcript no longer has reads everything.
        assert_eq!(since(&entries, Some("gone")).0.len(), 3);
        assert_eq!(since(&entries, None).0.len(), 3);
        assert_eq!(since(&[], None).1, None);
    }

    #[test]
    fn the_conversation_is_what_the_person_and_the_assistant_said() {
        let messages = vec![
            ApiMessage::user_text("<system-reminder>\nruntime\n</system-reminder>"),
            ApiMessage::user_text("Always answer in Chinese please"),
            ApiMessage::assistant_text("好的"),
            // The router's digest is delivered in the person's role but is
            // not the person speaking.
            ApiMessage::user_text(rebon_tool::cron::mark_scheduled_prompt(
                "[Worker digest]\n- w1 finished",
            )),
            ApiMessage::assistant_text("Nothing to do."),
            ApiMessage {
                role: Role::User,
                content: vec![rebon_api::ContentBlock::ToolResult(
                    rebon_api::ToolResultBlock {
                        tool_use_id: "t".into(),
                        content: rebon_api::ToolResultContent::text("file contents"),
                        is_error: false,
                    },
                )],
            },
        ];
        let (text, person) = render_conversation(&messages);
        assert_eq!(
            text,
            "Person: Always answer in Chinese please\nAssistant: 好的\nAssistant: Nothing to do."
        );
        assert_eq!(person, "Always answer in Chinese please".chars().count());
        assert!(ends_with_person_saying(
            &messages,
            "Always answer in Chinese please"
        ));
        assert!(!ends_with_person_saying(&messages, "something else"));
    }

    #[test]
    fn a_long_conversation_keeps_its_newest_part() {
        let messages: Vec<ApiMessage> = (0..40)
            .map(|n| ApiMessage::user_text(format!("{n}: {}", "x".repeat(900))))
            .collect();
        let (text, _) = render_conversation(&messages);
        assert!(text.chars().count() <= MAX_CONVERSATION_CHARS);
        assert!(text.ends_with(&"x".repeat(10)));
        assert!(text.contains("39: "));
        assert!(!text.contains("Person: 0: "));
    }

    #[test]
    fn a_workers_turn_reads_as_its_task_and_what_it_said() {
        let brief = rebon_tool::cron::mark_scheduled_prompt("Fix the flaky login test");
        let messages = vec![
            ApiMessage::user_text(&brief),
            ApiMessage::assistant_text("The test raced the session store; it needs a lock."),
            ApiMessage::user_text("<system-reminder>\nGroup news\n</system-reminder>"),
            ApiMessage::user_text("also check the logout one"),
        ];
        let (work, agent_chars) = render_work(&messages);
        assert_eq!(
            work,
            "Task: Fix the flaky login test\n\
             Assistant: The test raced the session store; it needs a lock.\n\
             Person: also check the logout one"
        );
        assert_eq!(agent_chars, 50);
    }

    #[test]
    fn a_long_turn_keeps_its_task_and_its_end() {
        let brief = rebon_tool::cron::mark_scheduled_prompt("Port the parser");
        let mut messages = vec![ApiMessage::user_text(&brief)];
        for index in 0..40 {
            messages.push(ApiMessage::assistant_text(format!(
                "{index} {}",
                "x".repeat(900)
            )));
        }
        let (work, _) = render_work(&messages);
        assert!(
            work.starts_with("Task: Port the parser\nAssistant: "),
            "{}",
            &work[..80]
        );
        assert!(work.ends_with(&"x".repeat(900)));
        assert!(!work.contains("\nAssistant: 0 "));
        assert!(work.chars().count() <= MAX_CONVERSATION_CHARS + 100);
    }

    #[test]
    fn a_router_turn_reads_as_what_was_delivered_said_and_done() {
        let digest = rebon_tool::cron::mark_scheduled_prompt("- worker w-1 done: tests pass");
        let mut acted = ApiMessage::assistant_text("Next: ship it.");
        for (name, input) in [
            (
                "DispatchWorker",
                serde_json::json!({"project": "/w/app", "title": "Release", "brief": "Cut 1.2"}),
            ),
            ("Recall", serde_json::json!({"id": "w-1"})),
            (
                "PlanTasks",
                serde_json::json!({"tasks": [{"title": "Docs", "project": "/w/site"},
                                              {"title": "Again", "project": "/w/app"}]}),
            ),
            (
                rebon_types::TELL_PERSON_TOOL_NAME,
                serde_json::json!({"message": "w-1 is done; releasing."}),
            ),
        ] {
            acted
                .content
                .push(rebon_api::ContentBlock::ToolUse(rebon_api::ToolUseBlock {
                    id: format!("toolu_{name}"),
                    name: name.to_string(),
                    input,
                }));
        }
        let messages = vec![
            ApiMessage::user_text(&digest),
            ApiMessage::user_text("always release on Mondays"),
            acted,
        ];
        let turn = render_router_turn(&messages);
        assert_eq!(
            turn.text,
            "Delivered: - worker w-1 done: tests pass\n\
             Person: always release on Mondays\n\
             Router (its own notes): Next: ship it.\n\
             Router dispatched a worker into /w/app: Release. Brief: Cut 1.2\n\
             Router planned tasks: Docs in /w/site; Again in /w/app\n\
             Router to the person: w-1 is done; releasing."
        );
        assert_eq!(turn.person_chars, "always release on Mondays".len());
        assert_eq!(turn.projects, ["/w/app", "/w/site"]);
    }

    #[test]
    fn the_router_cursor_keeps_when_it_last_asked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.router-capture.json");
        assert_eq!(read_router_cursor(&path), (None, None));
        write_router_cursor(&path, "u7", None);
        assert_eq!(read_router_cursor(&path), (Some("u7".into()), None));
        write_router_cursor(&path, "u9", Some(42));
        assert_eq!(read_router_cursor(&path), (Some("u9".into()), Some(42)));
    }

    #[test]
    fn where_it_stopped_survives_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("s1.jsonl");
        let path = cursor_path(&transcript);
        assert_eq!(path.file_name().unwrap(), "s1.memory-capture.json");
        assert_eq!(read_cursor(&path), None);
        write_cursor(&path, "u-42");
        assert_eq!(read_cursor(&path).as_deref(), Some("u-42"));
    }
}
