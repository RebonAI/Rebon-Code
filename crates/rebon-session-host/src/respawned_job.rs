use super::{BackgroundJobState, BackgroundStore};

pub(crate) fn validate_respawned_job_materialization(
    source: &BackgroundJobState,
    target: &BackgroundJobState,
    target_job_id: &str,
) -> anyhow::Result<()> {
    let mut expected_pending_prompts = source.identity.pending_prompts.clone();
    for prompt in &mut expected_pending_prompts {
        prompt.claimed_turn_generation = None;
        prompt.completed_turn_generation = None;
    }
    let expected_session_id = source
        .has_pending_prompts()
        .then(|| source.identity.session_id.clone())
        .flatten();
    if target.identity.job_id != target_job_id
        || target.identity.prompt != source.identity.prompt
        || target.identity.prompt_images != source.identity.prompt_images
        || target.identity.pending_prompts != expected_pending_prompts
        || target.identity.session_id != expected_session_id
        || target.identity.cwd != source.identity.cwd
        || target.identity.runtime != source.identity.runtime
        || target.identity.agent_type != source.identity.agent_type
        || target.workspace.isolate_in_worktree != source.workspace.isolate_in_worktree
        || target.workspace.require_worktree != source.workspace.require_worktree
        || target.workspace.preserve_worktree_on_success
            != source.workspace.preserve_worktree_on_success
        || target.identity.queue_session != source.identity.queue_session
    {
        anyhow::bail!("background respawn target {target_job_id} does not contain the source work");
    }
    Ok(())
}

#[cfg(test)]
fn create_respawned_job(
    store: &BackgroundStore,
    source: &BackgroundJobState,
) -> anyhow::Result<BackgroundJobState> {
    create_respawned_job_with_id(store, source, super::generate_job_id())
}

pub(crate) fn create_respawned_job_with_id(
    store: &BackgroundStore,
    source: &BackgroundJobState,
    job_id: String,
) -> anyhow::Result<BackgroundJobState> {
    if store.state_path(&job_id).exists() {
        let target = store.read_state(&job_id)?;
        validate_respawned_job_materialization(source, &target, &job_id)?;
        return Ok(target);
    }
    let mut new_job = BackgroundJobState::new(
        source.identity.prompt.clone(),
        source.identity.cwd.clone(),
        source.identity.runtime.clone(),
        Some(source.identity.name.clone()),
    );
    new_job.identity.job_id = job_id;
    new_job.identity.prompt_images = source.identity.prompt_images.clone();
    if source.has_pending_prompts() {
        new_job.identity.session_id = source.identity.session_id.clone();
        new_job.identity.pending_prompts = source.identity.pending_prompts.clone();
        for prompt in &mut new_job.identity.pending_prompts {
            prompt.claimed_turn_generation = None;
            prompt.completed_turn_generation = None;
        }
    }
    new_job.identity.agent_type = source.identity.agent_type.clone();
    new_job.workspace.isolate_in_worktree = source.workspace.isolate_in_worktree;
    new_job.workspace.require_worktree = source.workspace.require_worktree;
    new_job.workspace.preserve_worktree_on_success = source.workspace.preserve_worktree_on_success;
    // Queue authorization rides the respawn chain: dropping it here silently
    // demoted every queue supervisor to a coordinator without queue tools the
    // first time its worker died.
    new_job.identity.queue_session = source.identity.queue_session;
    store.write_state(&new_job)?;
    store.append_event(
        &new_job.identity.job_id,
        "created",
        serde_json::json!({
            "cwd": new_job.identity.cwd,
            "name": new_job.identity.name,
            "imageCount": new_job.identity.prompt_images.len(),
        }),
    )?;
    store.read_state(&new_job.identity.job_id)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{BackgroundJobStatus, BackgroundRuntimeFields};

    fn store() -> (tempfile::TempDir, BackgroundStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BackgroundStore::new(dir.path().join("background"));
        (dir, store)
    }

    #[test]
    fn create_respawned_job_keeps_source_cwd_not_worktree_path() {
        let (_dir, store) = store();
        let mut source = store
            .create_job(
                "original prompt".into(),
                PathBuf::from("/repo/project"),
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
                },
            )
            .unwrap();
        source.workspace.worktree_path = Some("/repo/.rebon/worktrees/bg-original".into());
        source.workspace.isolate_in_worktree = true;
        source.workspace.require_worktree = true;
        source.workspace.preserve_worktree_on_success = true;
        source.identity.queue_session = true;
        source.identity.agent_type = Some("code-reviewer".into());
        source.process.status = BackgroundJobStatus::Succeeded;
        store.write_state(&source).unwrap();

        let respawned = create_respawned_job(&store, &source).unwrap();

        assert_eq!(respawned.identity.cwd, "/repo/project");
        assert_eq!(respawned.workspace.worktree_path, None);
        assert!(respawned.workspace.isolate_in_worktree);
        assert!(respawned.workspace.require_worktree);
        assert!(respawned.workspace.preserve_worktree_on_success);
        assert!(
            respawned.identity.queue_session,
            "queue authorization must survive a respawn"
        );
        assert_eq!(
            respawned.identity.agent_type.as_deref(),
            Some("code-reviewer")
        );
    }

    #[test]
    fn create_respawned_job_reruns_pending_followup_with_its_images() {
        let (_dir, store) = store();
        let mut source = store
            .create_job(
                "original prompt".into(),
                PathBuf::from("/repo/project"),
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
                },
            )
            .unwrap();
        source.identity.prompt_images = vec![crate::BackgroundImageAttachment {
            id: 1,
            data: "initial-image".into(),
            media_type: "image/png".into(),
            filename: None,
            source_path: None,
        }];
        let followup_image = crate::BackgroundImageAttachment {
            id: 2,
            data: "followup-image".into(),
            media_type: "image/jpeg".into(),
            filename: None,
            source_path: None,
        };
        source.identity.session_id = Some("sess-followup".into());
        source.identity.pending_prompts = vec![crate::PendingPrompt::new(
            "pp-followup".into(),
            "queued reply".into(),
            vec![followup_image.clone()],
            crate::now_ms(),
        )
        .unwrap()];
        source.process.turn_generation = 3;
        source.identity.pending_prompts[0].claimed_turn_generation = Some(3);
        source.identity.pending_prompts[0].completed_turn_generation = Some(2);
        source.process.status = BackgroundJobStatus::Failed;
        store.write_state(&source).unwrap();

        let respawned = create_respawned_job(&store, &source).unwrap();

        assert_eq!(respawned.identity.prompt, "original prompt");
        assert_eq!(
            respawned.identity.prompt_images,
            source.identity.prompt_images
        );
        assert_eq!(
            respawned.identity.session_id.as_deref(),
            Some("sess-followup")
        );
        assert_eq!(respawned.identity.pending_prompts.len(), 1);
        assert_eq!(
            respawned.identity.pending_prompts[0].id,
            source.identity.pending_prompts[0].id
        );
        assert_eq!(respawned.identity.pending_prompts[0].text, "queued reply");
        assert_eq!(
            respawned.identity.pending_prompts[0].images,
            vec![followup_image]
        );
        assert_eq!(
            respawned.identity.pending_prompts[0].claimed_turn_generation,
            None
        );
        assert_eq!(
            respawned.identity.pending_prompts[0].completed_turn_generation,
            None
        );
    }
}
