use std::path::PathBuf;

use super::{
    ensure_background_job_has_no_unfinished_follow_up, ensure_background_job_ownership_reusable,
    non_empty_trimmed, now_ms, BackgroundJobState, BackgroundJobStatus, BackgroundRuntimeFields,
    BackgroundStore,
};

pub fn mark_existing_background_session_idle(
    store: &BackgroundStore,
    job_id: &str,
    prompt: Option<String>,
    cwd: PathBuf,
    runtime: BackgroundRuntimeFields,
    session_id: String,
    name: Option<String>,
) -> anyhow::Result<BackgroundJobState> {
    let name = name.and_then(|name| non_empty_trimmed(&name));
    let cwd = cwd.to_string_lossy().to_string();
    let state = store.update_state(job_id, |state| {
        if state.identity.session_id.as_deref() != Some(session_id.as_str()) {
            anyhow::bail!("background job {job_id} does not belong to session {session_id}");
        }
        ensure_background_job_ownership_reusable(state, job_id)?;
        ensure_background_job_has_no_unfinished_follow_up(state, job_id)?;
        if matches!(
            state.process.status,
            BackgroundJobStatus::Queued
                | BackgroundJobStatus::Running
                | BackgroundJobStatus::NeedsInput
        ) && state.process.pid != Some(std::process::id())
        {
            anyhow::bail!(
                "background job {job_id} is already active while status is {}",
                state.process.status.as_str()
            );
        }
        if let Some(prompt) = prompt.clone() {
            state.identity.prompt = prompt;
        }
        state.identity.cwd = cwd.clone();
        state.identity.runtime = runtime.clone();
        if let Some(name) = name.clone() {
            state.identity.name = name;
        }
        state.process.status = BackgroundJobStatus::Idle;
        state.clear_recorded_owner();
        state.process.spawn_admitted = false;
        state.outcome.pending_permission = None;
        state.clear_pending_prompts();
        state.outcome.summary_updated_at_ms = None;
        state.outcome.exit_code = None;
        state.outcome.error = None;
        state.process.completed_at_ms = None;
        state.process.updated_at_ms = now_ms();
        Ok(state.clone())
    })?;
    store.append_event(
        job_id,
        "session_backgrounded_idle",
        serde_json::json!({
            "sessionId": session_id,
            "reused": true,
        }),
    )?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adopt_existing_background_session;
    use crate::JobPlacement;

    fn store() -> (tempfile::TempDir, BackgroundStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path());
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
    fn mark_existing_background_session_idle_reuses_job_without_duplication() {
        let (_dir, store) = store();
        let job = adopt_existing_background_session(
            &store,
            "original prompt".into(),
            PathBuf::from("."),
            runtime(),
            "sess-one".into(),
            Some("original name".into()),
            false,
            JobPlacement::Background,
            std::path::Path::new("rebon"),
        )
        .unwrap();

        let updated = mark_existing_background_session_idle(
            &store,
            &job.identity.job_id,
            None,
            PathBuf::from("."),
            runtime(),
            "sess-one".into(),
            None,
        )
        .unwrap();

        assert_eq!(updated.identity.job_id, job.identity.job_id);
        assert_eq!(updated.process.status, BackgroundJobStatus::Idle);
        assert_eq!(updated.identity.prompt, "original prompt");
        assert_eq!(updated.identity.name, "original name");
        assert_eq!(store.list_jobs().unwrap().len(), 1);
    }
}
