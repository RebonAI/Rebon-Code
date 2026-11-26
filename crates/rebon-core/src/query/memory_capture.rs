//! The engine's half of memory capture
//! ([`rebon_instructions::memory_capture`]).
//!
//! At the start of a turn, in the background and on the small model, the
//! conversation since the last capture — the finished turns on disk plus the
//! prompt just sent — is put to the memory store's question, and what comes
//! back is kept. It runs beside the turn on a forked session, as the title
//! does, so it neither waits on the turn nor joins its history; the store's
//! index changing underneath is announced to the next turn by the ordinary
//! mid-session memory update.
//!
//! Where it stops is kept beside the transcript (`<session>.memory-capture.json`)
//! rather than in this process, because a background job builds a fresh
//! engine for every turn it runs.

use super::*;

use std::collections::HashSet;

use rebon_instructions::memory_capture::MemoryCapture;

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
        self.0
            .lock()
            .expect("memory capture runs poisoned")
            .insert(session_id.to_owned())
    }

    fn end(&self, session_id: &str) {
        self.0
            .lock()
            .expect("memory capture runs poisoned")
            .remove(session_id);
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
    if !kept.is_empty() {
        tracing::info!(
            session_id = %job.session_id,
            memories = ?kept,
            "memory capture kept what the conversation said"
        );
    }
    if let Some(uuid) = last_uuid {
        write_cursor(&cursor_path, &uuid);
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
