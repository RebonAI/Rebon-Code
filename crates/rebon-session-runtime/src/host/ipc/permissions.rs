use super::super::*;
use super::server::{request_error, request_refused, RequestResult};
use rebon_session_host::{HostCallError, HostReply};

pub(crate) struct BackgroundPermissionReceiver {
    pub(crate) turn_generation: u64,
    pub(crate) receiver: tokio::sync::mpsc::UnboundedReceiver<OutboundPermissionQuery>,
}

/// Session-scoped inputs needed to expand allow-always candidates on
/// outbound permission queries and to persist the selected rule when a
/// remote client (app/mobile) answers `allow_always` over IPC. Attached
/// once per turn alongside the permission receiver; `None` until a
/// session has been built.
#[derive(Clone)]
pub(crate) struct BackgroundPermissionRuleContext {
    pub(crate) cwd: String,
    pub(crate) policy_store: rebon_core::policy::PolicyStore,
}

pub(crate) fn send_background_permission_answer(
    store: &BackgroundStore,
    job_id: &str,
    owner: &BackgroundIpcOwner,
    permission_responses: &Arc<
        Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<PermissionAnswer>>>,
    >,
    permission_rule_context: &Arc<Mutex<Option<BackgroundPermissionRuleContext>>>,
    query_id: u64,
    turn_generation: u64,
    option_id: Option<String>,
    extra_text: Option<String>,
    updated_input: Option<serde_json::Value>,
) -> RequestResult {
    let mut responses = permission_responses.lock().expect("poisoned");
    if !responses.contains_key(&query_id) {
        return request_refused(format!("permission query {query_id} is not pending"));
    }
    let mut pending_tool: Option<(String, Option<serde_json::Value>)> = None;
    let transitioned = store.update_state(job_id, |state| {
        owner.ensure_matches(state)?;
        let Some(pending) = state.outcome.pending_permission.as_ref() else {
            anyhow::bail!("background job has no pending permission query");
        };
        if pending.endpoint.as_ref() != Some(&owner.endpoint) {
            return Err(anyhow::Error::new(HostCallError::StaleGeneration)
                .context("permission query belongs to a different IPC endpoint generation"));
        }
        if pending.query_id != query_id {
            anyhow::bail!("permission query {query_id} is not pending");
        }
        if pending.turn_generation != turn_generation {
            return Err(
                anyhow::Error::new(HostCallError::StaleGeneration).context(format!(
                    "permission query {query_id} belongs to a different turn generation"
                )),
            );
        }
        if let Some(option_id) = option_id.as_deref() {
            if !pending
                .options
                .iter()
                .any(|option| option.option_id == option_id)
            {
                anyhow::bail!("permission query {query_id} has no option {option_id}");
            }
        }
        pending_tool = pending
            .tool
            .clone()
            .map(|tool| (tool, pending.tool_input.clone()));
        let resumes_current_turn = pending.turn_generation == state.process.turn_generation;
        state.outcome.pending_permission = None;
        if resumes_current_turn && state.process.status == BackgroundJobStatus::NeedsInput {
            state.process.status = BackgroundJobStatus::Running;
            state.outcome.summary = Some("permission answered; continuing".to_string());
            state.outcome.summary_updated_at_ms = None;
            state.process.updated_at_ms = now_ms();
        }
        Ok(())
    });
    if let Err(err) = transitioned {
        return Err(request_error(err));
    }
    let sender = responses
        .remove(&query_id)
        .expect("permission sender checked above");
    drop(responses);
    // Remote allow-always answers persist the selected rule exactly
    // like the TUI confirm path: live policy store first (immediate
    // in-session effect), then `.rebon/settings.json`. The raw option
    // id picks the exact vs generalized candidate; the engine only
    // ever sees the canonical id.
    if let Some(selected) = option_id
        .as_deref()
        .filter(|selected| crate::permission_policy::is_allow_always_option_id(selected))
    {
        let rule_context = permission_rule_context.lock().expect("poisoned").clone();
        match (rule_context, pending_tool.as_ref()) {
            (Some(context), Some((tool_name, tool_input))) => {
                crate::permission_policy::persist_allow_always_rule(
                    tool_name,
                    tool_input.as_ref(),
                    &context.policy_store,
                    &context.cwd,
                    Some(selected),
                );
            }
            _ => {
                tracing::warn!(
                    job_id,
                    query_id,
                    "allow_always answered without rule context; rule not persisted"
                );
            }
        }
    }
    let answer = match crate::permission_policy::canonical_permission_option_id(option_id) {
        Some(option_id) => PermissionAnswer::Selected {
            option_id,
            updated_input,
            extra_text,
        },
        None => PermissionAnswer::Cancelled,
    };
    let _ = sender.send(answer);
    let _ = store.append_event(
        job_id,
        "permission_answered_ipc",
        serde_json::json!({ "queryId": query_id }),
    );
    Ok(HostReply::default())
}
