//! # MCP (Model Context Protocol) server management logic
//!
//! This module implements the pure-logic surface for managing MCP
//! servers: config parsing, connection status/reconnect state
//! machines, tool listing/detail projections, elicitation, and the
//! settings-panel view model. It produces plain data (structs,
//! enums, `String`s); rendering and IO are left to the consumer.
//!
//! ## What is in this module
//!
//! * [`config`] — serde-like discriminated-union Rust structs for the
//!   eight MCP server config variants (`stdio`, `sse`, `sse-ide`,
//!   `ws-ide`, `http`, `ws`, `sdk`, `claudeai-proxy`) + [`config::ConfigScope`]
//!   + [`config::ScopedMcpServerConfig`].
//! * [`status`] — [`status::McpServerStatus`] enum (Connected /
//!   NeedsAuth / Failed / Pending / Disabled) and the
//!   `transition` reducer for connection lifecycle.
//! * [`reconnect`] — [`reconnect::handle_reconnect_result`] /
//!   [`reconnect::handle_reconnect_error`] plus the [`reconnect::ReconnectState`]
//!   reducer (`idle` → `reconnecting` → `success` / `error`).
//! * [`capabilities`] — [`capabilities::build_capabilities_line`] and
//!   [`capabilities::Capability`], the advertised-capabilities display
//!   (`tools` / `resources` / `prompts`).
//! * [`tool_list`] — [`tool_list::build_tool_list_options`], the
//!   tool-list projection from a slice of [`tool_list::ToolInfo`] into
//!   selectable option rows.
//! * [`tool_detail`] — [`tool_detail::build_tool_detail`], the
//!   tool-detail projection showing name/description/schema/annotations.
//! * [`parsing_warnings`] — [`parsing_warnings::format_parsing_warnings`],
//!   turning a parsed MCP config with per-server errors into a
//!   formatted warning list.
//! * [`list_panel`] — [`list_panel::ListPanelState`], [`list_panel::ListPanelEvent`],
//!   [`list_panel::filter_servers`] and [`list_panel::sort_servers`],
//!   the server-list panel filter + sort + scroll reducer.
//! * [`settings`] — [`settings::build_mcp_settings_view`], the MCP
//!   settings tab view model.
//! * [`stdio_menu`] / [`agent_menu`] / [`remote_menu`] — the three
//!   server-menu reducers. Each is a small FSM with the same shape
//!   (Editing → Validating → Saving → Saved/Error).
//! * [`elicitation`] — [`elicitation::ElicitationSchema`],
//!   [`elicitation::ElicitationField`], [`elicitation::ElicitationResponse`]
//!   the [`elicitation::ElicitationFieldKind`] variants, the
//!   [`elicitation::validate_field_value`] per-field validation rule
//!   table, and [`elicitation::build_elicitation_response`].
//!
//! ## Outbound seams (each modeled as a small trait or pre-built input)
//!
//! * **Transport** — the actual stdio / sse / http / ws connection. These
//!   modules never touch a subprocess or socket.
//! * **Config storage** — persisting MCP server configs. The consumer
//!   owns fs IO.
//! * **Tools** — [`tool_list::filter_tools_by_server`] operates on a
//!   consumer-supplied slice of [`tool_list::ToolInfo`].
//! * **[`elicitation::ElicitationSchemaParser`]** — the JSON-Schema parser
//!   for elicitation requests. A parsed schema is an
//!   [`elicitation::ElicitationSchema`] taken as a value parameter; the
//!   consumer runs its own parser upstream.
//! * **Key handling** — these modules never dispatch key events
//!   directly; each reducer exposes an event enum the consumer drives.
//! * **Rendering** — NOT a trait. These modules produce plain `String`s
//!   and `Vec<…>` shapes; the consumer renders.
//!
//! ## What has been deliberately deferred
//!
//! * **The actual MCP transport** (stdio subprocess, SSE event stream,
//!   HTTP streaming, WebSocket handshake). Left to the transport modules.
//! * **Schema validation for server configs.** This crate models a
//!   parsed config as a typed struct and takes it as input; the
//!   consumer runs its own parser upstream.
//! * **The JSON-Schema parser for elicitation requests.** This crate
//!   models a parsed schema as [`elicitation::ElicitationSchema`].
//! * **The settings-file read/write** (`~/.rebon/settings.json`,
//!   project `.rebon/settings.json`). Left to the consumer.
//! * **OAuth flow for remote servers** (heavy OAuth redirect logic).
//!   This crate models the *state* of the flow (Idle → AwaitingCallback
//!   → Exchanging → Done/Error) but the actual http server + browser
//!   open is the consumer's problem.
//! * **Rendering primitives** (boxes, text, panes, tabs, selects,
//!   dialogs). This crate produces plain `String`s and structs.
//! * **Key dispatch, timer scheduling, and state ownership.**
//!   Each reducer exposes an event enum; the consumer wires keys.
//!
//! Each deferred concern has a clear landing path. None of them force
//! the types this crate exports into a specific shape.

pub mod agent_menu;
pub mod capabilities;
pub mod channel;
pub mod config;
pub mod elicitation;
pub mod list_panel;
pub mod parsing_warnings;
pub mod reconnect;
pub mod remote_menu;
pub mod settings;
pub mod status;
pub mod stdio_menu;
pub mod tool_detail;
pub mod tool_list;

pub use agent_menu::{AgentMenuEvent, AgentMenuState, AgentMenuStep};
pub use capabilities::{build_capabilities_line, Capability, CAPABILITIES_EMPTY_LABEL};
pub use channel::{
    effective_channel_allowlist, escape_xml_attr, find_channel_entry, gate_channel_server,
    is_safe_meta_key, parse_channel_entries, parse_channel_entry, parse_permission_reply,
    short_request_id, truncate_for_preview, wrap_channel_message, ChannelAllowlistEntry,
    ChannelCapabilities, ChannelEntry, ChannelEntryParseError, ChannelGateContext,
    ChannelGateResult, ChannelMessage, ChannelPermissionCallbacks, ChannelPermissionRequestParams,
    ChannelPermissionResponse, ChannelSkipKind, EffectiveAllowlistSource, ParsedPermissionReply,
    PermissionBehavior, SubscriptionType, CHANNEL_CAPABILITY, CHANNEL_NOTIFICATION_METHOD,
    CHANNEL_PERMISSION_CAPABILITY, CHANNEL_PERMISSION_METHOD, CHANNEL_PERMISSION_REQUEST_METHOD,
    CHANNEL_TAG, ID_ALPHABET, MAX_PREVIEW_CHARS,
};
pub use config::{
    ConfigScope, McpClaudeAIProxyServerConfig, McpHttpServerConfig, McpSdkServerConfig,
    McpSseIdeServerConfig, McpSseServerConfig, McpStdioServerConfig, McpWsIdeServerConfig,
    McpWsServerConfig, ScopedMcpServerConfig, ServerConfigKind, TransportKind,
};
pub use elicitation::{
    build_elicitation_response, validate_field_value, ElicitationField, ElicitationFieldKind,
    ElicitationResponse, ElicitationResponseKind, ElicitationSchema, ElicitationValidationError,
    FieldValue,
};
pub use list_panel::{
    filter_servers, sort_servers, ListPanelEvent, ListPanelState, ServerListItem, ServerSortKey,
};
pub use parsing_warnings::{
    format_parsing_warning_row, format_parsing_warnings, ParsingWarning, ParsingWarningKind,
};
pub use reconnect::{
    handle_reconnect_error, handle_reconnect_result, ReconnectClientKind, ReconnectEvent,
    ReconnectResult, ReconnectState,
};
pub use remote_menu::{
    RemoteAuthKind, RemoteMenuEvent, RemoteMenuState, RemoteMenuStep, RemoteTransportKind,
};
pub use settings::{build_mcp_settings_view, McpSettingsRow, McpSettingsRowKind, McpSettingsView};
pub use status::{
    transition_status, McpServerSnapshot, McpServerStatus, McpStatusEvent, StatusTransitionError,
};
pub use stdio_menu::{StdioMenuEvent, StdioMenuState, StdioMenuStep};
pub use tool_detail::{build_tool_detail, ToolAnnotation, ToolDetail};
pub use tool_list::{build_tool_list_options, filter_tools_by_server, ToolAnnotationKind, ToolRow};
