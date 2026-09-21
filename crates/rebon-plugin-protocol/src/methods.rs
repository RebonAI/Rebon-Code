//! The methods that carry a schema, and the payloads they carry.
//!
//! Most of this protocol's methods stay opaque on purpose — what `tool/call` or
//! `llm/stream` means belongs to those contracts, not to the wire layer. Four
//! do not have that luxury, because the wire layer has to make decisions about
//! them: which plugin is loaded, what it was allowed to register, whether a call
//! may still be routed to it, and which subscription an event belongs to.
//!
//! So these payloads are typed, strict (`deny_unknown_fields`), and validated
//! beyond what serde can express — and everything else keeps travelling as
//! [`crate::Payload`].
//!
//! Nothing here performs the work the methods name. Loading a module, invoking a
//! service, and delivering an event are the host's and the supervisor's jobs.
//! This module fixes the vocabulary; [`crate::PluginRegistry`] fixes the rules.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Payload, PLATFORM_PLUGIN_ID};

/// Load one plugin. Answered with a [`PluginReadyReport`] terminal.
pub const PLUGIN_LOAD_METHOD: &str = "plugin/load";
/// Unload one plugin. Answered with a [`PluginDrainReport`] terminal.
pub const PLUGIN_UNLOAD_METHOD: &str = "plugin/unload";
/// Call a service a plugin declared and registered.
pub const SERVICE_CALL_METHOD: &str = "service/call";
/// Deliver one event to a live subscription.
pub const EVENT_DELIVER_METHOD: &str = "event/deliver";
/// Plugin → host: register a subscription on a topic.
pub const EVENT_SUBSCRIBE_METHOD: &str = "event/subscribe";
/// Plugin → host: revoke a subscription this plugin registered.
pub const EVENT_UNSUBSCRIBE_METHOD: &str = "event/unsubscribe";
/// Plugin → host: publish one event onto rebon's event plane.
///
/// The other half of the event plane from [`EVENT_DELIVER_METHOD`], and gated
/// the way every other providing direction is: the manifest's `publishedTopics`
/// is the ceiling. Keeping it apart from `eventTopics` is the same distinction
/// `tools` and `invokableTools` draw — listening to a topic and publishing one
/// are different powers, and a plugin that does one rarely does the other.
pub const EVENT_EMIT_METHOD: &str = "event/emit";
/// Plugin → host: run one of rebon's own tools.
pub const TOOL_INVOKE_METHOD: &str = "tool/invoke";
/// Stream one model turn through an adapter a plugin registered.
pub const LLM_STREAM_METHOD: &str = "llm/stream";
/// Run one slash command a plugin registered.
///
/// The command half of [`TOOL_CALL_METHOD`], and the same direction: rebon is
/// the caller, the plugin owns the implementation. A command differs from a
/// tool in who typed it — a person, not a model — which is why the answer is
/// text for that person (or for the turn they are starting) rather than a
/// tool result.
pub const COMMAND_INVOKE_METHOD: &str = "command/invoke";

/// Tell an adapter something about the conversation around its turns.
///
/// Three signals that are not part of any one turn and therefore cannot ride
/// [`LLM_STREAM_METHOD`]: the session was reset, a turn ended, the server-side
/// response id a stateful provider was carrying is no longer valid. A provider
/// that keeps no state ignores all three, which is why the signal is a name
/// rather than a method per signal — the vocabulary belongs to the model
/// contract and adding to it must not add to the wire.
pub const LLM_CONTROL_METHOD: &str = "llm/control";
/// Plugin → host: call one of rebon's kernel seats.
///
/// A request, so it has a return value — which means it cannot serve a caller
/// that must not wait: a synchronous teardown path has nothing it can await, so
/// anything a teardown needs must be a declaration the protocol already unwinds
/// (unload drains and revokes) rather than a call made on the way out.
pub const SEAT_CALL_METHOD: &str = "seat/call";

/// Run a tool a plugin registered.
///
/// Carries the same payload as [`TOOL_INVOKE_METHOD`] — "run this named tool
/// with this input" is one message shape — travelling the other way. The
/// direction is the method, and the checks each direction runs are different:
/// one asks what the plugin was allowed to call, the other what it registered.
pub const TOOL_CALL_METHOD: &str = "tool/call";

/// Longest accepted plugin, service, topic, or subscription name.
///
/// These names appear in log lines, ledger keys, and error messages that reach a
/// user. A bound keeps a hostile or careless manifest from turning any of those
/// into an unreadable wall, and the limit is generous next to any real name.
pub const MAX_NAME_BYTES: usize = 128;

/// Most declarations a single `plugin/load` may carry.
///
/// A plugin declaring thousands of services is either generated wrongly or
/// trying to exhaust the registry; either way the honest answer is a refusal
/// rather than a slow one.
pub const MAX_DECLARATIONS: usize = 256;

/// Longest accepted tool description.
///
/// A tool description is read by a model, which means it is spent out of every
/// turn's context budget for as long as the tool is offered. An unbounded one
/// is therefore not merely untidy — it is a cost every request pays, so the
/// bound is part of the contract rather than a defensive habit.
pub const MAX_DESCRIPTION_BYTES: usize = 4096;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PayloadError {
    #[error("{kind} name must not be empty")]
    EmptyName { kind: &'static str },
    #[error("{kind} name is longer than {MAX_NAME_BYTES} bytes: {name:?}")]
    NameTooLong { kind: &'static str, name: String },
    #[error("{kind} name contains a control character: {name:?}")]
    ControlCharacter { kind: &'static str, name: String },
    #[error("{kind} declares {count} entries, over the {MAX_DECLARATIONS} limit")]
    TooManyDeclarations { kind: &'static str, count: usize },
    #[error("{kind} declares {name:?} twice")]
    DuplicateDeclaration { kind: &'static str, name: String },
    #[error("tool {tool:?} has no description, so a model cannot know when to use it")]
    EmptyDescription { tool: String },
    #[error(
        "tool {tool:?} has a {bytes}-byte description, over the {MAX_DESCRIPTION_BYTES} limit"
    )]
    DescriptionTooLong { tool: String, bytes: usize },
    #[error("plugin id {PLATFORM_PLUGIN_ID:?} is reserved for platform control traffic")]
    ReservedPluginId,
    #[error("{kind} path is longer than {MAX_PATH_BYTES} bytes: {path:?}")]
    PathTooLong { kind: &'static str, path: String },
    #[error("{kind} path must be absolute: {path:?}")]
    PathNotAbsolute { kind: &'static str, path: String },
    #[error("{kind} path must be relative to the package root: {path:?}")]
    PathNotRelative { kind: &'static str, path: String },
    #[error("{kind} path leaves the package root: {path:?}")]
    PathEscapes { kind: &'static str, path: String },
    #[error("adapter info describes provider {provider:?}, which this report does not serve")]
    UndeclaredAdapter { provider: String },
    #[error("llm control signal {signal:?} is not one of reset, endTurn, invalidate")]
    UnknownLlmSignal { signal: String },
}

impl PayloadError {
    /// A stable token shared with every other implementation of this protocol:
    /// the same malformed payload has to earn the same code on both sides, so a
    /// caller can switch on the code instead of on prose.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyName { .. } => "[EMPTY_NAME]",
            Self::NameTooLong { .. } => "[NAME_TOO_LONG]",
            Self::ControlCharacter { .. } => "[CONTROL_CHARACTER]",
            Self::TooManyDeclarations { .. } => "[TOO_MANY_DECLARATIONS]",
            Self::DuplicateDeclaration { .. } => "[DUPLICATE_DECLARATION]",
            Self::EmptyDescription { .. } => "[EMPTY_DESCRIPTION]",
            Self::DescriptionTooLong { .. } => "[DESCRIPTION_TOO_LONG]",
            Self::ReservedPluginId => "[RESERVED_PLUGIN_ID]",
            Self::PathTooLong { .. } => "[PATH_TOO_LONG]",
            Self::PathNotAbsolute { .. } => "[PATH_NOT_ABSOLUTE]",
            Self::PathNotRelative { .. } => "[PATH_NOT_RELATIVE]",
            Self::PathEscapes { .. } => "[PATH_ESCAPES]",
            Self::UndeclaredAdapter { .. } => "[UNDECLARED_ADAPTER]",
            Self::UnknownLlmSignal { .. } => "[UNKNOWN_LLM_SIGNAL]",
        }
    }
}

/// Rejects a name that could not be logged, keyed, or shown to a user safely.
pub fn validate_name(kind: &'static str, name: &str) -> Result<(), PayloadError> {
    if name.is_empty() {
        return Err(PayloadError::EmptyName { kind });
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(PayloadError::NameTooLong {
            kind,
            name: name.to_owned(),
        });
    }
    if name.chars().any(char::is_control) {
        return Err(PayloadError::ControlCharacter {
            kind,
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// Longest accepted path.
pub const MAX_PATH_BYTES: usize = 4096;

/// Whether a path is absolute, decided the same way on every platform.
///
/// `Path::is_absolute` answers for the platform it was compiled for, which is
/// correct for using a path and wrong for *agreeing* about one: two peers that
/// exchange this payload have to reach the same verdict on whichever machine
/// they run. Both spellings are accepted because those peers are always on the
/// same machine, and it is that machine's convention that matters.
fn looks_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes
        .first()
        .is_some_and(|byte| *byte == b'/' || *byte == b'\\')
    {
        return true;
    }
    // `C:/…` or `C:\…`
    matches!(bytes, [drive, b':', separator, ..]
        if drive.is_ascii_alphabetic() && (*separator == b'/' || *separator == b'\\'))
}

fn validate_path_shape(kind: &'static str, path: &str) -> Result<(), PayloadError> {
    if path.is_empty() {
        return Err(PayloadError::EmptyName { kind });
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(PayloadError::PathTooLong {
            kind,
            path: path.to_owned(),
        });
    }
    if path.chars().any(char::is_control) {
        return Err(PayloadError::ControlCharacter {
            kind,
            name: path.to_owned(),
        });
    }
    Ok(())
}

/// An absolute directory, such as an installed package's root.
pub fn validate_absolute_path(kind: &'static str, path: &str) -> Result<(), PayloadError> {
    validate_path_shape(kind, path)?;
    if !looks_absolute(path) {
        return Err(PayloadError::PathNotAbsolute {
            kind,
            path: path.to_owned(),
        });
    }
    Ok(())
}

/// A path relative to a package root, which must stay inside it.
///
/// The shape rules an archive reader has to apply, for the same reasons: `..`
/// climbs out, and a backslash is a separator on Windows only, so a path
/// containing one would mean two different things on two machines.
pub fn validate_relative_path(kind: &'static str, path: &str) -> Result<(), PayloadError> {
    validate_path_shape(kind, path)?;
    if looks_absolute(path) {
        return Err(PayloadError::PathNotRelative {
            kind,
            path: path.to_owned(),
        });
    }
    if path.contains('\\') || path.split('/').any(|part| part == ".." || part.is_empty()) {
        return Err(PayloadError::PathEscapes {
            kind,
            path: path.to_owned(),
        });
    }
    Ok(())
}

/// A plugin id, which additionally may not impersonate platform control.
pub fn validate_plugin_id(plugin_id: &str) -> Result<(), PayloadError> {
    validate_name("plugin", plugin_id)?;
    if plugin_id == PLATFORM_PLUGIN_ID {
        return Err(PayloadError::ReservedPluginId);
    }
    Ok(())
}

fn validate_declarations(kind: &'static str, names: &[String]) -> Result<(), PayloadError> {
    if names.len() > MAX_DECLARATIONS {
        return Err(PayloadError::TooManyDeclarations {
            kind,
            count: names.len(),
        });
    }
    let mut seen = std::collections::BTreeSet::new();
    for name in names {
        validate_name(kind, name)?;
        if !seen.insert(name.as_str()) {
            return Err(PayloadError::DuplicateDeclaration {
                kind,
                name: name.clone(),
            });
        }
    }
    Ok(())
}

/// `plugin/load` — what the host admits, taken from the plugin's manifest.
///
/// The declarations are the contract: a plugin may register a service or a topic
/// only if it said so here, which is what makes the manifest reviewable before
/// any of the plugin's code runs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PluginLoadRequest {
    pub plugin_id: String,
    /// Absolute directory of the installed package. The host resolves `entry`
    /// against it and refuses anything that lands outside.
    pub root: String,
    /// Package-relative entry module.
    pub entry: String,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub event_topics: Vec<String>,
    /// Topics this plugin may publish onto rebon's event plane.
    ///
    /// Distinct from [`Self::event_topics`], which is what it listens to.
    /// Emitting is a providing direction — something downstream acts on — so it
    /// is declared where a person reads the manifest, not discovered when the
    /// first event arrives.
    #[serde(default)]
    pub published_topics: Vec<String>,
    /// Model providers this plugin may serve an adapter for.
    ///
    /// Declared like a service and registered like one: the manifest is the
    /// ceiling, and a plugin claiming a provider name it never declared is
    /// refused. Which provider a stream is for is the routing key, so the wire
    /// layer has to know these names even though what a turn *contains* stays
    /// the model contract's business.
    #[serde(default)]
    pub llm_providers: Vec<String>,
    /// Tools this plugin provides.
    ///
    /// Only the names are declared here. A tool's description and input schema
    /// come with the registration, because they are how the tool describes
    /// itself to a model rather than what it is permitted to be — the name is
    /// the part a person reviews before installing.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Slash commands this plugin may register, by name.
    ///
    /// Declared like a tool and for the same reason: the name is the part a
    /// person reviews, and it is also the part that can collide with a
    /// built-in. What the command *says about itself* comes with the
    /// registration.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Kernel seats this plugin may call.
    ///
    /// The consuming direction, like [`Self::invokable_tools`]: what a plugin
    /// may reach for, declared where a person can read it before installing.
    #[serde(default)]
    pub seats: Vec<String>,
    /// Core rebon tools this plugin may invoke.
    ///
    /// The consuming direction, and the only list here that is not something
    /// the plugin provides — which is why it is the one with a qualifier in its
    /// name. It has no ready-report counterpart: a plugin calls these, it does
    /// not register them. Declared for the same reason the others are: the
    /// manifest is what a person reads before deciding to install, and "this
    /// plugin can read your files" has to be visible there rather than
    /// discovered at runtime.
    #[serde(default)]
    pub invokable_tools: Vec<String>,
    /// The plugin's own configuration, opaque to everything here.
    ///
    /// A plugin package describes what it *can* do; this is how the installation
    /// says what it *should* do — a model list, a base URL, a set of grants. It
    /// has to travel with the load rather than sit in the package, because the
    /// same package is loaded with different configuration by different
    /// installations, and (for a composition entry) the package is not writable
    /// by the thing configuring it.
    #[serde(default)]
    pub config: Payload,
}

impl PluginLoadRequest {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_plugin_id(&self.plugin_id)?;
        validate_absolute_path("root", &self.root)?;
        validate_relative_path("entry", &self.entry)?;
        validate_declarations("service", &self.services)?;
        validate_declarations("topic", &self.event_topics)?;
        validate_declarations("topic", &self.published_topics)?;
        validate_declarations("provider", &self.llm_providers)?;
        validate_declarations("tool", &self.tools)?;
        validate_declarations("command", &self.commands)?;
        validate_declarations("tool", &self.invokable_tools)?;
        validate_declarations("seat", &self.seats)
    }
}

/// Terminal payload of `plugin/load`: what the plugin actually registered.
///
/// Separate from the request because "what was declared" and "what was
/// registered" are different facts, and the gap between them is exactly what the
/// registry has to police.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PluginReadyReport {
    pub plugin_id: String,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub event_topics: Vec<String>,
    #[serde(default)]
    pub llm_providers: Vec<String>,
    /// What each adapter says about itself, keyed by the provider it serves.
    ///
    /// Opaque, and deliberately so. The wire layer decides exactly one thing
    /// about an adapter — may this plugin serve this provider — and
    /// [`Self::llm_providers`] answers it. Everything else a caller needs to
    /// know before routing a turn (which models, which default, what the
    /// provider can do) belongs to the model contract, the same way a turn's
    /// contents do.
    ///
    /// It rides the ready report rather than a call because it is a fact about
    /// the adapter, not about a turn: a caller has to know whether a provider
    /// streams reasoning text *before* it decides how to budget the turn it is
    /// about to send. A provider that says nothing leaves whatever its package
    /// manifest declared standing.
    #[serde(default)]
    pub llm_adapters: BTreeMap<String, Payload>,
    /// The tools this plugin registered, with what they tell a model.
    ///
    /// Definitions rather than names, because unlike a service — which is
    /// called by something that already knows it exists — a tool has to be
    /// *offered* to a model, and it cannot be offered without saying what it
    /// does and what it takes.
    #[serde(default)]
    pub tools: Vec<PluginToolDefinition>,
    /// The slash commands this plugin registered, with what they tell a
    /// person. Definitions for the same reason a tool's are: a command has to
    /// appear in a menu, and it cannot appear without saying what it does.
    #[serde(default)]
    pub commands: Vec<PluginCommandDefinition>,
}

impl PluginReadyReport {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_plugin_id(&self.plugin_id)?;
        validate_declarations("service", &self.services)?;
        validate_declarations("topic", &self.event_topics)?;
        validate_declarations("provider", &self.llm_providers)?;
        // An adapter's own account of itself only means anything next to the
        // route it belongs to. Describing a provider the report does not claim
        // to serve is a report that disagrees with itself, and the honest
        // answer is to refuse it here rather than to carry a description
        // nothing will ever route to.
        for provider in self.llm_adapters.keys() {
            if !self.llm_providers.iter().any(|name| name == provider) {
                return Err(PayloadError::UndeclaredAdapter {
                    provider: provider.clone(),
                });
            }
        }
        let names: Vec<String> = self.tools.iter().map(|tool| tool.name.clone()).collect();
        validate_declarations("tool", &names)?;
        for tool in &self.tools {
            tool.validate()?;
        }
        let commands: Vec<String> = self
            .commands
            .iter()
            .map(|command| command.name.clone())
            .collect();
        validate_declarations("command", &commands)?;
        for command in &self.commands {
            command.validate()?;
        }
        Ok(())
    }
}

/// How a plugin's tool describes itself.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PluginToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input. Opaque here: what a schema means
    /// belongs to JSON Schema, not to this layer.
    pub input_schema: Payload,
}

impl PluginToolDefinition {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_name("tool", &self.name)?;
        if self.description.is_empty() {
            return Err(PayloadError::EmptyDescription {
                tool: self.name.clone(),
            });
        }
        if self.description.len() > MAX_DESCRIPTION_BYTES {
            return Err(PayloadError::DescriptionTooLong {
                tool: self.name.clone(),
                bytes: self.description.len(),
            });
        }
        Ok(())
    }
}

/// How a plugin's slash command describes itself.
///
/// Deliberately no wider than what a command needs in order to appear in a menu
/// and be dispatched: a plugin registers a command the same way a built-in
/// does, so the wire shape is those fields and nothing else. The one field that
/// is *narrower* is the kind — a plugin may not claim a front-end-native
/// command (there is no front-end function to map an id to) or a session-owned
/// one (that is rebon's own engine state), so the kinds it may claim carry what
/// each needs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PluginCommandDefinition {
    /// Canonical name, without the leading slash.
    pub name: String,
    /// Other spellings that resolve to it.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Chinese words that *find* it in a `/` menu. Not spellings: they reach
    /// the command, they are not the command.
    #[serde(default)]
    pub zh_aliases: Vec<String>,
    /// Argument hint shown in a menu (`[on|off]`, `<prompt>`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// One line, in the imperative. Shown in menus and `/help`.
    pub description: String,
    /// `command` or `agent`; how a menu groups it.
    #[serde(default)]
    pub category: PluginCommandCategory,
    /// Where it works. Empty means every local front end, the same default a
    /// built-in takes.
    #[serde(default)]
    pub surfaces: Vec<PluginCommandSurface>,
    pub kind: PluginCommandKind,
}

/// The two groupings a menu knows.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginCommandCategory {
    #[default]
    Command,
    Agent,
}

/// A front end a command works on.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginCommandSurface {
    Tui,
    Desktop,
    Acp,
    Web,
    Mobile,
}

/// What running a plugin's command means.
///
/// `Prompt` is answered by [`COMMAND_INVOKE_METHOD`] — the plugin is asked,
/// and what it returns becomes the turn. The other two are answered *here*,
/// at registration, because neither has anything to compute: an explanation
/// is a sentence, and a panel is an id the front end already knows how to
/// open. Asking the plugin for either at invoke time would be a round trip
/// whose answer could not have changed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginCommandKind {
    /// Expands into a model turn. The text comes from `command/invoke`.
    Prompt,
    /// Only says something on this surface.
    Explain { text: String },
    /// Opens a panel by its dialog id.
    Panel { dialog: String },
}

impl PluginCommandDefinition {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_name("command", &self.name)?;
        for alias in &self.aliases {
            validate_name("command", alias)?;
        }
        for alias in &self.zh_aliases {
            validate_name("command", alias)?;
        }
        if self.description.is_empty() {
            return Err(PayloadError::EmptyDescription {
                tool: self.name.clone(),
            });
        }
        if self.description.len() > MAX_DESCRIPTION_BYTES {
            return Err(PayloadError::DescriptionTooLong {
                tool: self.name.clone(),
                bytes: self.description.len(),
            });
        }
        match &self.kind {
            PluginCommandKind::Prompt => Ok(()),
            PluginCommandKind::Explain { text } => {
                if text.is_empty() {
                    Err(PayloadError::EmptyDescription {
                        tool: self.name.clone(),
                    })
                } else {
                    Ok(())
                }
            }
            PluginCommandKind::Panel { dialog } => validate_name("dialog", dialog),
        }
    }
}

/// `command/invoke` — host → plugin, answered with one terminal.
///
/// `rest` rather than a parsed argument list: a command's arguments are its
/// own business, and a wire that parsed them would be inventing a grammar
/// every command would then have to fit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CommandInvokeRequest {
    pub name: String,
    /// The whole line as typed, trailing whitespace trimmed.
    pub raw: String,
    /// Everything after the command name, trimmed.
    pub rest: String,
    /// Where it was typed.
    pub surface: PluginCommandSurface,
}

impl CommandInvokeRequest {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_name("command", &self.name)
    }
}

/// `plugin/unload` — begin draining one plugin.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PluginUnloadRequest {
    pub plugin_id: String,
}

impl PluginUnloadRequest {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_plugin_id(&self.plugin_id)
    }
}

/// Terminal payload of `plugin/unload`: the drain ledger.
///
/// `outstanding_calls` is the whole point. Unloading is not "the plugin is
/// gone", it is "no new work will be routed there, and here is the work that
/// was already in flight". A drain that never empties has to be visible, not
/// rounded down to success.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PluginDrainReport {
    pub plugin_id: String,
    #[serde(default)]
    pub outstanding_calls: Vec<String>,
    #[serde(default)]
    pub revoked_subscriptions: Vec<String>,
}

impl PluginDrainReport {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_plugin_id(&self.plugin_id)?;
        validate_declarations("call", &self.outstanding_calls)?;
        validate_declarations("subscription", &self.revoked_subscriptions)
    }
}

/// `service/call` — invoke a service by name, with an opaque request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServiceCallRequest {
    pub service: String,
    /// The service's own contract. Opaque here by design.
    pub request: Payload,
}

impl ServiceCallRequest {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_name("service", &self.service)
    }
}

/// `event/subscribe` — plugin → host.
///
/// The subscription id comes from the plugin rather than being assigned by the
/// host, so that revocation and delivery name the same thing without a round
/// trip. The registry enforces uniqueness per plugin, so two plugins may use the
/// same id without colliding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EventSubscribeRequest {
    pub subscription: String,
    pub topic: String,
}

impl EventSubscribeRequest {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_name("subscription", &self.subscription)?;
        validate_name("topic", &self.topic)
    }
}

/// `event/unsubscribe` — plugin → host.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EventUnsubscribeRequest {
    pub subscription: String,
}

impl EventUnsubscribeRequest {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_name("subscription", &self.subscription)
    }
}

/// `event/emit` — plugin → host.
///
/// The event's contents stay opaque; the topic does not, because the topic is
/// what decides who is allowed to publish it and who hears it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EventEmitRequest {
    pub topic: String,
    pub event: Payload,
}

impl EventEmitRequest {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_name("topic", &self.topic)
    }
}

/// `tool/invoke` — plugin → host.
///
/// The input stays opaque: what a tool's arguments mean belongs to that tool's
/// contract, not to the wire layer. The tool *name* does not, because the wire
/// layer has to decide whether this plugin may run it at all.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ToolInvokeRequest {
    pub tool: String,
    pub input: Payload,
}

impl ToolInvokeRequest {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_name("tool", &self.tool)
    }
}

/// `llm/stream` — host → plugin, answered with chunks and then one terminal.
///
/// The request stays opaque: what a model turn contains belongs to the model
/// contract, not to the wire layer. The provider does not, because it is the
/// routing key — the wire layer has to decide which adapter this turn is for
/// and whether the plugin may serve it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LlmStreamRequest {
    pub provider: String,
    pub request: Payload,
}

impl LlmStreamRequest {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_name("provider", &self.provider)
    }
}

/// The conversation-level signals an adapter can be told about.
///
/// A closed set, unlike an `llm/stream` request: a provider has to be able to
/// answer "do I know this signal" without knowing what a turn contains, and an
/// open string would make every unknown signal indistinguishable from a typo.
pub const LLM_CONTROL_SIGNALS: &[&str] = &["reset", "endTurn", "invalidate"];

/// `llm/control` — host → plugin, answered with one terminal and no chunks.
///
/// Fire-and-forget from the caller's side, but a request rather than a
/// notification: a signal that vanished silently when a plugin was mid-unload
/// would leave the caller believing state had been cleared that was not.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LlmControlRequest {
    pub provider: String,
    pub signal: String,
}

impl LlmControlRequest {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_name("provider", &self.provider)?;
        if !LLM_CONTROL_SIGNALS.contains(&self.signal.as_str()) {
            return Err(PayloadError::UnknownLlmSignal {
                signal: self.signal.clone(),
            });
        }
        Ok(())
    }
}

/// `seat/call` — plugin → host.
///
/// `params` stays opaque: what a seat's method takes belongs to that seat's
/// contract. The seat and method names do not, because the wire layer decides
/// whether this plugin may reach that seat at all.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SeatCallRequest {
    pub seat: String,
    pub method: String,
    pub params: Payload,
}

impl SeatCallRequest {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_name("seat", &self.seat)?;
        validate_name("method", &self.method)
    }
}

/// `event/deliver` — host → plugin, on a live subscription.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EventDelivery {
    pub subscription: String,
    pub topic: String,
    pub event: Payload,
}

impl EventDelivery {
    pub fn validate(&self) -> Result<(), PayloadError> {
        validate_name("subscription", &self.subscription)?;
        validate_name("topic", &self.topic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn load(plugin: &str) -> PluginLoadRequest {
        PluginLoadRequest {
            plugin_id: plugin.into(),
            root: "/packages/demo".into(),
            entry: "index.mjs".into(),
            services: vec!["compose".into()],
            event_topics: vec!["session".into()],
            published_topics: vec!["compose:session/append".into()],
            llm_providers: vec!["demo".into()],
            tools: vec!["grep".into()],
            commands: vec!["demo".into()],
            invokable_tools: vec!["read_file".into()],
            seats: vec!["credentials".into()],
            config: Payload::null(),
        }
    }

    #[test]
    fn a_load_request_round_trips_as_camel_case() {
        let request = load("plugin.a");
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            encoded,
            json!({
                "pluginId": "plugin.a",
                "root": "/packages/demo",
                "entry": "index.mjs",
                "services": ["compose"],
                "eventTopics": ["session"],
                "publishedTopics": ["compose:session/append"],
                "llmProviders": ["demo"],
                "tools": ["grep"],
                "commands": ["demo"],
                "invokableTools": ["read_file"],
                "seats": ["credentials"],
                "config": null,
            })
        );
        assert_eq!(
            serde_json::from_value::<PluginLoadRequest>(encoded).unwrap(),
            request
        );
    }

    #[test]
    fn declarations_default_to_empty_but_unknown_fields_are_refused() {
        let minimal: PluginLoadRequest =
            serde_json::from_value(json!({"pluginId": "p", "root": "/pkg", "entry": "i.mjs"}))
                .unwrap();
        assert!(minimal.services.is_empty());
        assert!(minimal.event_topics.is_empty());
        assert!(minimal.llm_providers.is_empty());
        assert!(minimal.tools.is_empty());
        // A plugin that declares no tools can invoke none: the default is the
        // closed end of the range, not the open one.
        assert!(minimal.invokable_tools.is_empty());
        assert!(minimal.seats.is_empty());

        let error = serde_json::from_value::<PluginLoadRequest>(
            json!({"pluginId": "p", "root": "/pkg", "entry": "i.mjs", "extra": 1}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown field"), "{error}");
    }

    /// A manifest must not be able to address itself as platform control, which
    /// is the identity the supervisor reserves for its own traffic.
    #[test]
    fn the_platform_identity_cannot_be_loaded_as_a_plugin() {
        let mut request = load(PLATFORM_PLUGIN_ID);
        assert_eq!(request.validate(), Err(PayloadError::ReservedPluginId));
        request.plugin_id = "plugin.a".into();
        assert!(request.validate().is_ok());
    }

    #[test]
    fn names_are_bounded_nonempty_and_printable() {
        assert_eq!(
            validate_name("service", ""),
            Err(PayloadError::EmptyName { kind: "service" })
        );
        let long = "x".repeat(MAX_NAME_BYTES + 1);
        assert!(matches!(
            validate_name("service", &long),
            Err(PayloadError::NameTooLong { .. })
        ));
        assert!(validate_name("service", &"x".repeat(MAX_NAME_BYTES)).is_ok());
        assert!(matches!(
            validate_name("service", "a\nb"),
            Err(PayloadError::ControlCharacter { .. })
        ));
        assert!(matches!(
            validate_name("service", "a\u{0}b"),
            Err(PayloadError::ControlCharacter { .. })
        ));
        assert!(validate_name("service", "compose:run").is_ok());
    }

    #[test]
    fn declaration_lists_are_bounded_and_duplicate_free() {
        let mut request = load("plugin.a");
        request.services = vec!["a".into(), "a".into()];
        assert!(matches!(
            request.validate(),
            Err(PayloadError::DuplicateDeclaration { .. })
        ));

        request.services = (0..=MAX_DECLARATIONS).map(|n| n.to_string()).collect();
        assert!(matches!(
            request.validate(),
            Err(PayloadError::TooManyDeclarations { .. })
        ));

        request.services = (0..MAX_DECLARATIONS).map(|n| n.to_string()).collect();
        assert!(request.validate().is_ok());
    }

    #[test]
    fn a_root_must_be_absolute_and_an_entry_must_stay_inside_it() {
        // Both spellings count as absolute, because the pair of processes that
        // exchange this always run on the same machine.
        for root in [
            "/packages/demo",
            "C:/packages/demo",
            r"C:\packages\demo",
            r"\\host\share",
        ] {
            assert!(validate_absolute_path("root", root).is_ok(), "{root}");
        }
        for root in ["packages/demo", "./demo", ""] {
            assert!(validate_absolute_path("root", root).is_err(), "{root}");
        }

        assert!(validate_relative_path("entry", "src/index.mjs").is_ok());
        for entry in [
            "/index.mjs",
            "C:/index.mjs",
            "../index.mjs",
            "src/../../index.mjs",
            r"src\index.mjs",
            "src//index.mjs",
            "",
        ] {
            assert!(validate_relative_path("entry", entry).is_err(), "{entry}");
        }
    }

    #[test]
    fn every_payload_validates_its_own_names() {
        assert!(PluginUnloadRequest {
            plugin_id: PLATFORM_PLUGIN_ID.into()
        }
        .validate()
        .is_err());
        assert!(ServiceCallRequest {
            service: String::new(),
            request: Payload::null(),
        }
        .validate()
        .is_err());
        assert!(EventSubscribeRequest {
            subscription: "s".into(),
            topic: String::new(),
        }
        .validate()
        .is_err());
        assert!(EventUnsubscribeRequest {
            subscription: "\u{7}".into()
        }
        .validate()
        .is_err());
        assert!(EventDelivery {
            subscription: String::new(),
            topic: "t".into(),
            event: Payload::null(),
        }
        .validate()
        .is_err());
    }

    /// The opaque half of a service call keeps its exact text, like any payload.
    #[test]
    fn a_service_request_carries_its_payload_verbatim() {
        let wire = json!({"service": "compose", "request": {"n": 1}});
        let call: ServiceCallRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(call.request.as_raw(), r#"{"n":1}"#);
        assert_eq!(
            serde_json::to_string(&call).unwrap(),
            r#"{"service":"compose","request":{"n":1}}"#
        );
    }

    #[test]
    fn a_drain_report_round_trips_with_its_ledger() {
        let report = PluginDrainReport {
            plugin_id: "plugin.a".into(),
            outstanding_calls: vec!["call-1".into()],
            revoked_subscriptions: vec!["sub-1".into()],
        };
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(
            encoded,
            json!({
                "pluginId": "plugin.a",
                "outstandingCalls": ["call-1"],
                "revokedSubscriptions": ["sub-1"],
            })
        );
        assert_eq!(
            serde_json::from_value::<PluginDrainReport>(encoded).unwrap(),
            report
        );
    }
}
