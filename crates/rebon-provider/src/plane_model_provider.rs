//! An installed package's model provider, served over the plugin plane.
//!
//! A package's declared `modelProviders` stop being a protocol of their
//! own and become `llm/stream` adapters on the one Node host every other
//! plugin already runs in, so there is one transport vocabulary for model
//! providers and every other plugin.
//!
//! # What changed and what did not
//!
//! The **vocabulary did not**. A turn still travels as
//! [`CreateMessageRequestV1`] and comes back as [`StreamEventV1`]s, and the
//! ~460 lines that translate between those and rebon's own types are untouched
//! in `rebon-api`. That was the whole reason to keep them: they are not
//! transport, they are the model contract, and three of their decisions
//! (refusing `context_management`, folding a cache trace into
//! `extensions.promptCache`, dropping backend-bound compaction blocks) are
//! policy that would have had to be rewritten had the payload changed.
//!
//! The **transport did**. A bespoke JSON-RPC client — spawn, NDJSON reader,
//! stderr drain, request table, four timeouts, fatal-failure propagation,
//! roughly 480 lines of it — is gone, because the plugin supervisor already
//! does every one of those things for every plugin. What is left here is the
//! part that was never transport: turning a turn into a call, a chunk into an
//! event, and a conversation-level signal into `llm/control`.
//!
//! # The three semantics that had to come with it
//!
//! 1. **Capabilities.** The old `initialize` answered with eleven booleans
//!    that decide how a turn is budgeted and projected. They arrive in the
//!    ready report now ([`ModelProviderAdapterInfoV1`]), and are still
//!    OR-merged over what the package manifest declared — a manifest may
//!    under-claim and the adapter corrects it, never the reverse.
//! 2. **Cancel on walking away.** A reader that drops before the terminal used
//!    to make the client send `modelProvider/cancel`. [`ServiceStream`]'s own
//!    `Drop` only closes accounting, so the cancel is sent here.
//! 3. **The conversation signals.** `reset`, `endTurn` and `invalidate` are
//!    not part of any turn, so they go on `llm/control` — fire-and-forget from
//!    the caller's side, exactly as before.
//!
//! # And one that did not
//!
//! The old client put a timeout on every request. The supervisor deliberately
//! bounds only the handshake (its own note: a long call's answer is
//! `call/cancel`, not a timer the caller cannot see), and a model turn is the
//! longest call rebon makes. Turn deadlines belong to the caller that knows
//! what it is waiting for.

use std::sync::Arc;

use async_trait::async_trait;
use rebon_api::model_provider_protocol::{
    CreateMessageRequestV1, ModelProviderAdapterInfoV1, ModelProviderTurnV1,
    ProviderConnectionConfigV1, StreamEventV1, MODEL_PROVIDER_PROTOCOL_VERSION,
};
use rebon_api::{CreateMessageRequest, ModelClient, ModelError, ModelResult, StreamEventStream};
use rebon_plugin_protocol::Payload;
use rebon_plugin_supervisor::{PluginHostSupervisor, ServiceStream, StreamEvent};
use tokio::sync::mpsc;

use crate::model_provider_plugin::{
    ModelProviderCapabilityManifest, ModelProviderCapabilityManifestExt,
    PluginModelProviderContribution,
};

/// Everything the client needs to reach one adapter.
#[derive(Clone)]
pub struct PlaneModelProviderConfig {
    pub provider_id: String,
    pub source: String,
    /// The plane plugin serving it. Node-host loads a package's provider under
    /// its provider id, so the two are the same string — named separately
    /// because they are different facts and one of them may move.
    pub plugin_id: String,
    /// The plane scope this client's calls travel on — its own, not the
    /// plane's.
    ///
    /// This is what keeps two sessions from sharing one adapter's state. A
    /// provider used to be a child process per resolution, so isolation came
    /// free and `provider_runtime_cacheable` was false to preserve it: an
    /// `endTurn` from one agent must not clear another agent's turn. On a
    /// shared host the adapter is loaded once no matter how many clients bind
    /// to it, so the isolation has to be somewhere else, and the plugin
    /// protocol already has the right somewhere — a scope. Each client opens
    /// one, every turn and every signal travels on it, and an adapter that
    /// keeps state keys it by `ctx.scopeId`.
    pub scope_id: String,
    /// The workspace this client's scope names.
    pub workspace_root: String,
    pub supervisor: Arc<PluginHostSupervisor>,
    /// What the package manifest declared, before the adapter's own report is
    /// merged in.
    pub declared: ModelProviderCapabilityManifest,
    /// What the user configured for this provider entry.
    pub connection: Option<ProviderConnectionConfigV1>,
}

/// A [`ModelClient`] served by an adapter on the plugin plane.
pub struct PlaneModelProviderClient {
    config: PlaneModelProviderConfig,
    capabilities: ModelProviderCapabilityManifest,
    /// The runtime the plane runs on, so a synchronous signal from
    /// [`ModelClient`] has somewhere to go.
    runtime: tokio::runtime::Handle,
}

impl Drop for PlaneModelProviderClient {
    /// Closes the scope this client opened.
    ///
    /// Not merely tidy: a scope left open holds the adapter's state for a
    /// session that has ended, and a plugin unloading has to drain every
    /// scope. Spawned, because closing is a request and `Drop` cannot await
    /// one.
    fn drop(&mut self) {
        let supervisor = Arc::clone(&self.config.supervisor);
        let plugin_id = self.config.plugin_id.clone();
        let scope_id = self.config.scope_id.clone();
        self.runtime.spawn(async move {
            let _ = supervisor.close_scope(&plugin_id, &scope_id).await;
        });
    }
}

impl std::fmt::Debug for PlaneModelProviderClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaneModelProviderClient")
            .field("provider", &self.config.provider_id)
            .field("plugin", &self.config.plugin_id)
            .finish_non_exhaustive()
    }
}

impl PlaneModelProviderClient {
    /// Binds to an adapter that is already loaded.
    ///
    /// No handshake, because there is nothing left to hand shake about: the
    /// plugin was loaded and reported itself before this is reached, and the
    /// connection travels with each turn. What this does is read that report
    /// and merge it over the manifest.
    pub async fn bind(config: PlaneModelProviderConfig) -> anyhow::Result<Self> {
        config
            .supervisor
            .open_scope(&config.plugin_id, &config.scope_id, &config.workspace_root)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "model provider `{}` would not open a scope: {error}",
                    config.provider_id
                )
            })?;
        let reported = config
            .supervisor
            .llm_adapters(&config.plugin_id)
            .await
            .remove(&config.provider_id);
        let info = match reported {
            Some(payload) => adapter_info(&config.provider_id, payload)?,
            // An adapter that registered without describing itself is not an
            // error: the manifest already said what it can do, and saying
            // nothing is how a provider agrees with it.
            None => ModelProviderAdapterInfoV1::default(),
        };
        if let Some(version) = info.protocol_version {
            if version != MODEL_PROVIDER_PROTOCOL_VERSION {
                anyhow::bail!(
                    "model provider `{}` speaks protocol version {version}, and this build \
                     speaks {MODEL_PROVIDER_PROTOCOL_VERSION}",
                    config.provider_id
                );
            }
        }
        let capabilities = config
            .declared
            .clone()
            .with_runtime_capabilities(&info.capabilities);
        Ok(Self {
            config,
            capabilities,
            runtime: tokio::runtime::Handle::current(),
        })
    }

    /// Builds the client's configuration from a package contribution.
    pub fn config_for(
        contribution: &PluginModelProviderContribution,
        plugin_id: String,
        workspace_root: String,
        supervisor: Arc<PluginHostSupervisor>,
        connection: Option<ProviderConnectionConfigV1>,
    ) -> PlaneModelProviderConfig {
        PlaneModelProviderConfig {
            provider_id: contribution.id.clone(),
            source: contribution.source.clone(),
            scope_id: next_scope_id(&contribution.id),
            plugin_id,
            workspace_root,
            supervisor,
            declared: contribution.capabilities.clone(),
            connection,
        }
    }

    /// Sends one conversation-level signal and does not wait for it.
    ///
    /// Fire-and-forget is the old protocol's contract, kept: these are called
    /// from synchronous [`ModelClient`] methods that have no answer to return
    /// and no way to await one. A failure is logged rather than surfaced,
    /// because the alternative — a panic or a swallowed `Result` in a `fn`
    /// that returns `()` — tells nobody anything either.
    fn signal(&self, signal: &'static str) {
        let supervisor = Arc::clone(&self.config.supervisor);
        let plugin_id = self.config.plugin_id.clone();
        let scope_id = self.config.scope_id.clone();
        let provider = self.config.provider_id.clone();
        self.runtime.spawn(async move {
            if let Err(error) = supervisor
                .control_llm(&plugin_id, &scope_id, &provider, signal)
                .await
            {
                tracing::debug!(%provider, %signal, %error, "llm control signal was not delivered");
            }
        });
    }
}

/// A scope name no other client of this process will pick.
///
/// A counter rather than a uuid: the name only has to be unique among the
/// clients one process opens against one host, and a readable one is worth
/// more in a log line than an unguessable one.
fn next_scope_id(provider: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        "provider:{provider}#{}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn adapter_info(provider: &str, payload: Payload) -> anyhow::Result<ModelProviderAdapterInfoV1> {
    let value = payload
        .to_value()
        .map_err(|error| anyhow::anyhow!("adapter report for `{provider}` is not JSON: {error}"))?;
    serde_json::from_value(value).map_err(|error| {
        anyhow::anyhow!("adapter report for `{provider}` is not a v1 adapter info: {error}")
    })
}

#[async_trait]
impl ModelClient for PlaneModelProviderClient {
    fn provider_name(&self) -> &'static str {
        "external-plugin"
    }

    async fn create_message_stream(
        &self,
        request: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        let turn = ModelProviderTurnV1 {
            protocol_version: MODEL_PROVIDER_PROTOCOL_VERSION,
            connection: self.config.connection.clone(),
            request: CreateMessageRequestV1::from_request(&request)
                .map_err(|error| ModelError::other(error.to_string()))?,
        };
        tracing::trace!(
            provider = %self.config.provider_id,
            source = %self.config.source,
            "streaming a turn through a plane model provider"
        );
        let payload = Payload::from(
            serde_json::to_value(&turn).map_err(|error| ModelError::other(error.to_string()))?,
        );
        let stream = self
            .config
            .supervisor
            .stream_llm(
                &self.config.plugin_id,
                &self.config.scope_id,
                &self.config.provider_id,
                payload,
            )
            .await
            .map_err(|error| {
                ModelError::other(format!(
                    "model provider `{}` could not start a turn: {error}",
                    self.config.provider_id
                ))
            })?;
        let call_id = stream.call_id().to_owned();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(pump(self.config.provider_id.clone(), stream, tx));
        Ok(Box::pin(PlaneTurn {
            events: rx,
            supervisor: Arc::clone(&self.config.supervisor),
            call_id,
            terminal_seen: false,
        }))
    }

    fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
        None
    }

    fn supports_request_scoped_transient_context(&self) -> bool {
        self.capabilities.request_scoped_transient_context
    }

    fn supports_forced_tool_choice(&self) -> bool {
        self.capabilities.forced_tool_choice
    }

    fn supports_anchored_minimal(&self) -> bool {
        self.capabilities.anchored_minimal
    }

    /// `reasoningText` doubles as the budget signal: a provider that streams
    /// reasoning text speaks an OpenAI Responses-style dialect whose
    /// `max_output_tokens` covers the reasoning too (DeepSeek documents this
    /// explicitly).
    fn output_budget_includes_reasoning(&self) -> bool {
        self.capabilities.reasoning_text
    }

    /// The plugin protocol has no signature field on thinking blocks, so a
    /// plugin can never produce one; requiring it would make a truncated turn
    /// permanently uncontinuable.
    fn thinking_replay_requires_signature(&self) -> bool {
        false
    }

    fn reset_session_state(&self) {
        self.signal("reset");
    }

    fn end_turn(&self) {
        self.signal("endTurn");
    }

    fn invalidate_previous_response_id(&self) {
        self.signal("invalidate");
    }
}

/// One turn in flight: plane chunks in, rebon events out.
///
/// A pump task rather than a `Stream` that awaits `recv` inline. Building the
/// receive future inside `poll_next` would build a *new* one on every poll,
/// which drops the waker the previous one registered — the turn then stops
/// dead after its first `Pending` and nothing ever wakes it. Polling a channel
/// the pump feeds has no such edge, and it is the shape the plane's other
/// stream translator already uses.
struct PlaneTurn {
    events: mpsc::UnboundedReceiver<ModelResult<rebon_api::StreamEvent>>,
    supervisor: Arc<PluginHostSupervisor>,
    call_id: String,
    terminal_seen: bool,
}

/// Feeds one turn's events into `sink` until the turn ends or nobody is
/// listening.
async fn pump(
    provider: String,
    mut stream: ServiceStream,
    sink: mpsc::UnboundedSender<ModelResult<rebon_api::StreamEvent>>,
) {
    while let Some(event) = stream.recv().await {
        let item = match event {
            StreamEvent::Chunk(payload) => decode_event(&provider, payload),
            StreamEvent::End(Ok(_)) => break,
            StreamEvent::End(Err(error)) => Err(ModelError::other(format!(
                "model provider `{provider}` ended the turn: {error}"
            ))),
        };
        let failed = item.is_err();
        // A closed sink means the reader walked away; its `Drop` has already
        // sent the cancel, so there is nothing to do but stop reading.
        if sink.send(item).is_err() || failed {
            break;
        }
    }
}

fn decode_event(provider: &str, payload: Payload) -> ModelResult<rebon_api::StreamEvent> {
    let value = payload.to_value().map_err(|error| {
        ModelError::other(format!(
            "model provider `{provider}` sent a chunk that is not JSON: {error}"
        ))
    })?;
    let event: StreamEventV1 = serde_json::from_value(value).map_err(|error| {
        ModelError::other(format!(
            "model provider `{provider}` sent an unrecognised stream event: {error}"
        ))
    })?;
    event
        .into_stream_event()
        .map_err(|error| ModelError::other(error.to_string()))
}

impl futures_util::Stream for PlaneTurn {
    type Item = ModelResult<rebon_api::StreamEvent>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let polled = this.events.poll_recv(cx);
        if matches!(polled, std::task::Poll::Ready(None)) {
            this.terminal_seen = true;
        }
        polled
    }
}

impl Drop for PlaneTurn {
    /// A reader that walked away asks the adapter to stop.
    ///
    /// The stream's own `Drop` closes the call's accounting but sends nothing,
    /// so without this an abandoned turn would keep costing whatever the
    /// adapter is doing upstream — a live HTTP response from a model, most of
    /// the time. Cancel is a request, not a command: the terminal still comes
    /// from whatever the adapter does next.
    fn drop(&mut self) {
        if self.terminal_seen {
            return;
        }
        let supervisor = Arc::clone(&self.supervisor);
        let call_id = std::mem::take(&mut self.call_id);
        tokio::spawn(async move {
            let _ = supervisor.cancel(&call_id).await;
        });
    }
}
