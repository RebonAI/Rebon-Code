//! The `Workflow` tool (alias `RunWorkflow`).
//!
//! The domain types it exchanges with the launcher — [`WorkflowLaunchSpec`],
//! [`WorkflowLaunchStatus`], [`WorkflowPermissionPreview`], the
//! [`WorkflowLauncher`] trait itself — live in `rebon_tool::workflow`: the
//! engine matches on the two name constants and holds a launcher of its own,
//! and neither may depend on a plugin. The implementation is next door in
//! [`crate::runtime`].

use async_trait::async_trait;
use rebon_tool::{
    Tool, ToolContext, WorkflowLaunchSpec, WorkflowNesting, WorkflowPermissionPreview,
    RUN_WORKFLOW_ALIAS, WORKFLOW_TOOL_NAME,
};
use rebon_tools_core::{
    parse_tool_input, PermissionDecision, PermissionRequest, ToolError, ToolId, ToolInputSchema,
    ToolResult, ValidationOutcome,
};
use rebon_types::truncate_chars;
use serde_json::{json, Value};

const SCRIPT_MAX_BYTES: usize = 524_288;
const INVALID_INPUT_CODE: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct WorkflowTool;

impl WorkflowTool {
    pub fn new() -> Self {
        Self
    }
}

const WORKFLOW_MODEL_DESCRIPTION: &str = r#"Execute a deterministic JavaScript workflow that orchestrates multiple local subagents.

A workflow structures work across many agents — to be comprehensive (decompose and cover in parallel), to be confident (independent perspectives and adversarial checks before committing), or to take on scale one context can't hold (migrations, audits, broad sweeps). The script is where you encode that structure: what fans out, what verifies, what synthesizes.

ONLY call this tool when the user explicitly opted into workflow / ultrawork / multi-agent orchestration. When you do call it, the right move is often hybrid: scout first (list the files, scope the diff, find the targets) to discover the work-list, then encode it in the workflow. You don't need to know the shape before the task — only before the orchestration step.

Common workflow shapes you can chain across turns:
- Understand — parallel readers over relevant subsystems → structured map
- Design — judge panel of N independent approaches → scored synthesis
- Review — dimensions → find → adversarially verify (example below)
- Research — multi-modal sweep → deep-read → synthesize
- Migrate — discover sites → transform each (worktree isolation) → verify

Pass new scripts inline via `script`; do not Write them to a file first. Scripts must begin with `export const meta = { name, description, phases }`; meta.description and meta.phases are shown first in the permission review, so make them user-readable with clear phase details covering the agent plan, scope, risks, and verification; meta must be a pure static object literal with no functions, variables, spreads, computed values, or template interpolation. Use the SAME phase titles in meta.phases as in phase() calls.

Scripts are plain JavaScript, not TypeScript. The body runs in an async context — use await directly. Standard JS built-ins are available EXCEPT `Date.now()`, `Math.random()`, and argless `new Date()`, which throw (they would break resume); pass timestamps via `args`, and for diversity vary each agent's prompt or label by index. No filesystem or Node.js APIs.

Script hooks:
- agent(prompt, { label, phase, schema, model, provider, modelProfile, isolation, agentType, cwd, maxIterations, taskKind }) — spawn a subagent; provide label and phase for every agent. Snake-case aliases are accepted for modelProfile, agentType, maxIterations, and taskKind. These are the supported control options; `effort` is not supported, so select a configured reasoning profile with modelProfile instead. Without schema, agent() returns the child agent's final text string; with schema (a JSON Schema), the child must call StructuredOutput and agent() returns the validated JSON. A returned agent result whose status is not `completed`, or whose structured output is missing/invalid, fails the workflow; never infer success from its final prose. Put prior agents' findings/results into the next agent's prompt text with `json(value)` or `JSON.stringify(value, null, 2)`. Do not put handoff data in `opts` / `options`; options are control metadata only and are not visible to the subagent.
- pipeline(items, ...stages) — the DEFAULT for multi-stage work: every item flows independently with NO barrier between stages, so item A can verify while item B is still being reviewed; stage callbacks receive `(prevResult, originalItem, index)`, and a throwing stage drops that item to `null`.
- parallel(thunks) — a true BARRIER: use only when the next step genuinely needs ALL results together (cross-item dedup/merge, early-exit when nothing was found, prompts that compare against "the other findings"). A throwing thunk resolves to `null`; a runtime-rejected agent result fails the workflow, so do not summarize it as completed.
- phase(title) / log(message) — progress grouping and narrator lines. If the script bounds coverage (top-N, sampling, no-retry), log() what was dropped — silent truncation reads as "covered everything" when it didn't.
- args — the tool input's `args` value, verbatim; use it to parameterize scripts.
- budget — { total, spent(), remaining() } from the OPTIONAL `budget` input: a hard OUTPUT-token ceiling (generated text summed across all agents; input context is not charged); once spent reaches total, agent() throws. It is a cap you opt into, NOT a prerequisite for working: omit the input and `budget.total` is null, `budget.remaining()` is Infinity, and the run is unbounded. So never make *whether* to spawn subagents depend on it — give the base fleet an unbounded default (`const FLEET = budget.total ? Math.floor(budget.total / 100_000) : 5`) and gate only *extra* depth on it (`while (budget.total && budget.remaining() > 50_000) { ...spawn more... }`). Writing the whole run as `budget.total ? … : 0` or `while (budget.total && …)` around the only spawn makes the orchestrator start nothing whenever no budget was set — the default must always launch work.
- workflow(name, args) — run a saved workflow inline as a sub-step.

Concurrent agent() calls are capped at min(16, cpu cores - 2) per workflow — excess calls queue and run as slots free up, so you can pass a large work-list and let it drain. Total agent calls per run are capped at 1000.

The canonical multi-stage pattern — pipeline by default, each dimension verifies as soon as its review completes:
  export const meta = {
    name: 'review-changes',
    description: 'Review changed files across dimensions, verify each finding',
    phases: [{ title: 'Review' }, { title: 'Verify' }],
  }
  const DIMENSIONS = [{ key: 'bugs', prompt: '...' }, { key: 'perf', prompt: '...' }]
  const results = await pipeline(
    DIMENSIONS,
    d => agent(d.prompt, { label: `review:${d.key}`, phase: 'Review', schema: FINDINGS_SCHEMA }),
    review => parallel(((review && review.findings) || []).map(f => () =>
      agent(`Adversarially verify this finding — try to REFUTE it, and default to refuted if uncertain: ${json(f)}`,
            { label: `verify:${f.file}`, phase: 'Verify', schema: VERDICT_SCHEMA })
        .then(v => ({ finding: f, verdict: v }))))
  )
  const confirmed = results.flat().filter(Boolean).filter(r => r.verdict && r.verdict.isReal)
  return { confirmed }

Loop-until-dry with a multi-lens panel — for unknown-size discovery, keep going until consecutive rounds add nothing new:
  const seen = new Set(), confirmed = []
  let dry = 0
  while (dry < 2) {
    const found = (await parallel(FINDERS.map(f => () =>
      agent(f.prompt, { label: f.key, phase: 'Find', schema: BUGS_SCHEMA })))).filter(Boolean).flatMap(r => r.bugs)
    const fresh = found.filter(b => !seen.has(b.key))
    if (!fresh.length) { dry++; continue }
    dry = 0
    fresh.forEach(b => seen.add(b.key))
    const judged = await parallel(fresh.map(b => () =>
      parallel(['correctness', 'security', 'reproducibility'].map(lens => () =>
        agent(`Judge this finding via the ${lens} lens — is it real? ${json(b)}`,
              { label: `judge:${lens}`, phase: 'Verify', schema: VERDICT_SCHEMA })))
        .then(vs => ({ bug: b, real: vs.filter(Boolean).filter(v => v.real).length >= 2 }))))
    confirmed.push(...judged.filter(j => j.real).map(j => j.bug))
  }
  return { confirmed }
  // dedup against `seen`, NOT `confirmed` — otherwise judge-rejected findings reappear every round and the loop never converges.

Quality patterns — common shapes; pick by task and compose freely:
- Adversarial verify: spawn N independent skeptics per finding, each prompted to REFUTE it; kill the finding if a majority refute. Prevents plausible-but-wrong findings from surviving.
- Perspective-diverse verify: when a finding can fail in more than one way, give each verifier a distinct lens (correctness, security, performance, does-it-reproduce) instead of N identical refuters — diversity catches failure modes redundancy can't.
- Judge panel: generate N independent attempts from different angles, score them with parallel judges, then synthesize from the winner while grafting the best ideas from runners-up. Beats one-attempt-iterated when the solution space is wide.
- Loop-until-dry: for unknown-size discovery (bugs, edge cases, data issues), keep spawning finders until K consecutive rounds return nothing new. Simple counters miss the tail.
- Multi-modal sweep: parallel agents each searching a different way (by file, by symbol, by history, by docs). One search angle won't find everything.
- Completeness critic: a final agent that asks "what's missing — an angle not run, a claim unverified, a source unread?" What it finds becomes the next round of work.
- No silent caps: any top-N, sampling, or skip decision must be surfaced with log().

Scale to what the user asked for: "find any bugs" → a few finders and a single-vote verify; "thoroughly audit this" or "be comprehensive" → a larger finder pool, a 3-5 vote adversarial pass, and a synthesis stage. When unsure, lean toward thoroughness for research/review/audit requests and toward brevity for quick checks. These patterns aren't exhaustive — compose novel harnesses when the task calls for it (tournament brackets, self-repair loops, staged escalation, whatever fits).

Use `status: "running"` only to inspect live workflow progress; do not combine status queries with scripts. Workflow waits for completion in this runtime and returns the final result/status directly; do not Sleep or poll after launching unless the user asks to inspect live progress."#;

#[async_trait]
impl Tool for WorkflowTool {
    fn id(&self) -> ToolId {
        ToolId::new(WORKFLOW_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &[RUN_WORKFLOW_ALIAS]
    }

    fn description(&self) -> &str {
        "Run deterministic JavaScript workflow orchestration with local sub-agents."
    }

    fn model_description(&self) -> &str {
        WORKFLOW_MODEL_DESCRIPTION
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "maxLength": SCRIPT_MAX_BYTES,
                    "description": "Self-contained workflow script. Must begin with `export const meta = { name, description, phases }`; meta.description and meta.phases are shown first in the permission review, so make them user-readable with clear phase details covering the agent plan, scope, risks, and verification; meta must be a pure static object literal with no functions, variables, spreads, computed values, or template interpolation. Use plain JavaScript, not TypeScript. The script body can use `agent(prompt, opts?)`, `agent({ prompt, ...opts })`, `parallel(thunks)`, `pipeline(items, ...stages)`, `phase(title)`, `phase(title, fn)`, `workflow()`, `log()`, `prompt(value)`, and `json(value)`. Prefer `pipeline`: each item flows independently and stage callbacks receive `(prevResult, originalItem, index)`. Use `parallel` only for a true barrier. `agent()` control options are schema, label, phase, model, provider, modelProfile/model_profile, isolation, agentType/agent_type, cwd, maxIterations/max_iterations, and taskKind/task_kind; `effort` is unsupported, so select a configured reasoning profile with modelProfile. Provide label and phase; include schema when the script needs structured JSON from the subagent. A returned agent result whose status is not completed, or whose structured output is missing/invalid, fails the workflow; never infer success from its final prose. Pass prior agent results to downstream agents by embedding them in the downstream prompt with `json(value)` or `JSON.stringify(value, null, 2)`, not by putting notes/results in `opts` or `options`; options are control metadata only and are not visible to the subagent. `agent()` accepts strings or objects; objects without `prompt` are serialized as stable JSON prompts, not `[object Object]`. For review/audit/research-style workflows, include a verification stage — adversarial verifiers prompted to REFUTE each finding, a multi-lens judge vote, or a completeness critic — instead of trusting single-pass findings. Workflow waits for completion and returns the final result directly; do not Sleep or poll after calling it."
                },
                "name": {
                    "type": "string",
                    "description": "Name of a predefined workflow."
                },
                "description": {
                    "type": "string",
                    "description": "Ignored; set the workflow description in the script's meta block."
                },
                "title": {
                    "type": "string",
                    "description": "Ignored; set the workflow title in the script's meta block."
                },
                "args": {
                    "description": "Optional input value exposed to the script as the global `args`."
                },
                "budget": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Optional hard output-token ceiling for the whole run, exposed to the script as `budget.total`. `budget.spent()` reports output tokens generated so far across all agents (input context is not charged) and `budget.remaining()` the difference; once spent reaches the ceiling, `agent()` throws. Omit for an unbounded run (`budget.total` is null, `budget.remaining()` Infinity)."
                },
                "status": {
                    "type": "string",
                    "description": "Status filter. Use `running` to inspect live workflow progress, or omit for the current invocation result."
                },
                "runInBackground": {
                    "type": "boolean",
                    "default": false,
                    "description": "Return async_launched immediately; completion arrives through task-notification. Coordinator mode forces true."
                },
                "scriptPath": {
                    "type": "string",
                    "description": "Path to a workflow script file on disk. Takes precedence over `name` and `script`."
                },
                "resumeFromRunId": {
                    "type": "string",
                    "description": "Workflow run ID to resume. Completed agent() calls with unchanged prompt/options are reused."
                }
            },
            "additionalProperties": false
        })
    }

    fn search_hint(&self) -> Option<&str> {
        Some("orchestrate subagents deterministic javascript workflow")
    }

    fn needs_permission(&self, input: &Value) -> bool {
        parse_input(input)
            .ok()
            .is_none_or(|spec| !is_running_status_query(&spec))
    }

    async fn validate_input(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        let spec = parse_input(input)?;
        if let Some(reason) = validate_workflow_launch_spec(&spec) {
            return Ok(ValidationOutcome::invalid(reason, INVALID_INPUT_CODE));
        }
        if is_running_status_query(&spec) {
            return Ok(ValidationOutcome::valid());
        }
        if !WorkflowNesting::of(context).may_launch_workflow() {
            return Ok(ValidationOutcome::invalid(
                "Nested workflow depth exceeded",
                INVALID_INPUT_CODE,
            ));
        }
        if spec.script.is_none()
            && spec.name.is_none()
            && spec.script_path.is_none()
            && spec.resume_from_run_id.is_none()
        {
            return Ok(ValidationOutcome::invalid(
                "Must provide script, name, scriptPath, resumeFromRunId, or status: \"running\"",
                INVALID_INPUT_CODE,
            ));
        }
        if let Some(script) = spec.script.as_ref() {
            if script.len() > SCRIPT_MAX_BYTES {
                return Ok(ValidationOutcome::invalid(
                    format!("Script exceeds {SCRIPT_MAX_BYTES} bytes"),
                    INVALID_INPUT_CODE,
                ));
            }
        }
        Ok(ValidationOutcome::valid())
    }

    async fn check_permissions(
        &self,
        input: &Value,
        context: &ToolContext,
    ) -> ToolResult<PermissionDecision> {
        let mut spec = parse_input(input)?;
        if is_running_status_query(&spec) {
            return Ok(PermissionDecision::allow(input.clone()));
        }
        ensure_workflow_depth(context)?;
        if context.coordinator_mode() {
            spec.run_in_background = true;
        }
        let preview = match context.workflow_launcher() {
            Some(launcher) => launcher
                .preview_workflow(spec.clone(), context)
                .await
                .map_err(|reason| ToolError::InvalidInput {
                    tool: self.id(),
                    reason,
                    error_code: Some(INVALID_INPUT_CODE),
                })?,
            None => fallback_preview(&spec),
        };
        let title = format!("Review workflow {}", preview.name);
        let message = if preview.review_text.trim().is_empty() {
            fallback_review_text(&preview)
        } else {
            preview.review_text.trim().to_string()
        };
        let updated_input = serde_json::to_value(&spec).map_err(|err| ToolError::InvalidInput {
            tool: self.id(),
            reason: err.to_string(),
            error_code: Some(INVALID_INPUT_CODE),
        })?;
        Ok(PermissionDecision::ask(
            PermissionRequest::new(title, message)
                .with_options(["allow_once", "reject_once"])
                .with_metadata(preview.review_metadata()),
            Some(updated_input),
        ))
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let mut spec = parse_input(&input)?;
        if !is_running_status_query(&spec) {
            ensure_workflow_depth(context)?;
            if context.coordinator_mode() {
                spec.run_in_background = true;
            }
        }
        let launcher = context
            .workflow_launcher()
            .ok_or_else(|| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!("workflow launcher is not configured"),
            })?
            .clone();
        let status = launcher
            .launch_workflow(spec, context)
            .await
            .map_err(|source| ToolError::Execution {
                tool: self.id(),
                source: anyhow::anyhow!(source),
            })?;
        serde_json::to_value(status).map_err(|err| ToolError::Execution {
            tool: self.id(),
            source: err.into(),
        })
    }
}

fn parse_input(input: &Value) -> ToolResult<WorkflowLaunchSpec> {
    parse_tool_input(ToolId::new(WORKFLOW_TOOL_NAME), input)
}

fn ensure_workflow_depth(context: &ToolContext) -> ToolResult<()> {
    if !WorkflowNesting::of(context).may_launch_workflow() {
        return Err(ToolError::InvalidInput {
            tool: ToolId::new(WORKFLOW_TOOL_NAME),
            reason: "Nested workflow depth exceeded".into(),
            error_code: Some(INVALID_INPUT_CODE),
        });
    }
    Ok(())
}

fn is_running_status_query(spec: &WorkflowLaunchSpec) -> bool {
    spec.status
        .as_deref()
        .map(str::trim)
        .is_some_and(|status| status.eq_ignore_ascii_case("running"))
}

fn non_empty_str(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn validate_workflow_launch_spec(spec: &WorkflowLaunchSpec) -> Option<String> {
    // `resumeFromRunId` composes with `script` / `scriptPath` / `name`:
    // agent results are cached by content hash, so a corrected script may
    // ride along on a resume. The coordinator-side validator agrees — a
    // stricter copy here silently made the documented recovery path
    // ("fix the cause and resubmit with resumeFromRunId") unreachable.

    let Some(status) = non_empty_str(spec.status.as_deref()) else {
        return None;
    };
    if !status.eq_ignore_ascii_case("running") {
        return Some(
            "Unsupported workflow status query; only status: \"running\" is supported".into(),
        );
    }
    if spec.run_in_background {
        return Some("status: \"running\" cannot be combined with runInBackground: true".into());
    }
    if non_empty_str(spec.script.as_deref()).is_some()
        || non_empty_str(spec.script_path.as_deref()).is_some()
    {
        return Some(
            "status: \"running\" is a query and cannot be combined with script or scriptPath"
                .into(),
        );
    }
    None
}

fn fallback_preview(spec: &WorkflowLaunchSpec) -> WorkflowPermissionPreview {
    let name = spec
        .name
        .as_deref()
        .or(spec.title.as_deref())
        .or(spec.resume_from_run_id.as_deref())
        .unwrap_or("inline")
        .trim()
        .to_string();
    let script_preview = spec
        .script
        .as_deref()
        .map(|script| truncate_chars(script, 800))
        .or_else(|| spec.script_path.clone())
        .unwrap_or_default();
    let preview = WorkflowPermissionPreview {
        name: if name.is_empty() {
            "inline".into()
        } else {
            name
        },
        title: spec.title.clone(),
        description: spec.description.clone().unwrap_or_default(),
        source: spec.script_path.clone(),
        script_preview,
        review_text: String::new(),
        args_summary: spec.args.as_ref().map(summarize_fallback_arg),
        args: spec.args.clone(),
        phases: Vec::new(),
        calls: Vec::new(),
        warnings: vec![
            "review builder unavailable; showing metadata and script details only".into(),
        ],
        errors: Vec::new(),
    };
    WorkflowPermissionPreview {
        review_text: fallback_review_text(&preview),
        ..preview
    }
}

fn summarize_fallback_arg(value: &Value) -> String {
    const MAX: usize = 240;
    match serde_json::to_string(value) {
        Ok(text) => truncate_chars(&text, MAX),
        Err(_) => "(unserializable args)".into(),
    }
}

fn fallback_review_text(preview: &WorkflowPermissionPreview) -> String {
    let mut message = String::new();
    message.push_str("Overview\n");
    message.push_str("- Name: ");
    message.push_str(preview.name.trim());
    message.push('\n');
    if let Some(title) = preview
        .title
        .as_ref()
        .filter(|title| !title.trim().is_empty())
    {
        message.push_str("- Title: ");
        message.push_str(title.trim());
        message.push('\n');
    }
    if !preview.description.trim().is_empty() {
        message.push_str("- Description: ");
        message.push_str(preview.description.trim());
        message.push('\n');
    }
    if let Some(source) = preview
        .source
        .as_ref()
        .filter(|source| !source.trim().is_empty())
    {
        message.push_str("- Source: ");
        message.push_str(source.trim());
        message.push('\n');
    }
    push_fallback_review_line(
        &mut message,
        "Args",
        preview.args_summary.as_deref().unwrap_or("(none)"),
    );
    message.push_str("- Static calls: unavailable in fallback preview\n");

    message.push_str("\nPhases\n");
    if preview.phases.is_empty() {
        message
            .push_str("- unavailable in fallback preview; rely on workflow metadata if present\n");
    } else {
        for (idx, phase) in preview.phases.iter().enumerate() {
            message.push_str(&format!("{}. {}\n", idx + 1, phase.title.trim()));
            if let Some(detail) = phase
                .detail
                .as_deref()
                .filter(|detail| !detail.trim().is_empty())
            {
                message.push_str("   ");
                message.push_str(detail.trim());
                message.push('\n');
            }
        }
    }

    message.push_str("\nAgent calls\n");
    message.push_str("- unavailable in fallback preview\n");

    message.push_str("\nWarnings/errors\n");
    if preview.warnings.is_empty() && preview.errors.is_empty() {
        message.push_str("- warning: review builder unavailable; showing basic workflow preview\n");
    } else {
        for warning in &preview.warnings {
            message.push_str("- warning: ");
            message.push_str(warning.trim());
            message.push('\n');
        }
        for error in &preview.errors {
            message.push_str("- error: ");
            message.push_str(error.trim());
            message.push('\n');
        }
    }
    message.push_str(
        "\nScript details\n- JavaScript source is an implementation detail for audit. Review the plan above first; inspect this excerpt only if needed.\n",
    );
    if !preview.script_preview.trim().is_empty() {
        message.push_str("\nScript excerpt\n");
        message.push_str(preview.script_preview.trim());
        message.push('\n');
    } else {
        message.push_str("- No script excerpt available.\n");
    }
    message
}

fn push_fallback_review_line(out: &mut String, label: &str, value: &str) {
    out.push_str("- ");
    out.push_str(label);
    out.push_str(": ");
    out.push_str(value.trim());
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use rebon_tool::{
        WorkflowLaunchStatus, WorkflowLauncher, WorkflowPermissionReviewCall,
        WorkflowPermissionReviewPhase,
    };
    use rebon_types::MAX_NESTED_WORKFLOW_DEPTH;

    #[tokio::test]
    async fn workflow_tool_schema_is_provider_compatible() {
        let schema = WorkflowTool::new().input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema.get("oneOf").is_none());
        assert!(schema.get("anyOf").is_none());
        assert!(schema.get("allOf").is_none());
        assert!(schema.get("enum").is_none());
        assert!(schema.get("not").is_none());
        assert!(schema["properties"].get("status").is_some());
        assert_eq!(
            schema["properties"]["script"]["maxLength"],
            SCRIPT_MAX_BYTES
        );
    }

    #[tokio::test]
    async fn workflow_tool_prompt_requires_prompt_text_handoffs() {
        let tool = WorkflowTool::new();
        let model_description = tool.model_description();
        assert!(model_description.contains("prior agents' findings/results"));
        assert!(model_description.contains("prompt text"));
        assert!(model_description.contains("Do not put handoff data in `opts` / `options`"));
        assert!(model_description.contains("`effort` is not supported"));
        assert!(model_description.contains("modelProfile"));
        assert!(model_description.contains("never infer success from its final prose"));
        assert!(model_description.contains("StructuredOutput"));

        let schema = tool.input_schema();
        let script_description = schema["properties"]["script"]["description"]
            .as_str()
            .expect("script description");
        assert!(script_description.contains("prior agent results"));
        assert!(script_description.contains("downstream prompt"));
        assert!(script_description.contains("not by putting notes/results in `opts` or `options`"));
        assert!(script_description.contains("`effort` is unsupported"));
        assert!(script_description.contains("modelProfile/model_profile"));
        assert!(script_description.contains("never infer success from its final prose"));
    }

    #[tokio::test]
    async fn workflow_tool_requires_one_source() {
        let tool = WorkflowTool::new();
        let validation = tool
            .validate_input(&json!({}), &ToolContext::new())
            .await
            .expect("validation");
        assert!(!validation.result);
    }

    #[tokio::test]
    async fn workflow_tool_accepts_running_status_query_without_permission() {
        let tool = WorkflowTool::new();
        let input = json!({"status":" running "});
        let validation = tool
            .validate_input(&input, &ToolContext::new())
            .await
            .expect("validation");
        assert!(validation.result);
        assert!(!tool.needs_permission(&input));
    }

    #[tokio::test]
    async fn workflow_tool_allows_one_nested_level_and_rejects_deeper_launches() {
        struct Launcher;

        #[async_trait]
        impl WorkflowLauncher for Launcher {
            async fn launch_workflow(
                &self,
                _spec: WorkflowLaunchSpec,
                _context: &ToolContext,
            ) -> Result<WorkflowLaunchStatus, String> {
                Ok(WorkflowLaunchStatus {
                    status: "completed".into(),
                    task_id: None,
                    run_id: None,
                    summary: None,
                    transcript_dir: None,
                    script_path: None,
                    result: None,
                    workflow_progress: None,
                    agent_count: Some(0),
                    warning: None,
                    error: None,
                })
            }

            async fn preview_workflow(
                &self,
                _spec: WorkflowLaunchSpec,
                _context: &ToolContext,
            ) -> Result<WorkflowPermissionPreview, String> {
                unreachable!()
            }
        }

        let tool = WorkflowTool::new();
        let input =
            json!({"script":"export const meta = { name: 'nested', description: 'Nested' };"});
        let allowed = ToolContext::new()
            .with_workflow_nesting_depth(MAX_NESTED_WORKFLOW_DEPTH)
            .with_workflow_launcher(Arc::new(Launcher));
        let validation = tool
            .validate_input(&input, &allowed)
            .await
            .expect("validation");
        assert!(validation.result);
        assert_eq!(
            tool.call(input.clone(), &allowed)
                .await
                .expect("nested launch")["status"],
            "completed"
        );

        let rejected = ToolContext::new()
            .with_workflow_nesting_depth(MAX_NESTED_WORKFLOW_DEPTH + 1)
            .with_workflow_launcher(Arc::new(Launcher));
        let validation = tool
            .validate_input(&input, &rejected)
            .await
            .expect("validation");
        assert!(!validation.result);
        let error = tool
            .call(input, &rejected)
            .await
            .expect_err("deeper nested launch must fail");
        assert!(error.to_string().contains("Nested workflow depth exceeded"));
    }

    /// End-to-end recovery path: a resume may carry a corrected inline
    /// script. The workflow failure message advertises exactly this
    /// ("a corrected script may ride along"), so the tool layer must let it
    /// through validation, the permission preview, and the launch itself.
    #[tokio::test]
    async fn workflow_tool_accepts_resume_with_corrected_script() {
        #[derive(Default)]
        struct Launcher {
            launched: std::sync::Mutex<Option<WorkflowLaunchSpec>>,
        }

        #[async_trait]
        impl WorkflowLauncher for Launcher {
            async fn launch_workflow(
                &self,
                spec: WorkflowLaunchSpec,
                _context: &ToolContext,
            ) -> Result<WorkflowLaunchStatus, String> {
                *self.launched.lock().unwrap() = Some(spec);
                Ok(WorkflowLaunchStatus {
                    status: "completed".into(),
                    task_id: None,
                    run_id: Some("wf_resume".into()),
                    summary: None,
                    transcript_dir: None,
                    script_path: None,
                    result: None,
                    workflow_progress: None,
                    agent_count: Some(1),
                    warning: None,
                    error: None,
                })
            }

            async fn preview_workflow(
                &self,
                spec: WorkflowLaunchSpec,
                _context: &ToolContext,
            ) -> Result<WorkflowPermissionPreview, String> {
                Ok(fallback_preview(&spec))
            }
        }

        let tool = WorkflowTool::new();
        let launcher = Arc::new(Launcher::default());
        let context = ToolContext::new().with_workflow_launcher(launcher.clone());
        let input = json!({
            "resumeFromRunId": "wf_resume",
            "script": "export const meta = { name: 'demo', description: 'Demo' };",
        });

        let validation = tool
            .validate_input(&input, &context)
            .await
            .expect("validation");
        assert!(
            validation.result,
            "resume with a corrected script must validate: {:?}",
            validation.message
        );

        let decision = tool
            .check_permissions(&input, &context)
            .await
            .expect("permission check");
        assert_eq!(decision.behavior, rebon_tools_core::PermissionBehavior::Ask);
        let updated = decision.updated_input.expect("updated input");
        assert_eq!(updated["resumeFromRunId"], "wf_resume");
        assert!(updated["script"].as_str().is_some());

        let result = tool.call(input, &context).await.expect("resume launch");
        assert_eq!(result["status"], "completed");
        let launched = launcher.launched.lock().unwrap().clone().expect("spec");
        assert_eq!(launched.resume_from_run_id.as_deref(), Some("wf_resume"));
        assert!(launched.script.is_some());
    }

    /// A saved workflow name may ride along on a resume the same way.
    #[tokio::test]
    async fn workflow_tool_accepts_resume_with_named_workflow() {
        let tool = WorkflowTool::new();
        let validation = tool
            .validate_input(
                &json!({"resumeFromRunId": "wf_resume", "name": "review-changes"}),
                &ToolContext::new(),
            )
            .await
            .expect("validation");
        assert!(validation.result, "{:?}", validation.message);
    }

    #[tokio::test]
    async fn workflow_tool_rejects_status_query_with_script() {
        let tool = WorkflowTool::new();
        let validation = tool
            .validate_input(
                &json!({
                    "status": "running",
                    "script": "export const meta = { name: 'demo', description: 'Demo' };",
                }),
                &ToolContext::new(),
            )
            .await
            .expect("validation");
        assert!(!validation.result);
        assert!(validation
            .message
            .as_deref()
            .expect("validation message")
            .contains("query"));
    }

    #[test]
    fn fallback_workflow_review_puts_metadata_before_script_details() {
        let preview = fallback_preview(&WorkflowLaunchSpec {
            script: Some("export const meta = { name: 'demo', description: 'Demo', phases: [] };\nawait agent('x');".into()),
            title: Some("Demo title".into()),
            description: Some("Fallback description".into()),
            args: Some(json!({"scope":"repo"})),
            ..WorkflowLaunchSpec::default()
        });

        let text = preview.review_text;
        let overview = text.find("Overview").expect("overview section");
        let phases = text.find("Phases").expect("phases section");
        let agent_calls = text.find("Agent calls").expect("agent calls section");
        let warnings = text.find("Warnings/errors").expect("warnings section");
        let script_details = text.find("Script details").expect("script details section");
        let script_excerpt = text.find("Script excerpt").expect("script excerpt section");
        assert!(overview < phases);
        assert!(phases < agent_calls);
        assert!(agent_calls < warnings);
        assert!(warnings < script_details);
        assert!(script_details < script_excerpt);
        assert!(text.contains("JavaScript source is an implementation detail for audit"));
        assert!(text.contains("review builder unavailable"));
    }

    #[tokio::test]
    async fn workflow_tool_permission_uses_launcher_preview_when_available() {
        struct Launcher;

        #[async_trait]
        impl WorkflowLauncher for Launcher {
            async fn launch_workflow(
                &self,
                _spec: WorkflowLaunchSpec,
                _context: &ToolContext,
            ) -> Result<WorkflowLaunchStatus, String> {
                unreachable!()
            }

            async fn preview_workflow(
                &self,
                _spec: WorkflowLaunchSpec,
                _context: &ToolContext,
            ) -> Result<WorkflowPermissionPreview, String> {
                Ok(WorkflowPermissionPreview {
                    name: "demo".into(),
                    title: Some("Demo".into()),
                    description: "Run demo".into(),
                    source: Some("built-in".into()),
                    script_preview: "export const meta =".into(),
                    review_text: "Overview\n- Name: demo\n- Description: Run demo\n\nExecution graph\n`-- agent calls: 1".into(),
                    args_summary: None,
                    args: None,
                    phases: vec![WorkflowPermissionReviewPhase {
                        title: "Run".into(),
                        detail: Some("Run demo agents".into()),
                        model: None,
                    }],
                    calls: vec![WorkflowPermissionReviewCall {
                        kind: "agent".into(),
                        line: 3,
                        summary: "prompt \"do work\"".into(),
                        phase: None,
                        has_schema: false,
                        agent: None,
                    }],
                    warnings: Vec::new(),
                    errors: Vec::new(),
                })
            }
        }

        let tool = WorkflowTool::new();
        let context = ToolContext::new().with_workflow_launcher(Arc::new(Launcher));
        let decision = tool
            .check_permissions(&json!({"name":"demo"}), &context)
            .await
            .expect("permission decision");
        let request = decision.request.expect("ask request");
        assert!(request.title.contains("demo"));
        assert!(request.title.contains("Review workflow"));
        assert!(request.message.contains("Overview"));
        assert!(request.message.contains("Execution graph"));
        assert_eq!(
            request.metadata.as_ref().expect("workflow review metadata")["agentCallCount"],
            1
        );
        assert_eq!(request.options, vec!["allow_once", "reject_once"]);
    }
}
