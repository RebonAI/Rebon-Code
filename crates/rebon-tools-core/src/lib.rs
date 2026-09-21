//! Shared primitives for rebon tools.
//!
//! The lowest-level types a tool implementation has to speak: tool identifiers,
//! validation and permission decisions, progress payloads, the by-name catalog
//! that policy classifies calls through, and the structured error types.
//!
//! Everything here is free of harness concerns, so the crate can sit at the
//! bottom of the dependency graph.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod file_state;
pub mod process_tree;
pub mod shell_stream_order;
pub mod workflow_graph;

pub use file_state::{file_mtime_ms, normalize_path_key, FileState, FileStateCache};
pub use process_tree::ProcessTreeGuard;
pub use workflow_graph::{
    WorkflowAgentNodeMeta, WorkflowGraph, WorkflowGraphEdge, WorkflowGraphEdgeKind,
    WorkflowGraphNode, WorkflowGraphNodeStatus, WorkflowGraphNodeType,
};

/// Stable identifier for a tool registered with the harness.
///
/// A newtype rather than an enum, so unknown tool names coming off the
/// wire can still be represented without losing information.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolId(pub String);

impl ToolId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Minimal JSON-schema-like shape exposed by tools.
///
/// Raw JSON rather than a typed schema tree, because a tool schema is
/// JSON-shaped data on the wire — `inputSchema` is the field that carries it.
pub type ToolInputSchema = Value;

/// Result of tool-local input validation.
///
/// Serialised shape; the two optional fields are omitted when unset:
///
/// - `{ result: true }`
/// - `{ result: false, message, errorCode }`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationOutcome {
    pub result: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(rename = "errorCode", default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<i64>,
}

impl ValidationOutcome {
    pub fn valid() -> Self {
        Self {
            result: true,
            message: None,
            error_code: None,
        }
    }

    pub fn invalid(message: impl Into<String>, error_code: i64) -> Self {
        Self {
            result: false,
            message: Some(message.into()),
            error_code: Some(error_code),
        }
    }

    pub fn is_valid(&self) -> bool {
        self.result
    }
}

/// The code an invalid input reports when the parse did not name one.
const INVALID_INPUT_CODE: i64 = 400;

/// Deserialize a tool's input, reporting a malformed one as `InvalidInput`.
///
/// A tool names its parse once with this and hands the result to both its
/// `validate_input` and its `call`, so a malformed request is described the
/// same way wherever it is reported from.
pub fn parse_tool_input<T: serde::de::DeserializeOwned>(
    tool: ToolId,
    input: &Value,
) -> ToolResult<T> {
    serde_json::from_value(input.clone()).map_err(|error| ToolError::InvalidInput {
        tool,
        reason: error.to_string(),
        error_code: Some(INVALID_INPUT_CODE),
    })
}

/// Report a parse attempt as a validation outcome.
///
/// A tool validates by parsing: input that parses is valid, input that
/// fails to parse for a reason the caller can act on is invalid and
/// carries that reason, and anything else is a real error to propagate.
pub fn validation_outcome_from<T>(parsed: ToolResult<T>) -> ToolResult<ValidationOutcome> {
    match parsed {
        Ok(_) => Ok(ValidationOutcome::valid()),
        Err(ToolError::InvalidInput {
            reason, error_code, ..
        }) => Ok(ValidationOutcome::invalid(
            reason,
            error_code.unwrap_or(INVALID_INPUT_CODE),
        )),
        Err(err) => Err(err),
    }
}

/// Turn a validation verdict back into an error to propagate.
///
/// The inverse of [`validation_outcome_from`], and the other half of the
/// same fact: `validate_input` reports a refusal as a verdict, `call` has to
/// raise it as an error. `fallback_reason` names the refusal for the outcome
/// that says no without saying why.
pub fn require_valid_input(
    tool: ToolId,
    outcome: ValidationOutcome,
    fallback_reason: &str,
) -> ToolResult<()> {
    if outcome.is_valid() {
        return Ok(());
    }
    Err(ToolError::InvalidInput {
        tool,
        reason: outcome
            .message
            .unwrap_or_else(|| fallback_reason.to_string()),
        error_code: outcome.error_code,
    })
}

/// What the permission pipeline decided about one request: run it, refuse it,
/// or put the question to the user.
///
/// The wire form (`allow` / `deny` / `ask`) is a published contract: it appears
/// in settings files, in hook JSON on stdout, and in MCP channel replies, so
/// the strings are fixed even though the Rust names are free to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionBehavior {
    /// Run the request.
    Allow,
    /// Refuse the request.
    Deny,
    /// Put the request to the user.
    Ask,
}

impl PermissionBehavior {
    /// The wire string. Identical to the serialized form.
    pub fn as_wire(self) -> &'static str {
        match self {
            PermissionBehavior::Allow => "allow",
            PermissionBehavior::Deny => "deny",
            PermissionBehavior::Ask => "ask",
        }
    }

    /// Parse a wire string. Returns `None` for anything else, so callers
    /// decide what an unrecognized decision means rather than silently
    /// falling back to one.
    pub fn from_wire(s: &str) -> Option<PermissionBehavior> {
        Some(match s {
            "allow" => PermissionBehavior::Allow,
            "deny" => PermissionBehavior::Deny,
            "ask" => PermissionBehavior::Ask,
            _ => return None,
        })
    }
}

impl std::fmt::Display for PermissionBehavior {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// Tool-authored permission prompt payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub title: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Optional structured details for rich permission UIs. Generic clients can
    /// ignore this and continue rendering `message` as plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl PermissionRequest {
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            options: Vec::new(),
            metadata: None,
        }
    }

    pub fn with_options(mut self, options: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.options = options.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// Result of a tool-specific permission decision.
///
/// `updated_input` lets permission / hook handling rewrite the observed input
/// before execution proceeds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionDecision {
    pub behavior: PermissionBehavior,
    #[serde(
        rename = "updatedInput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub updated_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<PermissionRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PermissionDecision {
    pub fn allow(updated_input: Value) -> Self {
        Self {
            behavior: PermissionBehavior::Allow,
            updated_input: Some(updated_input),
            request: None,
            reason: None,
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            behavior: PermissionBehavior::Deny,
            updated_input: None,
            request: None,
            reason: Some(reason.into()),
        }
    }

    pub fn ask(request: PermissionRequest, updated_input: Option<Value>) -> Self {
        Self {
            behavior: PermissionBehavior::Ask,
            updated_input,
            request: Some(request),
            reason: None,
        }
    }
}

/// Streaming progress update emitted while a tool is running.
///
/// This intentionally stays generic: concrete tools can populate `kind` with
/// their own tags (`bash_stdout`, `hook_progress`, etc.) while the runtime
/// only needs a stable envelope today.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolProgressUpdate {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

impl ToolProgressUpdate {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: None,
            payload: None,
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = Some(payload);
        self
    }
}

/// Whether `candidate` is the tool's registered name or one of its aliases.
pub fn tool_matches_name(tool_name: &str, aliases: &[&str], candidate: &str) -> bool {
    tool_name == candidate || aliases.iter().any(|alias| alias == &candidate)
}

/// What a tool *is* to policy.
///
/// This is the axis permission modes, plan-mode deny lists, worker capability
/// checks and path-rule matching classify by, instead of hand-written lists of
/// tool names. A tool declares its own kind, so a plugin tool that edits files
/// is covered by the same rules as `Edit` without anyone editing a list.
///
/// This is the policy kind, not the rendering kind a transcript draws from; a
/// renderer derives its icon from this one where it has a tool to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reads a file the caller names (`Read`).
    FileRead,
    /// Writes a file the caller names (`Write`, `Edit`, `MultiEdit`,
    /// `NotebookEdit`). The `acceptEdits` class and the `Edit(...)` rule
    /// umbrella.
    FileEdit,
    /// Runs a command (`Bash`, `PowerShell`).
    Shell,
    /// Searches the tree (`Glob`, `Grep`).
    Search,
    /// Spawns another agent (`Agent`).
    Agent,
    /// Manipulates the task board (`TaskCreate`, `TaskGet`, …).
    Task,
    /// Reaches the network (`WebFetch`, `WebSearch`).
    Web,
    /// Everything else — the default.
    Other,
}

impl ToolKind {
    /// Kinds whose primary target is a file path.
    pub fn touches_a_file(self) -> bool {
        matches!(self, ToolKind::FileRead | ToolKind::FileEdit)
    }
}

/// What one builtin tool is to policy, by name.
///
/// The authority on a tool's kind is the tool itself. This table exists because
/// most of the code that asks the question ("is this call a file edit?", "what
/// field holds its path?") holds a *name* off the wire, and much of it lives in
/// crates that cannot depend on the tool implementations at all: the ACP
/// server, the message renderers, the context pruner, the ultraplan policy
/// presets.
///
/// So it is a mirror, not a second opinion: the tool layer walks every builtin,
/// derives name/aliases/kind/field from the tool itself, and fails if this
/// table disagrees by one entry. Adding a kinded builtin means adding a row
/// here, and that test says so before CI does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinToolFacts {
    /// Canonical registered name.
    pub name: &'static str,
    /// Back-compat names the runtime also answers to.
    pub aliases: &'static [&'static str],
    /// What the tool is to policy.
    pub kind: ToolKind,
    /// Input field naming the file the call acts on, for the file kinds.
    pub file_target_field: Option<&'static str>,
    /// The one input worth showing next to the tool's name, when there is one.
    ///
    /// A transcript row has space for the tool and one argument: the path for
    /// a file tool, the command for a shell, the pattern for a search. Which
    /// argument that is, is a fact about the tool, so it lives here rather
    /// than in whichever surface happens to be drawing the row.
    pub primary_input: Option<PrimaryInput>,
}

/// The input a transcript row shows beside a tool's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrimaryInput {
    /// What to call it to a person (`path`, `command`, `pattern`, ...).
    pub label: &'static str,
    /// The field it arrives in, which is what a caller reads it out of.
    pub field: &'static str,
}

impl BuiltinToolFacts {
    /// Whether `candidate` is this tool's canonical name or one of its aliases.
    pub fn matches(&self, candidate: &str) -> bool {
        tool_matches_name(self.name, self.aliases, candidate)
    }
}

const fn facts(
    name: &'static str,
    aliases: &'static [&'static str],
    kind: ToolKind,
    file_target_field: Option<&'static str>,
) -> BuiltinToolFacts {
    // The file kinds show the path they act on; that is the same field policy
    // already reads, so it is derived rather than repeated.
    let primary_input = match file_target_field {
        Some(field) => Some(PrimaryInput {
            label: "path",
            field,
        }),
        None => None,
    };
    BuiltinToolFacts {
        name,
        aliases,
        kind,
        file_target_field,
        primary_input,
    }
}

/// A tool whose shown input is not a file path: the field and its label are
/// given outright.
const fn facts_showing(
    name: &'static str,
    aliases: &'static [&'static str],
    kind: ToolKind,
    label: &'static str,
    field: &'static str,
) -> BuiltinToolFacts {
    BuiltinToolFacts {
        name,
        aliases,
        kind,
        file_target_field: None,
        primary_input: Some(PrimaryInput { label, field }),
    }
}

/// The `Skill` tool's registered name.
///
/// The tool, its registry and its loader belong to a plugin, so nothing below
/// the plugin layer can name it by reaching for the implementation. Two things
/// below still have to: the engine dispatches a `SkillInvocationRequest` as a
/// `Skill` tool call, and policy has to know that a `SkillTool` rule covers a
/// `Skill` call. A tool's name is a tool fact, so it lives here with the rest
/// of them, and the row below is spelled from it.
pub const SKILL_TOOL_NAME: &str = "Skill";

/// Every builtin whose kind is not [`ToolKind::Other`], in catalog order,
/// plus the `Other`-kind rows something below the owning plugin has to read
/// (today: [`SKILL_TOOL_NAME`], whose aliases policy resolves).
pub const BUILTIN_TOOL_FACTS: &[BuiltinToolFacts] = &[
    facts(
        "Read",
        &["FileReadTool"],
        ToolKind::FileRead,
        Some("file_path"),
    ),
    facts(
        "Write",
        &["FileWriteTool"],
        ToolKind::FileEdit,
        Some("file_path"),
    ),
    facts(
        "Edit",
        &["FileEditTool"],
        ToolKind::FileEdit,
        Some("file_path"),
    ),
    facts(
        "MultiEdit",
        &["MultiEditTool", "FileMultiEditTool"],
        ToolKind::FileEdit,
        Some("file_path"),
    ),
    facts_showing(
        "Glob",
        &["GlobTool"],
        ToolKind::Search,
        "pattern",
        "pattern",
    ),
    facts_showing(
        "Grep",
        &["GrepTool"],
        ToolKind::Search,
        "pattern",
        "pattern",
    ),
    facts_showing(
        "Bash",
        &["BashTool", "bash"],
        ToolKind::Shell,
        "command",
        "command",
    ),
    facts_showing(
        "PowerShell",
        &["PowerShellTool"],
        ToolKind::Shell,
        "command",
        "command",
    ),
    facts(
        "NotebookEdit",
        &["NotebookEditTool"],
        ToolKind::FileEdit,
        Some("notebook_path"),
    ),
    facts_showing(
        "Agent",
        &["AgentTool", "Task"],
        ToolKind::Agent,
        "prompt",
        "prompt",
    ),
    facts("TaskCreate", &["TaskCreateTool"], ToolKind::Task, None),
    facts("TaskGet", &["TaskGetTool"], ToolKind::Task, None),
    facts("TaskList", &["TaskListTool"], ToolKind::Task, None),
    facts("TaskUpdate", &["TaskUpdateTool"], ToolKind::Task, None),
    facts("TaskStop", &["TaskStopTool"], ToolKind::Task, None),
    facts("TodoWrite", &["TodoWriteTool"], ToolKind::Task, None),
    facts_showing("WebFetch", &["WebFetchTool"], ToolKind::Web, "url", "url"),
    facts_showing(
        "WebSearch",
        &["WebSearchTool"],
        ToolKind::Web,
        "query",
        "query",
    ),
    // `Other` to policy — invoking a skill is not reading, editing or
    // executing — but the row exists so `SkillTool` resolves to `Skill`
    // without policy carrying a synonym of its own.
    facts(SKILL_TOOL_NAME, &["SkillTool"], ToolKind::Other, None),
];

/// The recorded facts for a builtin tool named `name` (canonical or alias).
pub fn builtin_tool_facts_for_name(name: &str) -> Option<&'static BuiltinToolFacts> {
    BUILTIN_TOOL_FACTS.iter().find(|facts| facts.matches(name))
}

/// The policy kind of the tool a call names.
///
/// [`ToolKind::Other`] for anything not a kinded builtin, which is how a
/// plugin tool called `EditDatabase` stays out of the file-edit class.
pub fn tool_kind_for_name(name: &str) -> ToolKind {
    builtin_tool_facts_for_name(name)
        .map(|facts| facts.kind)
        .unwrap_or(ToolKind::Other)
}

/// Canonical names of every builtin of `kind`, in catalog order — the derived
/// replacement for a hand-written list such as "the file-edit tools".
pub fn tool_names_of_kind(kind: ToolKind) -> Vec<&'static str> {
    BUILTIN_TOOL_FACTS
        .iter()
        .filter(|facts| facts.kind == kind)
        .map(|facts| facts.name)
        .collect()
}

/// The input field a builtin file tool names its target with.
pub fn file_target_field_for_name(name: &str) -> Option<&'static str> {
    builtin_tool_facts_for_name(name).and_then(|facts| facts.file_target_field)
}

/// Separate model-facing recovery detail from the concise text rendered to users.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolErrorPresentation {
    pub code: String,
    pub display_message: String,
    pub model_message: String,
}

impl ToolErrorPresentation {
    pub fn new(
        code: impl Into<String>,
        display_message: impl Into<String>,
        model_message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            display_message: display_message.into(),
            model_message: model_message.into(),
        }
    }

    pub fn same(code: impl Into<String>, message: impl Into<String>) -> Self {
        let message = message.into();
        Self::new(code, message.clone(), message)
    }

    pub fn has_distinct_display_message(&self) -> bool {
        self.display_message != self.model_message
    }
}

impl std::fmt::Display for ToolErrorPresentation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.model_message)
    }
}

/// Errors produced while validating or executing a tool call.
///
/// Still intentionally small, but wide enough to express the first real tool
/// runtime phases: validation, permissions, cancellation, and execution.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool `{tool}` is not registered")]
    UnknownTool { tool: ToolId },

    #[error("invalid input for tool `{tool}`: {reason}")]
    InvalidInput {
        tool: ToolId,
        reason: String,
        error_code: Option<i64>,
    },

    #[error("permission denied for tool `{tool}`: {reason}")]
    PermissionDenied { tool: ToolId, reason: String },

    #[error("tool `{tool}` was cancelled: {reason}")]
    Cancelled { tool: ToolId, reason: String },

    #[error("{presentation}")]
    Presented {
        tool: ToolId,
        presentation: ToolErrorPresentation,
    },

    #[error("tool `{tool}` execution failed: {source}")]
    Execution {
        tool: ToolId,
        #[source]
        source: anyhow::Error,
    },
}

impl ToolError {
    pub fn presentation(&self) -> ToolErrorPresentation {
        match self {
            Self::Presented { presentation, .. } => presentation.clone(),
            _ => ToolErrorPresentation::same("tool_error", self.to_string()),
        }
    }
}

/// Structured schema-level validation error.
///
/// Categorises problems into missing required params, unexpected params, and
/// type mismatches; [`InputValidationError::format`] renders them into the
/// error message the model sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputValidationError {
    pub tool_name: String,
    pub missing_params: Vec<String>,
    pub unexpected_params: Vec<String>,
    pub type_mismatches: Vec<TypeMismatch>,
}

/// Single type-mismatch entry inside [`InputValidationError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeMismatch {
    pub param: String,
    pub expected: String,
    pub received: String,
}

impl InputValidationError {
    pub fn new(tool_name: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            missing_params: Vec::new(),
            unexpected_params: Vec::new(),
            type_mismatches: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.missing_params.is_empty()
            && self.unexpected_params.is_empty()
            && self.type_mismatches.is_empty()
    }

    /// Format into the error string the model reads:
    ///
    /// ```text
    /// Read failed due to the following issue(s):
    /// The required parameter `file_path` is missing
    /// An unexpected parameter `unknown` was provided
    /// The parameter `timeout` type is expected as `integer` but provided as `string`
    /// ```
    pub fn format(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        for p in &self.missing_params {
            parts.push(format!("The required parameter `{p}` is missing"));
        }
        for p in &self.unexpected_params {
            parts.push(format!("An unexpected parameter `{p}` was provided"));
        }
        for m in &self.type_mismatches {
            parts.push(format!(
                "The parameter `{}` type is expected as `{}` but provided as `{}`",
                m.param, m.expected, m.received
            ));
        }

        let noun = if parts.len() == 1 { "issue" } else { "issues" };
        format!(
            "{} failed due to the following {noun}:\n{}",
            self.tool_name,
            parts.join("\n")
        )
    }
}

impl std::fmt::Display for InputValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.format())
    }
}

impl std::error::Error for InputValidationError {}

/// Result of a tool call.
pub type ToolResult<T> = Result<T, ToolError>;

/// Parameter names to show in the streaming tool header for `tool_name`, in
/// priority order.
///
/// Kept here so the rendering layer stays in sync with tool schemas without
/// hardcoding tool-specific knowledge of its own. `None` for unknown tools —
/// callers should then fall back to showing all key=value pairs.
pub fn primary_display_params(tool_name: &str) -> Option<&'static [&'static str]> {
    match tool_name {
        "Read" | "Write" => Some(&["file_path"]),
        "Edit" | "MultiEdit" => Some(&["file_path"]),
        "NotebookEdit" => Some(&["notebook_path"]),
        "Bash" | "PowerShell" => Some(&["command"]),
        "ShellOutput" | "ShellStop" => Some(&["shellId"]),
        "Grep" | "Glob" => Some(&["pattern", "path"]),
        "Agent" => Some(&["description"]),
        // The program is the point of a `run_code` call, but a program on a
        // header line is a truncated line of JavaScript that says nothing. The
        // description says what it does; the program itself is rendered in the
        // body, whole.
        "run_code" => Some(&["description"]),
        "WebSearch" | "ToolSearch" => Some(&["query"]),
        "WebFetch" => Some(&["url"]),
        "Skill" => Some(&["skill"]),
        "TodoWrite" => Some(&[]),
        "SendMessage" => Some(&["to", "summary"]),
        "Sleep" => Some(&["duration_ms"]),
        "EscalateQuestion" => Some(&["question"]),
        "ResolveEscalation" => Some(&["agent_id", "answer"]),
        _ => None,
    }
}

/// Resolve `path` to the form a scope check compares: canonical where it
/// exists, and otherwise the canonical form of its deepest existing ancestor
/// with the missing tail appended lexically.
///
/// The ancestor walk is what lets a tool create a file that is not there yet:
/// with only `canonicalize`, an existing root would come back `\\?\`-prefixed
/// on Windows while the new path would not, and every create would be
/// refused. `None` means no ancestor exists at all — not even the root — which
/// only a relative path against a vanished cwd produces.
///
/// The tail is appended as given; a `..` inside it is left for the caller,
/// since [`std::path::Path::file_name`] stops the walk at one.
pub fn canonicalize_scope_path(path: &std::path::Path) -> Option<std::path::PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Some(normalize_scope_path(canonical));
    }
    let mut remainder = Vec::new();
    let mut ancestor = path;
    while let (Some(parent), Some(name)) = (ancestor.parent(), ancestor.file_name()) {
        remainder.push(name.to_os_string());
        ancestor = parent;
        if let Ok(canonical) = std::fs::canonicalize(ancestor) {
            let mut rebuilt = normalize_scope_path(canonical);
            for name in remainder.iter().rev() {
                rebuilt.push(name);
            }
            return Some(normalize_scope_path(rebuilt));
        }
    }
    None
}

/// [`strip_windows_verbatim_prefix`] followed by [`lexically_normalize_path`]:
/// the shape every path takes before a scope comparison.
pub fn normalize_scope_path(path: std::path::PathBuf) -> std::path::PathBuf {
    lexically_normalize_path(&strip_windows_verbatim_prefix(path))
}

/// Drop `.` components and let `..` consume the component before it, without
/// touching the filesystem.
///
/// A `..` with nothing left to consume — at the root, or at the start of a
/// relative path — is dropped rather than kept: the result is used to decide
/// whether a path lies under a root, and `../x` against `/repo` must not
/// resolve to something that still starts with `/repo`. Callers that need
/// leading `..` preserved (instruction-file discovery does) keep their own
/// normalizer.
pub fn lexically_normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

/// Turn the extended-length form `std::fs::canonicalize` returns on Windows
/// (`\\?\C:\…`, `\\?\UNC\server\share\…`) back into the plain spelling
/// (`C:\…`, `\\server\share\…`), so it compares equal to a path the user or
/// the model wrote. The prefixes never occur elsewhere, so this is the
/// identity on other platforms.
#[cfg(windows)]
pub fn strip_windows_verbatim_prefix(path: std::path::PathBuf) -> std::path::PathBuf {
    std::path::PathBuf::from(strip_windows_verbatim_prefix_str(
        &path.to_string_lossy(),
        true,
    ))
}

/// Identity: see the Windows version.
#[cfg(not(windows))]
pub fn strip_windows_verbatim_prefix(path: std::path::PathBuf) -> std::path::PathBuf {
    path
}

/// The same rule on a string, with the platform given rather than compiled in.
///
/// A caller that *keys* on a path -- the projects-root directory component,
/// say -- has to be testable against Windows spellings from any host, and must
/// not grow a second copy of this rule to do it. The `cfg` version above is
/// this one with `windows` filled in.
pub fn strip_windows_verbatim_prefix_str(path: &str, windows: bool) -> String {
    if !windows {
        return path.to_string();
    }
    if let Some(stripped) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{stripped}");
    }
    if let Some(stripped) = path.strip_prefix(r"\\?\") {
        return stripped.to_string();
    }
    path.to_string()
}

/// Whether `path` is `root` or lies under it, comparing the way the platform
/// does: on Windows separators and case are folded first, so `C:/Repo/x`
/// is under `c:\repo`.
#[cfg(windows)]
pub fn scope_path_starts_with(path: &std::path::Path, root: &std::path::Path) -> bool {
    let path = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    let root = root
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    path == root || path.starts_with(&format!("{}\\", root.trim_end_matches('\\')))
}

/// Whether `path` is `root` or lies under it, component-wise.
#[cfg(not(windows))]
pub fn scope_path_starts_with(path: &std::path::Path, root: &std::path::Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These three strings are a published contract: they appear in settings
    /// files, in hook JSON on stdout, and in MCP channel replies. Renaming a
    /// Rust variant must not move them.
    #[test]
    fn permission_behavior_wire_form_is_fixed() {
        for (behavior, wire) in [
            (PermissionBehavior::Allow, "allow"),
            (PermissionBehavior::Deny, "deny"),
            (PermissionBehavior::Ask, "ask"),
        ] {
            let quoted = format!("\"{wire}\"");
            assert_eq!(serde_json::to_string(&behavior).unwrap(), quoted);
            assert_eq!(
                serde_json::from_str::<PermissionBehavior>(&quoted).unwrap(),
                behavior
            );
            assert_eq!(behavior.as_wire(), wire);
            assert_eq!(PermissionBehavior::from_wire(wire), Some(behavior));
            assert_eq!(behavior.to_string(), wire);
        }
        assert_eq!(PermissionBehavior::from_wire("passthrough"), None);
        assert_eq!(PermissionBehavior::from_wire("Allow"), None);
    }

    #[test]
    fn lexical_normalization_drops_dot_and_lets_dotdot_consume_the_previous_component() {
        assert_eq!(
            lexically_normalize_path(std::path::Path::new("/repo/./crates/../src/lib.rs")),
            std::path::PathBuf::from("/repo/src/lib.rs")
        );
    }

    /// `..` past the root, or leading `..` in a relative path, is dropped:
    /// the result decides "is this under the root?", and must not keep a
    /// prefix it has already escaped.
    #[test]
    fn lexical_normalization_does_not_keep_an_escaping_dotdot() {
        assert_eq!(
            lexically_normalize_path(std::path::Path::new("/repo/../etc/hostname")),
            std::path::PathBuf::from("/etc/hostname")
        );
        assert_eq!(
            lexically_normalize_path(std::path::Path::new("../x/y")),
            std::path::PathBuf::from("x/y")
        );
    }

    #[test]
    fn tool_id_roundtrips_through_json() {
        let id = ToolId::new("BashTool");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"BashTool\"");
        let back: ToolId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn validation_outcome_helpers_match_expected_shape() {
        assert!(ValidationOutcome::valid().is_valid());
        assert_eq!(
            ValidationOutcome::invalid("bad path", 400),
            ValidationOutcome {
                result: false,
                message: Some("bad path".into()),
                error_code: Some(400),
            }
        );
    }

    #[test]
    fn require_valid_input_passes_a_valid_outcome_through() {
        assert!(
            require_valid_input(ToolId::new("Read"), ValidationOutcome::valid(), "fallback")
                .is_ok()
        );
    }

    #[test]
    fn require_valid_input_raises_the_outcome_verbatim() {
        let err = require_valid_input(
            ToolId::new("Read"),
            ValidationOutcome::invalid("Path is not a file: /tmp", 2),
            "Read input is invalid",
        )
        .unwrap_err();
        match err {
            ToolError::InvalidInput {
                tool,
                reason,
                error_code,
            } => {
                assert_eq!(tool.as_str(), "Read");
                assert_eq!(reason, "Path is not a file: /tmp");
                assert_eq!(error_code, Some(2));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn require_valid_input_names_the_fallback_when_the_outcome_is_silent() {
        let err = require_valid_input(
            ToolId::new("Read"),
            ValidationOutcome {
                result: false,
                message: None,
                error_code: None,
            },
            "Read input is invalid",
        )
        .unwrap_err();
        match err {
            ToolError::InvalidInput {
                reason, error_code, ..
            } => {
                assert_eq!(reason, "Read input is invalid");
                assert_eq!(error_code, None);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// The two conversions are inverses, which is what lets a tool's
    /// `validate_input` be `validation_outcome_from(prepare(..))` while its
    /// `call` raises the same refusal through `require_valid_input`.
    #[test]
    fn the_two_validation_conversions_round_trip() {
        let outcome = ValidationOutcome::invalid("Path is not a file: /tmp", 2);
        let raised = require_valid_input(ToolId::new("Read"), outcome.clone(), "fallback");
        assert_eq!(validation_outcome_from(raised).unwrap(), outcome);
    }

    #[test]
    fn permission_decision_allow_carries_updated_input() {
        let input = serde_json::json!({ "cmd": "ls" });
        let decision = PermissionDecision::allow(input.clone());
        assert_eq!(decision.behavior, PermissionBehavior::Allow);
        assert_eq!(decision.updated_input, Some(input));
        assert!(decision.request.is_none());
    }

    #[test]
    fn permission_request_builder_sets_options() {
        let request = PermissionRequest::new("Run bash", "Needs approval")
            .with_options(["allow_once", "deny"]);
        assert_eq!(request.options, vec!["allow_once", "deny"]);
    }

    #[test]
    fn tool_progress_update_builder_sets_optional_fields() {
        let update = ToolProgressUpdate::new("stdout")
            .with_message("hello")
            .with_payload(serde_json::json!({ "line": 1 }));
        assert_eq!(update.kind, "stdout");
        assert_eq!(update.message.as_deref(), Some("hello"));
        assert_eq!(update.payload, Some(serde_json::json!({ "line": 1 })));
    }

    #[test]
    fn tool_matches_name_checks_primary_and_aliases() {
        assert!(tool_matches_name("Read", &["FileRead"], "Read"));
        assert!(tool_matches_name("Read", &["FileRead"], "FileRead"));
        assert!(!tool_matches_name("Read", &["FileRead"], "Write"));
    }

    #[test]
    fn input_validation_error_formats_single_missing_param() {
        let mut err = InputValidationError::new("Read");
        err.missing_params.push("file_path".into());
        assert_eq!(
            err.format(),
            "Read failed due to the following issue:\nThe required parameter `file_path` is missing"
        );
    }

    #[test]
    fn input_validation_error_formats_multiple_issues() {
        let mut err = InputValidationError::new("Bash");
        err.missing_params.push("command".into());
        err.unexpected_params.push("cmd".into());
        err.type_mismatches.push(TypeMismatch {
            param: "timeout".into(),
            expected: "integer".into(),
            received: "string".into(),
        });
        let formatted = err.format();
        assert!(formatted.starts_with("Bash failed due to the following issues:\n"));
        assert!(formatted.contains("The required parameter `command` is missing"));
        assert!(formatted.contains("An unexpected parameter `cmd` was provided"));
        assert!(formatted.contains(
            "The parameter `timeout` type is expected as `integer` but provided as `string`"
        ));
    }

    #[test]
    fn input_validation_error_is_empty_when_no_issues() {
        let err = InputValidationError::new("Read");
        assert!(err.is_empty());
    }

    /// The two halves of a row's file fact have to agree.
    ///
    /// `file_target_field` is what policy reads to decide whether a call
    /// touches a path it may not; `primary_input` is what a transcript shows
    /// a person. A tool where those name different fields would be showing
    /// one path and gating another.
    #[test]
    fn a_file_tool_shows_the_path_that_policy_gates() {
        for facts in BUILTIN_TOOL_FACTS {
            let Some(target) = facts.file_target_field else {
                continue;
            };
            let shown = facts
                .primary_input
                .unwrap_or_else(|| panic!("{} gates a path but shows nothing", facts.name));
            assert_eq!(
                shown.field, target,
                "{} gates {target} but shows {}",
                facts.name, shown.field
            );
            assert_eq!(shown.label, "path", "{} shows a path", facts.name);
        }
    }

    /// `MultiEdit` answers to `FileMultiEditTool` as well.
    ///
    /// A hand-written edit-tool list once carried no such spelling, so a call
    /// named `FileMultiEditTool` fell into the "other" bucket. Classification
    /// goes through this table now, and this pins the alias, because a table
    /// that quietly loses one puts the bug straight back.
    #[test]
    fn multi_edit_answers_to_its_older_alias() {
        for name in ["MultiEdit", "MultiEditTool", "FileMultiEditTool"] {
            let facts = builtin_tool_facts_for_name(name)
                .unwrap_or_else(|| panic!("{name} is a MultiEdit spelling"));
            assert_eq!(facts.name, "MultiEdit");
            assert_eq!(facts.kind, ToolKind::FileEdit);
        }
    }

    /// Every tool that shows something names a field, and no two rows disagree
    /// about what a given label is called.
    #[test]
    fn a_shown_input_names_a_field_and_labels_stay_consistent() {
        let mut label_to_field: Vec<(&str, &str)> = Vec::new();
        for facts in BUILTIN_TOOL_FACTS {
            let Some(shown) = facts.primary_input else {
                continue;
            };
            assert!(
                !shown.field.is_empty(),
                "{} shows an unnamed field",
                facts.name
            );
            assert!(
                !shown.label.is_empty(),
                "{} shows an unlabelled field",
                facts.name
            );
            if shown.label != "path" {
                if let Some((_, seen)) = label_to_field.iter().find(|(l, _)| *l == shown.label) {
                    assert_eq!(
                        *seen, shown.field,
                        "label {:?} names {seen} on one row and {} on {}",
                        shown.label, shown.field, facts.name
                    );
                } else {
                    label_to_field.push((shown.label, shown.field));
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefixes_come_off_disk_and_unc_paths() {
        assert_eq!(
            strip_windows_verbatim_prefix(std::path::PathBuf::from(r"\\?\C:\repo\src")),
            std::path::PathBuf::from(r"C:\repo\src")
        );
        assert_eq!(
            strip_windows_verbatim_prefix(std::path::PathBuf::from(r"\\?\UNC\host\share\x")),
            std::path::PathBuf::from(r"\\host\share\x")
        );
        assert_eq!(
            strip_windows_verbatim_prefix(std::path::PathBuf::from(r"C:\plain")),
            std::path::PathBuf::from(r"C:\plain")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn verbatim_prefix_stripping_is_the_identity_off_windows() {
        let path = std::path::PathBuf::from(r"\\?\C:\repo");
        assert_eq!(strip_windows_verbatim_prefix(path.clone()), path);
    }

    /// A target that does not exist yet resolves through its deepest existing
    /// ancestor, so it compares equal to the same file once it exists.
    #[test]
    fn canonicalize_scope_path_anchors_a_missing_tail_on_its_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let existing = canonicalize_scope_path(dir.path()).unwrap();
        let missing = dir.path().join("not").join("yet").join("there.txt");

        let resolved = canonicalize_scope_path(&missing).unwrap();

        assert_eq!(resolved, existing.join("not").join("yet").join("there.txt"));
        assert!(scope_path_starts_with(&resolved, &existing));
        assert!(!scope_path_starts_with(&existing, &resolved));
    }
}
