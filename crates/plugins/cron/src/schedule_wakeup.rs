//! `ScheduleWakeup` tool — the agent schedules its own next turn, a number of
//! seconds from now, so a loop can pace itself to what it is waiting on.
//!
//! It is the session's one wakeup slot (`SessionCronStore::arm_wakeup`):
//! arming again replaces it, and `stop` disarms it. Each loop has a budget —
//! how long it may run and how many wakeups it may fire — that its re-arms
//! inherit. The scheduler's session tick delivers a wakeup like a fired cron
//! task — a turn marked as scheduled, not typed by the user — through the same
//! idle paths, and a handoff between terminal and worker carries it along.

use async_trait::async_trait;
use rebon_tool::cron::tasks::{
    cron_disabled, now_ms, NewWakeupBudget, SessionCronStore, WakeupBudget, WakeupRefused,
};
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
/// The longest a loop's budget may run: a week, the same age at which a
/// recurring cron task expires.
pub const MAX_WAKEUP_BUDGET_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Error code for "this loop's budget is spent": distinct from the setup
/// refusals (4–6) because the right response differs — end the loop.
const BUDGET_SPENT_CODE: i64 = 7;

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
    budget_seconds: Option<f64>,
    #[serde(default)]
    max_wakeups: Option<u32>,
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
        /// `Some` starts a new loop with this budget.
        new_budget: Option<NewWakeupBudget>,
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
        Some("schedule wakeup loop self-paced resume later delay seconds budget")
    }

    fn description(&self) -> &str {
        "Schedule this session's next turn yourself, a number of seconds from now — for a \
         self-paced loop (`/loop` with no interval) or for coming back to something that \
         needs your judgment to check. At that time the prompt is delivered as a turn \
         marked as a scheduled task; it is not new user consent.\n\
         \n\
         ## Not for\n\
         - Waiting on a background agent or shell you started: its completion already \
         wakes you.\n\
         - Waiting on a condition a command can test (a port answering, a file appearing, \
         a log line, a CI status changing): use `Monitor`, which wakes you the moment it \
         happens instead of you checking on a timer.\n\
         \n\
         ## How it works\n\
         - The session has one wakeup. Calling again replaces it, so a loop re-arms it each \
         turn with the same prompt. To end the loop, call with `stop: true` (or simply do \
         not re-arm it).\n\
         - The delay is clamped to [60, 3600] seconds; the result says what was used.\n\
         - Session-only, but it moves with the session when a terminal hands it to a \
         hosted worker or takes it back.\n\
         \n\
         ## Budget\n\
         Each loop has a budget its re-arms inherit: `budgetSeconds` (how long the loop may \
         run; default one day, at most a week) and `maxWakeups` (how many wakeups may fire; \
         default unlimited). Passing either starts a new loop with that budget — do so on a \
         loop's first arming, with the limits the user gave, and never on later ones. A \
         wakeup due past the deadline is moved up to it, so the last turn can report. Once \
         the budget is spent arming is refused: tell the user the loop ended and why.\n\
         \n\
         ## Picking the delay\n\
         - Match it to what you are waiting on: a CI run that takes ~8 minutes deserves one \
         check at ~480s, not eight at 60s.\n\
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
                "budgetSeconds": {
                    "type": "number",
                    "description": "Start a new loop that may run this long (clamped to [60, 604800]; default 86400). Only on a loop's first arming."
                },
                "maxWakeups": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Start a new loop that may fire at most this many wakeups. Only on a loop's first arming."
                },
                "stop": {
                    "type": "boolean",
                    "description": "true = disarm the pending wakeup and end the loop and its budget; every other field is ignored."
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
        apply(&store, request, now_ms())
    }
}

/// Parse and check a request: `stop`, or a delay, prompt and reason, with an
/// optional new budget.
fn parse(input: &Value) -> ToolResult<Wakeup> {
    let tool = ToolId::new(SCHEDULE_WAKEUP_TOOL_NAME);
    let request: Request = parse_tool_input(tool.clone(), input)?;
    if request.stop {
        return Ok(Wakeup::Stop);
    }
    let invalid = |reason: String| ToolError::InvalidInput {
        tool: tool.clone(),
        reason,
        error_code: Some(400),
    };
    let missing = |field: &str| {
        invalid(format!(
            "ScheduleWakeup needs `{field}` unless `stop` is true."
        ))
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
    if request.max_wakeups == Some(0) {
        return Err(invalid("`maxWakeups` must be at least 1.".into()));
    }
    let budget_seconds = match request.budget_seconds {
        Some(seconds) if !seconds.is_finite() => {
            return Err(invalid(
                "`budgetSeconds` must be a number of seconds.".into(),
            ))
        }
        other => other,
    };
    let new_budget = (budget_seconds.is_some() || request.max_wakeups.is_some()).then(|| {
        let seconds = budget_seconds.map_or(
            rebon_tool::cron::tasks::DEFAULT_WAKEUP_BUDGET_MS / 1000,
            |seconds| {
                clamp_seconds(seconds, MIN_WAKEUP_DELAY_SECONDS, MAX_WAKEUP_BUDGET_SECONDS) as i64
            },
        );
        NewWakeupBudget {
            duration_ms: seconds * 1000,
            max_wakeups: request.max_wakeups,
        }
    });
    Ok(Wakeup::Arm {
        delay_seconds: clamp_seconds(delay, MIN_WAKEUP_DELAY_SECONDS, MAX_WAKEUP_DELAY_SECONDS),
        prompt,
        reason: reason.trim().to_string(),
        new_budget,
    })
}

/// `seconds`, whole, within `[min, max]`.
fn clamp_seconds(seconds: f64, min: u64, max: u64) -> u64 {
    seconds.round().clamp(min as f64, max as f64) as u64
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
fn apply(store: &SessionCronStore, request: Wakeup, now: i64) -> ToolResult<Value> {
    match request {
        Wakeup::Stop => {
            let content = match store.clear_wakeup() {
                Some(wakeup) => format!(
                    "Stopped: the wakeup {} will not fire, and the loop is over.",
                    wakeup.id
                ),
                None => "No wakeup was scheduled; nothing to stop.".to_string(),
            };
            Ok(json!({"stopped": true, "content": content}))
        }
        Wakeup::Arm {
            delay_seconds,
            prompt,
            reason,
            new_budget,
        } => {
            let replaced = store.wakeup();
            let delay_ms = i64::try_from(delay_seconds * 1000).expect("at most an hour of ms");
            let wakeup = store
                .arm_wakeup(&prompt, &reason, now, delay_ms, new_budget)
                .map_err(|refused| ToolError::InvalidInput {
                    tool: ToolId::new(SCHEDULE_WAKEUP_TOOL_NAME),
                    reason: budget_spent_message(&refused),
                    error_code: Some(BUDGET_SPENT_CODE),
                })?;
            let budget = store
                .wakeup_budget()
                .expect("an armed wakeup has a loop budget");
            let in_seconds = (wakeup.fire_at - now) / 1000;
            let moved_up = if in_seconds < delay_seconds as i64 {
                " (moved up to the loop's deadline: this is its last wakeup)"
            } else {
                ""
            };
            let replaced = replaced
                .map(|old| format!(" It replaces wakeup {}.", old.id))
                .unwrap_or_default();
            Ok(json!({
                "id": wakeup.id,
                "delaySeconds": in_seconds,
                "fireAt": wakeup.fire_at,
                "reason": wakeup.reason,
                "budget": budget_json(&budget, now),
                "content": format!(
                    "Wakeup {} scheduled in {in_seconds}s{moved_up} ({}).{replaced} {}",
                    wakeup.id,
                    wakeup.reason,
                    budget_summary(&budget, now),
                ),
            }))
        }
    }
}

/// What the model is told when the loop may not go on.
fn budget_spent_message(refused: &WakeupRefused) -> String {
    let why = match refused {
        WakeupRefused::WakeupsSpent { fired, max } => {
            format!("it has fired {fired} of its {max} wakeups")
        }
        WakeupRefused::DeadlinePassed { .. } => "its time budget has run out".to_string(),
    };
    format!(
        "The loop's budget is spent: {why}. Do not re-arm it — tell the user the loop \
         ended and why. A new loop needs a new budget: pass `budgetSeconds` or \
         `maxWakeups` only if the user asked to keep going."
    )
}

fn budget_json(budget: &WakeupBudget, now: i64) -> Value {
    json!({
        "deadline": budget.deadline,
        "remainingSeconds": (budget.deadline - now).max(0) / 1000,
        "fired": budget.fired,
        "maxWakeups": budget.max_wakeups,
    })
}

/// One line on what the loop has left, for the tool result.
fn budget_summary(budget: &WakeupBudget, now: i64) -> String {
    let remaining = format_duration((budget.deadline - now).max(0) / 1000);
    match budget.max_wakeups {
        Some(max) => format!(
            "Loop budget: {} of {max} wakeups fired, {remaining} left.",
            budget.fired
        ),
        None => format!("Loop budget: {remaining} left."),
    }
}

/// `seconds` as the largest two units: `3h 05m`, `12m 30s`, `45s`.
fn format_duration(seconds: i64) -> String {
    let (days, hours, minutes, secs) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
        seconds % 60,
    );
    if days > 0 {
        format!("{days}d {hours:02}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs:02}s")
    } else {
        format!("{secs}s")
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
            assert_eq!(
                clamp_seconds(asked, MIN_WAKEUP_DELAY_SECONDS, MAX_WAKEUP_DELAY_SECONDS),
                used,
                "{asked}"
            );
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
                new_budget: None,
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

    fn arm_with(delay: f64, budget: Value) -> Value {
        let mut input = arm(delay);
        input
            .as_object_mut()
            .unwrap()
            .extend(budget.as_object().unwrap().clone());
        input
    }

    #[test]
    fn naming_a_budget_starts_a_new_loop_and_naming_none_inherits() {
        let new_budget = |input: Value| match parse(&input).unwrap() {
            Wakeup::Arm { new_budget, .. } => new_budget,
            Wakeup::Stop => unreachable!("an arm"),
        };
        assert_eq!(new_budget(arm(60.0)), None, "a re-arm inherits");
        assert_eq!(
            new_budget(arm_with(60.0, json!({"budgetSeconds": 7200}))),
            Some(NewWakeupBudget {
                duration_ms: 7_200_000,
                max_wakeups: None
            })
        );
        assert_eq!(
            new_budget(arm_with(60.0, json!({"maxWakeups": 5}))),
            Some(NewWakeupBudget {
                duration_ms: rebon_tool::cron::tasks::DEFAULT_WAKEUP_BUDGET_MS,
                max_wakeups: Some(5)
            }),
            "a count alone keeps the default day"
        );
        assert_eq!(
            new_budget(arm_with(60.0, json!({"budgetSeconds": 1e12}))),
            Some(NewWakeupBudget {
                duration_ms: MAX_WAKEUP_BUDGET_SECONDS as i64 * 1000,
                max_wakeups: None
            }),
            "clamped to a week"
        );
        for bad in [json!({"maxWakeups": 0}), json!({"maxWakeups": -1})] {
            assert!(parse(&arm_with(60.0, bad.clone())).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_loop_stops_arming_once_its_wakeups_are_spent() {
        let store = SessionCronStore::new();
        let context = tool_context(&store);
        let first = ScheduleWakeupTool
            .call(arm_with(60.0, json!({"maxWakeups": 2})), &context)
            .await
            .unwrap();
        assert_eq!(first["budget"]["maxWakeups"], 2);
        assert!(first["content"]
            .as_str()
            .unwrap()
            .contains("0 of 2 wakeups fired"));

        // Two fires, each followed by the loop re-arming without a budget.
        store.take_due_wakeup(i64::MAX).expect("fires");
        ScheduleWakeupTool.call(arm(60.0), &context).await.unwrap();
        store.take_due_wakeup(i64::MAX).expect("fires");

        let refused = ScheduleWakeupTool.call(arm(60.0), &context).await;
        let Err(ToolError::InvalidInput {
            reason, error_code, ..
        }) = refused
        else {
            panic!("the third arming is refused: {refused:?}");
        };
        assert_eq!(error_code, Some(BUDGET_SPENT_CODE));
        assert!(reason.contains("fired 2 of its 2 wakeups"), "{reason}");
        assert!(reason.contains("Do not re-arm"), "{reason}");
        assert!(store.wakeup().is_none());

        // The user asked to keep going: a new budget is a new loop.
        ScheduleWakeupTool
            .call(arm_with(60.0, json!({"maxWakeups": 1})), &context)
            .await
            .expect("a new loop arms");
    }

    #[test]
    fn a_wakeup_due_past_the_deadline_is_moved_up_to_it_and_then_refused() {
        let store = SessionCronStore::new();
        let budget = parse(&arm_with(3600.0, json!({"budgetSeconds": 600}))).unwrap();
        let out = apply(&store, budget, 0).unwrap();
        assert_eq!(out["delaySeconds"], 600, "moved up to the deadline");
        assert!(out["content"].as_str().unwrap().contains("last wakeup"));

        store
            .take_due_wakeup(600_000)
            .expect("fires at the deadline");
        let refused = apply(&store, parse(&arm(60.0)).unwrap(), 600_000).unwrap_err();
        assert!(
            refused.to_string().contains("time budget has run out"),
            "{refused}"
        );
    }

    #[test]
    fn stop_ends_the_loop_and_its_budget() {
        let store = SessionCronStore::new();
        apply(
            &store,
            parse(&arm_with(60.0, json!({"maxWakeups": 1}))).unwrap(),
            0,
        )
        .unwrap();
        store.take_due_wakeup(i64::MAX);
        apply(&store, Wakeup::Stop, 0).unwrap();
        assert!(store.wakeup_budget().is_none());
        // With the spent budget gone, a later loop starts fresh.
        apply(&store, parse(&arm(60.0)).unwrap(), 0).expect("a fresh loop arms");
        assert_eq!(store.wakeup_budget().unwrap().max_wakeups, None);
    }

    #[test]
    fn durations_read_as_their_two_largest_units() {
        for (seconds, text) in [
            (0, "0s"),
            (45, "45s"),
            (750, "12m 30s"),
            (11_100, "3h 05m"),
            (86_400 + 7200, "1d 02h"),
        ] {
            assert_eq!(format_duration(seconds), text);
        }
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
