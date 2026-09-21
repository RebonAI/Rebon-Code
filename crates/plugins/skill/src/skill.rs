//! `SkillTool` — expand a registered skill definition into the prompt
//! the model acts on.
//!
//! A skill is a rich multi-step workflow with:
//!
//! - Variable substitution in a frontmatter-style spec
//! - Pre-conditions (tool availability, file existence)
//! - Staged prompts forwarded back to the model
//! - Post-hooks that mutate session state
//!
//! This module implements the **deterministic template expander**:
//! given a skill id and a variables bag, find
//! the skill in the registry, render its prompt template, and
//! return the expanded text. The model can then act on that prompt
//! via its own follow-up turn. Richer staged execution is deferred.

use async_trait::async_trait;
use rebon_tool::{Tool, ToolContext};
use rebon_tools_core::{ToolError, ToolId, ToolInputSchema, ToolResult, ValidationOutcome};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};

use crate::loader::SkillState;

/// This plugin's session state, carried in [`ToolContext`]'s extension bag
/// and handed to the progressive-discovery subscriber on every tool round.
///
/// One value rather than two, because both readers need the registry: the
/// tool resolves a name against it, and the subscriber registers what it just
/// found in it. Hosts attach it with
/// [`ToolContext::with_extension`](rebon_tool::ToolContext::with_extension) —
/// the generic escape hatch a feature behind a plugin boundary uses instead
/// of adding a field to the shared struct.
#[derive(Clone, Default)]
pub struct SkillContext {
    /// The loaded skill index. `None` on a host that loaded no skills, which
    /// is what makes `Skill` answer that it has no registry rather than
    /// answering that every skill is unknown.
    pub registry: Option<Arc<SkillRegistry>>,
    /// Progressive-discovery bookkeeping: the conditional pool and the
    /// directories already scanned. `None` on a host that does not discover
    /// (a sub-agent turn, a projection fixture).
    pub discovery: Option<Arc<Mutex<SkillState>>>,
}

impl SkillContext {
    /// The index alone, for a host that loads skills but does not discover.
    pub fn with_registry(registry: Arc<SkillRegistry>) -> Self {
        Self {
            registry: Some(registry),
            discovery: None,
        }
    }

    /// The index plus the discovery state a full session carries.
    pub fn new(registry: Arc<SkillRegistry>, discovery: Arc<Mutex<SkillState>>) -> Self {
        Self {
            registry: Some(registry),
            discovery: Some(discovery),
        }
    }

    /// Borrow the skill state a [`ToolContext`] carries, if any.
    pub fn of(context: &ToolContext) -> Option<&Self> {
        context.extension::<Self>()
    }

    /// Borrow the loaded index a [`ToolContext`] carries, if any.
    pub fn registry_of(context: &ToolContext) -> Option<&Arc<SkillRegistry>> {
        Self::of(context)?.registry.as_ref()
    }
}

fn substitute_arguments(
    content: &str,
    args: Option<&str>,
    append_if_no_placeholder: bool,
    argument_names: &[String],
) -> String {
    let Some(args) = args else {
        return content.to_string();
    };

    let parsed_args = parse_arguments(args);
    let original_content = content.to_string();
    let mut result = content.to_string();

    for (i, name) in argument_names.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        let value = parsed_args.get(i).map(String::as_str).unwrap_or("");
        let pattern = format!("${name}");
        let mut out = String::with_capacity(result.len());
        let mut rest = result.as_str();
        while let Some(pos) = rest.find(&pattern) {
            out.push_str(&rest[..pos]);
            let after = &rest[pos + pattern.len()..];
            let is_boundary = after
                .chars()
                .next()
                .map(|c| !c.is_alphanumeric() && c != '_' && c != '[')
                .unwrap_or(true);
            if is_boundary {
                out.push_str(value);
            } else {
                out.push_str(&pattern);
            }
            rest = after;
        }
        out.push_str(rest);
        result = out;
    }

    result = replace_indexed_arguments(&result, "$ARGUMENTS[", "]", &parsed_args);
    result = replace_shorthand_indexed(&result, &parsed_args);
    result = result.replace("$ARGUMENTS", args);

    if result == original_content && append_if_no_placeholder && !args.is_empty() {
        result.push_str(&format!("\n\nARGUMENTS: {args}"));
    }

    result
}

fn parse_arguments(args: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut chars = args.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    result.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }

    if !current.is_empty() {
        result.push(current);
    }
    result
}

fn replace_indexed_arguments(content: &str, prefix: &str, suffix: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find(prefix) {
        out.push_str(&rest[..start]);
        let after_prefix = &rest[start + prefix.len()..];
        if let Some(end) = after_prefix.find(suffix) {
            let index_str = &after_prefix[..end];
            if let Ok(index) = index_str.parse::<usize>() {
                out.push_str(args.get(index).map(String::as_str).unwrap_or(""));
                rest = &after_prefix[end + suffix.len()..];
                continue;
            }
        }
        out.push_str(prefix);
        rest = after_prefix;
    }
    out.push_str(rest);
    out
}

fn replace_shorthand_indexed(content: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(content.len());
    let chars: Vec<char> = content.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            let next_is_word =
                end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_');
            if !next_is_word {
                let index_str: String = chars[start..end].iter().collect();
                if let Ok(index) = index_str.parse::<usize>() {
                    out.push_str(args.get(index).map(String::as_str).unwrap_or(""));
                    i = end;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Provenance of a registered skill. This is intentionally small and
/// matches only the source buckets the runtime loader can identify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillSource {
    /// Bundled skill compiled into the CLI.
    BuiltIn,
    /// Skill loaded from `~/.rebon/skills` or legacy `~/.rebon/commands`.
    User,
    /// Skill loaded from `<cwd>/.rebon/skills` or legacy `<cwd>/.rebon/commands`.
    Project,
    /// Policy-managed skill.
    Managed,
    /// Local settings skill.
    Local,
    /// CLI flag / feature-flag settings skill.
    Flag,
    /// Plugin-provided skill.
    Plugin,
    /// MCP server-provided skill.
    Mcp,
}

/// Registered name of the skill tool.
///
/// Spelled in `rebon-tools-core` with the rest of the tool facts, because the
/// engine has to name this tool when it dispatches a `SkillInvocationRequest`
/// and cannot depend on the plugin that owns it.
pub use rebon_tools_core::SKILL_TOOL_NAME;

const INVALID_INPUT_CODE: i64 = 400;

/// Skill definition: id (required), title (human-visible), prompt
/// template (with `{{ variable }}` placeholders), and an optional
/// list of suggested follow-up tools the response can carry
/// back to the model.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Unique skill id — the value callers pass as `skill` in the
    /// tool input.
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Short description shown when the skill is invoked.
    pub description: String,
    /// Prompt template with `{{ variable }}` placeholders.
    pub prompt_template: String,
    /// Optional list of suggested follow-up tools.
    pub suggested_tools: Vec<String>,
    /// Source bucket supplied by the runtime loader.
    pub source: SkillSource,
    /// Hint shown after the skill name when invoked as a slash command.
    pub argument_hint: Option<String>,
    /// Argument names used for `$name` substitution.
    pub argument_names: Vec<String>,
    /// Directory containing the skill files, if file-backed.
    pub skill_root: Option<String>,
    /// Whether users may invoke this skill directly as `/name`.
    pub user_invocable: bool,
    /// Whether the model may invoke this skill with the Skill tool.
    pub disable_model_invocation: bool,
    /// Tools a turn must offer before this skill is listed to the model or
    /// can be invoked there — the frontmatter's `required-tools`. Empty means
    /// the skill stands on its own.
    pub required_tools: Vec<String>,
}

impl Skill {
    /// Render the prompt template with the provided variables.
    /// Unknown placeholders are left unchanged (`{{ foo }}`) so the
    /// caller can spot unresolved references.
    pub fn render(&self, variables: &Map<String, Value>) -> String {
        let mut out = String::with_capacity(self.prompt_template.len());
        let mut rest = self.prompt_template.as_str();
        while let Some(open) = rest.find("{{") {
            out.push_str(&rest[..open]);
            rest = &rest[open + 2..];
            let Some(close) = rest.find("}}") else {
                out.push_str("{{");
                out.push_str(rest);
                return out;
            };
            let var_name = rest[..close].trim();
            rest = &rest[close + 2..];
            match variables.get(var_name) {
                Some(Value::String(s)) => out.push_str(s),
                Some(other) => out.push_str(&other.to_string()),
                None => {
                    out.push_str("{{ ");
                    out.push_str(var_name);
                    out.push_str(" }}");
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Render the prompt template for an invocation with optional slash args.
    pub fn render_invocation(&self, args: Option<&str>, variables: &Map<String, Value>) -> String {
        let rendered = self.render(variables);
        substitute_arguments(&rendered, args, true, &self.argument_names)
    }
}

/// Complete skill catalog plus the persisted denylist applied to its enabled
/// views. Keeping disabled entries in the catalog lets management UIs list and
/// re-enable them without reloading skill files.
#[derive(Debug, Default)]
struct SkillRegistryState {
    catalog: HashMap<String, Skill>,
    disabled: BTreeSet<String>,
}

/// Thread-safe registry of skill definitions.
#[derive(Debug, Default)]
pub struct SkillRegistry {
    inner: RwLock<SkillRegistryState>,
}

impl SkillRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a skill, replacing any previous entry with the
    /// same id. Returns the previous entry if one existed.
    ///
    /// A denylisted id remains disabled when it is registered later.
    pub fn register(&self, skill: Skill) -> Option<Skill> {
        let mut guard = self.inner.write().expect("skill registry poisoned");
        guard.catalog.insert(skill.id.clone(), skill)
    }

    /// Borrow an enabled skill by id.
    pub fn get(&self, id: &str) -> Option<Skill> {
        let guard = self.inner.read().expect("skill registry poisoned");
        if guard.disabled.contains(id) {
            return None;
        }
        guard.catalog.get(id).cloned()
    }

    /// Snapshot every enabled skill id in sorted order.
    pub fn ids(&self) -> Vec<String> {
        let guard = self.inner.read().expect("skill registry poisoned");
        let mut ids = guard
            .catalog
            .keys()
            .filter(|id| !guard.disabled.contains(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    /// Snapshot every enabled skill in registry key order.
    pub fn entries(&self) -> Vec<Skill> {
        let guard = self.inner.read().expect("skill registry poisoned");
        let mut entries = guard
            .catalog
            .values()
            .filter(|skill| !guard.disabled.contains(&skill.id))
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }

    /// Snapshot the complete catalog, including disabled skills, in registry
    /// key order. This is intended for management UIs.
    pub fn all_entries(&self) -> Vec<Skill> {
        let guard = self.inner.read().expect("skill registry poisoned");
        let mut entries = guard.catalog.values().cloned().collect::<Vec<_>>();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }

    /// Snapshot enabled user-facing skills in registry key order.
    pub fn user_invocable_entries(&self) -> Vec<Skill> {
        self.entries()
            .into_iter()
            .filter(|skill| skill.user_invocable)
            .collect()
    }

    /// Count of enabled user-facing skills (no allocation).
    pub fn user_invocable_count(&self) -> usize {
        let guard = self.inner.read().expect("skill registry poisoned");
        guard
            .catalog
            .values()
            .filter(|skill| skill.user_invocable && !guard.disabled.contains(&skill.id))
            .count()
    }

    /// Replace the current denylist. Ids are trimmed and empty ids ignored;
    /// unknown ids are retained so later registrations are disabled
    /// immediately.
    pub fn set_disabled_skills<I, S>(&self, disabled_skills: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let disabled = disabled_skills
            .into_iter()
            .filter_map(|id| {
                let id = id.as_ref().trim().to_string();
                (!id.is_empty()).then_some(id)
            })
            .collect();
        let mut guard = self.inner.write().expect("skill registry poisoned");
        guard.disabled = disabled;
    }

    /// Snapshot the full denylist, including ids not yet registered.
    pub fn disabled_skills(&self) -> BTreeSet<String> {
        let guard = self.inner.read().expect("skill registry poisoned");
        guard.disabled.clone()
    }

    /// Whether an id is present in the denylist.
    pub fn is_disabled(&self, id: &str) -> bool {
        let guard = self.inner.read().expect("skill registry poisoned");
        guard.disabled.contains(id)
    }

    /// Remove a skill by id. Its denylist entry, if any, is retained so a
    /// later registration still honors the persisted preference.
    pub fn remove(&self, id: &str) -> Option<Skill> {
        let mut guard = self.inner.write().expect("skill registry poisoned");
        guard.catalog.remove(id)
    }

    /// Count of enabled skills.
    pub fn count(&self) -> usize {
        let guard = self.inner.read().expect("skill registry poisoned");
        guard
            .catalog
            .keys()
            .filter(|id| !guard.disabled.contains(id.as_str()))
            .count()
    }

    /// Whether there are no enabled skills.
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }
}

/// Tool that invokes a registered skill.
#[derive(Debug, Clone, Default)]
pub struct SkillTool;

#[async_trait]
impl Tool for SkillTool {
    fn id(&self) -> ToolId {
        ToolId::new(SKILL_TOOL_NAME)
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["SkillTool"]
    }

    fn description(&self) -> &str {
        "Execute a skill within the main conversation.\n\
         \n\
         When users ask you to perform tasks, check if any of the available skills match. \
         Skills provide specialized capabilities and domain knowledge.\n\
         \n\
         When users reference a \"slash command\" or \"/<something>\" (e.g., \"/commit\", \
         \"/review-pr\"), they are referring to a skill. Use this tool to invoke it.\n\
         \n\
         How to invoke:\n\
         - Use this tool with the skill name and optional arguments\n\
         - Examples: `skill: \"commit\"`, `skill: \"commit\", args: \"-m 'Fix bug'\"`\n\
         \n\
         Important:\n\
         - Available skills are listed in system-reminder messages in the conversation\n\
         - When a skill matches the user's request, invoke the relevant Skill tool BEFORE \
         generating any other response about the task\n\
         - NEVER mention a skill without actually calling this tool\n\
         - Do not invoke a skill that is already running"
    }

    fn input_schema(&self) -> ToolInputSchema {
        json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "Skill id."
                },
                "variables": {
                    "type": "object",
                    "description": "Variables substituted into the skill prompt template."
                },
                "args": {
                    "type": "string",
                    "description": "Optional slash-command arguments for the skill."
                }
            },
            "required": ["skill"],
            "additionalProperties": false
        })
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _context: &ToolContext,
    ) -> ToolResult<ValidationOutcome> {
        match parse_input(input) {
            Ok(_) => Ok(ValidationOutcome::valid()),
            Err(message) => Ok(ValidationOutcome::invalid(message, INVALID_INPUT_CODE)),
        }
    }

    async fn call(&self, input: Value, context: &ToolContext) -> ToolResult<Value> {
        let (skill_id, variables, args) =
            parse_input(&input).map_err(|reason| ToolError::InvalidInput {
                tool: self.id(),
                reason,
                error_code: Some(INVALID_INPUT_CODE),
            })?;

        let registry = SkillContext::registry_of(context).ok_or_else(|| ToolError::Execution {
            tool: self.id(),
            source: anyhow::anyhow!("SkillTool requires a SkillRegistry on the ToolContext"),
        })?;

        let skill = match registry.get(&skill_id) {
            Some(skill) => skill,
            None => {
                let reason = if registry.is_disabled(&skill_id)
                    && registry
                        .all_entries()
                        .iter()
                        .any(|skill| skill.id == skill_id)
                {
                    format!("skill is disabled: {skill_id}")
                } else {
                    format!("unknown skill: {skill_id}")
                };
                return Err(ToolError::InvalidInput {
                    tool: self.id(),
                    reason,
                    error_code: Some(INVALID_INPUT_CODE),
                });
            }
        };

        if let Some(missing) = first_unoffered_tool(&skill, context) {
            return Err(ToolError::InvalidInput {
                tool: self.id(),
                reason: format!(
                    "skill {skill_id} needs the {missing} tool, which this session does not offer \
                     with the current provider or tool settings"
                ),
                error_code: Some(INVALID_INPUT_CODE),
            });
        }

        let rendered = skill.render_invocation(args.as_deref(), &variables);
        Ok(json!({
            "skill": skill.id,
            "title": skill.title,
            "description": skill.description,
            "prompt": rendered,
            "suggested_tools": skill.suggested_tools,
        }))
    }
}

/// The first of `skill`'s required tools this call's turn cannot run.
///
/// The listing already withholds such a skill, but a listing is history: a
/// session listed `imagegen` while on OpenAI still names it after `/model`
/// moves it elsewhere. Resolved against the call's own resolver and filter —
/// the same answer dispatch would give — and a context without a resolver
/// runs nothing, so every requirement is missing there.
fn first_unoffered_tool<'a>(skill: &'a Skill, context: &ToolContext) -> Option<&'a str> {
    skill
        .required_tools
        .iter()
        .find(|name| {
            let Some(resolver) = context.tool_resolver() else {
                return true;
            };
            !matches!(
                resolver.resolve(name, context.tool_filter()),
                Ok(Some(tool)) if tool.is_enabled()
            )
        })
        .map(String::as_str)
}

fn parse_input(input: &Value) -> Result<(String, Map<String, Value>, Option<String>), String> {
    let object = input
        .as_object()
        .ok_or_else(|| "SkillTool input must be a JSON object".to_string())?;
    let skill = object
        .get("skill")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "`skill` is required".to_string())?
        .trim()
        .trim_start_matches('/')
        .to_string();
    if skill.is_empty() {
        return Err("`skill` must not be empty".to_string());
    }
    let variables = match object.get("variables") {
        Some(Value::Object(map)) => map.clone(),
        Some(_) => return Err("`variables` must be an object".to_string()),
        None => Map::new(),
    };
    let args = match object.get("args") {
        Some(Value::String(args)) => Some(args.clone()),
        Some(_) => return Err("`args` must be a string".to_string()),
        None => None,
    };
    Ok((skill, variables, args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sample_registry() -> Arc<SkillRegistry> {
        let registry = SkillRegistry::new();
        registry.register(Skill {
            id: "greet".into(),
            title: "Greet the user".into(),
            description: "Generate a friendly greeting".into(),
            prompt_template: "Say hello to {{ name }} in {{ style }}.".into(),
            suggested_tools: vec!["Write".into()],
            source: SkillSource::User,
            argument_hint: None,
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable: true,
            disable_model_invocation: false,
            required_tools: Vec::new(),
        });
        Arc::new(registry)
    }

    #[tokio::test]
    async fn skill_tool_renders_registered_skill_prompt() {
        let registry = sample_registry();
        let tool = SkillTool;
        let context = ToolContext::new().with_extension(SkillContext::with_registry(registry));
        let out = tool
            .call(
                json!({
                    "skill": "greet",
                    "variables": { "name": "Claude", "style": "pirate speak" }
                }),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(out["skill"], "greet");
        assert_eq!(out["prompt"], "Say hello to Claude in pirate speak.");
        assert_eq!(out["suggested_tools"][0], "Write");
    }

    #[tokio::test]
    async fn skill_tool_leaves_unresolved_placeholders_verbatim() {
        let registry = sample_registry();
        let tool = SkillTool;
        let context = ToolContext::new().with_extension(SkillContext::with_registry(registry));
        let out = tool
            .call(
                json!({
                    "skill": "greet",
                    "variables": { "name": "Claude" }
                }),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(out["prompt"], "Say hello to Claude in {{ style }}.");
    }

    #[tokio::test]
    async fn skill_tool_applies_slash_args() {
        let registry = sample_registry();
        let tool = SkillTool;
        let context = ToolContext::new().with_extension(SkillContext::with_registry(registry));
        let out = tool
            .call(
                json!({ "skill": "/greet", "args": "extra request" }),
                &context,
            )
            .await
            .unwrap();
        assert!(out["prompt"]
            .as_str()
            .unwrap()
            .contains("ARGUMENTS: extra request"));
    }

    #[tokio::test]
    async fn skill_tool_errors_on_unknown_skill() {
        let registry = sample_registry();
        let tool = SkillTool;
        let context = ToolContext::new().with_extension(SkillContext::with_registry(registry));
        let err = tool
            .call(json!({ "skill": "missing" }), &context)
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput { reason, .. } => {
                assert_eq!(reason, "unknown skill: missing");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn skill_tool_reports_disabled_registered_skill_explicitly() {
        let registry = sample_registry();
        registry.set_disabled_skills(["greet"]);
        let tool = SkillTool;
        let context = ToolContext::new().with_extension(SkillContext::with_registry(registry));

        let err = tool
            .call(json!({ "skill": "greet" }), &context)
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput { reason, .. } => {
                assert_eq!(reason, "skill is disabled: greet");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn denylisted_but_unregistered_skill_is_still_unknown() {
        let registry = sample_registry();
        registry.set_disabled_skills(["missing"]);
        let tool = SkillTool;
        let context = ToolContext::new().with_extension(SkillContext::with_registry(registry));

        let err = tool
            .call(json!({ "skill": "missing" }), &context)
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput { reason, .. } => {
                assert_eq!(reason, "unknown skill: missing");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn skill_tool_errors_when_no_registry_is_injected() {
        let tool = SkillTool;
        let context = ToolContext::new();
        let err = tool
            .call(json!({ "skill": "greet" }), &context)
            .await
            .unwrap_err();
        match err {
            ToolError::Execution { source, .. } => {
                assert!(source.to_string().contains("SkillRegistry"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn skill_tool_validation_rejects_non_object_variables() {
        let tool = SkillTool;
        let context = ToolContext::new();
        let out = tool
            .validate_input(&json!({ "skill": "x", "variables": "nope" }), &context)
            .await
            .unwrap();
        assert!(!out.is_valid());
    }

    #[test]
    fn skill_render_handles_non_string_variables() {
        let skill = Skill {
            id: "x".into(),
            title: "x".into(),
            description: String::new(),
            prompt_template: "Count is {{ n }}".into(),
            suggested_tools: Vec::new(),
            source: SkillSource::BuiltIn,
            argument_hint: None,
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable: true,
            disable_model_invocation: false,
            required_tools: Vec::new(),
        };
        let mut vars = Map::new();
        vars.insert("n".into(), json!(7));
        assert_eq!(skill.render(&vars), "Count is 7");
    }

    #[test]
    fn skill_render_handles_dangling_open_brace() {
        let skill = Skill {
            id: "x".into(),
            title: "x".into(),
            description: String::new(),
            prompt_template: "Hello {{ name".into(),
            suggested_tools: Vec::new(),
            source: SkillSource::BuiltIn,
            argument_hint: None,
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable: true,
            disable_model_invocation: false,
            required_tools: Vec::new(),
        };
        let mut vars = Map::new();
        vars.insert("name".into(), json!("Claude"));
        assert_eq!(skill.render(&vars), "Hello {{ name");
    }

    #[test]
    fn skill_registry_round_trips_register_get_remove() {
        let reg = SkillRegistry::new();
        assert!(reg.is_empty());
        reg.register(Skill {
            id: "a".into(),
            title: "A".into(),
            description: String::new(),
            prompt_template: String::new(),
            suggested_tools: Vec::new(),
            source: SkillSource::Project,
            argument_hint: None,
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable: true,
            disable_model_invocation: false,
            required_tools: Vec::new(),
        });
        assert!(!reg.is_empty());
        assert_eq!(reg.count(), 1);
        assert!(reg.get("a").is_some());
        assert_eq!(reg.ids(), vec!["a".to_string()]);
        let entries = reg.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "a");
        assert_eq!(entries[0].source, SkillSource::Project);
        reg.register(Skill {
            id: "hidden".into(),
            title: "Hidden".into(),
            description: String::new(),
            prompt_template: String::new(),
            suggested_tools: Vec::new(),
            source: SkillSource::Project,
            argument_hint: None,
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable: false,
            disable_model_invocation: false,
            required_tools: Vec::new(),
        });
        let user_entries = reg.user_invocable_entries();
        assert_eq!(user_entries.len(), 1);
        assert_eq!(user_entries[0].id, "a");
        assert!(reg.remove("a").is_some());
        assert!(reg.remove("hidden").is_some());
        assert!(reg.is_empty());
        assert_eq!(reg.count(), 0);
    }

    #[test]
    fn skill_registry_keeps_disabled_catalog_entries_but_hides_enabled_views() {
        let registry = SkillRegistry::new();
        registry.set_disabled_skills(["  disabled  ", "future", "", "disabled"]);

        let mut disabled = sample_registry().get("greet").unwrap();
        disabled.id = "disabled".to_string();
        disabled.title = "Disabled".to_string();
        registry.register(disabled);

        let mut enabled = sample_registry().get("greet").unwrap();
        enabled.id = "enabled".to_string();
        enabled.title = "Enabled".to_string();
        registry.register(enabled);

        assert_eq!(
            registry.disabled_skills(),
            BTreeSet::from(["disabled".to_string(), "future".to_string()])
        );
        assert!(registry.is_disabled("disabled"));
        assert!(registry.is_disabled("future"));
        assert!(!registry.is_disabled("enabled"));
        assert!(registry.get("disabled").is_none());
        assert_eq!(registry.get("enabled").unwrap().title, "Enabled");
        assert_eq!(registry.ids(), vec!["enabled".to_string()]);
        assert_eq!(
            registry
                .entries()
                .into_iter()
                .map(|skill| skill.id)
                .collect::<Vec<_>>(),
            vec!["enabled".to_string()]
        );
        assert_eq!(
            registry
                .user_invocable_entries()
                .into_iter()
                .map(|skill| skill.id)
                .collect::<Vec<_>>(),
            vec!["enabled".to_string()]
        );
        assert_eq!(registry.user_invocable_count(), 1);
        assert_eq!(registry.count(), 1);
        assert_eq!(
            registry
                .all_entries()
                .into_iter()
                .map(|skill| skill.id)
                .collect::<Vec<_>>(),
            vec!["disabled".to_string(), "enabled".to_string()]
        );

        registry.set_disabled_skills(["disabled", "enabled"]);
        assert!(registry.is_empty());
        assert_eq!(registry.count(), 0);
        assert_eq!(registry.all_entries().len(), 2);
    }

    // ── required tools ────────────────────────────────────────────

    struct NamedTool {
        name: &'static str,
        enabled: bool,
    }

    #[async_trait]
    impl Tool for NamedTool {
        fn id(&self) -> ToolId {
            ToolId::new(self.name)
        }

        fn description(&self) -> &str {
            "fixture"
        }

        fn input_schema(&self) -> ToolInputSchema {
            json!({ "type": "object" })
        }

        fn is_enabled(&self) -> bool {
            self.enabled
        }

        async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(Value::Null)
        }
    }

    struct Resolver(Option<Arc<dyn Tool>>);

    impl rebon_tool::ToolResolver for Resolver {
        fn resolve(
            &self,
            name: &str,
            _filter: Option<&rebon_tool::ToolFilter>,
        ) -> ToolResult<Option<Arc<dyn Tool>>> {
            Ok(self.0.clone().filter(|tool| tool.id().as_str() == name))
        }

        fn tools(
            &self,
            _filter: Option<&rebon_tool::ToolFilter>,
        ) -> ToolResult<Vec<Arc<dyn Tool>>> {
            Ok(self.0.clone().into_iter().collect())
        }
    }

    fn imagegen_context(resolver: Option<Resolver>) -> ToolContext {
        let registry = SkillRegistry::new();
        let mut skill = sample_registry().get("greet").unwrap();
        skill.id = "imagegen".into();
        skill.required_tools = vec!["ImageGen".into()];
        registry.register(skill);
        let context =
            ToolContext::new().with_extension(SkillContext::with_registry(Arc::new(registry)));
        match resolver {
            Some(resolver) => context.with_tool_resolver(Arc::new(resolver)),
            None => context,
        }
    }

    async fn invoke(context: &ToolContext) -> ToolResult<Value> {
        SkillTool
            .call(json!({ "skill": "imagegen" }), context)
            .await
    }

    fn refusal(result: ToolResult<Value>) -> String {
        match result {
            Err(ToolError::InvalidInput { reason, .. }) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_skill_runs_when_its_required_tool_resolves_enabled() {
        let tool: Arc<dyn Tool> = Arc::new(NamedTool {
            name: "ImageGen",
            enabled: true,
        });
        let out = invoke(&imagegen_context(Some(Resolver(Some(tool)))))
            .await
            .expect("the tool is offered");
        assert_eq!(out["skill"], "imagegen");
    }

    #[tokio::test]
    async fn a_skill_is_refused_when_its_required_tool_is_absent() {
        let reason = refusal(invoke(&imagegen_context(Some(Resolver(None)))).await);
        assert!(reason.contains("needs the ImageGen tool"), "{reason}");
    }

    /// Registered but switched off by the provider — `ImageGen` off OpenAI.
    #[tokio::test]
    async fn a_skill_is_refused_when_its_required_tool_is_disabled() {
        let tool: Arc<dyn Tool> = Arc::new(NamedTool {
            name: "ImageGen",
            enabled: false,
        });
        let reason = refusal(invoke(&imagegen_context(Some(Resolver(Some(tool))))).await);
        assert!(reason.contains("ImageGen"), "{reason}");
    }

    #[tokio::test]
    async fn a_context_without_a_resolver_offers_no_required_tool() {
        let reason = refusal(invoke(&imagegen_context(None)).await);
        assert!(reason.contains("ImageGen"), "{reason}");
    }
}
