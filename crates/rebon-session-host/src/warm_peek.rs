use std::path::Path;

use super::{
    ensure_background_job_ownership_reusable, ensure_supervisor_running, now_ms,
    BackgroundJobStatus, BackgroundStore,
};

pub fn warm_background_job_for_peek_in_store(
    store: &BackgroundStore,
    job_id: &str,
    start_supervisor: bool,
    rebon_exe_path: &Path,
) -> anyhow::Result<bool> {
    let mut observed = store.read_state(job_id)?;
    store.reconcile_stale_pid(&mut observed)?;
    let queued = store.update_state(job_id, |state| {
        ensure_background_job_ownership_reusable(state, job_id)?;
        if state.identity.session_id.is_none()
            || state.process.pid.is_some()
            || state.process.ipc_port.is_some()
            || state.process.ipc_token.is_some()
            || state.outcome.pending_permission.is_some()
            || state.has_pending_prompts()
            || !matches!(
                state.process.status,
                BackgroundJobStatus::Idle
                    | BackgroundJobStatus::Succeeded
                    | BackgroundJobStatus::Failed
                    // A stopped job's session is still a session; attaching
                    // to it means "continue this", and a worker is the only
                    // place that happens now.
                    | BackgroundJobStatus::Stopped
            )
        {
            return Ok(false);
        }
        state.process.status = BackgroundJobStatus::Queued;
        state.identity.resume_only = true;
        state.outcome.summary_updated_at_ms = None;
        state.process.completed_at_ms = None;
        state.outcome.exit_code = None;
        state.outcome.error = None;
        state.process.updated_at_ms = now_ms();
        Ok(true)
    })?;
    if !queued {
        return Ok(false);
    }
    if start_supervisor {
        ensure_supervisor_running(store, rebon_exe_path)?;
        store.append_event(
            job_id,
            "peek_warm_queued",
            serde_json::json!({ "resumed": true }),
        )?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BackgroundRuntimeFields;
    use std::path::PathBuf;

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

    fn pending_prompt() -> crate::PendingPrompt {
        crate::PendingPrompt::new(
            "pp-warm-peek".into(),
            "accepted follow-up".into(),
            vec![crate::BackgroundImageAttachment {
                id: 1,
                data: "accepted-image".into(),
                media_type: "image/png".into(),
                filename: None,
                source_path: None,
            }],
            crate::now_ms(),
        )
        .unwrap()
    }

    #[test]
    fn warm_peek_queues_resume_only_worker_for_exited_job() {
        let (_dir, store) = store();
        let mut state = store
            .create_job("inspect".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Succeeded;
        state.identity.session_id = Some("sess-warm".into());
        state.process.completed_at_ms = Some(now_ms());
        store.write_state(&state).unwrap();

        assert!(warm_background_job_for_peek_in_store(
            &store,
            &state.identity.job_id,
            false,
            Path::new("rebon")
        )
        .unwrap());
        let loaded = store.read_state(&state.identity.job_id).unwrap();
        assert_eq!(loaded.process.status, BackgroundJobStatus::Queued);
        assert!(loaded.identity.resume_only);
        assert!(loaded.identity.pending_prompts.is_empty());
        assert!(loaded.process.completed_at_ms.is_none());
    }

    #[test]
    fn warm_peek_rejects_fenced_owner_and_preserves_accepted_follow_up() {
        let (_dir, store) = store();
        let mut state = store
            .create_job("inspect".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Failed;
        state.identity.session_id = Some("sess-fenced".into());
        state.process.pid = Some(std::process::id());
        state.process.pid_identity = crate::process_identity(std::process::id());
        state.process.process_owner_fenced = true;
        state.identity.pending_prompts = vec![pending_prompt()];
        store.write_state(&state).unwrap();
        let expected = store.read_state(&state.identity.job_id).unwrap();

        let error = warm_background_job_for_peek_in_store(
            &store,
            &state.identity.job_id,
            false,
            Path::new("rebon"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("ownership is fenced"));
        assert_eq!(store.read_state(&state.identity.job_id).unwrap(), expected);
    }

    #[test]
    fn warm_peek_skips_unfinished_accepted_follow_up_without_mutation() {
        let (_dir, store) = store();
        let mut state = store
            .create_job("inspect".into(), PathBuf::from("."), runtime())
            .unwrap();
        state.process.status = BackgroundJobStatus::Failed;
        state.identity.session_id = Some("sess-follow-up".into());
        state.identity.pending_prompts = vec![pending_prompt()];
        store.write_state(&state).unwrap();
        let expected = store.read_state(&state.identity.job_id).unwrap();

        assert!(!warm_background_job_for_peek_in_store(
            &store,
            &state.identity.job_id,
            false,
            Path::new("rebon"),
        )
        .unwrap());
        assert_eq!(store.read_state(&state.identity.job_id).unwrap(), expected);
    }

    #[test]
    fn warm_peek_skips_live_or_stopped_jobs() {
        let (_dir, store) = store();
        let mut live = store
            .create_job("live".into(), PathBuf::from("."), runtime())
            .unwrap();
        live.process.status = BackgroundJobStatus::Succeeded;
        live.identity.session_id = Some("sess-live".into());
        live.process.pid = Some(std::process::id());
        store.write_state(&live).unwrap();
        assert!(!warm_background_job_for_peek_in_store(
            &store,
            &live.identity.job_id,
            false,
            Path::new("rebon")
        )
        .unwrap());

        // A stopped job can be warmed again: attaching to it is how its
        // session continues, and that takes a worker.
        let mut stopped = store
            .create_job("stopped".into(), PathBuf::from("."), runtime())
            .unwrap();
        stopped.process.status = BackgroundJobStatus::Stopped;
        stopped.identity.session_id = Some("sess-stopped".into());
        store.write_state(&stopped).unwrap();
        assert!(warm_background_job_for_peek_in_store(
            &store,
            &stopped.identity.job_id,
            false,
            Path::new("rebon")
        )
        .unwrap());
        assert_eq!(
            store
                .read_state(&stopped.identity.job_id)
                .unwrap()
                .process
                .status,
            BackgroundJobStatus::Queued
        );
    }
}
