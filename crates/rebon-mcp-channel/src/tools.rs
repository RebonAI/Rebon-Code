//! The tool surface: what the client sees in `tools/list`, and how a
//! `tools/call` becomes a [`Desk`] operation.
//!
//! Frozen at the RFC-0007 §6.2 set plus `job_permit` (§9.4: a job parked on
//! a permission prompt must be answerable from the client, G4). There is no
//! `job_list` on purpose — the client learns about jobs from their pushes and
//! from the ids `exec_start` returned; listing every job is the CLI's job.
//! `channel_probe` exists only under `--probe`, for one-off fact finding.
//!
//! A call never fails at the protocol level: a refused or failed operation
//! comes back as an `isError` result the model can read, because some clients
//! treat a JSON-RPC error from a tool as fatal to the connection.

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::jobs::{Desk, JobRequest, PermitRequest, ReplyRequest, ResultRequest, StartRequest};
use crate::push::Update;
use crate::server::Outbox;

pub(crate) const EXEC_START: &str = "exec_start";
pub(crate) const JOB_STATUS: &str = "job_status";
pub(crate) const JOB_RESULT: &str = "job_result";
pub(crate) const JOB_CANCEL: &str = "job_cancel";
pub(crate) const JOB_REPLY: &str = "job_reply";
pub(crate) const JOB_PERMIT: &str = "job_permit";
pub(crate) const CHANNEL_PROBE: &str = "channel_probe";

pub(crate) fn list(channel: bool, probe: bool) -> Value {
    let arrival = if channel {
        "When it finishes, or stops to wait for an answer, a <channel source=\"rebon\"> \
         message with its job_id arrives on its own; do not poll while waiting for one. If \
         none ever arrives, channels are not enabled for this session — check job_status \
         instead."
    } else {
        "This server was started with --no-channel: nothing is pushed, so check job_status \
         to see when the job is done."
    };
    let job_id = json!({
        "type": "string",
        "description": "The id exec_start returned (bg-…)."
    });
    let mut tools = vec![
        json!({
            "name": EXEC_START,
            "description": format!(
                "Start a task as a Rebon background job and return at once with its job_id. \
                 Rebon runs a full agent loop of its own — its own model, tools and \
                 unattended budget — in a process that outlives this session, so the job \
                 is never cut off by a timeout here. It cannot see this conversation: the \
                 prompt must say everything the task needs (goal, files, constraints, what \
                 done looks like). The job runs in an isolated git worktree when the \
                 directory is a repository, and its work is merged back when it succeeds. \
                 {arrival}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "The complete, self-contained task." },
                    "agent": { "type": "string", "description": "A Rebon agent to run it as (a custom agent name, or an external agent CLI Rebon has configured)." },
                    "provider": { "type": "string", "description": "A Rebon provider name; defaults to Rebon's configured one." },
                    "model": { "type": "string", "description": "A model id for that provider; defaults to the provider's own." },
                    "cwd": { "type": "string", "description": "Directory to run in: this project's root (the default) or a directory inside it." },
                    "name": { "type": "string", "description": "A short display name for the job." },
                    "permission_mode": { "type": "string", "description": "Rebon permission mode for the job, e.g. default, acceptEdits, plan. auto and bypassPermissions only work once the user has accepted them for background jobs in Rebon. Prompts the mode does not settle park the job until answered with job_permit." }
                },
                "required": ["prompt"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true }
        }),
        json!({
            "name": JOB_STATUS,
            "description": "A job's current state (queued, running, needs_input, idle, succeeded, failed, stopped), its timing, and — when it is parked — the question or permission prompt it is waiting on, with the query_id and options job_reply or job_permit need. The authority when a channel message is missing or late.",
            "inputSchema": {
                "type": "object",
                "properties": { "job_id": job_id },
                "required": ["job_id"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        }),
        json!({
            "name": JOB_RESULT,
            "description": "What a finished job's last turn said: the end of it as `summary`, and the whole of it in the file at `result_path` (read that file for anything cut off). Only for a job that is no longer running.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": job_id,
                    "max_chars": { "type": "integer", "minimum": 200, "maximum": 20000, "description": "How much of the end to return inline; default 4000." }
                },
                "required": ["job_id"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": JOB_CANCEL,
            "description": "Stop a job, and every job it started. Its transcript is kept.",
            "inputSchema": {
                "type": "object",
                "properties": { "job_id": job_id },
                "required": ["job_id"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": JOB_REPLY,
            "description": "Talk to a job. If it is waiting on a question (job_status shows pending.kind = question), this answers it: pass its query_id, and either `text` (one question) or `answers` (one entry per question, with 1-based option numbers in `selected` and/or free `text`). Otherwise `text` is a follow-up the job runs as its next turn, in the same conversation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": job_id,
                    "text": { "type": "string", "description": "A follow-up message, or the answer to a single pending question." },
                    "query_id": { "type": "integer", "description": "The pending question's query_id, from job_status or the channel message." },
                    "answers": {
                        "type": "array",
                        "description": "One answer per pending question, in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "selected": { "type": "array", "items": { "type": "integer", "minimum": 1 } },
                                "text": { "type": "string" }
                            },
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["job_id"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": true }
        }),
        json!({
            "name": JOB_PERMIT,
            "description": "Answer a job's pending permission prompt (job_status shows pending.kind = permission) with one of the option_ids it lists. One-time answers only: options that would save a lasting rule are not offered here.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job_id": job_id,
                    "query_id": { "type": "integer", "description": "The pending prompt's query_id." },
                    "option_id": { "type": "string", "description": "One of pending.options[].option_id." }
                },
                "required": ["job_id", "query_id", "option_id"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true }
        }),
    ];
    if probe {
        tools.push(json!({
            "name": CHANNEL_PROBE,
            "description": "Diagnostics: push one channel message right now and return its nonce. If a <channel> message carrying that nonce does not arrive shortly, channel pushes do not reach this session.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        }));
    }
    json!({ "tools": tools })
}

/// Run one `tools/call`. `params` is the request's params object.
pub(crate) async fn call(desk: &Arc<Desk>, outbox: &Outbox, probe: bool, params: Value) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(arguments) => arguments.clone(),
    };
    let outcome = match name.as_str() {
        EXEC_START => {
            run(desk, arguments, |desk, request: StartRequest| {
                desk.start(request, rebon_types::wall_clock_ms())
            })
            .await
        }
        JOB_STATUS => {
            run(desk, arguments, |desk, request: JobRequest| {
                desk.status(&request.job_id)
            })
            .await
        }
        JOB_RESULT => {
            run(desk, arguments, |desk, request: ResultRequest| {
                desk.result(request)
            })
            .await
        }
        JOB_CANCEL => {
            run(desk, arguments, |desk, request: JobRequest| {
                desk.cancel(&request.job_id, rebon_types::wall_clock_ms())
            })
            .await
        }
        JOB_REPLY => {
            run(desk, arguments, |desk, request: ReplyRequest| {
                desk.reply(request)
            })
            .await
        }
        JOB_PERMIT => {
            run(desk, arguments, |desk, request: PermitRequest| {
                desk.permit(request)
            })
            .await
        }
        CHANNEL_PROBE if probe => probe_channel(desk, outbox),
        other => Err(anyhow::anyhow!(
            "unknown tool `{other}`; this server offers {EXEC_START}, {JOB_STATUS}, \
             {JOB_RESULT}, {JOB_CANCEL}, {JOB_REPLY} and {JOB_PERMIT}"
        )),
    };
    match outcome {
        Ok(value) => tool_result(&value, false),
        Err(error) => tool_result(&json!({ "error": format!("{error:#}") }), true),
    }
}

/// Parse the arguments, then run `operation` on the blocking pool: every
/// desk operation takes file locks, and some wait on a worker.
async fn run<T, F>(desk: &Arc<Desk>, arguments: Value, operation: F) -> anyhow::Result<Value>
where
    T: DeserializeOwned + Send + 'static,
    F: FnOnce(&Desk, T) -> anyhow::Result<Value> + Send + 'static,
{
    let request: T = serde_json::from_value(arguments)
        .map_err(|error| anyhow::anyhow!("invalid arguments: {error}"))?;
    let desk = Arc::clone(desk);
    tokio::task::spawn_blocking(move || operation(&desk, request))
        .await
        .map_err(|error| anyhow::anyhow!("the operation did not finish: {error}"))?
}

fn probe_channel(desk: &Desk, outbox: &Outbox) -> anyhow::Result<Value> {
    if !desk.channel_declared() {
        anyhow::bail!("this server runs with --no-channel; there is no channel to probe");
    }
    let nonce = rebon_types::secure_random_hex_token()
        .map_err(|error| anyhow::anyhow!("could not make a nonce: {error}"))?;
    let nonce = nonce[..8].to_string();
    let message = Update::Probe {
        nonce: nonce.clone(),
    }
    .message();
    outbox
        .send(message.to_notification())
        .map_err(|_| anyhow::anyhow!("the connection is closing"))?;
    Ok(json!({ "sent": true, "nonce": nonce }))
}

/// A tool result: the value as compact JSON text, which every client shows
/// the model, flagged when it is an error.
fn tool_result(value: &Value, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": value.to_string() }],
        "isError": is_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &Value) -> Vec<String> {
        list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_surface_is_the_frozen_set_and_the_probe_only_on_request() {
        let list = list(true, false);
        assert_eq!(
            names(&list),
            vec![EXEC_START, JOB_STATUS, JOB_RESULT, JOB_CANCEL, JOB_REPLY, JOB_PERMIT]
        );
        assert!(names(&super::list(true, true)).contains(&CHANNEL_PROBE.to_string()));
        assert!(
            !names(&list).iter().any(|name| name.contains("list")),
            "no job_list: the CLI lists jobs, pushes keep the client's list"
        );
    }

    #[test]
    fn every_schema_is_closed_and_ids_are_the_only_handle_on_a_result() {
        for tool in list(true, true)["tools"].as_array().unwrap() {
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object", "{}", tool["name"]);
            assert_eq!(schema["additionalProperties"], false, "{}", tool["name"]);
        }
        let result = list(true, false)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == JOB_RESULT)
            .unwrap()
            .clone();
        let properties = result["inputSchema"]["properties"].as_object().unwrap();
        assert!(
            properties
                .keys()
                .all(|key| key == "job_id" || key == "max_chars"),
            "job_result must not take a path: {properties:?}"
        );
    }

    #[test]
    fn exec_start_says_how_the_outcome_arrives_in_each_mode() {
        let describe = |channel| {
            list(channel, false)["tools"][0]["description"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert!(describe(true).contains("<channel source=\"rebon\">"));
        assert!(describe(true).contains("job_status"));
        assert!(describe(false).contains("--no-channel"));
        assert!(!describe(false).contains("<channel"));
    }

    #[test]
    fn a_tool_result_is_text_json_and_flags_errors() {
        let ok = tool_result(&json!({ "job_id": "bg-1" }), false);
        assert_eq!(ok["isError"], false);
        let text = ok["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap()["job_id"],
            "bg-1"
        );
        assert_eq!(tool_result(&json!({}), true)["isError"], true);
    }
}
