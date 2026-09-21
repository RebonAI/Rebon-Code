//! Permission-modal runtime: drains inbound [`OutboundPermissionQuery`]
//! events into a [`PendingPermission`], builds the modal view (including
//! tool-specific kinds like Plan-mode, ExitPlanMode, AskUserQuestion,
//! and allow-always Bash rules), and dispatches modal-key actions back
//! into the policy store and engine session.
//!
//! Companion to [`crate::tui::permission_modal`], which owns the data
//! structures and rendering; this submodule owns the runner-side state
//! transitions.

use std::collections::HashMap;

use ratatui::crossterm::event::KeyEvent;
use serde_json::Value;

use rebon_core::permission::{OutboundPermissionQuery, PermissionOptionKind};
use rebon_permissions::PermissionMode;
use rebon_plugin_profile::{PROFILE_SAVE_TOOL_NAME, PROFILE_SWITCH_TOOL_NAME};

use crate::tui::app::AppState;
use crate::tui::event::{
    cursor_end, cursor_home, cursor_left, cursor_right, splice_backspace, splice_char_str,
    splice_delete, translate_key, KeyAction,
};
use crate::tui::permission_modal::{
    build_ask_user_updated_input, parse_ask_user_questions, permission_title, translate_modal_key,
    AskUserQuestionAnswer, PendingPermission, PermissionKind, PermissionModalAction,
    PermissionOptionView, ProfileProposal, ProfileProposalAction, WorkflowReviewMode,
    WorkflowReviewState,
};
use crate::tui::wiring::TuiEngineSession;

use super::permission_mode::{
    apply_cycle_permission_mode, record_background_permission_mode_acceptance,
};
use super::ultraplan::{schedule_ultraplan_ceo_submit, schedule_ultraplan_ultrawork_submit};
use crate::session::permission_answer::{
    answer_permission, build_permission_answer_with_extra_text, build_permission_answer_with_input,
    PermissionChoice,
};
use crate::session::permission_policy::{
    add_allow_always_candidates, ULTRAPLAN_CEO_OPTION_ID, ULTRAPLAN_ULTRAWORK_OPTION_ID,
};
use crate::session::workflow_review::{
    build_workflow_revision_feedback, workflow_review_graph_from_message,
    workflow_review_summary_from_metadata,
};
use rebon_types::RunPhase;

use crate::session::ultraplan_run::{
    build_ultraplan_rejection_extra_text, maybe_gate_ultraplan_exit_plan_mode,
    persist_ultraplan_ask_user_question_gate, persist_ultraplan_phase,
    persist_ultraplan_rejection_feedback, ExitPlanGateOutcome,
};

pub(super) const ULTRAPLAN_CEO_LABEL: &str = "Yes, continue with CEO mode";
pub(super) const ULTRAPLAN_ULTRAWORK_LABEL: &str = "Yes, execute with ultrawork";

pub(super) fn drain_pending_permissions(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
) -> usize {
    use tokio::sync::mpsc::error::TryRecvError;

    if pending_permission.is_some() {
        return 0;
    }

    let cwd = session.cwd.clone();
    match session.engine_half.permission_rx.try_recv() {
        Ok(mut outbound) => {
            add_allow_always_candidates(&mut outbound, &cwd);
            tracing::info!(
                target: "stream_dbg",
                call_id = %outbound.tool_call_id,
                tool = %outbound.tool_name,
                overlay_blocks = app.rebon_tui.overlay.blocks.len(),
                "tui: permission inbound"
            );
            let task_snapshots = app.task_snapshots();
            let outbound = match maybe_gate_ultraplan_exit_plan_mode(
                &mut app.ultraplan_status,
                &task_snapshots,
                session,
                outbound,
            ) {
                ExitPlanGateOutcome::Proceed(outbound) => outbound,
                ExitPlanGateOutcome::Reject { outbound, feedback } => {
                    send_ultraplan_gate_rejection(app, session, outbound, feedback);
                    return 1;
                }
            };
            // Resolves the named profile against the store and diffs it
            // against this session, or refuses the call outright. Runs here
            // rather than in `build_pending_permission` because that one has
            // no session to compare against.
            let Some(outbound) =
                super::profile_proposal::resolve_profile_proposal(app, session, outbound)
            else {
                return 1;
            };
            *pending_permission = Some(build_pending_permission(app, outbound));
            // A freshly arrived permission view renders as a scrollable suffix
            // below the transcript (it cannot flow to native scrollback).
            // Anchor it to the bottom so the action options are visible
            // immediately; PageUp/PageDown (handled in the event loop) then
            // scroll up into the body — e.g. an ExitPlanMode plan taller than
            // the viewport.
            app.follow_transcript_tail = true;
            1
        }
        Err(TryRecvError::Empty | TryRecvError::Disconnected) => 0,
    }
}

fn send_ultraplan_gate_rejection(
    app: &mut AppState,
    _session: &TuiEngineSession,
    outbound: OutboundPermissionQuery,
    feedback: String,
) {
    let mut feedback = feedback;
    feedback.push_str("\n\nDo not ask for final approval yet. Continue from the current RunState and call ExitPlanMode again only when the stated blocker is resolved.");
    let answer =
        build_permission_answer_with_extra_text(Some("reject_once".into()), Some(feedback));
    let _ = outbound.response_tx.send(answer);
    app.pending_permission_view = None;
}

pub(super) fn build_pending_permission(
    app: &AppState,
    outbound: OutboundPermissionQuery,
) -> PendingPermission {
    let tool_call_id = outbound.tool_call_id.clone();
    let tool_name = if outbound.tool_name.is_empty() {
        app.rebon_tui
            .overlay
            .find_tool_use(&tool_call_id)
            .map(|tool| tool.tool_name.clone())
            .unwrap_or_else(|| "Tool".into())
    } else {
        outbound.tool_name.clone()
    };

    // Build tool-specific permission kind.
    let kind = match tool_name.as_str() {
        "Workflow" | "RunWorkflow" => PermissionKind::WorkflowReview(
            outbound
                .metadata
                .as_ref()
                .and_then(rebon_tools_core::WorkflowGraph::from_permission_metadata)
                .or_else(|| workflow_review_graph_from_message(&outbound.message))
                .map(WorkflowReviewState::new)
                .unwrap_or_else(|| {
                    WorkflowReviewState::new(rebon_tools_core::WorkflowGraph::default())
                }),
        ),
        "EnterPlanMode" => PermissionKind::EnterPlanMode,
        "AskUserQuestion" => {
            let input = outbound
                .tool_input
                .clone()
                .unwrap_or(serde_json::Value::Null);
            let questions = parse_ask_user_questions(&input);
            let answer_count = questions.len();
            PermissionKind::AskUserQuestion {
                questions,
                answers: (0..answer_count)
                    .map(|_| AskUserQuestionAnswer::new())
                    .collect(),
                active_question: 0,
                confirmation_active: false,
                confirmation_selected: 0,
                original_input: input,
            }
        }
        "ExitPlanMode" => {
            let plan = outbound
                .tool_input
                .as_ref()
                .and_then(|v| v.get("plan"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            PermissionKind::ExitPlanMode { plan }
        }
        // The proposal was resolved by the gate above; falling through to
        // Generic here would show the model's own words in place of the diff.
        PROFILE_SWITCH_TOOL_NAME | PROFILE_SAVE_TOOL_NAME => outbound
            .metadata
            .as_ref()
            .and_then(ProfileProposal::from_permission_metadata)
            .map(PermissionKind::Profile)
            .unwrap_or(PermissionKind::Generic),
        _ => PermissionKind::Generic,
    };

    // Build title and summary based on tool kind.
    let (title, summary) = match &kind {
        PermissionKind::EnterPlanMode => (
            "Enter plan mode?".to_string(),
            "Switch to plan mode for exploration and design before making changes?".to_string(),
        ),
        PermissionKind::AskUserQuestion { questions, .. } => {
            let q_count = questions.len();
            let title = questions
                .first()
                .map(|question| question.header.trim())
                .filter(|header| !header.is_empty())
                .unwrap_or("Question")
                .to_string();
            (
                title,
                format!("Rebon is asking {q_count} question(s) for your input."),
            )
        }
        PermissionKind::ExitPlanMode { .. } => (
            "Plan ready for review".to_string(),
            "Review the proposed plan and choose how to proceed.".to_string(),
        ),
        PermissionKind::Profile(proposal) => match proposal.action {
            ProfileProposalAction::Switch => (
                format!("Switch to profile \"{}\"?", proposal.label),
                "Rebon is asking to change how this session runs.".to_string(),
            ),
            ProfileProposalAction::Save => (
                format!("Save profile \"{}\"?", proposal.label),
                "Rebon is asking to save a profile you can switch to later.".to_string(),
            ),
        },
        PermissionKind::WorkflowReview(_) => (
            "Review workflow before running".to_string(),
            workflow_review_summary_from_metadata(outbound.metadata.as_ref()).unwrap_or_else(
                || {
                    if outbound.message.trim().is_empty() {
                        "Workflow review unavailable.".to_string()
                    } else {
                        outbound.message.clone()
                    }
                },
            ),
        ),
        PermissionKind::Generic => {
            let display_tool_name = if tool_name == "InvokeDeferredTool" {
                outbound
                    .tool_input
                    .as_ref()
                    .and_then(|v| v.get("tool_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&tool_name)
                    .to_string()
            } else {
                tool_name.clone()
            };
            let summary = tool_summary_for_permission(app, &tool_call_id)
                .or_else(|| {
                    let obj = outbound.tool_input.as_ref()?.as_object()?;
                    let map: std::collections::HashMap<String, serde_json::Value> =
                        obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    let s = compact_json_map(&map);
                    if s.is_empty() {
                        Some(display_tool_name.clone())
                    } else {
                        Some(format!("{display_tool_name}({s})"))
                    }
                })
                .unwrap_or_else(|| {
                    if outbound.message.trim().is_empty() {
                        display_tool_name.clone()
                    } else {
                        outbound.message.clone()
                    }
                });
            (permission_title(&display_tool_name), summary)
        }
    };

    let mut pending = PendingPermission::from_query(outbound, title, summary, kind);

    // Override option labels for tool-specific dialogs.
    match &pending.view.kind {
        PermissionKind::EnterPlanMode => {
            if let Some(opt) = pending.view.options.get_mut(0) {
                opt.label = "Yes, enter plan mode".into();
            }
            if let Some(opt) = pending.view.options.get_mut(1) {
                opt.label = "No, start implementing now".into();
            }
        }
        PermissionKind::ExitPlanMode { .. } if app.ultraplan_status.is_some() => {
            let insert_at = pending.view.options.len().saturating_sub(1);
            if !pending
                .view
                .options
                .iter()
                .any(|option| option.option_id == ULTRAPLAN_ULTRAWORK_OPTION_ID)
            {
                pending.view.options.insert(
                    insert_at,
                    PermissionOptionView {
                        option_id: ULTRAPLAN_ULTRAWORK_OPTION_ID.into(),
                        label: ULTRAPLAN_ULTRAWORK_LABEL.into(),
                        kind: PermissionOptionKind::AllowOnce,
                    },
                );
            }
            if !pending
                .view
                .options
                .iter()
                .any(|option| option.option_id == ULTRAPLAN_CEO_OPTION_ID)
            {
                pending.view.options.insert(
                    insert_at,
                    PermissionOptionView {
                        option_id: ULTRAPLAN_CEO_OPTION_ID.into(),
                        label: ULTRAPLAN_CEO_LABEL.into(),
                        kind: PermissionOptionKind::AllowOnce,
                    },
                );
            }
        }
        _ => {}
    }

    pending
}

pub(super) fn tool_summary_for_permission(app: &AppState, tool_call_id: &str) -> Option<String> {
    let tool = app.rebon_tui.overlay.find_tool_use(tool_call_id)?;
    let input = tool.raw_input.as_ref();

    let (display_name, summary) = match tool.tool_name.as_str() {
        "InvokeDeferredTool" => {
            let inner_name = input
                .and_then(|m| m.get("tool_name"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("InvokeDeferredTool")
                .to_string();
            let inner_summary = input
                .and_then(|m| m.get("arguments"))
                .and_then(serde_json::Value::as_object)
                .map(|args| {
                    let hash: HashMap<String, serde_json::Value> =
                        args.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    compact_json_map(&hash)
                })
                .filter(|s| !s.is_empty())
                .or_else(|| tool.title.clone())
                .unwrap_or_default();
            (inner_name, inner_summary)
        }
        "SendMessage" => {
            let to = get_str_field(input, "to").unwrap_or_default();
            let preview = get_str_field(input, "summary")
                .or_else(|| get_str_field(input, "message"))
                .unwrap_or_default();
            let summary = if preview.is_empty() {
                format!("\u{2192} {to}")
            } else {
                format!("\u{2192} {to}: {preview}")
            };
            (tool.tool_name.clone(), summary)
        }
        "ResolveEscalation" => {
            let agent = get_str_field(input, "agent_id").unwrap_or_default();
            let answer = get_str_field(input, "answer").unwrap_or_default();
            let summary = if answer.is_empty() {
                format!("\u{2192} {agent}")
            } else {
                format!("\u{2192} {agent}: {answer}")
            };
            (tool.tool_name.clone(), summary)
        }
        _ => {
            let summary = input
                .map(compact_json_map)
                .filter(|s| !s.is_empty())
                .or_else(|| tool.title.clone())
                .unwrap_or_default();
            (tool.tool_name.clone(), summary)
        }
    };

    if summary.is_empty() {
        Some(display_name)
    } else {
        Some(format!("{display_name}({summary})"))
    }
}

fn get_str_field(input: Option<&HashMap<String, serde_json::Value>>, key: &str) -> Option<String> {
    input?
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn compact_json_map(map: &HashMap<String, Value>) -> String {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by(|(ka, _), (kb, _)| ka.cmp(kb));
    entries
        .into_iter()
        .map(|(k, v)| format!("{k}={}", compact_json_value(v)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn compact_json_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    }
}

pub(super) fn sync_permission_view(
    app: &mut AppState,
    pending_permission: &Option<PendingPermission>,
) {
    app.pending_permission_view = pending_permission
        .as_ref()
        .map(|pending| pending.view.clone());
}

pub(super) fn maybe_handle_permission_key(
    app: &mut AppState,
    session: &TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
    key: &KeyEvent,
    policy_store: &rebon_core::policy::PolicyStore,
    cwd: &str,
    stream_in_flight: bool,
) -> bool {
    if pending_permission.is_none() {
        return false;
    }

    if matches!(translate_key(key, app), KeyAction::CyclePermissionMode) {
        apply_cycle_permission_mode(app, session, stream_in_flight);
        return true;
    }

    let action = translate_modal_key(key);
    apply_permission_modal_action_for_session(
        app,
        session,
        pending_permission,
        action,
        policy_store,
        cwd,
    );
    true
}

#[cfg(test)]
pub(super) fn apply_permission_modal_action(
    app: &mut AppState,
    pending_permission: &mut Option<PendingPermission>,
    action: PermissionModalAction,
    policy_store: &rebon_core::policy::PolicyStore,
    cwd: &str,
) {
    apply_permission_modal_action_inner(app, None, pending_permission, action, policy_store, cwd);
}

pub(super) fn apply_permission_modal_action_for_session(
    app: &mut AppState,
    session: &TuiEngineSession,
    pending_permission: &mut Option<PendingPermission>,
    action: PermissionModalAction,
    policy_store: &rebon_core::policy::PolicyStore,
    cwd: &str,
) {
    apply_permission_modal_action_inner(
        app,
        Some(session),
        pending_permission,
        action,
        policy_store,
        cwd,
    );
}

/// Submit an AskUserQuestion permission when every question has an answer, sending
/// `allow_once` with the rebuilt `updated_input` (answers + annotations). Returns
/// `true` when it submitted (and cleared the pending permission), `false` when not
/// all questions are answered yet. Shared by the local modal flow (the
/// `submit_if_ready` closure) and the foreground command mailbox so a remote
/// answer is byte-for-byte identical to a local one (including the ultraplan gate).
pub(super) fn submit_ask_user_question_if_ready(
    app: &mut AppState,
    session: Option<&TuiEngineSession>,
    pending_permission: &mut Option<PendingPermission>,
) -> bool {
    let Some(pending) = pending_permission.as_mut() else {
        return false;
    };
    let PermissionKind::AskUserQuestion {
        ref questions,
        ref answers,
        ref original_input,
        ..
    } = pending.view.kind
    else {
        return false;
    };
    if !questions
        .iter()
        .zip(answers.iter())
        .all(|(question, answer)| answer.has_answer(question))
    {
        sync_permission_view(app, pending_permission);
        return false;
    }
    let updated_input = build_ask_user_updated_input(original_input, questions, answers);
    if let Some(message) = persist_ultraplan_ask_user_question_gate(
        &mut app.ultraplan_status,
        session.map(|s| &s.session),
        &updated_input,
    ) {
        super::inject_system_message(app, "local_command", &message);
        app.follow_transcript_tail = true;
    }
    let answer = build_permission_answer_with_input(Some("allow_once".into()), Some(updated_input));
    let Some(pending) = pending_permission.take() else {
        return true;
    };
    let _ = pending.outbound.response_tx.send(answer);
    app.pending_permission_view = None;
    true
}

/// The workflow-review dialog's own key handling: editing a node,
/// walking the graph, the three-state Tab cycle, and the revision that a
/// reject with pending edits turns into. Returns true when the key
/// belonged to the review and nothing below should see it.
fn apply_workflow_review_action(
    app: &mut AppState,
    pending_permission: &mut Option<PendingPermission>,
    action: PermissionModalAction,
) -> bool {
    let Some(pending) = pending_permission.as_mut() else {
        return true;
    };
    if !matches!(
        &pending.view.kind,
        PermissionKind::WorkflowReview(review) if review.has_nodes()
    ) {
        return false;
    }

    let editing = matches!(
        &pending.view.kind,
        PermissionKind::WorkflowReview(review) if review.mode == WorkflowReviewMode::Edit
    );
    if editing {
        if let PermissionKind::WorkflowReview(review) = &mut pending.view.kind {
            match action {
                PermissionModalAction::TypeChar(ch) => review.insert_char(ch),
                PermissionModalAction::Toggle => review.insert_char(' '),
                PermissionModalAction::Backspace => review.backspace(),
                PermissionModalAction::InsertNewline => review.insert_newline(),
                PermissionModalAction::Confirm | PermissionModalAction::SaveEdit => {
                    review.save_edit()
                }
                PermissionModalAction::Cancel => review.cancel_edit(),
                PermissionModalAction::None
                | PermissionModalAction::MovePrev
                | PermissionModalAction::MoveNext
                | PermissionModalAction::MoveTabPrev
                | PermissionModalAction::MoveTabNext
                | PermissionModalAction::FocusExtraText
                | PermissionModalAction::Delete
                | PermissionModalAction::CursorHome
                | PermissionModalAction::CursorEnd => {}
            }
        }
        sync_permission_view(app, pending_permission);
        return true;
    }

    match action {
        PermissionModalAction::SaveEdit | PermissionModalAction::InsertNewline => {
            sync_permission_view(app, pending_permission);
            true
        }
        PermissionModalAction::MovePrev | PermissionModalAction::MoveTabPrev => {
            pending.view.move_prev();
            sync_permission_view(app, pending_permission);
            true
        }
        PermissionModalAction::MoveNext | PermissionModalAction::MoveTabNext => {
            pending.view.move_next();
            sync_permission_view(app, pending_permission);
            true
        }
        PermissionModalAction::FocusExtraText => {
            // Three-state Tab cycle, matching the on-screen hint
            // "Tab: graph/actions/note": graph → actions (arrow keys
            // pick an option, typed keys are ignored) → note (typed
            // keys append text) → back to graph.
            if let PermissionKind::WorkflowReview(review) = &mut pending.view.kind {
                if pending.view.extra_text_focused {
                    pending.view.extra_text_focused = false;
                    review.actions_focused = false;
                } else if review.actions_focused {
                    pending.view.extra_text_focused = true;
                } else {
                    review.actions_focused = true;
                }
            }
            sync_permission_view(app, pending_permission);
            true
        }
        PermissionModalAction::Confirm => {
            let action_selected = match &pending.view.kind {
                PermissionKind::WorkflowReview(review) => {
                    review.actions_focused || pending.view.extra_text_focused
                }
                _ => false,
            };
            if !action_selected {
                if let PermissionKind::WorkflowReview(review) = &mut pending.view.kind {
                    review.enter_edit();
                }
                sync_permission_view(app, pending_permission);
                return true;
            }

            let selected = pending.view.selected_option().cloned();
            let revision_requested = matches!(
                selected.as_ref().map(|option| option.kind),
                Some(PermissionOptionKind::RejectOnce)
            ) && matches!(
                &pending.view.kind,
                PermissionKind::WorkflowReview(review) if review.has_pending_edits()
            );
            if revision_requested {
                let feedback = match &pending.view.kind {
                    PermissionKind::WorkflowReview(review) => {
                        build_workflow_revision_feedback(&review.graph)
                    }
                    _ => String::new(),
                };
                let answer = build_permission_answer_with_extra_text(
                    selected.map(|option| option.option_id),
                    Some(feedback),
                );
                let Some(pending) = pending_permission.take() else {
                    return true;
                };
                let _ = pending.outbound.response_tx.send(answer);
                app.pending_permission_view = None;
                return true;
            }
            false
        }
        PermissionModalAction::TypeChar(ch) => {
            if pending.view.extra_text_focused && !ch.is_control() {
                pending.view.extra_text.push(ch);
            }
            sync_permission_view(app, pending_permission);
            true
        }
        PermissionModalAction::Backspace => {
            if pending.view.extra_text_focused {
                pending.view.extra_text.pop();
            }
            sync_permission_view(app, pending_permission);
            true
        }
        PermissionModalAction::Delete
        | PermissionModalAction::CursorHome
        | PermissionModalAction::CursorEnd => {
            sync_permission_view(app, pending_permission);
            true
        }
        PermissionModalAction::Toggle => {
            if pending.view.extra_text_focused {
                pending.view.extra_text.push(' ');
            }
            sync_permission_view(app, pending_permission);
            true
        }
        PermissionModalAction::None => {
            sync_permission_view(app, pending_permission);
            true
        }
        PermissionModalAction::Cancel => false,
    }
}

fn apply_permission_modal_action_inner(
    app: &mut AppState,
    session: Option<&TuiEngineSession>,
    pending_permission: &mut Option<PendingPermission>,
    action: PermissionModalAction,
    policy_store: &rebon_core::policy::PolicyStore,
    cwd: &str,
) {
    if pending_permission.is_none() {
        return;
    }
    if action == PermissionModalAction::None {
        return;
    }

    if apply_workflow_review_action(app, pending_permission, action) {
        return;
    }
    let Some(pending) = pending_permission.as_mut() else {
        return;
    };

    let mut action = action;

    // AskUserQuestion keeps vertical navigation within the current tab and
    // uses horizontal navigation for questions plus the final confirmation tab.
    if let PermissionKind::AskUserQuestion {
        ref questions,
        ref mut answers,
        ref mut active_question,
        ref mut confirmation_active,
        ref mut confirmation_selected,
        ..
    } = pending.view.kind
    {
        if questions.is_empty() || *active_question >= questions.len() {
            return;
        }

        let submit_if_ready = |pending_permission: &mut Option<PendingPermission>,
                               app: &mut AppState| {
            submit_ask_user_question_if_ready(app, session, pending_permission)
        };

        if *confirmation_active && questions.len() > 1 {
            match action {
                PermissionModalAction::MovePrev => {
                    if *confirmation_selected > 0 {
                        *confirmation_selected -= 1;
                        sync_permission_view(app, pending_permission);
                    }
                    return;
                }
                PermissionModalAction::MoveNext | PermissionModalAction::FocusExtraText => {
                    if *confirmation_selected < 1 {
                        *confirmation_selected += 1;
                        sync_permission_view(app, pending_permission);
                    }
                    return;
                }
                PermissionModalAction::MoveTabPrev => {
                    *confirmation_active = false;
                    sync_permission_view(app, pending_permission);
                    return;
                }
                PermissionModalAction::MoveTabNext
                | PermissionModalAction::TypeChar(_)
                | PermissionModalAction::Backspace
                | PermissionModalAction::Delete
                | PermissionModalAction::CursorHome
                | PermissionModalAction::CursorEnd => return,
                PermissionModalAction::Confirm | PermissionModalAction::Toggle => {
                    if *confirmation_selected == 1 {
                        action = PermissionModalAction::Cancel;
                    } else if let Some(unanswered) = questions
                        .iter()
                        .zip(answers.iter())
                        .position(|(question, answer)| !answer.has_answer(question))
                    {
                        *active_question = unanswered;
                        *confirmation_active = false;
                        sync_permission_view(app, pending_permission);
                        return;
                    } else {
                        submit_if_ready(pending_permission, app);
                        return;
                    }
                }
                PermissionModalAction::SaveEdit
                | PermissionModalAction::InsertNewline
                | PermissionModalAction::Cancel
                | PermissionModalAction::None => {}
            }
        } else {
            let q = &questions[*active_question];
            let answer = &mut answers[*active_question];
            let other_row = q.options.len();
            let chat_row = other_row + 1;

            match action {
                PermissionModalAction::MovePrev => {
                    if answer.highlighted_row > 0 {
                        answer.highlighted_row -= 1;
                        sync_permission_view(app, pending_permission);
                    }
                    return;
                }
                PermissionModalAction::MoveNext | PermissionModalAction::FocusExtraText => {
                    if answer.highlighted_row < chat_row {
                        answer.highlighted_row += 1;
                        sync_permission_view(app, pending_permission);
                    }
                    return;
                }
                PermissionModalAction::MoveTabPrev => {
                    if answer.highlighted_row == other_row {
                        answer.other_cursor_offset =
                            cursor_left(&answer.other_text, answer.other_cursor_offset);
                        sync_permission_view(app, pending_permission);
                    } else if *active_question > 0 {
                        *active_question -= 1;
                        sync_permission_view(app, pending_permission);
                    }
                    return;
                }
                PermissionModalAction::MoveTabNext => {
                    if answer.highlighted_row == other_row {
                        answer.other_cursor_offset =
                            cursor_right(&answer.other_text, answer.other_cursor_offset);
                        sync_permission_view(app, pending_permission);
                    } else if *active_question + 1 < questions.len() {
                        *active_question += 1;
                        sync_permission_view(app, pending_permission);
                    } else if questions.len() > 1 {
                        *confirmation_active = true;
                        *confirmation_selected = 0;
                        sync_permission_view(app, pending_permission);
                    }
                    return;
                }
                PermissionModalAction::TypeChar(ch) => {
                    if answer.highlighted_row == other_row && !ch.is_control() {
                        (answer.other_text, answer.other_cursor_offset) = splice_char_str(
                            &answer.other_text,
                            answer.other_cursor_offset,
                            &ch.to_string(),
                        );
                        sync_permission_view(app, pending_permission);
                    }
                    return;
                }
                PermissionModalAction::Backspace => {
                    if answer.highlighted_row == other_row {
                        (answer.other_text, answer.other_cursor_offset) =
                            splice_backspace(&answer.other_text, answer.other_cursor_offset);
                        sync_permission_view(app, pending_permission);
                    }
                    return;
                }
                PermissionModalAction::Delete => {
                    if answer.highlighted_row == other_row {
                        (answer.other_text, answer.other_cursor_offset) =
                            splice_delete(&answer.other_text, answer.other_cursor_offset);
                        sync_permission_view(app, pending_permission);
                    }
                    return;
                }
                PermissionModalAction::CursorHome => {
                    if answer.highlighted_row == other_row {
                        answer.other_cursor_offset =
                            cursor_home(&answer.other_text, answer.other_cursor_offset);
                        sync_permission_view(app, pending_permission);
                    }
                    return;
                }
                PermissionModalAction::CursorEnd => {
                    if answer.highlighted_row == other_row {
                        answer.other_cursor_offset =
                            cursor_end(&answer.other_text, answer.other_cursor_offset);
                        sync_permission_view(app, pending_permission);
                    }
                    return;
                }
                PermissionModalAction::Toggle => {
                    if answer.highlighted_row == chat_row {
                        action = PermissionModalAction::Cancel;
                    } else if answer.highlighted_row == other_row {
                        (answer.other_text, answer.other_cursor_offset) =
                            splice_char_str(&answer.other_text, answer.other_cursor_offset, " ");
                        sync_permission_view(app, pending_permission);
                        return;
                    } else if q.multi_select {
                        let idx = answer.highlighted_row;
                        if let Some(pos) = answer
                            .selected_options
                            .iter()
                            .position(|selected| *selected == idx)
                        {
                            answer.selected_options.remove(pos);
                        } else {
                            answer.selected_options.push(idx);
                        }
                        sync_permission_view(app, pending_permission);
                        return;
                    } else {
                        let idx = answer.highlighted_row;
                        if answer.selected_options.first() == Some(&idx) {
                            answer.selected_options.clear();
                        } else {
                            answer.selected_options.clear();
                            answer.selected_options.push(idx);
                        }
                        sync_permission_view(app, pending_permission);
                        return;
                    }
                }
                PermissionModalAction::Confirm => {
                    if answer.highlighted_row == chat_row {
                        action = PermissionModalAction::Cancel;
                    } else {
                        if q.multi_select && !answer.has_answer(q) {
                            return;
                        } else if !q.multi_select && answer.highlighted_row != other_row {
                            answer.selected_options.clear();
                            answer.selected_options.push(answer.highlighted_row);
                        }

                        if !answer.has_answer(q) {
                            return;
                        }

                        if questions.len() == 1 {
                            submit_if_ready(pending_permission, app);
                            return;
                        }
                        if *active_question + 1 < questions.len() {
                            *active_question += 1;
                        } else {
                            *confirmation_active = true;
                            *confirmation_selected = 0;
                        }
                        sync_permission_view(app, pending_permission);
                        return;
                    }
                }
                PermissionModalAction::SaveEdit
                | PermissionModalAction::InsertNewline
                | PermissionModalAction::Cancel
                | PermissionModalAction::None => {}
            }
        }
    }

    finish_permission_modal_action(app, session, pending_permission, action, policy_store, cwd);
}

/// What every permission modal does once the dialog-specific handling
/// above has had its turn: edit the extra-text field, move the
/// selection, or answer the query and close.
fn finish_permission_modal_action(
    app: &mut AppState,
    session: Option<&TuiEngineSession>,
    pending_permission: &mut Option<PendingPermission>,
    action: PermissionModalAction,
    policy_store: &rebon_core::policy::PolicyStore,
    cwd: &str,
) {
    let Some(pending) = pending_permission.as_mut() else {
        return;
    };
    match action {
        PermissionModalAction::None => return,
        PermissionModalAction::TypeChar(ch) => {
            if !pending.view.extra_text_focused || ch.is_control() {
                return;
            }
            pending.view.extra_text.push(ch);
        }
        PermissionModalAction::Backspace => {
            if !pending.view.extra_text_focused || pending.view.extra_text.pop().is_none() {
                return;
            }
        }
        PermissionModalAction::Toggle => {
            if !pending.view.extra_text_focused {
                return;
            }
            pending.view.extra_text.push(' ');
        }
        PermissionModalAction::MovePrev | PermissionModalAction::MoveTabPrev => {
            let previous = pending.view.selected;
            let was_focused = pending.view.extra_text_focused;
            pending.view.extra_text_focused = false;
            pending.view.move_prev();
            if !was_focused && pending.view.selected == previous {
                return;
            }
        }
        PermissionModalAction::MoveNext | PermissionModalAction::MoveTabNext => {
            let previous = pending.view.selected;
            let was_focused = pending.view.extra_text_focused;
            pending.view.extra_text_focused = false;
            pending.view.move_next();
            if !was_focused && pending.view.selected == previous {
                return;
            }
        }
        PermissionModalAction::FocusExtraText => {
            if pending.view.extra_text_focused {
                return;
            }
            pending.view.extra_text_focused = true;
        }
        PermissionModalAction::SaveEdit
        | PermissionModalAction::InsertNewline
        | PermissionModalAction::Delete
        | PermissionModalAction::CursorHome
        | PermissionModalAction::CursorEnd => return,
        PermissionModalAction::Confirm => {
            let mut option_id = pending
                .view
                .selected_option()
                .map(|option| option.option_id.clone());
            let mut ultraplan_execution_error = None;

            // For EnterPlanMode, also flip the footer mode on confirm.
            if matches!(pending.view.kind, PermissionKind::EnterPlanMode)
                && option_id.as_deref() == Some("allow_once")
            {
                set_permission_mode_for_session(app, session, PermissionMode::Plan);
            }

            // ExitPlanMode switches permission mode according to the
            // selected execution policy. The clear-context choice lets
            // ContextReset reinsert the plan card after clearing the old
            // transcript; every other choice keeps the current transcript
            // and adds the plan card immediately.
            if let PermissionKind::ExitPlanMode { plan } = &pending.view.kind {
                match option_id.as_deref() {
                    Some(id) if id == ULTRAPLAN_CEO_OPTION_ID => match session {
                        Some(session) => match schedule_ultraplan_ceo_submit(app, session, plan) {
                            Ok(()) => set_permission_mode_for_session(
                                app,
                                Some(session),
                                PermissionMode::Default,
                            ),
                            Err(err) => ultraplan_execution_error = Some(err),
                        },
                        None => {
                            ultraplan_execution_error =
                                Some("active ultraplan session is unavailable".into())
                        }
                    },
                    Some(id) if id == ULTRAPLAN_ULTRAWORK_OPTION_ID => match session {
                        Some(session) => {
                            match schedule_ultraplan_ultrawork_submit(app, session, plan) {
                                Ok(()) => set_permission_mode_for_session(
                                    app,
                                    Some(session),
                                    PermissionMode::Default,
                                ),
                                Err(err) => ultraplan_execution_error = Some(err),
                            }
                        }
                        None => {
                            ultraplan_execution_error =
                                Some("active ultraplan session is unavailable".into())
                        }
                    },
                    _ => {
                        let execution_mode = match option_id.as_deref() {
                            Some("yes_clear_context_auto") | Some("yes_auto") => {
                                Some(PermissionMode::Auto)
                            }
                            Some("yes_accept_edits") => Some(PermissionMode::AcceptEdits),
                            Some("yes_default") => Some(PermissionMode::Default),
                            _ => None,
                        };
                        if let Some(mode) = execution_mode {
                            let run_id = app
                                .ultraplan_status
                                .as_ref()
                                .map(|status| status.run_id.clone());
                            if run_id.is_none() {
                                set_permission_mode_for_session(app, session, mode);
                                if option_id.as_deref() != Some("yes_clear_context_auto") {
                                    super::inject_plan_card(app, plan);
                                }
                            } else {
                                match (session, run_id) {
                                    (Some(session), Some(run_id)) => {
                                        match persist_ultraplan_phase(
                                            session,
                                            &run_id,
                                            plan,
                                            RunPhase::Executing,
                                        ) {
                                            Ok(saved) => {
                                                if let Some(status) = app.ultraplan_status.as_mut()
                                                {
                                                    status.phase =
                                                        crate::session::ultraplan_run::UltraplanPhase::Executing;
                                                    if let Some(context) = status.context.as_mut() {
                                                        *context = context
                                                            .clone()
                                                            .with_run_head(&saved.head())
                                                            .with_execution_cards(
                                                                saved.execution_cards.clone(),
                                                            );
                                                    }
                                                }
                                                set_permission_mode_for_session(
                                                    app,
                                                    Some(session),
                                                    mode,
                                                );
                                                if option_id.as_deref()
                                                    != Some("yes_clear_context_auto")
                                                {
                                                    super::inject_plan_card(app, plan);
                                                }
                                            }
                                            Err(err) => ultraplan_execution_error = Some(err),
                                        }
                                    }
                                    _ => {
                                        ultraplan_execution_error =
                                            Some("active ultraplan RunState is unavailable".into())
                                    }
                                }
                            }
                        } else if option_id.as_deref() == Some("reject_once") {
                            super::inject_plan_card(app, plan);
                            let rejection_feedback =
                                normalized_extra_text(&pending.view.extra_text);
                            let rejection_state = persist_ultraplan_rejection_feedback(
                                &mut app.ultraplan_status,
                                session.map(|s| &s.session),
                                rejection_feedback.as_deref(),
                            );
                            pending.view.extra_text = build_ultraplan_rejection_extra_text(
                                rejection_feedback.as_deref(),
                                rejection_state.as_ref(),
                            );
                        }
                    }
                }
            }

            if let Some(err) = ultraplan_execution_error {
                tracing::warn!(error = %err, "aborted ultraplan execution handoff");
                super::inject_system_message(
                    app,
                    "local_command",
                    &format!("Ultraplan execution was not started: {err}"),
                );
                app.follow_transcript_tail = true;
                option_id = None;
            }

            // An approved profile proposal is carried out here, because this
            // is the last point that still holds the session. The report it
            // produces is handed back as the tool's own input, which is the
            // only thing that lets `ProfileSwitch::call` report success — a
            // bare approval means nothing applied it.
            let profile_proposal = match &pending.view.kind {
                PermissionKind::Profile(proposal) => Some(proposal.clone()),
                _ => None,
            };
            let profile_answer = profile_proposal
                .filter(|_| option_id.as_deref() == Some("allow_once"))
                .map(|proposal| {
                    match super::profile_proposal::apply_approved_proposal(app, session, &proposal)
                    {
                        Ok(result) => super::profile_proposal::approved_answer(&proposal, result),
                        Err(err) => {
                            // Approved and then failed: the user has to see
                            // that, and the model has to be told the session
                            // did not move — reporting the approval alone
                            // would leave both believing it had.
                            super::inject_system_message(
                                app,
                                "error",
                                &format!("Profile \"{}\" was not applied: {err}", proposal.label),
                            );
                            app.follow_transcript_tail = true;
                            super::profile_proposal::not_applied_answer(&err)
                        }
                    }
                });

            let extra_text = normalized_extra_text(&pending.view.extra_text);
            let overlay_blocks = app.rebon_tui.overlay.blocks.len();
            let Some(pending) = pending_permission.take() else {
                return;
            };
            let _ = answer_permission(
                pending.outbound,
                PermissionChoice::Confirm {
                    option_id,
                    extra_text,
                    prepared: profile_answer,
                    overlay_blocks,
                },
                policy_store,
                cwd,
            );
            app.pending_permission_view = None;
            return;
        }
        PermissionModalAction::Cancel => {
            let Some(pending) = pending_permission.take() else {
                return;
            };
            let _ = answer_permission(
                pending.outbound,
                PermissionChoice::Cancelled,
                policy_store,
                cwd,
            );
            app.pending_permission_view = None;
            return;
        }
    }

    sync_permission_view(app, pending_permission);
}

pub(super) fn set_permission_mode_for_session(
    app: &mut AppState,
    session: Option<&TuiEngineSession>,
    mode: PermissionMode,
) {
    app.set_permission_mode(mode);
    record_background_permission_mode_acceptance(mode);
    if let Some(session) = session {
        let wire = mode.as_wire();
        let _ = session
            .engine_half
            .handler
            .state()
            .set_permission_mode(&session.session_id, wire);
    }
}

fn normalized_extra_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_types::{analyze_ultraplan_plan, ultraplan_plan_hash_for_profile, VerdictSource};
    use rebon_types::{PlanStepCoverageInput, ReviewerVerdictRecord};

    use crate::session::ultraplan_run::{UltraplanPhase, UltraplanStatus};
    use crate::tui::app::AppState;
    use crate::tui::runner::test_support::{make_test_tui_session, RuntimeModeEnvGuard};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rebon_core::permission::{
        OutboundPermissionQuery, PermissionAnswer, PermissionOptionKind, PermissionQueryOption,
    };
    use rebon_types::{PolicyMode, UltraplanContext, UltraplanProfile, UltraplanRunState};
    use rebon_types::{ToolCallStatus, ToolKind};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::oneshot;
    use tokio::sync::oneshot::error::TryRecvError;

    #[test]
    fn runtime_permission_mode_sync_skips_persisted_config_side_effects() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        app.permission_mode_cell = session.engine_half.permission_mode_cell.clone();
        let apply_count = Arc::new(AtomicUsize::new(0));
        let apply_count_for_callback = Arc::clone(&apply_count);
        session.engine_half.handler.config_option_applier = Some(Arc::new(move |_, _| {
            apply_count_for_callback.fetch_add(1, Ordering::SeqCst);
        }));

        for mode in [PermissionMode::Plan, PermissionMode::Auto] {
            set_permission_mode_for_session(&mut app, Some(&session), mode);

            assert_eq!(app.permission_mode, mode);
            assert_eq!(
                *session
                    .engine_half
                    .permission_mode_cell
                    .lock()
                    .expect("mode cell"),
                mode
            );
            assert_eq!(
                session
                    .engine_half
                    .handler
                    .state()
                    .get_session(&session.session_id)
                    .expect("session record")
                    .permission_mode,
                mode.as_wire()
            );
            assert_eq!(
                session
                    .engine_half
                    .handler
                    .config_options_for_session(&session.session_id)
                    .iter()
                    .find(|option| option.id == "permissions")
                    .expect("permissions option")
                    .current_value,
                mode.as_wire()
            );
            assert_eq!(
                session
                    .engine_half
                    .handler
                    .config_options
                    .lock()
                    .expect("config options")
                    .iter()
                    .find(|option| option.id == "permissions")
                    .expect("permissions option")
                    .current_value,
                "default"
            );
        }

        assert_eq!(apply_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn explicit_permission_config_change_still_runs_persisted_side_effects() {
        let mut session = make_test_tui_session();
        let apply_count = Arc::new(AtomicUsize::new(0));
        let apply_count_for_callback = Arc::clone(&apply_count);
        session.engine_half.handler.config_option_applier =
            Some(Arc::new(move |config_id, value| {
                assert_eq!(config_id, "permissions");
                assert_eq!(value, "auto");
                apply_count_for_callback.fetch_add(1, Ordering::SeqCst);
            }));

        session.engine_half.handler.apply_config_option_local(
            &session.session_id,
            "permissions",
            "auto",
        );

        assert_eq!(apply_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            session
                .engine_half
                .handler
                .state()
                .get_session(&session.session_id)
                .expect("session record")
                .permission_mode,
            "auto"
        );
    }

    fn sample_query() -> OutboundPermissionQuery {
        let (response_tx, _response_rx) = oneshot::channel();
        OutboundPermissionQuery {
            id: 1,
            tool_name: "Read".into(),
            tool_call_id: "tool-1".into(),
            session_id: "sess-1".into(),
            title: "Read files".into(),
            message: "Read(path=\"Cargo.toml\")".into(),
            tool_input: None,
            metadata: None,
            options: vec![PermissionQueryOption {
                option_id: "allow_once".into(),
                label: "Allow once".into(),
                kind: PermissionOptionKind::AllowOnce,
            }],
            response_tx,
        }
    }

    fn ask_user_question_query(
        tool_input: serde_json::Value,
    ) -> (OutboundPermissionQuery, oneshot::Receiver<PermissionAnswer>) {
        let (response_tx, response_rx) = oneshot::channel();
        (
            OutboundPermissionQuery {
                id: 2,
                tool_name: "AskUserQuestion".into(),
                tool_call_id: "tool-ask-1".into(),
                session_id: "sess-1".into(),
                title: "Answer questions".into(),
                message: "Answer the question".into(),
                tool_input: Some(tool_input),
                metadata: None,
                options: vec![PermissionQueryOption {
                    option_id: "allow_once".into(),
                    label: "Allow once".into(),
                    kind: PermissionOptionKind::AllowOnce,
                }],
                response_tx,
            },
            response_rx,
        )
    }

    fn exit_plan_mode_query(options: Vec<PermissionQueryOption>) -> OutboundPermissionQuery {
        let (response_tx, _response_rx) = oneshot::channel();
        OutboundPermissionQuery {
            id: 42,
            tool_name: "ExitPlanMode".into(),
            tool_call_id: "tool-exit-1".into(),
            session_id: "sess-exit".into(),
            title: "Review plan".into(),
            message: "Review the proposed plan".into(),
            tool_input: Some(json!({"plan": "1. do stuff"})),
            metadata: None,
            options,
            response_tx,
        }
    }

    fn exit_plan_mode_options() -> Vec<PermissionQueryOption> {
        vec![
            PermissionQueryOption {
                option_id: "yes_clear_context_auto".into(),
                label: "Yes, clear context and run with auto mode".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionQueryOption {
                option_id: "yes_auto".into(),
                label: "Yes, run with auto mode".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionQueryOption {
                option_id: "yes_accept_edits".into(),
                label: "Yes, auto-accept edits".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionQueryOption {
                option_id: "yes_default".into(),
                label: "Yes, manually approve edits".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionQueryOption {
                option_id: "reject_once".into(),
                label: "No, chat with this".into(),
                kind: PermissionOptionKind::RejectOnce,
            },
        ]
    }

    fn save_approved_ultraplan_state(session: &TuiEngineSession, run_id: &str, plan: &str) {
        let mut state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "ship it".into(),
            None,
            1,
        );
        let plan_hash = ultraplan_plan_hash_for_profile(state.profile, plan);
        state.last_plan_draft = Some(plan.into());
        state.plan_hash = Some(plan_hash.clone());
        state.record_final_gate(rebon_types::FinalGateOutcome::Pass, plan_hash, Vec::new());
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
    }

    fn test_ultraplan_status(run_id: &str, round: u32) -> UltraplanStatus {
        UltraplanStatus {
            run_id: run_id.into(),
            phase: UltraplanPhase::AwaitingPlanApproval,
            task_title: "ship it".into(),
            started_at_ms: Some(1),
            worker_count: None,
            context: Some(UltraplanContext::planning_turn(
                run_id,
                "awaitingplanapproval",
                PolicyMode::Enforce,
            )),
            round,
            last_verdict: None,
            last_coverage: None,
            execution_reexploration_count: 0,
        }
    }

    fn assert_ultraplan_rejection_state_text(text: &str, round: u32, _max_rounds: u32) {
        assert!(text.contains(rebon_core::permission::ULTRAPLAN_REJECTION_FEEDBACK_PREFIX));
        assert!(text.contains("AUTHORITATIVE UPDATED /ultraplan LOOP STATE AFTER REJECTION"));
        assert!(text.contains(&format!("current_round: {round}")));
        assert!(text.contains("workflow_stage:"));
        assert!(text.contains("legacy_phase: Synthesizing"));
        assert!(text.contains("budget: research"));
        assert!(text.contains("plan_revisions"));
        assert!(text.contains("adversarial_reviews"));
        assert!(text.contains(&format!("round: {round}")));
        assert!(text.contains("source: UserRejection"));
        assert!(text.contains("USER_REJECTED"));
        assert!(text.contains("blocking_gaps: 1"));
        assert!(text.contains("This is state, not a gate"));
    }

    fn workflow_query(message: String) -> OutboundPermissionQuery {
        workflow_query_with_metadata(message, None)
    }

    fn workflow_query_with_metadata(
        message: String,
        metadata: Option<serde_json::Value>,
    ) -> OutboundPermissionQuery {
        workflow_query_with_metadata_and_rx(message, metadata).0
    }

    fn workflow_query_with_metadata_and_rx(
        message: String,
        metadata: Option<serde_json::Value>,
    ) -> (OutboundPermissionQuery, oneshot::Receiver<PermissionAnswer>) {
        let (response_tx, response_rx) = oneshot::channel();
        (
            OutboundPermissionQuery {
                id: 44,
                tool_name: "Workflow".into(),
                tool_call_id: "tool-workflow-1".into(),
                session_id: "sess-workflow".into(),
                title: "Review workflow demo".into(),
                message,
                tool_input: Some(json!({"name":"demo"})),
                metadata,
                options: vec![
                    PermissionQueryOption {
                        option_id: "allow_once".into(),
                        label: "Allow once".into(),
                        kind: PermissionOptionKind::AllowOnce,
                    },
                    PermissionQueryOption {
                        option_id: "reject_once".into(),
                        label: "Reject once".into(),
                        kind: PermissionOptionKind::RejectOnce,
                    },
                ],
                response_tx,
            },
            response_rx,
        )
    }

    #[test]
    fn build_pending_permission_uses_workflow_review_message() {
        let app = AppState::new();
        let review =
            "Overview\n- Name: demo\n\nPhases\n1. Run\n\nExecution graph\n`-- agent calls: 1"
                .to_string();

        let pending = build_pending_permission(&app, workflow_query(review.clone()));

        assert_eq!(pending.view.title, "Review workflow before running");
        assert_eq!(pending.view.summary, review);
        assert!(matches!(
            pending.view.kind,
            PermissionKind::WorkflowReview(_)
        ));
    }

    #[test]
    fn workflow_review_graph_browse_edit_and_revision_feedback() {
        let mut app = AppState::new();
        let metadata = json!({
            "kind": "workflowReview",
            "name": "demo",
            "description": "Review changed files",
            "phases": [
                {"title": "Inspect", "detail": "Find relevant files"},
                {"title": "Verify", "detail": "Run targeted checks"}
            ],
            "calls": [
                {"kind": "agent", "line": 12, "summary": "prompt \"inspect\" (phase=Inspect)"}
            ],
            "warnings": [],
            "errors": []
        });
        let (outbound, mut response_rx) =
            workflow_query_with_metadata_and_rx("raw workflow".into(), Some(metadata));
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveNext,
            &policy_store,
            ".",
        );
        let review = match &pending_permission.as_ref().unwrap().view.kind {
            PermissionKind::WorkflowReview(review) => review,
            other => panic!("expected workflow review, got {other:?}"),
        };
        assert_eq!(review.focused_node, 1);

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );
        let review = match &pending_permission.as_ref().unwrap().view.kind {
            PermissionKind::WorkflowReview(review) => review,
            other => panic!("expected workflow review, got {other:?}"),
        };
        assert_eq!(review.mode, WorkflowReviewMode::Edit);

        for action in [
            PermissionModalAction::TypeChar(' '),
            PermissionModalAction::TypeChar('a'),
            PermissionModalAction::TypeChar('n'),
            PermissionModalAction::TypeChar('d'),
            PermissionModalAction::TypeChar(' '),
            PermissionModalAction::TypeChar('l'),
            PermissionModalAction::TypeChar('i'),
            PermissionModalAction::TypeChar('m'),
            PermissionModalAction::TypeChar('i'),
            PermissionModalAction::TypeChar('t'),
            PermissionModalAction::TypeChar(' '),
            PermissionModalAction::TypeChar('s'),
            PermissionModalAction::TypeChar('c'),
            PermissionModalAction::TypeChar('o'),
            PermissionModalAction::TypeChar('p'),
            PermissionModalAction::TypeChar('e'),
            PermissionModalAction::SaveEdit,
        ] {
            apply_permission_modal_action(
                &mut app,
                &mut pending_permission,
                action,
                &policy_store,
                ".",
            );
        }
        let review = match &pending_permission.as_ref().unwrap().view.kind {
            PermissionKind::WorkflowReview(review) => review,
            other => panic!("expected workflow review, got {other:?}"),
        };
        assert_eq!(review.mode, WorkflowReviewMode::Browse);
        assert!(review.has_pending_edits());

        let review = match &mut pending_permission.as_mut().unwrap().view.kind {
            PermissionKind::WorkflowReview(review) => review,
            other => panic!("expected workflow review, got {other:?}"),
        };
        review.actions_focused = true;
        pending_permission.as_mut().unwrap().view.selected = 1;

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        assert!(pending_permission.is_none());
        let answer = response_rx
            .try_recv()
            .expect("pending edits should submit revision feedback");
        match answer {
            PermissionAnswer::Selected {
                option_id,
                extra_text,
                ..
            } => {
                assert_eq!(option_id, "reject_once");
                let feedback = extra_text.expect("revision feedback");
                assert!(feedback.contains("Please revise the workflow"));
                assert!(feedback.contains("Phase 1: Inspect"));
                assert!(feedback.contains("limit scope"));
            }
            PermissionAnswer::Cancelled => panic!("expected selected revision feedback"),
        }
    }

    #[test]
    fn workflow_review_tab_cycles_graph_actions_note() {
        let mut app = AppState::new();
        let metadata = json!({
            "kind": "workflowReview",
            "name": "demo",
            "description": "Review changed files",
            "phases": [
                {"title": "Inspect", "detail": "Find relevant files"}
            ],
            "calls": [
                {"kind": "agent", "line": 12, "summary": "prompt \"inspect\" (phase=Inspect)"}
            ],
            "warnings": [],
            "errors": []
        });
        let pending = build_pending_permission(
            &app,
            workflow_query_with_metadata("raw workflow".into(), Some(metadata)),
        );
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        let review_state = |pending_permission: &Option<PendingPermission>| {
            let pending = pending_permission.as_ref().unwrap();
            match &pending.view.kind {
                PermissionKind::WorkflowReview(review) => {
                    (review.actions_focused, pending.view.extra_text_focused)
                }
                other => panic!("expected workflow review, got {other:?}"),
            }
        };

        // graph → actions: option list takes focus, note stays unfocused.
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::FocusExtraText,
            &policy_store,
            ".",
        );
        assert_eq!(review_state(&pending_permission), (true, false));

        // In the actions state typed keys must NOT be appended as note text
        // (the original bug: Tab jumped straight to the note, so every key
        // landed in the extra text after "Allow once").
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::TypeChar('x'),
            &policy_store,
            ".",
        );
        assert!(pending_permission
            .as_ref()
            .unwrap()
            .view
            .extra_text
            .is_empty());

        // Arrow keys move the option selection while actions are focused.
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveNext,
            &policy_store,
            ".",
        );
        assert_eq!(pending_permission.as_ref().unwrap().view.selected, 1);

        // actions → note: now typing appends to the note.
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::FocusExtraText,
            &policy_store,
            ".",
        );
        assert_eq!(review_state(&pending_permission), (true, true));
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::TypeChar('n'),
            &policy_store,
            ".",
        );
        assert_eq!(pending_permission.as_ref().unwrap().view.extra_text, "n");

        // note → graph: both focuses cleared.
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::FocusExtraText,
            &policy_store,
            ".",
        );
        assert_eq!(review_state(&pending_permission), (false, false));
    }

    #[test]
    fn build_pending_permission_prefers_structured_workflow_review_metadata() {
        let app = AppState::new();
        let raw_message = "export const meta = { name: 'demo' };\nawait agent('x');".to_string();
        let metadata = json!({
            "kind": "workflowReview",
            "name": "demo",
            "title": "Demo Workflow",
            "description": "Review changed files with two subagents",
            "source": "inline",
            "argsSummary": "{\"scope\":\"repo\"}",
            "agentCallCount": 2,
            "totalCallCount": 3,
            "phases": [
                {"title": "Inspect", "detail": "Find relevant files"},
                {"title": "Verify", "detail": "Run targeted checks", "model": "m1"}
            ],
            "calls": [
                {"kind": "agent", "line": 12, "summary": "prompt \"inspect\""},
                {"kind": "agent", "line": 18, "summary": "prompt \"verify\""},
                {"kind": "log", "line": 20, "summary": "done"}
            ],
            "warnings": ["meta.phases is empty in fallback"],
            "errors": [],
            "scriptExcerpt": "  1| export const meta = { name: 'demo' };"
        });

        let pending = build_pending_permission(
            &app,
            workflow_query_with_metadata(raw_message.clone(), Some(metadata)),
        );

        assert_eq!(pending.view.title, "Review workflow before running");
        assert_ne!(pending.view.summary, raw_message);
        assert!(pending
            .view
            .summary
            .contains("- Summary: Review changed files"));
        assert!(pending
            .view
            .summary
            .contains("- Calls: 2 agent call(s), 3 total static call(s)"));
        assert!(pending.view.summary.contains("1. Inspect"));
        assert!(pending.view.summary.contains("2. Verify [model: m1]"));
        let overview_idx = pending.view.summary.find("Overview").expect("overview");
        let phases_idx = pending.view.summary.find("Phases").expect("phases");
        let agent_idx = pending
            .view
            .summary
            .find("Agent calls")
            .expect("agent calls");
        let details_idx = pending
            .view
            .summary
            .find("Script details")
            .expect("script details");
        let excerpt_idx = pending
            .view
            .summary
            .find("Script excerpt")
            .expect("script excerpt");
        assert!(overview_idx < phases_idx);
        assert!(phases_idx < agent_idx);
        assert!(agent_idx < details_idx);
        assert!(details_idx < excerpt_idx);
    }

    #[test]
    fn drain_pending_permissions_builds_modal_from_streaming_tool() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        session.cwd = "F:/dev/project".into();
        session
            .engine_half
            .runtime
            .permission_broker
            .forward_direct(sample_query());
        app.rebon_tui
            .overlay
            .upsert_streaming_tool_use(rebon_tui::StreamingToolUse {
                call_id: "tool-1".into(),
                tool_name: "Read".into(),
                kind: ToolKind::Read,
                status: ToolCallStatus::Pending,
                title: Some("Read Cargo.toml".into()),
                content: None,
                locations: None,
                raw_input: Some(HashMap::from([("path".into(), json!("Cargo.toml"))])),
                raw_output: None,
            });
        let mut pending_permission = None;

        let drained = drain_pending_permissions(&mut app, &mut session, &mut pending_permission);

        assert_eq!(drained, 1);
        assert!(pending_permission.is_some());
        assert_eq!(
            pending_permission.unwrap().view.summary,
            "Read(path=\"Cargo.toml\")"
        );
    }

    /// A5: a kernel plugin's ungranted tool call arrives in the TUI as an
    /// ordinary permission prompt — through the same broker channel, into the
    /// same modal — and it offers exactly two verdicts.
    ///
    /// The surface has no memory: an "allow always" here would promise a rule
    /// nothing stores, so the modal must not grow one.
    #[test]
    fn a_kernel_plugin_ask_renders_as_a_permission_prompt() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        session.cwd = "F:/dev/project".into();
        let prompt = rebon_kernel_seats::kernel_tool_asks::AskPrompt {
            id: 4,
            tool: "Write".into(),
            request: json!({ "message": "write the release notes" }),
            preview: json!({ "file_path": "NOTES.md" }),
        };
        let (query, response_rx) = rebon_kernel_seats::kernel_tool_asks::permission_query_for(
            &prompt,
            &session.session_id,
        );
        session
            .engine_half
            .runtime
            .permission_broker
            .forward_direct(query);
        let mut pending_permission = None;

        let drained = drain_pending_permissions(&mut app, &mut session, &mut pending_permission);

        assert_eq!(drained, 1);
        let pending = pending_permission.expect("the plugin ask became a modal");
        assert!(
            pending.view.summary.contains("kernel plugin"),
            "the user is told a plugin is asking: {}",
            pending.view.summary
        );
        assert!(
            pending.view.summary.contains("write the release notes"),
            "the tool's own words survive: {}",
            pending.view.summary
        );
        let kinds: Vec<PermissionOptionKind> =
            pending.view.options.iter().map(|o| o.kind).collect();
        assert_eq!(
            kinds,
            vec![
                PermissionOptionKind::AllowOnce,
                PermissionOptionKind::RejectOnce
            ],
            "two verdicts, and no rule the ask surface cannot remember"
        );
        drop(response_rx);
    }

    #[test]
    fn build_pending_permission_uses_tool_overlay_summary() {
        let mut app = AppState::new();
        app.rebon_tui
            .overlay
            .upsert_streaming_tool_use(rebon_tui::StreamingToolUse {
                call_id: "tool-1".into(),
                tool_name: "Read".into(),
                kind: ToolKind::Read,
                status: ToolCallStatus::Pending,
                title: Some("Read Cargo.toml".into()),
                content: None,
                locations: None,
                raw_input: Some(HashMap::from([("path".into(), json!("Cargo.toml"))])),
                raw_output: None,
            });

        let pending = build_pending_permission(&app, sample_query());

        assert_eq!(pending.view.title, "Allow Read?");
        assert_eq!(pending.view.summary, "Read(path=\"Cargo.toml\")");
    }

    #[test]
    fn maybe_handle_permission_key_confirms_and_closes_modal() {
        let mut app = AppState::new();
        let pending = build_pending_permission(&app, sample_query());
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let session = make_test_tui_session();

        assert!(maybe_handle_permission_key(
            &mut app,
            &session,
            &mut pending_permission,
            &key,
            &policy_store,
            ".",
            true,
        ));
        assert!(pending_permission.is_none());
        assert!(app.pending_permission_view.is_none());
    }

    #[test]
    fn permission_shift_tab_cycles_mode_without_closing_modal() {
        let mut app = AppState::new();
        let pending = build_pending_permission(&app, sample_query());
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();
        let session = make_test_tui_session();
        let key = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);

        assert!(maybe_handle_permission_key(
            &mut app,
            &session,
            &mut pending_permission,
            &key,
            &policy_store,
            ".",
            true,
        ));

        assert!(pending_permission.is_some());
        assert!(app.pending_permission_view.is_some());
        assert_eq!(app.permission_mode, PermissionMode::Plan);
        assert_eq!(
            *app.permission_mode_cell.lock().expect("permission cell"),
            PermissionMode::Plan
        );
        let stored_mode = session
            .engine_half
            .handler
            .state()
            .get_session(&session.session_id)
            .expect("session record")
            .permission_mode;
        assert_eq!(stored_mode, "plan");
    }

    #[test]
    fn permission_tab_extra_text_is_sent_with_confirmed_option() {
        let mut app = AppState::new();
        let (response_tx, mut response_rx) = oneshot::channel();
        let outbound = OutboundPermissionQuery {
            id: 3,
            tool_name: "Read".into(),
            tool_call_id: "tool-extra-1".into(),
            session_id: "sess-1".into(),
            title: "Read files".into(),
            message: "Read(path=\"Cargo.toml\")".into(),
            tool_input: None,
            metadata: None,
            options: vec![
                PermissionQueryOption {
                    option_id: "allow_once".into(),
                    label: "Allow once".into(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionQueryOption {
                    option_id: "reject_once".into(),
                    label: "Reject once".into(),
                    kind: PermissionOptionKind::RejectOnce,
                },
            ],
            response_tx,
        };
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        for action in [
            PermissionModalAction::FocusExtraText,
            PermissionModalAction::TypeChar('u'),
            PermissionModalAction::TypeChar('s'),
            PermissionModalAction::TypeChar('e'),
            PermissionModalAction::Toggle,
            PermissionModalAction::TypeChar('r'),
            PermissionModalAction::Confirm,
        ] {
            apply_permission_modal_action(
                &mut app,
                &mut pending_permission,
                action,
                &policy_store,
                ".",
            );
        }

        assert!(pending_permission.is_none());
        let answer = response_rx
            .try_recv()
            .expect("permission should submit with extra text");
        match answer {
            PermissionAnswer::Selected {
                option_id,
                extra_text,
                ..
            } => {
                assert_eq!(option_id, "allow_once");
                assert_eq!(extra_text.as_deref(), Some("use r"));
            }
            other => panic!("expected selected answer, got {other:?}"),
        }
    }

    #[test]
    fn every_answered_question_is_recorded_as_a_plain_interview_turn() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "question-answer".into();
        let run_id = "question-run";
        let state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "ship it".into(),
            None,
            1,
        );
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        let mut status = test_ultraplan_status(run_id, 1);
        status.phase = UltraplanPhase::Researching;
        status.context = Some(UltraplanContext::planning_turn(
            run_id,
            "researching",
            PolicyMode::Enforce,
        ));
        app.ultraplan_status = Some(status);
        // Multiple questions in one call used to be a Grill protocol
        // violation; now the model chooses the shape.
        let (outbound, mut response_rx) = ask_user_question_query(json!({
            "questions": [{
                "question": "Which rollout should we use?",
                "header": "Rollout",
                "options": [
                    {"label": "Gradual (Recommended)", "description": "Roll out in stages"},
                    {"label": "Immediate", "description": "Ship at once"}
                ]
            }]
        }));
        let mut pending_permission = Some(build_pending_permission(&app, outbound));
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        apply_permission_modal_action_for_session(
            &mut app,
            &session,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        assert!(pending_permission.is_none());
        assert!(response_rx.try_recv().is_ok());
        let state = rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
            .unwrap();
        assert_eq!(state.interview.turns.len(), 1);
        let turn = &state.interview.turns[0];
        assert_eq!(turn.question, "Which rollout should we use?");
        assert_eq!(
            turn.recommended_answer.as_deref(),
            Some("Gradual (Recommended)")
        );
        assert_eq!(turn.answer, "Gradual (Recommended)");
        assert!(state.asked_user_once);
    }

    #[test]
    fn standard_question_answer_is_persisted_in_scope_checkpoint() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "standard-question-answer".into();
        let run_id = "standard-question-run";
        let state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "ship it".into(),
            None,
            1,
        );
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));
        let (outbound, mut response_rx) = ask_user_question_query(json!({
            "questions": [{
                "question": "Which rollout should we use?",
                "header": "Rollout",
                "options": [
                    {"label": "Gradual", "description": "Roll out in stages"},
                    {"label": "Immediate", "description": "Ship at once"}
                ]
            }]
        }));
        let mut pending_permission = Some(build_pending_permission(&app, outbound));
        sync_permission_view(&mut app, &pending_permission);

        apply_permission_modal_action_for_session(
            &mut app,
            &session,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &rebon_core::policy::PolicyStore::new(),
            ".",
        );

        assert!(response_rx.try_recv().is_ok());
        let state = rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
            .unwrap();
        assert_eq!(state.stage(), rebon_types::UltraplanStage::EvidenceVerify);
        assert_eq!(state.interview.turns.len(), 1);
        assert_eq!(state.interview.turns[0].answer, "Gradual");
        let checkpoint = state.latest_checkpoint().expect("scope checkpoint");
        assert!(checkpoint
            .user_decisions
            .iter()
            .any(|decision| decision.contains("Which rollout should we use? => Gradual")));
    }

    #[test]
    fn a_question_after_a_review_pass_is_recorded_like_any_other() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "post-review-question".into();
        let run_id = "post-review-question-run";
        let plan = "P1. Ship it";
        let plan_hash = ultraplan_plan_hash_for_profile(UltraplanProfile::Standard, plan);
        let mut state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "ship it".into(),
            None,
            1,
        );
        state.set_plan_artifacts(
            plan.into(),
            rebon_types::PlanCoverageResult {
                covered: Vec::new(),
                missing: Vec::new(),
                unknown_ids: Vec::new(),
            },
            Vec::new(),
        );
        state.auto_review_passed_hash = Some(plan_hash.clone());
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));
        let (outbound, mut response_rx) = ask_user_question_query(json!({
            "questions": [{
                "question": "Proceed with this reviewed draft?",
                "header": "Proceed",
                "options": [
                    {"label": "Yes", "description": "Continue"},
                    {"label": "Revise", "description": "Change it"}
                ]
            }]
        }));
        let mut pending_permission = Some(build_pending_permission(&app, outbound));
        sync_permission_view(&mut app, &pending_permission);

        apply_permission_modal_action_for_session(
            &mut app,
            &session,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &rebon_core::policy::PolicyStore::new(),
            ".",
        );

        assert!(response_rx.try_recv().is_ok());
        let state = rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
            .unwrap();
        // No draft-release bookkeeping is needed any more: the answer is just
        // another recorded turn, and the draft binding is untouched.
        assert_eq!(state.interview.turns.len(), 1);
        assert!(state.released_plan_hash.is_none());
        assert_eq!(state.plan_hash.as_deref(), Some(plan_hash.as_str()));
        assert_eq!(state.last_plan_draft.as_deref(), Some(plan));
    }

    #[test]
    fn cancelled_question_does_not_advance_interview_state() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "grill-question-cancel".into();
        let run_id = "grill-question-cancel-run";
        let state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "ship it".into(),
            None,
            1,
        )
        .with_profile(UltraplanProfile::Grill);
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));
        let (outbound, _response_rx) = ask_user_question_query(json!({
            "questions": [{
                "question": "Which rollout should we use?",
                "header": "Rollout",
                "options": [
                    {"label": "Gradual", "description": "Roll out in stages"},
                    {"label": "Immediate", "description": "Ship at once"}
                ]
            }]
        }));
        let mut pending_permission = Some(build_pending_permission(&app, outbound));
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        apply_permission_modal_action_for_session(
            &mut app,
            &session,
            &mut pending_permission,
            PermissionModalAction::Cancel,
            &policy_store,
            ".",
        );

        let state = rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
            .unwrap();
        assert_eq!(state.interview.revision, state.ledger_revision);
        assert_eq!(state.interview.revision, 1);
        assert!(state.interview.turns.is_empty());
        assert!(!state.asked_user_once);
    }

    fn prepared_submitted_plan_state(
        session: &TuiEngineSession,
        run_id: &str,
        plan: &str,
    ) -> UltraplanRunState {
        let mut state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "ship it".into(),
            None,
            1,
        );
        state
            .requirement_ledger
            .push(rebon_types::RequirementLedgerEntry {
                id: "R1".into(),
                title: "Ship safely".into(),
                source: rebon_types::RequirementSource::Question,
                round_added: 1,
            });
        state.record_interview_turn(
            "Which rollout?".into(),
            Some("Gradual".into()),
            "Gradual".into(),
        );
        let analysis = analyze_ultraplan_plan(
            plan,
            &state.requirement_ledger,
            &[PlanStepCoverageInput {
                step_id: "P1".into(),
                requirement_ids: vec!["R1".into()],
            }],
        );
        let cards = analysis
            .steps
            .iter()
            .map(|step| step.card.clone())
            .collect();
        state.set_plan_artifacts(plan.into(), analysis.coverage, cards);
        state
    }

    #[test]
    fn ask_user_question_multi_select_space_toggles_multiple_before_submit() {
        let mut app = AppState::new();
        let (outbound, mut response_rx) = ask_user_question_query(json!({
            "questions": [{
                "question": "Pick many?",
                "header": "Choice",
                "multiSelect": true,
                "options": [
                    {"label": "A", "description": "Alpha"},
                    {"label": "B", "description": "Beta"}
                ]
            }]
        }));
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Toggle,
            &policy_store,
            ".",
        );
        assert!(
            pending_permission.is_some(),
            "first Space/toggle must not submit"
        );
        assert!(matches!(response_rx.try_recv(), Err(TryRecvError::Empty)));

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveNext,
            &policy_store,
            ".",
        );
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Toggle,
            &policy_store,
            ".",
        );
        assert!(
            pending_permission.is_some(),
            "second Space/toggle must not submit"
        );
        assert!(matches!(response_rx.try_recv(), Err(TryRecvError::Empty)));

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );
        assert!(pending_permission.is_none());
        let answer = response_rx
            .try_recv()
            .expect("AskUserQuestion should submit");
        match answer {
            PermissionAnswer::Selected { updated_input, .. } => {
                let updated_input = updated_input.expect("updated_input should be present");
                assert_eq!(updated_input["answers"]["Pick many?"], "A, B");
            }
            other => panic!("expected selected answer, got {other:?}"),
        }
    }

    #[test]
    fn ask_user_question_single_select_allows_note_before_submit() {
        let mut app = AppState::new();
        let (outbound, mut response_rx) = ask_user_question_query(json!({
            "questions": [{
                "question": "Pick one?",
                "header": "Choice",
                "options": [{"label": "A", "description": "Alpha"}]
            }]
        }));
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Toggle,
            &policy_store,
            ".",
        );
        assert!(
            pending_permission.is_some(),
            "selection must remain editable"
        );
        assert!(matches!(response_rx.try_recv(), Err(TryRecvError::Empty)));

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveNext,
            &policy_store,
            ".",
        );
        for ch in "context".chars() {
            apply_permission_modal_action(
                &mut app,
                &mut pending_permission,
                PermissionModalAction::TypeChar(ch),
                &policy_store,
                ".",
            );
        }
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        let answer = response_rx
            .try_recv()
            .expect("AskUserQuestion should submit");
        match answer {
            PermissionAnswer::Selected { updated_input, .. } => {
                let updated_input = updated_input.expect("updated_input should be present");
                assert_eq!(updated_input["answers"]["Pick one?"], "A");
                assert_eq!(
                    updated_input["annotations"]["Pick one?"]["notes"],
                    "context"
                );
            }
            other => panic!("expected selected answer, got {other:?}"),
        }
    }

    #[test]
    fn ask_user_question_other_edits_at_unicode_cursor() {
        let mut app = AppState::new();
        let (outbound, _response_rx) = ask_user_question_query(json!({
            "questions": [
                {
                    "question": "First?",
                    "header": "First",
                    "options": [{"label": "A", "description": "Alpha"}]
                },
                {
                    "question": "Second?",
                    "header": "Second",
                    "options": [{"label": "B", "description": "Beta"}]
                }
            ]
        }));
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        let actions = [
            PermissionModalAction::MoveNext,
            PermissionModalAction::TypeChar('你'),
            PermissionModalAction::TypeChar('a'),
            PermissionModalAction::TypeChar('b'),
            PermissionModalAction::MoveTabPrev,
            PermissionModalAction::MoveTabPrev,
            PermissionModalAction::TypeChar('X'),
            PermissionModalAction::CursorHome,
            PermissionModalAction::TypeChar('Q'),
            PermissionModalAction::CursorEnd,
            PermissionModalAction::TypeChar('Z'),
            PermissionModalAction::CursorHome,
            PermissionModalAction::Delete,
            PermissionModalAction::MoveTabNext,
            PermissionModalAction::Backspace,
            PermissionModalAction::Toggle,
            PermissionModalAction::CursorEnd,
            PermissionModalAction::MoveTabNext,
            PermissionModalAction::CursorHome,
            PermissionModalAction::MoveTabPrev,
        ];
        for action in actions {
            apply_permission_modal_action(
                &mut app,
                &mut pending_permission,
                action,
                &policy_store,
                ".",
            );
        }

        let view = app
            .pending_permission_view
            .as_ref()
            .expect("view should remain");
        let PermissionKind::AskUserQuestion {
            answers,
            active_question,
            confirmation_active,
            ..
        } = &view.kind
        else {
            panic!("expected AskUserQuestion view");
        };
        assert_eq!(answers[0].other_text, " XabZ");
        assert_eq!(answers[0].other_cursor_offset, 0);
        assert_eq!(*active_question, 0);
        assert!(!confirmation_active);
    }

    #[test]
    fn ask_user_question_multi_select_enter_advances_after_existing_answer_only() {
        let mut app = AppState::new();
        let (outbound, mut response_rx) = ask_user_question_query(json!({
            "questions": [
                {
                    "question": "Pick many?",
                    "header": "Choice",
                    "multiSelect": true,
                    "options": [
                        {"label": "A", "description": "Alpha"},
                        {"label": "B", "description": "Beta"}
                    ]
                },
                {
                    "question": "Pick one?",
                    "header": "Choice",
                    "options": [
                        {"label": "C", "description": "Gamma"}
                    ]
                }
            ]
        }));
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );
        let view = app
            .pending_permission_view
            .as_ref()
            .expect("view should remain");
        if let PermissionKind::AskUserQuestion {
            active_question, ..
        } = &view.kind
        {
            assert_eq!(
                *active_question, 0,
                "Enter without an answer must not advance"
            );
        } else {
            panic!("expected AskUserQuestion view");
        }
        assert!(matches!(response_rx.try_recv(), Err(TryRecvError::Empty)));

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Toggle,
            &policy_store,
            ".",
        );
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );
        let view = app
            .pending_permission_view
            .as_ref()
            .expect("view should remain");
        if let PermissionKind::AskUserQuestion {
            active_question, ..
        } = &view.kind
        {
            assert_eq!(*active_question, 1, "Enter with an answer should advance");
        } else {
            panic!("expected AskUserQuestion view");
        }
        assert!(matches!(response_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn ask_user_question_multi_question_uses_tabs_and_final_confirmation() {
        let mut app = AppState::new();
        let (outbound, mut response_rx) = ask_user_question_query(json!({
            "questions": [
                {
                    "question": "First?",
                    "header": "First",
                    "options": [{"label": "A", "description": "Alpha"}]
                },
                {
                    "question": "Second?",
                    "header": "Second",
                    "options": [{"label": "B", "description": "Beta"}]
                }
            ]
        }));
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveTabPrev,
            &policy_store,
            ".",
        );
        if let PermissionKind::AskUserQuestion {
            active_question,
            answers,
            ..
        } = &app.pending_permission_view.as_ref().unwrap().kind
        {
            assert_eq!(*active_question, 0);
            assert_eq!(answers[0].selected_options, vec![0]);
        } else {
            panic!("expected AskUserQuestion view");
        }

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveTabNext,
            &policy_store,
            ".",
        );
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );
        if let PermissionKind::AskUserQuestion {
            active_question,
            confirmation_active,
            ..
        } = &app.pending_permission_view.as_ref().unwrap().kind
        {
            assert_eq!(*active_question, 1);
            assert!(*confirmation_active);
        } else {
            panic!("expected AskUserQuestion view");
        }
        assert!(matches!(response_rx.try_recv(), Err(TryRecvError::Empty)));

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveTabPrev,
            &policy_store,
            ".",
        );
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveTabNext,
            &policy_store,
            ".",
        );
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        assert!(pending_permission.is_none());
        let answer = response_rx.try_recv().expect("answers should submit");
        match answer {
            PermissionAnswer::Selected { updated_input, .. } => {
                let updated_input = updated_input.expect("updated input");
                assert_eq!(updated_input["answers"]["First?"], "A");
                assert_eq!(updated_input["answers"]["Second?"], "B");
            }
            other => panic!("expected selected answer, got {other:?}"),
        }
    }

    #[test]
    fn ask_user_question_confirmation_tab_can_cancel() {
        let mut app = AppState::new();
        let (outbound, mut response_rx) = ask_user_question_query(json!({
            "questions": [
                {
                    "question": "First?",
                    "header": "First",
                    "options": [{"label": "A", "description": "Alpha"}]
                },
                {
                    "question": "Second?",
                    "header": "Second",
                    "options": [{"label": "B", "description": "Beta"}]
                }
            ]
        }));
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        for action in [
            PermissionModalAction::MoveTabNext,
            PermissionModalAction::MoveTabNext,
            PermissionModalAction::MoveNext,
            PermissionModalAction::Confirm,
        ] {
            apply_permission_modal_action(
                &mut app,
                &mut pending_permission,
                action,
                &policy_store,
                ".",
            );
        }

        assert!(pending_permission.is_none());
        assert!(matches!(
            response_rx.try_recv(),
            Ok(PermissionAnswer::Cancelled)
        ));
    }

    #[test]
    fn ask_user_question_space_in_other_text_inserts_space() {
        let mut app = AppState::new();
        let (outbound, mut response_rx) = ask_user_question_query(json!({
            "questions": [{
                "question": "Other?",
                "header": "Choice",
                "multiSelect": true,
                "options": [{"label": "A", "description": "Alpha"}]
            }]
        }));
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveNext,
            &policy_store,
            ".",
        );
        for action in [
            PermissionModalAction::TypeChar('h'),
            PermissionModalAction::TypeChar('i'),
            PermissionModalAction::Toggle,
            PermissionModalAction::TypeChar('t'),
        ] {
            apply_permission_modal_action(
                &mut app,
                &mut pending_permission,
                action,
                &policy_store,
                ".",
            );
        }
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        let answer = response_rx
            .try_recv()
            .expect("AskUserQuestion should submit");
        match answer {
            PermissionAnswer::Selected { updated_input, .. } => {
                let updated_input = updated_input.expect("updated_input should be present");
                assert_eq!(updated_input["answers"]["Other?"], "hi t");
            }
            other => panic!("expected selected answer, got {other:?}"),
        }
    }

    #[test]
    fn ask_user_question_chat_about_this_cancels_the_question() {
        let mut app = AppState::new();
        let (outbound, mut response_rx) = ask_user_question_query(json!({
            "questions": [{
                "question": "Pick one?",
                "header": "Choice",
                "options": [{"label": "A", "description": "Alpha"}]
            }]
        }));
        let pending = build_pending_permission(&app, outbound);
        assert_eq!(pending.view.title, "Choice");
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        for _ in 0..2 {
            apply_permission_modal_action(
                &mut app,
                &mut pending_permission,
                PermissionModalAction::MoveNext,
                &policy_store,
                ".",
            );
        }
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        assert!(pending_permission.is_none());
        assert!(app.pending_permission_view.is_none());
        assert!(matches!(
            response_rx.try_recv(),
            Ok(PermissionAnswer::Cancelled)
        ));
    }

    #[test]
    fn exit_plan_mode_clear_context_auto_sets_mode_without_preinserting_plan_card() {
        let mut app = AppState::new();
        app.permission_mode = PermissionMode::Plan;
        let outbound = exit_plan_mode_query(exit_plan_mode_options());
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        assert!(pending_permission.is_none());
        assert_eq!(app.permission_mode, PermissionMode::Auto,);
        assert!(app.rebon_tui.transcript.rows().is_empty());
    }

    #[test]
    fn exit_plan_mode_auto_sets_mode_and_keeps_plan_card() {
        let mut app = AppState::new();
        app.permission_mode = PermissionMode::Plan;
        let outbound = exit_plan_mode_query(exit_plan_mode_options());
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveNext,
            &policy_store,
            ".",
        );
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        assert!(pending_permission.is_none());
        assert_eq!(app.permission_mode, PermissionMode::Auto,);
        assert!(app.rebon_tui.transcript.rows().iter().any(
            |message| matches!(message, rebon_tui::Message::User(user) if user.plan_content.as_deref() == Some("1. do stuff"))
        ));
    }

    #[test]
    fn exit_plan_mode_accept_edits_sets_mode() {
        let mut app = AppState::new();
        app.permission_mode = PermissionMode::Plan;
        let outbound = exit_plan_mode_query(exit_plan_mode_options());
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveNext,
            &policy_store,
            ".",
        );
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveNext,
            &policy_store,
            ".",
        );
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        assert!(pending_permission.is_none());
        assert_eq!(app.permission_mode, PermissionMode::AcceptEdits,);
        assert_eq!(
            app.rebon_tui
                .transcript
                .rows()
                .iter()
                .filter(|message| matches!(message, rebon_tui::Message::User(user) if user.plan_content.as_deref() == Some("1. do stuff")))
                .count(),
            1
        );
    }

    #[test]
    fn exit_plan_mode_default_sets_mode() {
        let mut app = AppState::new();
        app.permission_mode = PermissionMode::Plan;
        let outbound = exit_plan_mode_query(exit_plan_mode_options());
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveNext,
            &policy_store,
            ".",
        );
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveNext,
            &policy_store,
            ".",
        );
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::MoveNext,
            &policy_store,
            ".",
        );
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        assert!(pending_permission.is_none());
        assert_eq!(app.permission_mode, PermissionMode::Default,);
        assert!(app.rebon_tui.transcript.rows().iter().any(
            |message| matches!(message, rebon_tui::Message::User(user) if user.plan_content.as_deref() == Some("1. do stuff"))
        ));
    }

    #[test]
    fn exit_plan_mode_reject_keeps_plan_mode() {
        let mut app = AppState::new();
        app.permission_mode = PermissionMode::Plan;
        let outbound = exit_plan_mode_query(exit_plan_mode_options());
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        for _ in 0..4 {
            apply_permission_modal_action(
                &mut app,
                &mut pending_permission,
                PermissionModalAction::MoveNext,
                &policy_store,
                ".",
            );
        }
        apply_permission_modal_action(
            &mut app,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        assert!(pending_permission.is_none());
        assert_eq!(app.permission_mode, PermissionMode::Plan,);
        assert!(app.rebon_tui.transcript.rows().iter().any(
            |message| matches!(message, rebon_tui::Message::User(user) if user.plan_content.as_deref() == Some("1. do stuff"))
        ));
    }

    #[test]
    fn ultraplan_rejection_extra_text_includes_authoritative_updated_state() {
        let mut state = UltraplanRunState::new(
            "ultraplan-reject-unit".into(),
            "session-id".into(),
            "ship it".into(),
            None,
            10,
        );
        state.round = 2;
        state.phase = RunPhase::Synthesizing;
        state.reviewer_verdicts.push(ReviewerVerdictRecord {
            round: 2,
            verdict: "USER_REJECTED: add more tests".into(),
            blocking_gaps: 1,
            source: VerdictSource::UserRejection,
        });

        let text = build_ultraplan_rejection_extra_text(Some("add more tests"), Some(&state));

        assert_ultraplan_rejection_state_text(&text, 2, 3);
        assert!(text.contains("run_id: ultraplan-reject-unit"));
        assert!(text.contains("Feedback: add more tests"));
        assert!(text.contains("submit one materially revised plan"));
        assert!(text.contains("Decide for yourself"));
    }

    #[test]
    fn ultraplan_exit_plan_reject_persists_and_returns_updated_loop_state() {
        let _env = RuntimeModeEnvGuard::set_ultraplan_max_rounds(Some("3"));
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "reject-with-feedback".into();
        let run_id = "ultraplan-reject-e2e";
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));
        let mut state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "ship it".into(),
            None,
            10,
        );
        state.phase = RunPhase::AwaitingPlanApproval;
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        let (response_tx, mut response_rx) = oneshot::channel();
        let mut outbound = exit_plan_mode_query(exit_plan_mode_options());
        outbound.response_tx = response_tx;
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        for _ in 0..6 {
            apply_permission_modal_action_for_session(
                &mut app,
                &session,
                &mut pending_permission,
                PermissionModalAction::MoveNext,
                &policy_store,
                ".",
            );
        }
        apply_permission_modal_action_for_session(
            &mut app,
            &session,
            &mut pending_permission,
            PermissionModalAction::FocusExtraText,
            &policy_store,
            ".",
        );
        for ch in "needs clearer acceptance criteria".chars() {
            apply_permission_modal_action_for_session(
                &mut app,
                &session,
                &mut pending_permission,
                PermissionModalAction::TypeChar(ch),
                &policy_store,
                ".",
            );
        }
        apply_permission_modal_action_for_session(
            &mut app,
            &session,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        assert!(pending_permission.is_none());
        let answer = response_rx.try_recv().unwrap();
        let extra_text = match answer {
            PermissionAnswer::Selected {
                option_id,
                extra_text: Some(extra_text),
                ..
            } => {
                assert_eq!(option_id, "reject_once");
                extra_text
            }
            other => panic!("expected selected rejection with extra text, got {other:?}"),
        };
        assert_ultraplan_rejection_state_text(&extra_text, 2, 3);
        assert!(extra_text.contains("USER_REJECTED: needs clearer acceptance criteria"));
        let persisted =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
                .unwrap();
        assert_eq!(persisted.round, 2);
        assert_eq!(persisted.phase, RunPhase::Synthesizing);
        let latest = persisted.reviewer_verdicts.last().unwrap();
        assert_eq!(latest.source, VerdictSource::UserRejection);
        assert_eq!(
            latest.verdict,
            "USER_REJECTED: needs clearer acceptance criteria"
        );
        let status = app.ultraplan_status.as_ref().unwrap();
        assert_eq!(status.round, 2);
        assert_eq!(status.phase, UltraplanPhase::Synthesizing);
    }

    #[test]
    fn ultraplan_exit_plan_reject_without_feedback_keeps_question_instruction_and_state() {
        let _env = RuntimeModeEnvGuard::set_ultraplan_max_rounds(Some("2"));
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "reject-without-feedback".into();
        let run_id = "ultraplan-reject-no-feedback";
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));
        let mut state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "ship it".into(),
            None,
            10,
        );
        state.phase = RunPhase::AwaitingPlanApproval;
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        let (response_tx, mut response_rx) = oneshot::channel();
        let mut outbound = exit_plan_mode_query(exit_plan_mode_options());
        outbound.response_tx = response_tx;
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        for _ in 0..6 {
            apply_permission_modal_action_for_session(
                &mut app,
                &session,
                &mut pending_permission,
                PermissionModalAction::MoveNext,
                &policy_store,
                ".",
            );
        }
        apply_permission_modal_action_for_session(
            &mut app,
            &session,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        let answer = response_rx.try_recv().unwrap();
        let extra_text = match answer {
            PermissionAnswer::Selected {
                extra_text: Some(extra_text),
                ..
            } => extra_text,
            other => panic!("expected selected rejection with extra text, got {other:?}"),
        };
        assert!(extra_text.contains("No feedback was provided"));
        assert!(extra_text.contains("Use AskUserQuestion"));
        assert_ultraplan_rejection_state_text(&extra_text, 2, 2);
        assert!(extra_text.contains("before revising; do not blindly resubmit the same plan"));
    }

    #[test]
    fn ultraplan_exit_plan_mode_adds_special_execution_options() {
        let expected = exit_plan_mode_options()
            .into_iter()
            .map(|option| (option.option_id, option.label))
            .collect::<Vec<_>>();

        let app = AppState::new();
        let ordinary =
            build_pending_permission(&app, exit_plan_mode_query(exit_plan_mode_options()));
        let ordinary_options = ordinary
            .view
            .options
            .iter()
            .map(|option| (option.option_id.clone(), option.label.clone()))
            .collect::<Vec<_>>();
        assert_eq!(ordinary_options, expected);

        let mut ultraplan_app = AppState::new();
        ultraplan_app.ultraplan_status = Some(test_ultraplan_status("ultraplan-test", 1));
        let ultraplan = build_pending_permission(
            &ultraplan_app,
            exit_plan_mode_query(exit_plan_mode_options()),
        );
        assert_eq!(ultraplan.view.options.len(), expected.len() + 2);
        assert!(ultraplan.view.options.iter().any(|option| {
            option.option_id == ULTRAPLAN_CEO_OPTION_ID && option.label == ULTRAPLAN_CEO_LABEL
        }));
        assert!(ultraplan.view.options.iter().any(|option| {
            option.option_id == ULTRAPLAN_ULTRAWORK_OPTION_ID
                && option.label == ULTRAPLAN_ULTRAWORK_LABEL
        }));
    }

    #[test]
    fn ultraplan_ceo_selection_enters_coordinator_and_defers_plan_payload() {
        let _env = RuntimeModeEnvGuard::set_coordinator(None);
        let mut app = AppState::new();
        let session = make_test_tui_session();
        save_approved_ultraplan_state(&session, "ultraplan-run-ceo", "1. do stuff");
        app.ultraplan_status = Some(test_ultraplan_status("ultraplan-run-ceo", 1));
        let (response_tx, mut response_rx) = oneshot::channel();
        let mut outbound = exit_plan_mode_query(exit_plan_mode_options());
        outbound.response_tx = response_tx;
        let mut pending = build_pending_permission(&app, outbound);
        pending.view.selected = pending
            .view
            .options
            .iter()
            .position(|option| option.option_id == ULTRAPLAN_CEO_OPTION_ID)
            .expect("CEO option");
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);

        apply_permission_modal_action_for_session(
            &mut app,
            &session,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &rebon_core::policy::PolicyStore::new(),
            ".",
        );

        assert!(pending_permission.is_none());
        assert!(matches!(
            response_rx.try_recv().unwrap(),
            PermissionAnswer::Selected { option_id, .. } if option_id == "yes_default"
        ));
        assert!(app.coordinator_mode);
        assert!(session.engine_half.coordinator_mode_handle.get());
        let submit = app
            .deferred_internal_submit_payloads
            .first()
            .expect("deferred CEO submit");
        assert!(submit.text.contains("yes, continue with CEO mode"));
        assert!(submit.text.contains("## Approved Ultraplan"));
        let context = submit
            .execution_policy
            .as_ref()
            .and_then(|policy| policy.ultraplan.as_ref())
            .expect("CEO ultraplan context");
        assert_eq!(context.phase, "ceo");
        assert!(!context.read_only);
        assert!(context.allowed_tools.iter().any(|tool| tool == "Agent"));
        assert!(context.allowed_tools.iter().any(|tool| tool == "Edit"));
        assert!(context.allowed_tools.iter().any(|tool| tool == "Bash"));
    }

    #[test]
    fn ultraplan_ultrawork_selection_defers_execution_controller_payload() {
        let mut app = AppState::new();
        let session = make_test_tui_session();
        save_approved_ultraplan_state(&session, "ultraplan-run-ultrawork", "1. do stuff");
        app.ultraplan_status = Some(test_ultraplan_status("ultraplan-run-ultrawork", 1));
        let (response_tx, mut response_rx) = oneshot::channel();
        let mut outbound = exit_plan_mode_query(exit_plan_mode_options());
        outbound.response_tx = response_tx;
        let mut pending = build_pending_permission(&app, outbound);
        pending.view.selected = pending
            .view
            .options
            .iter()
            .position(|option| option.option_id == ULTRAPLAN_ULTRAWORK_OPTION_ID)
            .expect("ultrawork option");
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);

        apply_permission_modal_action_for_session(
            &mut app,
            &session,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &rebon_core::policy::PolicyStore::new(),
            ".",
        );

        assert!(pending_permission.is_none());
        assert!(matches!(
            response_rx.try_recv().unwrap(),
            PermissionAnswer::Selected { option_id, .. } if option_id == "yes_default"
        ));
        assert!(!app.coordinator_mode);
        let submit = app
            .deferred_internal_submit_payloads
            .first()
            .expect("deferred ultrawork submit");
        assert!(submit.text.contains("yes, execute with ultrawork"));
        let context = submit
            .execution_policy
            .as_ref()
            .and_then(|policy| policy.ultraplan.as_ref())
            .expect("ultrawork ultraplan context");
        assert_eq!(context.phase, "ultrawork_execution");
        assert!(context.plan_fidelity);
        assert!(context.allowed_tools.iter().any(|tool| tool == "Workflow"));
        assert!(context.denied_tools.iter().any(|tool| tool == "Agent"));
    }

    #[test]
    fn ultraplan_default_selection_enters_execution_without_extra_workflow_options() {
        let _env = RuntimeModeEnvGuard::set_coordinator(None);
        let mut app = AppState::new();
        let session = make_test_tui_session();
        save_approved_ultraplan_state(&session, "ultraplan-run-1", "1. do stuff");
        let context = UltraplanContext::planning_turn(
            "ultraplan-run-1",
            "awaitingplanapproval",
            PolicyMode::Enforce,
        );
        app.ultraplan_status = Some(UltraplanStatus {
            run_id: "ultraplan-run-1".into(),
            phase: UltraplanPhase::AwaitingPlanApproval,
            task_title: "ship it".into(),
            started_at_ms: Some(1),
            worker_count: None,
            context: Some(context.clone()),
            round: 1,
            last_verdict: None,
            last_coverage: None,
            execution_reexploration_count: 0,
        });
        let outbound = exit_plan_mode_query(exit_plan_mode_options());
        let pending = build_pending_permission(&app, outbound);
        let mut pending_permission = Some(pending);
        sync_permission_view(&mut app, &pending_permission);
        let policy_store = rebon_core::policy::PolicyStore::new();

        for _ in 0..3 {
            apply_permission_modal_action_for_session(
                &mut app,
                &session,
                &mut pending_permission,
                PermissionModalAction::MoveNext,
                &policy_store,
                ".",
            );
        }
        apply_permission_modal_action_for_session(
            &mut app,
            &session,
            &mut pending_permission,
            PermissionModalAction::Confirm,
            &policy_store,
            ".",
        );

        assert!(pending_permission.is_none());
        assert!(!app.coordinator_mode);
        assert!(!session.engine_half.coordinator_mode_handle.get());
        assert_eq!(app.permission_mode, PermissionMode::Default);
        assert!(app.deferred_internal_submit_payloads.is_empty());
        assert_eq!(
            app.ultraplan_status.as_ref().map(|status| status.phase),
            Some(UltraplanPhase::Executing)
        );
        assert!(app.rebon_tui.transcript.rows().iter().any(
            |message| matches!(message, rebon_tui::Message::User(user) if user.plan_content.as_deref() == Some("1. do stuff"))
        ));
    }

    #[test]
    fn exit_plan_draft_submission_accepts_a_submission_without_a_ledger_revision() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "revisionless-submission".into();
        let run_id = "revisionless-submission-run";
        let state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "ship it".into(),
            None,
            1,
        );
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));
        let plan = "P1. Ship it\n- files: src.rs\n- change: update\n- verify: cargo test";

        // No question asked, no ledger, no coverage, no revision: persisted.
        let saved = crate::session::ultraplan_run::persist_ultraplan_exit_plan_draft_submission(
            &mut app.ultraplan_status,
            &[],
            Some(&session),
            plan,
            &[],
        )
        .expect("draft is persisted");

        assert_eq!(saved.last_plan_draft.as_deref(), Some(plan));
        let persisted =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
                .unwrap();
        assert_eq!(persisted.last_plan_draft.as_deref(), Some(plan));
        assert_eq!(persisted.execution_cards.len(), 1);
    }

    #[test]
    fn ultraplan_cas_mutation_reloads_once_without_losing_concurrent_state() {
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "cas-mutation-reload".into();
        let run_id = "cas-mutation-run";
        let state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "task".into(),
            None,
            1,
        );
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        let mut attempts = 0;

        let saved = crate::session::ultraplan_run::mutate_ultraplan_run_cas(
            &session.session,
            run_id,
            |state| {
                attempts += 1;
                if attempts == 1 {
                    let mut concurrent = rebon_session::load_ultraplan_run(
                        &session.projects_root,
                        &session.cwd,
                        run_id,
                    )
                    .unwrap();
                    concurrent.phase = RunPhase::Reviewing;
                    // A concurrent CAS writer always advances state_revision.
                    concurrent.state_revision = concurrent.state_revision.saturating_add(1);
                    concurrent.prepare_for_persist();
                    rebon_session::save_ultraplan_run(
                        &session.projects_root,
                        &session.cwd,
                        &concurrent,
                    )
                    .unwrap();
                }
                state.round = state.round.saturating_add(1);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(attempts, 2);
        assert_eq!(saved.phase, RunPhase::Reviewing);
        assert_eq!(saved.round, 2);
    }

    #[test]
    fn missing_active_standard_state_fails_exit_plan_closed() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "missing-standard-state".into();
        let run_id = "missing-standard-run";
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));
        let (response_tx, mut response_rx) = oneshot::channel();
        let outbound = OutboundPermissionQuery {
            id: 98,
            tool_name: "ExitPlanMode".into(),
            tool_call_id: "tool-exit-missing-standard".into(),
            session_id: session.session_id.clone(),
            title: "Review plan".into(),
            message: "Review the plan".into(),
            tool_input: Some(json!({"plan": "P1. Ship it"})),
            metadata: None,
            options: exit_plan_mode_options(),
            response_tx,
        };

        // The gate reports its refusal; sending the answer is the caller's
        // half, exactly as `drain_pending_permissions` does it.
        match maybe_gate_ultraplan_exit_plan_mode(
            &mut app.ultraplan_status,
            &[],
            &session,
            outbound,
        ) {
            ExitPlanGateOutcome::Reject { outbound, feedback } => {
                send_ultraplan_gate_rejection(&mut app, &session, outbound, feedback);
            }
            ExitPlanGateOutcome::Proceed(_) => panic!("the gate should have refused this plan"),
        }
        let answer = response_rx.try_recv().expect("gate rejection response");
        assert!(matches!(
            answer,
            PermissionAnswer::Selected { extra_text: Some(text), .. }
                if text.contains("ULTRAPLAN gate") && text.contains("missing or unreadable")
        ));
    }

    #[test]
    fn rejected_plan_records_the_hash_the_gate_refuses_to_resubmit() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "plan-rejection".into();
        let run_id = "plan-rejection-run";
        let plan = "P1. Ship it";
        let state = prepared_submitted_plan_state(&session, run_id, plan);
        let plan_hash = ultraplan_plan_hash_for_profile(UltraplanProfile::Standard, plan);
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));

        let state = persist_ultraplan_rejection_feedback(
            &mut app.ultraplan_status,
            Some(&session),
            Some("needs a safer rollback"),
        )
        .expect("rejection state");

        assert_eq!(
            state.user_rejected_plan_hash.as_deref(),
            Some(plan_hash.as_str())
        );
    }

    #[test]
    fn a_changed_plan_rebinds_the_persisted_draft() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "changed-after-submit".into();
        let run_id = "changed-after-submit-run";
        let plan = "P1. Ship it";
        let state = prepared_submitted_plan_state(&session, run_id, plan);
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));

        let changed_plan = "P1. Ship it gradually";
        crate::session::ultraplan_run::persist_ultraplan_exit_plan_draft(
            &mut app.ultraplan_status,
            Some(&session),
            changed_plan,
        );

        let state = rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
            .unwrap();
        assert_eq!(state.last_plan_draft.as_deref(), Some(changed_plan));
        assert_eq!(
            state.plan_hash.as_deref(),
            Some(
                ultraplan_plan_hash_for_profile(UltraplanProfile::Standard, changed_plan).as_str()
            )
        );
    }

    #[test]
    fn draft_hash_marker_change_is_non_material() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "marker-change".into();
        let run_id = "marker-change-run";
        let plan = "P1. Ship it";
        let state = prepared_submitted_plan_state(&session, run_id, plan);
        let plan_hash = ultraplan_plan_hash_for_profile(UltraplanProfile::Standard, plan);
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));

        let changed_plan = format!("ULTRAPLAN_DRAFT_HASH: arbitrary\n{plan}");
        crate::session::ultraplan_run::persist_ultraplan_exit_plan_draft(
            &mut app.ultraplan_status,
            Some(&session),
            &changed_plan,
        );

        let state = rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
            .unwrap();
        assert_eq!(state.plan_hash.as_deref(), Some(plan_hash.as_str()));
    }

    #[test]
    fn exit_plan_reaches_the_user_without_a_revision_question_or_review() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "free-form-delivery".into();
        let run_id = "ultraplan-free-form";
        let plan = "P1. Do work\n- files: src/lib.rs\n- change: implement\n- verify: cargo test";
        let state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "ship it".into(),
            None,
            1,
        );
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));
        let mut outbound = exit_plan_mode_query(exit_plan_mode_options());
        // No ledger_revision, no step_coverage, no asked question, no review.
        outbound.tool_input = Some(json!({ "plan": plan }));

        let delivered = match maybe_gate_ultraplan_exit_plan_mode(
            &mut app.ultraplan_status,
            &[],
            &session,
            outbound,
        ) {
            ExitPlanGateOutcome::Proceed(outbound) => outbound,
            ExitPlanGateOutcome::Reject { .. } => panic!("plan reaches the approval dialog"),
        };

        let input = delivered.tool_input.as_ref().expect("tool input");
        assert_eq!(input.get("plan").and_then(Value::as_str), Some(plan));
        let persisted =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
                .unwrap();
        // The runtime stamps the current revision so the approved payload
        // stays bound to this run head.
        assert_eq!(
            input.get("ledger_revision").and_then(Value::as_u64),
            Some(persisted.ledger_revision)
        );
        assert_eq!(persisted.last_plan_draft.as_deref(), Some(plan));
        assert_eq!(
            persisted.final_gate.as_ref().map(|gate| gate.outcome),
            Some(rebon_types::FinalGateOutcome::Pass)
        );
    }

    #[test]
    fn exit_plan_refuses_only_a_plan_the_user_already_rejected() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        session.cwd = "rejected-resubmission".into();
        let run_id = "ultraplan-rejected-resubmission";
        let plan = "P1. Do work\n- files: src/lib.rs\n- change: implement\n- verify: cargo test";
        let mut state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "ship it".into(),
            None,
            1,
        );
        state.user_rejected_plan_hash = Some(ultraplan_plan_hash_for_profile(
            UltraplanProfile::Standard,
            plan,
        ));
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));
        let mut outbound = exit_plan_mode_query(exit_plan_mode_options());
        outbound.tool_input = Some(json!({ "plan": plan }));
        let (response_tx, mut response_rx) = oneshot::channel();
        outbound.response_tx = response_tx;

        // The gate reports its refusal; sending the answer is the caller's
        // half, exactly as `drain_pending_permissions` does it.
        match maybe_gate_ultraplan_exit_plan_mode(
            &mut app.ultraplan_status,
            &[],
            &session,
            outbound,
        ) {
            ExitPlanGateOutcome::Reject { outbound, feedback } => {
                send_ultraplan_gate_rejection(&mut app, &session, outbound, feedback);
            }
            ExitPlanGateOutcome::Proceed(_) => panic!("the gate should have refused this plan"),
        }

        let answer = response_rx.try_recv().expect("gate rejection response");
        assert!(matches!(
            answer,
            PermissionAnswer::Selected { extra_text: Some(text), .. }
                if text.contains("already rejected")
        ));
    }

    #[test]
    fn ultraplan_exit_plan_draft_persists_execution_cards_and_hashes_before_execution() {
        let mut app = AppState::new();
        let mut session = make_test_tui_session();
        let projects_root = tempfile::tempdir().unwrap();
        session.projects_root = projects_root.path().to_path_buf();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("src.rs"), "before approval").unwrap();
        session.cwd = cwd.path().display().to_string();
        let run_id = "ultraplan-draft-cards";
        app.ultraplan_status = Some(test_ultraplan_status(run_id, 1));
        let mut state = UltraplanRunState::new(
            run_id.into(),
            session.session_id.clone(),
            "ship it".into(),
            None,
            10,
        );
        state.phase = RunPhase::AwaitingPlanApproval;
        state.record_interview_turn("Scope?".into(), None, "Narrow".into());
        rebon_session::save_ultraplan_run(&session.projects_root, &session.cwd, &state).unwrap();
        let plan = "P1. Update the file\n- files: src.rs\n- change: update the file\n- verify: cargo test -p rebon-cli draft_cards\n";

        crate::session::ultraplan_run::persist_ultraplan_exit_plan_draft(
            &mut app.ultraplan_status,
            Some(&session),
            plan,
        );

        let persisted =
            rebon_session::load_ultraplan_run(&session.projects_root, &session.cwd, run_id)
                .unwrap();
        assert_eq!(persisted.execution_cards.len(), 1);
        assert_eq!(persisted.execution_cards[0].step, "P1");
        assert_eq!(persisted.execution_cards[0].covers, None);
        assert_eq!(persisted.file_hashes.len(), 1);
        assert!(persisted
            .file_hashes
            .keys()
            .any(|path| path.ends_with("src.rs")));
        let status_cards = app
            .ultraplan_status
            .as_ref()
            .and_then(|status| status.context.as_ref())
            .map(|context| context.execution_cards.len());
        assert_eq!(status_cards, Some(1));
    }
}
