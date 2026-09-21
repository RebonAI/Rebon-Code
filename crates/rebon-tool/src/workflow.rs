//! The workflow domain types every side of a workflow launch shares.
//!
//! Both the `Workflow` tool and the runtime behind it live in the workflow
//! plugin. What stays here is what the plugin is not the only reader of: the
//! [`WorkflowLauncher`] trait the runtime implements, the launch spec /
//! status / preview it exchanges with the tool, the two tool-name constants
//! the run loop matches on, the [`WorkflowContext`] `ToolContext` carries,
//! the [`WORKFLOW_STRUCTURED_OUTPUT_SCHEMA_KEY`] the sub-agent spawner reads
//! back out of a spec's metadata, the [`WorkflowNesting`] policy the spawner
//! and the agent tools decide by, and the [`WorkflowLauncherService`] seat a
//! front end resolves the runtime off.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ToolContext;
use rebon_types::MAX_NESTED_WORKFLOW_DEPTH;
use std::sync::Arc;

/// Workflow state that [`ToolContext`] carries in its extension bag.
///
/// Storage only — read and written through the unchanged
/// `workflow_launcher()` / `workflow_nesting_depth()` accessors.
#[derive(Clone, Default)]
pub struct WorkflowContext {
    pub launcher: Option<Arc<dyn WorkflowLauncher>>,
    pub nesting_depth: usize,
}

pub const WORKFLOW_TOOL_NAME: &str = "Workflow";
pub const RUN_WORKFLOW_ALIAS: &str = "RunWorkflow";

/// Metadata key used to hand a workflow agent's structured-output JSON
/// schema to the spawner out-of-band. The spawner strips it before the
/// registry snapshot (like `__runtime_git`) so it never leaks into
/// `/tasks`, and uses it to install the `StructuredOutput` channel plus
/// post-turn coercion on the worker.
///
/// Here rather than with the runtime that writes it: the sub-agent spawner
/// is the reader, and a spawner may not depend on a plugin. One constant is
/// what keeps writer and reader from spelling it differently and losing
/// every schema silently.
pub const WORKFLOW_STRUCTURED_OUTPUT_SCHEMA_KEY: &str = "__structured_output_schema";

/// How deep inside a workflow a call is, and every decision that follows.
///
/// The depth itself lives on [`ToolContext`] because three crates stamp and
/// read it — the `Workflow` tool, the `Agent` tool, and the sub-agent
/// spawner. What used to be spread across those three were the *comparisons*:
/// `> 0` in five places, `> MAX_NESTED_WORKFLOW_DEPTH` in four, and one
/// `+ 1 >` that meant the same thing about a child. Each of them is one of
/// the four questions below, and each is asked here now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkflowNesting {
    depth: usize,
}

impl WorkflowNesting {
    /// The policy at an explicit depth — a spec's stamped depth, or a
    /// runtime's own counter.
    pub const fn at(depth: usize) -> Self {
        Self { depth }
    }

    /// The policy for the call this context describes.
    pub fn of(context: &ToolContext) -> Self {
        Self::at(context.workflow_nesting_depth())
    }

    pub const fn depth(self) -> usize {
        self.depth
    }

    /// One level deeper: what a sub-agent spawned from here would run at.
    pub const fn child(self) -> Self {
        Self::at(self.depth + 1)
    }

    /// Is this call inside a workflow at all?
    ///
    /// The question behind every "a workflow agent may not do this" rule:
    /// shared-worktree Git commands, unattended permission prompts, the
    /// coordinator's team lineage.
    pub const fn is_within_workflow(self) -> bool {
        self.depth > 0
    }

    /// May a workflow be launched at this depth?
    ///
    /// `MAX_NESTED_WORKFLOW_DEPTH` is a ceiling on nesting, not on the
    /// outermost run, so the comparison is `<=` and a run at exactly the
    /// ceiling is still allowed to start.
    pub const fn may_launch_workflow(self) -> bool {
        self.depth <= MAX_NESTED_WORKFLOW_DEPTH
    }

    /// A workflow sub-agent that may write and was not delegated outward.
    ///
    /// The case the runtime puts in its own worktree: inside a workflow
    /// (`is_within_workflow`), allowed to edit files, and running locally —
    /// an external delegation writes on someone else's machine, so the
    /// shared tree is not at risk.
    pub const fn forbids_shared_worktree_writes(
        self,
        may_write: bool,
        external_delegation: bool,
    ) -> bool {
        self.is_within_workflow() && may_write && !external_delegation
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowLaunchSpec {
    pub script: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub title: Option<String>,
    pub args: Option<Value>,
    pub script_path: Option<String>,
    pub resume_from_run_id: Option<String>,
    pub status: Option<String>,
    #[serde(default)]
    pub run_in_background: bool,
    /// Optional hard output-token ceiling for the whole run. Exposed to the
    /// script as `budget.total`; once `budget.spent()` (output tokens summed
    /// across all agents) reaches it, `agent()` throws.
    pub budget: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowLaunchStatus {
    pub status: String,
    #[serde(rename = "taskId", default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(rename = "workflowProgress", skip_serializing_if = "Option::is_none")]
    pub workflow_progress: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[async_trait]
pub trait WorkflowLauncher: Send + Sync {
    async fn launch_workflow(
        &self,
        spec: WorkflowLaunchSpec,
        context: &ToolContext,
    ) -> Result<WorkflowLaunchStatus, String>;

    async fn preview_workflow(
        &self,
        spec: WorkflowLaunchSpec,
        context: &ToolContext,
    ) -> Result<WorkflowPermissionPreview, String>;
}

/// Stable typed name of the seat a front end resolves a session's workflow
/// runtime off.
pub const WORKFLOW_LAUNCHER_SERVICE: &str = "workflow-launcher";

/// Typed definition for the kernel's `workflow-launcher` seat.
pub struct WorkflowLauncherService;

impl rebon_kernel::Service for WorkflowLauncherService {
    type Interface = dyn WorkflowLauncherSource;
    const NAME: &'static str = WORKFLOW_LAUNCHER_SERVICE;
}

/// The provider behind the seat: whatever can build a launcher for a session.
///
/// One provider, filled by the workflow plugin. With it switched off nothing
/// is on the seat, the front end attaches no launcher, and the
/// `Workflow` tool is off the tool seat in the same breath — the model cannot
/// ask for a workflow, and nothing is standing by to run one if it did.
pub trait WorkflowLauncherSource: Send + Sync {
    /// Build the launcher this session's `Workflow` calls go through.
    ///
    /// Called once per session, on the path that builds the executor. `Err`
    /// is a wiring fault (a handle of the wrong shape), not "no workflows
    /// here" — that case is the seat being empty.
    fn for_session(
        &self,
        request: WorkflowLauncherRequest,
    ) -> Result<Arc<dyn WorkflowLauncher>, String>;
}

/// What only the front end knows about a session.
///
/// Every field is something the launcher is *configured* with rather than
/// something it can look up: which tree the run happens in, where scripts and
/// transcripts live, and which model profiles the pre-run review is allowed
/// to validate names against.
pub struct WorkflowLauncherRequest {
    /// The session's task runtime — see [`WorkflowTaskRuntimeHandle`].
    pub task_runtime: WorkflowTaskRuntimeHandle,
    /// The workspace the run and its agents work in.
    pub cwd: std::path::PathBuf,
    /// `~/.rebon`, for user-level workflow scripts.
    pub config_home_dir: std::path::PathBuf,
    /// Projects root, under which a run's transcript directory is made.
    pub session_root: std::path::PathBuf,
    /// Profiles the static review may check a `modelProfile` name against.
    pub model_profiles: rebon_types::ModelProfileMap,
    /// The provider those profiles belong to; a call routed elsewhere is left
    /// to resolve at run time.
    pub active_provider: Option<String>,
}

/// The session's task runtime, carried across the seat without being named.
///
/// A workflow run is a row in the task registry, and that registry lives in a
/// crate above this one. Naming its resolver here would be a dependency
/// cycle, and
/// giving the seat a type parameter would stop it being a `dyn` service. So
/// the front end, which already holds the resolver, puts it in, and the one
/// implementation takes it back out with [`get`](Self::get).
///
/// The exchange is checked, not assumed: a handle of the wrong shape comes
/// back as an `Err` from [`WorkflowLauncherSource::for_session`], at the one
/// call site that builds a session, rather than as a panic mid-run.
#[derive(Clone)]
pub struct WorkflowTaskRuntimeHandle(Arc<dyn std::any::Any + Send + Sync>);

impl WorkflowTaskRuntimeHandle {
    pub fn new<T: Send + Sync + 'static>(runtime: T) -> Self {
        Self(Arc::new(runtime))
    }

    /// The handle, if it is a `T`. `None` means the front end and the
    /// provider disagree about what a task runtime is.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.0.downcast_ref::<T>()
    }
}

impl std::fmt::Debug for WorkflowTaskRuntimeHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkflowTaskRuntimeHandle")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPermissionReviewPhase {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPermissionReviewCall {
    pub kind: String,
    pub line: usize,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_schema: bool,
    /// Structured `agent()` option facts (label/model/provider/…) for
    /// editors; older previews omit it and reviewers fall back to `summary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<rebon_tools_core::WorkflowAgentNodeMeta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPermissionPreview {
    pub name: String,
    pub title: Option<String>,
    pub description: String,
    pub source: Option<String>,
    pub script_preview: String,
    pub review_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_summary: Option<String>,
    /// Structural launch args so review UIs can substitute `${args.…}`
    /// placeholders in prompts/labels with the actual values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<WorkflowPermissionReviewPhase>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<WorkflowPermissionReviewCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl WorkflowPermissionPreview {
    /// The `metadata` blob the permission prompt carries. Public because the
    /// `Workflow` tool, which builds that prompt, lives with its plugin.
    pub fn review_metadata(&self) -> Value {
        json!({
            "kind": "workflowReview",
            "name": &self.name,
            "title": &self.title,
            "description": &self.description,
            "source": &self.source,
            "argsSummary": &self.args_summary,
            "args": &self.args,
            "phases": &self.phases,
            "calls": &self.calls,
            "agentCallCount": self.calls.iter().filter(|call| call.kind == "agent").count(),
            "totalCallCount": self.calls.len(),
            "warnings": &self.warnings,
            "errors": &self.errors,
            "scriptExcerpt": &self.script_preview,
            "reviewText": &self.review_text,
        })
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The truth table the three crates used to spell out by hand, written
    /// out as the expressions they used to contain. `MAX_NESTED_WORKFLOW_DEPTH`
    /// is 1 today; the rows are written against the constant, not against 1,
    /// so raising it does not silently rewrite the policy.
    #[test]
    fn the_nesting_policy_answers_what_the_three_crates_used_to_ask() {
        for depth in 0..=(MAX_NESTED_WORKFLOW_DEPTH + 3) {
            let nesting = WorkflowNesting::at(depth);

            assert_eq!(nesting.depth(), depth);
            assert_eq!(nesting.is_within_workflow(), depth > 0, "depth {depth}");
            assert_eq!(
                nesting.may_launch_workflow(),
                depth <= MAX_NESTED_WORKFLOW_DEPTH,
                "depth {depth}"
            );
            assert_eq!(
                nesting.child().may_launch_workflow(),
                depth + 1 <= MAX_NESTED_WORKFLOW_DEPTH,
                "depth {depth} spawning a child"
            );

            for may_write in [false, true] {
                for external in [false, true] {
                    assert_eq!(
                        nesting.forbids_shared_worktree_writes(may_write, external),
                        depth > 0 && may_write && !external,
                        "depth {depth}, may_write {may_write}, external {external}"
                    );
                }
            }
        }
    }

    /// The outermost run is allowed; the first nested one is the boundary.
    #[test]
    fn the_ceiling_bounds_nesting_not_the_outermost_run() {
        assert!(WorkflowNesting::at(0).may_launch_workflow());
        assert!(WorkflowNesting::at(MAX_NESTED_WORKFLOW_DEPTH).may_launch_workflow());
        assert!(!WorkflowNesting::at(MAX_NESTED_WORKFLOW_DEPTH + 1).may_launch_workflow());
        assert!(!WorkflowNesting::at(0).is_within_workflow());
    }

    /// The depth stays on `ToolContext`: the policy reads it, it does not
    /// own it, because the `Agent` tool and the spawner stamp the same field.
    #[test]
    fn the_policy_reads_the_depth_the_tool_context_carries() {
        assert_eq!(WorkflowNesting::of(&ToolContext::new()).depth(), 0);
        assert_eq!(
            WorkflowNesting::of(&ToolContext::new().with_workflow_nesting_depth(3)).depth(),
            3
        );
    }
}
