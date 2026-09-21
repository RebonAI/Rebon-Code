use super::super::*;
use super::server::{request_error, request_refused, RequestResult};
use rebon_session_host::{HostCallError, HostReply};

pub(crate) fn send_background_question_answer(
    store: &BackgroundStore,
    job_id: &str,
    owner: &BackgroundIpcOwner,
    permission_responses: &Arc<
        Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<PermissionAnswer>>>,
    >,
    query_id: u64,
    turn_generation: u64,
    answers: Vec<rebon_session_host::ForegroundQuestionAnswer>,
) -> RequestResult {
    let mut responses = permission_responses.lock().expect("poisoned");
    if !responses.contains_key(&query_id) {
        return request_refused(format!("question query {query_id} is not pending"));
    }
    let updated_input = match store.update_state(job_id, |state| {
        owner.ensure_matches(state)?;
        let Some(pending) = state.outcome.pending_permission.as_ref() else {
            anyhow::bail!("background job has no pending permission query");
        };
        if pending.endpoint.as_ref() != Some(&owner.endpoint) {
            return Err(anyhow::Error::new(HostCallError::StaleGeneration)
                .context("question query belongs to a different IPC endpoint generation"));
        }
        if pending.query_id != query_id {
            anyhow::bail!("question query {query_id} is not pending");
        }
        if pending.turn_generation != turn_generation {
            return Err(
                anyhow::Error::new(HostCallError::StaleGeneration).context(format!(
                    "question query {query_id} belongs to a different turn generation"
                )),
            );
        }
        if !pending
            .options
            .iter()
            .any(|option| option.option_id == "allow_once")
        {
            anyhow::bail!("question query {query_id} has no allow_once option");
        }
        let updated_input = build_ask_user_question_updated_input(pending, &answers)?;
        let resumes_current_turn = pending.turn_generation == state.process.turn_generation;
        state.outcome.pending_permission = None;
        if resumes_current_turn && state.process.status == BackgroundJobStatus::NeedsInput {
            state.process.status = BackgroundJobStatus::Running;
            state.outcome.summary = Some("questions answered; continuing".to_string());
            state.outcome.summary_updated_at_ms = None;
            state.process.updated_at_ms = now_ms();
        }
        Ok(updated_input)
    }) {
        Ok(updated_input) => updated_input,
        Err(err) => {
            return Err(request_error(err));
        }
    };
    let sender = responses
        .remove(&query_id)
        .expect("question sender checked above");
    drop(responses);
    let _ = sender.send(PermissionAnswer::Selected {
        option_id: "allow_once".into(),
        updated_input: Some(updated_input),
        extra_text: None,
    });
    let _ = store.append_event(
        job_id,
        "question_answered_ipc",
        serde_json::json!({ "queryId": query_id }),
    );
    Ok(HostReply::default())
}
