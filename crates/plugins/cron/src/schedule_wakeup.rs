//! `ScheduleWakeup` tool — the agent schedules its own next turn, a number of
//! seconds from now, so a loop can pace itself to what it is waiting on.
//!
//! It is the session's one wakeup slot (`SessionCronStore::set_wakeup`):
//! arming again replaces it, and `stop` disarms it. The scheduler's session
//! tick delivers it like a fired cron task — a turn marked as scheduled, not
//! typed by the user — through the same idle paths.

use async_trait::async_trait;
use rebon_tool::cron::tasks::{cron_disabled, now_ms, SessionCronStore};
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{
    parse_tool_input, validation_outcome_from, ToolError, ToolId, ToolInputSchema, ToolResult,
    ValidationOutcome,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub const SCHEDULE_WAKEUP_TOOL_NAME: &str = "ScheduleWakeup";

/// The shortest delay. A wakeup costs a whole turn; anything sooner is
/// polling, and work the session started itself (a background agent or
/// shell) already wakes it when it finishes.
pub const MIN_WAKEUP_DELAY_SECONDS: u64 = 60;
/// The longest delay. Past an hour, a cron schedule says what is meant, and
/// a hosted worker stays alive for an armed wakeup only this long.
pub const MAX_WAKEUP_DELAY_SECONDS: u64 = 3600;

#[derive(Debug, Clone, Default)]
pub struct ScheduleWakeupTool;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    #[serde(default)]
    delay_seconds: Option<f64>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    stop: bool,
}

/// What a valid request asks for.
#[derive(Debug, Clone, PartialEq)]
enum Wakeup {
    Arm {
        delay_seconds: u64,
        prompt: String,
        reason: String,
    },
    Stop,
}

#[async_trait]
impl Tool for ScheduleWakeupTool {
    fn id(&self) -> ToolId {
        ToolId::new(SCHEDULE_WAKEUP_TOOL_NAME)
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn search_hint(&self) -> Option<&str> {
        Some("schedule wakeup loop self-paced resume later delay seconds")
    }

    fn description(&self) -> &str {
        "Schedule this session's next turn yourself, a number of seconds from now — for a \
         self-paced loop (`/loop` with no interval) or for waiting on something outside \
         this session that you will want to check on. At that time the prompt is delivered \
         as a turn marked as a scheduled task; it is not new user consent.\n\
         \n\
         ## How it works\n\
         - The session has one wakeup. Calling again replaces it, so a loop re-arms it each \
         turn with the same prompt. To end the loop, call with `stop: true` (or simply do \
         not re-arm it).\n\
         - The delay is clamped to [60, 3600] seconds; the result says what was used.\n\
         - Session-only: the wakeup is gone if this rebon process exits.\n\
         \n\
         ## Picking the delay\n\
         - Match it to what you are waiting on: a CI run that takes ~8 minutes deserves one \
         check at ~480s, not eight at 60s.\n\
         - Do not schedule a wakeup to wait for a background agent or shell you started: \
         its completion already wakes you.\n\
         - With nothing specific to watch, prefer 1200–1800s.\n\
         \n\
         ## reason\n\
         One short sentence on why this delay, shown to the user — \"watching the CI run\" \
         beats \"waiting\"."
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "delaySeconds": {
                    "type": "number",
                    "description": "Seconds from now to wake up. Clamped to [60, 3600]. Required unless `stop` is true."
                },
                "prompt": {
                    "type": "string",
                    "description": "The prompt delivered at wakeup. For a /loop, pass the same loop input back every turn. Required unless `stop` is true."
                },
                "reason": {
                    "type": "string",
                    "description": "One short sentence explaining the chosen delay, shown to the user. Required unless `stop` is true."
                },
                "stop": {
                    "type": "boolean",
                    "description": "true = disarm the pending wakeup and end the loop; every other field is ignored."
                }
            },
            "additionalProperties": false
        })
    }

    fn needs_permission(&self, _input: &Value) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        if let Err(refused) = parse(input) {
            return validation_outcome_from(Err::<(), _>(refused));
        }
        if let Err(refused) = session_store(context) {
            return validation_outcome_from(Err::<(), _>(refused));
        }
        Ok(ValidationOutcome::valid())
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let request = parse(&input)?;
        let store = session_store(context)?;
        Ok(apply(&store, request, now_ms()))
    }
}

/// Parse and check a request: `stop`, or a delay, prompt and reason.
fn parse(input: &Value) -> ToolResult<Wakeup> {
    let tool = ToolId::new(SCHEDULE_WAKEUP_TOOL_NAME);
    let request: Request = parse_tool_input(tool.clone(), input)?;
    if request.stop {
        return Ok(Wakeup::Stop);
    }
    let missing = |field: &str| ToolError::InvalidInput {
        tool: tool.clone(),
        reason: format!("ScheduleWakeup needs `{field}` unless `stop` is true."),
        error_code: Some(400),
    };
    let delay = request
        .delay_seconds
        .filter(|delay| delay.is_finite())
        .ok_or_else(|| missing("delaySeconds"))?;
    let prompt = request
        .prompt
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or_else(|| missing("prompt"))?;
    let reason = request
        .reason
        .filter(|reason| !reason.trim().is_empty())
        .ok_or_else(|| missing("reason"))?;
    Ok(Wakeup::Arm {
        delay_seconds: clamp_delay(delay),
        prompt,
        reason: reason.trim().to_string(),
    })
}

/// `delay` seconds as the scheduler uses them: whole, within the bounds.
fn clamp_delay(delay: f64) -> u64 {
    let bounded = delay.round().clamp(
        MIN_WAKEUP_DELAY_SECONDS as f64,
        MAX_WAKEUP_DELAY_SECONDS as f64,
    );
    bounded as u64
}

/// The session's store, or why this caller cannot schedule a wakeup. The
/// codes match `CronCreate`'s for the same refusals.
fn session_store(context: &ToolContext) -> ToolResult<Arc<SessionCronStore>> {
    let refuse = |reason: &str, code: i64| ToolError::InvalidInput {
        tool: ToolId::new(SCHEDULE_WAKEUP_TOOL_NAME),
        reason: reason.to_string(),
        error_code: Some(code),
    };
    if cron_disabled() {
        return Err(refuse("Scheduling is disabled by REBON_DISABLE_CRON.", 4));
    }
    if context.team_identity().is_some() {
        return Err(refuse(
            "ScheduleWakeup is only supported from the main session; a teammate is woken by its lead.",
            5,
        ));
    }
    context.session_cron_store().cloned().ok_or_else(|| {
        refuse(
            "ScheduleWakeup is unavailable in this runtime: it has no session scheduler.",
            6,
        )
    })
}

/// Arm or disarm the wakeup at `now`, and say what happened.
fn apply(store: &SessionCronStore, request: Wakeup, now: i64) -> Value {
    match request {
        Wakeup::Stop => {
            let content = match store.clear_wakeup() {
                Some(wakeup) => format!("Stopped: the wakeup {} will not fire.", wakeup.id),
                None => "No wakeup was scheduled; nothing to stop.".to_string(),
            };
            json!({"stopped": true, "content": content})
        }
        Wakeup::Arm {
            delay_seconds,
            prompt,
            reason,
        } => {
            let replaced = store.wakeup();
            let delay_ms = i64::try_from(delay_seconds * 1000).expect("at most an hour of ms");
            let wakeup = store.set_wakeup(&prompt, &reason, now, delay_ms);
            let replaced = replaced
                .map(|old| format!(" It replaces wakeup {}.", old.id))
                .unwrap_or_default();
            json!({
                "id": wakeup.id,
                "delaySeconds": delay_seconds,
                "fireAt": wakeup.fire_at,
                "reason": wakeup.reason,
                "content": format!(
                    "Wakeup {} scheduled in {delay_seconds}s ({}).{replaced} It is session-only.",
                    wakeup.id, wakeup.reason
                ),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_context(store: &Arc<SessionCronStore>) -> ToolContext {
        ToolContext::new().with_session_cron_store(store.clone())
    }

    fn arm(delay: f64) -> Value {
        json!({"delaySeconds": delay, "prompt": "/loop check CI", "reason": "CI takes ~8m"})
    }

    #[test]
    fn delays_are_rounded_and_clamped_to_the_bounds() {
        for (asked, used) in [
            (0.0, 60),
            (-5.0, 60),
            (59.4, 60),
            (60.0, 60),
            (480.0, 480),
            (480.6, 481),
            (3600.0, 3600),
            (86_400.0, 3600),
            (f64::MAX, 3600),
        ] {
            assert_eq!(clamp_delay(asked), used, "{asked}");
        }
    }

    #[test]
    fn a_request_needs_delay_prompt_and_reason_unless_it_stops() {
        assert_eq!(
            parse(&arm(480.0)).unwrap(),
            Wakeup::Arm {
                delay_seconds: 480,
                prompt: "/loop check CI".into(),
                reason: "CI takes ~8m".into(),
            }
        );
        assert_eq!(parse(&json!({"stop": true})).unwrap(), Wakeup::Stop);
        assert_eq!(
            parse(&json!({"stop": true, "delaySeconds": 5})).unwrap(),
            Wakeup::Stop,
            "stop ignores the other fields"
        );
        for (input, field) in [
            (json!({"prompt": "p", "reason": "r"}), "delaySeconds"),
            (json!({"delaySeconds": 60, "reason": "r"}), "prompt"),
            (
                json!({"delaySeconds": 60, "prompt": "  ", "reason": "r"}),
                "prompt",
            ),
            (json!({"delaySeconds": 60, "prompt": "p"}), "reason"),
            (
                json!({"delaySeconds": 60, "prompt": "p", "reason": ""}),
                "reason",
            ),
            (
                json!({"stop": false, "prompt": "p", "reason": "r"}),
                "delaySeconds",
            ),
        ] {
            let err = parse(&input).expect_err(field);
            assert!(err.to_string().contains(field), "{input} → {err}");
        }
    }

    #[test]
    fn malformed_input_is_invalid_not_a_panic() {
        for input in [
            json!("not an object"),
            json!({"delaySeconds": "soon", "prompt": "p", "reason": "r"}),
            json!({"delaySeconds": 60, "prompt": "p", "reason": "r", "extra": 1}),
        ] {
            assert!(
                matches!(parse(&input), Err(ToolError::InvalidInput { .. })),
                "{input}"
            );
        }
    }

    #[tokio::test]
    async fn arming_sets_the_sessions_one_wakeup_and_rearming_replaces_it() {
        let store = SessionCronStore::new();
        let context = tool_context(&store);
        let first = ScheduleWakeupTool.call(arm(480.0), &context).await.unwrap();
        assert_eq!(first["delaySeconds"], 480);
        let armed = store.wakeup().expect("armed");
        assert_eq!(armed.prompt, "/loop check CI");
        assert_eq!(armed.fire_at - armed.created_at, 480_000);

        let second = ScheduleWakeupTool.call(arm(10.0), &context).await.unwrap();
        assert_eq!(second["delaySeconds"], 60, "clamped up");
        let content = second["content"].as_str().unwrap();
        assert!(
            content.contains(&format!("replaces wakeup {}", armed.id)),
            "{content}"
        );
        assert_ne!(store.wakeup().unwrap().id, armed.id);
        assert!(store.list().is_empty(), "a wakeup is not a cron task");
    }

    #[tokio::test]
    async fn stop_disarms_the_wakeup_and_says_when_there_was_none() {
        let store = SessionCronStore::new();
        let context = tool_context(&store);
        ScheduleWakeupTool.call(arm(600.0), &context).await.unwrap();
        let stopped = ScheduleWakeupTool
            .call(json!({"stop": true}), &context)
            .await
            .unwrap();
        assert!(store.wakeup().is_none());
        assert!(stopped["content"]
            .as_str()
            .unwrap()
            .contains("will not fire"));

        let again = ScheduleWakeupTool
            .call(json!({"stop": true}), &context)
            .await
            .unwrap();
        assert!(again["content"]
            .as_str()
            .unwrap()
            .contains("nothing to stop"));
    }

    #[tokio::test]
    async fn a_runtime_without_a_session_scheduler_refuses() {
        let context = ToolContext::new();
        let outcome = ScheduleWakeupTool
            .validate_input(&arm(60.0), &context)
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        assert!(matches!(
            ScheduleWakeupTool.call(arm(60.0), &context).await,
            Err(ToolError::InvalidInput {
                error_code: Some(6),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn validation_accepts_what_call_accepts() {
        let store = SessionCronStore::new();
        let context = tool_context(&store);
        for input in [arm(60.0), json!({"stop": true})] {
            let outcome = ScheduleWakeupTool
                .validate_input(&input, &context)
                .await
                .unwrap();
            assert!(outcome.is_valid(), "{input}");
        }
        let outcome = ScheduleWakeupTool
            .validate_input(&json!({"prompt": "p"}), &context)
            .await
            .unwrap();
        assert!(!outcome.is_valid());
        assert!(store.wakeup().is_none(), "validation arms nothing");
    }
}
