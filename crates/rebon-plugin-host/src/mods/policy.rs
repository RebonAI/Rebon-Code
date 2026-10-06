//! The mods as one subscriber on the policy seat.
//!
//! Every hook event rebon raises — a tool about to run, a prompt submitted,
//! a turn stopped — reaches the mods here. For each loaded mod whose hooks
//! module listens for the event (natively, as `classic.<Event>`, or both),
//! the subscriber calls the mod's service with the event as a settings hook
//! would read it on stdin, and the mod answers a list of settings-hook JSON
//! outputs. Those are parsed, aggregated and projected by the hook runtime's
//! own functions, so what a mod may do to an event is exactly what a
//! configured hook may do, and the effects reach the emit site unchanged.
//!
//! The subscriber is one entry on the seat rather than one per mod: the
//! mods answer in load order and their outputs aggregate like several hooks
//! on one matcher do, with the same precedence rules.

use std::sync::{Arc, Weak};
use std::time::Duration;

use rebon_core::policy_seat::{
    PolicyEventKind, PolicyFuture, PolicyRequest, PolicySubscriber, Verdict,
};
use rebon_hooks::output_protocol::SyncHookJsonOutput;
use rebon_hooks::runtime_result::{aggregate_hook_results, process_hook_json_output, HookResult};
use rebon_hooks::{project_effects, HookEffect};
use serde_json::Value;

use super::{ModRecord, ModsRegistry};

/// The subscriber id on the seat.
pub const MODS_SUBSCRIBER_ID: &str = "mods";

/// How long one mod may take to answer one event. Under the plane's unary
/// bound, and well under the seat's own five-minute backstop.
pub const MOD_EVENT_BUDGET: Duration = Duration::from_secs(20);

/// The native event the Node bridge raises for a classic one, when it
/// raises one; the subscriber asks only the mods that listen to either.
pub fn native_event_of(kind: PolicyEventKind) -> Option<&'static str> {
    use rebon_hooks::HookEvent;
    Some(match kind {
        HookEvent::PreToolUse
        | HookEvent::PostToolUse
        | HookEvent::PostToolUseFailure
        | HookEvent::PermissionDenied => "tool.call",
        HookEvent::PermissionRequest => "tool.check",
        HookEvent::UserPromptSubmit => "prompt.submit",
        HookEvent::Stop => "turn.complete",
        HookEvent::PreCompact => "session.compact",
        HookEvent::SessionEnd => "session.end",
        _ => return None,
    })
}

/// Whether a mod wants to hear this event at all.
pub fn mod_listens(record: &ModRecord, kind: PolicyEventKind) -> bool {
    let classic = format!("classic.{}", kind.name());
    if record.hooks_event(&classic) {
        return true;
    }
    native_event_of(kind).is_some_and(|native| record.hooks_event(native))
}

pub struct ModsPolicySubscriber {
    registry: Weak<ModsRegistry>,
}

impl ModsPolicySubscriber {
    pub fn new(registry: &Arc<ModsRegistry>) -> Arc<Self> {
        Arc::new(Self {
            registry: Arc::downgrade(registry),
        })
    }
}

/// The event as a settings hook reads it: the payload's own fields under
/// `hook_event_name`, plus the envelope every hook gets.
pub fn classic_input(request: &PolicyRequest) -> Value {
    let mut input = serde_json::to_value(&request.payload)
        .unwrap_or_else(|_| Value::Object(Default::default()));
    if let Some(object) = input.as_object_mut() {
        let context = &request.context;
        object.insert(
            "session_id".into(),
            Value::String(context.session_id.clone()),
        );
        object.insert("cwd".into(), Value::String(context.cwd.clone()));
        object.insert(
            "transcript_path".into(),
            Value::String(context.transcript_path.clone()),
        );
        if let Some(mode) = context.permission_mode {
            object.insert(
                "permission_mode".into(),
                serde_json::to_value(mode).unwrap_or(Value::Null),
            );
        }
        if let Some(agent) = request.agent.as_ref().or(context.agent_id.as_ref()) {
            object.insert("agent_id".into(), Value::String(agent.clone()));
        }
        if let Some(agent_type) = &context.agent_type {
            object.insert("agent_type".into(), Value::String(agent_type.clone()));
        }
    }
    input
}

/// Turns the outputs one mod answered into hook results, naming the mod
/// as the "command" a refusal is attributed to.
pub fn results_of(mod_name: &str, kind: PolicyEventKind, outputs: &[Value]) -> Vec<HookResult> {
    let command = format!("mod:{mod_name}");
    let mut results = Vec::new();
    for output in outputs {
        let json: SyncHookJsonOutput = match serde_json::from_value(output.clone()) {
            Ok(json) => json,
            Err(error) => {
                tracing::warn!(mod_ = %mod_name, event = kind.name(), %error, "a mod answered a hook output rebon cannot read");
                continue;
            }
        };
        match process_hook_json_output(&json, &command, Some(kind)) {
            Ok(result) => results.push(result),
            Err(error) => {
                tracing::warn!(mod_ = %mod_name, event = kind.name(), %error, "a mod answered for another event");
            }
        }
    }
    results
}

/// The effects every mod's outputs come to, aggregated as one hook run.
pub fn effects_of(kind: PolicyEventKind, results: &[HookResult]) -> Vec<HookEffect> {
    if results.is_empty() {
        return Vec::new();
    }
    let aggregated = aggregate_hook_results(results);
    project_effects(kind, &aggregated)
}

/// What the mod commands run since the last prompt answered, as context
/// the model reads with this one: in Claude Code a command's output row is
/// part of the conversation, and here the prompt is where it can go.
pub fn command_context_effects(outputs: Vec<String>) -> Vec<HookEffect> {
    outputs
        .into_iter()
        .map(|text| HookEffect::InjectContext { text })
        .collect()
}

impl PolicySubscriber for ModsPolicySubscriber {
    fn interest(&self, kind: PolicyEventKind) -> bool {
        let Some(registry) = self.registry.upgrade() else {
            return false;
        };
        (kind == rebon_hooks::HookEvent::UserPromptSubmit && registry.has_command_context())
            || registry
                .mods()
                .iter()
                .any(|record| mod_listens(record, kind))
    }

    fn budget(&self) -> Option<Duration> {
        Some(MOD_EVENT_BUDGET + Duration::from_secs(5))
    }

    fn decide<'a>(&'a self, request: &'a PolicyRequest) -> PolicyFuture<'a> {
        Box::pin(async move {
            let Some(registry) = self.registry.upgrade() else {
                return Verdict::Allow;
            };
            let kind = request.kind();
            let input = classic_input(request);
            let mut results = Vec::new();
            for record in registry.mods() {
                if !mod_listens(&record, kind) {
                    continue;
                }
                let ask = serde_json::json!({
                    "kind": "classic",
                    "event": kind.name(),
                    "input": input,
                });
                match registry
                    .call_mod_bounded(&record.id, ask, Some(MOD_EVENT_BUDGET))
                    .await
                {
                    Ok(answer) => {
                        let outputs = answer
                            .get("outputs")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        results.extend(results_of(&record.name, kind, &outputs));
                    }
                    Err(error) => {
                        // One mod failing to answer is that mod's hooks not
                        // running, the same as a settings hook that crashed;
                        // the others still answer. The debug log says so.
                        tracing::warn!(mod_ = %record.name, event = kind.name(), %error, "a mod did not answer a hook event");
                        registry.ui.push_log(
                            &record.id,
                            format!("{}: {} did not run: {error}", record.name, kind.name()),
                            "debug",
                        );
                    }
                }
            }
            let mut effects = effects_of(kind, &results);
            if kind == rebon_hooks::HookEvent::UserPromptSubmit {
                effects.extend(command_context_effects(registry.take_command_context()));
            }
            if effects.is_empty() {
                Verdict::Allow
            } else {
                Verdict::Modify { effects }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_hooks::{HookEvent, HookEventPayload, HookInvocationContext};
    use serde_json::json;

    #[test]
    fn waiting_command_outputs_become_context_the_prompt_carries() {
        let effects = command_context_effects(vec!["one".into(), "two".into()]);
        assert_eq!(
            effects,
            vec![
                HookEffect::InjectContext { text: "one".into() },
                HookEffect::InjectContext { text: "two".into() },
            ]
        );
        match rebon_core::hooks::apply_user_prompt_submit_effects(&effects) {
            rebon_core::hooks::UserPromptSubmitDecision::Continue(applied) => {
                assert_eq!(applied.additional_context, vec!["one", "two"]);
            }
            other => panic!("the prompt runs, carrying them: {other:?}"),
        }
        assert!(command_context_effects(Vec::new()).is_empty());
    }

    #[test]
    fn the_input_is_the_stdin_shape_with_the_envelope() {
        let request = PolicyRequest::new(
            HookInvocationContext {
                cwd: "/w".into(),
                transcript_path: "/t.jsonl".into(),
                session_id: "s1".into(),
                permission_mode: None,
                agent_id: None,
                agent_type: None,
            },
            HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: json!({ "command": "ls" }),
                tool_use_id: "t1".into(),
            },
        )
        .in_agent("worker-1");
        let input = classic_input(&request);
        assert_eq!(input["hook_event_name"], json!("PreToolUse"));
        assert_eq!(input["tool_name"], json!("Bash"));
        assert_eq!(input["tool_input"]["command"], json!("ls"));
        assert_eq!(input["session_id"], json!("s1"));
        assert_eq!(input["cwd"], json!("/w"));
        assert_eq!(input["transcript_path"], json!("/t.jsonl"));
        assert_eq!(input["agent_id"], json!("worker-1"));
    }

    #[test]
    fn outputs_project_into_the_effects_a_settings_hook_would_produce() {
        let outputs = vec![
            json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "no" } }),
            json!({ "systemMessage": "seen" }),
            json!({ "hookSpecificOutput": { "hookEventName": "UserPromptSubmit", "replacementPrompt": "x" } }),
            json!("not an object"),
        ];
        let results = results_of("counter", HookEvent::PreToolUse, &outputs);
        // The UserPromptSubmit answer names another event and is dropped,
        // and the string is not an output at all.
        assert_eq!(results.len(), 2);
        let effects = effects_of(HookEvent::PreToolUse, &results);
        assert!(matches!(&effects[0], HookEffect::BlockToolCall { reason, .. } if reason == "no"));
        assert!(effects
            .iter()
            .any(|e| matches!(e, HookEffect::SystemMessage { text } if text == "seen")));

        let prompt = results_of(
            "counter",
            HookEvent::UserPromptSubmit,
            &[
                json!({ "hookSpecificOutput": { "hookEventName": "UserPromptSubmit", "replacementPrompt": "HELLO", "additionalContext": "c" } }),
            ],
        );
        let effects = effects_of(HookEvent::UserPromptSubmit, &prompt);
        assert!(effects
            .iter()
            .any(|e| matches!(e, HookEffect::ReplacePrompt { text } if text == "HELLO")));
        assert!(effects
            .iter()
            .any(|e| matches!(e, HookEffect::InjectContext { text } if text == "c")));
        assert!(effects_of(HookEvent::Stop, &[]).is_empty());
    }

    #[test]
    fn a_mod_is_asked_only_for_events_its_module_listens_to() {
        let record = ModRecord::for_test("a", &["tool.call", "classic.Stop"]);
        assert!(mod_listens(&record, HookEvent::PreToolUse));
        assert!(mod_listens(&record, HookEvent::PostToolUse));
        assert!(mod_listens(&record, HookEvent::Stop));
        assert!(!mod_listens(&record, HookEvent::UserPromptSubmit));
        assert!(!mod_listens(&record, HookEvent::Notification));
        let all = ModRecord::for_test("b", &["*"]);
        assert!(mod_listens(&all, HookEvent::Notification));
        let classic = ModRecord::for_test("c", &["classic.*"]);
        assert!(mod_listens(&classic, HookEvent::SessionStart));
    }
}
