//! `AgentRegistry` — merged view of built-in, plugin, user, project,
//! flag and managed agent definitions.
//!
//! ## Why this exists
//!
//! Without this registry, the `Agent` tool ([`crate::agent`])
//! would call [`crate::builtin_agents::resolve_builtin_agent`]
//! directly — a hard-coded `match` over the seven compiled-in agent
//! types. The user's `~/.rebon/agents/*.md` and the project's
//! `<cwd>/.rebon/agents/*.md` would never be considered, so a
//! workspace stub like `~/.rebon/agents/rebon-code-guide.md` would
//! silently lose to the built-in definition.
//!
//! The registry keeps agents in priority order — later sources
//! override earlier ones — so the same
//! override file flows through both the description shown to the
//! parent model and the per-spawn resolution path.
//!
//! ## Source priority
//!
//! ```text
//! groups, in priority order:
//!   builtIns
//!   plugins
//!   user
//!   project
//!   flag
//!   managed
//! ```
//!
//! Sources are merged in that order,
//! so the **last** group to define a given `agent_type` wins. This is
//! done with a `Vec<ResolvedAgentDef>` re-ordered by
//! priority and deduped through a `BTreeMap<String, …>` keyed on
//! `agent_type`.
//!
//! Plugin agents are not yet implemented (the rebon plugin loader doesn't
//! exist) so the slot is reserved but currently always empty. The
//! `from_groups` constructor still lays them out in the right slot so
//! the priority story doesn't shift when the plugin loader lands.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::builtin_agents::{all_builtin_agents, format_agent_line, BuiltInAgentDef};
use crate::ToolFilter;

/// Provenance of an entry in the registry: a compiled-in built-in, a
/// plugin, or one of the [`SettingSource`] buckets.
///
/// `SettingSource` itself is represented by [`SettingSource`] below.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AgentSource {
    /// Compiled-in built-in agent.
    BuiltIn,
    /// Loaded from a plugin. The string is the plugin's name.
    Plugin(String),
    /// Loaded from a settings source (markdown file on disk, or a
    /// JSON `agents` block in a settings file).
    Settings(SettingSource),
}

/// One member of the persisted `SettingSource` union. Ordering of the variants here
/// is `Ord`-meaningful only inside this module's tests; the actual
/// override priority is encoded in [`AgentRegistry::from_groups`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SettingSource {
    /// `~/.rebon/settings.json` + `~/.rebon/agents/*.md`.
    UserSettings,
    /// `<cwd>/.rebon/settings.json` + `<cwd>/.rebon/agents/*.md`.
    ProjectSettings,
    /// `<cwd>/.rebon/settings.local.json`. Reserved — not yet wired.
    LocalSettings,
    /// `--settings` CLI flag. Reserved — not yet wired.
    FlagSettings,
    /// Managed (org-pushed) settings. Reserved — not yet wired.
    PolicySettings,
}

/// Which agent runtime executes a definition.
///
/// The overwhelming majority of agents are [`Self::Local`]: a system
/// prompt and a tool filter handed to Rebon's own engine. An agent can
/// instead name a third-party CLI to run the turn, in which case Rebon
/// drives it over ACP and relays what comes back.
///
/// ```text
/// ---
/// name: gemini
/// description: Google's Gemini CLI, driven over ACP
/// runtime: acp
/// command: gemini
/// args: --experimental-acp
/// ---
/// ```
///
/// An external agent brings its own model, its own tools, and its own
/// system prompt — so `model:`, `tools:`, and the prompt body are not
/// its configuration and Rebon does not pretend otherwise. What Rebon
/// still owns is where its file writes land and who approves them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRuntime {
    /// Rebon's own engine runs the turn.
    Local,
    /// A third-party agent CLI runs the turn, driven over ACP.
    Acp {
        /// Executable to spawn.
        command: String,
        /// Arguments passed to it.
        args: Vec<String>,
    },
}

impl Default for AgentRuntime {
    fn default() -> Self {
        Self::Local
    }
}

impl AgentRuntime {
    /// Short label for the agent list.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Acp { .. } => "acp",
        }
    }

    /// Whether this agent runs outside Rebon's process.
    pub fn is_external(&self) -> bool {
        matches!(self, Self::Acp { .. })
    }

    /// The command line, for the agent list and for logs.
    pub fn command_line(&self) -> Option<String> {
        match self {
            Self::Local => None,
            Self::Acp { command, args } if args.is_empty() => Some(command.clone()),
            Self::Acp { command, args } => Some(format!("{command} {}", args.join(" "))),
        }
    }
}

/// One agent the registry can resolve. Owned, not borrowed — the
/// registry is constructed once and shared across the engine via
/// `Arc<AgentRegistry>`.
///
/// This type is intentionally a superset of [`BuiltInAgentDef`] —
/// every built-in flows through the same struct so callers don't need
/// to branch on source after lookup.
#[derive(Debug, Clone)]
pub struct ResolvedAgentDef {
    pub agent_type: String,
    pub when_to_use: String,
    pub system_prompt: String,
    pub tool_filter: ToolFilter,
    pub model: Option<String>,
    pub model_profile: Option<String>,
    pub provider: Option<String>,
    pub effort: Option<String>,
    pub background: bool,
    pub isolation: Option<String>,
    /// Per-agent persistent memory scope — `"user"`, `"project"`, or
    /// `"local"`. Corresponds to the `memory` frontmatter field.
    pub memory: Option<String>,
    /// Optional permission mode override propagated to sub-agent /
    /// background runtime. One of `"default" | "plan" | "acceptEdits" |
    /// "bypassPermissions" | "auto"`. Corresponds to the `permissionMode`
    /// frontmatter field.
    pub permission_mode: Option<String>,
    /// Which runtime executes this agent. See [`AgentRuntime`].
    pub runtime: AgentRuntime,
    pub source: AgentSource,
    /// The file this agent was read from, without the `.md` extension,
    /// when it differs from [`Self::agent_type`]. `None` for a built-in
    /// or a plugin agent, which have no file, and for a file whose stem
    /// already is the agent type.
    ///
    /// Recorded here because only the loader knows it, and rediscovering
    /// it later means re-scanning the directory and re-parsing every
    /// candidate — which is what the agents dialog used to do, with a
    /// second frontmatter parser that disagreed with this one about a
    /// quoted `name:`.
    pub file_stem: Option<String>,
}

impl ResolvedAgentDef {
    /// Lift a compiled-in [`BuiltInAgentDef`] into a [`ResolvedAgentDef`].
    pub fn from_builtin(def: BuiltInAgentDef) -> Self {
        Self {
            agent_type: def.agent_type.to_string(),
            when_to_use: def.when_to_use.to_string(),
            system_prompt: def.system_prompt,
            tool_filter: def.tool_filter,
            model: def.model.map(str::to_string),
            model_profile: def.model_profile.map(str::to_string),
            provider: None,
            effort: None,
            background: def.background,
            isolation: def.isolation.map(str::to_string),
            memory: def.memory.map(str::to_string),
            permission_mode: def.permission_mode.map(str::to_string),
            // Every compiled-in agent is Rebon's engine by
            // construction; there is no built-in that shells out.
            runtime: AgentRuntime::Local,
            source: AgentSource::BuiltIn,
            // Compiled in, so there is no file to name.
            file_stem: None,
        }
    }

    /// Whether this agent appears in the list the model reads.
    ///
    /// Separate from [`Self::is_disabled`], which takes an agent out of the
    /// registry altogether. An agent that is not offered still resolves and
    /// still runs when something names it; it is only absent from the
    /// description the model chooses from. A compiled-in agent can opt out —
    /// see [`crate::builtin_agents::builtin_agent_is_offered_to_model`] —
    /// while anything a user, project or plugin configured is always offered,
    /// including a file that reinstates an unlisted built-in by name.
    pub fn is_offered_to_model(&self) -> bool {
        !matches!(self.source, AgentSource::BuiltIn)
            || crate::builtin_agents::builtin_agent_is_offered_to_model(&self.agent_type)
    }

    pub fn is_disabled(&self) -> bool {
        let trimmed = self.when_to_use.trim_start();
        let bytes = trimmed.as_bytes();
        bytes
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"disabled"))
            && matches!(bytes.get(8), None | Some(b'.' | b':' | b'-' | b' ' | b'\t'))
    }

    /// View as the legacy [`BuiltInAgentDef`] shape for callers that
    /// still expect it. Owned strings collapse to `&'static str` slots
    /// where possible — fields backed by parsed YAML have to leak
    /// onto the heap, but that's fine because the conversion is only
    /// used at the AgentTool dispatch boundary which already cares
    /// about `String`s.
    ///
    /// **NOTE:** the returned struct uses `Box::leak` to satisfy the
    /// `&'static str` lifetime on `BuiltInAgentDef`. This is safe
    /// because the registry itself is also long-lived (held in an
    /// `Arc` for the whole process lifetime), but the conversion is
    /// best avoided when a `ResolvedAgentDef` will do.
    ///
    /// `AgentTool` reads `ResolvedAgentDef` directly; this shim is what
    /// the `/agents` listing formats through.
    pub(crate) fn as_builtin_like(&self) -> BuiltInAgentDef {
        BuiltInAgentDef {
            agent_type: Box::leak(self.agent_type.clone().into_boxed_str()),
            when_to_use: Box::leak(self.when_to_use.clone().into_boxed_str()),
            system_prompt: self.system_prompt.clone(),
            tool_filter: self.tool_filter.clone(),
            model: self
                .model
                .as_deref()
                .map(|s| &*Box::leak(s.to_owned().into_boxed_str())),
            model_profile: self
                .model_profile
                .as_deref()
                .map(|s| &*Box::leak(s.to_owned().into_boxed_str())),
            background: self.background,
            isolation: self
                .isolation
                .as_deref()
                .map(|s| &*Box::leak(s.to_owned().into_boxed_str())),
            memory: self
                .memory
                .as_deref()
                .map(|s| &*Box::leak(s.to_owned().into_boxed_str())),
            permission_mode: self
                .permission_mode
                .as_deref()
                .map(|s| &*Box::leak(s.to_owned().into_boxed_str())),
        }
    }
}

/// Process-wide merged agent registry. Construct once at engine
/// wiring time and share via `Arc`.
#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    by_type: BTreeMap<String, ResolvedAgentDef>,
    /// The order callers should iterate when listing the registry.
    /// Built-ins first (declaration order from
    /// [`all_builtin_agents`]), then any custom agents the override
    /// pass added on top. When a custom agent overrides a built-in,
    /// we keep the built-in's slot in the listing — the resolved
    /// definition under that key is the override.
    listing_order: Vec<String>,
    /// Whether external-runtime agents count as spawnable. Off by
    /// default; the host that wires an external sub-agent runner turns
    /// it on — see [`Self::spawnable`].
    external_spawnable: bool,
}

impl AgentRegistry {
    /// Construct an empty registry. Useful for tests that want full
    /// control; production callers should use [`Self::builtins_only`]
    /// or [`Self::load`].
    pub fn empty() -> Self {
        Self::default()
    }

    /// Construct a registry that contains only the seven compiled-in
    /// built-in agents. Used by [`crate::AgentTool::new`] so existing
    /// tests that don't go through the loader keep working unchanged.
    pub fn builtins_only() -> Self {
        let groups = AgentGroups {
            built_in: all_builtin_agents()
                .into_iter()
                .map(ResolvedAgentDef::from_builtin)
                .collect(),
            ..Default::default()
        };
        Self::from_groups(groups)
    }

    /// Load the merged registry from disk. Reads built-ins +
    /// `<home_config_dir>/agents/*.md` (user) +
    /// `<cwd>/.rebon/agents/*.md` (project), then dedupes by
    /// priority.
    ///
    /// I/O failures (missing dir, unreadable file, malformed YAML)
    /// are logged via `tracing::debug!` but never propagated — the
    /// registry always returns at least the built-in set.
    pub fn load(cwd: &Path, home_config_dir: &Path) -> Self {
        Self::load_with_plugin_dirs(cwd, home_config_dir, &[])
    }

    /// Load the merged registry with plugin-provided agent directories in
    /// the reserved plugin priority bucket. Each entry names the plugin
    /// it came from — deriving the name from the path is not possible,
    /// because the installed layout is `<name>/<version>/agents` while a
    /// `--plugin-dir` layout is `<name>/agents`, and guessing the depth
    /// used to label agents with their plugin's *version*.
    pub fn load_with_plugin_dirs(
        cwd: &Path,
        home_config_dir: &Path,
        plugin_agent_dirs: &[(String, PathBuf)],
    ) -> Self {
        let built_in: Vec<ResolvedAgentDef> = all_builtin_agents()
            .into_iter()
            .map(ResolvedAgentDef::from_builtin)
            .collect();

        let plugin = plugin_agent_dirs
            .iter()
            .flat_map(|(plugin_name, dir)| {
                load_agents_dir(dir, AgentSource::Plugin(plugin_name.clone()))
            })
            .collect();
        let user = load_agents_dir(
            &home_config_dir.join("agents"),
            AgentSource::Settings(SettingSource::UserSettings),
        );
        let project = load_agents_dir(
            &cwd.join(".rebon").join("agents"),
            AgentSource::Settings(SettingSource::ProjectSettings),
        );

        Self::from_groups(AgentGroups {
            built_in,
            plugin,
            user,
            project,
            flag: Vec::new(),
            managed: Vec::new(),
        })
    }

    /// Merge the input groups into a registry, applying override
    /// priority.
    pub fn from_groups(groups: AgentGroups) -> Self {
        // Build the listing order from the union of all sources, in
        // priority order, recording each `agent_type` exactly once
        // (first appearance wins for ordering, last appearance wins
        // for the resolved definition).
        let mut listing_order: Vec<String> = Vec::new();
        let mut seen_in_listing: BTreeSet<String> = BTreeSet::new();
        let priority_groups = [
            &groups.built_in,
            &groups.plugin,
            &groups.user,
            &groups.project,
            &groups.flag,
            &groups.managed,
        ];
        for group in &priority_groups {
            for def in group.iter() {
                if seen_in_listing.insert(def.agent_type.clone()) {
                    listing_order.push(def.agent_type.clone());
                }
            }
        }

        let mut by_type: BTreeMap<String, ResolvedAgentDef> = BTreeMap::new();
        for group in &priority_groups {
            for def in group.iter() {
                by_type.insert(def.agent_type.clone(), def.clone());
            }
        }

        Self {
            by_type,
            listing_order,
            external_spawnable: false,
        }
    }

    /// Count external-runtime agents as spawnable sub-agent types.
    ///
    /// Only for hosts that actually wire an external sub-agent runner
    /// into their spawner — with the flag on and no runner, dispatch
    /// falls back to refusing at spawn time rather than degrading.
    pub fn with_external_agents_spawnable(mut self) -> Self {
        self.external_spawnable = true;
        self
    }

    /// Number of distinct agent types in the registry.
    pub fn len(&self) -> usize {
        self.by_type.len()
    }

    /// Whether the registry has no resolvable agent types.
    pub fn is_empty(&self) -> bool {
        self.by_type.is_empty()
    }

    /// Resolve an `agent_type` string to its winning definition.
    /// Returns `None` for unknown types — the AgentTool falls through
    /// to the general-purpose path in that case.
    pub fn resolve(&self, agent_type: &str) -> Option<&ResolvedAgentDef> {
        self.by_type.get(agent_type).or_else(|| match agent_type {
            "explore" | "Explorer" | "explorer" => self.by_type.get("Explore"),
            _ => None,
        })
    }

    /// Iterate the active agents in listing order: built-ins first
    /// (declaration order), then any custom-only types appended.
    pub fn active(&self) -> impl Iterator<Item = &ResolvedAgentDef> {
        self.listing_order
            .iter()
            .filter_map(move |t| self.by_type.get(t))
            .filter(|def| !def.is_disabled())
    }

    /// Snapshot the active agents in listing order without exposing
    /// registry internals or forcing callers through formatted prompt text.
    pub fn active_snapshot(&self) -> Vec<ResolvedAgentDef> {
        self.active().cloned().collect()
    }

    /// Active agents that the sub-agent spawner can actually run.
    ///
    /// An [`AgentRuntime::Acp`] agent is a separate process reached
    /// over a protocol. Hosts that wire an external sub-agent runner
    /// into their spawner can run one as a `subagent_type` — those
    /// hosts opt in via [`Self::with_external_agents_spawnable`], and
    /// dispatch delegates the whole task to the agent's CLI. The flag
    /// defaults to off because a host *without* the runner (bare test
    /// harnesses, older wirings) would advertise a delegation target
    /// that silently degrades into a general-purpose local worker with
    /// an empty system prompt; with the flag off such agents are
    /// excluded here and refused at dispatch.
    ///
    /// External agents always appear in [`Self::active`] — the user
    /// configured them and should see them in the agent list.
    pub fn spawnable(&self) -> impl Iterator<Item = &ResolvedAgentDef> {
        self.active()
            .filter(move |def| self.external_spawnable || !def.runtime.is_external())
    }

    /// Format the active agents as the newline-separated list shown
    /// in [`crate::AgentTool`]'s description.
    pub fn format_lines(&self) -> String {
        self.spawnable()
            .filter(|def| def.is_offered_to_model())
            .map(|def| format_agent_line(&def.as_builtin_like()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn format_lines_without_worktree_agents(&self) -> String {
        self.spawnable()
            .filter(|def| def.is_offered_to_model())
            .filter(|def| {
                !def.isolation
                    .as_deref()
                    .is_some_and(|mode| mode.eq_ignore_ascii_case("worktree"))
            })
            .map(|def| format_agent_line(&def.as_builtin_like()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Convenience: wrap `self` in an `Arc` for sharing across the
    /// engine wiring.
    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

/// Source-bucketed input to [`AgentRegistry::from_groups`].
///
/// The six setting-source buckets. Plugin /
/// flag / managed are reserved for future support — the production
/// loader currently leaves them empty.
#[derive(Debug, Default, Clone)]
pub struct AgentGroups {
    pub built_in: Vec<ResolvedAgentDef>,
    pub plugin: Vec<ResolvedAgentDef>,
    pub user: Vec<ResolvedAgentDef>,
    pub project: Vec<ResolvedAgentDef>,
    pub flag: Vec<ResolvedAgentDef>,
    pub managed: Vec<ResolvedAgentDef>,
}

/// Walk a directory of `*.md` agent files and parse each one. Errors
/// are swallowed (logged at debug) — see [`AgentRegistry::load`].
fn load_agents_dir(dir: &Path, source: AgentSource) -> Vec<ResolvedAgentDef> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(err) => {
            tracing::debug!(
                target: "rebon_tool::agent_registry",
                ?dir,
                error = %err,
                "skipping agents dir"
            );
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for entry in read.flatten() {
        let path: PathBuf = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(err) => {
                tracing::debug!(
                    target: "rebon_tool::agent_registry",
                    ?path,
                    error = %err,
                    "failed to read agent file"
                );
                continue;
            }
        };
        match parse_agent_md(&path, &content, source.clone()) {
            Ok(Some(def)) => out.push(def),
            Ok(None) => {
                tracing::debug!(
                    target: "rebon_tool::agent_registry",
                    ?path,
                    "skipping markdown file without agent frontmatter"
                );
            }
            Err(reason) => {
                tracing::debug!(
                    target: "rebon_tool::agent_registry",
                    ?path,
                    %reason,
                    "failed to parse agent markdown"
                );
            }
        }
    }
    out
}

/// Parse one `*.md` file into a [`ResolvedAgentDef`].
///
/// Parse the subset of the agent-markdown grammar this registry honours.
/// Fields not yet honoured by the
/// runtime (`hooks`, `mcpServers`, `maxTurns`, `skills`, `acpCommand`,
/// `acpAgent`, `runtime`) are ignored silently — the file still loads,
/// the unhandled keys are just dropped.
///
/// Returns:
/// - `Ok(Some(def))` on a valid agent file.
/// - `Ok(None)` for a markdown file that lacks a `name:` field
///   entirely (treated as co-located reference docs
///   rather than agent attempts).
/// - `Err(reason)` for files that have a `name:` but are otherwise
///   malformed (missing `description`, etc.).
fn parse_agent_md(
    path: &Path,
    content: &str,
    source: AgentSource,
) -> Result<Option<ResolvedAgentDef>, String> {
    let Some((frontmatter_lines, body)) = split_frontmatter(content) else {
        // No `---` fence at all → not an agent file. Silently skip.
        return Ok(None);
    };

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut tools_raw: Option<String> = None;
    let mut disallowed_raw: Option<String> = None;
    let mut model: Option<String> = None;
    let mut model_profile: Option<String> = None;
    let mut provider: Option<String> = None;
    let mut effort: Option<String> = None;
    let mut memory: Option<String> = None;
    let mut isolation: Option<String> = None;
    let mut background: bool = false;
    let mut permission_mode: Option<String> = None;
    let mut runtime_raw: Option<String> = None;
    let mut command: Option<String> = None;
    let mut args_raw: Option<String> = None;

    for line in frontmatter_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').to_string();
        match key {
            "name" => name = non_empty(value),
            "description" => description = non_empty(decode_yaml_double_quoted(&value)),
            "tools" => tools_raw = non_empty(value),
            "disallowedTools" => disallowed_raw = non_empty(value),
            "model" => model = non_empty(value),
            "modelProfile" | "model_profile" => model_profile = non_empty(value),
            "provider" => provider = non_empty(value),
            "effort" | "reasoning_effort" | "reasoningEffort" | "variant" => {
                effort = non_empty(value)
            }
            "memory" => memory = non_empty(value),
            "isolation" => isolation = non_empty(value),
            "background" => background = matches!(value.as_str(), "true"),
            "permissionMode" | "permission_mode" => {
                permission_mode = non_empty(value).and_then(normalize_permission_mode)
            }
            "runtime" => runtime_raw = non_empty(value),
            "command" => command = non_empty(value),
            "args" => args_raw = non_empty(value),
            _ => {}
        }
    }

    if name.is_none() {
        // No `name:` → treat as a co-located reference doc.
        return Ok(None);
    }
    let name = name.unwrap();

    let description = description.ok_or_else(|| {
        format!(
            "agent file {} is missing required `description` field",
            path.display()
        )
    })?;

    let runtime = parse_agent_runtime(path, runtime_raw.as_deref(), command, args_raw.as_deref())?;

    let system_prompt = body.trim().to_string();
    // An external agent's system prompt belongs to the external agent.
    // Requiring one here would force users to write a body that is
    // never sent anywhere.
    if system_prompt.is_empty() && !runtime.is_external() {
        // Enforces the same non-empty-prompt rule as the input schema.
        return Err(format!(
            "agent file {} has an empty system prompt body",
            path.display()
        ));
    }

    let tool_filter = build_tool_filter(tools_raw.as_deref(), disallowed_raw.as_deref());

    // Only worth carrying when it differs: the dialog uses it to find the
    // file again, and a stem equal to the agent type is the path it would
    // have computed anyway.
    let file_stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| *stem != name)
        .map(str::to_string);

    Ok(Some(ResolvedAgentDef {
        agent_type: name,
        when_to_use: description,
        system_prompt,
        tool_filter,
        model,
        model_profile,
        provider,
        effort,
        background,
        isolation,
        memory,
        permission_mode,
        runtime,
        source,
        file_stem,
    }))
}

/// Turn the `runtime:` / `command:` / `args:` trio into an
/// [`AgentRuntime`].
///
/// Rejects rather than guesses. An unknown `runtime:` value is a typo
/// the user needs to see, and `runtime: acp` with no `command:` names
/// an agent that cannot be started — silently falling back to the
/// local engine would run a completely different agent than the file
/// asked for.
fn parse_agent_runtime(
    path: &Path,
    runtime_raw: Option<&str>,
    command: Option<String>,
    args_raw: Option<&str>,
) -> Result<AgentRuntime, String> {
    let Some(runtime_raw) = runtime_raw else {
        // No `runtime:` is the overwhelmingly common case, and a
        // `command:` without one is not an ACP agent — it is a
        // frontmatter key this registry does not use.
        return Ok(AgentRuntime::Local);
    };
    match runtime_raw.trim().to_ascii_lowercase().as_str() {
        "local" | "engine" | "rebon" => Ok(AgentRuntime::Local),
        "acp" | "external" => {
            let command = command.ok_or_else(|| {
                format!(
                    "agent file {} sets `runtime: acp` but no `command:` to run",
                    path.display()
                )
            })?;
            Ok(AgentRuntime::Acp {
                command,
                args: parse_agent_args(args_raw),
            })
        }
        other => Err(format!(
            "agent file {} has unknown `runtime: {other}` (expected `local` or `acp`)",
            path.display()
        )),
    }
}

/// Split an `args:` line into arguments.
///
/// Accepts the YAML flow sequence (`[--acp, --quiet]`) and the plain
/// space-separated form (`--acp --quiet`), because both are what
/// people type. Quoting is not supported — an argument containing a
/// space needs a wrapper script, which is clearer than a half-working
/// shell parser.
fn parse_agent_args(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'));
    let parts: Vec<String> = match inner {
        Some(inner) => inner
            .split(',')
            .map(|part| part.trim().trim_matches(['"', '\'']).to_string())
            .collect(),
        None => trimmed
            .split_whitespace()
            .map(|part| part.trim_matches(['"', '\'']).to_string())
            .collect(),
    };
    parts.into_iter().filter(|part| !part.is_empty()).collect()
}

/// Normalize a frontmatter `permissionMode` string to the canonical
/// runtime spelling. Accepts the camelCase / snake_case / kebab-case
/// variants users commonly type. Returns `None` for unrecognized values
/// so that typos don't silently change the runtime permission mode.
fn normalize_permission_mode(raw: String) -> Option<String> {
    let canon = raw.trim().to_ascii_lowercase().replace('-', "_");
    let mapped = match canon.as_str() {
        "default" => "default",
        "plan" => "plan",
        "acceptedits" | "accept_edits" | "accept" => "acceptEdits",
        "bypasspermissions" | "bypass_permissions" | "bypass" => "bypassPermissions",
        "auto" => "auto",
        _ => return None,
    };
    Some(mapped.to_string())
}

/// Build a [`ToolFilter`] from the YAML `tools:` (allow list) and
/// `disallowedTools:` (deny list) values. Either may be absent.
///
/// - `tools: a, b, c` → allow only `[a, b, c]`.
/// - `tools: "*"` or omitted → unrestricted.
/// - `disallowedTools: x, y` → deny `[x, y]` on top of whatever the
///   allow list resolved to.
fn build_tool_filter(tools_raw: Option<&str>, disallowed_raw: Option<&str>) -> ToolFilter {
    let allow_items: Option<Vec<String>> = tools_raw.map(parse_csv_list);
    let deny_items: Vec<String> = disallowed_raw.map(parse_csv_list).unwrap_or_default();

    let mut filter = match allow_items.as_deref() {
        None => ToolFilter::unrestricted(),
        Some(items) if items.iter().any(|t| t == "*") || items.is_empty() => {
            ToolFilter::unrestricted()
        }
        Some(items) => ToolFilter::allow_only(items.iter().cloned()),
    };
    if !deny_items.is_empty() {
        filter = filter.with_deny(deny_items);
    }
    filter
}

fn parse_csv_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn non_empty(s: String) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Returns `(frontmatter_lines, body)` if the content begins with a
/// `---` YAML fence; `None` otherwise.
fn split_frontmatter(content: &str) -> Option<(Vec<&str>, String)> {
    let mut lines = content.lines();
    if lines.next()? != "---" {
        return None;
    }
    let mut frontmatter = Vec::new();
    let mut body: Vec<&str> = Vec::new();
    let mut in_frontmatter = true;
    for line in lines {
        if in_frontmatter && line == "---" {
            in_frontmatter = false;
            continue;
        }
        if in_frontmatter {
            frontmatter.push(line);
        } else {
            body.push(line);
        }
    }
    if in_frontmatter {
        // No closing fence — treat as malformed (no agent content).
        return None;
    }
    Some((frontmatter, body.join("\n")))
}

/// Encode a string as the body of a YAML double-quoted scalar.
///
/// The `description:` field of an agent file is the only place Rebon
/// writes one, and [`decode_yaml_double_quoted`] is what reads it back.
/// The pair lives here, next to the rest of the agent-markdown grammar
/// this module already owns, rather than beside the writer that sits above
/// this crate: a decoder written there could not be reached from here, and
/// the two halves drifting apart is exactly how an agent description with a
/// line break stops surviving a save and a reload.
///
/// Order matters: escape backslashes FIRST, then quotes, then
/// newlines. Otherwise the second escape would double-escape the
/// first — which is also why a line break leaves as a backslash
/// followed by `n` rather than the single `\n` YAML would accept, and
/// why the decoder accepts both spellings.
pub fn escape_yaml_double_quoted(input: &str) -> String {
    // Step 1: backslashes
    let step1 = input.replace('\\', "\\\\");
    // Step 2: double quotes
    let step2 = step1.replace('"', "\\\"");
    // Step 3: newlines → literal `\\n`
    step2.replace('\n', "\\\\n")
}

/// Decode a YAML double-quoted scalar.
///
/// Mirrors [`escape_yaml_double_quoted`], which escapes backslashes
/// before newlines and so writes a line break as a backslash followed
/// by "\n". Reading that pair back as one newline is what lets an agent
/// description with a line break survive a save and a reload.
pub fn decode_yaml_double_quoted(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => {
                if chars.peek() == Some(&'n') {
                    let _ = chars.next();
                    out.push('\n');
                } else {
                    out.push('\\');
                }
            }
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn a_written_line_break_survives_being_read_back() {
        // The writer escapes backslashes first, so a newline reaches the
        // file as two characters: a backslash and an n.
        assert_eq!(decode_yaml_double_quoted(r"a\\nb"), "a\nb");
        assert_eq!(decode_yaml_double_quoted(r"a\nb"), "a\nb");
        assert_eq!(decode_yaml_double_quoted(r#"say \"hi\""#), "say \"hi\"");
        assert_eq!(decode_yaml_double_quoted(r"C:\\tmp"), "C:\\tmp");
    }

    /// The escape rules, pinned beside the decoder so the two halves cannot
    /// drift apart.
    #[test]
    fn the_escape_rules_run_backslash_then_quote_then_newline() {
        assert_eq!(escape_yaml_double_quoted("hello"), "hello");
        assert_eq!(escape_yaml_double_quoted("a\\b"), "a\\\\b");
        assert_eq!(escape_yaml_double_quoted("a\"b"), "a\\\"b");
        // newline → literal `\\n` (two backslashes + n)
        assert_eq!(escape_yaml_double_quoted("a\nb"), "a\\\\nb");
        // Input is backslash, quote, newline.
        // Step 1 (backslash → \\): `\"\n` becomes `\\"\n` (one extra backslash).
        // Step 2 (quote → \"):     `\\"\n` becomes `\\\"\n`.
        // Step 3 (\n → \\n):       `\\\"\n` becomes `\\\"\\n`.
        assert_eq!(escape_yaml_double_quoted("\\\"\n"), "\\\\\\\"\\\\n");
    }

    /// What keeping the pair in one crate is for: every description the
    /// writer can produce reads back as itself.
    #[test]
    fn every_escaped_description_decodes_back_to_itself() {
        for original in [
            "plain",
            "a\\b",
            "say \"hi\"",
            "line one\nline two",
            "C:\\tmp\\agents",
            "mixed \\ \" \n end",
            "",
        ] {
            assert_eq!(
                decode_yaml_double_quoted(&escape_yaml_double_quoted(original)),
                original,
                "round trip for {original:?}"
            );
        }
    }

    fn write_md(dir: &Path, name: &str, content: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), content).unwrap();
    }

    // ── Built-ins-only registry ──────────────────────────────────

    #[test]
    fn builtins_only_resolves_seven_known_agents() {
        let r = AgentRegistry::builtins_only();
        assert_eq!(r.len(), 7);
        for t in [
            "Explore",
            "Plan",
            "general-purpose",
            "batch-worker",
            "verification",
            "statusline-setup",
            "rebon-code-guide",
        ] {
            assert!(r.resolve(t).is_some(), "missing built-in: {t}");
            assert_eq!(r.resolve(t).unwrap().source, AgentSource::BuiltIn);
        }
    }

    #[test]
    fn builtins_only_resolve_unknown_returns_none() {
        let r = AgentRegistry::builtins_only();
        assert!(r.resolve("does-not-exist").is_none());
    }

    #[test]
    fn worktree_agent_lines_can_be_omitted_for_default_coordinator_prompt() {
        let r = AgentRegistry::builtins_only();
        let lines = r.format_lines_without_worktree_agents();
        assert!(!lines.contains("batch-worker"));
        assert!(!lines.contains("worktree"));
        assert!(lines.contains("Explore"));
        assert!(lines.contains("general-purpose"));
    }

    #[test]
    fn explore_aliases_resolve_to_builtin_explore() {
        let r = AgentRegistry::builtins_only();
        for alias in ["explore", "Explorer", "explorer"] {
            let resolved = r.resolve(alias).expect("alias should resolve");
            assert_eq!(resolved.agent_type, "Explore");
            assert_eq!(resolved.source, AgentSource::BuiltIn);
        }
    }

    #[test]
    fn exact_custom_explore_wins_over_alias_fallback() {
        let groups = AgentGroups {
            built_in: all_builtin_agents()
                .into_iter()
                .map(ResolvedAgentDef::from_builtin)
                .collect(),
            user: vec![def(
                "explore",
                "CUSTOM-EXPLORE",
                AgentSource::Settings(SettingSource::UserSettings),
            )],
            ..Default::default()
        };
        let r = AgentRegistry::from_groups(groups);
        let resolved = r.resolve("explore").unwrap();
        assert_eq!(resolved.agent_type, "explore");
        assert_eq!(resolved.system_prompt, "CUSTOM-EXPLORE");
        assert_eq!(
            resolved.source,
            AgentSource::Settings(SettingSource::UserSettings)
        );
    }

    #[test]
    fn uppercase_explore_is_not_broad_case_folded() {
        let r = AgentRegistry::builtins_only();
        assert!(r.resolve("EXPLORE").is_none());
    }

    #[test]
    fn builtins_only_format_lines_lists_the_offered_built_ins() {
        let r = AgentRegistry::builtins_only();
        let lines = r.format_lines();
        assert_eq!(lines.lines().count(), 6);
        assert!(lines.contains("Explore:"));
        assert!(lines.contains("general-purpose:"));
    }

    /// The Plan agent resolves but is not offered.
    ///
    /// Plan mode forbids it by name, so the only sessions that could run it
    /// are the ones not planning — where a planning agent in the roster is
    /// the pull toward planning. Naming it still works, which is what keeps
    /// this a hidden agent rather than a deleted one.
    #[test]
    fn the_plan_agent_resolves_but_is_not_offered_to_the_model() {
        let r = AgentRegistry::builtins_only();

        let plan = r.resolve("Plan").expect("Plan still resolves");
        assert_eq!(plan.source, AgentSource::BuiltIn);
        assert!(!plan.is_offered_to_model());

        for lines in [r.format_lines(), r.format_lines_without_worktree_agents()] {
            assert!(!lines.contains("Plan:"), "Plan is offered:\n{lines}");
        }
        assert!(!crate::builtin_agents::format_all_agent_lines().contains("Plan:"));

        // Every other built-in is offered, so this is one agent's carve-out
        // and not a filter that quietly swallowed the roster.
        for agent_type in [
            "Explore",
            "general-purpose",
            "batch-worker",
            "verification",
            "statusline-setup",
            "rebon-code-guide",
        ] {
            assert!(
                r.resolve(agent_type)
                    .expect(agent_type)
                    .is_offered_to_model(),
                "{agent_type} is no longer offered"
            );
        }
    }

    /// A user or project agent file named `Plan` overrides the built-in, and
    /// an override is offered like any other configured agent. That file is
    /// how someone puts the Plan agent back in the roster.
    #[test]
    fn a_configured_plan_agent_is_offered_again() {
        let mut configured =
            ResolvedAgentDef::from_builtin(crate::builtin_agents::plan_agent_def());
        configured.source = AgentSource::Settings(SettingSource::UserSettings);
        assert!(configured.is_offered_to_model());

        let r = AgentRegistry::from_groups(AgentGroups {
            built_in: all_builtin_agents()
                .into_iter()
                .map(ResolvedAgentDef::from_builtin)
                .collect(),
            user: vec![configured],
            ..Default::default()
        });
        assert!(r.format_lines().contains("Plan:"));
    }

    // ── Markdown parsing ─────────────────────────────────────────

    #[test]
    fn parse_minimal_agent_md_succeeds() {
        let body = "---\n\
                    name: my-agent\n\
                    description: Use this for X\n\
                    ---\n\
                    You are an agent that does X.\n";
        let def = parse_agent_md(
            Path::new("my-agent.md"),
            body,
            AgentSource::Settings(SettingSource::UserSettings),
        )
        .unwrap()
        .unwrap();
        assert_eq!(def.agent_type, "my-agent");
        assert_eq!(def.when_to_use, "Use this for X");
        assert!(def.system_prompt.starts_with("You are an agent"));
        assert!(def.tool_filter.is_unrestricted());
        assert!(def.model.is_none());
        assert!(def.model_profile.is_none());
        assert!(!def.background);
        assert_eq!(
            def.source,
            AgentSource::Settings(SettingSource::UserSettings)
        );
    }

    #[test]
    fn parse_agent_md_without_frontmatter_returns_none() {
        let result = parse_agent_md(
            Path::new("not-agent.md"),
            "Just a regular markdown file.\n",
            AgentSource::Settings(SettingSource::UserSettings),
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_agent_md_with_frontmatter_but_no_name_returns_none() {
        let body = "---\n\
                    description: a co-located doc\n\
                    ---\n\
                    body text\n";
        let result = parse_agent_md(
            Path::new("doc.md"),
            body,
            AgentSource::Settings(SettingSource::UserSettings),
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_agent_md_with_name_but_no_description_errors() {
        let body = "---\n\
                    name: half-baked\n\
                    ---\n\
                    body\n";
        let result = parse_agent_md(
            Path::new("half-baked.md"),
            body,
            AgentSource::Settings(SettingSource::UserSettings),
        );
        assert!(result.is_err());
    }

    #[test]
    fn parse_agent_md_with_empty_body_errors() {
        let body = "---\n\
                    name: empty\n\
                    description: x\n\
                    ---\n\
                    \n";
        let result = parse_agent_md(
            Path::new("empty.md"),
            body,
            AgentSource::Settings(SettingSource::UserSettings),
        );
        assert!(result.is_err());
    }

    #[test]
    fn parse_agent_md_honours_tools_allow_list() {
        let body = "---\n\
                    name: ranger\n\
                    description: limited\n\
                    tools: Read, Grep\n\
                    ---\n\
                    body\n";
        let def = parse_agent_md(
            Path::new("ranger.md"),
            body,
            AgentSource::Settings(SettingSource::UserSettings),
        )
        .unwrap()
        .unwrap();
        assert!(!def.tool_filter.is_unrestricted());
        assert!(def.tool_filter.allows("Read", &["FileReadTool"]));
        assert!(def.tool_filter.allows("Grep", &["GrepTool"]));
        assert!(!def.tool_filter.allows("Bash", &["BashTool"]));
    }

    #[test]
    fn parse_agent_md_tools_star_is_unrestricted() {
        let body = "---\n\
                    name: any\n\
                    description: anything\n\
                    tools: \"*\"\n\
                    ---\n\
                    body\n";
        let def = parse_agent_md(
            Path::new("any.md"),
            body,
            AgentSource::Settings(SettingSource::UserSettings),
        )
        .unwrap()
        .unwrap();
        assert!(def.tool_filter.is_unrestricted());
    }

    #[test]
    fn parse_agent_md_disallowed_tools_overlay_works() {
        let body = "---\n\
                    name: mostly-open\n\
                    description: open with deny\n\
                    disallowedTools: Bash, Edit\n\
                    ---\n\
                    body\n";
        let def = parse_agent_md(
            Path::new("mo.md"),
            body,
            AgentSource::Settings(SettingSource::UserSettings),
        )
        .unwrap()
        .unwrap();
        assert!(def.tool_filter.allows("Read", &["FileReadTool"]));
        assert!(!def.tool_filter.allows("Bash", &["BashTool"]));
        assert!(!def.tool_filter.allows("Edit", &["FileEditTool"]));
    }

    #[test]
    fn parse_agent_md_picks_up_permission_mode_camelcase() {
        let body = "---\n\
                    name: gated\n\
                    description: x\n\
                    permissionMode: acceptEdits\n\
                    ---\n\
                    body\n";
        let def = parse_agent_md(
            Path::new("gated.md"),
            body,
            AgentSource::Settings(SettingSource::UserSettings),
        )
        .unwrap()
        .unwrap();
        assert_eq!(def.permission_mode.as_deref(), Some("acceptEdits"));
    }

    #[test]
    fn parse_agent_md_normalizes_permission_mode_aliases() {
        for (raw, expected) in [
            ("default", "default"),
            ("plan", "plan"),
            ("accept_edits", "acceptEdits"),
            ("accept-edits", "acceptEdits"),
            ("bypass-permissions", "bypassPermissions"),
            ("auto", "auto"),
        ] {
            let body = format!("---\nname: g\ndescription: x\npermission_mode: {raw}\n---\nbody\n");
            let def = parse_agent_md(
                Path::new("g.md"),
                &body,
                AgentSource::Settings(SettingSource::UserSettings),
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                def.permission_mode.as_deref(),
                Some(expected),
                "input `{raw}` should map to `{expected}`",
            );
        }
    }

    #[test]
    fn parse_agent_md_drops_unknown_permission_mode() {
        let body = "---\nname: g\ndescription: x\npermissionMode: yolo\n---\nbody\n";
        let def = parse_agent_md(
            Path::new("g.md"),
            body,
            AgentSource::Settings(SettingSource::UserSettings),
        )
        .unwrap()
        .unwrap();
        assert!(def.permission_mode.is_none());
    }

    #[test]
    fn parse_agent_md_picks_up_model_and_background_and_isolation() {
        let body = "---\n\
                    name: flagged\n\
                    description: x\n\
                    model: sonnet\n\
                    modelProfile: reasoning\n\
                    provider: anthropic\n\
                    effort: high\n\
                    background: true\n\
                    isolation: worktree\n\
                    ---\n\
                    body\n";
        let def = parse_agent_md(
            Path::new("flagged.md"),
            body,
            AgentSource::Settings(SettingSource::UserSettings),
        )
        .unwrap()
        .unwrap();
        assert_eq!(def.model.as_deref(), Some("sonnet"));
        assert_eq!(def.model_profile.as_deref(), Some("reasoning"));
        assert_eq!(def.provider.as_deref(), Some("anthropic"));
        assert_eq!(def.effort.as_deref(), Some("high"));
        assert!(def.background);
        assert_eq!(def.isolation.as_deref(), Some("worktree"));
        assert_eq!(def.runtime, AgentRuntime::Local);
    }

    // ── runtime: ─────────────────────────────────────────────────

    fn parse(body: &str) -> Result<Option<ResolvedAgentDef>, String> {
        parse_agent_md(
            Path::new("agent.md"),
            body,
            AgentSource::Settings(SettingSource::UserSettings),
        )
    }

    #[test]
    fn an_agent_without_a_runtime_key_runs_locally() {
        let def = parse("---\nname: a\ndescription: x\n---\nbody\n")
            .unwrap()
            .unwrap();
        assert_eq!(def.runtime, AgentRuntime::Local);
        assert!(!def.runtime.is_external());
        assert!(def.runtime.command_line().is_none());
    }

    #[test]
    fn an_acp_agent_carries_its_command_line() {
        let def = parse(
            "---\n\
             name: gemini\n\
             description: gemini over acp\n\
             runtime: acp\n\
             command: gemini\n\
             args: --experimental-acp --yolo\n\
             ---\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            def.runtime,
            AgentRuntime::Acp {
                command: "gemini".into(),
                args: vec!["--experimental-acp".into(), "--yolo".into()],
            }
        );
        assert!(def.runtime.is_external());
        assert_eq!(
            def.runtime.command_line().as_deref(),
            Some("gemini --experimental-acp --yolo")
        );
    }

    #[test]
    fn an_external_agent_needs_no_system_prompt_body() {
        // The prompt belongs to the agent being driven; demanding one
        // here would force users to write text nothing ever sends.
        let def = parse("---\nname: g\ndescription: x\nruntime: acp\ncommand: g\n---\n")
            .unwrap()
            .unwrap();
        assert!(def.system_prompt.is_empty());
        // A local agent still has to have one.
        let err = parse("---\nname: l\ndescription: x\n---\n").unwrap_err();
        assert!(err.contains("empty system prompt"));
    }

    #[test]
    fn acp_without_a_command_is_an_error_not_a_silent_local_agent() {
        let err = parse("---\nname: g\ndescription: x\nruntime: acp\n---\nbody\n").unwrap_err();
        assert!(err.contains("`command:`"), "unhelpful error: {err}");
    }

    #[test]
    fn an_unknown_runtime_is_rejected_rather_than_guessed() {
        let err =
            parse("---\nname: g\ndescription: x\nruntime: telepathy\n---\nbody\n").unwrap_err();
        assert!(err.contains("telepathy"), "error should quote it: {err}");
        assert!(
            err.contains("local"),
            "error should list the options: {err}"
        );
    }

    #[test]
    fn a_command_without_a_runtime_key_stays_local() {
        // `command:` alone is a key this registry does not use, not an
        // implicit opt-in to running an external process.
        let def = parse("---\nname: a\ndescription: x\ncommand: rm\n---\nbody\n")
            .unwrap()
            .unwrap();
        assert_eq!(def.runtime, AgentRuntime::Local);
    }

    #[test]
    fn args_accept_both_the_yaml_list_and_the_plain_forms() {
        assert_eq!(parse_agent_args(None), Vec::<String>::new());
        assert_eq!(parse_agent_args(Some("  ")), Vec::<String>::new());
        assert_eq!(
            parse_agent_args(Some("--acp --quiet")),
            vec!["--acp".to_string(), "--quiet".to_string()]
        );
        assert_eq!(
            parse_agent_args(Some("[--acp, \"--quiet\"]")),
            vec!["--acp".to_string(), "--quiet".to_string()]
        );
        assert_eq!(parse_agent_args(Some("[]")), Vec::<String>::new());
    }

    #[test]
    fn runtime_spellings_are_accepted_case_insensitively() {
        for spelling in ["local", "Local", "engine", "rebon"] {
            let body = format!("---\nname: a\ndescription: x\nruntime: {spelling}\n---\nbody\n");
            assert_eq!(parse(&body).unwrap().unwrap().runtime, AgentRuntime::Local);
        }
        for spelling in ["acp", "ACP", "external"] {
            let body =
                format!("---\nname: a\ndescription: x\nruntime: {spelling}\ncommand: c\n---\n");
            assert!(parse(&body).unwrap().unwrap().runtime.is_external());
        }
    }

    #[test]
    fn an_external_agent_is_listed_but_never_offered_as_a_subagent_type() {
        let acp = parse(
            "---\nname: gemini\ndescription: gemini over acp\nruntime: acp\ncommand: gemini\n---\n",
        )
        .unwrap()
        .unwrap();
        let local = parse("---\nname: helper\ndescription: a helper\n---\nbody\n")
            .unwrap()
            .unwrap();
        let registry = AgentRegistry::from_groups(AgentGroups {
            user: vec![acp, local],
            ..AgentGroups::default()
        });

        // The user configured it, so it belongs in the agent list…
        let listed: Vec<&str> = registry
            .active()
            .map(|def| def.agent_type.as_str())
            .collect();
        assert!(listed.contains(&"gemini"));
        assert!(listed.contains(&"helper"));

        // …but by default (no external runner wired) offering it as a
        // delegation target would advertise something that silently
        // runs a different agent.
        let spawnable: Vec<&str> = registry
            .spawnable()
            .map(|def| def.agent_type.as_str())
            .collect();
        assert!(!spawnable.contains(&"gemini"));
        assert!(spawnable.contains(&"helper"));
        assert!(!registry.format_lines().contains("gemini"));
        assert!(registry.format_lines().contains("helper"));

        // It is still resolvable — the dispatch guard is what refuses
        // it, with a reason the caller can read.
        assert!(registry.resolve("gemini").is_some());
    }

    #[test]
    fn a_host_with_an_external_runner_offers_external_agents_as_subagent_types() {
        let acp = parse(
            "---\nname: gemini\ndescription: gemini over acp\nruntime: acp\ncommand: gemini\n---\n",
        )
        .unwrap()
        .unwrap();
        let registry = AgentRegistry::from_groups(AgentGroups {
            user: vec![acp],
            ..AgentGroups::default()
        })
        .with_external_agents_spawnable();

        let spawnable: Vec<&str> = registry
            .spawnable()
            .map(|def| def.agent_type.as_str())
            .collect();
        assert!(spawnable.contains(&"gemini"));
        assert!(registry.format_lines().contains("gemini"));
    }

    // ── Override priority ────────────────────────────────────────

    fn def(name: &str, prompt: &str, source: AgentSource) -> ResolvedAgentDef {
        ResolvedAgentDef {
            agent_type: name.to_string(),
            when_to_use: format!("when_to_use:{name}"),
            system_prompt: prompt.to_string(),
            tool_filter: ToolFilter::unrestricted(),
            model: None,
            model_profile: None,
            provider: None,
            effort: None,
            background: false,
            isolation: None,
            memory: None,
            permission_mode: None,
            runtime: AgentRuntime::Local,
            source,
            file_stem: None,
        }
    }

    #[test]
    fn user_overrides_builtin_when_agent_type_collides() {
        let groups = AgentGroups {
            built_in: vec![def("rebon-code-guide", "BUILTIN", AgentSource::BuiltIn)],
            user: vec![def(
                "rebon-code-guide",
                "USER",
                AgentSource::Settings(SettingSource::UserSettings),
            )],
            ..Default::default()
        };
        let r = AgentRegistry::from_groups(groups);
        let resolved = r.resolve("rebon-code-guide").unwrap();
        assert_eq!(resolved.system_prompt, "USER");
        assert_eq!(
            resolved.source,
            AgentSource::Settings(SettingSource::UserSettings)
        );
    }

    #[test]
    fn project_overrides_user_when_agent_type_collides() {
        let groups = AgentGroups {
            user: vec![def(
                "shared",
                "USER",
                AgentSource::Settings(SettingSource::UserSettings),
            )],
            project: vec![def(
                "shared",
                "PROJECT",
                AgentSource::Settings(SettingSource::ProjectSettings),
            )],
            ..Default::default()
        };
        let r = AgentRegistry::from_groups(groups);
        assert_eq!(r.resolve("shared").unwrap().system_prompt, "PROJECT");
    }

    #[test]
    fn managed_overrides_flag_overrides_project() {
        let groups = AgentGroups {
            project: vec![def(
                "shared",
                "PROJECT",
                AgentSource::Settings(SettingSource::ProjectSettings),
            )],
            flag: vec![def(
                "shared",
                "FLAG",
                AgentSource::Settings(SettingSource::FlagSettings),
            )],
            managed: vec![def(
                "shared",
                "MANAGED",
                AgentSource::Settings(SettingSource::PolicySettings),
            )],
            ..Default::default()
        };
        let r = AgentRegistry::from_groups(groups);
        assert_eq!(r.resolve("shared").unwrap().system_prompt, "MANAGED");
    }

    #[test]
    fn listing_keeps_builtin_slot_position_after_override() {
        // When a user override shadows a built-in, the built-in's
        // listing slot should stay put — only the resolved definition
        // changes, the visual order doesn't shuffle.
        let groups = AgentGroups {
            built_in: vec![
                def("a", "A", AgentSource::BuiltIn),
                def("b", "B", AgentSource::BuiltIn),
                def("c", "C", AgentSource::BuiltIn),
            ],
            user: vec![def(
                "b",
                "B-OVERRIDE",
                AgentSource::Settings(SettingSource::UserSettings),
            )],
            ..Default::default()
        };
        let r = AgentRegistry::from_groups(groups);
        let names: Vec<&str> = r.active().map(|d| d.agent_type.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        assert_eq!(r.resolve("b").unwrap().system_prompt, "B-OVERRIDE");
    }

    #[test]
    fn active_snapshot_returns_owned_listing_order() {
        let groups = AgentGroups {
            built_in: vec![def("a", "A", AgentSource::BuiltIn)],
            user: vec![def(
                "custom",
                "CUSTOM",
                AgentSource::Settings(SettingSource::UserSettings),
            )],
            ..Default::default()
        };
        let r = AgentRegistry::from_groups(groups);

        let snapshot = r.active_snapshot();

        assert_eq!(
            snapshot
                .iter()
                .map(|def| def.agent_type.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "custom"]
        );
        assert_eq!(snapshot[1].system_prompt, "CUSTOM");
    }

    #[test]
    fn listing_appends_custom_only_agent_after_builtins() {
        let groups = AgentGroups {
            built_in: all_builtin_agents()
                .into_iter()
                .map(ResolvedAgentDef::from_builtin)
                .collect(),
            user: vec![def(
                "custom-only",
                "CUSTOM",
                AgentSource::Settings(SettingSource::UserSettings),
            )],
            ..Default::default()
        };
        let r = AgentRegistry::from_groups(groups);
        let names: Vec<String> = r.active().map(|d| d.agent_type.clone()).collect();
        assert_eq!(names.len(), 8);
        assert_eq!(names.last().unwrap(), "custom-only");
    }

    // ── Disk loader ──────────────────────────────────────────────

    #[test]
    fn load_picks_up_user_dir_override_for_builtin() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("cwd");
        let home = tmp.path().join("home");
        fs::create_dir_all(&cwd).unwrap();

        write_md(
            &home.join("agents"),
            "rebon-code-guide.md",
            "---\n\
             name: rebon-code-guide\n\
             description: DISABLED. Do not invoke.\n\
             tools: Read\n\
             ---\n\
             This agent is intentionally disabled.\n",
        );

        let r = AgentRegistry::load(&cwd, &home);
        let resolved = r.resolve("rebon-code-guide").unwrap();
        assert!(resolved.system_prompt.contains("intentionally disabled"));
        assert_eq!(
            resolved.source,
            AgentSource::Settings(SettingSource::UserSettings)
        );
    }

    #[test]
    fn disabled_override_is_not_active_or_formatted() {
        let mut disabled = def(
            "rebon-code-guide",
            "USER",
            AgentSource::Settings(SettingSource::UserSettings),
        );
        disabled.when_to_use = "DISABLED. Do not invoke.".into();
        let groups = AgentGroups {
            built_in: vec![def("rebon-code-guide", "BUILTIN", AgentSource::BuiltIn)],
            user: vec![disabled],
            ..Default::default()
        };
        let r = AgentRegistry::from_groups(groups);

        assert!(r.resolve("rebon-code-guide").unwrap().is_disabled());
        assert!(r.active().next().is_none());
        assert!(r.format_lines().is_empty());
    }

    #[test]
    fn load_with_no_dirs_falls_back_to_builtins() {
        let tmp = TempDir::new().unwrap();
        let r = AgentRegistry::load(&tmp.path().join("cwd"), &tmp.path().join("home"));
        assert_eq!(r.len(), 7);
    }

    #[test]
    fn load_skips_non_md_files() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join("agents")).unwrap();
        fs::write(home.join("agents").join("README.txt"), "not an agent").unwrap();
        let r = AgentRegistry::load(&tmp.path().join("cwd"), &home);
        // Should still hold exactly the 7 built-ins.
        assert_eq!(r.len(), 7);
    }

    #[test]
    fn load_project_overrides_user_via_disk() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("cwd");
        let home = tmp.path().join("home");

        write_md(
            &home.join("agents"),
            "shared.md",
            "---\nname: shared\ndescription: u\n---\nUSER\n",
        );
        write_md(
            &cwd.join(".rebon").join("agents"),
            "shared.md",
            "---\nname: shared\ndescription: p\n---\nPROJECT\n",
        );

        let r = AgentRegistry::load(&cwd, &home);
        assert_eq!(r.resolve("shared").unwrap().system_prompt, "PROJECT");
    }

    #[test]
    fn load_format_lines_includes_user_only_agent() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        write_md(
            &home.join("agents"),
            "code-reviewer.md",
            "---\n\
             name: code-reviewer\n\
             description: independent code review\n\
             ---\n\
             You are a code reviewer.\n",
        );
        let r = AgentRegistry::load(&tmp.path().join("cwd"), &home);
        let lines = r.format_lines();
        assert!(lines.contains("code-reviewer:"));
        assert!(lines.contains("independent code review"));
    }
}
