use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tokio::runtime::Handle;

use crate::tui::wiring::TuiEngineSession;

pub(super) fn parse_agent_interrupt_redirect(message: &str) -> Option<String> {
    let trimmed = message.trim();
    for command in ["/interrupt", "/redirect"] {
        let Some(head) = trimmed.get(..command.len()) else {
            continue;
        };
        if !head.eq_ignore_ascii_case(command) {
            continue;
        }
        let rest = trimmed.get(command.len()..).unwrap_or_default();
        if rest.is_empty() {
            return Some(String::new());
        }
        if rest
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_whitespace())
        {
            return Some(rest.trim().to_string());
        }
    }
    None
}

pub(super) fn interrupt_and_continue_local_agent_task(
    session: &TuiEngineSession,
    handle: &Handle,
    task_id: &str,
    new_instruction: &str,
    start_backgrounded: bool,
) -> Result<String, String> {
    use rebon_plugin_tasks::runtime::{stop_task, TaskData, TaskId, TaskKind};
    use rebon_tool::{SubAgentSpec, ToolFilter};

    let old_id = TaskId::new(task_id);
    let snapshot = session
        .engine_half
        .tasks
        .snapshot(&old_id)
        .ok_or_else(|| format!("Agent \"{task_id}\" has no active task."))?;
    if snapshot.kind != TaskKind::LocalAgent {
        return Err(format!(
            "Agent \"{task_id}\" is a {} task, not a local Agent worker",
            snapshot.kind.as_str()
        ));
    }
    if snapshot.status.is_terminal() {
        return Err(format!(
            "Agent \"{task_id}\" is in terminal state \"{}\" and cannot be interrupted.",
            snapshot.status.as_str()
        ));
    }
    let TaskData::LocalAgent(data) = snapshot.data.clone() else {
        return Err(format!(
            "Agent \"{task_id}\" does not have local-agent state."
        ));
    };

    let new_task_id = next_local_agent_continuation_id();
    let model = data
        .model
        .clone()
        .unwrap_or_else(|| session.model.default_name.clone());
    let mut metadata = if snapshot.metadata.is_object() {
        snapshot.metadata.clone()
    } else {
        serde_json::json!({ "previous_metadata": snapshot.metadata.clone() })
    };
    let title = continuation_title(&snapshot.title, new_instruction);
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "continuation_of".to_string(),
            Value::String(task_id.to_string()),
        );
        obj.insert(
            "continuation_reason".to_string(),
            Value::String("interrupt".to_string()),
        );
        obj.insert("agent_id".to_string(), Value::String(new_task_id.clone()));
        obj.insert(
            "agent_type".to_string(),
            Value::String(data.agent_type.clone()),
        );
        obj.insert("description".to_string(), Value::String(title));
        obj.insert(
            "parent_session_id".to_string(),
            Value::String(session.session_id.clone()),
        );
    }

    let prompt = build_local_agent_continuation_prompt(&snapshot, &data, new_instruction);
    let mut spec = SubAgentSpec::new(prompt);
    spec.model = Some(model);
    spec.system = data.system.clone();
    spec.tool_filter = data.allowed_tools.clone().map(ToolFilter::allow_only);
    spec.metadata = metadata;
    spec.run_in_background = start_backgrounded;
    // Deliberately NOT tied to `start_backgrounded`: this flag means
    // "the runtime has no interactive prompt surface at all", and the
    // TUI always has one. Backgroundness travels in
    // `run_in_background`; conflating the two here would disable the
    // background deletion-ask float on continuation agents.
    spec.permission_prompts_unavailable = false;
    spec.cwd = Some(session.cwd.clone());
    spec.permission_broker = Some(session.engine_half.runtime.permission_broker.clone());

    let replacement_id = handle
        .block_on(session.engine_half.sub_agent_spawner.spawn_detached(spec))
        .map_err(|err| format!("failed to start replacement agent: {err}"))?;
    if let Err(err) = stop_task(session.engine_half.tasks.as_ref(), &old_id) {
        tracing::warn!(
            task_id,
            error = %err,
            "replacement agent started after the original task had already stopped"
        );
    }
    Ok(replacement_id)
}

static LOCAL_AGENT_CONTINUATION_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_local_agent_continuation_id() -> String {
    let counter = LOCAL_AGENT_CONTINUATION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("agent-{}-cont-{counter}", rebon_types::wall_clock_ms_u128())
}

fn continuation_title(previous_title: &str, new_instruction: &str) -> String {
    let basis = new_instruction
        .lines()
        .next()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(previous_title);
    format!("Continue: {}", clamp_text(basis, 80))
}

fn build_local_agent_continuation_prompt(
    snapshot: &rebon_plugin_tasks::runtime::TaskSnapshot,
    data: &rebon_plugin_tasks::runtime::LocalAgentData,
    new_instruction: &str,
) -> String {
    let recent_transcript = recent_local_agent_transcript(data, 12_000);
    format!(
        "You are continuing an interrupted local agent task.\n\
Original task id: {task_id}\n\
Original title: {title}\n\
Original status at interruption: {status}\n\
Agent type: {agent_type}\n\n\
Original task prompt:\n{prompt}\n\n\
Recent visible transcript before interruption:\n{transcript}\n\n\
        New user instruction:\n{new_instruction}\n\n\
Continue from the current repository state. Do not repeat completed side effects unless the new instruction explicitly asks for it.",
        task_id = snapshot.id.as_str(),
        title = snapshot.title.as_str(),
        status = snapshot.status.as_str(),
        agent_type = data.agent_type.as_str(),
        prompt = clamp_text(&data.prompt, 8_000),
        transcript = if recent_transcript.trim().is_empty() {
            "(no visible transcript yet)".to_string()
        } else {
            recent_transcript
        },
        new_instruction = new_instruction.trim(),
    )
}

fn recent_local_agent_transcript(
    data: &rebon_plugin_tasks::runtime::LocalAgentData,
    limit: usize,
) -> String {
    use rebon_plugin_tasks::runtime::LocalAgentTranscriptEntry;
    let lines = data
        .transcript
        .iter()
        .filter_map(|entry| match entry {
            LocalAgentTranscriptEntry::User { text } => {
                Some(format!("User: {}", clamp_text(text, 800)))
            }
            LocalAgentTranscriptEntry::Thinking { .. } => None,
            LocalAgentTranscriptEntry::Assistant { text } => {
                Some(format!("Assistant: {}", clamp_text(text, 1_600)))
            }
            LocalAgentTranscriptEntry::ToolStart { name, activity, .. } => Some(format!(
                "Tool start [{name}]: {}",
                clamp_text(activity, 800)
            )),
            LocalAgentTranscriptEntry::ToolProgress { name, message, .. } => Some(format!(
                "Tool progress [{name}]: {}",
                clamp_text(message, 800)
            )),
            LocalAgentTranscriptEntry::ToolFinish {
                name, ok, summary, ..
            } => {
                let status = if *ok { "ok" } else { "error" };
                Some(format!(
                    "Tool finish [{name} {status}]: {}",
                    clamp_text(summary, 800)
                ))
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    clamp_tail(&lines, limit)
}

fn clamp_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max_chars).collect();
    clipped.push_str("...");
    clipped
}

fn clamp_tail(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let tail = text
        .chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("... omitted earlier transcript ...\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::{parse_agent_interrupt_redirect, recent_local_agent_transcript};
    use rebon_plugin_tasks::runtime::{LocalAgentData, LocalAgentTranscriptEntry};

    #[test]
    fn parse_agent_interrupt_redirect_accepts_command_aliases() {
        assert_eq!(
            parse_agent_interrupt_redirect("/interrupt inspect the daemon path"),
            Some("inspect the daemon path".into())
        );
        assert_eq!(
            parse_agent_interrupt_redirect("/redirect focus on install flow"),
            Some("focus on install flow".into())
        );
        assert_eq!(
            parse_agent_interrupt_redirect("/interrupt"),
            Some(String::new())
        );
        assert_eq!(
            parse_agent_interrupt_redirect("/interruption is ordinary text"),
            None
        );
    }

    #[test]
    fn continuation_context_omits_unsigned_thinking_rows() {
        let data = LocalAgentData {
            prompt: "inspect".into(),
            agent_type: "general-purpose".into(),
            model: None,
            system: None,
            allowed_tools: None,
            token_count: 0,
            tool_use_count: 0,
            transcript: vec![
                LocalAgentTranscriptEntry::Thinking {
                    text: "private reasoning".into(),
                },
                LocalAgentTranscriptEntry::Assistant {
                    text: "visible answer".into(),
                },
            ],
            streaming_text: None,
            pending_messages: Vec::new(),
            retrieved: false,
        };

        let context = recent_local_agent_transcript(&data, 12_000);

        assert!(!context.contains("private reasoning"));
        assert!(context.contains("Assistant: visible answer"));
    }
}
