//! Harness / coordinator / task orchestration.
//!
//! This crate holds the coordinator, task, and top-level harness-loop
//! pieces, anchored on the `Engine` type the CLI and ACP server
//! entrypoints build on.
//!
//! The design direction:
//!
//! - An `Engine` owns a registry of `Tool`s and drives the model/tool loop.
//! - A coordinator can spawn worker agents as sub-engines with their own
//!   message history.
//! - Tasks are first-class and can be resumed / archived.
//!
//! [`message`] holds the message-graph types (system rows, attachments,
//! hook events) used across the engine.
//!
//! Beyond `message`, [`bridge`] introduces the first bridge-runtime owner
//! hook: a small state abstraction the engine uses to hold an active
//! `rebon-bridge` handle.

pub mod anchored_minimal;
pub mod attachment_seat;
pub mod attachments;
pub mod auto_mode_classifier;
pub mod bridge;
pub(crate) mod context_accounting;
pub(crate) mod context_manager;
/// The one marker a tool uses to say "this image is a live capture"; the
/// engine's history keeps only the newest few of them.
pub use context_manager::LIVE_CAPTURE_RESULT_TEXT_PREFIX;
pub mod coordinator_mode;
pub mod cron;
pub mod deferred_question;
pub mod hooks;
pub mod mcp_runtime;
pub mod message;
pub mod model_routing;
pub mod permission;
pub mod permission_seat;
pub mod policy;
pub mod policy_seat;
pub mod prompt_seat;
pub mod provider_profiles;
pub mod query;
pub mod session_scope;
pub mod skill_seat;
pub mod system_prompt;
pub(crate) mod tool_exposure;
pub mod tool_seat;
pub mod turn_hook;

/// Crate-wide lock for tests that mutate process-global environment
/// variables (`HOME`, `USERPROFILE`, `REBON_CONFIG_DIR`, …). Every test
/// module MUST take this one lock — a per-module mutex only serialises
/// within its module, and cargo's parallel runner then interleaves env
/// mutation across modules (observed as `REBON_CONFIG_DIR` from a
/// `system_prompt` HomeGuard leaking into `query` executor prompts).
/// Poison is recovered on purpose: guards restore variables on unwind.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use rebon_agent_core::ChannelPermissionRequestPublisher;
use rebon_proto::types::{
    PermissionOption, PermissionOptionKind, PermissionOutcome, RequestPermissionParams,
    RequestPermissionResult,
};
use rebon_tool::{
    BashTool, DenyAskPermissionBroker, MonitorRegistry, ShellProcessRegistry, Tool, ToolContext,
    ToolSearchIndex,
};
use rebon_types::{
    ExecutionPolicy, PolicyMode, ShellPolicy, ToolCallReference, UltraplanContext, UltraplanProfile,
};

/// Re-export of the [`rebon_tool::PermissionBroker`] trait so
/// downstream callers can keep importing `rebon_core::PermissionBroker`
/// after the trait moved to `rebon-tool`. Both names refer to the
/// exact same trait.
pub use rebon_tool::PermissionBroker;
use rebon_tools_core::{
    tool_matches_name, PermissionBehavior, PermissionDecision, PermissionRequest, ToolError,
    ToolId, ToolResult,
};
use serde_json::Value;

use crate::bridge::{BridgeConfig, BridgeHandle, BridgeRuntimeState, BridgeStatus};
use crate::tool_exposure::BuiltinToolExposurePolicy;

fn is_eager_promotion(execution_policy: Option<&ExecutionPolicy>, tool: &dyn Tool) -> bool {
    execution_policy.is_some_and(|policy| {
        policy
            .eager_promotions
            .iter()
            .any(|entry| tool_matches_name(tool.id().as_str(), tool.aliases(), entry))
    })
}

/// The [`PermissionBroker`] an ACP-hosted session runs on: an `Ask`
/// decision goes to the client as a permission request over the
/// reverse-RPC channel this broker was built with, and the client's
/// answer decides the tool call.
#[derive(Clone)]
pub struct AcpPermissionBroker {
    publisher: ChannelPermissionRequestPublisher,
    session_id: String,
    /// Live view of the session's permission mode. `None` means the caller
    /// wired no mode source, and every call behaves as `default`.
    ///
    /// Read per dispatch rather than captured, so a client that flips the mode
    /// through `session/set_config_option` mid-turn is obeyed on the next tool
    /// call — the same contract the TUI's mode cell provides.
    mode: Option<Arc<dyn rebon_permissions::denial_sink::PermissionModeProvider>>,
    /// Auto mode's state (verdict cache + denial sink). Built alongside `mode`.
    hooks: Option<rebon_permissions::denial_sink::AutoModeHooks>,
    /// Model-backed classifier for unresolved auto-mode asks. The ACP path uses
    /// the same classifier as the local channel broker; only the eventual human
    /// approval transport differs.
    auto_mode_classifier: Option<Arc<dyn crate::auto_mode_classifier::AutoModeClassifier>>,
    /// Exact session-scope generation held for this broker's whole turn.
    /// Kernel plugins run the `permission/ask` waterfall before the JSON-RPC
    /// round trip — the same seam the TUI/headless broker has, so a session's
    /// permission behavior does not depend on which surface opened it.
    /// Absent (or with no listeners) the ask flow is unchanged.
    kernel_ctx: Option<crate::permission::KernelContextLease>,
}

impl std::fmt::Debug for AcpPermissionBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpPermissionBroker")
            .field("session_id", &self.session_id)
            .field("has_mode_provider", &self.mode.is_some())
            .field(
                "has_auto_mode_classifier",
                &self.auto_mode_classifier.is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl AcpPermissionBroker {
    pub fn new(
        publisher: ChannelPermissionRequestPublisher,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            publisher,
            session_id: session_id.into(),
            mode: None,
            hooks: None,
            auto_mode_classifier: None,
            kernel_ctx: None,
        }
    }

    /// Attach the exact session-scope generation for this ACP turn.
    pub fn with_kernel_context_lease(
        mut self,
        lease: crate::permission::KernelContextLease,
    ) -> Self {
        self.kernel_ctx = Some(lease);
        self
    }

    /// Compatibility for a fixed context whose lifetime is owned elsewhere.
    /// Prefer [`Self::with_kernel_context_lease`] for bounded scopes.
    pub fn with_kernel_context(self, context: rebon_kernel::Context) -> Self {
        self.with_kernel_context_lease(crate::permission::KernelContextLease::unmanaged(context))
    }
    /// Install the model-backed auto-mode classifier used before ACP asks.
    pub fn with_auto_mode_classifier(
        mut self,
        classifier: Arc<dyn crate::auto_mode_classifier::AutoModeClassifier>,
    ) -> Self {
        self.auto_mode_classifier = Some(classifier);
        self
    }

    /// Wire the session's live permission mode.
    ///
    /// Auto mode gets a self-contained [`AutoModeHooks`] for verdict caching
    /// and denial recording. The executor separately installs the same
    /// model-backed classifier used by the local channel broker.
    pub fn with_permission_mode(
        mut self,
        mode: Arc<dyn rebon_permissions::denial_sink::PermissionModeProvider>,
    ) -> Self {
        self.hooks = Some(rebon_permissions::denial_sink::AutoModeHooks::new(
            Arc::new(rebon_permissions::denial_sink::NullDenialSink),
            Arc::clone(&mode),
        ));
        self.mode = Some(mode);
        self
    }

    /// The `permission-rules` seat as this broker's scope sees it. Empty
    /// without a kernel scope, which fails closed: a rule can only add a
    /// prompt, never remove one.
    fn permission_rules(&self) -> crate::permission_seat::PermissionRules {
        self.kernel_ctx
            .as_ref()
            .map(|lease| crate::permission_seat::rules_for(lease.context()))
            .unwrap_or_default()
    }

    fn current_mode(&self) -> rebon_permissions::types::PermissionMode {
        self.mode
            .as_ref()
            .map(|mode| mode.current_mode())
            .unwrap_or(rebon_permissions::types::PermissionMode::Default)
    }

    fn request_params(
        &self,
        context: &ToolContext,
        request: PermissionRequest,
        tool_name: String,
        tool_input: Option<Value>,
    ) -> RequestPermissionParams {
        let title = if request.title.trim().is_empty() {
            None
        } else {
            Some(request.title.clone())
        };
        let message = if request.message.trim().is_empty() {
            None
        } else {
            Some(request.message.clone())
        };
        let metadata = request.metadata.clone();
        RequestPermissionParams {
            session_id: self.session_id.clone(),
            tool_call: ToolCallReference {
                tool_call_id: context
                    .tool_use_id()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "tool-call".into()),
            },
            options: permission_options_from_request(request),
            title,
            message,
            tool_name: Some(tool_name),
            tool_input,
            metadata,
        }
    }
}

#[async_trait]
impl PermissionBroker for AcpPermissionBroker {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn resolve(
        &self,
        tool: &dyn Tool,
        input: Value,
        context: &ToolContext,
        decision: PermissionDecision,
    ) -> ToolResult<Value> {
        match decision.behavior {
            PermissionBehavior::Allow => {
                let effective_input = decision.updated_input.unwrap_or(input);
                tool.call(effective_input, context).await
            }
            PermissionBehavior::Deny => Err(ToolError::PermissionDenied {
                tool: tool.id(),
                reason: decision
                    .reason
                    .unwrap_or_else(|| "tool permission denied".into()),
            }),
            PermissionBehavior::Ask => {
                // Same mode semantics as the TUI broker, from the same
                // implementation — only the transport below differs. That
                // includes the `permission-rules` seat: one snapshot for the
                // whole decision.
                let rules = self.permission_rules();
                let check_input = decision.updated_input.as_ref().unwrap_or(&input);
                match crate::permission::resolve_ask_under_mode(
                    self.current_mode(),
                    tool.id().as_str(),
                    check_input,
                    context,
                    self.hooks.as_ref(),
                    self.auto_mode_classifier.clone(),
                    &rules,
                )
                .await
                {
                    crate::permission::ModeAskOutcome::Run(_) => {
                        let effective_input = decision.updated_input.unwrap_or(input);
                        return tool.call(effective_input, context).await;
                    }
                    crate::permission::ModeAskOutcome::Deny(reason) => {
                        return Err(ToolError::PermissionDenied {
                            tool: tool.id(),
                            reason,
                        });
                    }
                    crate::permission::ModeAskOutcome::Ask => {}
                }
                let request = decision
                    .request
                    .ok_or_else(|| ToolError::PermissionDenied {
                        tool: tool.id(),
                        reason: decision
                            .reason
                            .unwrap_or_else(|| "tool permission request missing payload".into()),
                    })?;
                let permission_authorized_context =
                    rebon_tool::context_with_permission_authorized_paths(
                        tool.id().as_str(),
                        context,
                        &request,
                    );
                let title = request.title.clone();
                let message = request.message.clone();
                let params = self.request_params(
                    context,
                    request,
                    tool.id().as_str().to_owned(),
                    decision.updated_input.clone(),
                );
                // Kernel plugins listening on `permission/ask` may answer (or
                // cancel) before the client is asked — the same seam the
                // TUI/headless broker offers, so which surface opened the
                // session does not change what the kernel sees. The lease is
                // the exact session generation acquired for this whole turn.
                let kernel_answer = self.kernel_ctx.as_ref().and_then(|lease| {
                    let options: Vec<(String, String)> = params
                        .options
                        .iter()
                        .map(|option| (option.option_id.clone(), option.name.clone()))
                        .collect();
                    crate::permission::kernel_ask_waterfall(
                        lease,
                        &self.session_id,
                        tool.id().as_str(),
                        context.tool_use_id(),
                        &title,
                        &message,
                        params.tool_input.as_ref(),
                        &options,
                    )
                });
                let result = match kernel_answer {
                    Some(crate::permission::PermissionAnswer::Cancelled) => {
                        RequestPermissionResult {
                            outcome: PermissionOutcome::Cancelled,
                            option_id: None,
                            updated_input: None,
                        }
                    }
                    Some(crate::permission::PermissionAnswer::Selected {
                        option_id,
                        updated_input,
                        extra_text,
                    }) => {
                        // On this transport the extra text rides inside the
                        // input object (that is how a client sends it), so
                        // fold it in rather than dropping it.
                        let mut updated_input =
                            updated_input.or_else(|| decision.updated_input.clone());
                        if extra_text.is_some() {
                            let mut value = updated_input.unwrap_or_else(|| input.clone());
                            append_permission_extra_text(&mut value, extra_text.as_deref());
                            updated_input = Some(value);
                        }
                        RequestPermissionResult {
                            outcome: PermissionOutcome::Selected,
                            option_id: Some(option_id),
                            updated_input,
                        }
                    }
                    None => self
                        .publisher
                        .request_permission(params)
                        .await
                        .map_err(|source| ToolError::Execution {
                            tool: tool.id(),
                            source,
                        })?,
                };
                match result.outcome {
                    PermissionOutcome::Selected => {
                        // The TUI sends `Selected` for every confirmed
                        // option, including reject options. Check whether
                        // the selected option is actually a rejection so
                        // we don't proceed with tool execution.
                        if let Some(ref opt_id) = result.option_id {
                            let option_kind = crate::permission::option_kind(opt_id);
                            if matches!(
                                option_kind,
                                PermissionOptionKind::RejectOnce
                                    | PermissionOptionKind::RejectAlways
                            ) {
                                let extra_text =
                                    permission_extra_text_from_input(result.updated_input.as_ref());
                                // Revision requests only make sense for a
                                // one-shot rejection; "reject always" means
                                // the user wants the tool stopped, not a
                                // regenerate-and-retry loop.
                                if matches!(option_kind, PermissionOptionKind::RejectOnce) {
                                    if let Some(output) =
                                        crate::permission::workflow_revision_requested_result(
                                            tool.id().as_str(),
                                            extra_text.as_deref(),
                                        )
                                    {
                                        return Ok(output);
                                    }
                                }
                                return Err(ToolError::PermissionDenied {
                                    tool: tool.id(),
                                    reason: crate::permission::permission_denial_reason(
                                        opt_id,
                                        extra_text.as_deref(),
                                        rules.rejection_note(tool.id().as_str()).as_ref(),
                                    ),
                                });
                            }
                        }
                        let extra_text =
                            permission_extra_text_from_input(result.updated_input.as_ref());
                        // Prefer the TUI-supplied updated_input (e.g.
                        // AskUserQuestion answers) over the original
                        // decision.updated_input.
                        let mut effective_input = result
                            .updated_input
                            .or(decision.updated_input)
                            .unwrap_or(input);
                        let mut call_context =
                            permission_authorized_context.unwrap_or_else(|| context.clone());
                        // An approved option means whatever the feature that
                        // offered it says. Same seat, same fold as the TUI
                        // broker.
                        if let Some(option_id) = result.option_id.as_deref() {
                            if let Some(approved) = rules.on_approved(
                                tool.id().as_str(),
                                option_id,
                                &mut effective_input,
                                &call_context,
                            ) {
                                call_context = approved;
                            }
                        }
                        let mut output = tool.call(effective_input, &call_context).await?;
                        append_permission_extra_text(&mut output, extra_text.as_deref());
                        Ok(output)
                    }
                    PermissionOutcome::Cancelled => Err(ToolError::PermissionDenied {
                        tool: tool.id(),
                        reason: "permission request cancelled".into(),
                    }),
                }
            }
        }
    }
}

fn permission_extra_text_from_input(input: Option<&Value>) -> Option<String> {
    input
        .and_then(|input| input.get("permissionExtraText"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn append_permission_extra_text(value: &mut Value, extra_text: Option<&str>) {
    let Some(extra_text) = extra_text.map(str::trim).filter(|text| !text.is_empty()) else {
        return;
    };

    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "permissionExtraText".into(),
            Value::String(extra_text.to_owned()),
        );
    }
}

fn permission_options_from_request(request: PermissionRequest) -> Vec<PermissionOption> {
    if request.options.is_empty() {
        return vec![
            PermissionOption {
                option_id: "allow_once".into(),
                name: "Allow once".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionOption {
                option_id: "reject_once".into(),
                name: "Reject once".into(),
                kind: PermissionOptionKind::RejectOnce,
            },
        ];
    }

    request
        .options
        .into_iter()
        .map(|option_id| PermissionOption {
            name: permission_option_label(&option_id),
            kind: crate::permission::option_kind(&option_id),
            option_id,
        })
        .collect()
}

fn permission_option_label(option_id: &str) -> String {
    match option_id {
        "yes_clear_context_auto" => "Yes, clear context and run with auto mode".into(),
        "yes_auto" => "Yes, run with auto mode".into(),
        "yes_accept_edits" => "Yes, auto-accept edits".into(),
        "yes_default" => "Yes, manually approve edits".into(),
        "allow_once" => "Allow once".into(),
        "allow_always" => "Allow always".into(),
        "reject_once" => "No, chat with this".into(),
        other => other.replace('_', " "),
    }
}

/// Top-level harness.
///
/// Owns the registered tool list and, increasingly, the bridge runtime
/// state. The real engine will also own session state, message history,
/// the permission prompter, coordinator state, etc. — this file
/// deliberately keeps each addition small so follow-up modules don't have
/// to reshape the struct.
pub struct Engine {
    /// Tools the host handed this engine directly. The primitives are not
    /// here: they live on the process tool seat (`core-tools`), and the
    /// remaining feature tools leave for their own plugins one by one.
    tools: Vec<Arc<dyn Tool>>,
    /// The engine's own copy of the core set, for an engine that runs with
    /// no kernel to ask (tests, headless embedding). Set by
    /// `register_builtin_tools_*`; never consulted while a process seat is
    /// reachable.
    core_fallback: Option<Vec<Arc<dyn Tool>>>,
    /// A kernel context whose `tool-registry` seat the process's plugins
    /// fill. Attached once by the host that boots the kernel.
    upstream_tools: std::sync::OnceLock<rebon_kernel::Context>,
    permission_broker: Arc<dyn PermissionBroker>,
    shell_process_registry: Arc<ShellProcessRegistry>,
    monitor_registry: Arc<MonitorRegistry>,
    code_mode_prompts: query::CodeModePromptCache,
    /// Bridge runtime state. Held behind a `std::sync::RwLock` so the
    /// async tool-invoke path can take shared references to `Engine` and
    /// still mutate the attached bridge handle on attach/detach. Critical
    /// sections are tiny (read or replace a handful of fields) and are
    /// never held across an `.await`, so a `tokio::sync::RwLock` would be
    /// overkill.
    bridge: RwLock<BridgeRuntimeState>,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            core_fallback: None,
            upstream_tools: std::sync::OnceLock::new(),
            permission_broker: Arc::new(DenyAskPermissionBroker),
            shell_process_registry: Arc::new(ShellProcessRegistry::new()),
            monitor_registry: Arc::new(MonitorRegistry::new()),
            code_mode_prompts: query::CodeModePromptCache::default(),
            bridge: RwLock::new(BridgeRuntimeState::default()),
        }
    }

    /// Point this engine at the kernel context whose `tool-registry` seat
    /// holds the process's plugin-registered tools. The first attachment
    /// wins; returns whether this call was it.
    pub fn attach_upstream_tool_context(&self, context: rebon_kernel::Context) -> bool {
        self.upstream_tools.set(context).is_ok()
    }

    // 插件派发器必须复用当前内核的路由席位，而不能反向让 core 依赖插件。
    pub fn upstream_tool_context(&self) -> Option<&rebon_kernel::Context> {
        self.upstream_tools.get()
    }

    /// The tools the host registered directly (feature tools, extras).
    pub(crate) fn host_tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// The engine's own copy of the core set, when the builtins were
    /// registered and no process seat is there to serve them.
    pub(crate) fn core_fallback_tools(&self) -> Option<Vec<Arc<dyn Tool>>> {
        self.core_fallback.clone()
    }

    /// Everything a turn can resolve, in precedence order: the host's own
    /// tools first, then the process seat (or, without a kernel, the core
    /// fallback), one entry per name. This is what every snapshot reads.
    pub fn tool_catalog(&self) -> Vec<Arc<dyn Tool>> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut catalog: Vec<Arc<dyn Tool>> = Vec::with_capacity(self.tools.len() + 16);
        let upstream = self
            .upstream_tool_seat(None)
            .and_then(|seat| seat.tools(None).ok())
            .or_else(|| self.core_fallback_tools())
            .unwrap_or_default();
        // Core tools lead the catalog — the order the model sees — and a
        // host-registered tool of the same name defers to the seat's.
        for tool in upstream.into_iter().chain(self.tools.iter().cloned()) {
            let name = tool.id().as_str().to_string();
            if seen.insert(name) {
                catalog.push(tool);
            }
        }
        catalog
    }

    pub fn with_permission_broker(mut self, permission_broker: Arc<dyn PermissionBroker>) -> Self {
        self.permission_broker = permission_broker;
        self
    }

    /// Access the engine's default permission broker.
    pub fn permission_broker(&self) -> &Arc<dyn PermissionBroker> {
        &self.permission_broker
    }

    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    /// The tools this engine still hosts itself, plus the core-set fallback.
    ///
    /// Everything else a session sees comes off the process tool seat, which
    /// the plugins fill. Only `StrReplaceEditor` is still registered directly
    /// for the Anchored Minimal bootstrap; interactive questions come from
    /// the escalation plugin. The `Agent` tool takes its registry from
    /// [`rebon_tool::set_agent_registry_selection`] now, which the host calls
    /// where it used to hand the registry to a constructor here.
    pub fn register_builtin_tools(&mut self) {
        // The thirteen primitives are not registered here: the `core-tools`
        // plugin puts them on the process seat, and a turn reaches them
        // through it. The same list is kept as this engine's fallback for
        // the kernel-less case, so headless callers lose nothing.
        self.core_fallback = Some(rebon_tool::core_tool_set(BashTool::new()));
        // Deferred everywhere except the Anchored Minimal bootstrap request,
        // which advertises it under its wire name.
        self.register_tool(Arc::new(rebon_tool::StrReplaceEditorTool));
    }

    pub fn with_builtin_tools() -> Self {
        let mut engine = Self::new();
        engine.register_builtin_tools();
        engine
    }

    pub fn tool_count(&self) -> usize {
        self.tool_catalog().len()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tool_catalog()
            .iter()
            .filter(|tool| tool.is_enabled())
            .map(|tool| tool.id().as_str().to_owned())
            .collect()
    }

    /// Snapshot every registered tool's name, description, and
    /// input schema. Used by [`query::tools_from_engine`] to
    /// project the registry into an API tool list.
    pub fn tool_snapshots(&self) -> Vec<query::ToolSnapshot> {
        self.tool_catalog()
            .iter()
            .filter(|tool| tool.is_enabled())
            .map(|tool| query::ToolSnapshot {
                name: tool.id().as_str().to_owned(),
                description: tool.model_description().to_owned(),
                input_schema: tool.input_schema(),
                aliases: tool.aliases(),
            })
            .collect()
    }

    /// Snapshot every tool that passes the given filter.
    ///
    /// Equivalent to calling [`Self::tool_snapshots`] and then
    /// running the filter over the resulting list, but more
    /// convenient at call sites that build a filter once and want
    /// to hand the result straight to the query layer.
    pub fn filtered_tool_snapshots(
        &self,
        filter: &rebon_tool::ToolFilter,
    ) -> Vec<query::ToolSnapshot> {
        self.tool_snapshots()
            .into_iter()
            .filter(|snap| filter.allows(&snap.name, snap.aliases))
            .collect()
    }

    /// Build a [`ToolSearchIndex`] from all policy-deferred registered tools
    /// that are enabled.
    pub fn build_tool_search_index(&self) -> ToolSearchIndex {
        let policy = BuiltinToolExposurePolicy::default_coding_agent();
        ToolSearchIndex::build_filtered_by(&self.tool_catalog(), None, |tool| {
            policy.is_deferred(tool)
        })
    }

    pub fn build_tool_search_index_for_policy(
        &self,
        execution_policy: Option<&ExecutionPolicy>,
    ) -> ToolSearchIndex {
        let policy = BuiltinToolExposurePolicy::default_coding_agent();
        ToolSearchIndex::build_filtered_by(&self.tool_catalog(), None, |tool| {
            policy.is_deferred(tool) && !is_eager_promotion(execution_policy, tool)
        })
    }

    /// Build a [`ToolSearchIndex`] from policy-deferred enabled tools that pass
    /// the supplied visibility filter.
    pub fn build_filtered_tool_search_index(
        &self,
        filter: &rebon_tool::ToolFilter,
    ) -> ToolSearchIndex {
        let policy = BuiltinToolExposurePolicy::default_coding_agent();
        ToolSearchIndex::build_filtered_by(&self.tool_catalog(), Some(filter), |tool| {
            policy.is_deferred(tool)
        })
    }

    pub fn build_filtered_tool_search_index_for_policy(
        &self,
        filter: &rebon_tool::ToolFilter,
        execution_policy: Option<&ExecutionPolicy>,
    ) -> ToolSearchIndex {
        let policy = BuiltinToolExposurePolicy::default_coding_agent();
        ToolSearchIndex::build_filtered_by(&self.tool_catalog(), Some(filter), |tool| {
            policy.is_deferred(tool) && !is_eager_promotion(execution_policy, tool)
        })
    }

    /// Snapshot only the policy-eager tools. Used by the
    /// query layer to build the initial tool list sent to the model.
    /// Deferred tools are excluded — the model discovers them via
    /// ToolSearchTool.
    pub fn eager_tool_snapshots(&self) -> Vec<query::ToolSnapshot> {
        self.eager_tool_snapshots_for_policy(None)
    }

    pub fn eager_tool_snapshots_for_policy(
        &self,
        execution_policy: Option<&ExecutionPolicy>,
    ) -> Vec<query::ToolSnapshot> {
        let policy = BuiltinToolExposurePolicy::default_coding_agent();
        self.tool_catalog()
            .iter()
            .filter(|tool| {
                tool.is_enabled()
                    && (policy.is_eager(tool.as_ref())
                        || is_eager_promotion(execution_policy, tool.as_ref()))
            })
            .map(|tool| query::ToolSnapshot {
                name: tool.id().as_str().to_owned(),
                description: tool.model_description().to_owned(),
                input_schema: tool.input_schema(),
                aliases: tool.aliases(),
            })
            .collect()
    }

    /// The names and aliases of the policy-eager tools, without their
    /// descriptions and schemas. What the system prompt needs; a full
    /// [`eager_tool_snapshots`](Self::eager_tool_snapshots) serialises
    /// every input schema, which is most of a session's startup.
    pub fn eager_tool_name_snapshots(&self) -> Vec<query::ToolNameSnapshot> {
        let policy = BuiltinToolExposurePolicy::default_coding_agent();
        self.tool_catalog()
            .iter()
            .filter(|tool| tool.is_enabled() && policy.is_eager(tool.as_ref()))
            .map(|tool| query::ToolNameSnapshot {
                name: tool.id().as_str().to_owned(),
                aliases: tool.aliases(),
            })
            .collect()
    }

    /// Snapshot only policy-deferred tool names (for the system prompt).
    pub fn deferred_tool_names(&self) -> Vec<String> {
        let policy = BuiltinToolExposurePolicy::default_coding_agent();
        self.tool_catalog()
            .iter()
            .filter(|tool| policy.is_deferred(tool.as_ref()))
            .map(|tool| tool.id().as_str().to_owned())
            .collect()
    }

    pub fn deferred_tool_names_for_policy(
        &self,
        execution_policy: Option<&ExecutionPolicy>,
    ) -> Vec<String> {
        let policy = BuiltinToolExposurePolicy::default_coding_agent();
        self.tool_catalog()
            .iter()
            .filter(|tool| policy.is_deferred(tool.as_ref()))
            .filter(|tool| !is_eager_promotion(execution_policy, tool.as_ref()))
            .map(|tool| tool.id().as_str().to_owned())
            .collect()
    }

    /// Snapshot only policy-deferred tool names that pass the given filter.
    pub fn filtered_deferred_tool_names(&self, filter: &rebon_tool::ToolFilter) -> Vec<String> {
        let policy = BuiltinToolExposurePolicy::default_coding_agent();
        self.tool_catalog()
            .iter()
            .filter(|tool| policy.is_deferred(tool.as_ref()))
            .filter(|tool| filter.allows(tool.id().as_str(), tool.aliases()))
            .map(|tool| tool.id().as_str().to_owned())
            .collect()
    }

    pub fn filtered_deferred_tool_names_for_policy(
        &self,
        filter: &rebon_tool::ToolFilter,
        execution_policy: Option<&ExecutionPolicy>,
    ) -> Vec<String> {
        let policy = BuiltinToolExposurePolicy::default_coding_agent();
        self.tool_catalog()
            .iter()
            .filter(|tool| policy.is_deferred(tool.as_ref()))
            .filter(|tool| !is_eager_promotion(execution_policy, tool.as_ref()))
            .filter(|tool| filter.allows(tool.id().as_str(), tool.aliases()))
            .map(|tool| tool.id().as_str().to_owned())
            .collect()
    }

    pub fn find_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tool_catalog().into_iter().find(|tool| {
            tool.is_enabled() && tool_matches_name(tool.id().as_str(), tool.aliases(), name)
        })
    }

    pub fn ultraplan_policy_violation_for_tool(
        ultraplan: &UltraplanContext,
        tool: &dyn Tool,
    ) -> Option<String> {
        let tool_id = tool.id();
        let tool_name = tool_id.as_str();
        if ultraplan.shell_policy == ShellPolicy::DenyShell
            && (tool_matches_name(tool_name, tool.aliases(), "Bash")
                || tool_matches_name(tool_name, tool.aliases(), "PowerShell"))
        {
            return Some("shell tools are denied by ultraplan shell_policy=DenyShell".into());
        }
        let denied = ultraplan
            .denied_tools
            .iter()
            .any(|entry| tool_matches_name(tool_name, tool.aliases(), entry));
        if denied {
            return Some(format!(
                "tool {tool_name} is denied by the active ultraplan policy"
            ));
        }
        let allowed = ultraplan
            .allowed_tools
            .iter()
            .any(|entry| tool_matches_name(tool_name, tool.aliases(), entry));
        if !allowed {
            return Some(format!(
                "tool {tool_name} is not in the active ultraplan allowed_tools list"
            ));
        }
        None
    }

    pub async fn invoke_tool(
        &self,
        name: &str,
        input: Value,
        context: &ToolContext,
    ) -> ToolResult<Value> {
        let mut effective_context = context
            .clone()
            .with_shell_process_registry_if_absent(Arc::clone(&self.shell_process_registry))
            .with_monitor_registry_if_absent(Arc::clone(&self.monitor_registry));
        let tool_resolver = self.tool_resolver_for_context(&effective_context);
        if effective_context.tool_resolver().is_none() {
            effective_context = effective_context.with_tool_resolver(tool_resolver.clone());
        }
        let context = &effective_context;
        let target_name;
        let target_input;
        if name == rebon_tool::INVOKE_DEFERRED_TOOL_NAME {
            let gateway_input: rebon_tool::InvokeDeferredToolInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput {
                    tool: ToolId::new(rebon_tool::INVOKE_DEFERRED_TOOL_NAME),
                    reason: format!("invalid InvokeDeferredTool input: {err}"),
                    error_code: Some(400),
                })?;
            let target = tool_resolver
                .resolve(&gateway_input.tool_name, context.tool_filter())?
                .ok_or_else(|| ToolError::UnknownTool {
                    tool: ToolId::new(&gateway_input.tool_name),
                })?;
            let policy = BuiltinToolExposurePolicy::default_coding_agent();
            if !(policy.is_deferred(target.as_ref())
                || (target.id().as_str() == gateway_input.tool_name && target.should_defer()))
            {
                return Err(ToolError::InvalidInput {
                    tool: ToolId::new(rebon_tool::INVOKE_DEFERRED_TOOL_NAME),
                    reason: format!(
                        "tool `{}` is not a deferred tool and cannot be invoked through InvokeDeferredTool",
                        gateway_input.tool_name
                    ),
                    error_code: Some(400),
                });
            }
            if let Some(index) = context.tool_search_index() {
                let (found, _) = index.select(&[gateway_input.tool_name.as_str()]);
                if found.is_empty() {
                    return Err(ToolError::UnknownTool {
                        tool: ToolId::new(&gateway_input.tool_name),
                    });
                }
                // Discovery state is bookkeeping, not a gate. The discovered
                // set lives on the per-query ToolContext while the transcript
                // (where the model actually saw the schema via an earlier
                // ToolSearch) spans the whole session, so rejecting here just
                // forces a redundant ToolSearch round-trip. Schema validation
                // and permission checks below protect execution regardless.
                context.record_discovered_deferred_tool(&gateway_input.tool_name);
            }
            if gateway_input
                .arguments
                .as_object()
                .is_some_and(|object| object.is_empty())
            {
                let required_fields: Vec<String> = target
                    .input_schema()
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|required| {
                        required
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                if !required_fields.is_empty() {
                    return Err(ToolError::InvalidInput {
                        tool: ToolId::new(rebon_tool::INVOKE_DEFERRED_TOOL_NAME),
                        reason: format!(
                            "Empty arguments for `{}`. Target tool requires: {}.",
                            gateway_input.tool_name,
                            required_fields.join(", ")
                        ),
                        error_code: Some(400),
                    });
                }
            }
            target_name = gateway_input.tool_name;
            target_input = gateway_input.arguments;
        } else {
            target_name = name.to_string();
            target_input = input;
        }

        let tool = tool_resolver
            .resolve(&target_name, context.tool_filter())?
            .ok_or_else(|| ToolError::UnknownTool {
                tool: ToolId::new(&target_name),
            })?;
        let tool_id = tool.id();
        let ultraplan_run_state_available =
            context.load_ultraplan_run_state().ok().flatten().is_some();

        if let Some(ultraplan) = context.ultraplan_context() {
            if ultraplan.profile == UltraplanProfile::Grill && !ultraplan_run_state_available {
                return Err(ToolError::PermissionDenied {
                    tool: tool_id,
                    reason: format!(
                        "Grill-profile ultraplan run state `{}` is missing or does not match the active session",
                        ultraplan.run_id
                    ),
                });
            }
            if let Some(reason) =
                Self::ultraplan_policy_violation_for_tool(ultraplan, tool.as_ref())
            {
                match ultraplan.mode {
                    PolicyMode::Observe => {
                        tracing::warn!(
                            tool = %tool_id.as_str(),
                            run_id = %ultraplan.run_id,
                            phase = %ultraplan.phase,
                            reason = %reason,
                            "ultraplan policy observe: tool invocation would be denied"
                        );
                    }
                    PolicyMode::Enforce => {
                        return Err(ToolError::PermissionDenied {
                            tool: tool_id,
                            reason: format!("ultraplan policy denied tool invocation: {reason}"),
                        });
                    }
                }
            }
        }

        // Centralized schema-level validation.
        // Catches missing required params, unexpected params, and type
        // mismatches before the tool's own validate_input runs.
        let schema = tool.input_schema();
        if let Err(schema_err) =
            rebon_tool::validation::validate_schema(tool_id.as_str(), &schema, &target_input)
        {
            return Err(ToolError::InvalidInput {
                tool: tool_id,
                reason: schema_err.format(),
                error_code: Some(400),
            });
        }

        // Phase 2: Tool-local semantic validation.
        let validation = tool.validate_input(&target_input, context).await?;
        if !validation.is_valid() {
            return Err(ToolError::InvalidInput {
                tool: tool_id,
                reason: validation
                    .message
                    .unwrap_or_else(|| "tool input failed validation".into()),
                error_code: validation.error_code,
            });
        }

        // Phase 3: Permission check.
        let permission = tool.check_permissions(&target_input, context).await?;
        let _ = tool_id;

        if let Some(reason) = context.denial_replay_reason() {
            // Only replay tool inputs restored from AutoModeDenialStore here. Letting
            // model-authored calls set this flag would turn retry into a permission bypass.
            tracing::info!(
                tool = %tool.id().as_str(),
                tool_use_id = context.tool_use_id(),
                reason = %reason,
                "rebon-core: bypassing permission gate for explicit denial replay"
            );
            return tool.call(target_input, context).await;
        }

        // Per-call brokers win over the engine's default. This is
        // how the ACP reverse-RPC broker gets wired into tool
        // dispatch: `EngineQueryExecutor` builds an
        // `AcpPermissionBroker` from the session's permission
        // publisher and threads it through the `ToolContext`.
        if let Some(override_broker) = context.permission_broker() {
            return override_broker
                .resolve(tool.as_ref(), target_input, context, permission)
                .await;
        }
        self.permission_broker
            .resolve(tool.as_ref(), target_input, context, permission)
            .await
    }

    // ------------------------------------------------------------------
    // Bridge runtime state
    //
    // The engine exposes a small, compile-friendly surface for attaching
    // and detaching a bridge handle. The handle trait itself comes from
    // `rebon-bridge` (`BridgeHandle`), implemented by
    // `rebon_bridge::ReplBridgeHandle`.
    // ------------------------------------------------------------------

    /// Current bridge connection status.
    pub fn bridge_status(&self) -> BridgeStatus {
        self.bridge_state_read().status
    }

    /// Whether a bridge handle is currently attached to the engine.
    pub fn is_bridge_attached(&self) -> bool {
        self.bridge_state_read().is_attached()
    }

    /// Clone of the config for the currently-tracked bridge, if any.
    pub fn bridge_config(&self) -> Option<BridgeConfig> {
        self.bridge_state_read().config.clone()
    }

    /// Cloneable handle to the active bridge, if one is attached.
    pub fn bridge_handle(&self) -> Option<Arc<dyn BridgeHandle>> {
        self.bridge_state_read().handle.clone()
    }

    /// Full snapshot of the engine's bridge state.
    pub fn bridge_state(&self) -> BridgeRuntimeState {
        self.bridge_state_read().clone()
    }

    /// Record a bridge config ahead of attachment. Useful for paths that
    /// negotiate config before the handle is ready (the CLI entrypoint or
    /// the ACP bring-up dance). Status stays [`BridgeStatus::Detached`]
    /// until [`Engine::attach_bridge`] is called.
    pub fn record_bridge_config(&self, config: BridgeConfig) {
        let mut state = self.bridge.write().expect("bridge state lock poisoned");
        state.config = Some(config);
    }

    /// Attach a live bridge handle alongside its config. Replaces any
    /// previously attached handle and returns the replaced one so the
    /// caller can drive teardown. Callers that want to be sure there is
    /// no prior handle should check [`Engine::is_bridge_attached`] first.
    pub fn attach_bridge(
        &self,
        config: BridgeConfig,
        handle: Arc<dyn BridgeHandle>,
    ) -> Option<Arc<dyn BridgeHandle>> {
        let mut state = self.bridge.write().expect("bridge state lock poisoned");
        let previous = state.handle.take();
        state.config = Some(config);
        state.handle = Some(handle);
        state.status = BridgeStatus::Attached;
        previous
    }

    /// Detach the current bridge handle and clear the tracked config.
    /// Returns the handle so the caller can run teardown **outside** the
    /// lock. Returns `None` if no bridge was attached.
    pub fn detach_bridge(&self) -> Option<Arc<dyn BridgeHandle>> {
        let mut state = self.bridge.write().expect("bridge state lock poisoned");
        state.status = BridgeStatus::Detached;
        state.config = None;
        state.handle.take()
    }

    fn bridge_state_read(&self) -> std::sync::RwLockReadGuard<'_, BridgeRuntimeState> {
        self.bridge.read().expect("bridge state lock poisoned")
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

// Metadata-only stand-ins: projection tests must not introduce an engine ->
// monitor/notebook/structured-output/memory plugin cycle. Execution is tested
// by the owners.
#[cfg(test)]
struct FeatureProjectionTool(&'static str);

/// `rebon-plugin-memory`'s tool name, spelled here rather than imported: the
/// import is the dependency this fixture exists to avoid, and no crate below
/// the plugin needs the name for anything else.
#[cfg(test)]
const SAVE_MEMORY_TOOL_NAME: &str = "SaveMemory";

#[cfg(test)]
#[async_trait::async_trait]
impl Tool for FeatureProjectionTool {
    fn id(&self) -> ToolId {
        ToolId::new(self.0)
    }
    fn aliases(&self) -> &'static [&'static str] {
        match self.0 {
            rebon_tool::MONITOR_TOOL_NAME => &["MonitorTool"],
            rebon_tool::NOTEBOOK_EDIT_TOOL_NAME => &["NotebookEditTool"],
            rebon_tool::mcp::MCP_TOOL_NAME => &["McpTool", "MCPTool"],
            rebon_tools_core::SKILL_TOOL_NAME => &["SkillTool"],
            SAVE_MEMORY_TOOL_NAME => &["SaveMemoryTool"],
            _ => &[],
        }
    }
    fn description(&self) -> &str {
        "Feature tool projection fixture; not executable"
    }
    fn input_schema(&self) -> rebon_tools_core::ToolInputSchema {
        serde_json::json!({"type": "object"})
    }
    fn should_defer(&self) -> bool {
        // Monitor, NotebookEdit, Mcp and SaveMemory are the deferred
        // four; the other stand-ins project eagerly, exactly as the
        // tools they stand for do.
        !matches!(
            self.0,
            rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME | rebon_tools_core::SKILL_TOOL_NAME
        )
    }
    fn kind(&self) -> rebon_tools_core::ToolKind {
        rebon_tools_core::tool_kind_for_name(self.0)
    }
    fn file_target_field(&self) -> Option<&'static str> {
        if self.0 == rebon_tool::NOTEBOOK_EDIT_TOOL_NAME {
            Some("notebook_path")
        } else {
            None
        }
    }
    async fn call(&self, _input: Value, _context: &ToolContext) -> ToolResult<Value> {
        panic!("projection fixtures must never execute")
    }
}

/// A bare engine carrying the whole builtin tool catalogue.
///
/// [`Engine::with_builtin_tools`] leaves the engine with `StrReplaceEditor`
/// plus the kernel-less core fallback; every
/// feature tool reaches a real session off the process tool seat, which the
/// plugins fill. Tests about *tool projection* — which tools a session kind
/// sees eagerly, which deferred, which not at all — need that full
/// catalogue, so this registers plugin tools directly and uses metadata-only
/// stand-ins for the six features whose dependency cycles would duplicate
/// the engine. No kernel or seat is needed.
///
/// `ComputerUse` is deliberately absent rather than stood in for. Every
/// catalogue path here filters on [`Tool::is_enabled`], and that tool answers
/// `false` unless a desktop service is running on this machine — so it has
/// never reached a projection under test, and an always-enabled stand-in
/// would put it in one for the first time. Its own crate owns it now.
///
/// The plugins are dev-dependencies only; nothing here reaches the built
/// library.
#[cfg(test)]
pub(crate) fn engine_with_every_builtin_tool() -> Engine {
    let mut engine = Engine::with_builtin_tools();
    let plugin_tools = rebon_plugin_agents::tools()
        .into_iter()
        .chain(rebon_plugin_cron::tools())
        .chain(rebon_plugin_escalation::tools())
        .chain(
            [
                rebon_tool::MONITOR_TOOL_NAME,
                rebon_tool::NOTEBOOK_EDIT_TOOL_NAME,
                rebon_tool::mcp::MCP_TOOL_NAME,
                rebon_tools_core::SKILL_TOOL_NAME,
                SAVE_MEMORY_TOOL_NAME,
            ]
            .map(|name| Arc::new(FeatureProjectionTool(name)) as Arc<dyn Tool>),
        )
        .chain(rebon_plugin_plan_mode::tools())
        .chain(rebon_plugin_profile::tools())
        .chain([Arc::new(FeatureProjectionTool(
            rebon_tool::STRUCTURED_OUTPUT_TOOL_NAME,
        )) as Arc<dyn Tool>])
        .chain(rebon_plugin_tasks::tools())
        .chain(rebon_plugin_web::tools())
        .chain(rebon_plugin_workflow::tools());
    for tool in plugin_tools {
        engine.register_tool(tool);
    }
    engine
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_mode_classifier::{
        AutoModeClassifier, AutoModeClassifierOutcome, AutoModeClassifierRequest,
        AutoModeClassifierStage,
    };
    use async_trait::async_trait;
    use rebon_plugin_plan_mode::ExitPlanModeTool;
    use rebon_tool::{EchoTool, GlobTool};
    use rebon_tools_core::{
        PermissionDecision, PermissionRequest, ToolInputSchema, ValidationOutcome,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};

    use rebon_proto::JsonRpcResponse;
    use rebon_proto::JsonRpcVersion;

    static SUB_AGENT_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    /// The primitives are on the process seat (`core-tools`) and reach a
    /// bare engine only as its kernel-less fallback; the feature tools are
    /// on their plugins. What the engine still registers itself is the
    /// short list that needs its wiring — pinned so a tool cannot drift
    /// back in unnoticed.
    #[test]
    fn a_bare_engine_hosts_only_the_tools_that_still_need_its_wiring() {
        let engine = Engine::with_builtin_tools();
        let mut hosted: Vec<String> = engine
            .tools
            .iter()
            .map(|tool| tool.id().as_str().to_owned())
            .collect();
        hosted.sort();
        // `StrReplaceEditorTool` answers to its wire name, `str_replace_editor`,
        // not to its Rust type name.
        assert_eq!(hosted, ["str_replace_editor"]);

        let fallback = engine
            .core_fallback_tools()
            .expect("a bare engine keeps the core set as its kernel-less fallback");
        assert!(fallback.iter().any(|tool| tool.id().as_str() == "Bash"));

        let names = engine.tool_names();
        for plugin_owned in [
            "AskUserQuestion",
            "TaskCreate",
            "EnterPlanMode",
            "WebSearch",
            "CronCreate",
            "Agent",
            "Workflow",
        ] {
            assert!(
                !names.iter().any(|name| name == plugin_owned),
                "{plugin_owned} belongs to a plugin, not to the engine"
            );
        }
    }
    static SHELL_TOOL_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct ApproveAskBroker;

    #[async_trait]
    impl PermissionBroker for ApproveAskBroker {
        async fn resolve(
            &self,
            tool: &dyn Tool,
            input: Value,
            context: &ToolContext,
            decision: PermissionDecision,
        ) -> ToolResult<Value> {
            match decision.behavior {
                PermissionBehavior::Allow | PermissionBehavior::Ask => {
                    let effective_input = decision.updated_input.unwrap_or(input);
                    tool.call(effective_input, context).await
                }
                other => panic!("expected allow/ask decision, got {other:?}"),
            }
        }
    }

    struct RewriteAskBroker;

    #[async_trait]
    impl PermissionBroker for RewriteAskBroker {
        async fn resolve(
            &self,
            tool: &dyn Tool,
            input: Value,
            context: &ToolContext,
            _decision: PermissionDecision,
        ) -> ToolResult<Value> {
            let rewritten = match input {
                Value::Object(mut map) => {
                    map.insert("approved".into(), json!(true));
                    Value::Object(map)
                }
                other => other,
            };
            tool.call(rewritten, context).await
        }
    }

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestConfigHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        task_list_id: String,
        old_config_dir: Option<String>,
        old_task_list_id: Option<String>,
        old_team_name: Option<String>,
        old_session_id: Option<String>,
    }

    impl TestConfigHome {
        fn new(prefix: &str) -> Self {
            // The crate-wide lock, not a private one. This guard used to hold
            // a module-local mutex, which serialised `TestConfigHome` against
            // itself and against nothing else — so it swapped `REBON_CONFIG_DIR`
            // out from under `query::tests`, whose own guard was busy excluding
            // a different set of tests. Both sides then wrote tasks into each
            // other's temp dir and failed on a lookup that found nothing,
            // rarely enough to read as flake and only when thread scheduling
            // happened to overlap the two windows.
            let guard = test_env_lock();
            let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = tempfile::Builder::new()
                .prefix(&format!("rebon-core-task-tests-{prefix}-"))
                .tempdir()
                .unwrap();

            let task_list_id = format!("{prefix}-{nonce}");
            let old_config_dir = std::env::var("REBON_CONFIG_DIR").ok();
            let old_task_list_id = std::env::var("REBON_TASK_LIST_ID").ok();
            let old_team_name = std::env::var("REBON_TEAM_NAME").ok();
            let old_session_id = std::env::var("REBON_SESSION_ID").ok();

            std::env::set_var("REBON_CONFIG_DIR", dir.path());
            std::env::set_var("REBON_TASK_LIST_ID", &task_list_id);
            std::env::remove_var("REBON_TEAM_NAME");
            std::env::remove_var("REBON_SESSION_ID");

            Self {
                _guard: guard,
                _dir: dir,
                task_list_id,
                old_config_dir,
                old_task_list_id,
                old_team_name,
                old_session_id,
            }
        }

        fn task_list_id(&self) -> &str {
            &self.task_list_id
        }
    }

    impl Drop for TestConfigHome {
        fn drop(&mut self) {
            restore_env("REBON_CONFIG_DIR", self.old_config_dir.as_deref());
            restore_env("REBON_TASK_LIST_ID", self.old_task_list_id.as_deref());
            restore_env("REBON_TEAM_NAME", self.old_team_name.as_deref());
            restore_env("REBON_SESSION_ID", self.old_session_id.as_deref());
        }
    }

    fn restore_env(name: &str, value: Option<&str>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    #[test]
    fn engine_registers_tools() {
        let mut engine = Engine::new();
        assert_eq!(engine.tool_count(), 0);
        engine.register_tool(Arc::new(EchoTool));
        assert_eq!(engine.tool_count(), 1);
        assert_eq!(engine.tool_names(), vec!["Echo"]);
    }

    #[test]
    fn engine_finds_tool_by_alias() {
        let mut engine = Engine::new();
        engine.register_tool(Arc::new(EchoTool));
        assert!(engine.find_tool("EchoTool").is_some());
        assert!(engine.find_tool("Echo").is_some());
        assert!(engine.find_tool("Missing").is_none());
    }

    #[tokio::test]
    async fn engine_invokes_tool_by_alias() {
        let mut engine = Engine::new();
        engine.register_tool(Arc::new(EchoTool));

        let out = engine
            .invoke_tool("EchoTool", json!({ "hello": "world" }), &ToolContext::new())
            .await
            .unwrap();

        assert_eq!(out, json!({ "hello": "world" }));
    }

    #[tokio::test]
    async fn engine_returns_validation_errors() {
        let mut engine = Engine::new();
        engine.register_tool(Arc::new(GlobTool));

        let err = engine
            .invoke_tool("Glob", json!({}), &ToolContext::new())
            .await
            .unwrap_err();

        match err {
            ToolError::InvalidInput { error_code, .. } => assert_eq!(error_code, Some(400)),
            other => panic!("expected invalid input, got {other:?}"),
        }
    }

    struct AskPermissionTool;

    #[async_trait]
    impl Tool for AskPermissionTool {
        fn id(&self) -> ToolId {
            ToolId::new("AskPermission")
        }

        fn description(&self) -> &str {
            "test tool"
        }

        fn input_schema(&self) -> ToolInputSchema {
            json!({
                "type": "object",
                "additionalProperties": true
            })
        }

        async fn validate_input(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<ValidationOutcome> {
            Ok(ValidationOutcome::valid())
        }

        async fn check_permissions(
            &self,
            _input: &Value,
            _context: &ToolContext,
        ) -> ToolResult<PermissionDecision> {
            Ok(PermissionDecision::ask(
                PermissionRequest::new("Need approval", "Ask first"),
                None,
            ))
        }

        async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(input)
        }
    }

    #[tokio::test]
    async fn engine_surfaces_permission_requests_until_runtime_is_wired() {
        let mut engine = Engine::new();
        engine.register_tool(Arc::new(AskPermissionTool));

        let err = engine
            .invoke_tool("AskPermission", json!({ "a": 1 }), &ToolContext::new())
            .await
            .unwrap_err();

        match err {
            ToolError::PermissionDenied { reason, .. } => {
                assert_eq!(reason, "permission required: Need approval");
            }
            other => panic!("expected permission denied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_tool_observe_mode_allows_disallowed_tool() {
        let mut engine = Engine::new();
        engine.register_tool(Arc::new(EchoTool));
        let policy = rebon_types::ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
            "run-1",
            "plan_mode_active",
            PolicyMode::Observe,
        ));
        let context = ToolContext::new().with_execution_policy(policy);

        let out = engine
            .invoke_tool("Echo", json!({ "a": 1 }), &context)
            .await
            .unwrap();

        assert_eq!(out, json!({ "a": 1 }));
    }

    #[tokio::test]
    async fn invoke_tool_enforce_mode_denies_disallowed_tool() {
        let mut engine = Engine::new();
        engine.register_tool(Arc::new(EchoTool));
        let policy = rebon_types::ExecutionPolicy::ultraplan(UltraplanContext::planning_turn(
            "run-1",
            "plan_mode_active",
            PolicyMode::Enforce,
        ));
        let context = ToolContext::new().with_execution_policy(policy);

        let err = engine
            .invoke_tool("Echo", json!({ "a": 1 }), &context)
            .await
            .unwrap_err();

        match err {
            ToolError::PermissionDenied { reason, .. } => {
                assert!(reason.contains("not in the active ultraplan allowed_tools list"));
            }
            other => panic!("expected permission denied, got {other:?}"),
        }
    }

    #[test]
    fn ultraplan_policy_denies_shell_tools_with_deny_shell() {
        let bash = rebon_tool::BashTool::new();
        let powershell = rebon_tool::PowerShellTool;
        let policy =
            UltraplanContext::planning_turn("run-1", "plan_mode_active", PolicyMode::Enforce);

        assert!(Engine::ultraplan_policy_violation_for_tool(&policy, &bash)
            .unwrap()
            .contains("shell tools are denied"));
        assert!(
            Engine::ultraplan_policy_violation_for_tool(&policy, &powershell)
                .unwrap()
                .contains("shell tools are denied")
        );
    }

    #[tokio::test]
    async fn engine_permission_broker_can_resume_ask_path() {
        let mut engine = Engine::new().with_permission_broker(Arc::new(ApproveAskBroker));
        engine.register_tool(Arc::new(AskPermissionTool));

        let out = engine
            .invoke_tool("AskPermission", json!({ "a": 1 }), &ToolContext::new())
            .await
            .unwrap();

        assert_eq!(out, json!({ "a": 1 }));
    }

    #[tokio::test]
    async fn engine_permission_broker_can_rewrite_input_before_call() {
        let mut engine = Engine::new().with_permission_broker(Arc::new(RewriteAskBroker));
        engine.register_tool(Arc::new(AskPermissionTool));

        let out = engine
            .invoke_tool("AskPermission", json!({ "a": 1 }), &ToolContext::new())
            .await
            .unwrap();

        assert_eq!(out, json!({ "a": 1, "approved": true }));
    }

    #[tokio::test]
    async fn engine_manages_background_shell_across_tool_calls() {
        let engine =
            Engine::with_builtin_tools().with_permission_broker(Arc::new(ApproveAskBroker));
        let context = ToolContext::new().with_session_id("background-shell-session");
        #[cfg(windows)]
        let (tool_name, command) = ("PowerShell", "Write-Output ready; Start-Sleep -Seconds 30");
        #[cfg(not(windows))]
        let (tool_name, command) = ("Bash", "printf 'ready\\n'; sleep 30");

        let started = engine
            .invoke_tool(
                tool_name,
                json!({ "command": command, "run_in_background": true }),
                &context,
            )
            .await
            .unwrap();
        let shell_id = started["shellId"].as_str().unwrap().to_owned();
        assert_eq!(started["timeoutMs"], Value::Null);

        let output = engine
            .invoke_tool(
                "ShellOutput",
                json!({
                    "shellId": shell_id,
                    "cursor": 0,
                    "wait": true,
                    "timeout": 5_000
                }),
                &context,
            )
            .await
            .unwrap();
        assert!(output["output"].as_str().unwrap().contains("ready"));

        let stopped = engine
            .invoke_tool("ShellStop", json!({ "shellId": shell_id }), &context)
            .await
            .unwrap();
        assert_eq!(stopped["stopRequested"], true);

        let mut cursor = output["nextCursor"].as_u64().unwrap();
        let completed = loop {
            let next = engine
                .invoke_tool(
                    "ShellOutput",
                    json!({
                        "shellId": shell_id,
                        "cursor": cursor,
                        "wait": true,
                        "timeout": 5_000
                    }),
                    &context,
                )
                .await
                .unwrap();
            cursor = next["nextCursor"].as_u64().unwrap();
            if next["completed"] == true {
                break next;
            }
        };
        assert_eq!(completed["status"], "stopped");
    }

    #[tokio::test]
    async fn engine_round_trips_task_tools() {
        // The task tools belong to the `tasks` plugin now; a bare engine has
        // none of them. Registering the five directly keeps this about the
        // engine's dispatch, which is what it was always testing.
        let mut engine = Engine::with_builtin_tools();
        engine.register_tool(Arc::new(rebon_plugin_tasks::TaskCreateTool));
        engine.register_tool(Arc::new(rebon_plugin_tasks::TaskGetTool));
        engine.register_tool(Arc::new(rebon_plugin_tasks::TaskListTool));
        engine.register_tool(Arc::new(rebon_plugin_tasks::TaskUpdateTool));
        let home = TestConfigHome::new("engine-task-tools");

        let created = engine
            .invoke_tool(
                "TaskCreate",
                json!({
                    "subject": "Run tests",
                    "description": "Exercise task round trip",
                    "activeForm": "Running tests"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(created["task"]["id"], json!("1"));

        let listed = engine
            .invoke_tool("TaskListTool", json!({}), &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(listed["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(listed["tasks"][0]["subject"], json!("Run tests"));

        let fetched = engine
            .invoke_tool("TaskGet", json!({ "taskId": "1" }), &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(
            fetched["task"]["description"],
            json!("Exercise task round trip")
        );

        let updated = engine
            .invoke_tool(
                "TaskUpdateTool",
                json!({
                    "taskId": "1",
                    "status": "completed",
                    "owner": "agent-a"
                }),
                &ToolContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(updated["success"], json!(true));
        assert_eq!(updated["updatedFields"], json!(["status"]));

        let listed_again = engine
            .invoke_tool("TaskList", json!({}), &ToolContext::new())
            .await
            .unwrap();
        assert_eq!(listed_again["tasks"][0]["status"], json!("completed"));
        assert_eq!(listed_again["tasks"][0]["owner"], Value::Null);

        let stored = rebon_tool::tasks::get_task(home.task_list_id(), "1")
            .unwrap()
            .unwrap();
        assert_eq!(stored.status.to_string(), "completed");
    }

    struct ShellToolPreferenceGuard {
        prior: rebon_tool::ShellToolPreference,
    }

    impl ShellToolPreferenceGuard {
        fn set(preference: rebon_tool::ShellToolPreference) -> Self {
            let prior = rebon_tool::shell_tool_preference();
            rebon_tool::set_shell_tool_preference(preference);
            Self { prior }
        }
    }

    impl Drop for ShellToolPreferenceGuard {
        fn drop(&mut self) {
            rebon_tool::set_shell_tool_preference(self.prior);
        }
    }

    /// The shell preference has to reach the *tool list*, not just the tool:
    /// `tool_names` filters on `is_enabled`, which is also what decides
    /// whether the system prompt's tool section mentions the shell at all.
    #[test]
    fn shell_tool_preference_picks_which_shell_the_model_sees() {
        let _lock = SHELL_TOOL_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let engine = Engine::with_builtin_tools();
        let has = |name: &str| engine.tool_names().iter().any(|tool| tool == name);

        {
            let _guard = ShellToolPreferenceGuard::set(rebon_tool::ShellToolPreference::Bash);
            assert!(has("Bash"));
            assert!(!has("PowerShell"));
        }

        // PowerShell-only and Both depend on a runtime being installed, which
        // the CI image may not have — assert against that fact rather than
        // against the platform.
        let powershell_installed = rebon_tool::powershell::is_available();
        {
            let _guard = ShellToolPreferenceGuard::set(rebon_tool::ShellToolPreference::Both);
            assert!(has("Bash"));
            assert_eq!(has("PowerShell"), powershell_installed);
        }
        {
            let _guard = ShellToolPreferenceGuard::set(rebon_tool::ShellToolPreference::PowerShell);
            assert_eq!(has("PowerShell"), powershell_installed);
            // Never leave a session with no shell at all.
            assert!(has("Bash") || has("PowerShell"));
        }
    }

    struct SubAgentsEnabledGuard {
        prior: bool,
    }

    impl SubAgentsEnabledGuard {
        fn set(enabled: bool) -> Self {
            let prior = rebon_tool::sub_agents_enabled();
            rebon_tool::set_sub_agents_enabled(enabled);
            Self { prior }
        }
    }

    impl Drop for SubAgentsEnabledGuard {
        fn drop(&mut self) {
            rebon_tool::set_sub_agents_enabled(self.prior);
        }
    }

    #[test]
    fn agent_tool_description_omits_worktree_agent_by_default() {
        let _lock = SUB_AGENT_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let _guard = SubAgentsEnabledGuard::set(true);
        // The tool is `rebon-plugin-agents`' now; what is under test here is
        // the engine's projection of it, so register it directly.
        let mut engine = Engine::with_builtin_tools();
        engine.register_tool(Arc::new(
            rebon_plugin_agents::AgentTool::with_registry_and_options(
                Arc::new(rebon_tool::AgentRegistry::builtins_only()),
                false,
            ),
        ));
        let agent = engine
            .tool_snapshots()
            .into_iter()
            .find(|tool| tool.name == "Agent")
            .expect("Agent tool should be registered");
        assert!(!agent.description.contains("batch-worker"));
        assert!(agent.description.contains("runtime-created worktrees"));
        assert!(engine
            .eager_tool_snapshots()
            .into_iter()
            .any(|tool| tool.name == "Agent"));
        assert!(!engine.deferred_tool_names().contains(&"Agent".to_string()));
    }

    #[test]
    fn agent_tool_description_includes_worktree_when_enabled() {
        let _lock = SUB_AGENT_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();
        let _guard = SubAgentsEnabledGuard::set(true);
        let mut engine = Engine::with_builtin_tools();
        engine.register_tool(Arc::new(
            rebon_plugin_agents::AgentTool::with_registry_and_options(
                Arc::new(rebon_tool::AgentRegistry::builtins_only()),
                true,
            ),
        ));
        let agent = engine
            .tool_snapshots()
            .into_iter()
            .find(|tool| tool.name == "Agent")
            .expect("Agent tool should be registered");
        assert!(agent.description.contains("batch-worker"));
        assert!(agent.description.contains("worktree"));
        assert!(engine
            .eager_tool_snapshots()
            .into_iter()
            .any(|tool| tool.name == "Agent"));
        assert!(!engine.deferred_tool_names().contains(&"Agent".to_string()));
    }

    // --------------------------------------------------------------
    // Bridge runtime state tests
    // --------------------------------------------------------------

    use crate::bridge::{BridgeConfig, BridgeHandle, BridgeStatus};

    struct StubBridgeHandle {
        bridge_session_id: String,
    }

    impl StubBridgeHandle {
        fn new(id: &str) -> Self {
            Self {
                bridge_session_id: id.to_string(),
            }
        }
    }

    impl BridgeHandle for StubBridgeHandle {
        fn bridge_session_id(&self) -> &str {
            &self.bridge_session_id
        }
    }

    fn sample_bridge_config() -> BridgeConfig {
        BridgeConfig::new(
            "bridge-uuid-1",
            "env_abc",
            "claude_code",
            "https://api.test",
            "wss://session.test",
        )
    }

    #[test]
    fn new_engine_has_detached_bridge_state() {
        let engine = Engine::new();
        assert_eq!(engine.bridge_status(), BridgeStatus::Detached);
        assert!(!engine.is_bridge_attached());
        assert!(engine.bridge_config().is_none());
        assert!(engine.bridge_handle().is_none());
    }

    #[test]
    fn record_bridge_config_does_not_attach() {
        let engine = Engine::new();
        engine.record_bridge_config(sample_bridge_config());
        assert_eq!(engine.bridge_status(), BridgeStatus::Detached);
        assert!(!engine.is_bridge_attached());
        let cfg = engine.bridge_config().expect("config should be recorded");
        assert_eq!(cfg.bridge_id, "bridge-uuid-1");
        assert_eq!(cfg.environment_id, "env_abc");
        assert!(engine.bridge_handle().is_none());
    }

    #[test]
    fn attach_bridge_sets_status_and_exposes_handle() {
        let engine = Engine::new();
        let handle: Arc<dyn BridgeHandle> = Arc::new(StubBridgeHandle::new("session_test_abc"));
        let replaced = engine.attach_bridge(sample_bridge_config(), handle.clone());
        assert!(replaced.is_none());
        assert_eq!(engine.bridge_status(), BridgeStatus::Attached);
        assert!(engine.is_bridge_attached());
        let got = engine.bridge_handle().expect("handle should be attached");
        assert_eq!(got.bridge_session_id(), "session_test_abc");
        let cfg = engine.bridge_config().expect("config should be attached");
        assert_eq!(cfg.worker_type, "claude_code");
    }

    #[test]
    fn attach_bridge_returns_previous_handle() {
        let engine = Engine::new();
        let first: Arc<dyn BridgeHandle> = Arc::new(StubBridgeHandle::new("session_first"));
        let second: Arc<dyn BridgeHandle> = Arc::new(StubBridgeHandle::new("session_second"));
        engine.attach_bridge(sample_bridge_config(), first.clone());
        let replaced = engine
            .attach_bridge(sample_bridge_config(), second.clone())
            .expect("first handle should be returned when replaced");
        assert!(Arc::ptr_eq(&first, &replaced));
        assert_eq!(replaced.bridge_session_id(), "session_first");
        // Engine should now be holding the second handle.
        let held = engine
            .bridge_handle()
            .expect("second handle should be held");
        assert!(Arc::ptr_eq(&held, &second));
        assert_eq!(held.bridge_session_id(), "session_second");
    }

    #[test]
    fn detach_bridge_returns_handle_and_clears_state() {
        let engine = Engine::new();
        let stub: Arc<dyn BridgeHandle> = Arc::new(StubBridgeHandle::new("session_test_abc"));
        engine.attach_bridge(sample_bridge_config(), stub.clone());

        let detached = engine
            .detach_bridge()
            .expect("detach should return the attached handle");
        assert!(Arc::ptr_eq(&detached, &stub));
        assert_eq!(detached.bridge_session_id(), "session_test_abc");

        assert_eq!(engine.bridge_status(), BridgeStatus::Detached);
        assert!(!engine.is_bridge_attached());
        assert!(engine.bridge_config().is_none());
        assert!(engine.bridge_handle().is_none());
    }

    #[test]
    fn detach_bridge_without_attachment_is_noop() {
        let engine = Engine::new();
        assert!(engine.detach_bridge().is_none());
        assert_eq!(engine.bridge_status(), BridgeStatus::Detached);
    }

    #[test]
    fn bridge_state_snapshot_is_independent_of_engine() {
        let engine = Engine::new();
        let handle: Arc<dyn BridgeHandle> = Arc::new(StubBridgeHandle::new("session_test_abc"));
        engine.attach_bridge(sample_bridge_config(), handle);

        let snapshot = engine.bridge_state();
        assert!(snapshot.is_attached());
        assert!(snapshot.handle.is_some());
        assert_eq!(snapshot.config.as_ref().unwrap().bridge_id, "bridge-uuid-1");

        engine.detach_bridge();
        // Snapshot taken before detach should still look attached.
        assert!(snapshot.is_attached());
        // Engine view is detached.
        assert!(!engine.is_bridge_attached());
    }

    // ------------------------------------------------------------------
    // End-to-end ReplBridgeHandle integration
    //
    // These tests exercise the full bridge stack: they build a real
    // `rebon_bridge::ReplBridgeHandle` (backed by `InMemoryBridgeApiClient`),
    // hand it to `Engine::attach_bridge`, and drive attach/detach/shutdown.
    // ------------------------------------------------------------------

    use rebon_bridge::{
        start_bridge_runtime, BridgeApiClient as FullBridgeApiClient,
        BridgeConfig as FullBridgeConfig, InMemoryBridgeApiClient, PermissionResponseBody,
        PermissionResponseEvent, RecordedMethod, RuntimeOptions, RuntimeStatus, WorkData,
        WorkDataType, WorkResponse,
    };

    fn sample_full_bridge_config() -> FullBridgeConfig {
        let mut cfg = FullBridgeConfig::minimal(
            "bridge-full-1",
            "env-req-1",
            "https://api.test",
            "wss://session.test",
        );
        cfg.dir = "/tmp/repo".into();
        cfg.branch = "main".into();
        cfg.worker_type = "claude_code".into();
        cfg
    }

    #[tokio::test]
    async fn engine_attaches_real_repl_bridge_handle_and_detaches_cleanly() {
        // Keep a typed clone of the in-memory client so we can assert
        // on recorded calls without fighting dyn upcasts.
        let in_memory = Arc::new(InMemoryBridgeApiClient::new());
        let client: Arc<dyn FullBridgeApiClient> = in_memory.clone();
        let runtime_handle = start_bridge_runtime(
            sample_full_bridge_config(),
            client,
            RuntimeOptions::for_tests(),
        )
        .await
        .unwrap();

        // The handle implements rebon_bridge::BridgeHandle, which is
        // the same trait rebon_core::bridge re-exports.
        let engine = Engine::new();
        let diagnostic_config = crate::bridge::BridgeConfig::from(runtime_handle.config());
        let as_engine_handle: Arc<dyn BridgeHandle> = runtime_handle.clone();
        let replaced = engine.attach_bridge(diagnostic_config.clone(), as_engine_handle);
        assert!(replaced.is_none());

        assert_eq!(engine.bridge_status(), BridgeStatus::Attached);
        assert!(engine.is_bridge_attached());
        let held = engine
            .bridge_handle()
            .expect("handle should be attached after start_bridge_runtime");
        assert_eq!(held.bridge_session_id(), runtime_handle.environment_id());

        let held_config = engine.bridge_config().expect("diagnostic config recorded");
        assert_eq!(held_config.bridge_id, "bridge-full-1");
        assert_eq!(held_config.worker_type, "claude_code");

        // Detach from the engine — returns the Arc so the caller can
        // drive teardown.
        let detached = engine
            .detach_bridge()
            .expect("detach should return the attached runtime handle");
        assert_eq!(
            detached.bridge_session_id(),
            runtime_handle.environment_id()
        );

        // Shut down the runtime itself.
        runtime_handle.shutdown().await.unwrap();
        assert!(matches!(
            runtime_handle.status(),
            RuntimeStatus::Stopped | RuntimeStatus::Failed
        ));

        // Confirm the end-to-end lifecycle hit the in-memory client.
        let methods: Vec<_> = in_memory.calls().into_iter().map(|c| c.method).collect();
        assert!(methods.contains(&RecordedMethod::Register));
        assert!(methods.contains(&RecordedMethod::Deregister));
    }

    #[tokio::test]
    async fn engine_end_to_end_poll_loop_observed_through_api_client() {
        // Keep a typed handle to the in-memory client so we can assert
        // on recorded calls without fighting dyn upcasts.
        let in_memory = Arc::new(InMemoryBridgeApiClient::new());
        // Pre-seed a scripted work response so the poll loop has
        // something to consume on its first iteration.
        in_memory.push_poll(Ok(Some(WorkResponse {
            id: "work-1".into(),
            response_type: "work".into(),
            environment_id: "env-inmemory-bridge-full-1".into(),
            state: "ready".into(),
            data: WorkData {
                data_type: WorkDataType::Session,
                id: "sess-1".into(),
            },
            secret: "opaque".into(),
            created_at: "2026-04-09T00:00:00.000Z".into(),
        })));
        let client: Arc<dyn FullBridgeApiClient> = in_memory.clone();

        let handle = start_bridge_runtime(
            sample_full_bridge_config(),
            client,
            RuntimeOptions::for_tests(),
        )
        .await
        .unwrap();

        // Wait for the poll task to reach Running so we know the
        // registration has landed.
        let status = handle.wait_until_running().await;
        assert_eq!(status, RuntimeStatus::Running);

        // Attach to the engine concurrently.
        let engine = Engine::new();
        let as_engine_handle: Arc<dyn BridgeHandle> = handle.clone();
        engine.attach_bridge(
            crate::bridge::BridgeConfig::from(handle.config()),
            as_engine_handle,
        );

        // Give the poll loop a few iterations on the paused clock —
        // we're on the multi-threaded default runtime here (the test
        // attribute is `#[tokio::test]`), so real wall-clock advances
        // are enough.
        for _ in 0..5 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        assert!(handle.poll_count() >= 1);

        // Fire a permission response through the api client — matches
        // the reverse path a session runner would use. Here we're just
        // verifying the plumbing.
        let event = PermissionResponseEvent::new(PermissionResponseBody::success(
            "req-1",
            serde_json::json!({"behavior": "allow"}),
        ));
        handle
            .api_client()
            .send_permission_response_event("sess-1", &event, "session-token")
            .await
            .unwrap();

        // Tear everything down in the right order: detach from engine,
        // then shut down the runtime.
        let detached = engine.detach_bridge().expect("engine had handle attached");
        assert_eq!(detached.bridge_session_id(), handle.environment_id());
        handle.shutdown().await.unwrap();

        // The in-memory client should have seen the register, at
        // least one poll, the send_permission_response_event call,
        // and the deregister on shutdown.
        let methods: Vec<_> = in_memory.calls().into_iter().map(|c| c.method).collect();
        assert!(methods.contains(&RecordedMethod::Register));
        assert!(methods.contains(&RecordedMethod::Poll));
        assert!(methods.contains(&RecordedMethod::SendPermissionResponse));
        assert!(methods.contains(&RecordedMethod::Deregister));
    }

    #[test]
    fn permission_option_label_maps_exit_plan_mode_options() {
        assert_eq!(
            permission_option_label("yes_clear_context_auto"),
            "Yes, clear context and run with auto mode"
        );
        assert_eq!(
            permission_option_label("yes_auto"),
            "Yes, run with auto mode"
        );
        assert_eq!(
            permission_option_label("yes_accept_edits"),
            "Yes, auto-accept edits"
        );
        assert_eq!(
            permission_option_label("yes_default"),
            "Yes, manually approve edits"
        );
        assert_eq!(permission_option_label("reject_once"), "No, chat with this");
        assert_eq!(permission_option_label("allow_once"), "Allow once");
        assert_eq!(permission_option_label("allow_always"), "Allow always");
    }

    #[test]
    fn option_kind_maps_exit_plan_mode_yes_to_allow() {
        for option_id in [
            "yes_clear_context_auto",
            "yes_auto",
            "yes_accept_edits",
            "yes_default",
        ] {
            assert_eq!(
                crate::permission::option_kind(option_id),
                PermissionOptionKind::AllowOnce,
            );
        }
        assert_eq!(
            crate::permission::option_kind("reject_once"),
            PermissionOptionKind::RejectOnce,
        );
    }

    // ── ACP permission modes ──────────────────────────────────────
    //
    // The ACP path used to have no mode branch at all: every mode behaved as
    // `default`. These pin that it now shares `ChannelPermissionBroker`'s
    // semantics, over its own transport.

    /// A mode source backed by a cell, standing in for the ACP session record
    /// that `session/set_config_option` mutates.
    fn acp_mode_cell(
        initial: rebon_permissions::types::PermissionMode,
    ) -> (
        Arc<std::sync::Mutex<rebon_permissions::types::PermissionMode>>,
        Arc<dyn rebon_permissions::denial_sink::PermissionModeProvider>,
    ) {
        let cell = Arc::new(std::sync::Mutex::new(initial));
        let read = Arc::clone(&cell);
        (
            cell,
            Arc::new(move || *read.lock().expect("mode cell poisoned")),
        )
    }

    struct AcpEchoTool(&'static str);

    #[async_trait]
    impl Tool for AcpEchoTool {
        fn id(&self) -> rebon_tools_core::ToolId {
            rebon_tools_core::ToolId::new(self.0)
        }

        fn description(&self) -> &str {
            "Returns its input."
        }

        fn input_schema(&self) -> rebon_tools_core::ToolInputSchema {
            json!({ "type": "object", "additionalProperties": true })
        }

        async fn call(&self, input: Value, _context: &ToolContext) -> ToolResult<Value> {
            Ok(input)
        }
    }

    #[derive(Debug)]
    struct AcpFixedClassifier {
        outcome: AutoModeClassifierOutcome,
        requests: Mutex<Vec<AutoModeClassifierRequest>>,
    }

    impl AcpFixedClassifier {
        fn new(outcome: AutoModeClassifierOutcome) -> Self {
            Self {
                outcome,
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AutoModeClassifier for AcpFixedClassifier {
        async fn classify(
            &self,
            request: AutoModeClassifierRequest,
        ) -> anyhow::Result<AutoModeClassifierOutcome> {
            self.requests
                .lock()
                .expect("requests poisoned")
                .push(request);
            Ok(self.outcome.clone())
        }
    }

    fn acp_ask_decision(input: &Value) -> PermissionDecision {
        PermissionDecision::ask(
            PermissionRequest::new("Approve", "Approve this call?")
                .with_options(["allow_once", "reject_once"]),
            Some(input.clone()),
        )
    }

    #[tokio::test]
    async fn acp_auto_runs_classifier_allow_without_requesting_permission() {
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        let (_cell, mode) = acp_mode_cell(rebon_permissions::types::PermissionMode::Auto);
        let classifier = Arc::new(AcpFixedClassifier::new(AutoModeClassifierOutcome::Allow {
            reason: "requested operation is safe".to_owned(),
            stage: AutoModeClassifierStage::Fast,
        }));
        let broker = AcpPermissionBroker::new(publisher, "sess-auto-allow")
            .with_permission_mode(mode)
            .with_auto_mode_classifier(classifier.clone());
        let input = json!({ "command": "cargo test" });
        let context = ToolContext::new()
            .with_tool_use_id("t-auto-allow")
            .with_auto_mode_classifier_transcript("User: run the project tests\n");

        let output = broker
            .resolve(
                &AcpEchoTool("Bash"),
                input.clone(),
                &context,
                acp_ask_decision(&input),
            )
            .await
            .expect("classifier allow should run the tool");

        assert_eq!(output, input);
        assert!(rx.try_recv().is_err(), "auto mode must not ask the client");
        let requests = classifier.requests.lock().expect("requests poisoned");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].transcript, "User: run the project tests\n");
    }

    #[tokio::test]
    async fn acp_auto_classifier_block_denies_without_requesting_permission() {
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        let (_cell, mode) = acp_mode_cell(rebon_permissions::types::PermissionMode::Auto);
        let classifier = Arc::new(AcpFixedClassifier::new(AutoModeClassifierOutcome::Block {
            reason: "[Git Destructive] force push was not authorized".to_owned(),
            category: Some("Git Destructive".to_owned()),
        }));
        let broker = AcpPermissionBroker::new(publisher, "sess-auto-block")
            .with_permission_mode(mode)
            .with_auto_mode_classifier(classifier.clone());
        let input = json!({ "command": "git push --force origin main" });

        let error = broker
            .resolve(
                &AcpEchoTool("Bash"),
                input.clone(),
                &ToolContext::new().with_tool_use_id("t-auto-block"),
                acp_ask_decision(&input),
            )
            .await
            .expect_err("classifier block should deny the tool");

        assert!(matches!(error, ToolError::PermissionDenied { .. }));
        assert!(error.to_string().contains("Git Destructive"));
        assert!(rx.try_recv().is_err(), "auto mode must not ask the client");
        assert_eq!(
            classifier.requests.lock().expect("requests poisoned").len(),
            1
        );
    }

    #[tokio::test]
    async fn acp_auto_without_classifier_fails_closed_without_requesting_permission() {
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        let (_cell, mode) = acp_mode_cell(rebon_permissions::types::PermissionMode::Auto);
        let broker =
            AcpPermissionBroker::new(publisher, "sess-auto-missing").with_permission_mode(mode);
        let input = json!({ "command": "cargo test" });

        let error = broker
            .resolve(
                &AcpEchoTool("Bash"),
                input.clone(),
                &ToolContext::new().with_tool_use_id("t-auto-missing"),
                acp_ask_decision(&input),
            )
            .await
            .expect_err("missing classifier must fail closed");

        assert!(error.to_string().contains("Classifier unavailable"));
        assert!(
            rx.try_recv().is_err(),
            "fail-closed must not ask the client"
        );
    }

    #[tokio::test]
    async fn acp_bypass_runs_without_requesting_permission() {
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        let (_cell, mode) =
            acp_mode_cell(rebon_permissions::types::PermissionMode::BypassPermissions);
        let broker = AcpPermissionBroker::new(publisher, "sess-bypass").with_permission_mode(mode);

        let context = ToolContext::new().with_tool_use_id("t-bypass");
        let input = json!({ "command": "cargo build" });
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            broker.resolve(
                &AcpEchoTool("Bash"),
                input.clone(),
                &context,
                acp_ask_decision(&input),
            ),
        )
        .await
        .expect("bypass must not wait on an ACP permission request")
        .expect("bypass should run the tool");

        assert_eq!(result, input);
        assert!(rx.try_recv().is_err(), "no request should reach the client");
    }

    #[tokio::test]
    async fn acp_dont_ask_denies_with_a_reason_and_no_request() {
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        let (_cell, mode) = acp_mode_cell(rebon_permissions::types::PermissionMode::DontAsk);
        let broker =
            AcpPermissionBroker::new(publisher, "sess-dont-ask").with_permission_mode(mode);

        let context = ToolContext::new().with_tool_use_id("t-dont-ask");
        let input = json!({ "command": "cargo build" });
        let error = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            broker.resolve(
                &AcpEchoTool("Bash"),
                input.clone(),
                &context,
                acp_ask_decision(&input),
            ),
        )
        .await
        .expect("dontAsk must not wait on an ACP permission request")
        .expect_err("dontAsk should refuse");

        match error {
            ToolError::PermissionDenied { reason, .. } => {
                assert!(reason.contains("dontAsk"), "{reason}");
                assert!(reason.contains("denied again"), "{reason}");
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn acp_accept_edits_runs_edits_but_still_requests_for_bash() {
        for tool_name in ["Edit", "Write"] {
            let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
            let (_cell, mode) =
                acp_mode_cell(rebon_permissions::types::PermissionMode::AcceptEdits);
            let broker =
                AcpPermissionBroker::new(publisher, "sess-accept").with_permission_mode(mode);
            let context = ToolContext::new().with_tool_use_id("t-edit");
            let input = json!({ "file_path": "src/main.rs" });

            let result = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                broker.resolve(
                    &AcpEchoTool(tool_name),
                    input.clone(),
                    &context,
                    acp_ask_decision(&input),
                ),
            )
            .await
            .expect("acceptEdits must not wait on a request")
            .expect("acceptEdits should run the edit");
            assert_eq!(result, input);
            assert!(rx.try_recv().is_err(), "tool={tool_name}");
        }

        // Outside the closed set the ACP request still goes out.
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        let (_cell, mode) = acp_mode_cell(rebon_permissions::types::PermissionMode::AcceptEdits);
        let broker = AcpPermissionBroker::new(publisher, "sess-accept").with_permission_mode(mode);
        let context = ToolContext::new().with_tool_use_id("t-bash");
        let input = json!({ "command": "rm -rf ./build" });
        let pending = tokio::spawn(async move {
            broker
                .resolve(
                    &AcpEchoTool("Bash"),
                    input.clone(),
                    &context,
                    acp_ask_decision(&input),
                )
                .await
        });
        let outbound = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("Bash must still reach the ACP client")
            .expect("permission request");
        drop(outbound.response_tx);
        assert!(pending.await.unwrap().is_err());
    }

    /// `session/set_config_option` mutates the session record the provider
    /// reads, so a mid-session switch has to apply to the next tool call.
    #[tokio::test]
    async fn acp_mode_switch_applies_without_rebuilding_the_broker() {
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        let (cell, mode) = acp_mode_cell(rebon_permissions::types::PermissionMode::Default);
        let broker = AcpPermissionBroker::new(publisher, "sess-flip").with_permission_mode(mode);

        let context = ToolContext::new().with_tool_use_id("t-flip");
        let input = json!({ "command": "cargo build" });

        // Default: the request goes to the client.
        let pending = tokio::spawn({
            let broker = broker.clone();
            let context = context.clone();
            let input = input.clone();
            async move {
                broker
                    .resolve(
                        &AcpEchoTool("Bash"),
                        input.clone(),
                        &context,
                        acp_ask_decision(&input),
                    )
                    .await
            }
        });
        let outbound = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("default must request permission")
            .expect("permission request");
        drop(outbound.response_tx);
        assert!(pending.await.unwrap().is_err());

        // Switched to bypass mid-session: the same call runs.
        *cell.lock().expect("mode cell poisoned") =
            rebon_permissions::types::PermissionMode::BypassPermissions;
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            broker.resolve(
                &AcpEchoTool("Bash"),
                input.clone(),
                &context,
                acp_ask_decision(&input),
            ),
        )
        .await
        .expect("bypass must not block")
        .expect("bypass should run the tool");
        assert_eq!(result, input);
        assert!(rx.try_recv().is_err());
    }

    /// Without a mode source every call must behave exactly as before this
    /// existed — that is what keeps the many callers that never wire one safe.
    #[tokio::test]
    async fn acp_broker_without_a_mode_source_still_requests_permission() {
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        let broker = AcpPermissionBroker::new(publisher, "sess-none");
        let context = ToolContext::new().with_tool_use_id("t-none");
        let input = json!({ "file_path": "src/main.rs" });
        let pending = tokio::spawn(async move {
            broker
                .resolve(
                    &AcpEchoTool("Edit"),
                    input.clone(),
                    &context,
                    acp_ask_decision(&input),
                )
                .await
        });
        let outbound = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("a broker with no mode source must still request")
            .expect("permission request");
        drop(outbound.response_tx);
        assert!(pending.await.unwrap().is_err());
    }

    /// A rule on the `permission-rules` seat, for the ACP transport's half of
    /// the contract. What plan mode's own rule claims is asserted in that
    /// plugin; what this file owns is that the broker asks the seat at all.
    struct AcpSeamRule;

    impl crate::permission_seat::PermissionRule for AcpSeamRule {
        fn on_approved(
            &self,
            _tool_name: &str,
            option_id: &str,
            input: &mut Value,
            context: &ToolContext,
        ) -> Option<ToolContext> {
            // A generic allow id: option kind is still the engine's table,
            // so an invented one classifies as a rejection.
            if option_id != "allow_once" {
                return None;
            }
            if let Some(object) = input.as_object_mut() {
                object.insert("permissionMode".into(), Value::String("auto".into()));
                object.insert("clearContext".into(), Value::Bool(true));
            }
            Some(context.clone())
        }

        fn rejection_note(
            &self,
            _tool_name: &str,
        ) -> Option<crate::permission_seat::RejectionNote> {
            Some(crate::permission_seat::RejectionNote {
                guidance: "Seam guidance from the rule.".into(),
                feedback_label: "Seam label".into(),
            })
        }
    }

    fn acp_seam_kernel() -> Arc<rebon_kernel::Kernel> {
        let kernel = rebon_kernel::Kernel::new();
        let seat = crate::permission_seat::PermissionRuleSeat::new();
        kernel
            .context()
            .provide::<crate::permission_seat::PermissionRuleSeatService>(seat.clone())
            .unwrap();
        seat.register(kernel.context(), "seam", Arc::new(AcpSeamRule))
            .unwrap();
        kernel
    }

    /// A rejection over the ACP transport carries the seat's wording, not the
    /// generic sentence.
    #[tokio::test]
    async fn acp_broker_denies_tool_when_reject_option_selected() {
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        let kernel = acp_seam_kernel();
        let broker = AcpPermissionBroker::new(publisher, "sess-reject")
            .with_kernel_context(kernel.context().clone());

        let tool = Arc::new(ExitPlanModeTool);
        let context = ToolContext::new().with_tool_use_id("tool-reject-1");

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Plan ready", "Review")
                .with_options(["allow_once", "reject_once"]),
            Some(json!({"plan": "do stuff"})),
        );

        let broker_handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(
                        tool.as_ref(),
                        json!({"plan": "do stuff"}),
                        &context,
                        decision,
                    )
                    .await
            }
        });

        let outbound = rx.recv().await.expect("permission request");
        let response = JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: Some(outbound.request_id),
            result: Some(json!({
                "outcome": { "outcome": "selected", "optionId": "reject_once" }
            })),
            error: None,
        };
        let _ = outbound.response_tx.send(response);

        match broker_handle.await.unwrap().unwrap_err() {
            ToolError::PermissionDenied { reason, .. } => {
                assert!(!reason.contains("reject_once"), "reason: {reason}");
                assert!(reason.contains("No, chat with this"), "reason: {reason}");
                assert!(
                    reason.contains("Seam guidance from the rule."),
                    "reason: {reason}"
                );
            }
            other => panic!("expected PermissionDenied, got: {other:?}"),
        }
    }

    /// An approved option's rewrite reaches the tool over the ACP transport
    /// too — the same seat, read the same way as the channel broker's.
    #[tokio::test]
    async fn acp_broker_applies_an_approved_options_rewrite() {
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        let kernel = acp_seam_kernel();
        let broker = AcpPermissionBroker::new(publisher, "sess-plan")
            .with_kernel_context(kernel.context().clone());
        let tool = Arc::new(ExitPlanModeTool);
        let context = ToolContext::new().with_tool_use_id("tool-plan-1");
        let decision = PermissionDecision::ask(
            PermissionRequest::new("Plan ready", "Review")
                .with_options(["allow_once", "reject_once"]),
            Some(json!({"plan": "do the work"})),
        );
        let handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(
                        tool.as_ref(),
                        json!({"plan": "do the work"}),
                        &context,
                        decision,
                    )
                    .await
            }
        });

        let outbound = rx.recv().await.expect("permission request");
        let response = JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: Some(outbound.request_id),
            result: Some(json!({
                "outcome": { "outcome": "selected", "optionId": "allow_once" }
            })),
            error: None,
        };
        outbound.response_tx.send(response).unwrap();

        let output = handle.await.unwrap().unwrap();
        assert_eq!(output["permissionMode"], "auto");
        assert_eq!(output["mode"], "auto");
        assert_eq!(output["clearContext"], Value::Bool(true));
    }

    #[tokio::test]
    async fn acp_broker_allow_injects_extra_text_into_output() {
        let (publisher, mut rx) = ChannelPermissionRequestPublisher::new();
        let broker = AcpPermissionBroker::new(publisher, "sess-allow-note");

        let tool = Arc::new(EchoTool);
        let context = ToolContext::new().with_tool_use_id("tool-allow-note-1");

        let decision = PermissionDecision::ask(
            PermissionRequest::new("Test", "Approve?").with_options(["allow_once", "reject_once"]),
            Some(json!({"command": "npm test"})),
        );

        let broker_handle = tokio::spawn({
            let tool = tool.clone();
            let context = context.clone();
            async move {
                broker
                    .resolve(
                        tool.as_ref(),
                        json!({"command": "npm test"}),
                        &context,
                        decision,
                    )
                    .await
            }
        });

        let outbound = rx.recv().await.expect("permission request");
        let response = JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: Some(outbound.request_id),
            result: Some(json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "allow_once",
                    "updatedInput": {
                        "command": "npm test",
                        "permissionExtraText": "only run unit tests"
                    }
                }
            })),
            error: None,
        };
        let _ = outbound.response_tx.send(response);

        let result = broker_handle
            .await
            .unwrap()
            .expect("allow should call tool");
        assert_eq!(result["permissionExtraText"], "only run unit tests");
    }

    #[test]
    fn permission_options_from_exit_plan_mode_request() {
        let request = PermissionRequest::new("Plan ready", "Review").with_options([
            "yes_clear_context_auto",
            "yes_auto",
            "yes_accept_edits",
            "yes_default",
            "reject_once",
        ]);
        let options = permission_options_from_request(request);
        assert_eq!(options.len(), 5);
        let actual = options
            .iter()
            .map(|option| (option.option_id.as_str(), option.name.as_str(), option.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![
                (
                    "yes_clear_context_auto",
                    "Yes, clear context and run with auto mode",
                    PermissionOptionKind::AllowOnce,
                ),
                (
                    "yes_auto",
                    "Yes, run with auto mode",
                    PermissionOptionKind::AllowOnce,
                ),
                (
                    "yes_accept_edits",
                    "Yes, auto-accept edits",
                    PermissionOptionKind::AllowOnce,
                ),
                (
                    "yes_default",
                    "Yes, manually approve edits",
                    PermissionOptionKind::AllowOnce,
                ),
                (
                    "reject_once",
                    "No, chat with this",
                    PermissionOptionKind::RejectOnce,
                ),
            ]
        );
    }
}
