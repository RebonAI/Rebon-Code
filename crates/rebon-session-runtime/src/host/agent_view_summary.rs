use crate::host::{BackgroundStore, BACKGROUND_SUMMARY_UPDATE_INTERVAL_MS};
use rebon_types::SessionUpdateParams;

pub(crate) fn should_refresh_agent_view_model_summary(
    last_refresh_ms: &mut Option<u64>,
    now_ms: u64,
) -> bool {
    let should_refresh = last_refresh_ms
        .map(|last| now_ms.saturating_sub(last) >= BACKGROUND_SUMMARY_UPDATE_INTERVAL_MS)
        .unwrap_or(true);
    if should_refresh {
        *last_refresh_ms = Some(now_ms);
    }
    should_refresh
}

pub(crate) fn background_job_agent_view_summary_input_from_store(
    store: &BackgroundStore,
    job_id: &str,
    prompt: &str,
) -> Option<String> {
    let updates = store
        .read_events_tail(job_id, 400)
        .ok()?
        .into_iter()
        .filter(|event| event.kind == "session_update")
        .filter_map(|event| serde_json::from_value::<SessionUpdateParams>(event.data).ok())
        .collect::<Vec<_>>();
    let input = rebon_api::extract_agent_view_summary_text(prompt, &updates);
    rebon_session_host::non_empty_trimmed(&input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::BackgroundRuntimeFields;
    use std::path::PathBuf;

    fn store() -> (tempfile::TempDir, BackgroundStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path().join(".rebon").join("background"));
        (dir, store)
    }

    fn runtime() -> BackgroundRuntimeFields {
        BackgroundRuntimeFields {
            provider: None,
            model: None,
            fast_mode: None,
            channels: Vec::new(),
            development_channels: Vec::new(),
            provider_format: None,
            ui_mode: None,
            effort_level: None,
            permission_mode: None,
            capability_mode: rebon_types::AgentCapabilityMode::Normal,
            settings: Vec::new(),
            add_dirs: Vec::new(),
            plugin_dirs: Vec::new(),
            mcp_configs: Vec::new(),
            strict_mcp_config: false,
        }
    }

    #[test]
    fn agent_view_summary_input_uses_prompt_and_session_updates() {
        let (_dir, store) = store();
        let mut state = store
            .create_job(
                "repair agent summaries".into(),
                PathBuf::from("."),
                runtime(),
            )
            .unwrap();
        state.process.status = crate::host::BackgroundJobStatus::Running;
        store.write_state(&state).unwrap();

        let update = rebon_types::SessionUpdateParams {
            session_id: "sess-one".into(),
            update: rebon_types::SessionUpdate::ToolCall {
                tool_call_id: "tool-1".into(),
                title: "Edit background.rs".into(),
                kind: rebon_types::ToolKind::Edit,
                status: rebon_types::ToolCallStatus::Completed,
                content: None,
                locations: None,
                raw_input: None,
                raw_output: None,
            },
        };
        store
            .append_session_update(&state.identity.job_id, &update)
            .unwrap();

        let input = background_job_agent_view_summary_input_from_store(
            &store,
            &state.identity.job_id,
            &state.identity.prompt,
        )
        .unwrap();

        assert!(input.contains("user: repair agent summaries"));
        assert!(input.contains("tool completed edit: Edit background.rs"));
    }

    #[test]
    fn model_summary_refresh_gate_uses_summary_interval() {
        let mut last = None;

        assert!(should_refresh_agent_view_model_summary(&mut last, 10_000));
        assert!(!should_refresh_agent_view_model_summary(&mut last, 20_000));
        assert!(should_refresh_agent_view_model_summary(
            &mut last,
            10_000 + BACKGROUND_SUMMARY_UPDATE_INTERVAL_MS
        ));
    }
}
