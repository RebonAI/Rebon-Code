use super::{BackgroundJobState, BackgroundStore, PendingPrompt};

/// What a respawn of `source` carries over: the session, and the queue with
/// every claim dropped.
///
/// The session only when a follow-up is owed — the respawn resumes the
/// conversation to run it. Otherwise it reruns the job's own prompt in a
/// fresh session, like any job with nothing queued.
///
/// The queue either way. With nothing owed, what waits is only messages from
/// other sessions — refused a turn, or never judged — and they still come
/// along, verdicts kept: their senders were told they were accepted, and a
/// respawned-away record is never run again, so leaving them there would lose
/// them without a word. They ride in the rerun's turn and may not lead it.
fn respawned_session_and_queue(
    source: &BackgroundJobState,
) -> (Option<String>, Vec<PendingPrompt>) {
    let session_id = source
        .has_pending_prompts_to_run()
        .then(|| source.identity.session_id.clone())
        .flatten();
    let mut pending_prompts = source.identity.pending_prompts.clone();
    for prompt in &mut pending_prompts {
        prompt.claimed_turn_generation = None;
        prompt.completed_turn_generation = None;
    }
    (session_id, pending_prompts)
}

pub(crate) fn validate_respawned_job_materialization(
    source: &BackgroundJobState,
    target: &BackgroundJobState,
    target_job_id: &str,
) -> anyhow::Result<()> {
    let (expected_session_id, expected_pending_prompts) = respawned_session_and_queue(source);
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
    let (session_id, pending_prompts) = respawned_session_and_queue(source);
    new_job.identity.session_id = session_id;
    new_job.identity.pending_prompts = pending_prompts;
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

    /// A respawn whose source holds only messages that wait — refused a turn,
    /// or never judged — owes no follow-up: it reruns the job's own prompt
    /// like any job with nothing queued, rather than resuming the session for
    /// messages that may not start a turn. The messages still come along,
    /// verdicts kept and claims dropped, to ride in that rerun: the source
    /// record is never run again, and their senders were told they arrived.
    #[test]
    fn create_respawned_job_with_only_waiting_messages_reruns_its_own_prompt() {
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
        let mut refused = crate::PendingPrompt::new(
            "m-refused".into(),
            "<teammate-message>later</teammate-message>".into(),
            Vec::new(),
            crate::now_ms(),
        )
        .unwrap();
        refused.origin = Some(crate::PendingPromptOrigin::AgentMessage {
            from_session_id: "sess-peer".into(),
            from_agent: "main".into(),
            from_handle: None,
            from_name: None,
            hops: 9,
        });
        refused.wake = rebon_tool::peer::WakeVerdict::Refused;
        let mut unjudged = refused.clone();
        unjudged.id = "m-unjudged".into();
        unjudged.wake = rebon_tool::peer::WakeVerdict::Unjudged;
        source.identity.session_id = Some("sess-respawn-waiting".into());
        source.process.turn_generation = 4;
        refused.claimed_turn_generation = Some(4);
        source.identity.pending_prompts = vec![refused, unjudged];
        source.process.status = BackgroundJobStatus::Failed;
        store.write_state(&source).unwrap();

        let respawned = create_respawned_job(&store, &source).unwrap();
        assert_eq!(respawned.identity.session_id, None);
        assert_eq!(respawned.identity.prompt, "original prompt");
        let reloaded = store.read_state(&respawned.identity.job_id).unwrap();
        for job in [&respawned, &reloaded] {
            assert_eq!(
                job.identity
                    .pending_prompts
                    .iter()
                    .map(|prompt| (
                        prompt.id.as_str(),
                        prompt.wake,
                        prompt.claimed_turn_generation
                    ))
                    .collect::<Vec<_>>(),
                [
                    ("m-refused", rebon_tool::peer::WakeVerdict::Refused, None),
                    ("m-unjudged", rebon_tool::peer::WakeVerdict::Unjudged, None)
                ],
                "the waiting messages come along"
            );
            assert!(
                !job.has_pending_prompts_to_run(),
                "and none of them may lead: the rerun is the job's own prompt"
            );
        }
        // Materialized again from the same source, the record still matches.
        let again =
            create_respawned_job_with_id(&store, &source, respawned.identity.job_id.clone())
                .unwrap();
        assert_eq!(again.identity.job_id, respawned.identity.job_id);
    }

    /// A respawn drops every claim but keeps what was decided about each
    /// message from another session: the admitted one that may already have
    /// led a turn still leads, and a refused one stays refused, in the new job
    /// and through its round trip on disk.
    #[test]
    fn create_respawned_job_keeps_each_messages_wake_verdict() {
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
        let message = |id: &str, wake| {
            let mut prompt = crate::PendingPrompt::new(
                id.into(),
                format!("<teammate-message>{id}</teammate-message>"),
                Vec::new(),
                crate::now_ms(),
            )
            .unwrap();
            prompt.origin = Some(crate::PendingPromptOrigin::AgentMessage {
                from_session_id: "sess-peer".into(),
                from_agent: "main".into(),
                from_handle: None,
                from_name: None,
                hops: 1,
            });
            prompt.wake = wake;
            prompt
        };
        source.identity.session_id = Some("sess-respawn-verdicts".into());
        source.process.turn_generation = 3;
        source.identity.pending_prompts = vec![
            message("m-admitted", rebon_tool::peer::WakeVerdict::Admitted),
            message("m-refused", rebon_tool::peer::WakeVerdict::Refused),
        ];
        source.identity.pending_prompts[0].claimed_turn_generation = Some(3);
        source.process.status = BackgroundJobStatus::Failed;
        store.write_state(&source).unwrap();

        let respawned = create_respawned_job(&store, &source).unwrap();
        let reloaded = store.read_state(&respawned.identity.job_id).unwrap();
        for job in [&respawned, &reloaded] {
            assert_eq!(
                job.identity
                    .pending_prompts
                    .iter()
                    .map(|prompt| (prompt.wake, prompt.claimed_turn_generation))
                    .collect::<Vec<_>>(),
                [
                    (rebon_tool::peer::WakeVerdict::Admitted, None),
                    (rebon_tool::peer::WakeVerdict::Refused, None)
                ]
            );
        }
    }
}
