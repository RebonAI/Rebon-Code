//! The workflow runtime: a deterministic JavaScript orchestration script,
//! executed against a fleet of sub-agents.
//!
//! [`WorkflowRegistryLauncher`] is the one implementation of
//! [`rebon_tool::WorkflowLauncher`], reached through the `workflow-launcher`
//! seat this plugin fills. What it does, in order: parse the script's `meta`
//! block, refuse anything non-deterministic (`Date.now`, `Math.random`),
//! build the pre-run review a permission prompt shows, register the run as a
//! task, then evaluate the script in Boa with `agent()` bridged onto the
//! sub-agent spawner and every result cached by a hash of the call so a
//! resumed run replays instead of re-spending.
//!
//! Two edges point out of it — the spawner it hands `SubAgentSpec`s to, and
//! the model-profile validation the static review borrows — because both are
//! the agent layer's, not the workflow's.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use boa_engine::{
    builtins::promise::PromiseState,
    js_string,
    object::builtins::{JsFunction, JsPromise},
    property::Attribute,
    Context as BoaContext, Finalize, JsArgs, JsData, JsError, JsNativeError, JsResult, JsValue,
    NativeFunction, Source, Trace,
};
use rebon_tool::{
    append_workflow_agent_shared_worktree_prompt, SubAgentSpec, SubAgentTaskKind, ToolContext,
    WorkflowLaunchSpec, WorkflowLaunchStatus, WorkflowLauncher, WorkflowNesting,
    WorkflowPermissionPreview, WorkflowPermissionReviewCall, WorkflowPermissionReviewPhase,
    STRUCTURED_OUTPUT_TOOL_NAME, WORKFLOW_STRUCTURED_OUTPUT_SCHEMA_KEY,
};
use rebon_tools_core::ToolProgressUpdate;
use rebon_types::{truncate_chars, PromptCancel};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use rebon_plugin_tasks::runtime::{
    complete_local_workflow_task, fail_local_workflow_task, generate_task_id,
    push_local_workflow_progress, register_local_workflow_task, release_local_workflow_run,
    LocalWorkflowTaskSpec, TaskData, TaskId, TaskKind, TaskRegistry, TaskSnapshot, TaskStatus,
    WorkflowProgressEntry,
};
// The progress projection lives with the data it projects
// (`rebon-plugin-tasks::workflow_progress`): the background task bridge in
// `rebon-cli` needs the same preview and may not depend on this runtime.
use rebon_plugin_tasks::workflow_progress::{
    bounded_workflow_text, workflow_progress_for_task, workflow_progress_metadata,
    workflow_progress_object_value, workflow_progress_payload, WorkflowProgressMetadata,
};

const SCRIPT_MAX_BYTES: usize = 524_288;
const WORKFLOW_SUBAGENT_SYSTEM_PROMPT: &str = "You are a subagent spawned by a workflow orchestration script. Use the tools available to complete the task. Your final assistant text is consumed as the raw workflow return value, not as a human-facing message.";
const WORKFLOW_STRUCTURED_SUBAGENT_SYSTEM_PROMPT: &str = r#"You are a subagent spawned by a workflow orchestration script.
Use Read, Grep, Glob, Bash, Edit, and other available tools as needed to complete the requested work normally.

FINAL DELIVERY CONTRACT:
- After the work is complete, your final step MUST be calling the StructuredOutput tool.
- The workflow runtime consumes the StructuredOutput tool call as your structured result.
- Your normal final text is not the intended structured result path.
- The StructuredOutput input MUST be one JSON object matching the schema below.
- If schema validation fails, fix the JSON shape and call StructuredOutput again.
- If you do not call StructuredOutput and your final text cannot be recovered as schema-valid JSON, this agent will be marked failed.
- After a successful StructuredOutput call, end your turn. Do not add prose."#;
const MAX_WORKFLOW_AGENT_CALLS: u64 = 1000;
const MAX_WORKFLOW_AGENT_CONCURRENCY: usize = 16;
const WORKFLOW_STATUS_MAX_RUNNING: usize = 8;
const WORKFLOW_AGENT_OPTION_KEYS: &[&str] = &[
    "schema",
    "label",
    "phase",
    "model",
    "provider",
    "modelProfile",
    "model_profile",
    "isolation",
    "agentType",
    "agent_type",
    "cwd",
    "maxIterations",
    "max_iterations",
    "taskKind",
    "task_kind",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkflowPhase {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WorkflowPhaseInput {
    Title(String),
    Object {
        title: String,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        model: Option<String>,
    },
}

impl<'de> Deserialize<'de> for WorkflowPhase {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match WorkflowPhaseInput::deserialize(deserializer)? {
            WorkflowPhaseInput::Title(title) => Ok(Self {
                title,
                detail: None,
                model: None,
            }),
            WorkflowPhaseInput::Object {
                title,
                detail,
                model,
            } => Ok(Self {
                title,
                detail,
                model,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowMeta {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "whenToUse", default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<WorkflowPhase>,
    #[serde(
        rename = "defaultModel",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub default_model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ParsedWorkflowScript {
    pub meta: WorkflowMeta,
    pub script_body: String,
}

#[derive(Debug, Clone)]
pub struct WorkflowResolvedScript {
    pub script: String,
    pub meta: WorkflowMeta,
    pub source: String,
    pub resolved_script_path: Option<PathBuf>,
}

const WORKFLOW_REVIEW_SCRIPT_EXCERPT_CHARS: usize = 1_600;
const WORKFLOW_REVIEW_PROMPT_CHARS: usize = 120;
const WORKFLOW_REVIEW_MAX_CALLS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowReview {
    pub name: String,
    pub title: Option<String>,
    pub source: String,
    pub description: String,
    pub args_summary: Option<String>,
    /// Structural launch args, so review UIs can substitute `${args.…}`
    /// template placeholders with the actual values.
    pub args: Option<Value>,
    pub phases: Vec<WorkflowReviewPhase>,
    pub calls: Vec<WorkflowReviewCall>,
    pub script_excerpt: String,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowReviewPhase {
    pub title: String,
    pub detail: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowReviewCall {
    pub kind: String,
    pub line: usize,
    pub summary: String,
    pub phase: Option<String>,
    pub has_schema: bool,
    /// Structured `agent()` option facts (label/model/provider/…) recovered by
    /// the static scan, for editors that want more than the display summary.
    pub agent: Option<rebon_tools_core::WorkflowAgentNodeMeta>,
}

#[derive(Debug, Clone)]
pub struct WorkflowReviewBuilder<'a> {
    script: &'a str,
    source: String,
    args: Option<Value>,
    fallback_name: Option<String>,
    fallback_description: Option<String>,
}

impl WorkflowReview {
    pub fn builder(script: &str) -> WorkflowReviewBuilder<'_> {
        WorkflowReviewBuilder::new(script)
    }

    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str("Overview\n");
        push_review_line(&mut out, "Name", &self.name);
        if let Some(title) = self
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
        {
            push_review_line(&mut out, "Title", title.trim());
        }
        push_review_line(&mut out, "Source", &self.source);
        if self.description.trim().is_empty() {
            push_review_line(&mut out, "Description", "(missing)");
        } else {
            push_review_line(&mut out, "Description", self.description.trim());
        }
        push_review_line(
            &mut out,
            "Args",
            self.args_summary.as_deref().unwrap_or("(none)"),
        );
        push_review_line(
            &mut out,
            "Static calls",
            &format_static_call_counts(&self.calls),
        );

        out.push_str("\nPhases\n");
        if self.phases.is_empty() {
            out.push_str("- (no phases declared in meta)\n");
        } else {
            for (idx, phase) in self.phases.iter().enumerate() {
                out.push_str(&format!("{}. {}", idx + 1, phase.title));
                if let Some(model) = phase
                    .model
                    .as_deref()
                    .filter(|model| !model.trim().is_empty())
                {
                    out.push_str(&format!(" [model: {}]", model.trim()));
                }
                out.push('\n');
                if let Some(detail) = phase
                    .detail
                    .as_deref()
                    .filter(|detail| !detail.trim().is_empty())
                {
                    out.push_str(&format!("   {}\n", detail.trim()));
                }
            }
        }

        out.push_str("\nExecution graph\n");
        for line in self.execution_graph_lines() {
            out.push_str(&line);
            out.push('\n');
        }

        out.push_str("\nAgent calls\n");
        let agent_calls: Vec<_> = self
            .calls
            .iter()
            .filter(|call| call.kind == "agent")
            .collect();
        if agent_calls.is_empty() {
            out.push_str("- (no agent calls found statically)\n");
        } else {
            for (idx, call) in agent_calls
                .iter()
                .take(WORKFLOW_REVIEW_MAX_CALLS)
                .enumerate()
            {
                out.push_str(&format!(
                    "{}. line {}: {}\n",
                    idx + 1,
                    call.line,
                    call.summary
                ));
            }
            if agent_calls.len() > WORKFLOW_REVIEW_MAX_CALLS {
                out.push_str(&format!(
                    "- ... {} more agent call(s) omitted\n",
                    agent_calls.len() - WORKFLOW_REVIEW_MAX_CALLS
                ));
            }
        }

        out.push_str("\nWarnings/errors\n");
        if self.warnings.is_empty() && self.errors.is_empty() {
            out.push_str("- none\n");
        } else {
            for warning in &self.warnings {
                out.push_str(&format!("- warning: {warning}\n"));
            }
            for error in &self.errors {
                out.push_str(&format!("- error: {error}\n"));
            }
        }

        out.push_str("\nScript details\n");
        out.push_str("- JavaScript source is an implementation detail for audit. Review the plan, phases, and agent calls above first; inspect this excerpt only if needed.\n");
        out.push_str("\nScript excerpt\n");
        out.push_str(&self.script_excerpt);
        if !self.script_excerpt.ends_with('\n') {
            out.push('\n');
        }

        out.push_str("\nMermaid source (copyable)\n");
        out.push_str("```mermaid\n");
        out.push_str(&self.mermaid_source());
        out.push_str("```\n");
        out
    }

    pub fn mermaid_source(&self) -> String {
        let mut out = String::new();
        out.push_str("flowchart TD\n");
        out.push_str(&format!(
            "  workflow[\"Workflow: {}\"]\n",
            escape_mermaid_label(&self.name)
        ));
        if self.phases.is_empty() {
            out.push_str("  workflow --> phases[\"Phases: none declared\"]\n");
        } else {
            for (idx, phase) in self.phases.iter().enumerate() {
                out.push_str(&format!(
                    "  workflow --> phase{}[\"Phase: {}\"]\n",
                    idx + 1,
                    escape_mermaid_label(&phase.title)
                ));
            }
        }
        let agent_count = self
            .calls
            .iter()
            .filter(|call| call.kind == "agent")
            .count();
        out.push_str(&format!(
            "  workflow --> agents[\"Agent calls: {agent_count}\"]\n"
        ));
        for (idx, call) in self
            .calls
            .iter()
            .filter(|call| call.kind != "agent" && call.kind != "phase")
            .take(8)
            .enumerate()
        {
            out.push_str(&format!(
                "  workflow --> call{}[\"{}: {}\"]\n",
                idx + 1,
                escape_mermaid_label(&call.kind),
                escape_mermaid_label(&call.summary)
            ));
        }
        out
    }

    fn execution_graph_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("Workflow: {}", self.name)];
        if self.phases.is_empty() {
            for call in self.calls.iter().filter(|call| call.kind == "phase") {
                lines.push(format!("|-- phase (line {}): {}", call.line, call.summary));
            }
        } else {
            for phase in &self.phases {
                lines.push(format!("|-- phase: {}", phase.title));
            }
        }
        for call in self
            .calls
            .iter()
            .filter(|call| call.kind != "agent" && call.kind != "phase")
        {
            lines.push(format!(
                "|-- {} (line {}): {}",
                call.kind, call.line, call.summary
            ));
        }
        let agent_count = self
            .calls
            .iter()
            .filter(|call| call.kind == "agent")
            .count();
        lines.push(format!("`-- agent calls: {agent_count}"));
        lines
    }
}

impl<'a> WorkflowReviewBuilder<'a> {
    pub fn new(script: &'a str) -> Self {
        Self {
            script,
            source: "inline".into(),
            args: None,
            fallback_name: None,
            fallback_description: None,
        }
    }

    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    pub fn args(mut self, args: Option<Value>) -> Self {
        self.args = args;
        self
    }

    pub fn fallback_name(mut self, name: impl Into<String>) -> Self {
        self.fallback_name = Some(name.into());
        self
    }

    pub fn fallback_description(mut self, description: impl Into<String>) -> Self {
        self.fallback_description = Some(description.into());
        self
    }

    pub fn build(self) -> WorkflowReview {
        let parsed = parse_workflow_meta(self.script);
        let mut errors = Vec::new();
        let (meta, script_body) = match parsed {
            Ok(parsed) => (parsed.meta, parsed.script_body),
            Err(error) => {
                errors.push(error);
                (
                    WorkflowMeta {
                        name: self
                            .fallback_name
                            .clone()
                            .filter(|name| !name.trim().is_empty())
                            .unwrap_or_else(|| "inline".into()),
                        description: self.fallback_description.clone().unwrap_or_default(),
                        title: None,
                        when_to_use: None,
                        phases: Vec::new(),
                        default_model: None,
                    },
                    self.script.to_string(),
                )
            }
        };
        let phases = meta
            .phases
            .iter()
            .map(|phase| WorkflowReviewPhase {
                title: phase.title.clone(),
                detail: phase.detail.clone(),
                model: phase.model.clone(),
            })
            .collect::<Vec<_>>();
        let calls = summarize_workflow_static_calls(&script_body);
        let mut warnings = Vec::new();
        if phases.is_empty() {
            warnings.push("meta.phases is empty; the review cannot show planned stages".into());
        }
        let agent_call_count = calls.iter().filter(|call| call.kind == "agent").count();
        if agent_call_count == 0 {
            warnings.push("no agent() calls were found statically".into());
        }
        if workflow_contains_nondeterminism(self.script) {
            warnings.push(
                "script references disabled nondeterministic APIs such as Date or Math.random"
                    .into(),
            );
        }
        if agent_call_count >= 2 && workflow_lacks_verification_stage(&meta, self.script) {
            warnings.push(
                "review/research-style workflow has no verification stage; consider adversarial verifiers prompted to REFUTE findings, a multi-lens judge vote, or a completeness critic"
                    .into(),
            );
        }
        WorkflowReview {
            name: meta.name,
            title: meta.title,
            source: self.source,
            description: meta.description,
            args_summary: self.args.as_ref().map(summarize_review_arg),
            args: self.args,
            phases,
            calls,
            script_excerpt: script_excerpt(self.script),
            warnings,
            errors,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkflowRegistryLauncher {
    registry: Option<TaskRegistry>,
    registry_resolver: Option<rebon_plugin_tasks::TaskRegistryResolver>,
    cwd: PathBuf,
    config_home_dir: PathBuf,
    session_id: Option<String>,
    session_root: PathBuf,
    model_profiles: rebon_types::ModelProfileMap,
    /// Name of the active provider whose profiles `model_profiles` holds.
    /// The static review can only validate `modelProfile` values against
    /// this provider; calls routed elsewhere resolve at run time.
    active_provider: Option<String>,
}

impl WorkflowRegistryLauncher {
    pub fn new(
        registry: TaskRegistry,
        cwd: impl Into<PathBuf>,
        config_home_dir: impl Into<PathBuf>,
        session_root: impl Into<PathBuf>,
        session_id: impl Into<String>,
    ) -> Self {
        Self::new_scoped(
            registry,
            cwd,
            config_home_dir,
            session_root,
            Some(session_id.into()),
        )
    }

    pub fn new_session_scoped(
        registry: TaskRegistry,
        cwd: impl Into<PathBuf>,
        config_home_dir: impl Into<PathBuf>,
        session_root: impl Into<PathBuf>,
    ) -> Self {
        Self::new_scoped(registry, cwd, config_home_dir, session_root, None)
    }

    fn new_scoped(
        registry: TaskRegistry,
        cwd: impl Into<PathBuf>,
        config_home_dir: impl Into<PathBuf>,
        session_root: impl Into<PathBuf>,
        session_id: Option<String>,
    ) -> Self {
        Self {
            registry: Some(registry),
            registry_resolver: None,
            cwd: cwd.into(),
            config_home_dir: config_home_dir.into(),
            session_id,
            session_root: session_root.into(),
            model_profiles: rebon_types::ModelProfileMap::default(),
            active_provider: None,
        }
    }

    pub fn new_resolving(
        resolver: rebon_plugin_tasks::TaskRegistryResolver,
        cwd: impl Into<PathBuf>,
        config_home_dir: impl Into<PathBuf>,
        session_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            registry: None,
            registry_resolver: Some(resolver),
            cwd: cwd.into(),
            config_home_dir: config_home_dir.into(),
            session_id: None,
            session_root: session_root.into(),
            model_profiles: rebon_types::ModelProfileMap::default(),
            active_provider: None,
        }
    }

    fn for_context(&self, context: &ToolContext) -> Result<Self, String> {
        let Some(resolver) = self.registry_resolver.as_ref() else {
            return Ok(self.clone());
        };
        let session_id = context
            .session_id()
            .filter(|session_id| !session_id.trim().is_empty())
            .ok_or_else(|| "Workflow launcher requires a session id".to_string())?;
        let mut scoped = self.clone();
        scoped.registry = Some(resolver.resolve(session_id)?.as_ref().clone());
        scoped.registry_resolver = None;
        scoped.session_id = Some(session_id.to_string());
        Ok(scoped)
    }

    fn registry(&self) -> &TaskRegistry {
        self.registry
            .as_ref()
            .expect("workflow registry must be resolved before use")
    }

    /// Configured model profiles, so the pre-run review can flag a
    /// `modelProfile` value the runtime would reject (regenerated scripts
    /// keep inventing plausible-sounding profile names, which now silently
    /// null the agent at run time).
    pub fn with_model_profiles(mut self, profiles: rebon_types::ModelProfileMap) -> Self {
        self.model_profiles = profiles;
        self
    }

    /// Provider the configured profile map belongs to, so the static review
    /// knows which `provider:` declarations it can actually check.
    pub fn with_active_provider(mut self, provider: impl Into<String>) -> Self {
        let provider = provider.into();
        self.active_provider = (!provider.trim().is_empty()).then_some(provider);
        self
    }

    /// Flag statically-visible `modelProfile` values the router would reject
    /// at run time — the regenerating model keeps inventing plausible profile
    /// names ("balanced"), which now silently null the agent mid-run.
    fn warn_on_unknown_model_profiles(&self, review: &mut WorkflowReview) {
        for call in &review.calls {
            let Some(meta) = call.agent.as_ref() else {
                continue;
            };
            let Some(profile) = meta
                .model_profile
                .as_deref()
                .map(str::trim)
                .filter(|profile| !profile.is_empty())
            else {
                continue;
            };
            // The launcher only holds the ACTIVE provider's profile map. A
            // call routed to another provider resolves its profile against
            // that provider's own map at run time — checking it here raised
            // false alarms, so the static warning fires only for calls that
            // stay on the active provider (or name none).
            let provider = meta
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|provider| !provider.is_empty());
            if let Some(provider) = provider {
                let stays_on_active = self
                    .active_provider
                    .as_deref()
                    .is_some_and(|active| active.eq_ignore_ascii_case(provider));
                if !stays_on_active {
                    continue;
                }
            }
            if let Err(error) = rebon_agent_core::model_router::validate_profile(
                Some(profile),
                &self.model_profiles,
                provider.unwrap_or("default"),
            ) {
                review.warnings.push(bounded_workflow_text(
                    &format!("line {}: {error}", call.line),
                    512,
                ));
            }
        }
    }

    fn effective_session_id(&self, context: Option<&ToolContext>) -> Result<String, String> {
        context
            .and_then(ToolContext::session_id)
            .map(ToOwned::to_owned)
            .or_else(|| self.session_id.clone())
            .ok_or_else(|| "Workflow launcher requires a session id".to_string())
    }

    pub fn resolve_script(
        &self,
        spec: &WorkflowLaunchSpec,
    ) -> Result<WorkflowResolvedScript, String> {
        self.resolve_script_for_context(spec, None)
    }

    pub fn resolve_script_for_context(
        &self,
        spec: &WorkflowLaunchSpec,
        context: Option<&ToolContext>,
    ) -> Result<WorkflowResolvedScript, String> {
        validate_workflow_launch_spec(spec)?;
        if is_running_status_query(spec) {
            return Err(
                "status: \"running\" is a query and does not resolve a workflow script".into(),
            );
        }
        if let Some(script_path) = spec
            .script_path
            .as_ref()
            .filter(|path| !path.trim().is_empty())
        {
            return self.load_script_path(script_path, context);
        }
        if let Some(name) = spec.name.as_ref().filter(|name| !name.trim().is_empty()) {
            return self.find_named_workflow(name);
        }
        if let Some(script) = spec
            .script
            .as_ref()
            .filter(|script| !script.trim().is_empty())
        {
            let parsed = parse_workflow_meta(script)?;
            return Ok(WorkflowResolvedScript {
                script: script.clone(),
                meta: parsed.meta,
                source: "inline".into(),
                resolved_script_path: None,
            });
        }
        if let Some(run_id) = spec
            .resume_from_run_id
            .as_ref()
            .filter(|run_id| !run_id.trim().is_empty())
        {
            return self.load_resume_script(run_id, context);
        }
        Err("Must provide script, name, scriptPath, or resumeFromRunId".into())
    }

    fn load_script_path(
        &self,
        script_path: &str,
        context: Option<&ToolContext>,
    ) -> Result<WorkflowResolvedScript, String> {
        let path = self.resolve_relative_path(script_path);
        if context.and_then(ToolContext::ultraplan_context).is_some()
            && !rebon_tool::path_scope::path_is_within_roots(
                &path,
                &[
                    self.config_home_dir.join("workflows"),
                    self.cwd.join(".rebon").join("workflows"),
                ],
            )
        {
            return Err(
                "Workflow scriptPath is restricted during /ultraplan to configured workflow directories"
                    .into(),
            );
        }
        let script = fs::read_to_string(&path)
            .map_err(|err| format!("Failed to read workflow at {}: {err}", path.display()))?;
        let parsed = parse_workflow_meta(&script)?;
        Ok(WorkflowResolvedScript {
            script,
            meta: parsed.meta,
            source: "scriptPath".into(),
            resolved_script_path: Some(path),
        })
    }

    fn load_resume_script(
        &self,
        run_id: &str,
        context: Option<&ToolContext>,
    ) -> Result<WorkflowResolvedScript, String> {
        if self.registry().snapshots().iter().any(|snapshot| {
            snapshot.status == rebon_plugin_tasks::runtime::TaskStatus::Running
                && snapshot.metadata_str("workflow_run_id") == Some(run_id)
        }) {
            return Err(format!(
                "Cannot resume running workflow run {run_id}; stop it first"
            ));
        }
        let snapshot_path = self
            .workflow_dir_for(&self.effective_session_id(context)?)
            .join(format!("{run_id}.json"));
        let snapshot = fs::read_to_string(&snapshot_path).map_err(|err| {
            format!(
                "Failed to read workflow snapshot {}: {err}",
                snapshot_path.display()
            )
        })?;
        let snapshot: Value = serde_json::from_str(&snapshot).map_err(|err| {
            format!(
                "Invalid workflow snapshot {}: {err}",
                snapshot_path.display()
            )
        })?;
        let script = snapshot
            .get("script")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Workflow snapshot {run_id} has no script"))?
            .to_string();
        let parsed = parse_workflow_meta(&script)?;
        Ok(WorkflowResolvedScript {
            script,
            meta: parsed.meta,
            source: "resume".into(),
            resolved_script_path: snapshot
                .get("scriptPath")
                .and_then(Value::as_str)
                .map(PathBuf::from),
        })
    }

    fn find_named_workflow(&self, name: &str) -> Result<WorkflowResolvedScript, String> {
        let workflows = self.discover_workflows();
        if let Some(found) = workflows
            .into_iter()
            .find(|workflow| workflow.meta.name == name)
        {
            return Ok(found);
        }
        Err(format!("Workflow \"{name}\" not found"))
    }

    fn discover_workflows(&self) -> Vec<WorkflowResolvedScript> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for workflow in builtin_workflows()
            .into_iter()
            .chain(self.discover_dir(&self.config_home_dir.join("workflows"), "user"))
            .chain(self.discover_dir(&self.cwd.join(".rebon").join("workflows"), "project"))
        {
            if seen.insert(workflow.meta.name.clone()) {
                out.push(workflow);
            }
        }
        out
    }

    fn discover_dir(&self, dir: &Path, source: &str) -> Vec<WorkflowResolvedScript> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("js"))
            .filter_map(|entry| {
                let path = entry.path();
                let script = fs::read_to_string(&path).ok()?;
                let parsed = parse_workflow_meta(&script).ok()?;
                Some(WorkflowResolvedScript {
                    script,
                    meta: parsed.meta,
                    source: source.into(),
                    resolved_script_path: Some(path),
                })
            })
            .collect()
    }

    fn resolve_relative_path(&self, path: &str) -> PathBuf {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            self.cwd.join(path)
        }
    }

    fn running_workflow_status(&self, spec: &WorkflowLaunchSpec) -> WorkflowLaunchStatus {
        let run_filter = non_empty_str(spec.resume_from_run_id.as_deref());
        let name_filter = non_empty_str(spec.name.as_deref());
        let mut workflows: Vec<Value> = self
            .registry()
            .snapshots()
            .iter()
            .filter(|snapshot| snapshot.status == TaskStatus::Running)
            .filter_map(|snapshot| workflow_status_item(snapshot, run_filter, name_filter))
            .collect();
        workflows.sort_by(|left, right| {
            value_u64(right, "startTimeMs").cmp(&value_u64(left, "startTimeMs"))
        });

        let total_count = workflows.len();
        workflows.truncate(WORKFLOW_STATUS_MAX_RUNNING);
        let selected = if workflows.len() == 1 {
            workflows.first().cloned()
        } else {
            run_filter.and_then(|run_id| {
                workflows
                    .iter()
                    .find(|workflow| workflow.get("runId").and_then(Value::as_str) == Some(run_id))
                    .cloned()
            })
        };
        let result = json!({
            "count": total_count,
            "truncated": total_count > workflows.len(),
            "workflows": workflows,
        });
        let status =
            if selected.is_some() || result.get("count").and_then(Value::as_u64).unwrap_or(0) > 0 {
                "running"
            } else if run_filter.is_some() {
                "not_found"
            } else {
                "idle"
            };
        let warning = if status == "not_found" {
            run_filter.map(|run_id| {
                bounded_workflow_text(&format!("No running workflow found for run {run_id}"), 384)
            })
        } else {
            None
        };
        let workflow_progress = selected
            .as_ref()
            .and_then(|workflow| workflow.get("workflowProgress").cloned())
            .or_else(|| {
                Some(json!({
                    "count": result.get("count").and_then(Value::as_u64).unwrap_or(0),
                    "workflows": result.get("workflows").cloned().unwrap_or_else(|| json!([])),
                    "entries": [],
                }))
            });

        bounded_workflow_launch_status(WorkflowLaunchStatus {
            status: status.into(),
            task_id: selected
                .as_ref()
                .and_then(|workflow| value_string(workflow, "taskId")),
            run_id: selected
                .as_ref()
                .and_then(|workflow| value_string(workflow, "runId"))
                .or_else(|| run_filter.map(|run_id| bounded_workflow_text(run_id, 96))),
            summary: selected
                .as_ref()
                .and_then(|workflow| value_string(workflow, "summary")),
            transcript_dir: selected
                .as_ref()
                .and_then(|workflow| value_string(workflow, "transcriptDir")),
            script_path: selected
                .as_ref()
                .and_then(|workflow| value_string(workflow, "scriptPath")),
            result: Some(result),
            workflow_progress,
            agent_count: selected
                .as_ref()
                .and_then(|workflow| workflow.get("agentCount").and_then(Value::as_u64)),
            warning,
            error: None,
        })
    }

    fn workflow_dir_for(&self, session_id: &str) -> PathBuf {
        self.session_root.join(session_id).join("workflows")
    }

    fn transcript_dir_for(&self, session_id: &str, run_id: &str) -> PathBuf {
        self.session_root
            .join(session_id)
            .join("subagents")
            .join("workflows")
            .join(run_id)
    }
}

fn workflow_review_source(resolved: &WorkflowResolvedScript) -> String {
    match resolved.resolved_script_path.as_ref() {
        Some(path) => format!("{} ({})", resolved.source, path.display()),
        None => resolved.source.clone(),
    }
}

fn workflow_permission_preview_from_review(review: WorkflowReview) -> WorkflowPermissionPreview {
    let review_text = review.render_text();
    WorkflowPermissionPreview {
        name: review.name,
        title: review.title,
        description: review.description,
        source: Some(review.source),
        script_preview: review.script_excerpt,
        review_text,
        args_summary: review.args_summary,
        args: review.args,
        phases: review
            .phases
            .into_iter()
            .map(|phase| WorkflowPermissionReviewPhase {
                title: phase.title,
                detail: phase.detail,
                model: phase.model,
            })
            .collect(),
        calls: review
            .calls
            .into_iter()
            .map(|call| WorkflowPermissionReviewCall {
                kind: call.kind,
                line: call.line,
                summary: call.summary,
                phase: call.phase,
                has_schema: call.has_schema,
                agent: call.agent,
            })
            .collect(),
        warnings: review.warnings,
        errors: review.errors,
    }
}

#[async_trait]
impl WorkflowLauncher for WorkflowRegistryLauncher {
    async fn preview_workflow(
        &self,
        spec: WorkflowLaunchSpec,
        context: &ToolContext,
    ) -> Result<WorkflowPermissionPreview, String> {
        if self.registry_resolver.is_some() {
            let scoped = self.for_context(context)?;
            return Box::pin(scoped.preview_workflow(spec, context)).await;
        }
        validate_workflow_launch_spec(&spec)?;
        if is_running_status_query(&spec) {
            return Ok(WorkflowPermissionPreview {
                name: "running workflows".into(),
                title: Some("Running workflows".into()),
                description: "Inspect live workflow progress".into(),
                source: Some("task registry".into()),
                script_preview: String::new(),
                review_text: "Overview\n- Query: running workflow progress\n".into(),
                args_summary: None,
                args: None,
                phases: Vec::new(),
                calls: Vec::new(),
                warnings: Vec::new(),
                errors: Vec::new(),
            });
        }
        if !WorkflowNesting::of(context).may_launch_workflow() {
            return Err("Nested workflow depth exceeded".into());
        }
        match self.resolve_script_for_context(&spec, Some(context)) {
            Ok(resolved) => {
                let source = workflow_review_source(&resolved);
                let mut review = WorkflowReview::builder(&resolved.script)
                    .source(source.clone())
                    .args(spec.args.clone())
                    .build();
                self.warn_on_unknown_model_profiles(&mut review);
                Ok(workflow_permission_preview_from_review(review))
            }
            Err(reason) => {
                let script = spec
                    .script
                    .as_deref()
                    .filter(|script| !script.trim().is_empty())
                    .ok_or(reason)?;
                let mut review = WorkflowReview::builder(script)
                    .source("inline")
                    .args(spec.args.clone())
                    .fallback_name(
                        spec.name
                            .as_deref()
                            .or(spec.title.as_deref())
                            .unwrap_or("inline"),
                    )
                    .fallback_description(spec.description.as_deref().unwrap_or_default())
                    .build();
                self.warn_on_unknown_model_profiles(&mut review);
                Ok(workflow_permission_preview_from_review(review))
            }
        }
    }

    async fn launch_workflow(
        &self,
        spec: WorkflowLaunchSpec,
        context: &ToolContext,
    ) -> Result<WorkflowLaunchStatus, String> {
        if self.registry_resolver.is_some() {
            let scoped = self.for_context(context)?;
            return Box::pin(scoped.launch_workflow(spec, context)).await;
        }
        validate_workflow_launch_spec(&spec)?;
        if is_running_status_query(&spec) {
            return Ok(self.running_workflow_status(&spec));
        }
        if !WorkflowNesting::of(context).may_launch_workflow() {
            return Err("Nested workflow depth exceeded".into());
        }
        let resolved = self.resolve_script_for_context(&spec, Some(context))?;
        if workflow_contains_nondeterminism(&resolved.script) {
            return Err("Workflow scripts may not use Date.now, Math.random, or new Date".into());
        }
        // A resume of a still-running run would put two writers on one
        // transcript dir. `load_resume_script` guards the bare-resume path;
        // this covers resume-with-a-new-script, which never loads the
        // snapshot.
        if let Some(resume_id) = non_empty_str(spec.resume_from_run_id.as_deref()) {
            if self.registry().snapshots().iter().any(|snapshot| {
                snapshot.status == rebon_plugin_tasks::runtime::TaskStatus::Running
                    && snapshot.metadata_str("workflow_run_id") == Some(resume_id)
            }) {
                return Err(format!(
                    "Cannot resume running workflow run {resume_id}; stop it first"
                ));
            }
        }
        let run_id = spec
            .resume_from_run_id
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(new_workflow_run_id);
        let session_id = self.effective_session_id(Some(context))?;
        let task_id = generate_task_id(TaskKind::LocalWorkflow);
        let transcript_dir = self.transcript_dir_for(&session_id, &run_id);
        let workflows_dir = self.workflow_dir_for(&session_id);
        let persisted_script = workflows_dir.join(format!(
            "{}_{}.js",
            sanitize_filename(&resolved.meta.name),
            run_id
        ));
        let cancel = PromptCancel::new();
        register_local_workflow_task(
            self.registry(),
            LocalWorkflowTaskSpec {
                id: task_id.clone(),
                run_id: run_id.clone(),
                workflow_name: resolved.meta.name.clone(),
                summary: resolved
                    .meta
                    .title
                    .clone()
                    .or_else(|| Some(resolved.meta.description.clone())),
                agent_count: 0,
                output_path: Some(transcript_dir.display().to_string()),
                script_path: Some(persisted_script.display().to_string()),
                args: spec.args.clone(),
                is_backgrounded: spec.run_in_background,
                parent_session_id: Some(session_id),
                parent_tool_call_id: context.tool_use_id().map(str::to_string),
            },
            cancel.clone(),
        )
        .map_err(|error| error.to_string())?;
        let prepare = persist_workflow_script(
            &workflows_dir,
            &resolved.meta.name,
            &run_id,
            &resolved.script,
        )
        .and_then(|_| {
            fs::create_dir_all(&transcript_dir).map_err(|err| {
                format!(
                    "Failed to create workflow transcript dir {}: {err}",
                    transcript_dir.display()
                )
            })
        });
        if let Err(error) = prepare {
            fail_local_workflow_task(self.registry(), &task_id, error.clone(), false);
            release_local_workflow_run(self.registry(), &run_id, &task_id);
            return Err(error);
        }
        let summary = resolved.meta.description.clone();

        let runner = WorkflowRun {
            registry: self.registry().clone(),
            task_id: task_id.clone(),
            run_id: run_id.clone(),
            transcript_dir: transcript_dir.clone(),
            persisted_script: persisted_script.clone(),
            cwd: self.cwd.clone(),
            script: resolved.script,
            meta: resolved.meta,
            args: spec.args,
            context: context.clone(),
            cancel,
            depth: context.workflow_nesting_depth(),
            permission_prompts_unavailable: spec.run_in_background
                || context.permission_prompts_unavailable(),
            nested_workflows: self.discover_workflows(),
            budget_total: spec.budget,
            agent_concurrency_cap: None,
        };
        if spec.run_in_background {
            let receipt = bounded_workflow_launch_status(WorkflowLaunchStatus {
                status: "async_launched".into(),
                task_id: Some(task_id.to_string()),
                run_id: Some(run_id),
                summary: Some(summary),
                transcript_dir: Some(transcript_dir.display().to_string()),
                script_path: Some(persisted_script.display().to_string()),
                result: None,
                workflow_progress: workflow_progress_for_task(self.registry(), &task_id),
                agent_count: Some(0),
                warning: None,
                error: None,
            });
            tokio::spawn(async move {
                execute_and_finalize_workflow(runner, true).await;
            });
            return Ok(receipt);
        }
        let outcome = execute_and_finalize_workflow(runner, false).await;
        let status = bounded_workflow_launch_status(WorkflowLaunchStatus {
            status: outcome.status,
            task_id: Some(task_id.to_string()),
            run_id: Some(run_id),
            summary: Some(summary),
            transcript_dir: Some(transcript_dir.display().to_string()),
            script_path: Some(persisted_script.display().to_string()),
            result: outcome.result,
            workflow_progress: outcome.workflow_progress,
            agent_count: Some(outcome.agent_count),
            warning: None,
            error: outcome.error,
        });
        emit_workflow_task_completed(context, &status);
        Ok(status)
    }
}
struct WorkflowTerminalOutcome {
    status: String,
    result: Option<Value>,
    workflow_progress: Option<Value>,
    agent_count: u64,
    error: Option<String>,
}
async fn execute_and_finalize_workflow(
    runner: WorkflowRun,
    background: bool,
) -> WorkflowTerminalOutcome {
    let execution = runner.execute().await;
    let killed = runner.cancel.is_cancelled()
        || runner
            .registry
            .snapshot(&runner.task_id)
            .is_some_and(|s| s.status == TaskStatus::Killed);
    let (status, result, agent_count, error) = if killed {
        let _ = runner.persist_snapshot("killed", None, execution.err());
        ("killed".into(), None, 0, None)
    } else {
        match execution {
            Ok(result) => {
                let n = result
                    .get("agentCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                complete_local_workflow_task(
                    &runner.registry,
                    &runner.task_id,
                    result.clone(),
                    n,
                    background,
                );
                let _ = runner.persist_snapshot("completed", Some(result.clone()), None);
                ("completed".into(), Some(result), n, None)
            }
            Err(error) => {
                let agent_count = workflow_agent_count(&runner.registry, &runner.task_id);
                let visible_error =
                    model_visible_workflow_failure(&error, agent_count, &runner.run_id);
                tracing::warn!(
                    run_id = %runner.run_id,
                    agent_count,
                    diagnostic_error = %error,
                    "workflow execution failed"
                );
                fail_local_workflow_task(
                    &runner.registry,
                    &runner.task_id,
                    visible_error.clone(),
                    background,
                );
                let _ = runner.persist_snapshot("failed", None, Some(error));
                ("failed".into(), None, agent_count, Some(visible_error))
            }
        }
    };
    let workflow_progress = workflow_progress_for_task(&runner.registry, &runner.task_id);
    release_local_workflow_run(&runner.registry, &runner.run_id, &runner.task_id);
    WorkflowTerminalOutcome {
        status,
        result,
        workflow_progress,
        agent_count,
        error,
    }
}

fn workflow_agent_count(registry: &TaskRegistry, id: &TaskId) -> u64 {
    registry
        .snapshot(id)
        .and_then(|snapshot| match snapshot.data {
            TaskData::LocalWorkflow(data) => Some(data.agent_count),
            _ => None,
        })
        .unwrap_or(0)
}

fn model_visible_workflow_failure(error: &str, agent_count: u64, run_id: &str) -> String {
    let message = if agent_count == 0 {
        format!(
            "Workflow failed before any agent calls ran. No requested implementation or verification was performed. {error}"
        )
    } else {
        format!(
            "Workflow failed after {agent_count} agent call(s). The requested workflow did not complete; do not treat its implementation or verification as complete. Completed agent results are cached: fix the cause and resubmit with resumeFromRunId: \"{run_id}\" (a corrected script may ride along) — unchanged agent() calls replay from cache and only the failed/changed ones run. {error}"
        )
    };
    bounded_workflow_text(&message, 2_048)
}

fn bounded_workflow_launch_status(mut status: WorkflowLaunchStatus) -> WorkflowLaunchStatus {
    status.status = bounded_workflow_text(&status.status, 32);
    status.task_id = status
        .task_id
        .as_deref()
        .map(|task_id| bounded_workflow_text(task_id, 128));
    status.run_id = status
        .run_id
        .as_deref()
        .map(|run_id| bounded_workflow_text(run_id, 96));
    status.summary = status
        .summary
        .as_deref()
        .map(|summary| bounded_workflow_text(summary, 384));
    status.transcript_dir = status
        .transcript_dir
        .as_deref()
        .map(|path| bounded_workflow_text(path, 1_024));
    status.script_path = status
        .script_path
        .as_deref()
        .map(|path| bounded_workflow_text(path, 1_024));
    status.warning = status
        .warning
        .as_deref()
        .map(|warning| bounded_workflow_text(warning, 384));
    status.error = status
        .error
        .as_deref()
        .map(|error| bounded_workflow_text(error, 2_048));
    status
}

fn bounded_workflow_args(args: Option<&Value>) -> Option<Value> {
    const MAX_ARGS_CHARS: usize = 2_048;
    args.map(|args| {
        if args.to_string().chars().count() <= MAX_ARGS_CHARS {
            args.clone()
        } else {
            json!({ "truncated": true })
        }
    })
}

fn workflow_status_item(
    snapshot: &TaskSnapshot,
    run_filter: Option<&str>,
    name_filter: Option<&str>,
) -> Option<Value> {
    let TaskData::LocalWorkflow(data) = &snapshot.data else {
        return None;
    };
    if run_filter.is_some_and(|run_id| data.run_id != run_id) {
        return None;
    }
    if name_filter.is_some_and(|name| data.workflow_name != name) {
        return None;
    }
    let workflow_progress = workflow_progress_object_value(
        &data.run_id,
        &data.workflow_name,
        data.summary.as_deref(),
        &data.progress_entries,
    );
    Some(json!({
        "taskId": snapshot.id.to_string(),
        "runId": bounded_workflow_text(&data.run_id, 96),
        "workflowName": bounded_workflow_text(&data.workflow_name, 192),
        "summary": data
            .summary
            .as_deref()
            .map(|summary| bounded_workflow_text(summary, 384)),
        "status": snapshot.status.as_str(),
        "lastProgress": snapshot
            .last_progress
            .as_deref()
            .map(|progress| bounded_workflow_text(progress, 512)),
        "startTimeMs": snapshot.start_time_ms,
        "agentCount": data.agent_count,
        "tokenCount": data.token_count,
        "toolUseCount": data.tool_use_count,
        "transcriptDir": data
            .output_path
            .as_deref()
            .map(|path| bounded_workflow_text(path, 1_024)),
        "scriptPath": data
            .script_path
            .as_deref()
            .map(|path| bounded_workflow_text(path, 1_024)),
        "args": bounded_workflow_args(data.args.as_ref()),
        "workflowProgress": workflow_progress,
    }))
}

fn value_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn value_u64(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn non_empty_str(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn is_running_status_query(spec: &WorkflowLaunchSpec) -> bool {
    spec.status
        .as_deref()
        .map(str::trim)
        .is_some_and(|status| status.eq_ignore_ascii_case("running"))
}

fn validate_workflow_launch_spec(spec: &WorkflowLaunchSpec) -> Result<(), String> {
    // `resumeFromRunId` composes with `script` / `scriptPath` / `name`: agent
    // results are cached by content hash of (prompt, opts), so an EDITED
    // script resuming a run replays every unchanged agent() from cache and
    // only executes the calls that actually changed. Restricting resume to
    // the original script forced a full re-run for every revision.

    let Some(status) = non_empty_str(spec.status.as_deref()) else {
        return Ok(());
    };
    if !status.eq_ignore_ascii_case("running") {
        return Err(
            "Unsupported workflow status query; only status: \"running\" is supported".into(),
        );
    }
    if spec.run_in_background {
        return Err("status: \"running\" cannot be combined with runInBackground: true".into());
    }
    if non_empty_str(spec.script.as_deref()).is_some()
        || non_empty_str(spec.script_path.as_deref()).is_some()
    {
        return Err(
            "status: \"running\" is a query and cannot be combined with script or scriptPath"
                .into(),
        );
    }
    Ok(())
}

fn emit_workflow_progress(
    context: &ToolContext,
    entry: &WorkflowProgressEntry,
    sequence: u64,
    metadata: Option<&WorkflowProgressMetadata>,
) {
    let message = bounded_workflow_text(
        &match entry {
            WorkflowProgressEntry::Agent {
                state,
                phase_title,
                label,
                ..
            } => match phase_title
                .as_deref()
                .filter(|phase| !phase.trim().is_empty())
            {
                Some(phase) => format!("{phase}: agent {state}: {label}"),
                None => format!("agent {state}: {label}"),
            },
            WorkflowProgressEntry::Phase { title, state, .. } => {
                format!("phase {state}: {title}")
            }
            WorkflowProgressEntry::Log { message } => message.clone(),
        },
        512,
    );
    context.emit_progress(
        ToolProgressUpdate::new("workflow_progress")
            .with_message(message)
            .with_payload(workflow_progress_payload(entry, sequence, metadata)),
    );
}

fn emit_workflow_task_progress(
    registry: &TaskRegistry,
    id: &TaskId,
    context: &ToolContext,
    entry: WorkflowProgressEntry,
) {
    let sequence = push_local_workflow_progress(registry, id, entry.clone());
    let metadata = workflow_progress_metadata(registry, id);
    emit_workflow_progress(context, &entry, sequence, metadata.as_ref());
}

fn emit_workflow_task_completed(context: &ToolContext, status: &WorkflowLaunchStatus) {
    let message = bounded_workflow_text(
        &match status.error.as_deref() {
            Some(error) => format!("workflow failed: {error}"),
            None => {
                let agent_count = status.agent_count.unwrap_or(0);
                format!("workflow completed: {agent_count} agent(s)")
            }
        },
        512,
    );
    let mut payload = serde_json::to_value(status).unwrap_or_else(|_| json!({}));
    if let Some(object) = payload.as_object_mut() {
        object.remove("result");
    }
    context.emit_progress(
        ToolProgressUpdate::new("workflow_progress")
            .with_message(message)
            .with_payload(payload),
    );
}

struct WorkflowRun {
    registry: TaskRegistry,
    task_id: TaskId,
    run_id: String,
    transcript_dir: PathBuf,
    persisted_script: PathBuf,
    cwd: PathBuf,
    script: String,
    meta: WorkflowMeta,
    args: Option<Value>,
    context: ToolContext,
    cancel: PromptCancel,
    depth: usize,
    permission_prompts_unavailable: bool,
    nested_workflows: Vec<WorkflowResolvedScript>,
    budget_total: Option<u64>,
    /// Per-run override of the agent concurrency cap (`None` = derive from
    /// the machine). Injected by concurrency tests so they hold a fixed
    /// slot count instead of sharing mutable process-global state.
    agent_concurrency_cap: Option<usize>,
}

impl WorkflowRun {
    async fn execute(&self) -> Result<Value, String> {
        if self.cancel.is_cancelled() {
            return Err("Workflow aborted".into());
        }
        if !WorkflowNesting::at(self.depth).may_launch_workflow() {
            return Err("Nested workflow depth exceeded".into());
        }
        let parsed = parse_workflow_meta(&self.script)?;
        let journal = WorkflowJournal::new(self.transcript_dir.join("journal.jsonl"));
        let cached = journal.load()?;
        let runtime = ScriptRuntime::new(self, parsed.script_body, journal, cached);
        runtime.run().await
    }

    fn persist_snapshot(
        &self,
        status: &str,
        result: Option<Value>,
        error: Option<String>,
    ) -> Result<(), String> {
        let snapshot_dir = self
            .persisted_script
            .parent()
            .or_else(|| self.transcript_dir.parent())
            .unwrap_or(&self.transcript_dir);
        fs::create_dir_all(snapshot_dir)
            .map_err(|err| format!("Failed to create workflow snapshot dir: {err}"))?;
        fs::create_dir_all(self.transcript_dir.clone())
            .map_err(|err| format!("Failed to create workflow transcript dir: {err}"))?;
        let snapshot = json!({
            "runId": self.run_id,
            "timestamp": now_ms(),
            "taskId": self.task_id.to_string(),
            "script": self.script,
            "scriptPath": self.persisted_script.display().to_string(),
            "args": self.args,
            "result": result,
            "error": error,
            "summary": self.meta.description,
            "workflowName": self.meta.name,
            "title": self.meta.title,
            "status": status,
            "phases": self.meta.phases,
        });
        let path = snapshot_dir.join(format!("{}.json", self.run_id));
        fs::write(&path, serde_json::to_string_pretty(&snapshot).unwrap()).map_err(|err| {
            format!(
                "Failed to write workflow snapshot {}: {err}",
                path.display()
            )
        })
    }
}

#[derive(Clone)]
struct ScriptRuntime {
    input: WorkflowRuntimeInput,
}

#[derive(Clone)]
struct WorkflowRuntimeInput {
    registry: TaskRegistry,
    task_id: TaskId,
    run_id: String,
    body: String,
    cwd: PathBuf,
    args: Option<Value>,
    /// The parsed `export const meta` object, re-exposed to the script body
    /// as a `meta` global — scripts naturally write `return { workflow:
    /// meta.name }` and the parser strips the declaration they'd rely on.
    meta: Option<Value>,
    context: ToolContext,
    cancel: PromptCancel,
    journal: WorkflowJournal,
    cached: HashMap<String, Value>,
    nested_workflows: Vec<WorkflowResolvedScript>,
    depth: usize,
    permission_prompts_unavailable: bool,
    budget_total: Option<u64>,
    agent_concurrency_cap: Option<usize>,
}

#[derive(Debug, Clone)]
struct WorkflowPhaseContext {
    id: String,
    title: String,
}

struct WorkflowHostState {
    registry: TaskRegistry,
    task_id: TaskId,
    run_id: String,
    context: ToolContext,
    cancel: PromptCancel,
    journal: WorkflowJournal,
    cached: HashMap<String, Value>,
    nested_workflows: Vec<WorkflowResolvedScript>,
    cwd: PathBuf,
    handle: tokio::runtime::Handle,
    agent_count: u64,
    logs: Vec<String>,
    current_phase: Option<WorkflowPhaseContext>,
    phase_count: u64,
    phase_parents: HashMap<String, Option<String>>,
    active_phases: HashMap<String, WorkflowPhaseContext>,
    depth: usize,
    permission_prompts_unavailable: bool,
    /// Hard token ceiling for the run (`None` = unbounded). Mirrors `budget.total`.
    budget_total: Option<u64>,
    /// Resolved agent concurrency cap for THIS run (test override applied,
    /// else machine-derived). Instance state on purpose: a process-global
    /// override raced with parallel tests reading the cap.
    agent_concurrency_cap: usize,
    /// Running total of output tokens spent across every agent (cached or live).
    /// Shared with spawned tasks so `budget.spent()` reflects in-flight work.
    spent_tokens: Arc<AtomicU64>,
    /// Per-base-key occurrence counter used to disambiguate duplicate agent
    /// calls for resume caching. Replayed deterministically in call order.
    call_occurrences: HashMap<String, u64>,
}

#[derive(Trace, Finalize, boa_engine::JsData)]
struct BoaWorkflowHostState {
    #[unsafe_ignore_trace]
    state: Arc<Mutex<WorkflowHostState>>,
}

impl ScriptRuntime {
    fn new(
        run: &WorkflowRun,
        body: String,
        journal: WorkflowJournal,
        cached: HashMap<String, Value>,
    ) -> Self {
        Self {
            input: WorkflowRuntimeInput {
                registry: run.registry.clone(),
                task_id: run.task_id.clone(),
                run_id: run.run_id.clone(),
                body,
                cwd: run.cwd.clone(),
                args: run.args.clone(),
                meta: serde_json::to_value(&run.meta).ok(),
                context: run.context.clone(),
                cancel: run.cancel.clone(),
                journal,
                cached,
                nested_workflows: run.nested_workflows.clone(),
                depth: run.depth,
                permission_prompts_unavailable: run.permission_prompts_unavailable,
                budget_total: run.budget_total,
                agent_concurrency_cap: run.agent_concurrency_cap,
            },
        }
    }

    async fn run(self) -> Result<Value, String> {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || run_workflow_js(self.input, handle))
            .await
            .map_err(|err| format!("Workflow runtime task failed: {err}"))?
    }
}

fn run_workflow_js(
    input: WorkflowRuntimeInput,
    handle: tokio::runtime::Handle,
) -> Result<Value, String> {
    let state = Arc::new(Mutex::new(WorkflowHostState {
        registry: input.registry,
        task_id: input.task_id,
        run_id: input.run_id,
        context: input.context,
        cancel: input.cancel,
        journal: input.journal,
        cached: input.cached,
        nested_workflows: input.nested_workflows,
        cwd: input.cwd,
        handle,
        agent_count: 0,
        logs: Vec::new(),
        current_phase: None,
        phase_count: 0,
        phase_parents: HashMap::new(),
        active_phases: HashMap::new(),
        depth: input.depth,
        permission_prompts_unavailable: input.permission_prompts_unavailable,
        budget_total: input.budget_total,
        agent_concurrency_cap: input
            .agent_concurrency_cap
            .map(|cap| cap.clamp(1, MAX_WORKFLOW_AGENT_CONCURRENCY))
            .unwrap_or_else(workflow_agent_concurrency_cap),
        spent_tokens: Arc::new(AtomicU64::new(0)),
        call_occurrences: HashMap::new(),
    }));
    let result = run_workflow_js_body(state.clone(), input.body, input.args, input.meta)?;
    let state = state
        .lock()
        .map_err(|_| "workflow state poisoned".to_string())?;
    Ok(json!({
        "result": result,
        "agentCount": state.agent_count,
        "outputPath": workflow_output_path_for_state(&state),
        "logs": state.logs,
    }))
}

fn workflow_output_path_for_state(state: &WorkflowHostState) -> Option<String> {
    state
        .registry
        .snapshot(&state.task_id)
        .and_then(|snap| match snap.data {
            TaskData::LocalWorkflow(data) => data.output_path,
            _ => None,
        })
}

fn run_workflow_js_body(
    state: Arc<Mutex<WorkflowHostState>>,
    body: String,
    args: Option<Value>,
    meta: Option<Value>,
) -> Result<Value, String> {
    let mut context = BoaContext::default();
    context.insert_data(BoaWorkflowHostState {
        state: state.clone(),
    });
    // Each Boa context (parent or nested workflow) owns its own pending-agent
    // queue so concurrent / nested drivers never share or race on it.
    let pending = Rc::new(RefCell::new(PendingInner {
        next_id: 0,
        queue: VecDeque::new(),
    }));
    context.insert_data(WorkflowPendingQueue {
        inner: pending.clone(),
    });
    register_workflow_js_bindings(&mut context)?;
    let args = JsValue::from_json(&args.unwrap_or(Value::Null), &mut context)
        .map_err(|err| format_boa_error(err, &mut context))?;
    context
        .register_global_property(js_string!("args"), args, Attribute::all())
        .map_err(|err| format_boa_error(err, &mut context))?;
    // The parser consumes `export const meta = {…}` and only the remainder
    // runs, so re-expose the parsed object — scripts reasonably reference
    // `meta.name` in their return value.
    let meta = JsValue::from_json(&meta.unwrap_or(Value::Null), &mut context)
        .map_err(|err| format_boa_error(err, &mut context))?;
    context
        .register_global_property(js_string!("meta"), meta, Attribute::all())
        .map_err(|err| format_boa_error(err, &mut context))?;
    context
        .eval(Source::from_bytes(WORKFLOW_JS_PRELUDE))
        .map_err(|err| format_boa_error(err, &mut context))?;
    let wrapped = format!("(async () => {{\n{body}\n}})()");
    let top = context
        .eval(Source::from_bytes(wrapped.as_str()))
        .map_err(|err| format_boa_error(err, &mut context))?;
    let value = drive_workflow(state, pending, top, &mut context)?;
    js_value_to_json(&value, &mut context)
}

/// Drives the workflow's top-level promise to completion by interleaving the
/// Boa microtask queue (`run_jobs`) with concurrent sub-agent execution.
///
/// `agent()` enqueues a request and returns a pending promise; this loop drains
/// those requests, resolves cached ones inline, spawns the rest on the Tokio
/// runtime (throttled to [`workflow_agent_concurrency_cap`]), and resolves each
/// promise as results arrive — so every item flows independently and wall-clock
/// is bounded by the slowest dependency chain, not a per-stage barrier.
fn drive_workflow(
    state: Arc<Mutex<WorkflowHostState>>,
    pending: Rc<RefCell<PendingInner>>,
    top: JsValue,
    context: &mut BoaContext,
) -> Result<JsValue, String> {
    let cap = state
        .lock()
        .map_err(|_| "workflow state poisoned".to_string())?
        .agent_concurrency_cap;
    let promise = match top
        .as_object()
        .cloned()
        .and_then(|object| JsPromise::from_object(object).ok())
    {
        Some(promise) => promise,
        None => {
            context.run_jobs();
            return Ok(top);
        }
    };

    let (tx, rx) = mpsc::channel::<DriverAgentResult>();
    let mut inflight: usize = 0;
    let mut local: VecDeque<(u64, WorkflowAgentRequest)> = VecDeque::new();

    let outcome = (|| -> Result<JsValue, String> {
        loop {
            context.run_jobs();
            match promise.state() {
                PromiseState::Fulfilled(value) => return Ok(value),
                PromiseState::Rejected(reason) => {
                    return Err(format_boa_error(JsError::from_opaque(reason), context));
                }
                PromiseState::Pending => {}
            }

            {
                let mut queue = pending.borrow_mut();
                while let Some(item) = queue.queue.pop_front() {
                    local.push_back(item);
                }
            }

            let mut made_progress = false;
            while let Some((id, request)) = local.pop_front() {
                let is_cached = {
                    let guard = state
                        .lock()
                        .map_err(|_| "workflow state poisoned".to_string())?;
                    guard.cached.contains_key(&request.key)
                };
                if is_cached {
                    let value = run_agent_request_sync(state.clone(), request)?;
                    resolve_pending(context, id, value)?;
                    made_progress = true;
                } else if inflight < cap {
                    spawn_agent_task(&state, id, request, tx.clone())?;
                    inflight += 1;
                } else {
                    local.push_front((id, request));
                    break;
                }
            }

            if made_progress {
                // Let `run_jobs` observe the cached resolutions before we block on a
                // network round-trip; the continuation may settle `top` or enqueue
                // more work.
                continue;
            }

            if inflight == 0 {
                return Err("Workflow promise did not settle".into());
            }

            let done = rx
                .recv()
                .map_err(|_| "workflow agent task channel closed".to_string())?;
            inflight -= 1;
            match done.result {
                Ok(value) => resolve_pending(context, done.id, value)?,
                Err(error) => return Err(error),
            }
        }
    })();

    // However the run settles — value, script error, or a fail-fast agent
    // error — every spawned agent task must land before this returns: they
    // are detached tokio tasks writing to this run's journal and progress
    // stream, and returning early let the caller mark the run terminal and
    // release the run id while they were still writing (an immediate resume
    // then attached a SECOND journal writer to the same run). Dropping our
    // sender turns `recv` into a completion barrier: it drains every result
    // and disconnects only once all task-held senders are gone. In-flight
    // agents finish naturally and journal their results, which the resume
    // cache then replays.
    drop(tx);
    while rx.recv().is_ok() {}
    outcome
}

/// Resolves the pending promise created for agent `id` with `value`.
///
/// Agent failures abort the whole workflow rather than resolving the promise,
/// so the stored resolver is always invoked with a fulfilled value and never
/// rejected. The resolver lives in the GC-rooted `__wfResolvers` table; we
/// clear the slot afterwards to release it.
fn resolve_pending(context: &mut BoaContext, id: u64, value: Value) -> Result<(), String> {
    let resolvers = context
        .global_object()
        .get(js_string!("__wfResolvers"), context)
        .map_err(|err| format_boa_error(err, context))?;
    let resolvers = resolvers
        .as_object()
        .cloned()
        .ok_or_else(|| "workflow resolver registry missing".to_string())?;
    let key = js_string!(id.to_string());
    let resolve = resolvers
        .get(key.clone(), context)
        .map_err(|err| format_boa_error(err, context))?;
    let resolve_fn = resolve
        .as_object()
        .cloned()
        .and_then(JsFunction::from_object)
        .ok_or_else(|| format!("workflow resolver {id} missing"))?;
    let js_value =
        JsValue::from_json(&value, context).map_err(|err| format_boa_error(err, context))?;
    resolve_fn
        .call(&JsValue::undefined(), &[js_value], context)
        .map_err(|err| format_boa_error(err, context))?;
    resolvers
        .set(key, JsValue::undefined(), false, context)
        .map_err(|err| format_boa_error(err, context))?;
    Ok(())
}

const WORKFLOW_JS_PRELUDE: &str = r#"
Object.defineProperty(globalThis, 'Date', { value: function Date() { throw new Error('Date is disabled in Workflow scripts'); }, writable: false, configurable: false });
Object.defineProperty(globalThis.Date, 'now', { value: function () { throw new Error('Date.now is disabled in Workflow scripts'); }, writable: false, configurable: false });
Object.defineProperty(globalThis.Math, 'random', { value: function () { throw new Error('Math.random is disabled in Workflow scripts'); }, writable: false, configurable: false });
Object.freeze(globalThis.Date);
Object.freeze(globalThis.Math);
function __workflowAgentCall(input, opts) {
  if (input !== null && typeof input === 'object' && !Array.isArray(input) && Object.prototype.hasOwnProperty.call(input, 'prompt')) {
    const agentOpts = {};
    for (const key of Object.keys(input)) {
      if (key !== 'prompt' && key !== 'opts' && key !== 'options') {
        agentOpts[key] = input[key];
      }
    }
    const nestedOpts = opts ?? input.opts ?? input.options ?? null;
    if (nestedOpts !== null && typeof nestedOpts === 'object' && !Array.isArray(nestedOpts)) {
      for (const key of Object.keys(nestedOpts)) {
        agentOpts[key] = nestedOpts[key];
      }
    }
    return __workflowAgent(__workflowPrompt(input.prompt), Object.keys(agentOpts).length === 0 ? nestedOpts : agentOpts);
  }
  return __workflowAgent(__workflowPrompt(input), opts ?? null);
}
function __wfParallel(tasks) {
  const list = Array.from(tasks ?? []);
  return Promise.all(list.map((task) => {
    let value;
    try {
      value = (typeof task === 'function') ? task() : task;
    } catch (error) {
      return null;
    }
    return Promise.resolve(value).then(
      (resolved) => (resolved === undefined ? null : resolved),
      () => null,
    );
  }));
}
function __wfPipeline(items, stages) {
  const list = Array.from(items ?? []);
  const stageList = Array.from(stages ?? []);
  return Promise.all(list.map((item, index) => {
    let chain = Promise.resolve(item);
    for (let stageIndex = 0; stageIndex < stageList.length; stageIndex++) {
      const stage = stageList[stageIndex];
      chain = chain.then((current) => {
        if (current === null || current === undefined) {
          return null;
        }
        if (typeof stage !== 'function') {
          return stage;
        }
        return stage(current, item, index);
      });
    }
    return chain.then(
      (resolved) => (resolved === undefined ? null : resolved),
      () => null,
    );
  }));
}
function __wfPhase(title, fn) {
  title = String(title);
  const phaseId = __workflowPhaseEnter(title);
  if (typeof fn !== 'function') {
    return undefined;
  }
  let out;
  try {
    out = fn();
  } catch (error) {
    __workflowPhaseExit(title, phaseId, false);
    throw error;
  }
  return Promise.resolve(out).then(
    (value) => { __workflowPhaseExit(title, phaseId, true); return value; },
    (error) => { __workflowPhaseExit(title, phaseId, false); throw error; },
  );
}
Object.defineProperty(globalThis, '__wfResolvers', { value: {}, writable: false, configurable: false, enumerable: false });
Object.defineProperty(globalThis, 'agent', { value: __workflowAgentCall, writable: false, configurable: false });
Object.defineProperty(globalThis, 'prompt', { value: (value) => __workflowPrompt(value), writable: false, configurable: false });
Object.defineProperty(globalThis, 'json', { value: (value) => JSON.stringify(value, null, 2), writable: false, configurable: false });
Object.defineProperty(globalThis, 'budget', { value: Object.freeze({ get total() { return __workflowBudgetTotal(); }, spent: () => __workflowBudgetSpent(), remaining: () => __workflowBudgetRemaining() }), writable: false, configurable: false });
Object.defineProperty(globalThis, 'log', { value: (message) => __workflowLog(String(message)), writable: false, configurable: false });
Object.defineProperty(globalThis, 'phase', { value: (title, fn) => __wfPhase(title, fn ?? null), writable: false, configurable: false });
Object.defineProperty(globalThis, 'workflow', { value: (nameOrRef, args) => Promise.resolve(__workflowNested(nameOrRef, args ?? null)), writable: false, configurable: false });
Object.defineProperty(globalThis, 'parallel', { value: (tasks) => __wfParallel(tasks), writable: false, configurable: false });
Object.defineProperty(globalThis, 'pipeline', { value: function(items) {
  return __wfPipeline(items ?? [], Array.prototype.slice.call(arguments, 1));
}, writable: false, configurable: false });
"#;

fn register_workflow_js_bindings(context: &mut BoaContext) -> Result<(), String> {
    for (name, length, function) in [
        (
            "__workflowPrompt",
            1,
            NativeFunction::from_fn_ptr(native_workflow_prompt),
        ),
        (
            "__workflowAgent",
            2,
            NativeFunction::from_fn_ptr(native_workflow_agent),
        ),
        (
            "__workflowLog",
            1,
            NativeFunction::from_fn_ptr(native_workflow_log),
        ),
        (
            "__workflowPhaseEnter",
            1,
            NativeFunction::from_fn_ptr(native_workflow_phase_enter),
        ),
        (
            "__workflowPhaseExit",
            3,
            NativeFunction::from_fn_ptr(native_workflow_phase_exit),
        ),
        (
            "__workflowBudgetTotal",
            0,
            NativeFunction::from_fn_ptr(native_workflow_budget_total),
        ),
        (
            "__workflowBudgetSpent",
            0,
            NativeFunction::from_fn_ptr(native_workflow_budget_spent),
        ),
        (
            "__workflowBudgetRemaining",
            0,
            NativeFunction::from_fn_ptr(native_workflow_budget_remaining),
        ),
        (
            "__workflowNested",
            2,
            NativeFunction::from_fn_ptr(native_workflow_nested),
        ),
    ] {
        context
            .register_global_builtin_callable(js_string!(name), length, function)
            .map_err(|err| format_boa_error(err, context))?;
    }
    Ok(())
}

fn workflow_state(context: &mut BoaContext) -> JsResult<Arc<Mutex<WorkflowHostState>>> {
    context
        .get_data::<BoaWorkflowHostState>()
        .map(|data| data.state.clone())
        .ok_or_else(|| js_type_error("workflow host state missing"))
}

fn workflow_pending_queue(context: &mut BoaContext) -> JsResult<Rc<RefCell<PendingInner>>> {
    context
        .get_data::<WorkflowPendingQueue>()
        .map(|data| data.inner.clone())
        .ok_or_else(|| js_type_error("workflow pending queue missing"))
}

#[derive(Debug)]
struct WorkflowAgentRequest {
    index: u64,
    key: String,
    prompt: String,
    opts: Option<Value>,
    phase_title: Option<String>,
    phase_id: Option<String>,
}

/// Requests created by `agent()` during `run_jobs`, awaiting dispatch by the
/// driver. Each entry pairs the promise id (resolver lookup key) with its
/// request. Held per-context so nested workflows never share the queue.
struct PendingInner {
    next_id: u64,
    queue: VecDeque<(u64, WorkflowAgentRequest)>,
}

#[derive(Trace, Finalize, JsData)]
struct WorkflowPendingQueue {
    #[unsafe_ignore_trace]
    inner: Rc<RefCell<PendingInner>>,
}

/// Result of a spawned sub-agent, routed back to the driver loop. `id` matches
/// the pending promise; `result` is `Ok(value)` (value may be `Null` on agent
/// failure) or `Err` only when the whole workflow must abort.
struct DriverAgentResult {
    id: u64,
    result: Result<Value, String>,
}

#[derive(Debug)]
enum NestedWorkflowRef {
    Name(String),
    ScriptPath(String),
}

fn native_workflow_prompt(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut BoaContext,
) -> JsResult<JsValue> {
    let prompt = js_arg_prompt(args, 0, context)?;
    Ok(js_string!(prompt).into())
}

fn native_workflow_agent(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut BoaContext,
) -> JsResult<JsValue> {
    let prompt = js_arg_prompt(args, 0, context)?;
    let opts = js_arg_json_optional(args, 1, context)?;
    // Validate, count, budget-gate, and key the call now (synchronously, in
    // script order) so determinism and limits hold regardless of completion
    // order; the actual sub-agent runs later in the driver loop.
    let request =
        prepare_agent_request(workflow_state(context)?, prompt, opts).map_err(js_type_error)?;
    let pending = workflow_pending_queue(context)?;
    let id = {
        let mut queue = pending.borrow_mut();
        let id = queue.next_id;
        queue.next_id += 1;
        queue.queue.push_back((id, request));
        id
    };
    let (promise, resolvers) = JsPromise::new_pending(context);
    // Root the resolver in a JS-reachable table so `run_jobs` GC can't collect
    // it before the driver resolves the promise.
    let store = context
        .global_object()
        .get(js_string!("__wfResolvers"), context)?;
    let store = store
        .as_object()
        .cloned()
        .ok_or_else(|| js_type_error("workflow resolver registry missing"))?;
    store.set(
        js_string!(id.to_string()),
        JsValue::from(resolvers.resolve.clone()),
        false,
        context,
    )?;
    Ok(promise.into())
}

fn native_workflow_log(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut BoaContext,
) -> JsResult<JsValue> {
    let message = js_arg_string(args, 0, context)?;
    run_log_sync(workflow_state(context)?, message).map_err(js_type_error)?;
    Ok(JsValue::undefined())
}

fn native_workflow_phase_enter(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut BoaContext,
) -> JsResult<JsValue> {
    let title = js_arg_string(args, 0, context)?;
    let phase_id = start_phase_sync(workflow_state(context)?, title).map_err(js_type_error)?;
    Ok(js_string!(phase_id).into())
}

fn native_workflow_phase_exit(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut BoaContext,
) -> JsResult<JsValue> {
    let title = js_arg_string(args, 0, context)?;
    let phase_id = js_arg_string_optional(args, 1, context)?;
    let ok = args.get_or_undefined(2).to_boolean();
    finish_phase_sync(workflow_state(context)?, title, phase_id, ok).map_err(js_type_error)?;
    Ok(JsValue::undefined())
}

/// Builds a JS number that serializes back to a JSON integer when it fits in
/// `i32` (Boa renders integral `f64`s as `N.0`, which would break integer
/// comparisons); larger token counts fall back to `f64`.
fn js_u64(value: u64) -> JsValue {
    if value <= i32::MAX as u64 {
        JsValue::new(value as i32)
    } else {
        JsValue::new(value as f64)
    }
}

fn native_workflow_budget_total(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut BoaContext,
) -> JsResult<JsValue> {
    let state = workflow_state(context)?;
    let total = {
        let guard = state
            .lock()
            .map_err(|_| js_type_error("workflow state poisoned"))?;
        guard.budget_total
    };
    Ok(match total {
        Some(value) => js_u64(value),
        None => JsValue::null(),
    })
}

fn native_workflow_budget_spent(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut BoaContext,
) -> JsResult<JsValue> {
    let state = workflow_state(context)?;
    let spent = {
        let guard = state
            .lock()
            .map_err(|_| js_type_error("workflow state poisoned"))?;
        guard.spent_tokens.load(Ordering::SeqCst)
    };
    Ok(js_u64(spent))
}

fn native_workflow_budget_remaining(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut BoaContext,
) -> JsResult<JsValue> {
    let state = workflow_state(context)?;
    let (total, spent) = {
        let guard = state
            .lock()
            .map_err(|_| js_type_error("workflow state poisoned"))?;
        (
            guard.budget_total,
            guard.spent_tokens.load(Ordering::SeqCst),
        )
    };
    Ok(match total {
        // No target → unbounded; serializes to JSON null (Infinity is not
        // representable), which scripts read as "no ceiling".
        None => JsValue::new(f64::INFINITY),
        Some(total) => js_u64(total.saturating_sub(spent)),
    })
}

fn native_workflow_nested(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut BoaContext,
) -> JsResult<JsValue> {
    let nested_ref = js_arg_nested_workflow_ref(args, 0, context)?;
    let nested_args = js_arg_json_optional(args, 1, context)?;
    let result = run_nested_workflow_sync(workflow_state(context)?, nested_ref, nested_args)
        .map_err(js_type_error)?;
    JsValue::from_json(&result, context)
}

fn prepare_agent_request(
    state: Arc<Mutex<WorkflowHostState>>,
    prompt: String,
    opts: Option<Value>,
) -> Result<WorkflowAgentRequest, String> {
    let mut state = state
        .lock()
        .map_err(|_| "workflow state poisoned".to_string())?;
    if state.cancel.is_cancelled() {
        return Err("Workflow aborted".into());
    }
    validate_agent_options(opts.as_ref())?;
    // Hard budget ceiling: once spent tokens reach the target, refuse to start
    // new agents: further agent() calls throw.
    if let Some(total) = state.budget_total {
        if state.spent_tokens.load(Ordering::SeqCst) >= total {
            return Err(format!(
                "Workflow token budget exhausted ({total}); no more agents may be spawned"
            ));
        }
    }
    state.agent_count += 1;
    if state.agent_count > MAX_WORKFLOW_AGENT_CALLS {
        return Err(format!(
            "Workflow agent call limit exceeded ({MAX_WORKFLOW_AGENT_CALLS})"
        ));
    }
    let index = state.agent_count;
    // Disambiguate the cache key by occurrence so two identical (prompt, opts)
    // calls don't collapse to one cached result on resume. The first occurrence
    // keeps the bare key (so existing journals replay unchanged); later ones get
    // a stable `#n` suffix. Occurrence is assigned in agent() invocation order,
    // which is deterministic for sequential / parallel / per-item pipeline flow.
    let base_key = compute_agent_call_key(&prompt, opts.as_ref());
    let occurrence = {
        let counter = state.call_occurrences.entry(base_key.clone()).or_insert(0);
        let current = *counter;
        *counter += 1;
        current
    };
    let key = if occurrence == 0 {
        base_key
    } else {
        format!("{base_key}#{occurrence}")
    };
    let option_phase = agent_option_string(opts.as_ref(), "phase");
    let (phase_title, phase_id) = match option_phase {
        Some(title) => {
            let phase_id = state
                .current_phase
                .as_ref()
                .filter(|phase| phase.title == title)
                .map(|phase| phase.id.clone());
            (Some(title), phase_id)
        }
        None => state
            .current_phase
            .as_ref()
            .map(|phase| (Some(phase.title.clone()), Some(phase.id.clone())))
            .unwrap_or((None, None)),
    };
    Ok(WorkflowAgentRequest {
        index,
        key,
        prompt,
        opts,
        phase_title,
        phase_id,
    })
}

fn run_agent_request_sync(
    state: Arc<Mutex<WorkflowHostState>>,
    request: WorkflowAgentRequest,
) -> Result<Value, String> {
    let WorkflowAgentRequest {
        index,
        key,
        prompt,
        opts,
        phase_title,
        phase_id,
    } = request;
    let (
        cached,
        registry,
        task_id,
        run_id,
        context,
        journal,
        handle,
        cancel,
        spent_tokens,
        workflow_nesting_depth,
        permission_prompts_unavailable,
    ) = {
        let state = state
            .lock()
            .map_err(|_| "workflow state poisoned".to_string())?;
        let cached = state.cached.get(&key).cloned();
        (
            cached,
            state.registry.clone(),
            state.task_id.clone(),
            state.run_id.clone(),
            state.context.clone(),
            state.journal.clone(),
            state.handle.clone(),
            state.cancel.clone(),
            state.spent_tokens.clone(),
            state.depth.saturating_add(1),
            state.permission_prompts_unavailable
                || state
                    .registry
                    .snapshot(&state.task_id)
                    .is_some_and(|snapshot| snapshot.is_backgrounded),
        )
    };

    let label = agent_label(&prompt, opts.as_ref());

    if let Some(cached) = cached {
        let cached_tokens = cached.get("tokens").and_then(Value::as_u64).unwrap_or(0);
        // Budget replay charges `outputTokens` (what live runs
        // charge); `tokens` is the display total (billed input +
        // output). Journals from before the split lack the field —
        // charge 0 rather than re-introduce the context-sized
        // overcharge.
        let cached_output_tokens = cached
            .get("outputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        spent_tokens.fetch_add(cached_output_tokens, Ordering::SeqCst);
        emit_workflow_task_progress(
            &registry,
            &task_id,
            &context,
            WorkflowProgressEntry::Agent {
                index,
                state: "completed".into(),
                phase_title,
                phase_id,
                label: label.clone(),
                tokens: cached_tokens,
                tool_calls: cached.get("toolCalls").and_then(Value::as_u64).unwrap_or(0),
                tool_call_details: cached_tool_call_details(&cached),
                duration_ms: cached.get("durationMs").and_then(Value::as_u64),
                error: None,
                // Replayed from a journal: the live spawn's id was not
                // persisted, so the node cannot link to a task.
                agent_id: None,
            },
        );
        let data = cached.get("data").cloned().unwrap_or(Value::Null);
        return Ok(agent_cached_return_data(data, opts.as_ref()));
    }

    let spawner = context
        .sub_agent_spawner()
        .ok_or_else(|| "workflow agent primitive requires a sub-agent spawner".to_string())?
        .clone();
    let schema = agent_option_value(opts.as_ref(), "schema").cloned();
    let mut spec = prepare_workflow_subagent_spec(
        &context,
        prompt.clone(),
        workflow_nesting_depth,
        permission_prompts_unavailable,
        schema.as_ref(),
        opts.as_ref(),
    )?;
    let mut agent_metadata = json!({
        "workflow_run_id": run_id,
        "workflow_phase": phase_title,
        "workflow_phase_instance_id": phase_id,
    });
    if let Some(schema) = schema.as_ref() {
        agent_metadata[WORKFLOW_STRUCTURED_OUTPUT_SCHEMA_KEY] = schema.clone();
    }
    merge_agent_metadata(&mut spec.metadata, agent_metadata);
    let agent_id = rebon_tool::ensure_agent_id(&mut spec.metadata);

    emit_workflow_task_progress(
        &registry,
        &task_id,
        &context,
        WorkflowProgressEntry::Agent {
            index,
            state: "start".into(),
            phase_title: phase_title.clone(),
            phase_id: phase_id.clone(),
            label: label.clone(),
            tokens: 0,
            tool_calls: 0,
            tool_call_details: Vec::new(),
            duration_ms: None,
            error: None,
            agent_id: Some(agent_id.clone()),
        },
    );
    journal.append(json!({
        "type": "started",
        "key": key,
        "prompt": prompt,
        "opts": opts,
        "timestamp": now_ms(),
    }))?;

    let start = now_ms();
    let result = handle.block_on(async {
        if cancel.is_cancelled() {
            Err("Workflow aborted".to_string())
        } else {
            spawner.spawn(spec).await
        }
    });
    finalize_agent_result(
        result,
        AgentFinalizeContext {
            registry,
            task_id,
            context,
            journal,
            index,
            key,
            prompt,
            opts,
            phase_title,
            phase_id,
            start,
            schema,
            label,
            spent_tokens,
            agent_id,
        },
    )
}

struct AgentFinalizeContext {
    registry: TaskRegistry,
    task_id: TaskId,
    context: ToolContext,
    journal: WorkflowJournal,
    index: u64,
    key: String,
    prompt: String,
    opts: Option<Value>,
    phase_title: Option<String>,
    phase_id: Option<String>,
    start: u64,
    schema: Option<Value>,
    label: String,
    spent_tokens: Arc<AtomicU64>,
    agent_id: String,
}

fn finalize_agent_result(
    result: Result<rebon_tool::SubAgentResult, String>,
    ctx: AgentFinalizeContext,
) -> Result<Value, String> {
    match result {
        Ok(result) => {
            let duration = result
                .duration_ms
                .unwrap_or_else(|| now_ms().saturating_sub(ctx.start));
            let extraction = agent_return_data(
                &result,
                ctx.schema.as_ref(),
                &ctx.label,
                ctx.phase_title.as_deref(),
            );
            let AgentReturnData {
                data,
                structured_output,
                agent_result,
            } = match extraction {
                Ok(extraction) => extraction,
                Err(error) => {
                    tracing::warn!(
                        label = %ctx.label,
                        phase = ctx.phase_title.as_deref().unwrap_or("<none>"),
                        status = %result.status,
                        diagnostic_error = %error,
                        final_text_excerpt = %final_text_excerpt(&result.final_text),
                        "workflow agent result rejected"
                    );
                    ctx.journal.append(json!({
                        "type": "result",
                        "key": ctx.key,
                        "prompt": ctx.prompt,
                        "opts": ctx.opts,
                        "data": Value::Null,
                        "agent_final_text": result.final_text.clone(),
                        "structured_output": structured_output_diagnostics_for_error(
                            &result,
                            ctx.schema.as_ref(),
                            Some(error.as_str()),
                        ),
                        "agent_result": agent_result_diagnostics(
                            &result,
                            "none",
                            Some(error.as_str()),
                        ),
                        "tool_visibility": result.diagnostics.as_ref().and_then(|d| d.get("tool_visibility")).cloned().unwrap_or(Value::Null),
                        "validators": result.diagnostics.as_ref().and_then(|d| d.get("validators")).cloned().unwrap_or(Value::Null),
                        "durationMs": duration,
                        "timestamp": now_ms(),
                    }))?;
                    emit_workflow_task_progress(
                        &ctx.registry,
                        &ctx.task_id,
                        &ctx.context,
                        WorkflowProgressEntry::Agent {
                            index: ctx.index,
                            state: "error".into(),
                            phase_title: ctx.phase_title,
                            phase_id: ctx.phase_id,
                            label: ctx.label.clone(),
                            tokens: result.total_tokens.unwrap_or(0),
                            tool_calls: result.tool_call_count as u64,
                            tool_call_details: result
                                .sub_agent_tool_calls
                                .clone()
                                .unwrap_or_default(),
                            duration_ms: Some(duration),
                            error: Some(error.clone()),
                            agent_id: result
                                .agent_id
                                .clone()
                                .or_else(|| Some(ctx.agent_id.clone())),
                        },
                    );
                    // Fail fast: silently resolving null let downstream
                    // phases fabricate output from missing context. The
                    // completed agents' results are journaled, so the fix is
                    // cheap — resume with `resumeFromRunId` replays them from
                    // cache and retries only this call.
                    return Err(error);
                }
            };
            let tokens = result.total_tokens.unwrap_or(0);
            // The budget charges generated output only. `total_tokens`
            // also counts the billed input context (which scales with
            // context size, not work done) and would exhaust the
            // budget far faster than the tool contract's
            // output-token-scale examples ("+500k", `remaining() >
            // 50_000`) imply.
            let output_tokens = result.output_tokens.unwrap_or(0);
            ctx.spent_tokens.fetch_add(output_tokens, Ordering::SeqCst);
            let tool_calls = result.tool_call_count as u64;
            let tool_call_details = result.sub_agent_tool_calls.clone().unwrap_or_default();
            ctx.journal.append(json!({
                "type": "result",
                "key": ctx.key,
                "prompt": ctx.prompt,
                "opts": ctx.opts,
                "data": data,
                "tokens": tokens,
                "outputTokens": output_tokens,
                "toolCalls": tool_calls,
                "toolCallDetails": tool_call_details.clone(),
                "subAgentToolCalls": tool_call_details.clone(),
                "structured_output": structured_output,
                "tool_visibility": result.diagnostics.as_ref().and_then(|d| d.get("tool_visibility")).cloned().unwrap_or(Value::Null),
                "validators": result.diagnostics.as_ref().and_then(|d| d.get("validators")).cloned().unwrap_or(Value::Null),
                "agent_result": agent_result,
                "durationMs": duration,
                "timestamp": now_ms(),
            }))?;
            emit_workflow_task_progress(
                &ctx.registry,
                &ctx.task_id,
                &ctx.context,
                WorkflowProgressEntry::Agent {
                    index: ctx.index,
                    state: "completed".into(),
                    phase_title: ctx.phase_title,
                    phase_id: ctx.phase_id,
                    label: ctx.label.clone(),
                    tokens,
                    tool_calls,
                    tool_call_details,
                    duration_ms: Some(duration),
                    error: None,
                    agent_id: result
                        .agent_id
                        .clone()
                        .or_else(|| Some(ctx.agent_id.clone())),
                },
            );
            Ok(data)
        }
        Err(error) => {
            emit_workflow_task_progress(
                &ctx.registry,
                &ctx.task_id,
                &ctx.context,
                WorkflowProgressEntry::Agent {
                    index: ctx.index,
                    state: "error".into(),
                    phase_title: ctx.phase_title,
                    phase_id: ctx.phase_id,
                    label: ctx.label.clone(),
                    tokens: 0,
                    tool_calls: 0,
                    tool_call_details: Vec::new(),
                    duration_ms: None,
                    error: Some(error.clone()),
                    agent_id: Some(ctx.agent_id),
                },
            );
            // Fail fast on every terminal agent failure: a null flowing
            // downstream makes later phases fabricate output from missing
            // context. Completed results stay journaled — a resume replays
            // them from cache and retries only the failed call.
            Err(error)
        }
    }
}

fn workflow_agent_execution_policy(
    context: &ToolContext,
    structured_output: bool,
) -> Option<rebon_types::ExecutionPolicy> {
    let mut policy = context.execution_policy().cloned().unwrap_or_default();
    // Workflow sub-agents are continuity turns by definition: nobody is
    // watching an interactive dialog mid-run, so the auto-mode gate
    // should deny-with-reason and let the agent adapt instead of
    // parking it on a prompt. Previously this held only when the parent
    // turn already carried a policy (/ultrawork, goals); a model-initiated
    // Workflow call — and every desktop-app workflow — ran without it.
    policy.auto_mode_script_continuity = true;
    if let Some(child_context) = policy.ultraplan.as_ref().and_then(|ultraplan| {
        ultraplan
            .ultrawork_execution_child_turn()
            .or_else(|| ultraplan.workflow_controller_child_turn())
    }) {
        policy.ultraplan = Some(child_context);
        policy.eager_promotions.clear();
    }
    if structured_output {
        if let Some(ultraplan) = policy.ultraplan.as_mut() {
            if !ultraplan
                .allowed_tools
                .iter()
                .any(|tool| tool == STRUCTURED_OUTPUT_TOOL_NAME)
            {
                ultraplan
                    .allowed_tools
                    .push(STRUCTURED_OUTPUT_TOOL_NAME.to_string());
            }
            ultraplan
                .denied_tools
                .retain(|tool| tool != STRUCTURED_OUTPUT_TOOL_NAME);
        }
        policy = policy.with_eager_promotions([STRUCTURED_OUTPUT_TOOL_NAME]);
    }
    policy.is_active().then_some(policy)
}

fn workflow_subagent_system_prompt(schema: Option<&Value>) -> String {
    match schema {
        Some(schema) => format!(
            "{}\n\nJSON schema:\n{}",
            append_workflow_agent_shared_worktree_prompt(Some(
                WORKFLOW_STRUCTURED_SUBAGENT_SYSTEM_PROMPT.to_string(),
            )),
            serde_json::to_string_pretty(schema).expect("serde_json::Value always serializes")
        ),
        None => append_workflow_agent_shared_worktree_prompt(Some(
            WORKFLOW_SUBAGENT_SYSTEM_PROMPT.to_string(),
        )),
    }
}

#[derive(Debug)]
struct AgentReturnData {
    data: Value,
    structured_output: Value,
    agent_result: Value,
}

fn workflow_agent_failure(detail: String) -> String {
    format!(
        "Workflow agent failed; do not treat its requested implementation or verification as complete. {detail}"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructuredOutputFallbackSource {
    PureJson,
    FencedJson,
    LastJsonObject,
}

impl StructuredOutputFallbackSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::PureJson => "pure_json",
            Self::FencedJson => "fenced_json",
            Self::LastJsonObject => "last_json_object",
        }
    }
}

fn agent_return_data(
    result: &rebon_tool::SubAgentResult,
    schema: Option<&Value>,
    label: &str,
    phase: Option<&str>,
) -> Result<AgentReturnData, String> {
    let (tool_call_count, successful_tool_call_count) = structured_output_call_counts(result);
    if result.status != "completed" {
        let detail = if schema.is_some() {
            format!(
                "MissingStructuredOutput: schema workflow agent did not complete successfully; label={label}; phase={}; status={}; error={}; StructuredOutput tool calls={tool_call_count}; successful StructuredOutput tool calls={successful_tool_call_count}",
                phase.unwrap_or("<none>"),
                result.status,
                result.error.as_deref().unwrap_or("<none>"),
            )
        } else {
            format!(
                "AgentExecutionFailed: workflow agent did not complete successfully; label={label}; phase={}; status={}; error={}",
                phase.unwrap_or("<none>"),
                result.status,
                result.error.as_deref().unwrap_or("<none>"),
            )
        };
        return Err(workflow_agent_failure(detail));
    }

    if schema.is_none() {
        return Ok(AgentReturnData {
            data: Value::String(result.final_text.clone()),
            structured_output: structured_output_diagnostics_for_error(result, None, None),
            agent_result: agent_result_diagnostics(result, "final_text", None),
        });
    }
    let schema = schema.expect("checked above");

    if let Some(call) = last_successful_structured_output_call(result) {
        let value = call.get("input").cloned().unwrap_or(Value::Null);
        validate_structured_output_schema(schema, &value).map_err(|reason| {
            workflow_agent_failure(format!(
                "StructuredOutputToolCallSchemaError: label={label}; phase={}; status={}; StructuredOutput tool calls={tool_call_count}; successful StructuredOutput tool calls={successful_tool_call_count}; schema_validation_error={reason}",
                phase.unwrap_or("<none>"),
                result.status,
            ))
        })?;
        return Ok(AgentReturnData {
            data: value,
            structured_output: json!({
                "schema_present": true,
                "tool_call_count": tool_call_count,
                "successful_tool_call_count": successful_tool_call_count,
                "missing_tool_call": false,
                "fallback_attempted": false,
                "fallback_source": "none",
                "fallback_schema_valid": Value::Null,
                "failure_reason": Value::Null,
                "coercion_attempts": structured_output_coercion_attempts(result),
                "coercion_exhausted": structured_output_coercion_exhausted(result),
            }),
            agent_result: agent_result_diagnostics(result, "structured_output_tool", None),
        });
    }

    match extract_structured_output_fallback_json(&result.final_text) {
        Ok((value, source)) => {
            validate_structured_output_schema(schema, &value).map_err(|reason| {
                workflow_agent_failure(format!(
                    "StructuredOutputFallbackSchemaError: label={label}; phase={}; status={}; StructuredOutput tool calls={tool_call_count}; successful StructuredOutput tool calls={successful_tool_call_count}; fallback_source={}; schema_validation_error={reason}",
                    phase.unwrap_or("<none>"),
                    result.status,
                    source.as_str(),
                ))
            })?;
            Ok(AgentReturnData {
                data: value,
                structured_output: json!({
                    "schema_present": true,
                    "tool_call_count": tool_call_count,
                    "successful_tool_call_count": successful_tool_call_count,
                    "missing_tool_call": true,
                    "fallback_attempted": true,
                    "fallback_source": source.as_str(),
                    "fallback_schema_valid": true,
                    "failure_reason": Value::Null,
                    "coercion_attempts": structured_output_coercion_attempts(result),
                    "coercion_exhausted": structured_output_coercion_exhausted(result),
                }),
                agent_result: agent_result_diagnostics(result, "final_text_fallback", None),
            })
        }
        Err(parse_error) => Err(workflow_agent_failure(format!(
            "StructuredOutputFallbackParseError: label={label}; phase={}; status={}; StructuredOutput tool calls={tool_call_count}; successful StructuredOutput tool calls={successful_tool_call_count}; json_extract_error={parse_error}",
            phase.unwrap_or("<none>"),
            result.status,
        ))),
    }
}

fn structured_output_call_counts(result: &rebon_tool::SubAgentResult) -> (usize, usize) {
    let mut total = 0;
    let mut successful = 0;
    if let Some(calls) = result.sub_agent_tool_calls.as_ref() {
        for call in calls {
            if call.get("name").and_then(Value::as_str) == Some(STRUCTURED_OUTPUT_TOOL_NAME) {
                total += 1;
                if call.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                    successful += 1;
                }
            }
        }
    }
    (total, successful)
}

fn last_successful_structured_output_call(result: &rebon_tool::SubAgentResult) -> Option<&Value> {
    result.sub_agent_tool_calls.as_ref().and_then(|calls| {
        calls.iter().rev().find(|call| {
            call.get("name").and_then(Value::as_str) == Some(STRUCTURED_OUTPUT_TOOL_NAME)
                && call.get("ok").and_then(Value::as_bool).unwrap_or(false)
        })
    })
}

fn extract_structured_output_fallback_json(
    final_text: &str,
) -> Result<(Value, StructuredOutputFallbackSource), String> {
    let trimmed = final_text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) if value.is_object() => {
                return Ok((value, StructuredOutputFallbackSource::PureJson));
            }
            Ok(_) => return Err("pure JSON fallback was not a JSON object".to_string()),
            Err(error) => {
                // Continue to fenced/embedded extraction; prose may start/end with braces in examples.
                tracing::debug!(?error, "pure JSON fallback parse failed");
            }
        }
    }

    if let Some(value) = extract_last_fenced_json_object(final_text) {
        return Ok((value, StructuredOutputFallbackSource::FencedJson));
    }

    if let Some(value) = extract_last_json_object(final_text) {
        return Ok((value, StructuredOutputFallbackSource::LastJsonObject));
    }

    Err("could not extract a JSON object from final_text".to_string())
}

fn extract_last_fenced_json_object(text: &str) -> Option<Value> {
    let mut rest = text;
    let mut found = None;
    while let Some(start) = rest.find("```") {
        rest = &rest[start + 3..];
        let Some(line_end) = rest.find('\n') else {
            break;
        };
        let header = rest[..line_end].trim().to_ascii_lowercase();
        rest = &rest[line_end + 1..];
        let Some(end) = rest.find("```") else { break };
        let body = &rest[..end];
        if header == "json" || header.starts_with("json ") {
            if let Ok(value) = serde_json::from_str::<Value>(body.trim()) {
                if value.is_object() {
                    found = Some(value);
                }
            }
        }
        rest = &rest[end + 3..];
    }
    found
}

fn extract_last_json_object(text: &str) -> Option<Value> {
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut in_string = false;
    let mut escape = false;

    for (idx, ch) in text.char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = idx;
                }
                depth = depth.saturating_add(1);
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    candidates.push((start, idx + ch.len_utf8()));
                }
            }
            _ => {}
        }
    }

    candidates.into_iter().rev().find_map(|(start, end)| {
        serde_json::from_str::<Value>(&text[start..end])
            .ok()
            .filter(Value::is_object)
    })
}

fn final_text_excerpt(text: &str) -> String {
    const N: usize = 240;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= N * 2 {
        return text.to_string();
    }
    let head: String = chars.iter().take(N).collect();
    let tail: String = chars
        .iter()
        .rev()
        .take(N)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

fn structured_output_coercion_attempts(result: &rebon_tool::SubAgentResult) -> Value {
    result
        .diagnostics
        .as_ref()
        .and_then(|diagnostics| diagnostics.pointer("/structured_output/coercion_attempts"))
        .cloned()
        .unwrap_or(Value::Number(0.into()))
}

fn structured_output_coercion_exhausted(result: &rebon_tool::SubAgentResult) -> Value {
    result
        .diagnostics
        .as_ref()
        .and_then(|diagnostics| diagnostics.pointer("/structured_output/coercion_exhausted"))
        .cloned()
        .unwrap_or(Value::Bool(false))
}

fn structured_output_diagnostics_for_error(
    result: &rebon_tool::SubAgentResult,
    schema: Option<&Value>,
    failure_reason: Option<&str>,
) -> Value {
    let (tool_call_count, successful_tool_call_count) = structured_output_call_counts(result);
    json!({
        "schema_present": schema.is_some(),
        "tool_call_count": tool_call_count,
        "successful_tool_call_count": successful_tool_call_count,
        "missing_tool_call": schema.is_some() && successful_tool_call_count == 0,
        "fallback_attempted": schema.is_some() && result.status == "completed" && successful_tool_call_count == 0,
        "fallback_source": "none",
        "fallback_schema_valid": Value::Null,
        "failure_reason": failure_reason,
        "coercion_attempts": structured_output_coercion_attempts(result),
        "coercion_exhausted": structured_output_coercion_exhausted(result),
    })
}

fn agent_result_diagnostics(
    result: &rebon_tool::SubAgentResult,
    final_data_source: &str,
    error: Option<&str>,
) -> Value {
    json!({
        "status": result.status.clone(),
        "error": error.or(result.error.as_deref()),
        "output_file": result.output_file.clone(),
        "final_text_excerpt": final_text_excerpt(&result.final_text),
        "final_data_source": final_data_source,
    })
}

fn cached_tool_call_details(cached: &Value) -> Vec<Value> {
    cached
        .get("toolCallDetails")
        .or_else(|| cached.get("subAgentToolCalls"))
        .or_else(|| cached.get("sub_agent_tool_calls"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn agent_cached_return_data(data: Value, opts: Option<&Value>) -> Value {
    if agent_option_value(opts, "schema").is_some() {
        return data;
    }
    match data {
        Value::Object(object) => object
            .get("finalText")
            .and_then(Value::as_str)
            .map(|text| Value::String(text.to_string()))
            .unwrap_or(Value::Object(object)),
        other => other,
    }
}

fn validate_structured_output_schema(schema: &Value, value: &Value) -> Result<(), String> {
    // Single source of truth shared with the `StructuredOutput` tool's
    // own validation, so the tool boundary and this post-hoc check agree
    // on what counts as a valid shape.
    rebon_tool::validate_structured_output(schema, value)
        .map_err(|reason| format!("StructuredOutput {reason}"))
}

fn validate_agent_options(opts: Option<&Value>) -> Result<(), String> {
    let Some(opts) = opts.and_then(Value::as_object) else {
        return Ok(());
    };
    let unsupported: Vec<&str> = opts
        .keys()
        .map(String::as_str)
        .filter(|key| !WORKFLOW_AGENT_OPTION_KEYS.contains(key))
        .collect();
    if unsupported.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Unsupported workflow agent option(s): {}. Supported options are schema, label, phase, model, provider, modelProfile (or model_profile), isolation, agentType (or agent_type), cwd, maxIterations (or max_iterations), and taskKind (or task_kind). `effort` is not supported by workflow agent(); select a configured reasoning profile with modelProfile instead. agent() options are control metadata only; put handoff data such as notes/results into the prompt text with json(value) or JSON.stringify(value, null, 2).",
            unsupported.join(", ")
        ))
    }
}

fn agent_option_value<'a>(opts: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    opts.and_then(Value::as_object)
        .and_then(|object| object.get(key))
        .filter(|value| !value.is_null())
}

fn agent_option_string(opts: Option<&Value>, key: &str) -> Option<String> {
    agent_option_value(opts, key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn agent_label(prompt: &str, opts: Option<&Value>) -> String {
    agent_option_string(opts, "label").unwrap_or_else(|| truncate_chars(prompt, 96))
}

/// Spawns a single non-cached agent on the Tokio runtime. The driver tracks
/// concurrency, so this just emits progress, journals the start, builds the
/// spec, and fires the task; the result is routed back over `tx` tagged with
/// the promise `id`.
fn spawn_agent_task(
    state: &Arc<Mutex<WorkflowHostState>>,
    id: u64,
    request: WorkflowAgentRequest,
    tx: mpsc::Sender<DriverAgentResult>,
) -> Result<(), String> {
    let WorkflowAgentRequest {
        index,
        key,
        prompt,
        opts,
        phase_title,
        phase_id,
    } = request;
    let label = agent_label(&prompt, opts.as_ref());
    let (
        registry,
        task_id,
        run_id,
        journal,
        context,
        handle,
        cancel,
        spent_tokens,
        workflow_nesting_depth,
        permission_prompts_unavailable,
        start,
    ) = {
        let guard = state
            .lock()
            .map_err(|_| "workflow state poisoned".to_string())?;
        if guard.cancel.is_cancelled() {
            return Err("Workflow aborted".into());
        }
        (
            guard.registry.clone(),
            guard.task_id.clone(),
            guard.run_id.clone(),
            guard.journal.clone(),
            guard.context.clone(),
            guard.handle.clone(),
            guard.cancel.clone(),
            guard.spent_tokens.clone(),
            guard.depth.saturating_add(1),
            guard.permission_prompts_unavailable
                || guard
                    .registry
                    .snapshot(&guard.task_id)
                    .is_some_and(|snapshot| snapshot.is_backgrounded),
            now_ms(),
        )
    };
    let spawner = context
        .sub_agent_spawner()
        .ok_or_else(|| "workflow agent primitive requires a sub-agent spawner".to_string())?
        .clone();
    let schema = agent_option_value(opts.as_ref(), "schema").cloned();
    let mut spec = prepare_workflow_subagent_spec(
        &context,
        prompt.clone(),
        workflow_nesting_depth,
        permission_prompts_unavailable,
        schema.as_ref(),
        opts.as_ref(),
    )?;
    let mut agent_metadata = json!({
        "workflow_run_id": run_id,
        "workflow_phase": phase_title,
        "workflow_phase_instance_id": phase_id,
    });
    if let Some(schema) = schema.as_ref() {
        agent_metadata[WORKFLOW_STRUCTURED_OUTPUT_SCHEMA_KEY] = schema.clone();
    }
    merge_agent_metadata(&mut spec.metadata, agent_metadata);
    let agent_id = rebon_tool::ensure_agent_id(&mut spec.metadata);

    emit_workflow_task_progress(
        &registry,
        &task_id,
        &context,
        WorkflowProgressEntry::Agent {
            index,
            state: "start".into(),
            phase_title: phase_title.clone(),
            phase_id: phase_id.clone(),
            label: label.clone(),
            tokens: 0,
            tool_calls: 0,
            tool_call_details: Vec::new(),
            duration_ms: None,
            error: None,
            agent_id: Some(agent_id.clone()),
        },
    );
    journal.append(json!({
        "type": "started",
        "key": key,
        "prompt": prompt,
        "opts": opts,
        "timestamp": now_ms(),
    }))?;
    handle.spawn(async move {
        let result = if cancel.is_cancelled() {
            Err("Workflow aborted".to_string())
        } else {
            spawner.spawn(spec).await
        };
        let result = finalize_agent_result(
            result,
            AgentFinalizeContext {
                registry,
                task_id,
                context,
                journal,
                index,
                key,
                prompt,
                opts,
                phase_title,
                phase_id,
                start,
                schema,
                label,
                spent_tokens,
                agent_id,
            },
        );
        let _ = tx.send(DriverAgentResult { id, result });
    });
    Ok(())
}

fn workflow_agent_concurrency_cap() -> usize {
    let available = std::thread::available_parallelism()
        .map(|value| value.get().saturating_sub(2))
        .unwrap_or(1)
        .max(1);
    available.min(MAX_WORKFLOW_AGENT_CONCURRENCY)
}

fn prepare_workflow_subagent_spec(
    context: &ToolContext,
    prompt: String,
    workflow_nesting_depth: usize,
    permission_prompts_unavailable: bool,
    schema: Option<&Value>,
    opts: Option<&Value>,
) -> Result<SubAgentSpec, String> {
    let mut spec = SubAgentSpec::new(prompt);
    spec.workflow_nesting_depth = workflow_nesting_depth;
    spec.permission_prompts_unavailable = permission_prompts_unavailable;
    spec.permission_broker = context.permission_broker().cloned();
    spec.system = Some(workflow_subagent_system_prompt(schema));
    spec.execution_policy = workflow_agent_execution_policy(context, schema.is_some());
    apply_agent_options(&mut spec, opts);
    ensure_workflow_task_kind(&mut spec);
    enforce_workflow_child_path_scope(context, &mut spec)?;
    Ok(spec)
}

fn apply_agent_options(spec: &mut SubAgentSpec, opts: Option<&Value>) {
    let Some(opts) = opts.and_then(Value::as_object) else {
        return;
    };
    if let Some(model) = opts.get("model").and_then(Value::as_str) {
        spec.model = Some(model.to_string());
    }
    if let Some(profile) = opts
        .get("modelProfile")
        .or_else(|| opts.get("model_profile"))
        .and_then(Value::as_str)
    {
        spec.model_profile = Some(profile.to_string());
    }
    if let Some(provider) = opts.get("provider").and_then(Value::as_str) {
        spec.provider = Some(provider.to_string());
    }
    if let Some(cwd) = opts.get("cwd").and_then(Value::as_str) {
        spec.cwd = Some(cwd.to_string());
    }
    if let Some(max_iterations) = opts.get("maxIterations").and_then(Value::as_u64) {
        spec.max_iterations = max_iterations as usize;
    }
    if let Some(agent_type) = opts
        .get("agentType")
        .or_else(|| opts.get("agent_type"))
        .and_then(Value::as_str)
    {
        merge_agent_metadata(&mut spec.metadata, json!({ "agent_type": agent_type }));
    }
    if let Some(isolation) = opts.get("isolation").and_then(Value::as_str) {
        merge_agent_metadata(&mut spec.metadata, json!({ "isolation": isolation }));
    }
    if let Some(kind) = opts
        .get("taskKind")
        .or_else(|| opts.get("task_kind"))
        .and_then(Value::as_str)
    {
        merge_agent_metadata(&mut spec.metadata, json!({ "task_kind": kind }));
    }
}

/// Workflow agents historically spawned with `task_kind: Other`, so
/// worktree isolation (`coordinator.use_worktree`) never applied to
/// them even when they held writable tools. Label policy-derived
/// writable agents as implementation workers unless the script
/// declared a kind explicitly.
fn ensure_workflow_task_kind(spec: &mut SubAgentSpec) {
    if spec.task_kind.is_some() || metadata_declares_task_kind(&spec.metadata) {
        return;
    }
    let writable = spec
        .execution_policy
        .as_ref()
        .and_then(|policy| policy.ultraplan.as_ref())
        .is_some_and(|ultraplan| {
            ultraplan.allowed_tools.iter().any(|tool| {
                rebon_tools_core::tool_kind_for_name(tool) == rebon_tools_core::ToolKind::FileEdit
            })
        });
    if writable {
        spec.task_kind = Some(SubAgentTaskKind::Implementation);
    }
}

fn metadata_declares_task_kind(metadata: &Value) -> bool {
    [
        "coordinator_task_kind",
        "coordinator_role",
        "task_kind",
        "taskKind",
        "role",
    ]
    .iter()
    .any(|key| metadata.get(key).and_then(Value::as_str).is_some())
}

fn resolve_child_path(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn format_path_roots(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn workflow_parent_path_scope(context: &ToolContext) -> Result<(PathBuf, Vec<PathBuf>), String> {
    let parent_cwd = context
        .cwd()
        .map(PathBuf::from)
        .or_else(|| context.path_scope_roots().first().cloned())
        .map(Ok)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map_err(|err| format!("cannot determine Workflow agent working directory: {err}"))
        })?;
    let mut parent_roots = if context.path_scope_roots().is_empty() {
        vec![parent_cwd.clone()]
    } else {
        context.path_scope_roots().to_vec()
    };
    for directory in context.additional_working_directories() {
        let resolved = resolve_child_path(Path::new(directory), &parent_cwd);
        if !parent_roots.contains(&resolved) {
            parent_roots.push(resolved);
        }
    }
    Ok((parent_cwd, parent_roots))
}

fn enforce_workflow_child_path_scope(
    context: &ToolContext,
    spec: &mut SubAgentSpec,
) -> Result<(), String> {
    let (parent_cwd, parent_roots) = workflow_parent_path_scope(context)?;
    let child_cwd = spec
        .cwd
        .as_deref()
        .map(Path::new)
        .map(|cwd| resolve_child_path(cwd, &parent_cwd));
    if let Some(child_cwd) = child_cwd.as_ref() {
        if !parent_roots
            .iter()
            .any(|root| rebon_tool::path_scope::path_is_within_root(child_cwd, root))
        {
            return Err(format!(
                "Workflow agent `cwd` must stay within parent authorized roots {}; `{}` is outside that scope.",
                format_path_roots(&parent_roots),
                child_cwd.display()
            ));
        }
        spec.cwd = Some(child_cwd.display().to_string());
    }

    let mut resolved_allowed_roots = Vec::new();
    for root in &spec.allowed_roots {
        let resolved = resolve_child_path(root, &parent_cwd);
        if parent_roots
            .iter()
            .any(|parent| rebon_tool::path_scope::path_is_within_root(&resolved, parent))
        {
            if !resolved_allowed_roots.contains(&resolved) {
                resolved_allowed_roots.push(resolved);
            }
            continue;
        }
        let inherited_roots = parent_roots
            .iter()
            .filter(|parent| rebon_tool::path_scope::path_is_within_root(parent, &resolved))
            .cloned()
            .collect::<Vec<_>>();
        if inherited_roots.is_empty() {
            return Err(format!(
                "Workflow agent `allowed_roots` must stay within parent authorized roots {}; `{}` is outside that scope.",
                format_path_roots(&parent_roots),
                resolved.display()
            ));
        }
        for inherited_root in inherited_roots {
            if !resolved_allowed_roots.contains(&inherited_root) {
                resolved_allowed_roots.push(inherited_root);
            }
        }
    }

    if spec.allowed_roots.is_empty() {
        spec.allowed_roots = child_cwd.map(|cwd| vec![cwd]).unwrap_or(parent_roots);
    } else {
        spec.allowed_roots = resolved_allowed_roots;
    }
    Ok(())
}

fn merge_agent_metadata(target: &mut Value, extra: Value) {
    if !target.is_object() {
        *target = json!({});
    }
    let Some(target) = target.as_object_mut() else {
        return;
    };
    if let Some(extra) = extra.as_object() {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn run_log_sync(state: Arc<Mutex<WorkflowHostState>>, message: String) -> Result<(), String> {
    let (registry, task_id, context) = {
        let mut state = state
            .lock()
            .map_err(|_| "workflow state poisoned".to_string())?;
        state.logs.push(message.clone());
        (
            state.registry.clone(),
            state.task_id.clone(),
            state.context.clone(),
        )
    };
    emit_workflow_task_progress(
        &registry,
        &task_id,
        &context,
        WorkflowProgressEntry::Log { message },
    );
    Ok(())
}

fn start_phase_sync(state: Arc<Mutex<WorkflowHostState>>, title: String) -> Result<String, String> {
    let (registry, task_id, context, phase_id) = {
        let mut state = state
            .lock()
            .map_err(|_| "workflow state poisoned".to_string())?;
        state.phase_count = state.phase_count.saturating_add(1);
        let phase_id = format!("phase-{}", state.phase_count);
        let phase = WorkflowPhaseContext {
            id: phase_id.clone(),
            title: title.clone(),
        };
        let parent_id = state.current_phase.as_ref().map(|phase| phase.id.clone());
        state.phase_parents.insert(phase_id.clone(), parent_id);
        state.active_phases.insert(phase_id.clone(), phase.clone());
        state.current_phase = Some(phase);
        (
            state.registry.clone(),
            state.task_id.clone(),
            state.context.clone(),
            phase_id,
        )
    };
    emit_workflow_task_progress(
        &registry,
        &task_id,
        &context,
        WorkflowProgressEntry::Phase {
            title,
            state: "start".into(),
            phase_id: Some(phase_id.clone()),
        },
    );
    Ok(phase_id)
}

fn finish_phase_sync(
    state: Arc<Mutex<WorkflowHostState>>,
    title: String,
    phase_id: Option<String>,
    ok: bool,
) -> Result<(), String> {
    let (registry, task_id, context, phase_id) = {
        let mut state = state
            .lock()
            .map_err(|_| "workflow state poisoned".to_string())?;
        let phase_id = phase_id.or_else(|| {
            state
                .current_phase
                .as_ref()
                .filter(|phase| phase.title == title)
                .map(|phase| phase.id.clone())
        });
        if let Some(phase_id) = phase_id.as_ref() {
            state.active_phases.remove(phase_id);
            if state
                .current_phase
                .as_ref()
                .is_some_and(|phase| phase.id == *phase_id)
            {
                let mut parent_id = state.phase_parents.get(phase_id).cloned().flatten();
                state.current_phase = None;
                while let Some(candidate_id) = parent_id {
                    if let Some(parent) = state.active_phases.get(&candidate_id).cloned() {
                        state.current_phase = Some(parent);
                        break;
                    }
                    parent_id = state.phase_parents.get(&candidate_id).cloned().flatten();
                }
            }
        }
        (
            state.registry.clone(),
            state.task_id.clone(),
            state.context.clone(),
            phase_id,
        )
    };
    emit_workflow_task_progress(
        &registry,
        &task_id,
        &context,
        WorkflowProgressEntry::Phase {
            title,
            state: if ok { "completed" } else { "error" }.into(),
            phase_id,
        },
    );
    Ok(())
}

fn run_nested_workflow_sync(
    state: Arc<Mutex<WorkflowHostState>>,
    nested_ref: NestedWorkflowRef,
    args: Option<Value>,
) -> Result<Value, String> {
    let (script, name) = {
        let state = state
            .lock()
            .map_err(|_| "workflow state poisoned".to_string())?;
        if !WorkflowNesting::at(state.depth)
            .child()
            .may_launch_workflow()
        {
            return Err("Nested workflow depth exceeded".into());
        }
        match nested_ref {
            NestedWorkflowRef::Name(name) => {
                let script = state
                    .nested_workflows
                    .iter()
                    .find(|workflow| workflow.meta.name == name)
                    .map(|workflow| workflow.script.clone())
                    .ok_or_else(|| format!("Workflow \"{name}\" not found"))?;
                (script, name)
            }
            NestedWorkflowRef::ScriptPath(script_path) => {
                let path = resolve_nested_workflow_script_path(&state.cwd, &script_path);
                let script = fs::read_to_string(&path).map_err(|err| {
                    format!(
                        "Failed to read nested workflow at {}: {err}",
                        path.display()
                    )
                })?;
                (script, path.display().to_string())
            }
        }
    };
    let parsed = parse_workflow_meta(&script)?;
    let phase_title = format!("▸ {}", parsed.meta.name);
    let meta = serde_json::to_value(&parsed.meta).ok();
    let phase_id = start_phase_sync(state.clone(), phase_title.clone())?;
    let result = run_workflow_js_body_with_depth(state.clone(), parsed.script_body, args, meta, 1);
    finish_phase_sync(state, phase_title, Some(phase_id), result.is_ok())?;
    result.map_err(|err| format!("Nested workflow {name} failed: {err}"))
}

fn resolve_nested_workflow_script_path(cwd: &Path, script_path: &str) -> PathBuf {
    let path = PathBuf::from(script_path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn run_workflow_js_body_with_depth(
    state: Arc<Mutex<WorkflowHostState>>,
    body: String,
    args: Option<Value>,
    meta: Option<Value>,
    depth_delta: usize,
) -> Result<Value, String> {
    if depth_delta > 0 {
        let mut guard = state
            .lock()
            .map_err(|_| "workflow state poisoned".to_string())?;
        guard.depth += depth_delta;
    }
    let result = run_workflow_js_body(state.clone(), body, args, meta);
    if depth_delta > 0 {
        {
            let mut guard = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.depth = guard.depth.saturating_sub(depth_delta);
        }
    }
    result
}

fn js_arg_string(args: &[JsValue], index: usize, context: &mut BoaContext) -> JsResult<String> {
    args.get_or_undefined(index)
        .to_string(context)
        .map(|value| value.to_std_string_escaped())
}

fn js_arg_string_optional(
    args: &[JsValue],
    index: usize,
    context: &mut BoaContext,
) -> JsResult<Option<String>> {
    let value = args.get_or_undefined(index);
    if value.is_null_or_undefined() {
        Ok(None)
    } else {
        Ok(Some(value.to_string(context)?.to_std_string_escaped()))
    }
}

fn js_arg_nested_workflow_ref(
    args: &[JsValue],
    index: usize,
    context: &mut BoaContext,
) -> JsResult<NestedWorkflowRef> {
    let value = args.get_or_undefined(index);
    if let Some(object) = value.as_object() {
        if let Some(script_path) = object
            .get(js_string!("scriptPath"), context)?
            .as_string()
            .map(|value| value.to_std_string_escaped())
            .filter(|value| !value.trim().is_empty())
        {
            return Ok(NestedWorkflowRef::ScriptPath(script_path));
        }
    }
    js_arg_string(args, index, context).map(NestedWorkflowRef::Name)
}

fn js_arg_prompt(args: &[JsValue], index: usize, context: &mut BoaContext) -> JsResult<String> {
    let value = args.get_or_undefined(index);
    if value.is_null_or_undefined() {
        return Ok(String::new());
    }
    if value.is_object() {
        let json = value.to_json(context)?;
        Ok(serde_json::to_string_pretty(&json).expect("serde_json::Value always serializes"))
    } else {
        value
            .to_string(context)
            .map(|value| value.to_std_string_escaped())
    }
}

fn js_arg_json_optional(
    args: &[JsValue],
    index: usize,
    context: &mut BoaContext,
) -> JsResult<Option<Value>> {
    let value = args.get_or_undefined(index);
    if value.is_null_or_undefined() {
        return Ok(None);
    }
    value.to_json(context).map(Some)
}

fn js_value_to_json(value: &JsValue, context: &mut BoaContext) -> Result<Value, String> {
    if value.is_undefined() {
        return Ok(Value::Null);
    }
    value
        .to_json(context)
        .map_err(|err| format_boa_error(err, context))
}

fn js_type_error(message: impl Into<String>) -> JsError {
    JsNativeError::typ().with_message(message.into()).into()
}

fn format_boa_error(err: JsError, context: &mut BoaContext) -> String {
    err.to_opaque(context)
        .to_string(context)
        .map(|value| value.to_std_string_escaped())
        .unwrap_or_else(|_| "workflow JavaScript error".into())
}

#[derive(Clone)]
struct WorkflowJournal {
    path: PathBuf,
}

impl WorkflowJournal {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn load(&self) -> Result<HashMap<String, Value>, String> {
        let Ok(content) = fs::read_to_string(&self.path) else {
            return Ok(HashMap::new());
        };
        let mut results = HashMap::new();
        for line in content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) == Some("result") {
                // A failed agent's entry (null data + recorded error) is a
                // diagnostic, not a reusable result — replaying it would make
                // a resume return null again instead of retrying the call.
                let failed = value
                    .get("agent_result")
                    .and_then(|result| result.get("error"))
                    .is_some_and(|error| !error.is_null());
                if failed {
                    continue;
                }
                if let Some(key) = value.get("key").and_then(Value::as_str) {
                    results.insert(key.to_string(), value);
                }
            }
        }
        Ok(results)
    }

    fn append(&self, entry: Value) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "Failed to create workflow journal dir {}: {err}",
                    parent.display()
                )
            })?;
        }
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|err| {
                format!(
                    "Failed to open workflow journal {}: {err}",
                    self.path.display()
                )
            })?;
        writeln!(file, "{}", serde_json::to_string(&entry).unwrap()).map_err(|err| {
            format!(
                "Failed to write workflow journal {}: {err}",
                self.path.display()
            )
        })
    }
}

pub fn parse_workflow_meta(script: &str) -> Result<ParsedWorkflowScript, String> {
    if script.len() > SCRIPT_MAX_BYTES {
        return Err(format!("Script exceeds {SCRIPT_MAX_BYTES} bytes"));
    }
    let trimmed = script.trim_start();
    if !trimmed.starts_with("export const meta") {
        return Err("`export const meta = { name, description, phases }` must be the FIRST statement in the script".into());
    }
    let start = trimmed
        .find('{')
        .ok_or_else(|| "meta export must contain an object literal".to_string())?;
    let end = find_matching_brace(trimmed, start)
        .ok_or_else(|| "meta object literal is not closed".to_string())?;
    let meta_literal = &trimmed[start..=end];
    reject_impure_meta(meta_literal)?;
    let meta_json = js_object_literal_to_jsonish(meta_literal);
    let meta: WorkflowMeta = serde_json::from_str(&meta_json)
        .map_err(|err| format!("meta must be a pure literal: {err}"))?;
    if meta.name.trim().is_empty() {
        return Err("meta.name must be a non-empty string".into());
    }
    if meta.description.trim().is_empty() {
        return Err("meta.description must be a non-empty string".into());
    }
    let script_body = trimmed[end + 1..]
        .trim_start_matches(';')
        .trim_start()
        .to_string();
    Ok(ParsedWorkflowScript { meta, script_body })
}

fn reject_impure_meta(meta_literal: &str) -> Result<(), String> {
    for needle in [
        "...",
        "=>",
        "function",
        "Date.now",
        "Math.random",
        "new Date",
    ] {
        if meta_literal.contains(needle) {
            return Err(format!("unsupported expression in meta: {needle}"));
        }
    }
    for key in ["__proto__", "constructor", "prototype"] {
        if meta_literal.contains(key) {
            return Err(format!("reserved key name not allowed in meta: {key}"));
        }
    }
    Ok(())
}

fn find_matching_brace(text: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut escaped = false;
    for (idx, ch) in text.char_indices().skip_while(|(idx, _)| *idx < start) {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match ch {
            '\'' if !in_double && !in_backtick => in_single = !in_single,
            '"' if !in_single && !in_backtick => in_double = !in_double,
            '`' if !in_single && !in_double => in_backtick = !in_backtick,
            '{' if !in_single && !in_double && !in_backtick => depth += 1,
            '}' if !in_single && !in_double && !in_backtick => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn js_object_literal_to_jsonish(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            out.push(ch);
            escaped = true;
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
                out.push('"');
            } else if ch == '"' {
                out.push_str("\\\"");
            } else {
                out.push(ch);
            }
            continue;
        }
        if in_double {
            if ch == '"' {
                in_double = false;
            }
            out.push(ch);
            continue;
        }
        match ch {
            '\'' => {
                in_single = true;
                out.push('"');
            }
            '"' => {
                in_double = true;
                out.push(ch);
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut ident = String::from(c);
                while let Some(next) = chars.peek().copied() {
                    if next.is_ascii_alphanumeric() || next == '_' {
                        ident.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let before = out.chars().rev().find(|c| !c.is_whitespace());
                let after = chars.clone().find(|c| !c.is_whitespace());
                if matches!(after, Some(':')) && matches!(before, Some('{' | ',')) {
                    out.push('"');
                    out.push_str(&ident);
                    out.push('"');
                } else {
                    out.push_str(&ident);
                }
            }
            _ => out.push(ch),
        }
    }
    regex::Regex::new(r",\s*([}\]])")
        .unwrap()
        .replace_all(&out, "$1")
        .into_owned()
}

pub fn workflow_contains_nondeterminism(script: &str) -> bool {
    let stripped = strip_js_strings_and_comments(script);
    [
        "Date.now",
        "Math.random",
        "new Date",
        "Date[",
        "Date .",
        "Date(",
        "new\nDate",
        "new\rDate",
        "globalThis.Date",
        "globalThis[",
        "Math[",
        "Math . random",
    ]
    .iter()
    .any(|needle| stripped.contains(needle))
}

/// Advisory check: a workflow whose meta declares review/audit/research-style
/// intent and that fans out to multiple agents should also contain some
/// verification signal (an adversarial refuter, judge vote, verify phase, or
/// completeness critic). Single-pass findings from such workflows are the
/// failure mode this warning exists to surface.
fn workflow_lacks_verification_stage(meta: &WorkflowMeta, script: &str) -> bool {
    let intent = format!(
        "{} {} {} {}",
        meta.name,
        meta.title.as_deref().unwrap_or_default(),
        meta.description,
        meta.when_to_use.as_deref().unwrap_or_default()
    )
    .to_lowercase();
    let looks_review_style = [
        "review",
        "audit",
        "research",
        "bug hunt",
        "bughunt",
        "find bugs",
    ]
    .iter()
    .any(|keyword| intent.contains(keyword));
    if !looks_review_style {
        return false;
    }
    let script_lower = script.to_lowercase();
    ![
        "verif", "refute", "judge", "adversar", "vote", "critic", "skeptic",
    ]
    .iter()
    .any(|signal| script_lower.contains(signal))
}

fn strip_js_strings_and_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if escaped {
            escaped = false;
            out.push(' ');
            continue;
        }
        if ch == '\\' && (in_single || in_double || in_backtick) {
            escaped = true;
            out.push(' ');
            continue;
        }
        if !in_single && !in_double && !in_backtick && ch == '/' {
            if chars.peek() == Some(&'/') {
                chars.next();
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
                continue;
            }
            if chars.peek() == Some(&'*') {
                chars.next();
                let mut prev = '\0';
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                    } else {
                        out.push(' ');
                    }
                    if prev == '*' && next == '/' {
                        break;
                    }
                    prev = next;
                }
                continue;
            }
        }
        match ch {
            '\'' if !in_double && !in_backtick => {
                in_single = !in_single;
                out.push(' ');
            }
            '"' if !in_single && !in_backtick => {
                in_double = !in_double;
                out.push(' ');
            }
            '`' if !in_single && !in_double => {
                in_backtick = !in_backtick;
                out.push(' ');
            }
            _ if in_single || in_double || in_backtick => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

pub fn compute_agent_call_key(prompt: &str, opts: Option<&Value>) -> String {
    let opts = opts.map(stable_relevant_opts).unwrap_or_else(|| json!({}));
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_string(&opts).unwrap_or_default().as_bytes());
    format!("agent:{:x}", hasher.finalize())
}

fn stable_relevant_opts(opts: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in WORKFLOW_AGENT_OPTION_KEYS {
        if let Some(value) = opts.get(*key).filter(|value| !value.is_null()) {
            out.insert((*key).to_string(), sort_json_value(value.clone()));
        }
    }
    Value::Object(out)
}

fn sort_json_value(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            for key in keys {
                if let Some(value) = map.get(&key) {
                    sorted.insert(key, sort_json_value(value.clone()));
                }
            }
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(sort_json_value).collect()),
        other => other,
    }
}

const BUILTIN_DEEP_RESEARCH: &str = r#"export const meta = {
  name: 'deep-research',
  description: 'Deep multi-agent research: parallel sweep from several angles, adversarial claim verification, then synthesis',
  whenToUse: 'Use for broad research questions that need coverage from several angles plus verified claims. Pass the research question as args.',
  phases: [
    { title: 'Sweep', detail: 'parallel researchers, one per angle (sources, implementation, limitations, alternatives)' },
    { title: 'Verify', detail: 'one adversarial refuter per claim, defaulting to refuted when evidence is weak' },
    { title: 'Synthesize', detail: 'merge verified claims into a final report, noting refuted claims and gaps' },
  ],
};
const question = (typeof args === 'undefined' || args == null)
  ? 'No research question was provided in args. State this and stop.'
  : (typeof args === 'string' ? args : json(args));
const CLAIMS_SCHEMA = {
  type: 'object',
  required: ['claims'],
  properties: {
    claims: {
      type: 'array',
      items: {
        type: 'object',
        required: ['claim', 'evidence'],
        properties: {
          claim: { type: 'string' },
          evidence: { type: 'string' },
          confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
        },
      },
    },
  },
};
const VERDICT_SCHEMA = {
  type: 'object',
  required: ['refuted', 'reason'],
  properties: { refuted: { type: 'boolean' }, reason: { type: 'string' } },
};
const ANGLES = [
  'primary sources and official documentation',
  'implementation details, code, and concrete data',
  'known limitations, criticisms, and failure reports',
  'alternatives and comparative context',
];
phase('Sweep');
const sweeps = await parallel(ANGLES.map((angle, i) => () =>
  agent('Research the question below, focusing specifically on ' + angle + '. Return only claims you can support with concrete evidence.\n\nQuestion: ' + question,
    { label: 'sweep:' + i, phase: 'Sweep', schema: CLAIMS_SCHEMA })));
const claims = sweeps.filter(Boolean).reduce((acc, s) => acc.concat(s.claims || []), []);
log('collected ' + claims.length + ' claims from ' + ANGLES.length + ' research angles');
phase('Verify');
const checked = await parallel(claims.map((c, i) => () =>
  agent('Adversarially verify this research claim: try to REFUTE it, and default to refuted=true when the evidence does not hold up.\n\nClaim: ' + json(c) + '\n\nOriginal question: ' + question,
    { label: 'verify:' + i, phase: 'Verify', schema: VERDICT_SCHEMA })
    .then(v => ({ claim: c, verdict: v }))));
const kept = checked.filter(Boolean).filter(r => r.verdict && r.verdict.refuted === false);
const refuted = checked.filter(Boolean).filter(r => r.verdict && r.verdict.refuted === true);
log(kept.length + ' of ' + claims.length + ' claims survived adversarial verification');
phase('Synthesize');
const report = await agent('Write the final research report for the question below using only the verified claims. List refuted claims separately and call out remaining gaps a reader should know about.\n\nQuestion: ' + question + '\n\nVerified claims: ' + json(kept) + '\n\nRefuted claims: ' + json(refuted),
  { label: 'synthesize', phase: 'Synthesize' });
return { report: report, verified: kept.length, refuted: refuted.length };
"#;

const BUILTIN_BUGHUNT_LITE: &str = r#"export const meta = {
  name: 'bughunt-lite',
  description: 'Lightweight parallel bug hunt: one finder per defect dimension, then one adversarial verifier per finding',
  whenToUse: 'Use for a fast code review of a diff, file, or directory. Pass the review target as args.',
  phases: [
    { title: 'Find', detail: 'parallel finders, one per dimension (correctness, edge cases, regressions, resources)' },
    { title: 'Verify', detail: 'one adversarial refuter per finding, reading the actual code' },
  ],
};
const target = (typeof args === 'undefined' || args == null)
  ? 'the uncommitted changes in the current repository'
  : (typeof args === 'string' ? args : json(args));
const FINDINGS_SCHEMA = {
  type: 'object',
  required: ['findings'],
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        required: ['file', 'summary', 'failureScenario'],
        properties: {
          file: { type: 'string' },
          line: { type: 'integer' },
          summary: { type: 'string' },
          failureScenario: { type: 'string' },
        },
      },
    },
  },
};
const VERDICT_SCHEMA = {
  type: 'object',
  required: ['refuted', 'reason'],
  properties: { refuted: { type: 'boolean' }, reason: { type: 'string' } },
};
const DIMENSIONS = [
  { key: 'correctness', focus: 'logic errors, inverted conditions, off-by-one, wrong variable, missing await or error handling' },
  { key: 'edge-cases', focus: 'null or empty inputs, boundary values, unexpected types, concurrent access' },
  { key: 'regressions', focus: 'guards or behavior the old code enforced that the new code no longer establishes' },
  { key: 'resources', focus: 'leaks, unbounded growth, blocking calls on hot paths, missing cleanup' },
];
const results = await pipeline(
  DIMENSIONS,
  d => agent('Review ' + target + '. Hunt only for ' + d.key + ' problems: ' + d.focus + '. Report up to 6 findings, each with file, line, a one-line summary, and a concrete failure scenario.',
    { label: 'find:' + d.key, phase: 'Find', schema: FINDINGS_SCHEMA }),
  (found, d) => parallel(((found && found.findings) || []).map((f, i) => () =>
    agent('Adversarially verify this code-review finding in ' + target + ': try to REFUTE it by reading the actual code, and default to refuted=true when the failure scenario cannot happen.\n\nFinding: ' + json(f),
      { label: 'verify:' + d.key + ':' + i, phase: 'Verify', schema: VERDICT_SCHEMA })
      .then(v => ({ finding: f, verdict: v }))))
);
const confirmed = results.filter(Boolean)
  .reduce((acc, r) => acc.concat(r), [])
  .filter(Boolean)
  .filter(r => r.verdict && r.verdict.refuted === false)
  .map(r => r.finding);
log(confirmed.length + ' findings survived adversarial verification');
return { confirmed: confirmed };
"#;

const BUILTIN_PLAN_HUNTER: &str = r#"export const meta = {
  name: 'plan-hunter',
  description: 'Adversarial implementation plan review: parallel attackers per lens, a judge vote per weakness, deterministic requirement coverage, then a hardened plan',
  whenToUse: 'Use to harden an implementation plan before execution. Pass args as a plan string or { plan, requirements? }.',
  phases: [
    { title: 'Attack', detail: 'parallel attackers, one per lens (feasibility, edge cases, integration, rollback, scope, requirement coverage)' },
    { title: 'Vote', detail: 'two extra judges per weakness; with the attacker that makes a 2-of-3 majority' },
    { title: 'Harden', detail: 'rewrite the plan addressing every confirmed weakness and requirement coverage gaps' },
  ],
};
const plan = (typeof args === 'undefined' || args == null)
  ? 'No plan was provided in args. State this and stop.'
  : (typeof args === 'string' ? args : (args.plan || json(args)));
const rawRequirements = (typeof args === 'object' && args && args.requirements) ? args.requirements : null;
const requirements = rawRequirements
  ? (typeof rawRequirements === 'string' ? rawRequirements : json(rawRequirements))
  : '';
function unique(xs) {
  const seen = new Set();
  const out = [];
  for (const x of xs) {
    const id = String(x || '').trim();
    if (id && !seen.has(id)) { seen.add(id); out.push(id); }
  }
  return out;
}
function idsFromText(text) {
  const out = [];
  const s = String(text || '');
  const ignored = new Set(['Requirement', 'Requirements', 'Acceptance', 'Criteria', 'Plan', 'ID', 'Id']);
  const keep = id => {
    const value = String(id || '').trim();
    if (value && !ignored.has(value)) out.push(value);
  };
  const explicit = /(?:^|[^A-Za-z0-9_])(?:id|requirementId|requirement|req)\s*[:=#]\s*([A-Za-z][A-Za-z0-9_.-]{0,63})/gi;
  let m;
  while ((m = explicit.exec(s))) keep(m[1]);
  const bracketed = /\[([A-Za-z][A-Za-z0-9_.-]{0,63})\]/g;
  while ((m = bracketed.exec(s))) keep(m[1]);
  const lineItem = /(?:^|\n)\s*(?:[-*]\s*)?(?:\d+[.)]\s*)?([A-Za-z][A-Za-z0-9_.-]{0,63})\s*[:)]/g;
  while ((m = lineItem.exec(s))) keep(m[1]);
  const dashed = /\b([A-Z][A-Z0-9_]{1,20}-[A-Z0-9][A-Z0-9_.-]*)\b/g;
  while ((m = dashed.exec(s))) keep(m[1]);
  return out;
}
function extractRequirementIds(input) {
  if (!input) return [];
  const out = [];
  function visit(v, key) {
    if (v == null) return;
    if (typeof v === 'string') {
      if (key && /^[A-Za-z][A-Za-z0-9_.-]{1,63}$/.test(key) && !['text', 'description', 'detail', 'title', 'summary', 'acceptance', 'criteria'].includes(key)) out.push(key);
      out.push(...idsFromText(v));
      return;
    }
    if (Array.isArray(v)) { v.forEach(x => visit(x, null)); return; }
    if (typeof v === 'object') {
      const direct = v.id || v.requirementId || v.requirement_id || v.key || v.name;
      if (typeof direct === 'string') out.push(direct);
      for (const [k, val] of Object.entries(v)) {
        if (/^(id|requirementId|requirement_id|key|name)$/i.test(k)) continue;
        if (/^[A-Za-z][A-Za-z0-9_.-]{1,63}$/.test(k) && !['text', 'description', 'detail', 'title', 'summary', 'acceptance', 'criteria'].includes(k) && (typeof val === 'string' || typeof val === 'object')) out.push(k);
        visit(val, k);
      }
    }
  }
  visit(input, null);
  return unique(out);
}
const requirementIds = extractRequirementIds(rawRequirements);
const WEAKNESS_SCHEMA = {
  type: 'object',
  required: ['weaknesses'],
  properties: {
    weaknesses: {
      type: 'array',
      items: {
        type: 'object',
        required: ['title', 'detail'],
        properties: {
          title: { type: 'string' },
          detail: { type: 'string' },
          severity: { type: 'string', enum: ['high', 'medium', 'low'] },
          requirementId: { type: 'string' },
        },
      },
    },
  },
};
const COVERAGE_SCHEMA = {
  type: 'object',
  required: ['rows'],
  properties: {
    rows: {
      type: 'array',
      items: {
        type: 'object',
        required: ['requirementId', 'status', 'evidence'],
        properties: {
          requirementId: { type: 'string' },
          status: { type: 'string', enum: ['covered', 'partial', 'missing', 'unknown'] },
          evidence: { type: 'string' },
        },
      },
    },
  },
};
const VOTE_SCHEMA = {
  type: 'object',
  required: ['real', 'reason'],
  properties: { real: { type: 'boolean' }, reason: { type: 'string' } },
};
const LENSES = [
  'technical feasibility and hidden complexity',
  'edge cases and failure modes the plan ignores',
  'integration with existing code and migration order',
  'rollback safety and partial-failure recovery',
  'scope creep and missing acceptance criteria',
  'requirement coverage: check every requirement ID is substantively covered, not merely tagged with [COVERS:ID]',
];
phase('Attack');
const reviewPacket = requirements ? ('Requirements / acceptance ledger:\n' + requirements + '\n\nPlan:\n' + plan) : ('Plan:\n' + plan);
let requirementCoverage = { providedIds: requirementIds, assessedIds: [], missingAssessments: requirementIds, covered: [], partial: [], missing: [], unknown: [], rows: [] };
let coverageWeaknesses = [];
if (requirementIds.length > 0) {
  const coverage = await agent('Assess requirement coverage for the implementation plan. You MUST return exactly one rows[] entry for every provided requirement ID, using only these IDs: ' + json(requirementIds) + '. Status meanings: covered=substantively addressed by the plan, partial=some work but gaps remain, missing=not addressed, unknown=insufficient evidence.\n\n' + reviewPacket,
    { label: 'coverage:requirements', phase: 'Attack', schema: COVERAGE_SCHEMA });
  const byId = new Map();
  for (const row of ((coverage && coverage.rows) || [])) {
    const id = String(row.requirementId || '').trim();
    if (requirementIds.includes(id) && !byId.has(id)) byId.set(id, row);
  }
  const rows = [];
  for (const id of requirementIds) {
    const row = byId.get(id) || { requirementId: id, status: 'unknown', evidence: 'coverage assessor omitted this requirement ID' };
    const status = ['covered', 'partial', 'missing', 'unknown'].includes(row.status) ? row.status : 'unknown';
    const normalized = { requirementId: id, status, evidence: String(row.evidence || '') };
    rows.push(normalized);
  }
  requirementCoverage = {
    providedIds: requirementIds,
    assessedIds: rows.filter(r => byId.has(r.requirementId)).map(r => r.requirementId),
    missingAssessments: requirementIds.filter(id => !byId.has(id)),
    covered: rows.filter(r => r.status === 'covered').map(r => r.requirementId),
    partial: rows.filter(r => r.status === 'partial').map(r => r.requirementId),
    missing: rows.filter(r => r.status === 'missing').map(r => r.requirementId),
    unknown: rows.filter(r => r.status === 'unknown').map(r => r.requirementId),
    rows,
  };
  coverageWeaknesses = rows
    .filter(r => r.status !== 'covered')
    .map(r => ({
      title: 'Requirement coverage gap: ' + r.requirementId,
      detail: (r.status === 'unknown' && requirementCoverage.missingAssessments.includes(r.requirementId))
        ? 'Requirement ID was not assessed by the coverage agent: ' + r.evidence
        : 'Requirement coverage status is ' + r.status + ': ' + r.evidence,
      severity: r.status === 'missing' || requirementCoverage.missingAssessments.includes(r.requirementId) ? 'high' : 'medium',
      requirementId: r.requirementId,
    }));
  log('requirement coverage assessed ' + requirementCoverage.assessedIds.length + '/' + requirementIds.length + ' provided IDs; gaps=' + coverageWeaknesses.length);
}
const attacks = await parallel(LENSES.map((lens, i) => () =>
  agent('Attack the implementation plan below through one lens only: ' + lens + '. Report concrete weaknesses, not style preferences. If this is the requirement coverage lens, name the relevant requirementId for each coverage weakness.\n\n' + reviewPacket,
    { label: 'attack:' + i, phase: 'Attack', schema: WEAKNESS_SCHEMA })));
const weaknesses = attacks.filter(Boolean).reduce((acc, a) => acc.concat(a.weaknesses || []), []);
log('collected ' + weaknesses.length + ' candidate weaknesses across ' + LENSES.length + ' lenses');
phase('Vote');
const voted = await parallel(weaknesses.map((w, i) => () =>
  parallel([0, 1].map(v => () =>
    agent('Judge whether this weakness in the plan is real and worth addressing, or a false alarm. Be skeptical.\n\nWeakness: ' + json(w) + '\n\n' + reviewPacket,
      { label: 'vote:' + i + ':' + v, phase: 'Vote', schema: VOTE_SCHEMA })))
    .then(votes => ({
      weakness: w,
      real: votes.filter(Boolean).filter(x => x.real === true).length >= 1,
    }))));
const votedConfirmed = voted.filter(Boolean).filter(v => v.real).map(v => v.weakness);
const confirmedWeaknesses = votedConfirmed.concat(coverageWeaknesses);
log(confirmedWeaknesses.length + ' weaknesses confirmed, including ' + coverageWeaknesses.length + ' deterministic requirement coverage gaps');
phase('Harden');
const hardened = await agent('Rewrite the implementation plan to address every confirmed weakness below. Keep what already works, preserve/strengthen substantive requirement coverage, and call out any weakness you intentionally accept instead of fixing. The structured requirementCoverage summary is authoritative: every non-covered or unassessed requirement ID must be addressed.\n\n' + reviewPacket + '\n\nRequirement coverage summary: ' + json(requirementCoverage) + '\n\nConfirmed weaknesses: ' + json(confirmedWeaknesses),
  { label: 'harden', phase: 'Harden' });
return { hardenedPlan: hardened, confirmed: confirmedWeaknesses.length, rejected: weaknesses.length - votedConfirmed.length, requirementCoverage: requirementCoverage };
"#;

fn builtin_workflows() -> Vec<WorkflowResolvedScript> {
    [
        BUILTIN_DEEP_RESEARCH,
        BUILTIN_BUGHUNT_LITE,
        BUILTIN_PLAN_HUNTER,
    ]
    .into_iter()
    .map(|script| {
        let parsed = parse_workflow_meta(script).expect("built-in workflow parses");
        WorkflowResolvedScript {
            script: script.to_string(),
            meta: parsed.meta,
            source: "built-in".into(),
            resolved_script_path: None,
        }
    })
    .collect()
}

fn persist_workflow_script(
    workflows_dir: &Path,
    workflow_name: &str,
    run_id: &str,
    script: &str,
) -> Result<PathBuf, String> {
    fs::create_dir_all(workflows_dir).map_err(|err| {
        format!(
            "Failed to create workflow script dir {}: {err}",
            workflows_dir.display()
        )
    })?;
    let safe_name = sanitize_filename(workflow_name);
    let path = workflows_dir.join(format!("{safe_name}_{run_id}.js"));
    fs::write(&path, script).map_err(|err| {
        format!(
            "Failed to persist workflow script {}: {err}",
            path.display()
        )
    })?;
    Ok(path)
}

fn sanitize_filename(value: &str) -> String {
    let out: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "inline".into()
    } else {
        out
    }
}

fn new_workflow_run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("wf_{nanos:x}_{n:x}")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn push_review_line(out: &mut String, label: &str, value: &str) {
    out.push_str("- ");
    out.push_str(label);
    out.push_str(": ");
    out.push_str(&single_line(value));
    out.push('\n');
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_static_call_counts(calls: &[WorkflowReviewCall]) -> String {
    if calls.is_empty() {
        return "(none)".into();
    }
    ["phase", "agent", "parallel", "pipeline", "workflow", "log"]
        .into_iter()
        .filter_map(|kind| {
            let count = calls.iter().filter(|call| call.kind == kind).count();
            (count > 0).then(|| format!("{kind}={count}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn summarize_review_arg(value: &Value) -> String {
    truncate_chars(
        &serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
        240,
    )
}

fn script_excerpt(script: &str) -> String {
    let mut excerpt = String::new();
    let truncated = truncate_chars(script.trim(), WORKFLOW_REVIEW_SCRIPT_EXCERPT_CHARS);
    for (idx, line) in truncated.lines().enumerate() {
        excerpt.push_str(&format!("{:>3}| {}\n", idx + 1, line));
    }
    if excerpt.is_empty() {
        excerpt.push_str("  1| \n");
    }
    excerpt
}

fn escape_mermaid_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}

fn summarize_workflow_static_calls(script: &str) -> Vec<WorkflowReviewCall> {
    let mut calls = Vec::new();
    let mut index = 0usize;
    while index < script.len() {
        if starts_with_at(script, index, "//") {
            index = skip_line_comment(script, index);
            continue;
        }
        if starts_with_at(script, index, "/*") {
            index = skip_block_comment(script, index);
            continue;
        }
        let Some(ch) = char_at(script, index) else {
            break;
        };
        if matches!(ch, '\'' | '"' | '`') {
            index = skip_js_string(script, index, ch);
            continue;
        }
        if is_js_ident_start(ch) {
            let start = index;
            let end = read_js_ident_end(script, index);
            let ident = &script[start..end];
            index = end;
            if is_workflow_call_name(ident) && !previous_non_ws_is(script, start, '.') {
                let open = skip_ws(script, end);
                if starts_with_at(script, open, "(") {
                    if let Some(close) = find_matching_paren_review(script, open) {
                        let args = &script[open + 1..close];
                        calls.push(summarize_static_workflow_call(
                            ident,
                            byte_line_number(script, start),
                            args,
                        ));
                    }
                }
            }
            continue;
        }
        index += ch.len_utf8();
    }
    calls
}

fn summarize_static_workflow_call(kind: &str, line: usize, args: &str) -> WorkflowReviewCall {
    if kind == "agent" {
        return summarize_agent_review_call(line, args);
    }
    let (summary, phase, has_schema) = match kind {
        "phase" => (
            format!(
                "phase {}",
                first_top_level_arg(args)
                    .map(describe_js_expr)
                    .unwrap_or_else(|| "(unknown)".into())
            ),
            None,
            false,
        ),
        "parallel" => (
            format!(
                "parallel with {} branch(es)",
                first_top_level_arg(args)
                    .and_then(top_level_array_item_count)
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "unknown".into())
            ),
            None,
            false,
        ),
        "pipeline" => (
            format!(
                "pipeline with {} step(s)",
                first_top_level_arg(args)
                    .and_then(top_level_array_item_count)
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "unknown".into())
            ),
            None,
            false,
        ),
        "workflow" => (
            format!(
                "workflow {}",
                first_top_level_arg(args)
                    .map(describe_js_expr)
                    .unwrap_or_else(|| "(unknown)".into())
            ),
            None,
            false,
        ),
        "log" => (
            format!(
                "log {}",
                first_top_level_arg(args)
                    .map(describe_js_expr)
                    .unwrap_or_else(|| "(unknown)".into())
            ),
            None,
            false,
        ),
        _ => (kind.to_string(), None, false),
    };
    WorkflowReviewCall {
        kind: kind.into(),
        line,
        summary,
        phase,
        has_schema,
        agent: None,
    }
}

fn summarize_agent_review_call(line: usize, args: &str) -> WorkflowReviewCall {
    let parts = split_top_level(args);
    let first = parts.first().copied().unwrap_or("");
    let prompt_expr = extract_js_property_value(first, &["prompt"]);
    let prompt = prompt_expr
        .as_deref()
        .map(describe_js_expr)
        .unwrap_or_else(|| describe_js_expr(first));
    let options = summarize_agent_options(&parts);
    let summary = if options.summary.is_empty() {
        format!("prompt {prompt}")
    } else {
        format!("prompt {prompt} ({})", options.summary)
    };
    let mut details = options.details;
    details.prompt = prompt_expr
        .as_deref()
        .or(Some(first))
        .map(|expr| describe_js_option_value_bounded(expr, WORKFLOW_REVIEW_PROMPT_CHARS))
        .filter(|value| !value.trim().is_empty());
    details.phase_title = options.phase.clone();
    details.has_schema = options.has_schema;
    WorkflowReviewCall {
        kind: "agent".into(),
        line,
        summary,
        phase: options.phase,
        has_schema: options.has_schema,
        agent: (!details.is_empty()).then_some(details),
    }
}

struct AgentOptionSummary {
    summary: String,
    phase: Option<String>,
    has_schema: bool,
    details: rebon_tools_core::WorkflowAgentNodeMeta,
}

fn summarize_agent_options(parts: &[&str]) -> AgentOptionSummary {
    let option_expr = parts
        .first()
        .copied()
        .filter(|first| first.trim_start().starts_with('{'))
        .or_else(|| parts.get(1).copied())
        .unwrap_or("");
    if option_expr.trim().is_empty() {
        return AgentOptionSummary {
            summary: String::new(),
            phase: None,
            has_schema: false,
            details: rebon_tools_core::WorkflowAgentNodeMeta::default(),
        };
    }
    let phase = extract_js_property_value(option_expr, &["phase"])
        .map(describe_js_option_value)
        .filter(|value| !value.trim().is_empty());
    let schema_expr = extract_js_property_value(option_expr, &["schema"]);
    let has_schema = schema_expr.is_some();
    // A bare identifier (`schema: REVIEW_SCHEMA`) carries a reviewable name;
    // an inline object literal stays anonymous. The last property of an
    // object literal drags the enclosing closers along — strip them first.
    let schema_name = schema_expr
        .map(|expr| {
            expr.trim()
                .trim_end_matches(|ch: char| ch.is_whitespace() || matches!(ch, '}' | ')' | ';'))
        })
        .filter(|expr| {
            !expr.is_empty()
                && expr.chars().enumerate().all(|(index, ch)| {
                    ch == '_'
                        || ch == '$'
                        || if index == 0 {
                            ch.is_ascii_alphabetic()
                        } else {
                            ch.is_ascii_alphanumeric()
                        }
                })
        })
        .map(str::to_string);
    let structured = |keys: &[&str]| {
        extract_js_property_value(option_expr, keys)
            .map(describe_js_option_value)
            .filter(|value| !value.trim().is_empty())
    };
    let details = rebon_tools_core::WorkflowAgentNodeMeta {
        label: structured(&["label"]),
        prompt: None,
        model: structured(&["model"]),
        provider: structured(&["provider"]),
        model_profile: structured(&["modelProfile", "model_profile"]),
        isolation: structured(&["isolation"]),
        agent_type: structured(&["agentType", "agent_type"]),
        phase_title: None,
        has_schema,
        schema_name,
    };
    let summary = [
        ("model", &["model"][..]),
        ("provider", &["provider"][..]),
        ("agentType", &["agentType", "agent_type"][..]),
        ("cwd", &["cwd"][..]),
        ("maxIterations", &["maxIterations", "max_iterations"][..]),
    ]
    .into_iter()
    .filter_map(|(label, keys)| {
        extract_js_property_value(option_expr, keys)
            .map(describe_js_option_value)
            .map(|value| format!("{label}={value}"))
    })
    .chain(phase.as_ref().map(|value| format!("phase={value}")))
    .chain(has_schema.then(|| "schema=present".to_string()))
    .collect::<Vec<_>>()
    .join(", ");
    AgentOptionSummary {
        summary,
        phase,
        has_schema,
        details,
    }
}

fn describe_js_expr(expr: &str) -> String {
    if let Some(value) = parse_js_string_literal(expr.trim_start()) {
        format!(
            "\"{}\"",
            truncate_chars(&single_line(&value), WORKFLOW_REVIEW_PROMPT_CHARS)
        )
    } else {
        truncate_chars(&single_line(expr.trim()), WORKFLOW_REVIEW_PROMPT_CHARS)
    }
}

fn describe_js_option_value(expr: &str) -> String {
    describe_js_option_value_bounded(expr, 80)
}

fn describe_js_option_value_bounded(expr: &str, max_chars: usize) -> String {
    if let Some(value) = parse_js_string_literal(expr.trim_start()) {
        truncate_chars(&single_line(&value), max_chars)
    } else {
        truncate_chars(&single_line(expr.trim()), max_chars)
    }
}

fn extract_js_property_value<'a>(expr: &'a str, keys: &[&str]) -> Option<&'a str> {
    for key in keys {
        for token in [key.to_string(), format!("'{key}'"), format!("\"{key}\"")] {
            let mut search_at = 0usize;
            while let Some(relative) = expr.get(search_at..)?.find(&token) {
                let pos = search_at + relative;
                if !property_key_boundary(expr, pos, token.len()) {
                    search_at = pos + token.len();
                    continue;
                }
                let colon = skip_ws(expr, pos + token.len());
                if !starts_with_at(expr, colon, ":") {
                    search_at = pos + token.len();
                    continue;
                }
                let value_start = skip_ws(expr, colon + 1);
                return Some(first_top_level_value(&expr[value_start..]));
            }
        }
    }
    None
}

fn property_key_boundary(expr: &str, start: usize, len: usize) -> bool {
    let before = expr[..start].chars().rev().find(|ch| !ch.is_whitespace());
    let after = expr[start + len..].chars().find(|ch| !ch.is_whitespace());
    !matches!(before, Some(ch) if is_js_ident_continue(ch)) && matches!(after, Some(':') | None)
}

fn first_top_level_value(expr: &str) -> &str {
    split_top_level(expr)
        .first()
        .copied()
        .unwrap_or(expr)
        .trim()
}

fn first_top_level_arg(args: &str) -> Option<&str> {
    split_top_level(args)
        .into_iter()
        .map(str::trim)
        .find(|part| !part.is_empty())
}

fn top_level_array_item_count(expr: &str) -> Option<usize> {
    let trimmed = expr.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    if inner.trim().is_empty() {
        Some(0)
    } else {
        Some(
            split_top_level(inner)
                .into_iter()
                .filter(|part| !part.trim().is_empty())
                .count(),
        )
    }
}

fn split_top_level(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut brace = 0usize;
    while index < input.len() {
        if starts_with_at(input, index, "//") {
            index = skip_line_comment(input, index);
            continue;
        }
        if starts_with_at(input, index, "/*") {
            index = skip_block_comment(input, index);
            continue;
        }
        let Some(ch) = char_at(input, index) else {
            break;
        };
        if matches!(ch, '\'' | '"' | '`') {
            index = skip_js_string(input, index, ch);
            continue;
        }
        match ch {
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            '{' => brace += 1,
            '}' => brace = brace.saturating_sub(1),
            ',' if paren == 0 && bracket == 0 && brace == 0 => {
                parts.push(input[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
        index += ch.len_utf8();
    }
    parts.push(input[start..].trim());
    parts
}

fn parse_js_string_literal(input: &str) -> Option<String> {
    let quote = input.chars().next()?;
    if !matches!(quote, '\'' | '"' | '`') {
        return None;
    }
    let mut out = String::new();
    let mut escaped = false;
    for ch in input[quote.len_utf8()..].chars() {
        if escaped {
            // Decode the common escapes instead of dropping the backslash —
            // `\n` shown as a literal "n" corrupts every multi-line prompt.
            out.push(match ch {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '0' => '\0',
                other => other,
            });
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return Some(out);
        }
        out.push(ch);
    }
    None
}

fn is_workflow_call_name(ident: &str) -> bool {
    matches!(
        ident,
        "phase" | "agent" | "parallel" | "pipeline" | "workflow" | "log"
    )
}

fn starts_with_at(input: &str, index: usize, needle: &str) -> bool {
    input
        .get(index..)
        .is_some_and(|tail| tail.starts_with(needle))
}

fn char_at(input: &str, index: usize) -> Option<char> {
    input.get(index..)?.chars().next()
}

fn skip_ws(input: &str, mut index: usize) -> usize {
    while let Some(ch) = char_at(input, index) {
        if !ch.is_whitespace() {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

fn skip_line_comment(input: &str, index: usize) -> usize {
    input
        .get(index..)
        .and_then(|tail| tail.find('\n').map(|offset| index + offset + 1))
        .unwrap_or(input.len())
}

fn skip_block_comment(input: &str, index: usize) -> usize {
    input
        .get(index + 2..)
        .and_then(|tail| tail.find("*/").map(|offset| index + 2 + offset + 2))
        .unwrap_or(input.len())
}

fn skip_js_string(input: &str, index: usize, quote: char) -> usize {
    let mut cursor = index + quote.len_utf8();
    let mut escaped = false;
    while cursor < input.len() {
        let Some(ch) = char_at(input, cursor) else {
            break;
        };
        cursor += ch.len_utf8();
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return cursor;
        }
    }
    input.len()
}

fn is_js_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || matches!(ch, '_' | '$')
}

fn is_js_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$')
}

fn read_js_ident_end(input: &str, mut index: usize) -> usize {
    while let Some(ch) = char_at(input, index) {
        if !is_js_ident_continue(ch) {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

fn previous_non_ws_is(input: &str, index: usize, needle: char) -> bool {
    input[..index].chars().rev().find(|ch| !ch.is_whitespace()) == Some(needle)
}

fn find_matching_paren_review(input: &str, open: usize) -> Option<usize> {
    let mut index = open;
    let mut depth = 0usize;
    while index < input.len() {
        if starts_with_at(input, index, "//") {
            index = skip_line_comment(input, index);
            continue;
        }
        if starts_with_at(input, index, "/*") {
            index = skip_block_comment(input, index);
            continue;
        }
        let ch = char_at(input, index)?;
        if matches!(ch, '\'' | '"' | '`') {
            index = skip_js_string(input, index, ch);
            continue;
        }
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += ch.len_utf8();
    }
    None
}

fn byte_line_number(input: &str, index: usize) -> usize {
    input[..index].bytes().filter(|byte| *byte == b'\n').count() + 1
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use rebon_tool::{SubAgentResult, SubAgentSpawner};
    use tempfile::TempDir;

    use super::*;
    use rebon_plugin_tasks::runtime::TaskStatus;
    use rebon_types::MAX_NESTED_WORKFLOW_DEPTH;

    #[test]
    fn model_visible_workflow_failure_is_bounded() {
        let visible = model_visible_workflow_failure(&"boom".repeat(10_000), 3, "wf_bounded");

        assert!(visible.starts_with("Workflow failed after 3 agent call(s)."));
        assert!(visible.contains("resumeFromRunId: \"wf_bounded\""));
        assert!(visible.ends_with('…'));
        assert!(serde_json::to_string(&visible).unwrap().chars().count() <= 2_050);
    }

    #[test]
    fn ultraplan_restricts_script_path_to_workflow_directories() {
        let temp = TempDir::new().expect("tempdir");
        let project_workflows = temp.path().join("project").join(".rebon").join("workflows");
        std::fs::create_dir_all(&project_workflows).unwrap();
        let allowed = project_workflows.join("allowed.js");
        std::fs::write(
            &allowed,
            "export const meta = { name: 'allowed', description: 'Allowed', phases: [] };",
        )
        .unwrap();
        let outside = temp.path().join("outside.js");
        std::fs::write(
            &outside,
            "export const meta = { name: 'outside', description: 'Outside', phases: [] };",
        )
        .unwrap();
        let launcher = WorkflowRegistryLauncher::new_session_scoped(
            TaskRegistry::default(),
            temp.path().join("project"),
            temp.path().join("config"),
            temp.path().join("session"),
        );
        let context = ToolContext::new().with_execution_policy(
            rebon_types::ExecutionPolicy::ultraplan(rebon_types::UltraplanContext::planning_turn(
                "run",
                "plan",
                rebon_types::PolicyMode::Enforce,
            )),
        );

        let allowed_spec = WorkflowLaunchSpec {
            script_path: Some(allowed.display().to_string()),
            ..Default::default()
        };
        assert!(launcher
            .resolve_script_for_context(&allowed_spec, Some(&context))
            .is_ok());

        let outside_spec = WorkflowLaunchSpec {
            script_path: Some(outside.display().to_string()),
            ..Default::default()
        };
        let err = launcher
            .resolve_script_for_context(&outside_spec, Some(&context))
            .unwrap_err();
        assert!(err.contains("restricted during /ultraplan"));
    }

    #[test]
    fn workflow_child_cwd_must_stay_within_parent_scope() {
        let temp = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let context = ToolContext::new().with_cwd(temp.path().display().to_string());
        let mut spec = SubAgentSpec::new("work");
        spec.cwd = Some(outside.path().display().to_string());

        let err = enforce_workflow_child_path_scope(&context, &mut spec).unwrap_err();
        assert!(err.contains("must stay within parent authorized roots"));
    }

    #[test]
    fn workflow_child_relative_cwd_is_resolved_inside_parent_scope() {
        let temp = TempDir::new().expect("tempdir");
        let context = ToolContext::new().with_cwd(temp.path().display().to_string());
        let mut spec = SubAgentSpec::new("work");
        spec.cwd = Some("child".into());

        enforce_workflow_child_path_scope(&context, &mut spec).unwrap();
        assert_eq!(
            spec.cwd,
            Some(temp.path().join("child").display().to_string())
        );
    }

    #[test]
    fn workflow_child_inherits_explicit_roots_without_context_cwd() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("authorized");
        std::fs::create_dir_all(&root).unwrap();
        let context = ToolContext::new().with_path_scope_roots([root.clone()]);
        let mut spec = SubAgentSpec::new("work");

        enforce_workflow_child_path_scope(&context, &mut spec).unwrap();

        assert_eq!(spec.cwd, None);
        assert_eq!(spec.allowed_roots, vec![root]);
    }

    #[test]
    fn workflow_child_rejects_external_cwd_without_context_cwd() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("authorized");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let context = ToolContext::new().with_path_scope_roots([root]);
        let mut spec = SubAgentSpec::new("work");
        spec.cwd = Some(outside.display().to_string());

        let err = enforce_workflow_child_path_scope(&context, &mut spec).unwrap_err();

        assert!(err.contains("must stay within parent authorized roots"));
    }

    #[test]
    fn workflow_child_allows_additional_working_directory() {
        let parent = TempDir::new().expect("parent tempdir");
        let additional = TempDir::new().expect("additional tempdir");
        let context = ToolContext::new()
            .with_cwd(parent.path().display().to_string())
            .with_additional_working_directories([additional.path().display().to_string()]);
        let mut spec = SubAgentSpec::new("work");
        spec.cwd = Some(additional.path().display().to_string());

        enforce_workflow_child_path_scope(&context, &mut spec).unwrap();

        assert_eq!(spec.cwd.as_deref(), additional.path().to_str());
        assert_eq!(spec.allowed_roots, vec![additional.path().to_path_buf()]);
    }

    #[test]
    fn workflow_child_broader_allowed_root_is_narrowed_to_parent_scope() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("authorized");
        std::fs::create_dir_all(&root).unwrap();
        let context = ToolContext::new().with_path_scope_roots([root.clone()]);
        let mut spec = SubAgentSpec::new("work");
        spec.allowed_roots = vec![temp.path().to_path_buf()];

        enforce_workflow_child_path_scope(&context, &mut spec).unwrap();

        assert_eq!(spec.allowed_roots, vec![root]);
    }

    fn structured_schema() -> Value {
        json!({
            "type": "object",
            "required": ["answer", "ok"],
            "properties": {
                "answer": { "type": "string" },
                "ok": { "type": "boolean" }
            }
        })
    }

    fn completed_result(final_text: &str, calls: Option<Vec<Value>>) -> SubAgentResult {
        SubAgentResult {
            final_text: final_text.to_string(),
            status: "completed".into(),
            tool_call_count: calls.as_ref().map_or(0, Vec::len),
            stop_reason: None,
            error: None,
            output_file: None,
            duration_ms: None,
            agent_id: None,
            agent_type: None,
            provider: None,
            model: None,
            sub_agent_tool_calls: calls,
            read_file_count: None,
            total_tokens: None,
            output_tokens: None,
            usage: None,
            diagnostics: None,
            git: None,
        }
    }

    #[test]
    fn agent_return_data_without_schema_returns_final_text_string() {
        let result = completed_result("plain result", None);
        let data = agent_return_data(&result, None, "label", Some("phase")).unwrap();
        assert_eq!(data.data, Value::String("plain result".into()));
    }

    #[test]
    fn agent_return_data_rejects_failed_unstructured_agent_without_leaking_final_text() {
        let mut result = completed_result("验证完成：全部实现已经通过", None);
        result.status = "failed".into();
        result.error = Some("worker crashed".into());

        let error = agent_return_data(&result, None, "label", Some("phase")).unwrap_err();

        assert!(error.contains("Workflow agent failed"));
        assert!(error.contains("AgentExecutionFailed"));
        assert!(error.contains("status=failed"));
        assert!(!error.contains("验证完成"));
        assert_eq!(
            agent_result_diagnostics(&result, "none", Some(&error))["final_text_excerpt"],
            "验证完成：全部实现已经通过"
        );
    }

    #[test]
    fn agent_return_data_schema_uses_last_successful_structured_output() {
        let result = completed_result(
            r#"{"answer":"fallback","ok":true}"#,
            Some(vec![
                json!({"name": STRUCTURED_OUTPUT_TOOL_NAME, "ok": true, "input": {"answer":"old", "ok": true}}),
                json!({"name": "Read", "ok": true}),
                json!({"name": STRUCTURED_OUTPUT_TOOL_NAME, "ok": true, "input": {"answer":"tool", "ok": true}}),
            ]),
        );
        let data =
            agent_return_data(&result, Some(&structured_schema()), "label", Some("phase")).unwrap();
        assert_eq!(data.data["answer"], "tool");
        assert_eq!(
            data.agent_result["final_data_source"],
            "structured_output_tool"
        );
    }

    #[test]
    fn agent_return_data_schema_fallbacks_validate() {
        for (text, expected, source) in [
            (r#"{"answer":"pure","ok":true}"#, "pure", "pure_json"),
            (
                "Done:\n```json\n{\"answer\":\"fenced\",\"ok\":true}\n```",
                "fenced",
                "fenced_json",
            ),
            (
                "Earlier {\"ignore\":true} final {\"answer\":\"last\",\"ok\":true}",
                "last",
                "last_json_object",
            ),
        ] {
            let result = completed_result(text, None);
            let data =
                agent_return_data(&result, Some(&structured_schema()), "label", Some("phase"))
                    .unwrap();
            assert_eq!(data.data["answer"], expected);
            assert_eq!(data.structured_output["fallback_source"], source);
        }
    }

    #[test]
    fn agent_return_data_schema_fallback_json_must_validate_schema() {
        let result = completed_result(r#"{"answer":"bad"}"#, None);
        let error = agent_return_data(&result, Some(&structured_schema()), "label", Some("phase"))
            .unwrap_err();
        assert!(error.contains("StructuredOutputFallbackSchemaError"));
        assert!(error.contains("label=label"));
        assert!(error.contains("phase=phase"));
    }

    #[test]
    fn agent_return_data_schema_unparseable_final_text_errors_not_null() {
        let result = completed_result("I completed the task but forgot the tool", None);
        let error = agent_return_data(&result, Some(&structured_schema()), "label", Some("phase"))
            .unwrap_err();
        assert!(error.contains("StructuredOutputFallbackParseError"));
        assert!(error.contains("StructuredOutput tool calls=0"));
    }

    #[test]
    fn agent_return_data_schema_allows_valid_fallback_when_structured_output_call_failed() {
        let result = completed_result(
            r#"{"answer":"fallback","ok":true}"#,
            Some(vec![
                json!({"name": STRUCTURED_OUTPUT_TOOL_NAME, "ok": false, "input": {"answer":"ignored", "ok": true}}),
            ]),
        );
        let data =
            agent_return_data(&result, Some(&structured_schema()), "label", Some("phase")).unwrap();
        assert_eq!(data.data["answer"], "fallback");
        assert_eq!(data.structured_output["tool_call_count"], 1);
        assert_eq!(data.structured_output["successful_tool_call_count"], 0);
    }

    #[test]
    fn agent_return_data_schema_rejects_invalid_structured_output_tool_call() {
        let result = completed_result(
            r#"{"answer":"fallback","ok":true}"#,
            Some(vec![
                json!({"name": STRUCTURED_OUTPUT_TOOL_NAME, "ok": true, "input": {"answer":"bad"}}),
            ]),
        );
        let error = agent_return_data(&result, Some(&structured_schema()), "label", Some("phase"))
            .unwrap_err();
        assert!(error.contains("StructuredOutputToolCallSchemaError"));
    }

    #[test]
    fn agent_return_data_schema_rejects_null_structured_output_for_object_schema() {
        let result = completed_result(
            "",
            Some(vec![
                json!({"name": STRUCTURED_OUTPUT_TOOL_NAME, "ok": true, "input": Value::Null}),
            ]),
        );
        let error = agent_return_data(
            &result,
            Some(&json!({"type":"object"})),
            "label",
            Some("phase"),
        )
        .unwrap_err();
        assert!(error.contains("StructuredOutputToolCallSchemaError"));
    }

    #[test]
    fn agent_return_data_schema_non_completed_status_errors_not_null() {
        for status in ["failed", "cancelled", "timeout"] {
            let mut result = completed_result("验证完成：所有改动已通过", None);
            result.status = status.into();
            let error =
                agent_return_data(&result, Some(&structured_schema()), "label", Some("phase"))
                    .unwrap_err();
            assert!(error.contains(status));
            assert!(error.contains("MissingStructuredOutput"));
            assert!(error.contains("Workflow agent failed"));
            assert!(!error.contains("验证完成"));
        }
    }

    struct TestSpawner;

    #[async_trait]
    impl SubAgentSpawner for TestSpawner {
        async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentResult, String> {
            let agent_id = spec
                .metadata
                .get("agent_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(SubAgentResult {
                final_text: format!("done: {}", spec.prompt),
                status: "completed".into(),
                tool_call_count: 2,
                stop_reason: None,
                error: None,
                output_file: None,
                duration_ms: Some(7),
                agent_id,
                agent_type: None,
                provider: spec.provider,
                model: spec.model,
                sub_agent_tool_calls: spec
                    .execution_policy
                    .as_ref()
                    .is_some_and(|policy| {
                        policy
                            .eager_promotions
                            .iter()
                            .any(|tool| tool == STRUCTURED_OUTPUT_TOOL_NAME)
                    })
                    .then(|| {
                        vec![json!({
                            "name": STRUCTURED_OUTPUT_TOOL_NAME,
                            "input": { "answer": spec.prompt, "ok": true },
                            "ok": true,
                        })]
                    }),
                read_file_count: None,
                // Display total deliberately differs from output so the
                // budget tests prove charging uses output tokens only.
                total_tokens: Some(99),
                output_tokens: Some(11),
                usage: None,
                diagnostics: None,
                git: None,
            })
        }
    }

    struct FailedNarrativeSpawner;

    #[async_trait]
    impl SubAgentSpawner for FailedNarrativeSpawner {
        async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentResult, String> {
            let mut result = TestSpawner.spawn(spec).await?;
            result.status = "failed".into();
            result.error = Some("worker crashed before delivery".into());
            result.final_text = "验证完成：所有实现与测试均已通过".into();
            result.sub_agent_tool_calls = None;
            Ok(result)
        }
    }

    struct PromptAvailabilitySpawner {
        unavailable: Arc<AtomicBool>,
    }

    #[async_trait]
    impl SubAgentSpawner for PromptAvailabilitySpawner {
        async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentResult, String> {
            self.unavailable
                .store(spec.permission_prompts_unavailable, Ordering::SeqCst);
            TestSpawner.spawn(spec).await
        }
    }

    struct PlanHunterCoverageSpawner {
        omit_second_requirement: bool,
        second_status: &'static str,
    }

    #[async_trait]
    impl SubAgentSpawner for PlanHunterCoverageSpawner {
        async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentResult, String> {
            let structured = spec.execution_policy.as_ref().is_some_and(|policy| {
                policy
                    .eager_promotions
                    .iter()
                    .any(|tool| tool == STRUCTURED_OUTPUT_TOOL_NAME)
            });
            let input = if spec.prompt.starts_with("Assess requirement coverage") {
                let mut rows = vec![json!({
                    "requirementId": "REQ-A",
                    "status": "covered",
                    "evidence": "plan explicitly handles A"
                })];
                if !self.omit_second_requirement {
                    rows.push(json!({
                        "requirementId": "REQ-B",
                        "status": self.second_status,
                        "evidence": "controlled test coverage verdict for B"
                    }));
                }
                json!({ "rows": rows })
            } else if spec.prompt.starts_with("Attack the implementation plan") {
                json!({ "weaknesses": [] })
            } else if spec.prompt.starts_with("Judge whether this weakness") {
                json!({ "real": false, "reason": "no weakness" })
            } else {
                Value::Null
            };

            Ok(SubAgentResult {
                final_text: if spec.prompt.starts_with("Rewrite the implementation plan") {
                    format!("hardened prompt: {}", spec.prompt)
                } else {
                    String::new()
                },
                status: "completed".into(),
                tool_call_count: usize::from(structured),
                stop_reason: None,
                error: None,
                output_file: None,
                duration_ms: Some(1),
                agent_id: Some("plan-hunter-test".into()),
                agent_type: None,
                provider: spec.provider,
                model: spec.model,
                sub_agent_tool_calls: structured.then(|| {
                    vec![json!({
                        "name": STRUCTURED_OUTPUT_TOOL_NAME,
                        "input": input,
                        "ok": true,
                    })]
                }),
                read_file_count: None,
                total_tokens: Some(1),
                output_tokens: Some(1),
                usage: None,
                diagnostics: None,
                git: None,
            })
        }
    }

    struct FinalTextJsonSpawner;

    #[async_trait]
    impl SubAgentSpawner for FinalTextJsonSpawner {
        async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentResult, String> {
            Ok(SubAgentResult {
                final_text: format!(r#"{{"answer":"{}","ok":true}}"#, spec.prompt),
                status: "completed".into(),
                tool_call_count: 0,
                stop_reason: None,
                error: None,
                output_file: None,
                duration_ms: Some(7),
                agent_id: Some("agent-fallback".into()),
                agent_type: None,
                provider: spec.provider,
                model: spec.model,
                sub_agent_tool_calls: None,
                read_file_count: None,
                total_tokens: Some(11),
                output_tokens: Some(11),
                usage: None,
                diagnostics: Some(json!({
                    "structured_output": {
                        "coercion_attempts": 1,
                        "coercion_exhausted": true
                    }
                })),
                git: None,
            })
        }
    }

    #[derive(Clone)]
    struct CountingSpawner {
        calls: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct BlockingSpawner {
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SubAgentSpawner for BlockingSpawner {
        async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentResult, String> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(SubAgentResult {
                final_text: spec.prompt,
                status: "completed".into(),
                tool_call_count: 0,
                stop_reason: None,
                error: None,
                output_file: None,
                duration_ms: Some(1),
                agent_id: None,
                agent_type: None,
                provider: None,
                model: None,
                sub_agent_tool_calls: None,
                read_file_count: None,
                total_tokens: Some(1),
                output_tokens: Some(1),
                usage: None,
                diagnostics: None,
                git: None,
            })
        }
    }

    #[async_trait]
    impl SubAgentSpawner for CountingSpawner {
        async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentResult, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(SubAgentResult {
                final_text: spec.prompt,
                status: "completed".into(),
                tool_call_count: 0,
                stop_reason: None,
                error: None,
                output_file: None,
                duration_ms: Some(1),
                agent_id: None,
                agent_type: None,
                provider: None,
                model: None,
                sub_agent_tool_calls: None,
                read_file_count: None,
                total_tokens: Some(1),
                output_tokens: Some(1),
                usage: None,
                diagnostics: None,
                git: None,
            })
        }
    }

    fn workflow_test_run_with_args(
        temp: &TempDir,
        run_id: &str,
        context: ToolContext,
        nested_workflows: Vec<WorkflowResolvedScript>,
        args: Option<Value>,
    ) -> WorkflowRun {
        let registry = TaskRegistry::default();
        let task_id = generate_task_id(TaskKind::LocalWorkflow);
        let cancel = PromptCancel::new();
        register_local_workflow_task(
            &registry,
            LocalWorkflowTaskSpec {
                id: task_id.clone(),
                run_id: run_id.into(),
                workflow_name: "demo".into(),
                summary: None,
                agent_count: 0,
                output_path: None,
                script_path: None,
                args: args.clone(),
                parent_session_id: None,
                parent_tool_call_id: None,
                is_backgrounded: true,
            },
            cancel.clone(),
        )
        .unwrap();
        WorkflowRun {
            registry,
            task_id,
            run_id: run_id.into(),
            transcript_dir: temp.path().join("transcript"),
            persisted_script: temp.path().join("script.js"),
            cwd: temp.path().to_path_buf(),
            script: "export const meta = { name: 'demo', description: 'Demo' };".into(),
            meta: WorkflowMeta {
                name: "demo".into(),
                description: "Demo".into(),
                title: None,
                when_to_use: None,
                phases: Vec::new(),
                default_model: None,
            },
            args,
            context,
            cancel,
            depth: 0,
            permission_prompts_unavailable: false,
            nested_workflows,
            budget_total: None,
            agent_concurrency_cap: None,
        }
    }

    fn workflow_test_run(
        temp: &TempDir,
        run_id: &str,
        context: ToolContext,
        nested_workflows: Vec<WorkflowResolvedScript>,
    ) -> WorkflowRun {
        workflow_test_run_with_args(temp, run_id, context, nested_workflows, None)
    }

    #[tokio::test]
    async fn workflow_failure_before_agent_dispatch_reports_zero_agents_and_no_completion() {
        let temp = TempDir::new().expect("tempdir");
        let mut run = workflow_test_run(
            &temp,
            "wf_unsupported_effort",
            ToolContext::new().with_sub_agent_spawner(Arc::new(TestSpawner)),
            Vec::new(),
        );
        run.script = r#"
export const meta = { name: 'demo', description: 'Demo', phases: [{ title: 'Verify' }] };
return await agent('verify', { label: 'verify', phase: 'Verify', effort: 'high' });
"#
        .into();

        let outcome = execute_and_finalize_workflow(run, false).await;

        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.agent_count, 0);
        let error = outcome.error.expect("model-visible failure");
        assert!(error.contains("failed before any agent calls ran"));
        assert!(error.contains("No requested implementation or verification was performed"));
        assert!(error.contains("Unsupported workflow agent option(s): effort"));
        assert!(error.contains("modelProfile"));
    }

    /// A terminally failed agent fails the workflow immediately — a null
    /// flowing downstream would make later phases fabricate output from
    /// missing context. The failure steers the model to `resumeFromRunId`:
    /// the journal keeps completed results (and skips the failed entry), so
    /// a resume replays the cache and retries only the failed call.
    #[tokio::test]
    async fn workflow_failed_agent_fails_fast_and_points_at_resume() {
        let temp = TempDir::new().expect("tempdir");
        let mut run = workflow_test_run(
            &temp,
            "wf_failed_narrative",
            ToolContext::new().with_sub_agent_spawner(Arc::new(FailedNarrativeSpawner)),
            Vec::new(),
        );
        run.script = r#"
export const meta = { name: 'demo', description: 'Demo', phases: [{ title: 'Verify' }] };
return await agent('verify', {
  label: 'verify',
  phase: 'Verify',
  schema: { type: 'object', required: ['ok'], properties: { ok: { type: 'boolean' } } },
});
"#
        .into();

        let outcome = execute_and_finalize_workflow(run, false).await;

        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.agent_count, 1);
        let error = outcome.error.expect("model-visible failure");
        assert!(error.contains("Workflow failed after 1 agent call(s)"));
        assert!(error.contains("MissingStructuredOutput"));
        assert!(error.contains("resumeFromRunId: \"wf_failed_narrative\""));
        assert!(!error.contains("验证完成"));

        let journal = fs::read_to_string(temp.path().join("transcript/journal.jsonl"))
            .expect("workflow journal");
        assert!(journal.contains("验证完成：所有实现与测试均已通过"));
        assert!(journal.contains("agent_final_text"));
        // The failed entry is a diagnostic, not a cache hit: a resume must
        // retry this call instead of replaying its null.
        let journal_entries = WorkflowJournal::new(temp.path().join("transcript/journal.jsonl"))
            .load()
            .expect("journal load");
        assert!(journal_entries.is_empty(), "failed result must not cache");
    }

    #[tokio::test]
    async fn js_runtime_returns_agent_final_text_string_without_schema() {
        let temp = TempDir::new().expect("tempdir");
        let run = workflow_test_run(
            &temp,
            "wf_text_return",
            ToolContext::new().with_sub_agent_spawner(Arc::new(TestSpawner)),
            Vec::new(),
        );

        let result = ScriptRuntime::new(
            &run,
            "return await agent('plain text');".into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");

        assert_eq!(result["result"], "done: plain text");
    }

    #[tokio::test]
    async fn js_runtime_budget_global_is_available() {
        let temp = TempDir::new().expect("tempdir");
        let run = workflow_test_run(&temp, "wf_budget", ToolContext::new(), Vec::new());

        let result = ScriptRuntime::new(
            &run,
            "return { total: budget.total, spent: budget.spent(), remaining: budget.remaining() };"
                .into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");

        assert_eq!(result["result"]["total"], Value::Null);
        assert_eq!(result["result"]["spent"], 0);
        assert_eq!(result["result"]["remaining"], Value::Null);
    }

    /// The parser strips `export const meta = {…}` before the body runs, so
    /// the runtime must re-expose it — scripts naturally return `meta.name`.
    #[tokio::test]
    async fn js_runtime_meta_global_is_available() {
        let temp = TempDir::new().expect("tempdir");
        let run = workflow_test_run(&temp, "wf_meta", ToolContext::new(), Vec::new());

        let result = ScriptRuntime::new(
            &run,
            "return { workflow: meta.name, described: typeof meta.description === 'string' };"
                .into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");

        assert_eq!(result["result"]["workflow"], "demo");
        assert_eq!(result["result"]["described"], true);
    }

    #[tokio::test]
    async fn js_runtime_nested_workflow_supports_script_path_and_one_level_limit() {
        let temp = TempDir::new().expect("tempdir");
        let child_path = temp.path().join("child.js");
        fs::write(
            &child_path,
            "export const meta = { name: 'child', description: 'Child' };\nreturn { childArg: args.task };",
        )
        .expect("write child");
        let parent = workflow_test_run(&temp, "wf_nested", ToolContext::new(), Vec::new());

        let result = ScriptRuntime::new(
            &parent,
            format!(
                "return await workflow({{ scriptPath: {} }}, {{ task: 'ok' }});",
                serde_json::to_string(child_path.to_str().expect("path")).expect("path json")
            ),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("nested result");
        assert_eq!(result["result"]["childArg"], "ok");

        let grandchild_path = temp.path().join("grandchild.js");
        fs::write(
            &grandchild_path,
            "export const meta = { name: 'grandchild', description: 'Grandchild' };\nreturn 'too deep';",
        )
        .expect("write grandchild");
        fs::write(
            &child_path,
            format!(
                "export const meta = {{ name: 'child', description: 'Child' }};\nreturn await workflow({{ scriptPath: {} }});",
                serde_json::to_string(grandchild_path.to_str().expect("path")).expect("path json")
            ),
        )
        .expect("write child nested");

        let error = ScriptRuntime::new(
            &parent,
            format!(
                "return await workflow({{ scriptPath: {} }});",
                serde_json::to_string(child_path.to_str().expect("path")).expect("path json")
            ),
            WorkflowJournal::new(temp.path().join("journal-too-deep.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect_err("nested workflow depth should be limited");
        assert!(error.contains("Nested workflow depth exceeded"));
    }

    #[tokio::test]
    async fn js_runtime_agent_call_limit_is_rfc_1000() {
        let temp = TempDir::new().expect("tempdir");
        let run = workflow_test_run(
            &temp,
            "wf_agent_limit",
            ToolContext::new().with_sub_agent_spawner(Arc::new(CountingSpawner {
                calls: Arc::new(AtomicUsize::new(0)),
            })),
            Vec::new(),
        );

        let result = ScriptRuntime::new(
            &run,
            "for (let i = 0; i < 1000; i++) { await agent(`ok ${i}`); } return 'done';".into(),
            WorkflowJournal::new(temp.path().join("journal-limit-ok.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("1000 calls should be allowed");
        assert_eq!(result["agentCount"], 1000);

        let error = ScriptRuntime::new(
            &run,
            "for (let i = 0; i < 1001; i++) { await agent(`too many ${i}`); } return 'done';"
                .into(),
            WorkflowJournal::new(temp.path().join("journal-limit-fail.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect_err("1001 calls should exceed limit");
        assert!(error.contains("1000"));
    }

    #[test]
    fn workflow_agent_concurrency_cap_matches_rfc_ceiling() {
        let cap = workflow_agent_concurrency_cap();
        assert!((1..=16).contains(&cap));
    }

    #[tokio::test]
    async fn js_runtime_executes_primitives_and_returns_result() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let task_id = generate_task_id(TaskKind::LocalWorkflow);
        let run_id = "wf_test_runtime".to_string();
        let cancel = PromptCancel::new();
        register_local_workflow_task(
            &registry,
            LocalWorkflowTaskSpec {
                id: task_id.clone(),
                run_id: run_id.clone(),
                workflow_name: "demo".into(),
                summary: None,
                agent_count: 0,
                output_path: None,
                script_path: None,
                args: Some(json!({"task":"ship"})),
                parent_session_id: None,
                parent_tool_call_id: None,
                is_backgrounded: true,
            },
            cancel.clone(),
        )
        .unwrap();
        let run = WorkflowRun {
            registry: registry.clone(),
            task_id: task_id.clone(),
            run_id,
            transcript_dir: temp.path().join("transcript"),
            persisted_script: temp.path().join("script.js"),
            cwd: temp.path().to_path_buf(),
            script: "export const meta = { name: 'demo', description: 'Demo' };".into(),
            meta: WorkflowMeta {
                name: "demo".into(),
                description: "Demo".into(),
                title: None,
                when_to_use: None,
                phases: Vec::new(),
                default_model: None,
            },
            args: Some(json!({"task":"ship"})),
            context: ToolContext::new().with_sub_agent_spawner(Arc::new(TestSpawner)),
            cancel,
            depth: 0,
            permission_prompts_unavailable: false,
            nested_workflows: Vec::new(),
            budget_total: None,
            agent_concurrency_cap: None,
        };
        let body = r#"
log('starting');
const first = await phase('Run', async () => await agent(`build ${args.task}`, { model: 'm1' }));
const both = await parallel([
  () => agent('check one'),
  () => agent('check two'),
]);
const seq = await pipeline(['step'],
  () => agent('step one'),
  prev => agent(`step two after ${prev}`),
);
return { first, both, seq };
"#;
        let result = ScriptRuntime::new(
            &run,
            body.into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");
        assert_eq!(result["agentCount"], 5);
        assert_eq!(result["logs"], json!(["starting"]));
        assert_eq!(result["result"]["first"], "done: build ship");
        assert_eq!(
            registry
                .snapshot(&task_id)
                .expect("snapshot")
                .last_progress
                .as_deref(),
            Some("agent completed: step two after done: step one"),
        );
    }

    #[tokio::test]
    async fn js_runtime_parallel_runs_agent_callbacks_concurrently() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let task_id = generate_task_id(TaskKind::LocalWorkflow);
        let run_id = "wf_test_parallel".to_string();
        let cancel = PromptCancel::new();
        register_local_workflow_task(
            &registry,
            LocalWorkflowTaskSpec {
                id: task_id.clone(),
                run_id: run_id.clone(),
                workflow_name: "demo".into(),
                summary: None,
                agent_count: 0,
                output_path: None,
                script_path: None,
                args: None,
                parent_session_id: None,
                parent_tool_call_id: None,
                is_backgrounded: true,
            },
            cancel.clone(),
        )
        .unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let run = WorkflowRun {
            registry,
            task_id,
            run_id,
            transcript_dir: temp.path().join("transcript"),
            persisted_script: temp.path().join("script.js"),
            cwd: temp.path().to_path_buf(),
            script: "export const meta = { name: 'demo', description: 'Demo' };".into(),
            meta: WorkflowMeta {
                name: "demo".into(),
                description: "Demo".into(),
                title: None,
                when_to_use: None,
                phases: Vec::new(),
                default_model: None,
            },
            args: None,
            context: ToolContext::new().with_sub_agent_spawner(Arc::new(BlockingSpawner {
                active: active.clone(),
                max_active: max_active.clone(),
            })),
            cancel,
            depth: 0,
            permission_prompts_unavailable: false,
            nested_workflows: Vec::new(),
            budget_total: None,
            agent_concurrency_cap: None,
        };
        let body = r#"
const both = await parallel([
  () => agent('one'),
  () => agent('two'),
]);
return both;
"#;
        let result = ScriptRuntime::new(
            &run,
            body.into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");

        assert_eq!(result["result"], json!(["one", "two"]));
        assert!(max_active.load(Ordering::SeqCst) > 1);
    }

    /// Fail-fast must not leave in-flight agents writing to the run's
    /// journal after the run settles: with one parallel branch failing and
    /// the other still running, the run only turns terminal once the slow
    /// branch has landed — and its completed result is journaled for the
    /// resume cache.
    #[tokio::test]
    async fn workflow_fail_fast_drains_inflight_agents_before_settling() {
        struct SplitSpawner {
            slow_started: Arc<tokio::sync::Notify>,
            gate: Arc<tokio::sync::Notify>,
            slow_finished: Arc<std::sync::atomic::AtomicBool>,
        }

        #[async_trait]
        impl SubAgentSpawner for SplitSpawner {
            async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentResult, String> {
                if spec.prompt == "fail" {
                    // Error only once the slow branch is genuinely in flight.
                    self.slow_started.notified().await;
                    return Err("agent exploded".into());
                }
                self.slow_started.notify_one();
                self.gate.notified().await;
                self.slow_finished.store(true, Ordering::SeqCst);
                Ok(SubAgentResult {
                    final_text: "slow done".into(),
                    status: "completed".into(),
                    tool_call_count: 0,
                    stop_reason: None,
                    error: None,
                    output_file: None,
                    duration_ms: Some(1),
                    agent_id: None,
                    agent_type: None,
                    provider: None,
                    model: None,
                    sub_agent_tool_calls: None,
                    read_file_count: None,
                    total_tokens: Some(1),
                    output_tokens: Some(1),
                    usage: None,
                    diagnostics: None,
                    git: None,
                })
            }
        }

        let temp = TempDir::new().expect("tempdir");
        let slow_started = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(tokio::sync::Notify::new());
        let slow_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut run = workflow_test_run(
            &temp,
            "wf_fail_fast_drain",
            ToolContext::new().with_sub_agent_spawner(Arc::new(SplitSpawner {
                slow_started,
                gate: gate.clone(),
                slow_finished: slow_finished.clone(),
            })),
            Vec::new(),
        );
        run.script = r#"
export const meta = { name: 'demo', description: 'Demo' };
return await parallel([
  () => agent('slow'),
  () => agent('fail'),
]);
"#
        .into();
        // Two agent slots for THIS run, so the slow branch is genuinely in
        // flight on any machine — a dual-core CI derives a cap of 1 and
        // would otherwise reduce this regression test to a no-op. Instance
        // state, not a process global: parallel tests read their own caps.
        run.agent_concurrency_cap = Some(2);
        let registry = run.registry.clone();
        let task_id = run.task_id.clone();

        let handle = tokio::spawn(execute_and_finalize_workflow(run, false));

        // Wait until the failing branch has errored (its progress entry is
        // recorded before the driver hears about the failure)…
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let errored = registry.snapshot(&task_id).is_some_and(|snap| {
                matches!(&snap.data, TaskData::LocalWorkflow(data) if data
                    .progress_entries
                    .iter()
                    .any(|entry| matches!(entry, WorkflowProgressEntry::Agent { state, .. } if state == "error")))
            });
            if errored {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "failing agent never errored"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // …then give a broken (non-draining) implementation ample time to
        // settle early. The run must still be live: the slow branch is.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            !handle.is_finished(),
            "run settled while an agent was still in flight"
        );

        gate.notify_one();
        let outcome = handle.await.expect("join");
        assert_eq!(outcome.status, "failed");
        assert!(slow_finished.load(Ordering::SeqCst));
        assert!(outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("agent exploded")));

        // The slow branch's completed result landed in the journal before
        // the run turned terminal, so a resume replays it from cache.
        let journal = fs::read_to_string(temp.path().join("transcript/journal.jsonl"))
            .expect("workflow journal");
        assert!(journal.contains("slow done"));
        let cached = WorkflowJournal::new(temp.path().join("transcript/journal.jsonl"))
            .load()
            .expect("journal load");
        assert!(!cached.is_empty(), "slow result must cache for resume");
    }

    #[tokio::test]
    async fn js_runtime_resume_uses_cached_agent_result() {
        let temp = TempDir::new().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = TaskRegistry::default();
        let task_id = generate_task_id(TaskKind::LocalWorkflow);
        let cancel = PromptCancel::new();
        register_local_workflow_task(
            &registry,
            LocalWorkflowTaskSpec {
                id: task_id.clone(),
                run_id: "wf_cached".into(),
                workflow_name: "demo".into(),
                summary: None,
                agent_count: 0,
                output_path: None,
                script_path: None,
                args: None,
                parent_session_id: None,
                parent_tool_call_id: None,
                is_backgrounded: true,
            },
            cancel.clone(),
        )
        .unwrap();
        let run = WorkflowRun {
            registry,
            task_id,
            run_id: "wf_cached".into(),
            transcript_dir: temp.path().join("transcript"),
            persisted_script: temp.path().join("script.js"),
            cwd: temp.path().to_path_buf(),
            script: "export const meta = { name: 'demo', description: 'Demo' };".into(),
            meta: WorkflowMeta {
                name: "demo".into(),
                description: "Demo".into(),
                title: None,
                when_to_use: None,
                phases: Vec::new(),
                default_model: None,
            },
            args: None,
            context: ToolContext::new().with_sub_agent_spawner(Arc::new(CountingSpawner {
                calls: calls.clone(),
            })),
            cancel,
            depth: 0,
            permission_prompts_unavailable: false,
            nested_workflows: Vec::new(),
            budget_total: None,
            agent_concurrency_cap: None,
        };
        let key = compute_agent_call_key("cached prompt", None);
        let mut cached = HashMap::new();
        cached.insert(
            key,
            json!({
                "data": { "finalText": "from cache" },
                "tokens": 3,
                "toolCalls": 1,
                "durationMs": 2,
            }),
        );
        let result = ScriptRuntime::new(
            &run,
            "return await agent('cached prompt');".into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            cached,
        )
        .run()
        .await
        .expect("runtime result");
        assert_eq!(result["result"], "from cache");
        assert_eq!(result["agentCount"], 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn js_runtime_driver_throttles_concurrency_to_cap() {
        // Prove the runtime driver actually caps in-flight agents at
        // `workflow_agent_concurrency_cap()`, not merely that the helper returns
        // a value in range. Spawn strictly more agents than the cap; the
        // BlockingSpawner records the peak number running simultaneously.
        let temp = TempDir::new().expect("tempdir");
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let run = workflow_test_run(
            &temp,
            "wf_throttle",
            ToolContext::new().with_sub_agent_spawner(Arc::new(BlockingSpawner {
                active: active.clone(),
                max_active: max_active.clone(),
            })),
            Vec::new(),
        );
        let cap = workflow_agent_concurrency_cap();
        let total = cap + 8; // strictly greater than the cap so throttling must engage
        let body = format!(
            "const thunks = []; for (let i = 0; i < {total}; i++) {{ thunks.push(() => agent('c' + i)); }} await parallel(thunks); return 'ok';"
        );

        let result = ScriptRuntime::new(
            &run,
            body,
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");

        assert_eq!(result["result"], "ok");
        assert_eq!(result["agentCount"], total);
        let observed = max_active.load(Ordering::SeqCst);
        assert!(
            observed <= cap,
            "driver let {observed} agents run concurrently, exceeding cap {cap}"
        );
        if cap > 1 {
            assert!(
                observed > 1,
                "expected real concurrency, only saw {observed}"
            );
        }
    }

    #[tokio::test]
    async fn js_runtime_budget_blocks_new_agents_when_exhausted() {
        // The hard constraint — once spent tokens reach the target,
        // further agent() calls must be refused. TestSpawner reports 11 output
        // tokens per agent, so with a budget of 10 the first agent exhausts it
        // and the second must throw.
        let temp = TempDir::new().expect("tempdir");
        let mut run = workflow_test_run(
            &temp,
            "wf_budget_enforce",
            ToolContext::new().with_sub_agent_spawner(Arc::new(TestSpawner)),
            Vec::new(),
        );
        run.budget_total = Some(10);

        let error = ScriptRuntime::new(
            &run,
            "await agent('first'); await agent('second'); return 'done';".into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect_err("second agent must be blocked once the budget is exhausted");
        assert!(error.contains("budget"), "unexpected error: {error}");
    }

    #[tokio::test]
    async fn js_runtime_budget_tracks_spent_and_remaining() {
        // spent()/remaining() must reflect real token accounting, not a
        // stub. One TestSpawner agent charges its 11 output tokens against a
        // budget of 100 — NOT its display total of 99, which includes billed
        // input context.
        let temp = TempDir::new().expect("tempdir");
        let mut run = workflow_test_run(
            &temp,
            "wf_budget_track",
            ToolContext::new().with_sub_agent_spawner(Arc::new(TestSpawner)),
            Vec::new(),
        );
        run.budget_total = Some(100);

        let result = ScriptRuntime::new(
            &run,
            "await agent('x'); return { total: budget.total, spent: budget.spent(), remaining: budget.remaining() };".into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");

        assert_eq!(result["result"]["total"], 100);
        assert_eq!(result["result"]["spent"], 11);
        assert_eq!(result["result"]["remaining"], 89);
    }

    #[tokio::test]
    async fn js_runtime_supports_then_chaining_and_nested_parallel() {
        // agent() returns a real promise, so `.then()` composes, and a
        // thunk that itself calls parallel() resolves correctly instead of
        // collapsing to a prototype-less null sentinel.
        let temp = TempDir::new().expect("tempdir");
        let run = workflow_test_run(
            &temp,
            "wf_then_nested",
            ToolContext::new().with_sub_agent_spawner(Arc::new(TestSpawner)),
            Vec::new(),
        );
        let body = r#"
const chained = await agent('chain').then(text => `${text}!`);
const nested = await parallel([
  () => parallel([() => agent('inner one'), () => agent('inner two')]),
  () => agent('outer'),
]);
return { chained, nested };
"#;

        let result = ScriptRuntime::new(
            &run,
            body.into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");

        assert_eq!(result["result"]["chained"], "done: chain!");
        assert_eq!(
            result["result"]["nested"],
            json!([["done: inner one", "done: inner two"], "done: outer"])
        );
        assert_eq!(result["agentCount"], 4);
    }

    #[tokio::test]
    async fn js_runtime_pipeline_flows_items_through_stages_independently() {
        // Each item flows through every stage on its own chain, with no
        // barrier between stages. Correctness: 3 items × 2 stages produce the
        // expected per-item results in order. Independence: the three stage-1
        // agents are dispatched concurrently (peak > 1) rather than serialized
        // item-by-item as the old nested-`for` implementation did.
        let temp = TempDir::new().expect("tempdir");
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let run = workflow_test_run(
            &temp,
            "wf_pipeline_flow",
            ToolContext::new().with_sub_agent_spawner(Arc::new(BlockingSpawner {
                active: active.clone(),
                max_active: max_active.clone(),
            })),
            Vec::new(),
        );
        let body = r#"
return await pipeline(['a', 'b', 'c'],
  item => agent(`s1:${item}`),
  prev => agent(`s2:${prev}`),
);
"#;

        let result = ScriptRuntime::new(
            &run,
            body.into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");

        assert_eq!(result["result"], json!(["s2:s1:a", "s2:s1:b", "s2:s1:c"]));
        assert_eq!(result["agentCount"], 6);
        if workflow_agent_concurrency_cap() > 1 {
            assert!(
                max_active.load(Ordering::SeqCst) > 1,
                "pipeline serialized items instead of flowing them independently"
            );
        }
    }

    #[tokio::test]
    async fn js_runtime_duplicate_identical_calls_get_distinct_cache_keys() {
        // Two identical (prompt, opts) agent() calls must map to distinct
        // cache keys (`base` and `base#1`) so resume does not collapse them onto
        // one cached result. Seed the cache with distinct values per key; if the
        // keys collapsed, both calls would return "cached-A".
        let temp = TempDir::new().expect("tempdir");
        let calls = Arc::new(AtomicUsize::new(0));
        let run = workflow_test_run(
            &temp,
            "wf_dup_keys",
            ToolContext::new().with_sub_agent_spawner(Arc::new(CountingSpawner {
                calls: calls.clone(),
            })),
            Vec::new(),
        );
        let base = compute_agent_call_key("dup", None);
        let mut cached = HashMap::new();
        cached.insert(
            base.clone(),
            json!({ "data": { "finalText": "cached-A" }, "tokens": 1, "toolCalls": 0, "durationMs": 1 }),
        );
        cached.insert(
            format!("{base}#1"),
            json!({ "data": { "finalText": "cached-B" }, "tokens": 1, "toolCalls": 0, "durationMs": 1 }),
        );

        let result = ScriptRuntime::new(
            &run,
            "const a = await agent('dup'); const b = await agent('dup'); return [a, b];".into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            cached,
        )
        .run()
        .await
        .expect("runtime result");

        assert_eq!(result["result"], json!(["cached-A", "cached-B"]));
        assert_eq!(result["agentCount"], 2);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn launcher_waits_for_workflow_completion_and_returns_result() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let launcher = WorkflowRegistryLauncher::new(
            registry.clone(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "session-a",
        );
        let context = ToolContext::new().with_sub_agent_spawner(Arc::new(TestSpawner));

        let status = launcher
            .launch_workflow(
                WorkflowLaunchSpec {
                    script: Some(
                        "export const meta = { name: 'wait-demo', description: 'Wait demo' };\n\
                         const result = await agent('finish workflow');\n\
                         return { child: result };"
                            .into(),
                    ),
                    ..WorkflowLaunchSpec::default()
                },
                &context,
            )
            .await
            .expect("launch workflow");

        assert_eq!(status.status, "completed");
        assert_eq!(status.agent_count, Some(1));
        assert_eq!(
            status.result.as_ref().unwrap()["result"]["child"],
            "done: finish workflow"
        );
        let snapshot = registry
            .snapshot(&TaskId::new(status.task_id.clone().expect("task id")))
            .expect("workflow task snapshot");
        assert_eq!(snapshot.status, TaskStatus::Completed);
        assert!(snapshot.notified);
        assert!(registry
            .unnotified_terminal_agent_notifications()
            .is_empty());
    }

    #[tokio::test]
    async fn launcher_background_launch_returns_receipt_and_terminal_notification() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let launcher = WorkflowRegistryLauncher::new(
            registry.clone(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "session-a",
        );

        let status = launcher
            .launch_workflow(
                WorkflowLaunchSpec {
                    script: Some(
                        "export const meta = { name: 'background-demo', description: 'Background demo' };\nreturn { ok: true };"
                            .into(),
                    ),
                    run_in_background: true,
                    ..WorkflowLaunchSpec::default()
                },
                &ToolContext::new(),
            )
            .await
            .expect("background launch");

        assert_eq!(status.status, "async_launched");
        let task_id = TaskId::new(status.task_id.expect("task id"));
        let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(snapshot) = registry.snapshot(&task_id) {
                    if snapshot.status.is_terminal() {
                        break snapshot;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background workflow completion");

        assert_eq!(snapshot.status, TaskStatus::Completed);
        assert!(snapshot.is_backgrounded);
        assert!(!snapshot.notified);
        let notifications = registry.unnotified_terminal_notifications();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].task_id, task_id);
        assert!(notifications[0].message.contains("<task-notification>"));
        assert!(notifications[0].message.contains("background-demo"));
    }

    #[tokio::test]
    async fn launcher_allows_one_nested_level_and_rejects_deeper_launches() {
        let temp = TempDir::new().expect("tempdir");
        let launcher = WorkflowRegistryLauncher::new(
            TaskRegistry::default(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "session-a",
        );
        let spec = WorkflowLaunchSpec {
            script: Some(
                "export const meta = { name: 'nested-demo', description: 'Nested demo' };\nreturn 'done';"
                    .into(),
            ),
            ..WorkflowLaunchSpec::default()
        };
        let allowed = ToolContext::new().with_workflow_nesting_depth(MAX_NESTED_WORKFLOW_DEPTH);

        launcher
            .preview_workflow(spec.clone(), &allowed)
            .await
            .expect("nested preview");
        let status = launcher
            .launch_workflow(spec.clone(), &allowed)
            .await
            .expect("nested launch");
        assert_eq!(status.status, "completed");

        let rejected =
            ToolContext::new().with_workflow_nesting_depth(MAX_NESTED_WORKFLOW_DEPTH + 1);
        let preview_error = launcher
            .preview_workflow(spec.clone(), &rejected)
            .await
            .expect_err("deeper nested preview must fail");
        assert!(preview_error.contains("Nested workflow depth exceeded"));
        let launch_error = launcher
            .launch_workflow(spec, &rejected)
            .await
            .expect_err("deeper nested launch must fail");
        assert!(launch_error.contains("Nested workflow depth exceeded"));
    }

    #[tokio::test]
    async fn launcher_completed_status_includes_workflow_progress_for_replay() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let launcher = WorkflowRegistryLauncher::new(
            registry.clone(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "session-a",
        );
        let context = ToolContext::new().with_sub_agent_spawner(Arc::new(TestSpawner));

        let status = launcher
            .launch_workflow(
                WorkflowLaunchSpec {
                    script: Some(
                        "export const meta = { name: 'progress-demo', description: 'Progress demo' };\n\
                         const result = await agent('finish workflow');\n\
                         return { child: result };"
                            .into(),
                    ),
                    ..WorkflowLaunchSpec::default()
                },
                &context,
            )
            .await
            .expect("launch workflow");

        assert_eq!(status.status, "completed");
        // The terminal result must carry the render-facing progress tree so
        // the TUI workflow card survives transcript replay; the streaming
        // overlay that accumulates these entries is discarded on commit.
        let progress = status
            .workflow_progress
            .as_ref()
            .expect("completed status carries workflowProgress");
        assert_eq!(progress["workflowName"], "progress-demo");
        let entries = progress["entries"]
            .as_array()
            .expect("workflowProgress entries array");
        assert!(
            !entries.is_empty(),
            "expected accumulated progress entries, got {progress:?}"
        );
        let agent_entries: Vec<_> = entries
            .iter()
            .filter(|entry| entry["entry"]["type"] == "agent")
            .collect();
        assert_eq!(
            agent_entries.len(),
            1,
            "terminal replay keeps only the latest bounded agent state: {entries:?}"
        );
        assert_eq!(agent_entries[0]["entry"]["state"], "completed");
        assert!(agent_entries[0]["entry"]["agentId"].as_str().is_some());

        let task_id = TaskId::new(status.task_id.as_deref().expect("task id"));
        let snapshot = registry.snapshot(&task_id).expect("workflow snapshot");
        let TaskData::LocalWorkflow(data) = snapshot.data else {
            panic!("local workflow snapshot");
        };
        let raw_agent_ids: Vec<_> = data
            .progress_entries
            .iter()
            .filter_map(|entry| match entry {
                WorkflowProgressEntry::Agent { agent_id, .. } => agent_id.as_deref(),
                _ => None,
            })
            .collect();
        assert!(raw_agent_ids.len() >= 2);
        assert!(raw_agent_ids
            .iter()
            .all(|agent_id| *agent_id == raw_agent_ids[0]));
    }

    #[tokio::test]
    async fn reentrant_same_title_phases_keep_matching_instance_ids() {
        let temp = TempDir::new().expect("tempdir");
        let launcher = WorkflowRegistryLauncher::new(
            TaskRegistry::default(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "session-a",
        );

        let status = launcher
            .launch_workflow(
                WorkflowLaunchSpec {
                    script: Some(
                        "export const meta = { name: 'nested-phases', description: 'Nested phases' };\n\
                         let releaseInner;\n\
                         const gate = new Promise((resolve) => { releaseInner = resolve; });\n\
                         const outer = phase('Review', async () => {});\n\
                         const inner = phase('Review', async () => { await gate; });\n\
                         await outer;\n\
                         releaseInner();\n\
                         await inner;\n\
                         return 'done';"
                            .into(),
                    ),
                    ..WorkflowLaunchSpec::default()
                },
                &ToolContext::new(),
            )
            .await
            .expect("launch workflow");

        let entries = status
            .workflow_progress
            .as_ref()
            .and_then(|progress| progress["entries"].as_array())
            .expect("workflow progress entries");
        let phases: Vec<_> = entries
            .iter()
            .filter_map(|event| {
                let entry = &event["entry"];
                (entry["type"] == "phase").then(|| {
                    (
                        entry["state"].as_str().unwrap().to_string(),
                        entry["phaseInstanceId"].as_str().unwrap().to_string(),
                    )
                })
            })
            .collect();

        assert_eq!(phases.len(), 2, "{entries:?}");
        assert!(phases.iter().all(|phase| phase.0 == "completed"));
        assert_ne!(phases[0].1, phases[1].1);
    }

    #[tokio::test]
    async fn reentrant_phase_agents_keep_invocation_identity_through_completion() {
        let temp = TempDir::new().expect("tempdir");
        let launcher = WorkflowRegistryLauncher::new(
            TaskRegistry::default(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "session-a",
        );
        let context = ToolContext::new().with_sub_agent_spawner(Arc::new(TestSpawner));

        let status = launcher
            .launch_workflow(
                WorkflowLaunchSpec {
                    script: Some(
                        "export const meta = { name: 'nested-phase-agent', description: 'Nested phase agent' };\n\
                         const outer = phase('Review', async () => await agent('outer work', { label: 'outer-agent' }));\n\
                         const inner = phase('Review', async () => {});\n\
                         await Promise.all([outer, inner]);\n\
                         return 'done';"
                            .into(),
                    ),
                    ..WorkflowLaunchSpec::default()
                },
                &context,
            )
            .await
            .expect("launch workflow");

        let entries = status
            .workflow_progress
            .as_ref()
            .and_then(|progress| progress["entries"].as_array())
            .expect("workflow progress entries");
        let phases: Vec<_> = entries
            .iter()
            .filter_map(|event| {
                let entry = &event["entry"];
                (entry["type"] == "phase").then(|| {
                    (
                        entry["state"].as_str().unwrap().to_string(),
                        entry["phaseInstanceId"].as_str().unwrap().to_string(),
                    )
                })
            })
            .collect();
        let agent_events: Vec<_> = entries
            .iter()
            .filter_map(|event| {
                let entry = &event["entry"];
                (entry["type"] == "agent").then(|| {
                    (
                        entry["state"].as_str().unwrap().to_string(),
                        entry["phaseInstanceId"].as_str().unwrap().to_string(),
                        entry["agentId"].as_str().unwrap().to_string(),
                    )
                })
            })
            .collect();

        assert_eq!(phases.len(), 2, "{entries:?}");
        assert!(phases.iter().all(|phase| phase.0 == "completed"));
        assert_ne!(phases[0].1, phases[1].1);
        assert_eq!(agent_events.len(), 1, "{entries:?}");
        assert_eq!(agent_events[0].0, "completed");
        assert!(phases.iter().any(|phase| phase.1 == agent_events[0].1));
        assert!(!agent_events[0].2.is_empty());
    }

    #[tokio::test]
    async fn background_workflow_agents_disable_interactive_permission_prompts() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let launcher = WorkflowRegistryLauncher::new(
            registry.clone(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "session-a",
        );
        let unavailable = Arc::new(AtomicBool::new(false));
        let context =
            ToolContext::new().with_sub_agent_spawner(Arc::new(PromptAvailabilitySpawner {
                unavailable: unavailable.clone(),
            }));

        let status = launcher
            .launch_workflow(
                WorkflowLaunchSpec {
                    script: Some(
                        "export const meta = { name: 'background-agent', description: 'Background agent' };\nconst result = await agent('finish safely');\nreturn { result };"
                            .into(),
                    ),
                    run_in_background: true,
                    ..WorkflowLaunchSpec::default()
                },
                &context,
            )
            .await
            .expect("background launch");

        let task_id = TaskId::new(status.task_id.expect("task id"));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if registry
                    .snapshot(&task_id)
                    .is_some_and(|snapshot| snapshot.status.is_terminal())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background workflow completion");

        assert!(unavailable.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn nested_workflow_inherits_unavailable_permission_prompts_from_parent_context() {
        let temp = TempDir::new().expect("tempdir");
        let unavailable = Arc::new(AtomicBool::new(false));
        let launcher = WorkflowRegistryLauncher::new(
            TaskRegistry::default(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "session-a",
        );
        let context = ToolContext::new()
            .with_workflow_nesting_depth(1)
            .with_permission_prompts_unavailable(true)
            .with_sub_agent_spawner(Arc::new(PromptAvailabilitySpawner {
                unavailable: unavailable.clone(),
            }));

        let status = launcher
            .launch_workflow(
                WorkflowLaunchSpec {
                    script: Some(
                        "export const meta = { name: 'nested-unavailable', description: 'Nested unavailable' };\nconst result = await agent('finish safely');\nreturn { result };"
                            .into(),
                    ),
                    ..WorkflowLaunchSpec::default()
                },
                &context,
            )
            .await
            .expect("nested workflow launch");

        assert_eq!(status.status, "completed");
        assert!(unavailable.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn launcher_returns_failed_status_without_background_notification() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let launcher = WorkflowRegistryLauncher::new(
            registry.clone(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "session-a",
        );

        let status = launcher
            .launch_workflow(
                WorkflowLaunchSpec {
                    script: Some(
                        "export const meta = { name: 'fail-demo', description: 'Fail demo' };\n\
                         throw new Error('boom');"
                            .into(),
                    ),
                    ..WorkflowLaunchSpec::default()
                },
                &ToolContext::new(),
            )
            .await
            .expect("launch workflow");

        assert_eq!(status.status, "failed");
        assert!(status.error.as_deref().unwrap_or_default().contains("boom"));
        assert!(status.result.is_none());
        let snapshot = registry
            .snapshot(&TaskId::new(status.task_id.clone().expect("task id")))
            .expect("workflow task snapshot");
        assert_eq!(snapshot.status, TaskStatus::Failed);
        assert!(snapshot.notified);
        assert!(registry
            .unnotified_terminal_agent_notifications()
            .is_empty());
    }

    #[tokio::test]
    async fn launcher_status_running_caps_and_bounds_projection() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let launcher = WorkflowRegistryLauncher::new(
            registry.clone(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "session-a",
        );
        let long = "\"\\\nvalue".repeat(2_000);
        for index in 0..10 {
            register_local_workflow_task(
                &registry,
                LocalWorkflowTaskSpec {
                    id: TaskId::new(format!("workflow-{index}")),
                    run_id: format!("wf-{index}-{long}"),
                    workflow_name: long.clone(),
                    summary: Some(long.clone()),
                    agent_count: 0,
                    output_path: Some(long.clone()),
                    script_path: Some(long.clone()),
                    args: Some(json!({ "payload": long.clone() })),
                    parent_session_id: None,
                    parent_tool_call_id: None,
                    is_backgrounded: true,
                },
                PromptCancel::new(),
            )
            .unwrap();
        }

        let status = launcher
            .launch_workflow(
                WorkflowLaunchSpec {
                    status: Some("running".into()),
                    ..WorkflowLaunchSpec::default()
                },
                &ToolContext::new(),
            )
            .await
            .expect("bounded running status");
        let result = status.result.as_ref().expect("running status result");
        let workflows = result["workflows"].as_array().expect("running workflows");

        assert_eq!(result["count"], 10);
        assert_eq!(result["truncated"], true);
        assert_eq!(workflows.len(), WORKFLOW_STATUS_MAX_RUNNING);
        assert!(result.to_string().chars().count() < 131_072);
        assert!(workflows.iter().all(|workflow| {
            workflow["args"] == json!({ "truncated": true })
                && serde_json::to_string(&workflow["runId"])
                    .unwrap()
                    .chars()
                    .count()
                    <= 98
                && serde_json::to_string(&workflow["workflowName"])
                    .unwrap()
                    .chars()
                    .count()
                    <= 194
                && serde_json::to_string(&workflow["summary"])
                    .unwrap()
                    .chars()
                    .count()
                    <= 386
        }));

        let missing = launcher
            .launch_workflow(
                WorkflowLaunchSpec {
                    status: Some("running".into()),
                    resume_from_run_id: Some(long),
                    ..WorkflowLaunchSpec::default()
                },
                &ToolContext::new(),
            )
            .await
            .expect("missing running status");
        assert_eq!(missing.status, "not_found");
        assert!(missing
            .run_id
            .as_ref()
            .is_some_and(|run_id| serde_json::to_string(run_id).unwrap().chars().count() <= 98));
        assert!(missing.warning.as_ref().is_some_and(|warning| {
            serde_json::to_string(warning).unwrap().chars().count() <= 386
        }));
    }

    #[tokio::test]
    async fn launcher_status_running_returns_live_workflow_progress() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let launcher = WorkflowRegistryLauncher::new(
            registry.clone(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "session-a",
        );
        let task_id = generate_task_id(TaskKind::LocalWorkflow);
        register_local_workflow_task(
            &registry,
            LocalWorkflowTaskSpec {
                id: task_id.clone(),
                run_id: "wf_running".into(),
                workflow_name: "live-demo".into(),
                summary: Some("Live demo".into()),
                agent_count: 0,
                output_path: Some(temp.path().join("transcript").display().to_string()),
                script_path: Some(temp.path().join("script.js").display().to_string()),
                args: Some(json!({"task":"inspect"})),
                parent_session_id: None,
                parent_tool_call_id: None,
                is_backgrounded: true,
            },
            PromptCancel::new(),
        )
        .unwrap();
        push_local_workflow_progress(
            &registry,
            &task_id,
            WorkflowProgressEntry::Phase {
                title: "Research".into(),
                state: "start".into(),
                phase_id: None,
            },
        );
        push_local_workflow_progress(
            &registry,
            &task_id,
            WorkflowProgressEntry::Agent {
                index: 1,
                state: "start".into(),
                phase_title: Some("Research".into()),
                phase_id: None,
                label: "finder".into(),
                tokens: 0,
                tool_calls: 0,
                tool_call_details: Vec::new(),
                duration_ms: None,
                error: None,
                agent_id: None,
            },
        );

        let status = launcher
            .launch_workflow(
                WorkflowLaunchSpec {
                    status: Some("running".into()),
                    resume_from_run_id: Some("wf_running".into()),
                    ..WorkflowLaunchSpec::default()
                },
                &ToolContext::new(),
            )
            .await
            .expect("running status query");

        assert_eq!(status.status, "running");
        assert_eq!(status.task_id.as_deref(), Some(task_id.as_str()));
        assert_eq!(status.run_id.as_deref(), Some("wf_running"));
        assert_eq!(status.summary.as_deref(), Some("Live demo"));
        assert_eq!(status.agent_count, Some(1));
        let progress = status
            .workflow_progress
            .as_ref()
            .expect("workflow progress");
        assert_eq!(progress["entries"].as_array().expect("entries").len(), 2);
        assert_eq!(progress["entries"][0]["entry"]["type"], "phase");
        assert_eq!(progress["entries"][1]["entry"]["label"], "finder");
        assert_eq!(status.result.as_ref().unwrap()["count"], 1);

        let status = launcher
            .launch_workflow(
                WorkflowLaunchSpec {
                    status: Some(" running ".into()),
                    name: Some("missing-demo".into()),
                    ..WorkflowLaunchSpec::default()
                },
                &ToolContext::new(),
            )
            .await
            .expect("filtered running status query");
        assert_eq!(status.status, "idle");
        assert_eq!(status.result.as_ref().unwrap()["count"], 0);
    }

    #[tokio::test]
    async fn launcher_resume_loads_snapshot_from_workflow_dir() {
        let temp = TempDir::new().expect("tempdir");
        let launcher = WorkflowRegistryLauncher::new(
            TaskRegistry::default(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "session-a",
        );
        let workflows_dir = temp
            .path()
            .join("sessions")
            .join("session-a")
            .join("workflows");
        fs::create_dir_all(&workflows_dir).expect("workflow dir");
        fs::write(
            workflows_dir.join("wf_resume.json"),
            serde_json::to_string(&json!({
                "script": "export const meta = { name: 'resume-demo', description: 'Resume demo' };\nreturn args;",
                "scriptPath": workflows_dir.join("resume-demo_wf_resume.js").display().to_string(),
                "status": "completed",
            }))
            .expect("snapshot json"),
        )
        .expect("write snapshot");

        let resolved = launcher
            .resolve_script(&WorkflowLaunchSpec {
                resume_from_run_id: Some("wf_resume".into()),
                ..WorkflowLaunchSpec::default()
            })
            .expect("resolve resume");

        assert_eq!(resolved.meta.name, "resume-demo");
        assert_eq!(resolved.source, "resume");
    }

    #[test]
    fn parse_workflow_meta_extracts_literal_meta_and_body() {
        let parsed = parse_workflow_meta(
            "export const meta = { name: 'demo', description: 'Demo', title: 'Run', phases: [{ title: 'One', detail: 'Do it' }], defaultModel: 'gpt' };\nlog('x');",
        )
        .expect("parse");
        assert_eq!(parsed.meta.name, "demo");
        assert_eq!(parsed.meta.phases[0].title, "One");
        assert_eq!(parsed.meta.default_model.as_deref(), Some("gpt"));
        assert_eq!(parsed.script_body, "log('x');");
    }

    #[test]
    fn parse_workflow_meta_accepts_string_phase_shortcuts() {
        let parsed = parse_workflow_meta(
            "export const meta = { name: 'demo', description: 'Demo', phases: ['Discover', { title: 'Test', detail: 'Run checks' }] };",
        )
        .expect("parse");
        assert_eq!(parsed.meta.phases[0].title, "Discover");
        assert_eq!(parsed.meta.phases[1].title, "Test");
        assert_eq!(parsed.meta.phases[1].detail.as_deref(), Some("Run checks"));
    }

    #[test]
    fn builtin_workflows_are_multi_agent_with_verification_stage() {
        let workflows = builtin_workflows();
        assert_eq!(workflows.len(), 3);
        for workflow in &workflows {
            assert_eq!(workflow.source, "built-in");
            assert!(
                workflow.meta.phases.len() >= 2,
                "built-in workflow {} must declare multiple phases",
                workflow.meta.name
            );
            let calls = summarize_workflow_static_calls(&workflow.script);
            let agent_calls = calls.iter().filter(|call| call.kind == "agent").count();
            assert!(
                agent_calls >= 2,
                "built-in workflow {} must orchestrate multiple agents, found {agent_calls}",
                workflow.meta.name
            );
            assert!(
                workflow
                    .meta
                    .phases
                    .iter()
                    .any(|phase| matches!(phase.title.as_str(), "Verify" | "Vote")),
                "built-in workflow {} must include a verification phase",
                workflow.meta.name
            );
            if workflow.meta.name == "plan-hunter" {
                assert!(workflow.script.contains("args.plan"));
                assert!(workflow.script.contains("requirements"));
                assert!(workflow.script.contains("requirement coverage"));
                assert!(workflow.script.contains("requirementId"));
            }
            for phase in &workflow.meta.phases {
                assert!(
                    workflow
                        .script
                        .contains(&format!("phase: '{}'", phase.title))
                        || workflow
                            .script
                            .contains(&format!("phase('{}')", phase.title)),
                    "built-in workflow {} declares phase {} but never enters it",
                    workflow.meta.name,
                    phase.title
                );
            }
        }
    }

    #[tokio::test]
    async fn plan_hunter_confirms_gap_when_requirement_assessment_is_omitted() {
        let temp = TempDir::new().expect("tempdir");
        let run = workflow_test_run_with_args(
            &temp,
            "wf_plan_hunter_omitted_req",
            ToolContext::new().with_sub_agent_spawner(Arc::new(PlanHunterCoverageSpawner {
                omit_second_requirement: true,
                second_status: "covered",
            })),
            Vec::new(),
            Some(json!({
                "plan": "Implement REQ-A only.",
                "requirements": [
                    {"id":"REQ-A", "text":"first requirement"},
                    {"id":"REQ-B", "text":"second requirement"}
                ]
            })),
        );
        let parsed = parse_workflow_meta(BUILTIN_PLAN_HUNTER).expect("plan hunter parses");

        let result = ScriptRuntime::new(
            &run,
            parsed.script_body,
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("plan hunter result");

        let output = &result["result"];
        assert_eq!(output["confirmed"], 1);
        assert_eq!(output["rejected"], 0);
        assert_eq!(
            output["requirementCoverage"]["providedIds"],
            json!(["REQ-A", "REQ-B"])
        );
        assert_eq!(
            output["requirementCoverage"]["assessedIds"],
            json!(["REQ-A"])
        );
        assert_eq!(
            output["requirementCoverage"]["missingAssessments"],
            json!(["REQ-B"])
        );
        assert_eq!(output["requirementCoverage"]["covered"], json!(["REQ-A"]));
        assert_eq!(output["requirementCoverage"]["unknown"], json!(["REQ-B"]));
        assert!(output["hardenedPlan"]
            .as_str()
            .expect("hardened plan text")
            .contains("Requirement coverage summary"));
    }

    #[tokio::test]
    async fn plan_hunter_returns_complete_summary_when_all_requirements_are_covered() {
        let temp = TempDir::new().expect("tempdir");
        let run = workflow_test_run_with_args(
            &temp,
            "wf_plan_hunter_all_covered",
            ToolContext::new().with_sub_agent_spawner(Arc::new(PlanHunterCoverageSpawner {
                omit_second_requirement: false,
                second_status: "covered",
            })),
            Vec::new(),
            Some(json!({
                "plan": "Implement REQ-A and REQ-B.",
                "requirements": {
                    "REQ-A": "first requirement",
                    "REQ-B": "second requirement"
                }
            })),
        );
        let parsed = parse_workflow_meta(BUILTIN_PLAN_HUNTER).expect("plan hunter parses");

        let result = ScriptRuntime::new(
            &run,
            parsed.script_body,
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("plan hunter result");

        let output = &result["result"];
        assert_eq!(output["confirmed"], 0);
        assert_eq!(output["rejected"], 0);
        assert_eq!(
            output["requirementCoverage"]["providedIds"],
            json!(["REQ-A", "REQ-B"])
        );
        assert_eq!(
            output["requirementCoverage"]["assessedIds"],
            json!(["REQ-A", "REQ-B"])
        );
        assert_eq!(
            output["requirementCoverage"]["missingAssessments"],
            json!([])
        );
        assert_eq!(
            output["requirementCoverage"]["covered"],
            json!(["REQ-A", "REQ-B"])
        );
        assert_eq!(output["requirementCoverage"]["partial"], json!([]));
        assert_eq!(output["requirementCoverage"]["missing"], json!([]));
        assert_eq!(output["requirementCoverage"]["unknown"], json!([]));
        assert_eq!(
            output["requirementCoverage"]["rows"]
                .as_array()
                .expect("rows array")
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn plan_hunter_extracts_requirement_ids_from_string_ledger() {
        let temp = TempDir::new().expect("tempdir");
        let run = workflow_test_run_with_args(
            &temp,
            "wf_plan_hunter_string_requirements",
            ToolContext::new().with_sub_agent_spawner(Arc::new(PlanHunterCoverageSpawner {
                omit_second_requirement: false,
                second_status: "covered",
            })),
            Vec::new(),
            Some(json!({
                "plan": "Implement both requirements.",
                "requirements": "- REQ-A: first requirement\n- REQ-B: second requirement"
            })),
        );
        let parsed = parse_workflow_meta(BUILTIN_PLAN_HUNTER).expect("plan hunter parses");

        let result = ScriptRuntime::new(
            &run,
            parsed.script_body,
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("plan hunter result");

        let output = &result["result"];
        assert_eq!(output["confirmed"], 0);
        assert_eq!(
            output["requirementCoverage"]["providedIds"],
            json!(["REQ-A", "REQ-B"])
        );
        assert_eq!(
            output["requirementCoverage"]["assessedIds"],
            json!(["REQ-A", "REQ-B"])
        );
        assert_eq!(
            output["requirementCoverage"]["missingAssessments"],
            json!([])
        );
    }

    #[test]
    fn workflow_review_renders_meta_phases_and_string_phases() {
        let review = WorkflowReview::builder(
            "export const meta = { name: 'demo', description: 'Demo flow', phases: ['Discover', { title: 'Build', detail: 'Implement it', model: 'm1' }] };\nawait agent('do work');",
        )
        .source("inline")
        .args(Some(json!({"task":"ship"})))
        .build();

        assert_eq!(review.name, "demo");
        assert_eq!(review.phases.len(), 2);
        assert_eq!(review.phases[0].title, "Discover");
        assert_eq!(review.phases[1].detail.as_deref(), Some("Implement it"));
        let text = review.render_text();
        assert!(text.contains("Overview"));
        assert!(text.contains("Phases"));
        assert!(text.contains("1. Discover"));
        assert!(text.contains("2. Build [model: m1]"));
        assert!(text.contains("Args: {\"task\":\"ship\"}"));
    }

    /// `\n` in a prompt literal must decode to a newline, not a literal "n" —
    /// the display pipeline flattens real newlines to spaces, but a dropped
    /// backslash corrupts the text ("nn检查角度").
    #[test]
    fn js_string_literal_decodes_common_escapes() {
        assert_eq!(
            parse_js_string_literal("'a\\nb\\tc'").as_deref(),
            Some("a\nb\tc")
        );
        assert_eq!(
            parse_js_string_literal("`x\\n\\n${y}`").as_deref(),
            Some("x\n\n${y}")
        );
        assert_eq!(
            parse_js_string_literal("\"q\\\"e\"").as_deref(),
            Some("q\"e")
        );
    }

    #[test]
    fn workflow_review_summarizes_static_calls() {
        let review = WorkflowReview::builder(
            r#"export const meta = { name: 'demo', description: 'Demo', phases: [{ title: 'Run' }] };
log('start');
await phase('Run', async () => {
  const first = await agent({ prompt: 'inspect workflow', model: 'm1', provider: 'p1', phase: 'Run', schema: { type: 'object' } });
  const named = await agent({ prompt: 'named schema', schema: REVIEW_SCHEMA });
  await parallel([
    () => agent('one'),
    () => agent('two')
  ]);
  await pipeline([
    () => agent('step one'),
    previous => agent(`step two ${previous}`)
  ]);
  await workflow('child', { task: 'nested' });
});"#,
        )
        .build();

        assert!(review
            .calls
            .iter()
            .any(|call| call.kind == "phase" && call.summary.contains("Run")));
        assert!(review
            .calls
            .iter()
            .any(|call| call.kind == "parallel" && call.summary.contains("2 branch")));
        assert!(review
            .calls
            .iter()
            .any(|call| call.kind == "pipeline" && call.summary.contains("2 step")));
        assert!(review
            .calls
            .iter()
            .any(|call| call.kind == "workflow" && call.summary.contains("child")));
        assert!(review
            .calls
            .iter()
            .any(|call| call.kind == "log" && call.summary.contains("start")));
        let agent_call = review
            .calls
            .iter()
            .find(|call| call.kind == "agent" && call.summary.contains("model=m1"))
            .expect("agent call summary");
        assert_eq!(agent_call.phase.as_deref(), Some("Run"));
        assert!(agent_call.has_schema);
        assert!(agent_call.summary.contains("phase=Run"));
        assert!(agent_call.summary.contains("schema=present"));
        let details = agent_call.agent.as_ref().expect("structured agent details");
        assert_eq!(details.model.as_deref(), Some("m1"));
        assert_eq!(details.provider.as_deref(), Some("p1"));
        assert_eq!(details.phase_title.as_deref(), Some("Run"));
        assert_eq!(details.prompt.as_deref(), Some("inspect workflow"));
        assert!(details.has_schema);
        // Inline object literal → anonymous schema, no name to review.
        assert_eq!(details.schema_name, None);
        let named_agent = review
            .calls
            .iter()
            .find(|call| call.kind == "agent" && call.summary.contains("named schema"))
            .expect("named schema agent call");
        let named_details = named_agent.agent.as_ref().expect("named schema details");
        assert!(named_details.has_schema);
        assert_eq!(named_details.schema_name.as_deref(), Some("REVIEW_SCHEMA"));
        let bare_agent = review
            .calls
            .iter()
            .find(|call| call.kind == "agent" && call.summary.contains("\"one\""))
            .expect("bare agent call");
        let bare_details = bare_agent.agent.as_ref().expect("bare agent details");
        assert_eq!(bare_details.model, None);
        assert_eq!(bare_details.prompt.as_deref(), Some("one"));
        let text = review.render_text();
        assert!(text.contains("Execution graph"));
        assert!(text.contains("Agent calls"));
        assert!(text.contains("Script details"));
        assert!(text.contains("Script excerpt"));
        assert!(text.contains("Mermaid source"));
        let overview_idx = text.find("Overview").expect("Overview section");
        let phases_idx = text.find("Phases").expect("Phases section");
        let agent_calls_idx = text.find("Agent calls").expect("Agent calls section");
        let script_details_idx = text.find("Script details").expect("Script details section");
        let script_excerpt_idx = text.find("Script excerpt").expect("Script excerpt section");
        assert!(overview_idx < phases_idx);
        assert!(phases_idx < agent_calls_idx);
        assert!(agent_calls_idx < script_details_idx);
        assert!(script_details_idx < script_excerpt_idx);
    }

    #[test]
    fn workflow_review_warns_when_review_style_workflow_lacks_verification() {
        let review = WorkflowReview::builder(
            "export const meta = { name: 'quick-review', description: 'Review the diff for bugs', phases: [{ title: 'Find' }] };\nphase('Find');\nawait parallel([() => agent('find bugs in module a'), () => agent('find bugs in module b')]);",
        )
        .source("inline")
        .build();

        assert!(
            review
                .warnings
                .iter()
                .any(|warning| warning.contains("no verification stage")),
            "expected verification warning, got: {:?}",
            review.warnings
        );
    }

    #[test]
    fn workflow_review_verification_warning_skips_verified_and_non_review_scripts() {
        let verified = WorkflowReview::builder(
            "export const meta = { name: 'quick-review', description: 'Review the diff for bugs', phases: [{ title: 'Find' }, { title: 'Check' }] };\nconst found = await parallel([() => agent('find bugs in module a'), () => agent('find bugs in module b')]);\nawait agent('adversarially refute these findings: ' + json(found));",
        )
        .source("inline")
        .build();
        assert!(
            !verified
                .warnings
                .iter()
                .any(|warning| warning.contains("no verification stage")),
            "verified script should not warn: {:?}",
            verified.warnings
        );

        let migration = WorkflowReview::builder(
            "export const meta = { name: 'rename-symbols', description: 'Rename the legacy API across crates', phases: [{ title: 'Apply' }] };\nawait parallel([() => agent('rename in crate a'), () => agent('rename in crate b')]);",
        )
        .source("inline")
        .build();
        assert!(
            !migration
                .warnings
                .iter()
                .any(|warning| warning.contains("no verification stage")),
            "non-review script should not warn: {:?}",
            migration.warnings
        );
    }

    #[test]
    fn workflow_review_falls_back_for_invalid_script() {
        let review = WorkflowReview::builder("log('missing meta');\nawait agent('x');")
            .source("inline")
            .fallback_name("bad-demo")
            .fallback_description("Broken workflow")
            .build();

        assert_eq!(review.name, "bad-demo");
        assert!(!review.errors.is_empty());
        let text = review.render_text();
        assert!(text.contains("Warnings/errors"));
        assert!(text.contains("meta"));
        assert!(text.contains("Script excerpt"));
    }

    #[tokio::test]
    async fn js_runtime_serializes_object_prompts_as_json() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let task_id = generate_task_id(TaskKind::LocalWorkflow);
        let cancel = PromptCancel::new();
        register_local_workflow_task(
            &registry,
            LocalWorkflowTaskSpec {
                id: task_id.clone(),
                run_id: "wf_json_prompt".into(),
                workflow_name: "demo".into(),
                summary: None,
                agent_count: 0,
                output_path: None,
                script_path: None,
                args: Some(json!({"task":"ship","checks":["fmt","test"]})),
                parent_session_id: None,
                parent_tool_call_id: None,
                is_backgrounded: true,
            },
            cancel.clone(),
        )
        .unwrap();
        let run = WorkflowRun {
            registry,
            task_id,
            run_id: "wf_json_prompt".into(),
            transcript_dir: temp.path().join("transcript"),
            persisted_script: temp.path().join("script.js"),
            cwd: temp.path().to_path_buf(),
            script: "export const meta = { name: 'demo', description: 'Demo' };".into(),
            meta: WorkflowMeta {
                name: "demo".into(),
                description: "Demo".into(),
                title: None,
                when_to_use: None,
                phases: Vec::new(),
                default_model: None,
            },
            args: Some(json!({"task":"ship","checks":["fmt","test"]})),
            context: ToolContext::new().with_sub_agent_spawner(Arc::new(TestSpawner)),
            cancel,
            depth: 0,
            permission_prompts_unavailable: false,
            nested_workflows: Vec::new(),
            budget_total: None,
            agent_concurrency_cap: None,
        };
        let result = ScriptRuntime::new(
            &run,
            "return await agent(args);".into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");

        let final_text = result["result"].as_str().expect("final text");
        assert!(!final_text.contains("[object Object]"));
        assert!(final_text.contains("\"task\": \"ship\""));
        assert!(final_text.contains("\"checks\""));
    }

    #[tokio::test]
    async fn js_runtime_accepts_agent_object_call_with_prompt_and_options() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let task_id = generate_task_id(TaskKind::LocalWorkflow);
        let cancel = PromptCancel::new();
        register_local_workflow_task(
            &registry,
            LocalWorkflowTaskSpec {
                id: task_id.clone(),
                run_id: "wf_object_call".into(),
                workflow_name: "demo".into(),
                summary: None,
                agent_count: 0,
                output_path: None,
                script_path: None,
                args: None,
                parent_session_id: None,
                parent_tool_call_id: None,
                is_backgrounded: true,
            },
            cancel.clone(),
        )
        .unwrap();
        let run = WorkflowRun {
            registry,
            task_id,
            run_id: "wf_object_call".into(),
            transcript_dir: temp.path().join("transcript"),
            persisted_script: temp.path().join("script.js"),
            cwd: temp.path().to_path_buf(),
            script: "export const meta = { name: 'demo', description: 'Demo' };".into(),
            meta: WorkflowMeta {
                name: "demo".into(),
                description: "Demo".into(),
                title: None,
                when_to_use: None,
                phases: Vec::new(),
                default_model: None,
            },
            args: None,
            context: ToolContext::new().with_sub_agent_spawner(Arc::new(TestSpawner)),
            cancel,
            depth: 0,
            permission_prompts_unavailable: false,
            nested_workflows: Vec::new(),
            budget_total: None,
            agent_concurrency_cap: None,
        };
        let result = ScriptRuntime::new(
            &run,
            r#"
const first = await agent({ prompt: 'inspect workflow', model: 'm1', provider: 'p1' });
const results = await pipeline(
  ['one', 'two'],
  item => agent(item, { label: `first:${item}`, phase: 'One' }),
  prev => prev
);
return { first, results };
"#
            .into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");

        assert_eq!(result["result"]["first"], "done: inspect workflow");
        assert_eq!(
            result["result"]["results"],
            json!(["done: one", "done: two"])
        );
    }

    #[tokio::test]
    async fn js_runtime_returns_structured_output_for_schema_agent() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let task_id = generate_task_id(TaskKind::LocalWorkflow);
        let cancel = PromptCancel::new();
        register_local_workflow_task(
            &registry,
            LocalWorkflowTaskSpec {
                id: task_id.clone(),
                run_id: "wf_schema".into(),
                workflow_name: "demo".into(),
                summary: None,
                agent_count: 0,
                output_path: None,
                script_path: None,
                args: None,
                parent_session_id: None,
                parent_tool_call_id: None,
                is_backgrounded: true,
            },
            cancel.clone(),
        )
        .unwrap();
        let run = WorkflowRun {
            registry,
            task_id,
            run_id: "wf_schema".into(),
            transcript_dir: temp.path().join("transcript"),
            persisted_script: temp.path().join("script.js"),
            cwd: temp.path().to_path_buf(),
            script: "export const meta = { name: 'demo', description: 'Demo' };".into(),
            meta: WorkflowMeta {
                name: "demo".into(),
                description: "Demo".into(),
                title: None,
                when_to_use: None,
                phases: Vec::new(),
                default_model: None,
            },
            args: None,
            context: ToolContext::new().with_sub_agent_spawner(Arc::new(TestSpawner)),
            cancel,
            depth: 0,
            permission_prompts_unavailable: false,
            nested_workflows: Vec::new(),
            budget_total: None,
            agent_concurrency_cap: None,
        };
        let result = ScriptRuntime::new(
            &run,
            r#"
return await agent('schema prompt', {
  label: 'schema-agent',
  phase: 'Run',
  schema: {
    type: 'object',
    required: ['answer', 'ok'],
    properties: { answer: { type: 'string' }, ok: { type: 'boolean' } }
  }
});
"#
            .into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");

        assert_eq!(result["result"]["answer"], "schema prompt");
        assert_eq!(result["result"]["ok"], true);
    }

    #[tokio::test]
    async fn js_runtime_uses_final_text_fallback_for_schema_agent_without_structured_output() {
        let temp = TempDir::new().expect("tempdir");
        let registry = TaskRegistry::default();
        let task_id = generate_task_id(TaskKind::LocalWorkflow);
        let cancel = PromptCancel::new();
        register_local_workflow_task(
            &registry,
            LocalWorkflowTaskSpec {
                id: task_id.clone(),
                run_id: "wf_schema_fallback".into(),
                workflow_name: "demo".into(),
                summary: None,
                agent_count: 0,
                output_path: None,
                script_path: None,
                args: None,
                parent_session_id: None,
                parent_tool_call_id: None,
                is_backgrounded: true,
            },
            cancel.clone(),
        )
        .unwrap();
        let run = WorkflowRun {
            registry,
            task_id,
            run_id: "wf_schema_fallback".into(),
            transcript_dir: temp.path().join("transcript"),
            persisted_script: temp.path().join("script.js"),
            cwd: temp.path().to_path_buf(),
            script: "export const meta = { name: 'demo', description: 'Demo' };".into(),
            meta: WorkflowMeta {
                name: "demo".into(),
                description: "Demo".into(),
                title: None,
                when_to_use: None,
                phases: Vec::new(),
                default_model: None,
            },
            args: None,
            context: ToolContext::new().with_sub_agent_spawner(Arc::new(FinalTextJsonSpawner)),
            cancel,
            depth: 0,
            permission_prompts_unavailable: false,
            nested_workflows: Vec::new(),
            budget_total: None,
            agent_concurrency_cap: None,
        };
        let result = ScriptRuntime::new(
            &run,
            r#"
return await agent('schema fallback', {
  label: 'schema-agent',
  phase: 'Run',
  schema: {
    type: 'object',
    required: ['answer', 'ok'],
    properties: { answer: { type: 'string' }, ok: { type: 'boolean' } }
  }
});
"#
            .into(),
            WorkflowJournal::new(temp.path().join("journal.jsonl")),
            HashMap::new(),
        )
        .run()
        .await
        .expect("runtime result");

        assert_eq!(result["result"]["answer"], "schema fallback");
        assert_eq!(result["result"]["ok"], true);
    }

    #[test]
    fn parse_workflow_meta_rejects_non_first_meta_and_reserved_keys() {
        assert!(parse_workflow_meta("log('x'); export const meta = {}").is_err());
        assert!(parse_workflow_meta(
            "export const meta = { name: 'x', description: 'y', __proto__: 'z' };"
        )
        .is_err());
    }

    #[test]
    fn determinism_scan_ignores_strings_and_comments() {
        assert!(!workflow_contains_nondeterminism(
            "log('Date.now Math.random new Date') // Date.now\n/* Math.random */"
        ));
        assert!(workflow_contains_nondeterminism("const x = Date.now();"));
        assert!(workflow_contains_nondeterminism("const y = Math.random();"));
        assert!(workflow_contains_nondeterminism("const z = new Date();"));
        assert!(workflow_contains_nondeterminism("const z = Date['now']();"));
    }

    #[test]
    fn workflow_subagent_prompts_protect_shared_worktree_changes() {
        let plain = workflow_subagent_system_prompt(None);
        assert!(plain.contains("SHARED WORKTREE SAFETY:"));
        assert!(plain.contains("other agents"));
        assert!(plain.contains("Never use git reset"));

        let structured = workflow_subagent_system_prompt(Some(&json!({
            "type": "object",
            "required": ["answer"]
        })));
        assert!(structured.contains("SHARED WORKTREE SAFETY:"));
        assert!(structured.contains("FINAL DELIVERY CONTRACT:"));
        assert!(structured.contains("JSON schema:"));
        assert!(structured.find("SHARED WORKTREE SAFETY:") < structured.find("JSON schema:"));
    }

    #[test]
    fn workflow_subagent_spec_inherits_parent_permission_broker() {
        let broker: Arc<dyn rebon_tool::PermissionBroker> =
            Arc::new(rebon_tool::AutoApprovePermissionBroker);
        let context = ToolContext::new().with_permission_broker(broker.clone());
        let spec =
            prepare_workflow_subagent_spec(&context, "edit safely".into(), 1, true, None, None)
                .expect("workflow subagent spec");

        assert_eq!(spec.workflow_nesting_depth, 1);
        assert!(spec.permission_prompts_unavailable);
        assert!(Arc::ptr_eq(
            &broker,
            spec.permission_broker.as_ref().expect("permission broker")
        ));
        assert!(spec
            .system
            .as_deref()
            .unwrap_or_default()
            .contains("SHARED WORKTREE SAFETY:"));
    }

    #[test]
    fn structured_workflow_agents_get_structured_output_without_restricting_tools() {
        let policy = workflow_agent_execution_policy(&ToolContext::new(), true)
            .expect("structured output policy");
        assert!(policy
            .eager_promotions
            .iter()
            .any(|tool| tool == STRUCTURED_OUTPUT_TOOL_NAME));

        // Even without a parent policy, workflow children are continuity
        // turns: the auto-mode gate denies-with-reason instead of parking
        // the agent on an interactive prompt nobody is watching.
        let policy = workflow_agent_execution_policy(&ToolContext::new(), false)
            .expect("workflow children always carry a continuity policy");
        assert!(policy.auto_mode_script_continuity);
        assert!(policy.ultraplan.is_none());
        assert!(policy.eager_promotions.is_empty());
    }

    #[test]
    fn ultrawork_workflow_children_get_writable_plan_fidelity_policy() {
        let parent = rebon_types::ExecutionPolicy::ultrawork_execution_controller("run-1");
        let policy = workflow_agent_execution_policy(
            &ToolContext::new().with_execution_policy(parent),
            false,
        )
        .expect("child execution policy");
        assert!(policy.auto_mode_script_continuity);
        let ctx = policy.ultraplan.as_ref().expect("ultraplan context");

        assert_eq!(ctx.run_id, "run-1");
        assert_eq!(ctx.phase, "ultrawork_child");
        assert!(ctx.local_only);
        assert!(ctx.plan_fidelity);
        assert!(!ctx.read_only);
        assert_eq!(ctx.shell_policy, rebon_types::ShellPolicy::AllowShell);
        assert!(ctx.allowed_tools.iter().any(|tool| tool == "Edit"));
        assert!(ctx.allowed_tools.iter().any(|tool| tool == "Write"));
        assert!(ctx.allowed_tools.iter().any(|tool| tool == "Bash"));
        assert!(!ctx.denied_tools.iter().any(|tool| tool == "Edit"));
        assert!(!ctx.denied_tools.iter().any(|tool| tool == "Write"));
        assert!(!ctx.denied_tools.iter().any(|tool| tool == "Bash"));
        assert!(ctx.denied_tools.iter().any(|tool| tool == "Agent"));
        assert!(ctx.denied_tools.iter().any(|tool| tool == "TeamCreate"));
        assert!(ctx.denied_tools.iter().any(|tool| tool == "TeamDelete"));
        assert!(ctx.denied_tools.iter().any(|tool| tool == "SendMessage"));
    }

    #[test]
    fn workflow_controller_children_get_writable_exploration_policy() {
        let parent = rebon_types::ExecutionPolicy::workflow_controller();
        let policy = workflow_agent_execution_policy(
            &ToolContext::new().with_execution_policy(parent),
            false,
        )
        .expect("child execution policy");
        assert!(policy.auto_mode_script_continuity);
        let ctx = policy.ultraplan.as_ref().expect("ultraplan context");

        // The controller is a read-only scout/orchestrator, but its
        // workflow children do the actual work: write + shell, and no
        // plan-fidelity contract (there is no approved plan, so workers
        // must be free to explore the repository).
        assert_eq!(ctx.phase, "workflow_child");
        assert!(!ctx.read_only);
        assert!(!ctx.plan_fidelity);
        assert_eq!(ctx.shell_policy, rebon_types::ShellPolicy::AllowShell);
        assert!(ctx.allowed_tools.iter().any(|tool| tool == "Edit"));
        assert!(ctx.allowed_tools.iter().any(|tool| tool == "Write"));
        assert!(ctx.allowed_tools.iter().any(|tool| tool == "Bash"));
        assert!(!ctx.denied_tools.iter().any(|tool| tool == "Edit"));
        assert!(!ctx.denied_tools.iter().any(|tool| tool == "Write"));
        assert!(!ctx.denied_tools.iter().any(|tool| tool == "Bash"));
        assert!(ctx.denied_tools.iter().any(|tool| tool == "Agent"));
        assert!(ctx.denied_tools.iter().any(|tool| tool == "TeamCreate"));
        assert!(ctx.denied_tools.iter().any(|tool| tool == "TeamDelete"));
        assert!(ctx.denied_tools.iter().any(|tool| tool == "SendMessage"));
    }

    #[test]
    fn goal_only_parents_propagate_script_continuity_to_workflow_children() {
        // A goal turn carries no ultraplan context, so the only thing
        // keeping its policy alive through the `is_active()` gate below
        // is `auto_mode_script_continuity`. That inheritance is the
        // contract: the goal turn itself may run scripts, and so must
        // the workflow children it spawns on its behalf.
        let parent = rebon_types::ExecutionPolicy::goal();
        let policy = workflow_agent_execution_policy(
            &ToolContext::new().with_execution_policy(parent),
            false,
        )
        .expect("goal continuity policy reaches workflow children");

        assert!(policy.auto_mode_script_continuity);
        assert!(policy.ultraplan.is_none());
        assert!(policy.eager_promotions.is_empty());
    }

    #[test]
    fn writable_workflow_agents_default_to_implementation_task_kind() {
        // Ultrawork execution children hold Edit/Write — they must be
        // labeled implementation workers so worktree isolation
        // (`coordinator.use_worktree`) can apply to them.
        let parent = rebon_types::ExecutionPolicy::ultraplan(
            rebon_types::UltraplanContext::ultrawork_execution_controller_turn(
                "run-1",
                rebon_types::PolicyMode::Enforce,
            ),
        );
        let mut spec = SubAgentSpec::new("implement the plan");
        spec.execution_policy = workflow_agent_execution_policy(
            &ToolContext::new().with_execution_policy(parent),
            false,
        );
        ensure_workflow_task_kind(&mut spec);
        assert_eq!(spec.task_kind, Some(SubAgentTaskKind::Implementation));
    }

    #[test]
    fn explicit_task_kind_option_wins_over_inference() {
        let parent = rebon_types::ExecutionPolicy::workflow_controller();
        let mut spec = SubAgentSpec::new("verify only");
        spec.execution_policy = workflow_agent_execution_policy(
            &ToolContext::new().with_execution_policy(parent),
            false,
        );
        apply_agent_options(&mut spec, Some(&json!({ "taskKind": "verification" })));
        ensure_workflow_task_kind(&mut spec);
        assert!(spec.task_kind.is_none());
        assert_eq!(
            spec.metadata.get("task_kind").and_then(Value::as_str),
            Some("verification")
        );
    }

    #[test]
    fn non_writable_workflow_agents_keep_default_task_kind() {
        let mut spec = SubAgentSpec::new("read-only analysis");
        ensure_workflow_task_kind(&mut spec);
        assert!(spec.task_kind.is_none());
        assert!(spec.metadata.get("task_kind").is_none());
    }

    #[test]
    fn workflow_agent_options_reject_handoff_data() {
        let error = validate_agent_options(Some(&json!({
            "label": "synthesize",
            "phase": "Synthesize",
            "notes": [{ "finding": "workflow result" }],
            "results": [{ "summary": "research" }],
        })))
        .expect_err("handoff data in opts should be rejected");

        assert!(error.contains("notes"));
        assert!(error.contains("results"));
        assert!(error.contains("prompt text"));
        assert!(error.contains("JSON.stringify"));
    }

    #[test]
    fn workflow_agent_options_reject_effort_and_point_to_model_profile() {
        let error = validate_agent_options(Some(&json!({
            "label": "verify",
            "phase": "Verify",
            "effort": "high",
        })))
        .expect_err("effort must not be accepted as workflow agent metadata");

        assert!(error.contains("Unsupported workflow agent option(s): effort"));
        assert!(error.contains("Supported options are"));
        assert!(error.contains("modelProfile"));
        assert!(error.contains("`effort` is not supported"));
    }

    #[test]
    fn model_visible_zero_agent_failure_is_explicitly_unexecuted() {
        let error = model_visible_workflow_failure(
            "Unsupported workflow agent option(s): effort",
            0,
            "wf_zero",
        );

        assert!(error.contains("failed before any agent calls ran"));
        assert!(error.contains("No requested implementation or verification was performed"));
        assert!(!error.contains("completed"));
    }

    #[test]
    fn workflow_agent_options_accept_control_metadata() {
        validate_agent_options(Some(&json!({
            "schema": { "type": "object" },
            "label": "finder",
            "phase": "Research",
            "model": "m1",
            "provider": "p1",
            "modelProfile": "large",
            "model_profile": "large",
            "isolation": "worktree",
            "agentType": "Explore",
            "agent_type": "Explore",
            "cwd": "/workspace/project",
            "maxIterations": 50,
            "max_iterations": 50,
        })))
        .expect("documented control metadata should be accepted");
    }

    #[test]
    fn workflow_agent_options_apply_isolation_metadata() {
        let mut spec = SubAgentSpec::new("work in isolation");
        apply_agent_options(
            &mut spec,
            Some(&json!({
                "label": "writer",
                "phase": "Implement",
                "isolation": "worktree",
                "agentType": "Explore",
            })),
        );

        assert_eq!(spec.prompt, "work in isolation");
        assert_eq!(spec.metadata["agent_type"], "Explore");
        assert_eq!(spec.metadata["isolation"], "worktree");
    }

    #[test]
    fn agent_call_key_is_stable_and_filters_irrelevant_opts() {
        let a = compute_agent_call_key("prompt", Some(&json!({"model":"m","noise":1})));
        let b = compute_agent_call_key("prompt", Some(&json!({"model":"m","noise":2})));
        let c = compute_agent_call_key("prompt", Some(&json!({"model":"other"})));
        let d = compute_agent_call_key("prompt", Some(&json!({"model":"m","provider":"p1"})));
        let e = compute_agent_call_key("prompt", Some(&json!({"model":"m","provider":"p2"})));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(d, e);
    }

    /// The launcher only holds the active provider's profile map, so the
    /// static `modelProfile` warning must not fire for calls routed to a
    /// different provider — those resolve against that provider's own
    /// profiles at run time.
    #[test]
    fn model_profile_warning_skips_calls_routed_to_other_providers() {
        let temp = TempDir::new().expect("tempdir");
        let launcher = WorkflowRegistryLauncher::new(
            TaskRegistry::default(),
            temp.path(),
            temp.path().join("config"),
            temp.path().join("sessions"),
            "sess-profile",
        )
        .with_model_profiles(rebon_types::ModelProfileMap::from_entries([(
            "large", "model-x",
        )]))
        .with_active_provider("openai");

        let call = |provider: Option<&str>, line: usize| WorkflowReviewCall {
            kind: "agent".into(),
            line,
            summary: format!("line {line}: prompt \"x\""),
            phase: None,
            has_schema: false,
            agent: Some(rebon_tools_core::WorkflowAgentNodeMeta {
                model_profile: Some("balanced".into()),
                provider: provider.map(str::to_string),
                ..Default::default()
            }),
        };
        let mut review = WorkflowReview {
            name: "demo".into(),
            title: None,
            source: "inline".into(),
            description: "demo".into(),
            args_summary: None,
            args: None,
            phases: Vec::new(),
            calls: vec![
                call(None, 1),
                call(Some("openai"), 2),
                call(Some("deepseek"), 3),
            ],
            script_excerpt: String::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
        };
        launcher.warn_on_unknown_model_profiles(&mut review);

        assert_eq!(review.warnings.len(), 2, "{:?}", review.warnings);
        assert!(review.warnings.iter().any(|w| w.starts_with("line 1:")));
        assert!(review.warnings.iter().any(|w| w.starts_with("line 2:")));
        assert!(!review.warnings.iter().any(|w| w.starts_with("line 3:")));
    }
}
