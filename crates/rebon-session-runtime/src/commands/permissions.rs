//! `/permissions`: what auto mode refused, and replaying one.
//!
//! The text surface for the denial store the engine's auto-mode path
//! seeds. A full modal dialog has not been implemented yet; a terse
//! listing is enough to make the chain classifier-deny -> record ->
//! review -> mark-retried usable from the terminal today, and a modal
//! can arrive later without changing the store contract.
//!
//! Subcommands:
//!
//! * `/permissions` -- list pending and recent denials
//! * `/permissions approve <id>` -- mark as Approved
//! * `/permissions retry <id>` -- mark as Retried (replay request)
//! * `/permissions clear` -- drop every resolved record
//! * `/permissions clear all` -- drop every record

use rebon_agent_core::DenialReplayRequest;
use rebon_permissions::auto_mode_denials::PermissionsCloseSelection;

use super::SessionCommandInputs;
use rebon_slash_commands::strip_command_prefix;

pub struct PermissionsCommandResult {
    pub text: String,
    pub replay_requests: Vec<DenialReplayRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionsCommand {
    List,
    Approve(String),
    Retry(String),
    ClearResolved,
    ClearAll,
}

pub fn parse_permissions_command(text: &str) -> Option<PermissionsCommand> {
    let rest = strip_command_prefix(text.trim_end(), "permissions")?;
    if rest.is_empty() {
        return Some(PermissionsCommand::List);
    }
    let rest = rest.strip_prefix(' ')?.trim();
    let mut parts = rest.split_whitespace();
    match parts.next()? {
        "approve" => parts
            .next()
            .map(|id| PermissionsCommand::Approve(id.to_string())),
        "retry" => parts
            .next()
            .map(|id| PermissionsCommand::Retry(id.to_string())),
        "clear" => match parts.next() {
            Some("all") => Some(PermissionsCommand::ClearAll),
            None => Some(PermissionsCommand::ClearResolved),
            _ => None,
        },
        _ => None,
    }
}

pub fn execute_permissions_command(
    inputs: &SessionCommandInputs,
    cmd: PermissionsCommand,
) -> PermissionsCommandResult {
    let mut guard = match inputs.auto_mode_denials.lock() {
        Ok(g) => g,
        Err(_) => {
            return PermissionsCommandResult {
                text: "! denial store lock poisoned".into(),
                replay_requests: Vec::new(),
            };
        }
    };
    let mut replay_requests = Vec::new();
    let text = match cmd {
        PermissionsCommand::List => format_denials(&guard),
        PermissionsCommand::Approve(id) => {
            if guard.mark_approved(&id) {
                // The user reviewed this exact invocation: install a
                // one-shot fingerprint exemption so the model's own retry
                // of the identical call slides through the auto-mode gate
                // once, instead of replaying the cached denial forever.
                if let Some(record) = guard.get(&id) {
                    inputs
                        .auto_mode_verdicts
                        .exempt_once(&record.tool_name, &record.tool_input);
                }
                format!("✓ marked {id} as approved — the next identical call is allowed once")
            } else {
                format!("! no pending denial with id {id}")
            }
        }
        PermissionsCommand::Retry(id) => {
            let selection = PermissionsCloseSelection {
                approved_ids: Vec::new(),
                retry_ids: vec![id.clone()],
            };
            let outcome =
                rebon_permissions::auto_mode_denials::resolve_denials_for_permissions_close(
                    &mut guard, &selection,
                );
            if outcome.missing_ids.iter().any(|missing| missing == &id) {
                format!("! no pending/approved denial with id {id}")
            } else if outcome.to_retry.is_empty() {
                format!("! no pending/approved denial with id {id}")
            } else {
                replay_requests = outcome
                    .to_retry
                    .into_iter()
                    .map(|request| DenialReplayRequest {
                        denial_id: request.denial_id,
                        tool_use_id: request.tool_use_id,
                        tool_name: request.tool_name,
                        tool_input: request.tool_input,
                        reason: request.reason,
                        task_id: request.task_id,
                        conversation_id: request.conversation_id,
                    })
                    .collect();
                format!("↻ retrying {id}")
            }
        }
        PermissionsCommand::ClearResolved => {
            let n = guard.clear_resolved();
            format!("cleared {n} resolved denial(s)")
        }
        PermissionsCommand::ClearAll => {
            let n = guard.len();
            guard.clear_all();
            format!("cleared {n} denial(s)")
        }
    };
    PermissionsCommandResult {
        text,
        replay_requests,
    }
}

fn format_denials(store: &rebon_permissions::auto_mode_denials::AutoModeDenialStore) -> String {
    if store.len() == 0 {
        return "No auto-mode denials captured in this session.".into();
    }
    let mut lines = Vec::with_capacity(store.len() + 2);
    lines.push(format!(
        "Auto-mode denials: {} total. Use `/permissions approve <id>` or `/permissions retry <id>`.",
        store.len()
    ));
    for d in store.iter() {
        lines.push(format!(
            "  [{}] {} — {} ({})",
            d.status.as_wire(),
            d.id,
            d.display,
            d.reason
        ));
    }
    lines.join("\n")
}
