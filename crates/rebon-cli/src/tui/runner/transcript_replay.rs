//! The terminal's commit shell for transcript replay: it takes the rows
//! `crate::session::transcript_replay::replayed_messages` produces and
//! commits them to an `AppState` through the reducer. Everything that
//! decides what a persisted entry looks like -- the enrichment, the skips,
//! the recovery of historical malformed entries -- lives on the session
//! side, where a caller with no screen reads the rows directly.

use std::collections::HashMap;

use crate::session::transcript_replay::replayed_messages;

#[cfg(test)]
pub(super) fn replay_transcript_entries(
    tui: &mut rebon_tui::AppState,
    entries: Vec<rebon_session::TranscriptEntry>,
) {
    let _ = replay_transcript_entries_with_agent_tasks(tui, entries);
}

pub(crate) fn replay_transcript_entries_with_agent_tasks(
    tui: &mut rebon_tui::AppState,
    entries: Vec<rebon_session::TranscriptEntry>,
) -> HashMap<String, crate::session::transcript_replay::BackgroundAgentTaskRef> {
    let (messages, agent_tasks) = replayed_messages(entries);
    for msg in messages {
        rebon_tui::reducer(tui, rebon_tui::Action::Commit(msg));
    }
    agent_tasks
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    fn assistant_tool_use_entry(
        uuid: &str,
        parent: &str,
        tool_use_id: &str,
        tool_name: &str,
    ) -> rebon_session::TranscriptEntry {
        rebon_session::TranscriptEntry {
            entry_type: "assistant".into(),
            uuid: uuid.into(),
            parent_uuid: Some(parent.into()),
            timestamp: Some("2026-04-18T00:00:00.000Z".into()),
            raw: json!({
                "type": "assistant",
                "uuid": uuid,
                "parentUuid": parent,
                "timestamp": "2026-04-18T00:00:00.000Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": tool_use_id,
                            "name": tool_name,
                            "input": {},
                        }
                    ],
                }
            }),
        }
    }

    fn user_tool_result_entry(
        uuid: &str,
        parent: &str,
        results: Vec<(&str, serde_json::Value)>,
        outputs_by_id: Option<Vec<(&str, serde_json::Value)>>,
    ) -> rebon_session::TranscriptEntry {
        let content: Vec<serde_json::Value> = results
            .into_iter()
            .map(|(id, content)| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": content,
                })
            })
            .collect();
        let mut raw = json!({
            "type": "user",
            "uuid": uuid,
            "parentUuid": parent,
            "timestamp": "2026-04-18T00:00:01.000Z",
            "message": {
                "role": "user",
                "content": content,
            }
        });
        if let Some(outputs) = outputs_by_id {
            let mut map = serde_json::Map::new();
            for (id, value) in outputs {
                map.insert(id.into(), value);
            }
            raw.as_object_mut()
                .unwrap()
                .insert("toolUseResults".into(), serde_json::Value::Object(map));
        }
        rebon_session::TranscriptEntry {
            entry_type: "user".into(),
            uuid: uuid.into(),
            parent_uuid: Some(parent.into()),
            timestamp: Some("2026-04-18T00:00:01.000Z".into()),
            raw,
        }
    }

    #[test]
    fn replay_restores_one_plan_card_for_approved_and_rejected_exit_plan_mode() {
        for (case, output) in [
            (
                "approved",
                Some(json!({
                    "exitedPlanMode": true,
                    "permissionMode": "acceptEdits",
                    "mode": "acceptEdits",
                    "clearContext": false,
                    "plan": "approved plan"
                })),
            ),
            (
                "clear",
                Some(json!({
                    "exitedPlanMode": true,
                    "permissionMode": "auto",
                    "mode": "auto",
                    "clearContext": true,
                    "plan": "approved plan"
                })),
            ),
            ("rejected", None),
        ] {
            let mut assistant =
                assistant_tool_use_entry("a-exit", "root", "tu-exit", "ExitPlanMode");
            assistant.raw["message"]["content"][0]["input"] = json!({"plan": "approved plan"});
            let outputs = output.map(|value| vec![("tu-exit", value)]);
            let tool_result = user_tool_result_entry(
                "u-exit-result",
                "a-exit",
                vec![("tu-exit", json!(format!("{case} result")))],
                outputs,
            );
            let mut tui = rebon_tui::AppState::default();

            replay_transcript_entries(&mut tui, vec![assistant, tool_result]);

            let plan_cards = tui
                .transcript
                .rows()
                .iter()
                .filter_map(|message| match message {
                    rebon_tui::Message::User(user) => user.plan_content.as_deref(),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(plan_cards, vec!["approved plan"], "case: {case}");
            assert_eq!(tui.transcript.len(), 1, "case: {case}");
        }
    }
}
