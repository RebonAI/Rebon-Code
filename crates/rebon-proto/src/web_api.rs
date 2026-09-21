//! The shapes the local server returns from its `/api/*` reads.
//!
//! Not ACP — these are Rebon's own HTTP reads, which the browser client makes
//! beside the protocol it speaks over the socket. They live here because the
//! page reads both, and the page's TypeScript is generated from this crate: a
//! hand-written mirror drifts silently.
//!
//! Every field name, every `camelCase` rename and every "absent when empty"
//! rule is declared here once rather than spelled in a macro on the server and
//! again in TypeScript on the client.
//!
//! # The wire did not change
//!
//! Each type serialises byte for byte what the `serde_json::json!` shape before
//! it produced, and a golden per response pins that. Where a field was always
//! present it is a plain field; where the macro emitted `null` the field is
//! `Option` **without** `skip_serializing_if`, because a key that used to be
//! there and is now missing is a change to the wire even when the value was
//! null either way.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::SlashCommand;

/// `GET /api/info` — what this server is and what it is running.
// No `PartialEq`: `InitializeResult` has none, and the golden below compares
// serialised values, which is the comparison that matters here anyway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ServerInfo {
    /// The `rebon` build serving this page.
    pub version: String,
    /// The directory the server was started in.
    pub cwd: String,
    /// The provider the runtime model resolves to.
    pub provider: String,
    /// The model id the runtime resolves to.
    pub model: String,
    /// The agents the page may switch between.
    pub agents: Vec<AgentOption>,
    /// The config option id that selects an agent.
    #[serde(rename = "agentConfigOption")]
    pub agent_config_option: String,
    /// Sessions with a `session/prompt` in flight on the server right now.
    #[serde(rename = "activeTurns")]
    pub active_turns: Vec<String>,
    /// Where the page's assets came from, for the footer.
    #[serde(rename = "webUi")]
    pub web_ui: WebUiSource,
    /// Whether this server accepts steering mid-turn.
    pub steering: bool,
    /// The handshake the mux completed, or `null` before it has.
    ///
    /// The mux keeps it as the JSON it received, so that is what is passed
    /// on. An [`InitializeResult`] in every case that matters, and the page
    /// reads it as one; typing it here would mean a decode and a re-encode
    /// on a value nothing in this process looks at, and a field the agent
    /// added would be dropped on the way through.
    pub initialize: Option<Value>,
}

/// Where the page's own files came from.
///
/// The two cases are not the same shape and never have been: an embedded
/// build reports the commit it was built from and a null path, and a
/// directory reports its path and no `built` key at all. Kept asymmetric on
/// purpose — adding `"built": null` to the directory case would be a change
/// to the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WebUiSource {
    /// `embedded` or `directory`.
    pub source: String,
    /// The directory being served, or `null` when embedded.
    pub path: Option<String>,
    /// What the embedded build was built from. Absent for a directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built: Option<String>,
}

/// One entry of [`ServerInfo::agents`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentOption {
    /// The value to set the agent config option to.
    pub id: String,
    /// What to show in the picker.
    pub label: String,
}

/// `GET /api/commands` — the `/` menu.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CommandsResponse {
    /// The catalog's `WEB` surface, then every enabled user-invocable skill.
    pub commands: Vec<Value>,
}

impl CommandsResponse {
    /// One skill, shaped like the [`SlashCommand`] entries beside it.
    ///
    /// A `Value` rather than the typed command because the two are not the
    /// same thing on the wire: a catalog command carries the catalog's fields,
    /// and a skill carries the four the page needs to list it.
    pub fn skill_entry(id: &str, description: &str, argument_hint: Option<&str>) -> Value {
        serde_json::json!({
            "name": id,
            "description": description,
            "input": argument_hint.map(|hint| serde_json::json!({ "hint": hint })),
            "category": "skill",
            "aliases": [],
        })
    }

    /// The catalog half, serialised as the page has always seen it.
    pub fn catalog_entry(command: &SlashCommand) -> Value {
        serde_json::to_value(command).unwrap_or(Value::Null)
    }
}

/// `GET /api/skills`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SkillsResponse {
    /// Every skill the registry knows, enabled or not.
    pub skills: Vec<SkillEntry>,
}

/// One entry of [`SkillsResponse::skills`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SkillEntry {
    /// The skill's id, which is also how it is invoked.
    pub name: String,
    /// The human title.
    pub title: String,
    /// One line about what it does.
    pub description: String,
    /// Where it came from: `builtin`, `user`, `project`, `managed`, `local`,
    /// `flag`, `plugin`, `mcp`.
    pub source: String,
    /// The directory it was loaded from, or `null` for one that has none.
    pub path: Option<String>,
    /// Whether it is switched on.
    pub enabled: bool,
    /// Whether it appears in the `/` menu.
    #[serde(rename = "userInvocable")]
    pub user_invocable: bool,
    /// What its argument looks like, or `null`.
    pub hint: Option<String>,
}

/// `GET /api/agents`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentsResponse {
    /// Every agent definition this server can run.
    pub agents: Vec<AgentEntry>,
}

/// One entry of [`AgentsResponse::agents`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentEntry {
    /// The agent type, which is what a spawn names.
    pub name: String,
    /// When to use it, from the definition's own words.
    pub description: String,
    /// The model it pins, or `null` to follow the session's.
    pub model: Option<String>,
    /// The provider it pins, or `null`.
    pub provider: Option<String>,
    /// The reasoning effort it pins, or `null`.
    pub effort: Option<String>,
    /// The tools it is allowed, or `null` for the default set.
    pub tools: Option<Vec<String>>,
    /// Which runtime runs it.
    pub runtime: String,
    /// Whether it runs in the background by default.
    pub background: bool,
    /// Where the definition came from: `builtin`, `plugin:<name>`, `user`,
    /// `project`, `local`, `flag`, `policy`.
    pub source: String,
}

/// `GET /api/tasks` — the background tasks of one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TasksResponse {
    /// Newest first.
    pub tasks: Vec<TaskEntry>,
}

/// One entry of [`TasksResponse::tasks`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TaskEntry {
    /// The task id, which is what an action names.
    pub id: String,
    /// What kind of task it is.
    pub kind: String,
    /// Where it has got to.
    pub status: String,
    /// The one-line title.
    pub title: String,
    /// Its latest progress line, or `null`.
    pub description: Option<String>,
    /// Why it failed, or `null`.
    pub error: Option<String>,
    /// Its result as text, or `null` while it runs.
    pub output: Option<String>,
    /// Whether it was sent to the background.
    pub backgrounded: bool,
    /// ISO-8601, or `null` on a task with no start time.
    #[serde(rename = "createdAt")]
    pub created_at: Option<String>,
    /// ISO-8601, or `null` while it runs.
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<String>,
}

/// `GET /api/models` — what this session can be switched to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelsResponse {
    /// The active custom provider, or the runtime's own provider name.
    pub provider: Option<String>,
    /// The model id in use.
    pub current: String,
    /// Every model the provider offers, the current one first if it is not
    /// among them.
    pub models: Vec<ModelOption>,
}

/// One entry of [`ModelsResponse::models`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelOption {
    /// The model id.
    pub id: String,
}

/// What the four `/api/*` levers answer: task cancel and dismiss, MCP
/// reconnect and disconnect, a rewind, a compaction.
///
/// `ok` is whether the thing was done, not whether the request was
/// understood — a request that was not understood is a 400 with a reason and
/// never reaches this shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ActionResult {
    /// Whether it happened.
    pub ok: bool,
    /// What to show the person who asked.
    pub message: String,
    /// A rewind hands back the prompt of the turn it rewound to, so the
    /// composer can be refilled with it. Absent on every other lever, and on
    /// a rewind the owner performed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill_prompt: Option<String>,
    /// How many turns the rewound conversation has left. Absent for the
    /// same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<usize>,
}

impl ActionResult {
    /// The two-field shape, which is what every lever but a local rewind
    /// answers with.
    pub fn new(ok: bool, message: impl Into<String>) -> Self {
        Self {
            ok,
            message: message.into(),
            prefill_prompt: None,
            turns: None,
        }
    }
}

/// `GET /api/files` — the file picker's candidates for one workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FilesResponse {
    /// The directory the paths are relative to.
    pub root: String,
    /// The candidates, a directory marked by a trailing slash.
    pub files: Vec<String>,
    /// Whether more matched than were returned.
    pub truncated: bool,
    /// Whether the workspace index is still being built, so an empty list
    /// means "not yet" rather than "nothing".
    pub scanning: bool,
    /// Why the directory could not be listed. Absent when it could.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// `GET /api/usage` — the token ledger for one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct UsageResponse {
    /// The session this counts.
    pub session_id: String,
    /// Tokens sent, including the turn in flight.
    pub input_tokens: u64,
    /// Tokens received, including the turn in flight.
    pub output_tokens: u64,
    /// How many turns the ledger has seen.
    pub turns: u64,
    /// Messages on disk, or `null` when the transcript could not be read.
    pub messages: Option<usize>,
    /// How long the session has been open, or `null` for one this server
    /// did not open.
    pub duration_ms: Option<u64>,
    /// The model the runtime resolves to.
    pub model: String,
    /// Cache reads, which only the session's own host counts. `null`
    /// without one.
    pub cache_read_tokens: Option<u64>,
    /// Cache writes, on the same terms.
    pub cache_creation_tokens: Option<u64>,
    /// Which of the two counts this is, in words, because the difference
    /// matters to whoever reads the number.
    pub problem: String,
}

/// `GET /api/rewind` — where a session can be rewound to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RewindResponse {
    /// The user turns, newest last.
    pub checkpoints: Vec<RewindCheckpoint>,
    /// Whether this server can rewind a conversation at all.
    pub supported: bool,
    /// Whether it can restore files too, which needs the session's host —
    /// the file-history snapshots are the host's.
    pub files_supported: bool,
    /// Why files cannot be restored, or `null` when they can.
    pub problem: Option<String>,
}

/// One entry of [`RewindResponse::checkpoints`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RewindCheckpoint {
    /// The turn number, counting from the start of the session.
    pub index: usize,
    /// The user message this turn began with.
    pub uuid: String,
    /// When it was sent, or `null` on a turn whose line carried no stamp.
    pub timestamp: Option<String>,
    /// The first 240 characters of the prompt, with an ellipsis when it was
    /// cut.
    pub preview: String,
    /// Whether the turn finished. An unfinished one is still a checkpoint.
    pub complete: bool,
}

/// `GET /api/history` — a session as the page reopens it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct HistorySnapshot {
    /// The session read.
    pub session_id: String,
    /// The directory it belongs to.
    pub cwd: String,
    /// Its title, or `null` for one that has none.
    pub title: Option<String>,
    /// The display rows the terminal draws, from the one producer.
    pub rows: Value,
    /// **Deprecated.** The model's view of the
    /// session, folded a second and different way: it carries tools meant
    /// for their own surface, and it deletes a whole user block where the
    /// row stream keeps the half a person typed. Nothing first-party reads
    /// it; `rows` is what the terminal, the page and the desktop app show.
    pub messages: Value,
}

/// `GET /api/plugins` — the plugin plane as this process sees it.
///
/// The page's answer to "is my plugin running": what runtime it would run on,
/// what the composition asked for, what loaded, and why the rest did not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PluginStatus {
    /// Which loop host this build carries, in one line.
    pub host: String,
    /// The Node runtime a composition would run on.
    pub runtime: PluginRuntimeStatus,
    /// The plane that is actually up, if one is.
    pub plane: PluginPlaneStatus,
    /// One per leaf the composition resolved, in load order.
    pub entries: Vec<PluginEntry>,
    /// Entries that could not be turned into a load request, and why.
    ///
    /// A plugin that starts and is then refused at every registration is a
    /// worse failure than one that never starts, so each line carries its
    /// reason rather than only a name.
    pub skipped: Vec<String>,
    /// The kernel loop vendors a session can be switched to.
    pub loops: Vec<KernelLoopOption>,
}

/// The Node runtime under a plugin plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PluginRuntimeStatus {
    /// How many entries `kernelPlugins` asks for. Zero is a normal state,
    /// not a problem — the section is opt-in.
    ///
    /// A count, not a boolean: nothing on the page ever read it as a flag.
    #[serde(rename = "configuredPlugins")]
    pub configured_plugins: usize,
    /// The versions a runtime may report, as a range someone can read.
    pub supported: String,
    /// The runtime that resolved, or `null` if none did.
    pub resolved: Option<ResolvedRuntime>,
    /// The resolver's own words for why nothing resolved.
    pub problem: Option<String>,
    /// A Node that was found and turned down for its version.
    ///
    /// Separate from `problem` because it is a different situation with a
    /// different fix.
    #[serde(rename = "outOfRange")]
    pub out_of_range: Option<OutOfRangeRuntime>,
    /// Whether `rebon node install` could fix this here.
    pub installable: bool,
}

/// One resolved Node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ResolvedRuntime {
    /// The version it reported.
    pub version: String,
    /// Where it lives, as a string: the server has a path and the wire has
    /// no path type.
    pub executable: String,
    /// Which rung of the ladder answered: an explicit demand, a managed
    /// install, or `PATH`.
    pub origin: String,
}

/// A Node that answered, but not with a version this build accepts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OutOfRangeRuntime {
    /// The version it reported.
    pub version: String,
    /// Where it lives.
    pub executable: String,
}

/// The running plane, or the reasons there is none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PluginPlaneStatus {
    /// Whether a plane is up in this process.
    pub running: bool,
    /// Which generation it is, or `null` when nothing is running.
    pub generation: Option<u64>,
    /// Why the composition was refused, in the refusal's own words.
    pub refusal: Option<String>,
    /// The ids that loaded.
    pub loaded: Vec<String>,
    /// Why the composition could not be read at all.
    pub problem: Option<String>,
}

/// One entry of the composition, and whether it loaded.
///
/// A projection of the server's own `ComposeEntry`, not that type: the entry
/// carries a dozen more fields — config, seats, topics, command names — and
/// this listing shows seven of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PluginEntry {
    /// The plugin id.
    pub id: String,
    /// The package it loads from.
    pub root: String,
    /// The module inside that package.
    pub entry: String,
    /// Whether this entry is in the running plane's loaded set.
    pub loaded: bool,
    /// Services it declares.
    pub services: Vec<String>,
    /// Tools it declares.
    pub tools: Vec<String>,
    /// LLM providers it declares.
    #[serde(rename = "llmProviders")]
    pub llm_providers: Vec<String>,
}

/// One kernel loop vendor, and whether this session could switch to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct KernelLoopOption {
    /// The vendor id: the `/kernel` argument and the config key.
    pub id: String,
    /// Human label for the list.
    pub label: String,
    /// Whether this build carries the vendor's assembly. A known but
    /// unbundled vendor keeps its seat so the switch can say so loudly
    /// instead of "no such vendor".
    pub bundled: bool,
    /// The backend name a switch would name.
    #[serde(rename = "backendId")]
    pub backend_id: String,
    /// Whether a provider and model are configured for it.
    pub configured: bool,
    /// Whether the router has this backend to switch to.
    pub switchable: bool,
    /// The configured provider, or `null`.
    pub provider: Option<String>,
    /// The configured model, or `null`.
    pub model: Option<String>,
}

/// `GET /api/mcp` — the MCP servers this process configured and connected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpStatus {
    /// One per server, configured or merely connected.
    pub servers: Vec<McpServerEntry>,
    /// Why the configuration could not be read at all.
    pub problem: Option<String>,
    /// Per-server warnings the loader collected.
    ///
    /// The server has always sent these; declaring them here changes no
    /// bytes and lets the page read them without a cast.
    pub warnings: Vec<String>,
}

/// One MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServerEntry {
    /// The name it is configured or connected under.
    pub name: String,
    /// `connected`, `failed`, `not started` or `disconnected`.
    pub status: String,
    /// `stdio`, `http` or `sse`; `null` for a server that only the client
    /// knows about, which is how an env-declared one arrives.
    pub transport: Option<String>,
    /// The tool names it contributes, prefixed. Empty unless connected.
    pub tools: Vec<String>,
    /// The warning naming this server, if one does.
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Each of these compares a built value against the wire literal the
    /// server has always produced.
    #[test]
    fn a_lever_answers_two_fields_and_a_local_rewind_answers_four() {
        assert_eq!(
            serde_json::to_value(ActionResult::new(true, "Cancelled task-1")).unwrap(),
            json!({"ok": true, "message": "Cancelled task-1"}),
            "the extra two keys are absent, not null"
        );
        assert_eq!(
            serde_json::to_value(ActionResult {
                ok: true,
                message: "Rewound to turn 3".into(),
                prefill_prompt: Some("read the file".into()),
                turns: Some(3),
            })
            .unwrap(),
            json!({
                "ok": true,
                "message": "Rewound to turn 3",
                "prefillPrompt": "read the file",
                "turns": 3,
            })
        );
    }

    #[test]
    fn a_file_listing_reports_whether_the_index_is_still_being_built() {
        let listing = FilesResponse {
            root: "/work".into(),
            files: vec!["src/".into(), "src/main.rs".into()],
            truncated: false,
            scanning: true,
            problem: None,
        };
        assert_eq!(
            serde_json::to_value(&listing).unwrap(),
            json!({
                "root": "/work",
                "files": ["src/", "src/main.rs"],
                "truncated": false,
                "scanning": true,
            }),
            "`problem` is absent, not null, when there is none"
        );
    }

    #[test]
    fn usage_says_which_of_the_two_counts_it_is() {
        let usage = UsageResponse {
            session_id: "sess-1".into(),
            input_tokens: 120,
            output_tokens: 40,
            turns: 2,
            messages: Some(9),
            duration_ms: Some(65_000),
            model: "claude-opus-5".into(),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            problem: "Counted from this server's own turns; earlier turns are not on record."
                .into(),
        };
        assert_eq!(
            serde_json::to_value(&usage).unwrap(),
            json!({
                "sessionId": "sess-1",
                "inputTokens": 120,
                "outputTokens": 40,
                "turns": 2,
                "messages": 9,
                "durationMs": 65_000,
                "model": "claude-opus-5",
                "cacheReadTokens": null,
                "cacheCreationTokens": null,
                "problem": "Counted from this server's own turns; earlier turns are not on record.",
            })
        );
    }

    #[test]
    fn a_rewind_listing_keeps_its_camel_case_and_its_null_problem() {
        let listing = RewindResponse {
            checkpoints: vec![RewindCheckpoint {
                index: 1,
                uuid: "u-1".into(),
                timestamp: Some("2026-09-07T00:00:00.000Z".into()),
                preview: "read the file".into(),
                complete: true,
            }],
            supported: true,
            files_supported: true,
            problem: None,
        };
        assert_eq!(
            serde_json::to_value(&listing).unwrap(),
            json!({
                "checkpoints": [{
                    "index": 1,
                    "uuid": "u-1",
                    "timestamp": "2026-09-07T00:00:00.000Z",
                    "preview": "read the file",
                    "complete": true,
                }],
                "supported": true,
                "filesSupported": true,
                "problem": null,
            })
        );
    }

    #[test]
    fn a_history_snapshot_still_carries_the_deprecated_half() {
        let snapshot = HistorySnapshot {
            session_id: "sess-1".into(),
            cwd: "/work".into(),
            title: None,
            rows: json!([]),
            messages: json!([]),
        };
        assert_eq!(
            serde_json::to_value(&snapshot).unwrap(),
            json!({
                "sessionId": "sess-1",
                "cwd": "/work",
                "title": null,
                "rows": [],
                "messages": [],
            })
        );
    }

    #[test]
    fn server_info_serialises_the_shape_the_page_reads() {
        let info = ServerInfo {
            version: "0.24.0".into(),
            cwd: "/work".into(),
            provider: "anthropic".into(),
            model: "claude-opus-5".into(),
            agents: vec![AgentOption {
                id: "reviewer".into(),
                label: "Reviewer".into(),
            }],
            agent_config_option: "agent".into(),
            active_turns: vec!["sess-1".into()],
            web_ui: WebUiSource {
                source: "embedded".into(),
                path: None,
                built: Some("dist@abc123".into()),
            },
            steering: true,
            initialize: None,
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            json!({
                "version": "0.24.0",
                "cwd": "/work",
                "provider": "anthropic",
                "model": "claude-opus-5",
                "agents": [{"id": "reviewer", "label": "Reviewer"}],
                "agentConfigOption": "agent",
                "activeTurns": ["sess-1"],
                "webUi": {"source": "embedded", "path": null, "built": "dist@abc123"},
                "steering": true,
                "initialize": null,
            })
        );
    }

    #[test]
    fn the_two_asset_sources_keep_their_two_shapes() {
        let embedded = WebUiSource {
            source: "embedded".into(),
            path: None,
            built: Some("dist@abc123".into()),
        };
        assert_eq!(
            serde_json::to_value(&embedded).unwrap(),
            json!({"source": "embedded", "path": null, "built": "dist@abc123"})
        );

        let directory = WebUiSource {
            source: "directory".into(),
            path: Some("/work/assets/web-ui/dist".into()),
            built: None,
        };
        assert_eq!(
            serde_json::to_value(&directory).unwrap(),
            json!({"source": "directory", "path": "/work/assets/web-ui/dist"}),
            "a directory has never reported a `built` key"
        );
    }

    #[test]
    fn a_skill_entry_reads_as_the_menu_expects() {
        assert_eq!(
            CommandsResponse::skill_entry("release", "cut a release", Some("<version>")),
            json!({
                "name": "release",
                "description": "cut a release",
                "input": {"hint": "<version>"},
                "category": "skill",
                "aliases": [],
            })
        );
        // No argument hint is a null `input`, not a missing key.
        assert_eq!(
            CommandsResponse::skill_entry("status", "say where things are", None)["input"],
            Value::Null
        );
    }

    #[test]
    fn skills_and_agents_keep_their_camel_case_field() {
        let skills = SkillsResponse {
            skills: vec![SkillEntry {
                name: "release".into(),
                title: "Release".into(),
                description: "cut a release".into(),
                source: "project".into(),
                path: Some("/work/.rebon/skills/release".into()),
                enabled: true,
                user_invocable: true,
                hint: Some("<version>".into()),
            }],
        };
        assert_eq!(
            serde_json::to_value(&skills).unwrap(),
            json!({"skills": [{
                "name": "release",
                "title": "Release",
                "description": "cut a release",
                "source": "project",
                "path": "/work/.rebon/skills/release",
                "enabled": true,
                "userInvocable": true,
                "hint": "<version>",
            }]})
        );

        let agents = AgentsResponse {
            agents: vec![AgentEntry {
                name: "reviewer".into(),
                description: "reads a diff".into(),
                model: None,
                provider: None,
                effort: None,
                tools: None,
                runtime: "engine".into(),
                background: false,
                source: "plugin:review".into(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&agents).unwrap(),
            json!({"agents": [{
                "name": "reviewer",
                "description": "reads a diff",
                "model": null,
                "provider": null,
                "effort": null,
                "tools": null,
                "runtime": "engine",
                "background": false,
                "source": "plugin:review",
            }]})
        );
    }

    #[test]
    fn a_task_entry_keeps_every_key_including_the_null_ones() {
        let tasks = TasksResponse {
            tasks: vec![TaskEntry {
                id: "task-1".into(),
                kind: "agent".into(),
                status: "running".into(),
                title: "verify the fix".into(),
                description: None,
                error: None,
                output: None,
                backgrounded: false,
                created_at: Some("2026-09-07T00:00:00.000Z".into()),
                updated_at: None,
            }],
        };
        assert_eq!(
            serde_json::to_value(&tasks).unwrap(),
            json!({"tasks": [{
                "id": "task-1",
                "kind": "agent",
                "status": "running",
                "title": "verify the fix",
                "description": null,
                "error": null,
                "output": null,
                "backgrounded": false,
                "createdAt": "2026-09-07T00:00:00.000Z",
                "updatedAt": null,
            }]})
        );
    }

    #[test]
    fn models_lists_the_current_one_and_its_provider() {
        let models = ModelsResponse {
            provider: Some("openai".into()),
            current: "gpt-6-astra".into(),
            models: vec![ModelOption {
                id: "gpt-6-astra".into(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&models).unwrap(),
            json!({
                "provider": "openai",
                "current": "gpt-6-astra",
                "models": [{"id": "gpt-6-astra"}],
            })
        );
    }

    #[test]
    fn the_plugin_plane_reports_every_key_the_page_was_reading() {
        let status = PluginStatus {
            host: "node".into(),
            runtime: PluginRuntimeStatus {
                configured_plugins: 2,
                supported: ">=22".into(),
                resolved: Some(ResolvedRuntime {
                    version: "24.19.0".into(),
                    executable: "/home/a/.rebon/runtime/node/24.19.0/bin/node".into(),
                    origin: "managed".into(),
                }),
                problem: None,
                out_of_range: None,
                installable: true,
            },
            plane: PluginPlaneStatus {
                running: true,
                generation: Some(3),
                refusal: None,
                loaded: vec!["dsh".into()],
                problem: None,
            },
            entries: vec![PluginEntry {
                id: "dsh".into(),
                root: "packages/dsh".into(),
                entry: "dist/index.js".into(),
                loaded: true,
                services: vec!["llm".into()],
                tools: vec!["dsh_run".into()],
                llm_providers: vec!["deepseek".into()],
            }],
            skipped: vec!["review: no manifest".into()],
            loops: vec![KernelLoopOption {
                id: "dsh".into(),
                label: "dsh loop (deepseek-harness)".into(),
                bundled: true,
                backend_id: "kernel:dsh".into(),
                configured: true,
                switchable: false,
                provider: Some("deepseek".into()),
                model: None,
            }],
        };
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            json!({
                "host": "node",
                "runtime": {
                    "configuredPlugins": 2,
                    "supported": ">=22",
                    "resolved": {
                        "version": "24.19.0",
                        "executable": "/home/a/.rebon/runtime/node/24.19.0/bin/node",
                        "origin": "managed",
                    },
                    "problem": null,
                    "outOfRange": null,
                    "installable": true,
                },
                "plane": {
                    "running": true,
                    "generation": 3,
                    "refusal": null,
                    "loaded": ["dsh"],
                    "problem": null,
                },
                "entries": [{
                    "id": "dsh",
                    "root": "packages/dsh",
                    "entry": "dist/index.js",
                    "loaded": true,
                    "services": ["llm"],
                    "tools": ["dsh_run"],
                    "llmProviders": ["deepseek"],
                }],
                "skipped": ["review: no manifest"],
                "loops": [{
                    "id": "dsh",
                    "label": "dsh loop (deepseek-harness)",
                    "bundled": true,
                    "backendId": "kernel:dsh",
                    "configured": true,
                    "switchable": false,
                    "provider": "deepseek",
                    "model": null,
                }],
            })
        );
    }

    #[test]
    fn a_plane_that_never_started_keeps_its_null_keys() {
        // The state a fresh install reports, and the one the panel has to be
        // able to draw: nothing resolved, nothing running, nothing composed.
        let status = PluginStatus {
            host: "node".into(),
            runtime: PluginRuntimeStatus {
                configured_plugins: 0,
                supported: ">=22".into(),
                resolved: None,
                problem: Some("no usable Node runtime".into()),
                out_of_range: Some(OutOfRangeRuntime {
                    version: "18.20.4".into(),
                    executable: "/usr/bin/node".into(),
                }),
                installable: false,
            },
            plane: PluginPlaneStatus {
                running: false,
                generation: None,
                refusal: None,
                loaded: Vec::new(),
                problem: None,
            },
            entries: Vec::new(),
            skipped: Vec::new(),
            loops: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            json!({
                "host": "node",
                "runtime": {
                    "configuredPlugins": 0,
                    "supported": ">=22",
                    "resolved": null,
                    "problem": "no usable Node runtime",
                    "outOfRange": {
                        "version": "18.20.4",
                        "executable": "/usr/bin/node",
                    },
                    "installable": false,
                },
                "plane": {
                    "running": false,
                    "generation": null,
                    "refusal": null,
                    "loaded": [],
                    "problem": null,
                },
                "entries": [],
                "skipped": [],
                "loops": [],
            })
        );
    }

    #[test]
    fn mcp_lists_a_connected_server_and_a_failed_one() {
        let status = McpStatus {
            servers: vec![
                McpServerEntry {
                    name: "files".into(),
                    status: "connected".into(),
                    transport: Some("stdio".into()),
                    tools: vec!["mcp__files__read".into()],
                    error: None,
                },
                McpServerEntry {
                    name: "search".into(),
                    status: "failed".into(),
                    transport: Some("http".into()),
                    tools: Vec::new(),
                    error: Some("search: connection refused".into()),
                },
            ],
            problem: None,
            warnings: vec!["search: connection refused".into()],
        };
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            json!({
                "servers": [
                    {
                        "name": "files",
                        "status": "connected",
                        "transport": "stdio",
                        "tools": ["mcp__files__read"],
                        "error": null,
                    },
                    {
                        "name": "search",
                        "status": "failed",
                        "transport": "http",
                        "tools": [],
                        "error": "search: connection refused",
                    },
                ],
                "problem": null,
                "warnings": ["search: connection refused"],
            })
        );
    }

    #[test]
    fn a_server_only_the_client_knows_has_no_transport() {
        // An env-declared server: connected, but no config names it, so the
        // transport is a null key rather than a missing one.
        let status = McpStatus {
            servers: vec![McpServerEntry {
                name: "ambient".into(),
                status: "connected".into(),
                transport: None,
                tools: Vec::new(),
                error: None,
            }],
            problem: Some("settings.json: expected an object".into()),
            warnings: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            json!({
                "servers": [{
                    "name": "ambient",
                    "status": "connected",
                    "transport": null,
                    "tools": [],
                    "error": null,
                }],
                "problem": "settings.json: expected an object",
                "warnings": [],
            })
        );
    }
}
