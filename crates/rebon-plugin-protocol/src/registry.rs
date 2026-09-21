//! Who is loaded, what they were allowed to register, and whether a call may
//! still be routed to them.
//!
//! [`crate::CallLedger`] answers "may this message commit a terminal?". This
//! answers the question before that one: "should this message exist at all?".
//! The two are deliberately separate — one is about a call's lifetime, the other
//! about a plugin's — and a host needs both.
//!
//! # Declared, then registered
//!
//! A `plugin/load` carries what the manifest declared. The ready report carries
//! what the plugin's code actually registered. The registry refuses a report
//! that registers anything the manifest did not declare, so the reviewable
//! artifact — the manifest — is the upper bound on what the plugin can offer,
//! and no amount of plugin code widens it.
//!
//! # Unloading is draining, not deleting
//!
//! `plugin/unload` moves a plugin to [`PluginPhase::Draining`]: new calls are
//! refused with [`RegistryError::code`] `[STALE_PROVIDER]`, subscriptions are
//! revoked immediately, and the calls already in flight are *reported*, not
//! forgotten. Finishing an unload while any remain is an error rather than a
//! quiet success, because the alternative is a plugin whose replacement is
//! already handling traffic the old one has not finished.
//!
//! # Whoever closes the last call finishes the drain
//!
//! A drain ends in one of two places, and which one depends only on whether
//! anything was running when it began: [`PluginRegistry::finish_unload`] when
//! nothing was, and [`PluginRegistry::complete_call`] when something was — that
//! call's own exit is the moment the last of it stops running, and nothing else
//! observes it.
//!
//! Leaving the second one out is what strands a plugin. A drain that began with
//! work in flight would have no way to end, and `Draining` is a phase nothing
//! routes to, nothing can unload again, and nothing can load over — so the id
//! would be spent for the life of the process.
//!
//! This module holds no I/O and starts nothing. Supervision, restart, and the
//! actual loading of a module belong to the host.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::{
    methods::{
        CommandInvokeRequest, EventDelivery, EventEmitRequest, EventSubscribeRequest,
        EventUnsubscribeRequest, LlmControlRequest, LlmStreamRequest, PayloadError,
        PluginCommandDefinition, PluginCommandKind, PluginDrainReport, PluginLoadRequest,
        PluginReadyReport, PluginToolDefinition, PluginUnloadRequest, SeatCallRequest,
        ServiceCallRequest, ToolInvokeRequest,
    },
    CallIdentity, Payload,
};

/// Where a plugin is in the load → ready → drain → unloaded progression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginPhase {
    /// Admitted by the host; its code has not reported what it registered.
    Admitted,
    /// Running and routable.
    Ready,
    /// Unload has begun: no new work, in-flight work still accounted for.
    Draining,
    /// Drained. Kept as a tombstone so a late message reads as stale rather
    /// than as an unrecognised plugin, which are different diagnoses.
    Unloaded,
}

impl PluginPhase {
    fn routable(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// What closing a call did to the plugin around it.
///
/// Returned rather than kept private because a drain finishing is a lifecycle
/// event, not an implementation detail of bookkeeping: a caller tracking what
/// is loaded has to hear about it, and one that does not can ignore the value
/// without the behaviour changing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallClosed {
    /// The plugin is untouched — it is live, or draining with work left.
    PluginUnaffected,
    /// This was the last call of a drain, so the plugin is now unloaded.
    DrainFinished,
}

/// One live subscription, pinned to the scope incarnation that created it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subscription {
    pub topic: String,
    pub scope_id: String,
    pub scope_generation: u64,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RegistryError {
    #[error("{0}")]
    Payload(#[from] PayloadError),
    #[error("plugin {plugin_id:?} is already loaded")]
    AlreadyLoaded { plugin_id: String },
    #[error("plugin {plugin_id:?} is not loaded")]
    UnknownPlugin { plugin_id: String },
    #[error("plugin {plugin_id:?} is {phase:?}, not ready")]
    NotReady {
        plugin_id: String,
        phase: PluginPhase,
    },
    #[error("plugin {plugin_id:?} is {phase:?} and no longer accepts work")]
    StaleProvider {
        plugin_id: String,
        phase: PluginPhase,
    },
    #[error(
        "plugin {plugin_id:?} registered {kind} {name:?}, which its manifest does not declare"
    )]
    Undeclared {
        plugin_id: String,
        kind: &'static str,
        name: String,
    },
    #[error("plugin {plugin_id:?} does not provide service {service:?}")]
    UnknownService { plugin_id: String, service: String },
    #[error("plugin {plugin_id:?} registered no llm adapter for provider {provider:?}")]
    UnknownProvider { plugin_id: String, provider: String },
    #[error("plugin {plugin_id:?} does not provide tool {tool:?}")]
    UnknownTool { plugin_id: String, tool: String },
    #[error("plugin {plugin_id:?} does not provide command {command:?}")]
    UnknownCommand { plugin_id: String, command: String },
    #[error("plugin {plugin_id:?} did not register topic {topic:?}")]
    UnknownTopic { plugin_id: String, topic: String },
    #[error(
        "plugin {plugin_id:?} may not invoke tool {tool:?}, which its manifest does not declare"
    )]
    UnauthorizedTool { plugin_id: String, tool: String },
    #[error(
        "plugin {plugin_id:?} may not call seat {seat:?}, which its manifest does not declare"
    )]
    UnauthorizedSeat { plugin_id: String, seat: String },
    #[error(
        "plugin {plugin_id:?} may not publish topic {topic:?}, which its manifest does not declare"
    )]
    UnauthorizedTopic { plugin_id: String, topic: String },
    #[error("call {call_id:?} is already in flight")]
    DuplicateCall { call_id: String },
    #[error("call {call_id:?} is not in flight for plugin {plugin_id:?}")]
    UnknownCall { plugin_id: String, call_id: String },
    #[error("plugin {plugin_id:?} already holds subscription {subscription:?}")]
    DuplicateSubscription {
        plugin_id: String,
        subscription: String,
    },
    #[error("plugin {plugin_id:?} holds no subscription {subscription:?}")]
    UnknownSubscription {
        plugin_id: String,
        subscription: String,
    },
    #[error(
        "subscription {subscription:?} belongs to {scope_id:?} generation {recorded}, not {actual}"
    )]
    StaleSubscription {
        subscription: String,
        scope_id: String,
        recorded: u64,
        actual: u64,
    },
    #[error("subscription {subscription:?} is on topic {recorded:?}, not {actual:?}")]
    TopicMismatch {
        subscription: String,
        recorded: String,
        actual: String,
    },
    #[error("plugin {plugin_id:?} still has {count} calls in flight")]
    DrainIncomplete { plugin_id: String, count: usize },
    #[error("identity names plugin {actual:?} but the payload names {expected:?}")]
    IdentityMismatch { expected: String, actual: String },
}

/// The refusal for an id the answering side has never heard of.
///
/// Named rather than inlined because more than one layer can answer with it and
/// a caller has to be able to tell them apart: this registry says it about a
/// plugin that was never loaded, while a caller that mounted the plugin itself
/// says it about an entry it never mounted — the ordinary state of a host-native
/// plugin, not a failure. Matching the code is what keeps that distinction out
/// of prose.
pub const UNKNOWN_PLUGIN_CODE: &str = "[UNKNOWN_PLUGIN]";

impl RegistryError {
    /// A stable bracketed token for logs and wire errors. Callers should switch
    /// on this rather than on the prose of the message.
    pub fn code(&self) -> &'static str {
        match self {
            // One code namespace across both layers: a malformed payload keeps
            // the specific reason it was malformed for.
            Self::Payload(error) => error.code(),
            Self::AlreadyLoaded { .. } => "[PLUGIN_ALREADY_LOADED]",
            Self::UnknownPlugin { .. } => UNKNOWN_PLUGIN_CODE,
            Self::NotReady { .. } => "[PLUGIN_NOT_READY]",
            Self::StaleProvider { .. } => "[STALE_PROVIDER]",
            Self::Undeclared { .. } => "[UNAUTHORIZED_REGISTER]",
            Self::UnknownService { .. } => "[UNKNOWN_SERVICE]",
            Self::UnknownProvider { .. } => "[UNKNOWN_PROVIDER]",
            Self::UnknownTool { .. } => "[UNKNOWN_TOOL]",
            Self::UnknownCommand { .. } => "[UNKNOWN_COMMAND]",
            Self::UnknownTopic { .. } => "[UNKNOWN_TOPIC]",
            Self::UnauthorizedTool { .. } => "[UNAUTHORIZED_TOOL]",
            Self::UnauthorizedSeat { .. } => "[UNAUTHORIZED_SEAT]",
            Self::UnauthorizedTopic { .. } => "[UNAUTHORIZED_TOPIC]",
            Self::DuplicateCall { .. } => "[DUPLICATE_CALL]",
            Self::UnknownCall { .. } => "[UNKNOWN_CALL]",
            Self::DuplicateSubscription { .. } => "[DUPLICATE_SUBSCRIPTION]",
            Self::UnknownSubscription { .. } => "[UNKNOWN_SUBSCRIPTION]",
            Self::StaleSubscription { .. } => "[STALE_SUBSCRIPTION]",
            Self::TopicMismatch { .. } => "[TOPIC_MISMATCH]",
            Self::DrainIncomplete { .. } => "[DRAIN_INCOMPLETE]",
            Self::IdentityMismatch { .. } => "[IDENTITY_MISMATCH]",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct PluginState {
    phase: Option<PluginPhase>,
    declared_services: BTreeSet<String>,
    declared_topics: BTreeSet<String>,
    declared_llm_providers: BTreeSet<String>,
    declared_tools: BTreeSet<String>,
    declared_commands: BTreeSet<String>,
    /// Tools the manifest permits this plugin to *invoke*. The only declared
    /// set with no registered counterpart: a plugin calls these, it does not
    /// provide them.
    declared_invokable_tools: BTreeSet<String>,
    /// Kernel seats the manifest permits this plugin to call. Consuming, like
    /// the invokable tools, so it too has no registered counterpart.
    declared_seats: BTreeSet<String>,
    /// Topics the manifest permits this plugin to publish. Providing, but with
    /// no registration step — an emit is the registration and the use at once.
    declared_published_topics: BTreeSet<String>,
    services: BTreeSet<String>,
    topics: BTreeSet<String>,
    llm_providers: BTreeSet<String>,
    /// What each adapter said about itself, by provider. Opaque here; the
    /// model layer is what reads it.
    llm_adapters: BTreeMap<String, Payload>,
    /// Tools this plugin registered, by name, with what they tell a model.
    tools: BTreeMap<String, PluginToolDefinition>,
    /// Slash commands it registered, by name, with what they tell a person.
    commands: BTreeMap<String, PluginCommandDefinition>,
    subscriptions: BTreeMap<String, Subscription>,
    in_flight: BTreeSet<String>,
}

impl PluginState {
    fn phase(&self) -> PluginPhase {
        self.phase.unwrap_or(PluginPhase::Admitted)
    }
}

/// The set of plugins one host epoch knows about.
///
/// Not keyed by host epoch: a registry belongs to one epoch by construction, and
/// a restarted host builds a new one. Messages from an older epoch are the
/// [`crate::CallLedger`]'s business.
#[derive(Clone, Debug, Default)]
pub struct PluginRegistry {
    plugins: BTreeMap<String, PluginState>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&self, plugin_id: &str) -> Option<PluginPhase> {
        self.plugins.get(plugin_id).map(PluginState::phase)
    }

    pub fn services(&self, plugin_id: &str) -> Option<&BTreeSet<String>> {
        self.plugins.get(plugin_id).map(|state| &state.services)
    }

    pub fn subscription(&self, plugin_id: &str, subscription: &str) -> Option<&Subscription> {
        self.plugins
            .get(plugin_id)
            .and_then(|state| state.subscriptions.get(subscription))
    }

    /// Every live subscription one plugin holds, as `(subscription, topic)`.
    pub fn subscriptions(&self, plugin_id: &str) -> Vec<(String, String)> {
        self.plugins
            .get(plugin_id)
            .map(|state| {
                state
                    .subscriptions
                    .iter()
                    .map(|(id, subscription)| (id.clone(), subscription.topic.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn in_flight(&self, plugin_id: &str) -> Vec<String> {
        self.plugins
            .get(plugin_id)
            .map(|state| state.in_flight.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Stage 3, admission: the manifest is accepted and its declarations become
    /// the ceiling on what this plugin may register.
    ///
    /// A previously unloaded plugin may be admitted again — that is what a
    /// reload is — and doing so starts from the new manifest's declarations, not
    /// the old ones.
    pub fn admit_load(&mut self, request: &PluginLoadRequest) -> Result<(), RegistryError> {
        request.validate()?;
        if let Some(state) = self.plugins.get(&request.plugin_id) {
            if state.phase() != PluginPhase::Unloaded {
                return Err(RegistryError::AlreadyLoaded {
                    plugin_id: request.plugin_id.clone(),
                });
            }
        }
        self.plugins.insert(
            request.plugin_id.clone(),
            PluginState {
                phase: Some(PluginPhase::Admitted),
                declared_services: request.services.iter().cloned().collect(),
                declared_topics: request.event_topics.iter().cloned().collect(),
                declared_llm_providers: request.llm_providers.iter().cloned().collect(),
                declared_commands: request.commands.iter().cloned().collect(),
                declared_tools: request.tools.iter().cloned().collect(),
                declared_invokable_tools: request.invokable_tools.iter().cloned().collect(),
                declared_seats: request.seats.iter().cloned().collect(),
                declared_published_topics: request.published_topics.iter().cloned().collect(),
                ..PluginState::default()
            },
        );
        Ok(())
    }

    /// Stage 4 failure: a plugin whose load raised is dropped entirely, so the
    /// next attempt starts clean rather than inheriting a half-built entry.
    pub fn reject_load(&mut self, plugin_id: &str) -> Result<(), RegistryError> {
        match self.plugins.get(plugin_id).map(PluginState::phase) {
            Some(PluginPhase::Admitted) => {
                self.plugins.remove(plugin_id);
                Ok(())
            }
            Some(phase) => Err(RegistryError::NotReady {
                plugin_id: plugin_id.to_owned(),
                phase,
            }),
            None => Err(RegistryError::UnknownPlugin {
                plugin_id: plugin_id.to_owned(),
            }),
        }
    }

    /// Stage 5, ready: record what the plugin registered, refusing anything its
    /// manifest did not declare.
    pub fn accept_ready(&mut self, report: &PluginReadyReport) -> Result<(), RegistryError> {
        report.validate()?;
        let state = self.plugins.get_mut(&report.plugin_id).ok_or_else(|| {
            RegistryError::UnknownPlugin {
                plugin_id: report.plugin_id.clone(),
            }
        })?;
        if state.phase() != PluginPhase::Admitted {
            return Err(RegistryError::NotReady {
                plugin_id: report.plugin_id.clone(),
                phase: state.phase(),
            });
        }
        let registered_tools: Vec<String> =
            report.tools.iter().map(|tool| tool.name.clone()).collect();
        let registered_commands: Vec<String> = report
            .commands
            .iter()
            .map(|command| command.name.clone())
            .collect();
        for (kind, declared, registered) in [
            ("service", &state.declared_services, &report.services),
            ("topic", &state.declared_topics, &report.event_topics),
            (
                "provider",
                &state.declared_llm_providers,
                &report.llm_providers,
            ),
            ("tool", &state.declared_tools, &registered_tools),
            ("command", &state.declared_commands, &registered_commands),
        ] {
            if let Some(name) = registered.iter().find(|name| !declared.contains(*name)) {
                return Err(RegistryError::Undeclared {
                    plugin_id: report.plugin_id.clone(),
                    kind,
                    name: name.clone(),
                });
            }
        }
        state.services = report.services.iter().cloned().collect();
        state.topics = report.event_topics.iter().cloned().collect();
        state.llm_providers = report.llm_providers.iter().cloned().collect();
        // Already checked against `llm_providers` by the report's own
        // validation, and `llm_providers` was just checked against what the
        // manifest declared — so an adapter description can only ever be about
        // a route this plugin is permitted to serve.
        state.llm_adapters = report.llm_adapters.clone();
        state.tools = report
            .tools
            .iter()
            .map(|tool| (tool.name.clone(), tool.clone()))
            .collect();
        state.commands = report
            .commands
            .iter()
            .map(|command| (command.name.clone(), command.clone()))
            .collect();
        state.phase = Some(PluginPhase::Ready);
        Ok(())
    }

    /// Stage 6, routing: admit one `service/call` and start accounting for it.
    pub fn admit_service_call(
        &mut self,
        identity: &CallIdentity,
        request: &ServiceCallRequest,
    ) -> Result<(), RegistryError> {
        request.validate()?;
        let plugin_id = identity.plugin_id.clone();
        let state = Self::routable_mut(&mut self.plugins, &plugin_id)?;
        if !state.services.contains(&request.service) {
            return Err(RegistryError::UnknownService {
                plugin_id,
                service: request.service.clone(),
            });
        }
        if !state.in_flight.insert(identity.call_id.clone()) {
            return Err(RegistryError::DuplicateCall {
                call_id: identity.call_id.clone(),
            });
        }
        Ok(())
    }

    /// Stage 6, routing: admit one `llm/stream` and start accounting for it.
    ///
    /// A streamed turn is work in flight exactly like a service call — a plugin
    /// torn down mid-stream is the same hazard — so it uses the same ledger and
    /// is closed by the same [`Self::complete_call`].
    pub fn admit_llm_stream(
        &mut self,
        identity: &CallIdentity,
        request: &LlmStreamRequest,
    ) -> Result<(), RegistryError> {
        request.validate()?;
        let plugin_id = identity.plugin_id.clone();
        let state = Self::routable_mut(&mut self.plugins, &plugin_id)?;
        if !state.llm_providers.contains(&request.provider) {
            return Err(RegistryError::UnknownProvider {
                plugin_id,
                provider: request.provider.clone(),
            });
        }
        if !state.in_flight.insert(identity.call_id.clone()) {
            return Err(RegistryError::DuplicateCall {
                call_id: identity.call_id.clone(),
            });
        }
        Ok(())
    }

    /// Stage 6, routing: admit one `llm/control` and start accounting for it.
    ///
    /// Gated by the same set a stream is: telling an adapter to forget its
    /// session is as much a use of that route as sending it a turn.
    pub fn admit_llm_control(
        &mut self,
        identity: &CallIdentity,
        request: &LlmControlRequest,
    ) -> Result<(), RegistryError> {
        request.validate()?;
        let plugin_id = identity.plugin_id.clone();
        let state = Self::routable_mut(&mut self.plugins, &plugin_id)?;
        if !state.llm_providers.contains(&request.provider) {
            return Err(RegistryError::UnknownProvider {
                plugin_id,
                provider: request.provider.clone(),
            });
        }
        if !state.in_flight.insert(identity.call_id.clone()) {
            return Err(RegistryError::DuplicateCall {
                call_id: identity.call_id.clone(),
            });
        }
        Ok(())
    }

    /// Stage 6, routing: admit one `command/invoke` and start accounting.
    ///
    /// A command a plugin never registered is refused the way an unknown tool
    /// is: a front end holding a stale menu must get a named refusal rather
    /// than a call that arrives at nothing.
    pub fn admit_command_invoke(
        &mut self,
        identity: &CallIdentity,
        request: &CommandInvokeRequest,
    ) -> Result<(), RegistryError> {
        request.validate()?;
        let plugin_id = identity.plugin_id.clone();
        let state = Self::routable_mut(&mut self.plugins, &plugin_id)?;
        // A `prompt` command is the only one there is anything to ask about:
        // an `explain` said its sentence and a `panel` named its dialog at
        // registration. Refusing the other two here rather than letting the
        // host find no handler keeps the two sides of the protocol agreeing
        // about what an invoke can even be for.
        match state.commands.get(&request.name) {
            Some(command) if matches!(command.kind, PluginCommandKind::Prompt) => {}
            _ => {
                return Err(RegistryError::UnknownCommand {
                    plugin_id,
                    command: request.name.clone(),
                })
            }
        }
        if !state.in_flight.insert(identity.call_id.clone()) {
            return Err(RegistryError::DuplicateCall {
                call_id: identity.call_id.clone(),
            });
        }
        Ok(())
    }

    /// The slash commands one plugin registered, as they describe themselves.
    pub fn commands(&self, plugin_id: &str) -> Vec<PluginCommandDefinition> {
        self.plugins
            .get(plugin_id)
            .map(|state| state.commands.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Model providers this plugin actually registered an adapter for.
    pub fn llm_providers(&self, plugin_id: &str) -> Option<&BTreeSet<String>> {
        self.plugins
            .get(plugin_id)
            .map(|state| &state.llm_providers)
    }

    /// What each adapter this plugin registered said about itself.
    pub fn llm_adapters(&self, plugin_id: &str) -> BTreeMap<String, Payload> {
        self.plugins
            .get(plugin_id)
            .map(|state| state.llm_adapters.clone())
            .unwrap_or_default()
    }

    /// Stops accounting for a call, whatever its terminal was, and finishes the
    /// drain if this was the last one.
    ///
    /// Deliberately allowed while draining: that is the only way a drain ever
    /// completes. Unconditionally so — an unload reply still travelling is not
    /// a reason to leave the last call's exit unacted on, because that reply
    /// carries the *host's* count, which may have reached zero earlier and
    /// retired the plugin over there already.
    pub fn complete_call(
        &mut self,
        plugin_id: &str,
        call_id: &str,
    ) -> Result<CallClosed, RegistryError> {
        let state =
            self.plugins
                .get_mut(plugin_id)
                .ok_or_else(|| RegistryError::UnknownPlugin {
                    plugin_id: plugin_id.to_owned(),
                })?;
        if !state.in_flight.remove(call_id) {
            return Err(RegistryError::UnknownCall {
                plugin_id: plugin_id.to_owned(),
                call_id: call_id.to_owned(),
            });
        }
        if state.phase() != PluginPhase::Draining || !state.in_flight.is_empty() {
            return Ok(CallClosed::PluginUnaffected);
        }
        *state = PluginState {
            phase: Some(PluginPhase::Unloaded),
            ..PluginState::default()
        };
        Ok(CallClosed::DrainFinished)
    }

    /// Registers a subscription, pinned to the scope incarnation that made it.
    pub fn subscribe(
        &mut self,
        identity: &CallIdentity,
        request: &EventSubscribeRequest,
    ) -> Result<(), RegistryError> {
        request.validate()?;
        let plugin_id = identity.plugin_id.clone();
        let state = Self::routable_mut(&mut self.plugins, &plugin_id)?;
        if !state.topics.contains(&request.topic) {
            return Err(RegistryError::UnknownTopic {
                plugin_id,
                topic: request.topic.clone(),
            });
        }
        if state.subscriptions.contains_key(&request.subscription) {
            return Err(RegistryError::DuplicateSubscription {
                plugin_id,
                subscription: request.subscription.clone(),
            });
        }
        state.subscriptions.insert(
            request.subscription.clone(),
            Subscription {
                topic: request.topic.clone(),
                scope_id: identity.scope_id.clone(),
                scope_generation: identity.scope_generation,
            },
        );
        Ok(())
    }

    pub fn unsubscribe(
        &mut self,
        plugin_id: &str,
        request: &EventUnsubscribeRequest,
    ) -> Result<(), RegistryError> {
        request.validate()?;
        let state =
            self.plugins
                .get_mut(plugin_id)
                .ok_or_else(|| RegistryError::UnknownPlugin {
                    plugin_id: plugin_id.to_owned(),
                })?;
        state
            .subscriptions
            .remove(&request.subscription)
            .map(|_| ())
            .ok_or_else(|| RegistryError::UnknownSubscription {
                plugin_id: plugin_id.to_owned(),
                subscription: request.subscription.clone(),
            })
    }

    /// Tools one plugin's manifest permits it to invoke.
    pub fn declared_invokable_tools(&self, plugin_id: &str) -> Option<&BTreeSet<String>> {
        self.plugins
            .get(plugin_id)
            .map(|state| &state.declared_invokable_tools)
    }

    /// The tools one plugin registered, as they describe themselves.
    ///
    /// This is what a caller offers to a model, which is why it hands back the
    /// definitions rather than the names.
    pub fn tools(&self, plugin_id: &str) -> Vec<PluginToolDefinition> {
        self.plugins
            .get(plugin_id)
            .map(|state| state.tools.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Stage 6, routing: admit one `tool/call` and start accounting for it.
    ///
    /// The mirror of [`Self::admit_service_call`], and the same accounting: a
    /// running tool is work a drain has to wait for.
    pub fn admit_tool_call(
        &mut self,
        identity: &CallIdentity,
        request: &ToolInvokeRequest,
    ) -> Result<(), RegistryError> {
        request.validate()?;
        let plugin_id = identity.plugin_id.clone();
        let state = Self::routable_mut(&mut self.plugins, &plugin_id)?;
        if !state.tools.contains_key(&request.tool) {
            return Err(RegistryError::UnknownTool {
                plugin_id,
                tool: request.tool.clone(),
            });
        }
        if !state.in_flight.insert(identity.call_id.clone()) {
            return Err(RegistryError::DuplicateCall {
                call_id: identity.call_id.clone(),
            });
        }
        Ok(())
    }

    /// Checks that this plugin may invoke this tool right now.
    ///
    /// The manifest is the ceiling here exactly as it is for registration, and
    /// for the same reason: it is the artifact a person read before installing.
    /// Whether the *platform* exposes the tool at all, and whether the user
    /// permits this particular use of it, are separate questions asked after
    /// this one — this check only says the plugin never claimed the right.
    pub fn admit_tool_invoke(
        &self,
        identity: &CallIdentity,
        request: &ToolInvokeRequest,
    ) -> Result<(), RegistryError> {
        request.validate()?;
        let state =
            self.plugins
                .get(&identity.plugin_id)
                .ok_or_else(|| RegistryError::UnknownPlugin {
                    plugin_id: identity.plugin_id.clone(),
                })?;
        if !state.phase().routable() {
            return Err(Self::not_routable(&identity.plugin_id, state.phase()));
        }
        if !state.declared_invokable_tools.contains(&request.tool) {
            return Err(RegistryError::UnauthorizedTool {
                plugin_id: identity.plugin_id.clone(),
                tool: request.tool.clone(),
            });
        }
        Ok(())
    }

    /// Kernel seats one plugin's manifest permits it to call.
    pub fn declared_seats(&self, plugin_id: &str) -> Option<&BTreeSet<String>> {
        self.plugins
            .get(plugin_id)
            .map(|state| &state.declared_seats)
    }

    /// Checks that this plugin may call this seat right now.
    ///
    /// The same shape as [`Self::admit_tool_invoke`], and for the same reason:
    /// the manifest is the artifact a person read before installing, so it is
    /// the ceiling on what the plugin can reach for.
    pub fn admit_seat_call(
        &self,
        identity: &CallIdentity,
        request: &SeatCallRequest,
    ) -> Result<(), RegistryError> {
        request.validate()?;
        let state =
            self.plugins
                .get(&identity.plugin_id)
                .ok_or_else(|| RegistryError::UnknownPlugin {
                    plugin_id: identity.plugin_id.clone(),
                })?;
        if !state.phase().routable() {
            return Err(Self::not_routable(&identity.plugin_id, state.phase()));
        }
        if !state.declared_seats.contains(&request.seat) {
            return Err(RegistryError::UnauthorizedSeat {
                plugin_id: identity.plugin_id.clone(),
                seat: request.seat.clone(),
            });
        }
        Ok(())
    }

    /// Checks that this plugin may publish this topic right now.
    ///
    /// The mirror of [`Self::admit_event_delivery`]: that one asks whether a
    /// plugin still holds the subscription an event is arriving on, this one
    /// whether its manifest let it publish at all. No subscription is involved
    /// — an emit names a topic, and who hears it is rebon's business.
    pub fn admit_event_emit(
        &self,
        identity: &CallIdentity,
        request: &EventEmitRequest,
    ) -> Result<(), RegistryError> {
        request.validate()?;
        let state =
            self.plugins
                .get(&identity.plugin_id)
                .ok_or_else(|| RegistryError::UnknownPlugin {
                    plugin_id: identity.plugin_id.clone(),
                })?;
        if !state.phase().routable() {
            return Err(Self::not_routable(&identity.plugin_id, state.phase()));
        }
        if !state.declared_published_topics.contains(&request.topic) {
            return Err(RegistryError::UnauthorizedTopic {
                plugin_id: identity.plugin_id.clone(),
                topic: request.topic.clone(),
            });
        }
        Ok(())
    }

    /// Checks that an event may be delivered on this subscription right now.
    ///
    /// The scope generation must match exactly. A delivery carrying an older
    /// generation is aimed at a scope incarnation that no longer exists, and
    /// delivering it would backfill a closed scope.
    pub fn admit_event_delivery(
        &self,
        identity: &CallIdentity,
        delivery: &EventDelivery,
    ) -> Result<(), RegistryError> {
        delivery.validate()?;
        let state =
            self.plugins
                .get(&identity.plugin_id)
                .ok_or_else(|| RegistryError::UnknownPlugin {
                    plugin_id: identity.plugin_id.clone(),
                })?;
        if !state.phase().routable() {
            return Err(Self::not_routable(&identity.plugin_id, state.phase()));
        }
        let subscription = state
            .subscriptions
            .get(&delivery.subscription)
            .ok_or_else(|| RegistryError::UnknownSubscription {
                plugin_id: identity.plugin_id.clone(),
                subscription: delivery.subscription.clone(),
            })?;
        if subscription.topic != delivery.topic {
            return Err(RegistryError::TopicMismatch {
                subscription: delivery.subscription.clone(),
                recorded: subscription.topic.clone(),
                actual: delivery.topic.clone(),
            });
        }
        if subscription.scope_id != identity.scope_id
            || subscription.scope_generation != identity.scope_generation
        {
            return Err(RegistryError::StaleSubscription {
                subscription: delivery.subscription.clone(),
                scope_id: subscription.scope_id.clone(),
                recorded: subscription.scope_generation,
                actual: identity.scope_generation,
            });
        }
        Ok(())
    }

    /// Drops every subscription one plugin made under an older incarnation of
    /// one of its scopes.
    ///
    /// Called when a generation advances — the invalidation linearization point.
    /// Returns what was revoked so the host can tell the plugin, and so the loss
    /// is visible rather than inferred from later delivery failures.
    ///
    /// Scoped to the one plugin on purpose. A generation counts incarnations of
    /// *this plugin's* scope; two plugins working on the same session hold two
    /// independent counters. Sweeping every plugin holding that scope id would
    /// compare one plugin's clock against another's and silently cancel a
    /// subscription whose own scope is still open — the exact silent breakage
    /// this returned list exists to prevent.
    pub fn revoke_stale_subscriptions(
        &mut self,
        plugin_id: &str,
        scope_id: &str,
        current_generation: u64,
    ) -> Vec<(String, String)> {
        let mut revoked = Vec::new();
        let Some(state) = self.plugins.get_mut(plugin_id) else {
            return revoked;
        };
        let stale: Vec<String> = state
            .subscriptions
            .iter()
            .filter(|(_, subscription)| {
                subscription.scope_id == scope_id
                    && subscription.scope_generation < current_generation
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            state.subscriptions.remove(&id);
            revoked.push((plugin_id.to_owned(), id));
        }
        revoked
    }

    /// Stage 7, unload: stop routing, revoke subscriptions, report what is still
    /// in flight.
    ///
    /// Only a [`PluginPhase::Ready`] plugin unloads, matching the rule on the
    /// far side of the wire: a plugin whose load has not finished is not
    /// something an unload can address, so an unload naming one is refused
    /// rather than obeyed. Accepting `Admitted` here would move this side to
    /// `Draining` for an unload that side is about to refuse, and the ready
    /// report that follows would find a phase it cannot accept — leaving the
    /// plugin running on the far side with nothing able to route to it, unload
    /// it, or load its id again.
    pub fn begin_unload(
        &mut self,
        request: &PluginUnloadRequest,
    ) -> Result<PluginDrainReport, RegistryError> {
        request.validate()?;
        let state = self.plugins.get_mut(&request.plugin_id).ok_or_else(|| {
            RegistryError::UnknownPlugin {
                plugin_id: request.plugin_id.clone(),
            }
        })?;
        match state.phase() {
            PluginPhase::Ready => {}
            // Still loading. Its own load is what finishes or rejects it, and
            // that decision is not this one's to take.
            PluginPhase::Admitted => {
                return Err(RegistryError::NotReady {
                    plugin_id: request.plugin_id.clone(),
                    phase: PluginPhase::Admitted,
                })
            }
            phase => {
                return Err(RegistryError::StaleProvider {
                    plugin_id: request.plugin_id.clone(),
                    phase,
                })
            }
        }
        state.phase = Some(PluginPhase::Draining);
        let revoked_subscriptions = std::mem::take(&mut state.subscriptions)
            .into_keys()
            .collect();
        Ok(PluginDrainReport {
            plugin_id: request.plugin_id.clone(),
            outstanding_calls: state.in_flight.iter().cloned().collect(),
            revoked_subscriptions,
        })
    }

    /// Completes an unload. Refuses while work is still in flight, because a
    /// silent success here is how a replacement plugin starts taking traffic the
    /// old one has not finished answering.
    pub fn finish_unload(&mut self, plugin_id: &str) -> Result<(), RegistryError> {
        let state =
            self.plugins
                .get_mut(plugin_id)
                .ok_or_else(|| RegistryError::UnknownPlugin {
                    plugin_id: plugin_id.to_owned(),
                })?;
        if state.phase() != PluginPhase::Draining {
            return Err(RegistryError::NotReady {
                plugin_id: plugin_id.to_owned(),
                phase: state.phase(),
            });
        }
        if !state.in_flight.is_empty() {
            return Err(RegistryError::DrainIncomplete {
                plugin_id: plugin_id.to_owned(),
                count: state.in_flight.len(),
            });
        }
        *state = PluginState {
            phase: Some(PluginPhase::Unloaded),
            ..PluginState::default()
        };
        Ok(())
    }

    fn routable_mut<'a>(
        plugins: &'a mut BTreeMap<String, PluginState>,
        plugin_id: &str,
    ) -> Result<&'a mut PluginState, RegistryError> {
        let phase = plugins
            .get(plugin_id)
            .map(PluginState::phase)
            .ok_or_else(|| RegistryError::UnknownPlugin {
                plugin_id: plugin_id.to_owned(),
            })?;
        if !phase.routable() {
            return Err(Self::not_routable(plugin_id, phase));
        }
        Ok(plugins.get_mut(plugin_id).expect("checked above"))
    }

    /// A plugin that was routable and no longer is, versus one that never got
    /// there, are different diagnoses and get different codes.
    fn not_routable(plugin_id: &str, phase: PluginPhase) -> RegistryError {
        match phase {
            PluginPhase::Draining | PluginPhase::Unloaded => RegistryError::StaleProvider {
                plugin_id: plugin_id.to_owned(),
                phase,
            },
            phase => RegistryError::NotReady {
                plugin_id: plugin_id.to_owned(),
                phase,
            },
        }
    }
}
