//! JSON-RPC 2.0 and ACP protocol types.
//!
//! Shared content/tool/session-update types live in `rebon-types` and are
//! re-exported here so ACP consumers can take everything from one crate root.
//! Defined locally: the JSON-RPC 2.0 base types, the ACP protocol messages
//! (initialize, session setup) and the whole `session/request_permission`
//! round-trip — request, result, wire result, options and outcome. The
//! permission shapes have exactly one definition and it is this one.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

// Re-export the shared types from rebon-types so ACP protocol consumers can
// take everything from one crate root. These are the ones the engine, the
// session store and the TUI all handle as internal domain values, so they
// stay in the dependency-light crate. Code that only needs them should depend
// on `rebon-types` directly instead of routing through this wire-layer crate.
pub use rebon_types::{
    AudioContent, ConfigOption, ConfigOptionType, ConfigOptionValue, ContentBlock, DiffContent,
    ImageContent, PlanEntry, PlanEntryPriority, PlanEntryStatus, RegularContent, ResourceBody,
    ResourceContent, ResourceLinkContent, SessionId, SessionUpdate, SessionUpdateParams,
    SlashCommand, SlashCommandCategory, SlashCommandInput, StopReason, TerminalContent,
    TextContent, ToolCallContent, ToolCallLocation, ToolCallReference, ToolCallStatus, ToolKind,
};

// ==========================
// JSON-RPC 2.0 base types
// ==========================

/// Standard JSON-RPC 2.0 error codes.
pub mod error_code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    /// Implementation-defined: the session is open in another process, which
    /// holds its active lock. `data.owner` carries what is known about that
    /// owner. Inside the `-32000..=-32099` range JSON-RPC 2.0 reserves for
    /// server-defined errors.
    pub const SESSION_OWNED_ELSEWHERE: i32 = -32000;

    // The `_session/*` extension's own failures. Also inside the
    // server-defined range, and numbered from -32010 rather than -32001 to
    // leave a gap after the code above: -32000 is already answered by
    // `session/load` when a session is open elsewhere, and one number cannot
    // mean two things on one wire, so the block moved down whole and kept its
    // order.
    //
    // The code says *which kind* of failure; the typed variant name rides on
    // `data.kind`. A client reads one of those two, never the message text.

    /// The deadline passed with no answer. The waiter has been released; a
    /// retry is meaningful.
    pub const HOST_UNANSWERED: i32 = -32010;
    /// The token did not match. Fail-closed, and never retried with the same
    /// token.
    pub const UNAUTHENTICATED: i32 = -32011;
    /// The request named a job or session this owner is not. Not "you are not
    /// allowed" — "you are talking to the wrong process".
    pub const OWNER_FENCE: i32 = -32012;
    /// The turn, query or endpoint generation this was fenced against has
    /// moved on, so applying it now would land it on something else.
    pub const STALE_GENERATION: i32 = -32013;
    /// The owner could not be reached, or is shutting down and no longer
    /// taking work.
    pub const OWNER_UNREACHABLE: i32 = -32014;
    /// A permission answer named an option the owner could not fold into any
    /// it offered, *and* had nothing to reject with. An unknown option that
    /// can be folded is not an error — it is answered, folded, and reported
    /// as success.
    pub const NO_USABLE_OPTION: i32 = -32015;
    /// The owner refused on policy grounds. A refusal is an answer, not a
    /// fault: it means the request was understood and declined.
    pub const PERMISSION_REJECTED: i32 = -32016;
    /// The call was released before it finished -- by `_session/cancel_call`,
    /// by a cancelled turn, or by the host shutting down.
    ///
    /// Its own code rather than the catch-all because a client *branches* on
    /// it: a call that stopped half way may be worth retrying, while a refusal
    /// is a decision and retrying it only repeats the refusal. A distinction
    /// that changes what the caller does next must be readable from the code,
    /// not only from `data.kind`.
    pub const CALL_CANCELLED: i32 = -32017;
    /// Everything else the host could not do. `data.kind` says which typed
    /// failure it was; the catch-all exists so an owner never has to invent a
    /// number for a case the client does not know yet.
    pub const HOST_CALL_FAILED: i32 = -32099;
}

/// A JSON-RPC 2.0 request/response id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl From<i64> for RequestId {
    fn from(v: i64) -> Self {
        Self::Number(v)
    }
}

impl From<String> for RequestId {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}

impl<'a> From<&'a str> for RequestId {
    fn from(v: &'a str) -> Self {
        Self::String(v.to_string())
    }
}

/// Marker for the `"jsonrpc": "2.0"` field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JsonRpcVersion;

impl Serialize for JsonRpcVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s != "2.0" {
            return Err(serde::de::Error::custom(format!(
                "expected jsonrpc version \"2.0\", got {s:?}"
            )));
        }
        Ok(Self)
    }
}

/// A JSON-RPC 2.0 request (method call from peer, expects a response).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 notification (method call with no response).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: Option<RequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
// `PartialEq` and not `Eq`: `data` is a `Value`, which may hold a float, and
// floats have no total equality. Comparable at all because a test that asserts
// *which* error was produced is the normal way to pin a refusal. Not a doc
// comment: schemars turns those into the schema's `description`, and how this
// struct derives its traits is not something a browser client should read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    pub fn parse_error(msg: impl Into<String>) -> Self {
        Self {
            code: error_code::PARSE_ERROR,
            message: msg.into(),
            data: None,
        }
    }
    pub fn invalid_request(msg: impl Into<String>) -> Self {
        Self {
            code: error_code::INVALID_REQUEST,
            message: msg.into(),
            data: None,
        }
    }
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: error_code::METHOD_NOT_FOUND,
            message: format!("Method not found: {method}"),
            data: None,
        }
    }
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: error_code::INVALID_PARAMS,
            message: msg.into(),
            data: None,
        }
    }
    /// The session is open in another process. `owner` is whatever the
    /// refusing side could learn about that process; it rides on `data.owner`
    /// so a client can render "open in <surface>" instead of a bare string.
    pub fn session_owned_elsewhere(msg: impl Into<String>, owner: Value) -> Self {
        Self {
            code: error_code::SESSION_OWNED_ELSEWHERE,
            message: msg.into(),
            data: Some(serde_json::json!({ "owner": owner })),
        }
    }
    pub fn internal_error(msg: impl Into<String>) -> Self {
        Self {
            code: error_code::INTERNAL_ERROR,
            message: msg.into(),
            data: None,
        }
    }
}

/// Classify a raw JSON value into one of the three JSON-RPC 2.0 message kinds.
#[derive(Debug, Clone)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Response(JsonRpcResponse),
}

/// Errors produced while classifying an incoming JSON-RPC body.
#[derive(Debug, thiserror::Error)]
pub enum JsonRpcParseError {
    #[error("invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("invalid JSON-RPC request")]
    InvalidRequest,
}

impl JsonRpcMessage {
    pub fn from_bytes(body: &[u8]) -> Result<Self, JsonRpcParseError> {
        let value: Value = serde_json::from_slice(body)?;
        Self::from_value(value)
    }

    pub fn from_value(value: Value) -> Result<Self, JsonRpcParseError> {
        if !value.is_object() {
            return Err(JsonRpcParseError::InvalidRequest);
        }
        let has_method = value.get("method").is_some();
        let has_id = value.get("id").is_some();

        if has_method && has_id {
            Ok(Self::Request(serde_json::from_value(value)?))
        } else if has_method {
            Ok(Self::Notification(serde_json::from_value(value)?))
        } else if has_id {
            Ok(Self::Response(serde_json::from_value(value)?))
        } else {
            Err(JsonRpcParseError::InvalidRequest)
        }
    }
}

// ==========================
// ACP protocol types
// ==========================

pub type ProtocolVersion = i32;

// --- Initialization ---

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs: Option<FsCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_text_file: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_text_file: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AgentCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_session: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_capabilities: Option<SessionCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_capabilities: Option<PromptCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_capabilities: Option<McpCapabilities>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<SessionListCapability>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionListCapability {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sse: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PromptCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded_context: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImplementationInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: ProtocolVersion,
    pub client_capabilities: ClientCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ImplementationInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct InitializeResult {
    pub protocol_version: ProtocolVersion,
    pub agent_capabilities: AgentCapabilities,
    #[serde(default)]
    pub auth_methods: Vec<AuthMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<ImplementationInfo>,
    /// Top-level extension data — a sibling of `agentCapabilities`,
    /// not inside it. This is where the steering wire protocol
    /// advertises itself: both `claude-agent-acp` and `codex-acp` put
    /// `{"steering": {"supported": true}}` here. `default` keeps
    /// agents without it decodable, which is also the compatibility
    /// story: no `_meta`, no steering, fall back to queueing.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<HashMap<String, Value>>,
}

// --- Steering (extension) ---

/// Params of the `_session/steering` extension request: inject a user
/// message into the turn that is currently running, instead of
/// queueing it as a separate `session/prompt`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSteeringParams {
    pub session_id: SessionId,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionSteeringResult {
    pub outcome: SteeringOutcome,
}

/// What became of a steering request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SteeringOutcome {
    /// Delivered into the running turn.
    Injected,
    /// The turn raced ahead and finished; the agent started a fresh
    /// turn with the message instead. Fire-and-forget on the agent's
    /// side — the new turn's output flows through `session/update`
    /// with nobody awaiting a prompt response.
    StartedNewTurn,
}

/// One way to authenticate with an agent, as advertised in
/// `initialize`.
///
/// The spec's shape is `{ id, name, description? }`. Every field is
/// optional here and unknown keys are kept, on purpose: nothing in
/// Rebon reads this yet, and an agent that grows a field would
/// otherwise fail the *whole* handshake over a value nobody looked at.
/// A real agent already broke this once — `@agentclientprotocol/codex-acp`
/// sends `{id, name, description, _meta}`, and requiring a `type` field
/// that the spec never had made `initialize` undecodable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuthMethod {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(flatten, default)]
    pub extra: HashMap<String, Value>,
}

// --- Session setup ---

/// One MCP server, as `session/new`'s `mcpServers` carries it.
///
/// The in-memory shape keeps `env`/`headers` as maps because that is
/// what every caller wants to build and read. The wire shape is the
/// spec's, which is different in two ways that really matter:
///
/// - `env` and `headers` are **arrays of `{name, value}` objects**,
///   not maps, and stdio's `env`/`args` are required even when empty;
/// - stdio has **no discriminator field**, while http/sse carry
///   `type`. There is no `transport` tag in the protocol.
///
/// Sending anything else is worse than an error: real agents
/// (`codex-acp`) validate each entry with a *skip-on-error* rule, so a
/// misshapen server is silently dropped and the session starts without
/// it. Deserialization additionally accepts the legacy
/// `transport`-tagged / map-valued shape this crate used to emit, so
/// older peers and stored fixtures keep parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerConfig {
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<String>,
    },
    Http {
        name: String,
        url: String,
        headers: BTreeMap<String, String>,
    },
    Sse {
        name: String,
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl McpServerConfig {
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio { name, .. } | Self::Http { name, .. } | Self::Sse { name, .. } => name,
        }
    }
}

/// A map serialized the way the spec spells key/value lists:
/// `[{"name": ..., "value": ...}, ...]`.
fn serialize_name_value_list<S: serde::Serializer>(
    map: &BTreeMap<String, String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;
    let mut seq = serializer.serialize_seq(Some(map.len()))?;
    #[derive(Serialize)]
    struct Entry<'a> {
        name: &'a str,
        value: &'a str,
    }
    for (name, value) in map {
        seq.serialize_element(&Entry { name, value })?;
    }
    seq.end()
}

impl Serialize for McpServerConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct StdioWire<'a> {
            name: &'a str,
            command: &'a str,
            // Required by the spec even when empty — an absent `args`
            // or `env` fails validation on strict agents.
            args: &'a [String],
            #[serde(serialize_with = "serialize_name_value_list")]
            env: &'a BTreeMap<String, String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            cwd: &'a Option<String>,
        }
        #[derive(Serialize)]
        struct RemoteWire<'a> {
            #[serde(rename = "type")]
            kind: &'a str,
            name: &'a str,
            url: &'a str,
            #[serde(serialize_with = "serialize_name_value_list")]
            headers: &'a BTreeMap<String, String>,
        }
        match self {
            Self::Stdio {
                name,
                command,
                args,
                env,
                cwd,
            } => StdioWire {
                name,
                command,
                args,
                env,
                cwd,
            }
            .serialize(serializer),
            Self::Http { name, url, headers } => RemoteWire {
                kind: "http",
                name,
                url,
                headers,
            }
            .serialize(serializer),
            Self::Sse { name, url, headers } => RemoteWire {
                kind: "sse",
                name,
                url,
                headers,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// `env`/`headers` in either spelling: the spec's `[{name,
        /// value}]` list or the legacy map.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum NameValueRepr {
            List(Vec<NameValueEntry>),
            Map(BTreeMap<String, String>),
        }
        #[derive(Deserialize)]
        struct NameValueEntry {
            name: String,
            value: String,
        }
        impl Default for NameValueRepr {
            fn default() -> Self {
                Self::Map(BTreeMap::new())
            }
        }
        impl NameValueRepr {
            fn into_map(self) -> BTreeMap<String, String> {
                match self {
                    Self::Map(map) => map,
                    Self::List(entries) => entries
                        .into_iter()
                        .map(|entry| (entry.name, entry.value))
                        .collect(),
                }
            }
        }

        #[derive(Deserialize)]
        struct Wire {
            name: String,
            /// The spec's discriminator for http/sse; absent for stdio.
            #[serde(rename = "type")]
            kind: Option<String>,
            /// Legacy discriminator this crate used to emit.
            transport: Option<String>,
            command: Option<String>,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default)]
            env: NameValueRepr,
            cwd: Option<String>,
            url: Option<String>,
            #[serde(default)]
            headers: NameValueRepr,
        }

        let wire = Wire::deserialize(deserializer)?;
        let kind = wire
            .kind
            .or(wire.transport)
            .map(|kind| kind.to_ascii_lowercase());
        match kind.as_deref() {
            Some("http") | Some("sse") => {
                let url = wire.url.ok_or_else(|| {
                    serde::de::Error::custom("an http/sse MCP server needs a `url`")
                })?;
                let headers = wire.headers.into_map();
                if kind.as_deref() == Some("http") {
                    Ok(Self::Http {
                        name: wire.name,
                        url,
                        headers,
                    })
                } else {
                    Ok(Self::Sse {
                        name: wire.name,
                        url,
                        headers,
                    })
                }
            }
            Some("stdio") | None => Ok(Self::Stdio {
                name: wire.name,
                command: wire.command.ok_or_else(|| {
                    serde::de::Error::custom("a stdio MCP server needs a `command`")
                })?,
                args: wire.args,
                env: wire.env.into_map(),
                cwd: wire.cwd,
            }),
            Some(other) => Err(serde::de::Error::custom(format!(
                "unknown MCP server type `{other}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewParams {
    pub cwd: String,
    /// Always serialized, even when empty. `mcpServers` is required by
    /// the spec, and omitting it is not the same as sending `[]` to an
    /// agent that validates its params — `codex-acp` answers
    /// `-32602 Invalid params` for the missing key. `default` keeps the
    /// server side tolerant of clients that leave it out.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Free-form extension data for the agent. This is how adapters
    /// take options the spec has no field for — `claude-agent-acp`
    /// reads `_meta.claudeCode.options` (SDK options: `disallowedTools`
    /// and friends), which is the practical way to disable an agent's
    /// own edit tools so the injected ones actually get used.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionNewResult {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<ConfigOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slash_commands: Option<Vec<SlashCommand>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLoadParams {
    pub session_id: SessionId,
    pub cwd: String,
    /// Always serialized — see [`SessionNewParams::mcp_servers`].
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// See [`SessionNewParams::meta`] — sent on load too, so a
    /// restored session keeps the same adapter options it started with.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionLoadResult {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<ConfigOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slash_commands: Option<Vec<SlashCommand>>,
}

// --- Prompt turn ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptParams {
    pub session_id: SessionId,
    pub prompt: Vec<ContentBlock>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionPromptResult {
    pub stop_reason: StopReason,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCancelParams {
    pub session_id: SessionId,
}

// --- Session list ---

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSetConfigOptionParams {
    pub session_id: SessionId,
    pub config_id: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionSetConfigOptionResult {
    pub config_options: Vec<ConfigOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionListResult {
    pub sessions: Vec<SessionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// --- Filesystem (agent → client) ---
//
// These are the reverse direction: the *agent* asks the client to read
// or write a file rather than touching the disk itself. A client that
// advertises `ClientCapabilities::fs` is asking for exactly that, and
// the reason to ask is that a write routed back through the client is
// a write the client can snapshot, permission-check, and undo.

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadTextFileParams {
    pub session_id: SessionId,
    /// Absolute path to read.
    pub path: String,
    /// 1-based line to start from. `None` means the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Maximum number of lines to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadTextFileResult {
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteTextFileParams {
    pub session_id: SessionId,
    /// Absolute path to write.
    pub path: String,
    /// Full new contents of the file.
    pub content: String,
}

/// `fs/write_text_file` has no result payload. Kept as a named type so
/// the client's request plumbing stays uniform across methods.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteTextFileResult {}

// ==========================
// Permission types
// ==========================

/// Permission option kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// One option offered to the client when the agent asks for permission.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// Request body for `session/request_permission`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RequestPermissionParams {
    pub session_id: SessionId,
    pub tool_call: ToolCallReference,
    pub options: Vec<PermissionOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Flat host-side outcome for `session/request_permission`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RequestPermissionResult {
    pub outcome: PermissionOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<serde_json::Value>,
}

/// Wire-format response body for `session/request_permission`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RequestPermissionWireResult {
    pub outcome: RequestPermissionResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<serde_json::Value>,
}

/// Outcome of a `session/request_permission` round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PermissionOutcome {
    Selected,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_new_params_accept_named_mcp_servers() {
        let params: SessionNewParams = serde_json::from_value(serde_json::json!({
            "cwd": "/tmp/work",
            "mcpServers": [
                {"transport":"stdio", "name":"fs", "command":"node", "args":["server.js"], "env":{"B":"2", "A":"1"}, "cwd":"/tmp/mcp"},
                {"transport":"http", "name":"remote", "url":"https://example.test/mcp", "headers":{"Authorization":"Bearer x"}},
                {"transport":"sse", "name":"events", "url":"https://example.test/sse", "headers":{"X-Test":"1"}}
            ]
        }))
        .unwrap();
        assert_eq!(params.mcp_servers.len(), 3);
        assert_eq!(params.mcp_servers[0].name(), "fs");
        match &params.mcp_servers[0] {
            McpServerConfig::Stdio { env, cwd, .. } => {
                assert_eq!(env.keys().cloned().collect::<Vec<_>>(), vec!["A", "B"]);
                assert_eq!(cwd.as_deref(), Some("/tmp/mcp"));
            }
            other => panic!("expected stdio config, got {other:?}"),
        }
    }

    #[test]
    fn mcp_servers_serialize_to_the_spec_wire_shape() {
        // The shape real agents validate — codex-acp drops any entry
        // that fails its schema *silently*, so getting this wrong
        // means tools vanish without an error anywhere.
        let stdio = McpServerConfig::Stdio {
            name: "rebon-fs".into(),
            command: "rebon".into(),
            args: vec!["__acp-fs-mcp".into()],
            env: BTreeMap::from([("PORT".to_string(), "1".to_string())]),
            cwd: None,
        };
        assert_eq!(
            serde_json::to_value(&stdio).unwrap(),
            serde_json::json!({
                "name": "rebon-fs",
                "command": "rebon",
                "args": ["__acp-fs-mcp"],
                "env": [{"name": "PORT", "value": "1"}],
            }),
            "stdio: no discriminator, env as a name/value list"
        );

        let empty = McpServerConfig::Stdio {
            name: "bare".into(),
            command: "bin".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        };
        let value = serde_json::to_value(&empty).unwrap();
        assert_eq!(value["args"], serde_json::json!([]));
        assert_eq!(
            value["env"],
            serde_json::json!([]),
            "args and env are required by the spec even when empty"
        );

        let http = McpServerConfig::Http {
            name: "remote".into(),
            url: "https://example.test/mcp".into(),
            headers: BTreeMap::from([("Authorization".to_string(), "Bearer x".to_string())]),
        };
        assert_eq!(
            serde_json::to_value(&http).unwrap(),
            serde_json::json!({
                "type": "http",
                "name": "remote",
                "url": "https://example.test/mcp",
                "headers": [{"name": "Authorization", "value": "Bearer x"}],
            }),
            "http/sse carry `type`, not `transport`"
        );
    }

    #[test]
    fn mcp_servers_parse_the_spec_shape_and_round_trip() {
        // What a spec-conformant editor (Zed) actually sends.
        let parsed: McpServerConfig = serde_json::from_value(serde_json::json!({
            "name": "filesystem",
            "command": "/path/to/mcp-server",
            "args": ["--stdio"],
            "env": [{"name": "API_KEY", "value": "secret123"}],
        }))
        .unwrap();
        let McpServerConfig::Stdio { env, .. } = &parsed else {
            panic!("no discriminator means stdio");
        };
        assert_eq!(env.get("API_KEY").map(String::as_str), Some("secret123"));

        // And whatever this crate emits must parse back to itself.
        let round: McpServerConfig =
            serde_json::from_value(serde_json::to_value(&parsed).unwrap()).unwrap();
        assert_eq!(round, parsed);

        let sse: McpServerConfig = serde_json::from_value(serde_json::json!({
            "type": "sse",
            "name": "events",
            "url": "https://example.test/sse",
            "headers": [{"name": "X-Test", "value": "1"}],
        }))
        .unwrap();
        assert!(
            matches!(sse, McpServerConfig::Sse { ref headers, .. } if headers["X-Test"] == "1")
        );
    }

    #[test]
    fn session_load_params_accept_named_mcp_servers() {
        let params: SessionLoadParams = serde_json::from_value(serde_json::json!({
            "sessionId": "sess-1",
            "cwd": "/tmp/work",
            "mcpServers": [
                {"transport":"stdio", "name":"fs", "command":"node"},
                {"transport":"http", "name":"remote", "url":"https://example.test/mcp"},
                {"transport":"sse", "name":"events", "url":"https://example.test/sse"}
            ]
        }))
        .unwrap();
        assert_eq!(params.session_id, "sess-1");
        assert_eq!(
            params
                .mcp_servers
                .iter()
                .map(|server| server.name().to_string())
                .collect::<Vec<_>>(),
            vec!["fs", "remote", "events"]
        );
    }

    #[test]
    fn session_load_result_round_trips_resume_metadata() {
        let result: SessionLoadResult = serde_json::from_value(serde_json::json!({
            "sessionId": "sess-1",
            "configOptions": [{
                "id": "permissions",
                "name": "Permissions",
                "category": "mode",
                "type": "select",
                "currentValue": "default",
                "options": []
            }],
            "slashCommands": [{
                "name": "status",
                "description": "Show session status",
                "category": "command"
            }]
        }))
        .unwrap();
        assert_eq!(result.session_id, "sess-1");
        assert_eq!(result.config_options.as_ref().map(Vec::len), Some(1));
        assert_eq!(result.slash_commands.as_ref().map(Vec::len), Some(1));

        let wire = serde_json::to_value(result).unwrap();
        assert!(wire.get("configOptions").is_some());
        assert!(wire.get("slashCommands").is_some());
        assert!(wire.get("config_options").is_none());
        assert!(wire.get("slash_commands").is_none());
    }

    #[test]
    fn jsonrpc_version_round_trips() {
        let s = serde_json::to_string(&JsonRpcVersion).unwrap();
        assert_eq!(s, "\"2.0\"");
        let v: JsonRpcVersion = serde_json::from_str("\"2.0\"").unwrap();
        let _ = v;
    }

    #[test]
    fn jsonrpc_version_rejects_wrong_string() {
        let err = serde_json::from_str::<JsonRpcVersion>("\"1.0\"");
        assert!(err.is_err(), "expected rejection of non-2.0 version");
    }

    #[test]
    fn request_round_trips() {
        let req = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(7),
            method: "initialize".into(),
            params: Some(serde_json::json!({"protocolVersion": 1})),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"jsonrpc\":\"2.0\""));
        assert!(s.contains("\"id\":7"));
        assert!(s.contains("\"method\":\"initialize\""));

        let back: JsonRpcRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, RequestId::Number(7));
        assert_eq!(back.method, "initialize");
    }

    #[test]
    fn classify_request() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#;
        let msg = JsonRpcMessage::from_bytes(body).unwrap();
        assert!(matches!(msg, JsonRpcMessage::Request(_)));
    }

    #[test]
    fn classify_notification() {
        let body = br#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s1"}}"#;
        let msg = JsonRpcMessage::from_bytes(body).unwrap();
        assert!(matches!(msg, JsonRpcMessage::Notification(_)));
    }

    #[test]
    fn classify_response() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let msg = JsonRpcMessage::from_bytes(body).unwrap();
        assert!(matches!(msg, JsonRpcMessage::Response(_)));
    }

    #[test]
    fn classify_rejects_garbage() {
        let body = br#"{"jsonrpc":"2.0"}"#;
        let err = JsonRpcMessage::from_bytes(body).unwrap_err();
        assert!(matches!(err, JsonRpcParseError::InvalidRequest));
    }

    #[test]
    fn classify_rejects_bad_json() {
        let body = br#"{not-json"#;
        let err = JsonRpcMessage::from_bytes(body).unwrap_err();
        assert!(matches!(err, JsonRpcParseError::InvalidJson(_)));
    }

    #[test]
    fn initialize_params_round_trip_camel_case() {
        let json = serde_json::json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": { "readTextFile": true, "writeTextFile": false },
                "terminal": true
            },
            "clientInfo": { "name": "zed", "version": "0.123" }
        });
        let p: InitializeParams = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(p.protocol_version, 1);
        assert_eq!(
            p.client_capabilities.fs.as_ref().unwrap().read_text_file,
            Some(true)
        );
        assert_eq!(p.client_info.as_ref().unwrap().name, "zed");
        let out = serde_json::to_value(&p).unwrap();
        assert_eq!(out["protocolVersion"], 1);
        assert!(out["clientCapabilities"]["fs"]["readTextFile"]
            .as_bool()
            .unwrap());
    }

    #[test]
    fn content_block_tag_discriminates() {
        let text: ContentBlock = serde_json::from_value(serde_json::json!({
            "type": "text",
            "text": "hello",
        }))
        .unwrap();
        assert!(matches!(text, ContentBlock::Text(_)));

        let img: ContentBlock = serde_json::from_value(serde_json::json!({
            "type": "image",
            "mimeType": "image/png",
            "data": "BASE64",
        }))
        .unwrap();
        assert!(matches!(img, ContentBlock::Image(_)));

        let link: ContentBlock = serde_json::from_value(serde_json::json!({
            "type": "resource_link",
            "uri": "file:///tmp/a.txt",
            "name": "a.txt",
        }))
        .unwrap();
        assert!(matches!(link, ContentBlock::ResourceLink(_)));

        let res: ContentBlock = serde_json::from_value(serde_json::json!({
            "type": "resource",
            "resource": { "uri": "file:///tmp/a.txt", "text": "hi" },
        }))
        .unwrap();
        assert!(matches!(res, ContentBlock::Resource(_)));
    }

    #[test]
    fn stop_reason_serializes_snake_case() {
        let s = serde_json::to_string(&StopReason::EndTurn).unwrap();
        assert_eq!(s, "\"end_turn\"");
        let r: StopReason = serde_json::from_str("\"max_turn_requests\"").unwrap();
        assert_eq!(r, StopReason::MaxTurnRequests);
    }

    #[test]
    fn session_list_params_accepts_empty_and_camelcase() {
        let p: SessionListParams = serde_json::from_str("{}").unwrap();
        assert!(p.cwd.is_none());
        assert!(p.cursor.is_none());

        let p: SessionListParams =
            serde_json::from_value(serde_json::json!({ "cwd": "/tmp", "cursor": "abc" })).unwrap();
        assert_eq!(p.cwd.as_deref(), Some("/tmp"));
        assert_eq!(p.cursor.as_deref(), Some("abc"));
    }

    #[test]
    fn session_list_params_rejects_wrong_cwd_type() {
        let e = serde_json::from_value::<SessionListParams>(serde_json::json!({ "cwd": 42 }));
        assert!(e.is_err(), "cwd: 42 must be rejected by serde");
    }

    #[test]
    fn session_info_omits_optional_fields_when_none() {
        let info = SessionInfo {
            session_id: "sess-1".into(),
            cwd: "/tmp".into(),
            title: None,
            updated_at: None,
            meta: None,
        };
        let v = serde_json::to_value(&info).unwrap();
        assert_eq!(v["sessionId"], "sess-1");
        assert_eq!(v["cwd"], "/tmp");
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("title"));
        assert!(!obj.contains_key("updatedAt"));
        assert!(!obj.contains_key("_meta"));
    }

    #[test]
    fn session_info_meta_uses_underscore_prefix() {
        let mut meta = std::collections::HashMap::new();
        meta.insert("k".to_string(), serde_json::json!("v"));
        let info = SessionInfo {
            session_id: "sess-1".into(),
            cwd: "/tmp".into(),
            title: Some("hello".into()),
            updated_at: Some("2025-01-01T00:00:00.000Z".into()),
            meta: Some(meta),
        };
        let v = serde_json::to_value(&info).unwrap();
        assert_eq!(v["title"], "hello");
        assert_eq!(v["updatedAt"], "2025-01-01T00:00:00.000Z");
        assert_eq!(v["_meta"]["k"], "v");
        assert!(v.as_object().unwrap().get("meta").is_none());
    }

    #[test]
    fn session_list_result_omits_next_cursor_when_none() {
        let result = SessionListResult {
            sessions: Vec::new(),
            next_cursor: None,
        };
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["sessions"].as_array().unwrap().len(), 0);
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("nextCursor"));
    }

    #[test]
    fn session_list_result_round_trips_with_sessions() {
        let result = SessionListResult {
            sessions: vec![SessionInfo {
                session_id: "s1".into(),
                cwd: "/tmp".into(),
                title: None,
                updated_at: Some("2025-01-01T00:00:00.000Z".into()),
                meta: None,
            }],
            next_cursor: None,
        };
        let s = serde_json::to_string(&result).unwrap();
        let back: SessionListResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back.sessions.len(), 1);
        assert_eq!(back.sessions[0].session_id, "s1");
        assert!(back.next_cursor.is_none());
    }

    #[test]
    fn session_update_agent_message_chunk_wire_shape() {
        let upd = SessionUpdate::AgentMessageChunk {
            content: ContentBlock::Text(TextContent {
                text: "hi".into(),
                annotations: None,
            }),
        };
        let v = serde_json::to_value(&upd).unwrap();
        assert_eq!(v["sessionUpdate"], "agent_message_chunk");
        assert_eq!(v["content"]["type"], "text");
        assert_eq!(v["content"]["text"], "hi");

        let back: SessionUpdate = serde_json::from_value(v.clone()).unwrap();
        let reserialized = serde_json::to_value(&back).unwrap();
        assert_eq!(reserialized, v);
    }

    #[test]
    fn session_update_tool_call_wire_shape_with_camel_case_keys() {
        let mut raw_input = HashMap::new();
        raw_input.insert("file_path".into(), serde_json::json!("foo.rs"));

        let upd = SessionUpdate::ToolCall {
            tool_call_id: "toolu_01abc".into(),
            title: "Read foo.rs".into(),
            kind: ToolKind::Read,
            status: ToolCallStatus::Pending,
            content: None,
            locations: None,
            raw_input: Some(raw_input),
            raw_output: None,
        };
        let v = serde_json::to_value(&upd).unwrap();
        assert_eq!(v["sessionUpdate"], "tool_call");
        assert_eq!(v["toolCallId"], "toolu_01abc");
        assert_eq!(v["title"], "Read foo.rs");
        assert_eq!(v["kind"], "read");
        assert_eq!(v["status"], "pending");
        assert_eq!(v["rawInput"]["file_path"], "foo.rs");

        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("content"));
        assert!(!obj.contains_key("locations"));
        assert!(!obj.contains_key("rawOutput"));
        assert!(!obj.contains_key("tool_call_id"));
        assert!(!obj.contains_key("raw_input"));
        assert!(!obj.contains_key("raw_output"));

        let raw = serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "toolu_01abc",
            "title": "Read foo.rs",
            "kind": "read",
            "status": "pending",
            "rawInput": { "file_path": "foo.rs" }
        });
        let parsed: SessionUpdate = serde_json::from_value(raw.clone()).unwrap();
        match parsed {
            SessionUpdate::ToolCall {
                tool_call_id,
                title,
                kind,
                status,
                ..
            } => {
                assert_eq!(tool_call_id, "toolu_01abc");
                assert_eq!(title, "Read foo.rs");
                assert_eq!(kind, ToolKind::Read);
                assert_eq!(status, ToolCallStatus::Pending);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn session_update_tool_call_update_omits_unspecified_fields() {
        let upd = SessionUpdate::ToolCallUpdate {
            tool_call_id: "toolu_01abc".into(),
            status: Some(ToolCallStatus::InProgress),
            title: None,
            content: None,
            locations: None,
            raw_output: None,
        };
        let v = serde_json::to_value(&upd).unwrap();
        assert_eq!(v["sessionUpdate"], "tool_call_update");
        assert_eq!(v["toolCallId"], "toolu_01abc");
        assert_eq!(v["status"], "in_progress");
        let obj = v.as_object().unwrap();
        for key in ["title", "content", "locations", "rawOutput"] {
            assert!(!obj.contains_key(key), "{key} must be omitted");
        }
        assert_eq!(obj.len(), 3);
    }

    #[test]
    fn session_update_tool_call_update_completed_with_content_locations() {
        let upd = SessionUpdate::ToolCallUpdate {
            tool_call_id: "toolu_01abc".into(),
            status: Some(ToolCallStatus::Completed),
            title: None,
            content: Some(vec![ToolCallContent::Content(RegularContent {
                content: ContentBlock::Text(TextContent {
                    text: "fn main() {}".into(),
                    annotations: None,
                }),
            })]),
            locations: Some(vec![ToolCallLocation {
                path: "src/main.rs".into(),
                line: Some(42),
            }]),
            raw_output: None,
        };
        let v = serde_json::to_value(&upd).unwrap();
        assert_eq!(v["sessionUpdate"], "tool_call_update");
        assert_eq!(v["status"], "completed");
        let content_arr = v["content"].as_array().expect("content is array");
        assert_eq!(content_arr.len(), 1);
        assert_eq!(content_arr[0]["type"], "content");
        assert_eq!(content_arr[0]["content"]["type"], "text");
        assert_eq!(content_arr[0]["content"]["text"], "fn main() {}");
        let loc_arr = v["locations"].as_array().expect("locations is array");
        assert_eq!(loc_arr.len(), 1);
        assert_eq!(loc_arr[0]["path"], "src/main.rs");
        assert_eq!(loc_arr[0]["line"], 42);
    }

    #[test]
    fn session_update_tool_call_status_completed_handles_failed_status() {
        let upd = SessionUpdate::ToolCallUpdate {
            tool_call_id: "toolu_02".into(),
            status: Some(ToolCallStatus::Failed),
            title: None,
            content: None,
            locations: None,
            raw_output: None,
        };
        let v = serde_json::to_value(&upd).unwrap();
        assert_eq!(v["status"], "failed");
    }

    #[test]
    fn tool_call_location_line_handles_values_above_i32_max() {
        let large: u64 = (i32::MAX as u64) + 100;
        let loc = ToolCallLocation {
            path: "/some/generated.log".into(),
            line: Some(large),
        };
        let v = serde_json::to_value(&loc).unwrap();
        assert_eq!(v["line"].as_u64(), Some(large));

        let raw = serde_json::json!({
            "path": "/some/generated.log",
            "line": large,
        });
        let parsed: ToolCallLocation = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.line, Some(large));
    }

    #[test]
    fn diff_content_serializes_old_text_null_when_none() {
        let diff = DiffContent {
            path: "src/new.rs".into(),
            old_text: None,
            new_text: "fn added() {}".into(),
        };
        let v = serde_json::to_value(&diff).unwrap();
        assert_eq!(v["path"], "src/new.rs");
        assert!(v["oldText"].is_null());
        assert_eq!(v["newText"], "fn added() {}");

        let back: DiffContent = serde_json::from_value(v).unwrap();
        assert!(back.old_text.is_none());
        assert_eq!(back.new_text, "fn added() {}");
    }

    #[test]
    fn diff_content_serializes_old_text_string_when_some() {
        let diff = DiffContent {
            path: "src/changed.rs".into(),
            old_text: Some("old".into()),
            new_text: "new".into(),
        };
        let v = serde_json::to_value(&diff).unwrap();
        assert_eq!(v["oldText"], "old");
        assert_eq!(v["newText"], "new");
    }

    #[test]
    fn tool_call_content_diff_variant_wire_shape() {
        let tcc = ToolCallContent::Diff(DiffContent {
            path: "src/main.rs".into(),
            old_text: Some("a".into()),
            new_text: "b".into(),
        });
        let v = serde_json::to_value(&tcc).unwrap();
        assert_eq!(v["type"], "diff");
        assert_eq!(v["path"], "src/main.rs");
        assert_eq!(v["oldText"], "a");
        assert_eq!(v["newText"], "b");
    }

    #[test]
    fn tool_call_content_terminal_variant_wire_shape() {
        let tcc = ToolCallContent::Terminal(TerminalContent {
            terminal_id: "term-1".into(),
        });
        let v = serde_json::to_value(&tcc).unwrap();
        assert_eq!(v["type"], "terminal");
        assert_eq!(v["terminalId"], "term-1");
        assert!(!v.as_object().unwrap().contains_key("terminal_id"));
    }

    #[test]
    fn session_update_plan_wire_shape() {
        let upd = SessionUpdate::Plan {
            entries: vec![
                PlanEntry {
                    content: "step 1".into(),
                    priority: PlanEntryPriority::High,
                    status: PlanEntryStatus::InProgress,
                },
                PlanEntry {
                    content: "step 2".into(),
                    priority: PlanEntryPriority::Low,
                    status: PlanEntryStatus::Pending,
                },
            ],
        };
        let v = serde_json::to_value(&upd).unwrap();
        assert_eq!(v["sessionUpdate"], "plan");
        let entries = v["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["content"], "step 1");
        assert_eq!(entries[0]["priority"], "high");
        assert_eq!(entries[0]["status"], "in_progress");
        assert_eq!(entries[1]["status"], "pending");
    }

    #[test]
    fn session_update_slash_commands_wire_shape() {
        let upd = SessionUpdate::SlashCommands {
            commands: vec![
                SlashCommand {
                    name: "help".into(),
                    description: "Show help".into(),
                    input: None,
                    category: None,
                    aliases: Vec::new(),
                },
                SlashCommand {
                    name: "rename".into(),
                    description: "Rename session".into(),
                    input: Some(SlashCommandInput {
                        hint: Some("new title".into()),
                    }),
                    category: None,
                    aliases: vec!["mv".into()],
                },
            ],
        };
        let v = serde_json::to_value(&upd).unwrap();
        assert_eq!(v["sessionUpdate"], "slash_commands");
        let cmds = v["commands"].as_array().unwrap();
        assert_eq!(cmds.len(), 2);
        assert!(!cmds[0].as_object().unwrap().contains_key("input"));
        assert!(!cmds[0].as_object().unwrap().contains_key("aliases"));
        assert_eq!(cmds[1]["input"]["hint"], "new title");
        assert_eq!(cmds[1]["aliases"].as_array().unwrap()[0], "mv");
    }

    #[test]
    fn session_update_config_option_update_uses_camel_case_config_options_key() {
        let upd = SessionUpdate::ConfigOptionUpdate {
            config_options: vec![ConfigOption {
                id: "model".into(),
                name: "Model".into(),
                description: Some("AI model to use".into()),
                category: Some("model".into()),
                option_type: ConfigOptionType::Select,
                current_value: "claude-opus".into(),
                options: vec![ConfigOptionValue {
                    value: "claude-opus".into(),
                    name: "Claude Opus".into(),
                    description: None,
                }],
            }],
        };
        let v = serde_json::to_value(&upd).unwrap();
        assert_eq!(v["sessionUpdate"], "config_option_update");
        let opts = v["configOptions"].as_array().unwrap();
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0]["id"], "model");
        assert_eq!(opts[0]["type"], "select");
        assert_eq!(opts[0]["currentValue"], "claude-opus");
        let inner = opts[0]["options"].as_array().unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0]["value"], "claude-opus");
        assert!(!v.as_object().unwrap().contains_key("config_options"));
        assert!(!opts[0].as_object().unwrap().contains_key("current_value"));
    }

    #[test]
    fn session_update_session_info_update_omits_optional_fields() {
        let upd = SessionUpdate::SessionInfoUpdate {
            title: None,
            updated_at: None,
            meta: None,
        };
        let v = serde_json::to_value(&upd).unwrap();
        assert_eq!(v["sessionUpdate"], "session_info_update");
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        for key in ["title", "updatedAt", "_meta"] {
            assert!(!obj.contains_key(key), "{key} must be omitted");
        }
    }

    #[test]
    fn session_update_session_info_update_emits_meta_with_underscore_prefix() {
        let mut meta = HashMap::new();
        meta.insert("k".to_string(), serde_json::json!("v"));
        let upd = SessionUpdate::SessionInfoUpdate {
            title: Some("hello".into()),
            updated_at: Some("2025-01-01T00:00:00.000Z".into()),
            meta: Some(meta),
        };
        let v = serde_json::to_value(&upd).unwrap();
        assert_eq!(v["title"], "hello");
        assert_eq!(v["updatedAt"], "2025-01-01T00:00:00.000Z");
        assert_eq!(v["_meta"]["k"], "v");
        assert!(v.as_object().unwrap().get("meta").is_none());
    }

    #[test]
    fn session_update_params_envelope_round_trip() {
        let params = SessionUpdateParams {
            session_id: "sess-1".into(),
            update: SessionUpdate::AgentMessageChunk {
                content: ContentBlock::Text(TextContent {
                    text: "delta".into(),
                    annotations: None,
                }),
            },
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["sessionId"], "sess-1");
        assert_eq!(v["update"]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(v["update"]["content"]["text"], "delta");

        let back: SessionUpdateParams = serde_json::from_value(v).unwrap();
        assert_eq!(back.session_id, "sess-1");
        assert!(matches!(
            back.update,
            SessionUpdate::AgentMessageChunk { .. }
        ));
    }

    #[test]
    fn unknown_session_update_discriminator_fails_to_deserialize() {
        let raw = serde_json::json!({
            "sessionUpdate": "totally_made_up_update",
            "anything": 1,
        });
        let result: Result<SessionUpdate, _> = serde_json::from_value(raw);
        assert!(result.is_err());
    }

    #[test]
    fn request_permission_params_wire_shape() {
        let params = RequestPermissionParams {
            session_id: "sess-1".into(),
            tool_call: ToolCallReference {
                tool_call_id: "toolu_01abc".into(),
            },
            options: vec![
                PermissionOption {
                    option_id: "allow_once".into(),
                    name: "Allow once".into(),
                    kind: PermissionOptionKind::AllowOnce,
                },
                PermissionOption {
                    option_id: "reject_always".into(),
                    name: "Reject always".into(),
                    kind: PermissionOptionKind::RejectAlways,
                },
            ],
            title: Some("Review workflow demo".into()),
            message: None,
            tool_name: None,
            tool_input: None,
            metadata: Some(serde_json::json!({"kind":"workflowReview"})),
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["sessionId"], "sess-1");
        assert_eq!(v["toolCall"]["toolCallId"], "toolu_01abc");
        let opts = v["options"].as_array().unwrap();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0]["optionId"], "allow_once");
        assert_eq!(opts[0]["kind"], "allow_once");
        assert_eq!(opts[1]["kind"], "reject_always");
        assert_eq!(v["title"], "Review workflow demo");
        assert_eq!(v["metadata"]["kind"], "workflowReview");

        let back: RequestPermissionParams = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(back.session_id, "sess-1");
        assert_eq!(back.tool_call.tool_call_id, "toolu_01abc");
        assert_eq!(back.options.len(), 2);
        assert_eq!(back.options[0].kind, PermissionOptionKind::AllowOnce);
        assert_eq!(back.options[1].kind, PermissionOptionKind::RejectAlways);
        assert_eq!(back.title.as_deref(), Some("Review workflow demo"));
        assert_eq!(back.metadata.unwrap()["kind"], "workflowReview");

        let mut legacy = v;
        legacy.as_object_mut().unwrap().remove("title");
        let legacy: RequestPermissionParams = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.title, None);

        let mut without_title = params;
        without_title.title = None;
        let without_title = serde_json::to_value(without_title).unwrap();
        assert!(!without_title.as_object().unwrap().contains_key("title"));
    }

    #[test]
    fn request_permission_result_selected_with_option_id() {
        let result = RequestPermissionResult {
            outcome: PermissionOutcome::Selected,
            option_id: Some("allow_once".into()),
            updated_input: None,
        };
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["outcome"], "selected");
        assert_eq!(v["optionId"], "allow_once");
    }

    #[test]
    fn request_permission_result_cancelled_omits_option_id() {
        let result = RequestPermissionResult {
            outcome: PermissionOutcome::Cancelled,
            option_id: None,
            updated_input: None,
        };
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["outcome"], "cancelled");
        assert!(!v.as_object().unwrap().contains_key("optionId"));
    }

    // The four golden tests below pin the permission round-trip byte for
    // byte. They are the guard on the shape now having exactly one
    // definition: any field rename, reordering, optionality change or
    // enum-string change breaks them before it reaches a client.

    #[test]
    fn permission_option_kind_strings_are_golden() {
        let kinds = [
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ];
        assert_eq!(
            serde_json::to_string(&kinds).unwrap(),
            r#"["allow_once","allow_always","reject_once","reject_always"]"#
        );
        let back: Vec<PermissionOptionKind> =
            serde_json::from_str(r#"["allow_once","allow_always","reject_once","reject_always"]"#)
                .unwrap();
        assert_eq!(back, kinds.to_vec());

        let outcomes = [PermissionOutcome::Selected, PermissionOutcome::Cancelled];
        assert_eq!(
            serde_json::to_string(&outcomes).unwrap(),
            r#"["selected","cancelled"]"#
        );
    }

    #[test]
    fn request_permission_params_full_json_is_golden() {
        let params = RequestPermissionParams {
            session_id: "sess-1".into(),
            tool_call: ToolCallReference {
                tool_call_id: "toolu_01abc".into(),
            },
            options: vec![PermissionOption {
                option_id: "allow_once".into(),
                name: "Allow once".into(),
                kind: PermissionOptionKind::AllowOnce,
            }],
            title: Some("Run a command".into()),
            message: Some("ls -la".into()),
            tool_name: Some("Bash".into()),
            tool_input: Some(serde_json::json!({"command": "ls -la"})),
            metadata: Some(serde_json::json!({"kind": "workflowReview"})),
        };
        let golden = concat!(
            r#"{"sessionId":"sess-1","toolCall":{"toolCallId":"toolu_01abc"},"#,
            r#""options":[{"optionId":"allow_once","name":"Allow once","kind":"allow_once"}],"#,
            r#""title":"Run a command","message":"ls -la","toolName":"Bash","#,
            r#""toolInput":{"command":"ls -la"},"metadata":{"kind":"workflowReview"}}"#
        );
        assert_eq!(serde_json::to_string(&params).unwrap(), golden);

        let back: RequestPermissionParams = serde_json::from_str(golden).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), golden);
    }

    #[test]
    fn request_permission_params_minimal_json_is_golden() {
        let params = RequestPermissionParams {
            session_id: "sess-1".into(),
            tool_call: ToolCallReference {
                tool_call_id: "toolu_01abc".into(),
            },
            options: vec![],
            title: None,
            message: None,
            tool_name: None,
            tool_input: None,
            metadata: None,
        };
        let golden =
            r#"{"sessionId":"sess-1","toolCall":{"toolCallId":"toolu_01abc"},"options":[]}"#;
        assert_eq!(serde_json::to_string(&params).unwrap(), golden);

        // Every optional field is absent on the wire and reads back as None.
        let back: RequestPermissionParams = serde_json::from_str(golden).unwrap();
        assert!(back.title.is_none());
        assert!(back.message.is_none());
        assert!(back.tool_name.is_none());
        assert!(back.tool_input.is_none());
        assert!(back.metadata.is_none());
    }

    #[test]
    fn request_permission_results_json_are_golden() {
        let selected = RequestPermissionResult {
            outcome: PermissionOutcome::Selected,
            option_id: Some("allow_once".into()),
            updated_input: Some(serde_json::json!({"command": "ls"})),
        };
        assert_eq!(
            serde_json::to_string(&selected).unwrap(),
            r#"{"outcome":"selected","optionId":"allow_once","updatedInput":{"command":"ls"}}"#
        );

        let cancelled = RequestPermissionResult {
            outcome: PermissionOutcome::Cancelled,
            option_id: None,
            updated_input: None,
        };
        assert_eq!(
            serde_json::to_string(&cancelled).unwrap(),
            r#"{"outcome":"cancelled"}"#
        );

        // The wire result nests the flat result under `outcome` and carries
        // its own `updatedInput` sibling.
        let wire = RequestPermissionWireResult {
            outcome: selected,
            updated_input: Some(serde_json::json!({"command": "ls"})),
        };
        assert_eq!(
            serde_json::to_string(&wire).unwrap(),
            concat!(
                r#"{"outcome":{"outcome":"selected","optionId":"allow_once","#,
                r#""updatedInput":{"command":"ls"}},"updatedInput":{"command":"ls"}}"#
            )
        );

        let bare = RequestPermissionWireResult {
            outcome: cancelled,
            updated_input: None,
        };
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"outcome":{"outcome":"cancelled"}}"#
        );
    }
}
