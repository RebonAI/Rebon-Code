//! `EngineSubAgentSpawner` construction, and the capability preflight every
//! local spawn passes through: the builder setters, the per-session scope the
//! spawner is rebuilt in, and the circuit that stops a capability which keeps
//! failing from being retried for the rest of the run.
//!
//! Split out of `runtime/spawner.rs` for the 10k-line cap; the items are
//! unchanged and in their original order.

use super::*;
impl EngineSubAgentSpawner {
    /// Construct a spawner holding a weak reference to the engine.
    ///
    /// Callers typically build the engine as `Arc<Engine>`, then
    /// construct the spawner via `Arc::downgrade(&engine)` and
    /// inject it into every [`rebon_tool::ToolContext`] the engine
    /// hands to its tools.
    pub fn new(engine: Weak<Engine>, client: Arc<dyn ModelClient>) -> Self {
        Self {
            engine,
            client,
            default_model: DEFAULT_SUB_AGENT_MODEL.to_string(),
            base_filter: None,
            coordinator_mode: None,
            coordinator_use_worktree: false,
            model_config: SubAgentModelConfig::default(),
            model_profiles: ModelProfileMap::default(),
            model_router: None,
            automatic_routes: Arc::new(Mutex::new(HashMap::new())),
            task_registry_resolver: None,
            task_registry: None,
            session_tools: None,
            escalation_registry: EscalationRegistry::new(),
            file_history_tracker: None,
            capability_failures: Arc::new(Mutex::new(HashMap::new())),
            ultraplan_worker_reservations: Arc::new(Mutex::new(HashMap::new())),
            external_runner: None,
            policy: rebon_core::policy_seat::PolicySources::default(),
        }
    }

    /// Attach the session's policy-event subscribers, so a worker's tool
    /// calls and turns reach the same guards the session's own do.
    pub fn with_policy(mut self, policy: rebon_core::policy_seat::PolicySources) -> Self {
        self.policy = policy;
        self
    }

    /// Attach the external sub-agent runner. With it wired, a model
    /// spec of the form `<acpAgentId>:<model>` (where the prefix is a
    /// declared agent) runs the whole task on that ACP agent instead of
    /// a local worker.
    pub fn with_external_runner(mut self, runner: Arc<dyn ExternalSubAgentRunner>) -> Self {
        self.external_runner = Some(runner);
        self
    }

    /// Override the model that's used when a sub-agent spec omits
    /// `model`. Builder-style.
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    /// Attach a base [`ToolFilter`] every spawned sub-agent will
    /// inherit. Typically the caller passes
    /// [`rebon_core::coordinator_mode::async_agent_filter`]
    /// when the parent session is in coordinator mode.
    ///
    /// How the base filter interacts with a per-spec filter
    /// ([`SubAgentSpec::tool_filter`]) depends on the runtime mode:
    ///
    /// - **Coordinator mode**: the per-spec filter takes full
    ///   precedence when present — the base filter is only used as
    ///   a fallback when no per-spec filter is supplied. This
    ///   allows workers to receive tools outside the default
    ///   allow-list (e.g. MCP tools).
    ///
    /// - **Non-coordinator mode**: the two filters are
    ///   [`ToolFilter::intersect`]ed so the sub-agent always sees
    ///   the most restrictive combination.
    ///
    /// The filter is wrapped in a fresh [`SharedToolFilter`] cell so
    /// it can be swapped later; callers that need to drive the swap
    /// externally should use [`Self::with_shared_base_filter`] and
    /// keep their own handle to the cell.
    pub fn with_base_filter(mut self, filter: ToolFilter) -> Self {
        self.base_filter = Some(SharedToolFilter::new(filter));
        self
    }

    /// Attach a pre-existing [`SharedToolFilter`] handle as the base
    /// filter so it can be replaced at runtime (e.g. by the TUI
    /// `/ceo` toggle) without rebuilding the spawner.
    pub fn with_shared_base_filter(mut self, filter: SharedToolFilter) -> Self {
        self.base_filter = Some(filter);
        self
    }

    pub fn with_shared_coordinator_mode(mut self, mode: rebon_tool::SharedCoordinatorMode) -> Self {
        self.coordinator_mode = Some(mode);
        self
    }

    pub fn with_coordinator_mode(mut self, enabled: bool) -> Self {
        self.coordinator_mode = Some(rebon_tool::SharedCoordinatorMode::new(enabled));
        self
    }

    pub fn with_coordinator_use_worktree(mut self, enabled: bool) -> Self {
        self.coordinator_use_worktree = enabled;
        self
    }

    /// Set the sub-agent model config the default model router resolves
    /// models with.
    pub fn with_model_config(mut self, config: SubAgentModelConfig) -> Self {
        self.model_config = config;
        self
    }

    pub fn with_model_profiles(mut self, profiles: ModelProfileMap) -> Self {
        self.model_profiles = profiles;
        self
    }

    pub fn with_model_router(mut self, router: Arc<dyn AgentModelRouter>) -> Self {
        self.model_router = Some(router);
        self
    }

    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// Build the router used to resolve the worker's model.
    pub(super) fn model_router(&self) -> Arc<dyn AgentModelRouter> {
        self.model_router.clone().unwrap_or_else(|| {
            Arc::new(
                SingleProviderModelRouter::new(self.client.clone(), self.default_model.clone())
                    .with_provider_name(self.client.provider_name().to_string())
                    .with_model_config(self.model_config.clone())
                    .with_model_profiles(self.model_profiles.clone()),
            )
        })
    }

    /// Resolve task state from the exact session scope named by each spawn.
    /// Missing/disposed/disabled scopes fail before the worker starts.
    pub fn with_task_registry_resolver(
        mut self,
        resolver: rebon_plugin_tasks::TaskRegistryResolver,
    ) -> Self {
        self.task_registry_resolver = Some(resolver);
        self
    }

    #[cfg(test)]
    pub(super) fn with_test_task_registry(mut self, registry: TaskRegistry) -> Self {
        self.task_registry = Some(registry.clone());
        self.escalation_registry = registry.escalation_registry();
        self
    }

    pub(super) fn for_session_spec(&self, spec: &SubAgentSpec) -> Result<Self, String> {
        let Some(resolver) = self.task_registry_resolver.as_ref() else {
            return Ok(self.clone());
        };
        let session_id = spec
            .capability_context
            .as_ref()
            .map(|context| context.session_id.as_str())
            .filter(|session_id| !session_id.trim().is_empty())
            .or_else(|| {
                spec.metadata
                    .get("parent_session_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|session_id| !session_id.is_empty())
            })
            .ok_or_else(|| {
                "sub-agent task registration requires a parent session id".to_string()
            })?;
        let (registry, lease) = resolver.resolve_with_lease(session_id)?;
        let mut scoped = self.clone();
        let engine = self.engine.upgrade().ok_or("engine has been dropped")?;
        scoped.session_tools = Some(engine.scoped_tool_resolver(Some(lease), &[], None));
        scoped.task_registry = Some(registry.as_ref().clone());
        scoped.escalation_registry = registry.escalation_registry();
        Ok(scoped)
    }

    /// Attach the session file-history tracker so sub-agent Write/Edit
    /// snapshots the pre-write file into the same store the main
    /// session uses, making `/rewind` able to undo worker changes.
    pub fn with_file_history_tracker(
        mut self,
        tracker: Arc<dyn rebon_agent_core::file_history::FileHistoryTracker>,
    ) -> Self {
        self.file_history_tracker = Some(tracker);
        self
    }

    pub fn escalation_registry(&self) -> EscalationRegistry {
        self.escalation_registry.clone()
    }

    pub(super) fn preflight_spec(&self, spec: &mut SubAgentSpec) -> Result<(), String> {
        let engine = self
            .engine
            .upgrade()
            .ok_or_else(|| "engine has been dropped".to_string())?;
        let role = metadata_string(&spec.metadata, "ultraplan_role")
            .or_else(|| metadata_string(&spec.metadata, "agent_type"))
            .unwrap_or_else(|| "worker".to_string());
        let is_coord = self
            .coordinator_mode
            .as_ref()
            .map(|mode| mode.get())
            .unwrap_or_else(rebon_core::coordinator_mode::coordinator_mode_from_env_default);
        let base_filter = self.base_filter.as_ref().map(|shared| shared.current());
        let worker_filter = if is_coord {
            spec.tool_filter.clone().or(base_filter)
        } else {
            match (base_filter, spec.tool_filter.clone()) {
                (Some(base), Some(child)) => Some(base.intersect(&child)),
                (Some(base), None) => Some(base),
                (None, child) => child,
            }
        };
        let effective_filter = crate::runtime::worker::effective_worker_tool_filter(
            worker_filter.as_ref(),
            spec.execution_policy.as_ref(),
        );
        let tools = effective_filter
            .as_ref()
            .map(|filter| filtered_tools_from_engine(&engine, filter))
            .unwrap_or_else(|| tools_from_engine(&engine));
        let failure_scope = capability_failure_scope(spec, &role, effective_filter.as_ref());
        if let (Some(capability), Some(requested)) = (
            spec.capability_context.as_ref(),
            effective_filter.as_ref().and_then(ToolFilter::allow_list),
        ) {
            if let Some(missing) = requested
                .iter()
                .find(|tool| engine.find_tool(tool.as_str()).is_none())
            {
                let diagnostic = CapabilityDiagnostic {
                    class: CapabilityDiagnosticClass::ToolUnavailable,
                    message: format!("worker role `{role}` requires unavailable tool `{missing}`"),
                    run_id: capability.run_id.clone(),
                    ledger_revision: capability.ledger_revision,
                    capability_hash: capability.capability_hash.clone(),
                    role: role.clone(),
                    root: None,
                    capability: Some(missing.clone()),
                    retryable: false,
                    fallback_to_parent: true,
                };
                if let Some(open) =
                    self.open_capability_circuit(spec, failure_scope.as_deref(), &diagnostic)
                {
                    return Err(open.to_string());
                }
                return Err(self
                    .record_capability_failure(spec, failure_scope.as_deref(), diagnostic, 1)
                    .to_string());
            }
        }

        let validate = |spec: &SubAgentSpec| {
            let session_id = spec
                .metadata
                .get("parent_session_id")
                .and_then(Value::as_str);
            preflight_worker_capabilities(
                spec.capability_context.as_ref(),
                spec.execution_policy.as_ref(),
                session_id,
                spec.cwd.as_deref(),
                &effective_path_scope_roots(spec),
                &tools,
                &role,
            )
        };
        match validate(spec) {
            Ok(()) => {
                self.clear_capability_failures(spec, failure_scope.as_deref());
                Ok(())
            }
            Err(first) => {
                if let Some(open) =
                    self.open_capability_circuit(spec, failure_scope.as_deref(), &first)
                {
                    return Err(open.to_string());
                }
                if !first.retryable
                    || spec
                        .capability_context
                        .as_ref()
                        .is_none_or(|capability| capability.max_tool_error_retries == 0)
                {
                    return Err(self
                        .record_capability_failure(spec, failure_scope.as_deref(), first, 1)
                        .to_string());
                }

                repair_spec_from_capability(spec);
                let repaired_scope =
                    capability_failure_scope(spec, &role, effective_filter.as_ref());
                match validate(spec) {
                    Ok(()) => {
                        self.clear_capability_failures(spec, repaired_scope.as_deref());
                        Ok(())
                    }
                    Err(second) => {
                        if let Some(open) =
                            self.open_capability_circuit(spec, repaired_scope.as_deref(), &second)
                        {
                            return Err(open.to_string());
                        }
                        let first_fingerprint =
                            scoped_capability_failure_fingerprint(failure_scope.as_deref(), &first);
                        let second_fingerprint = scoped_capability_failure_fingerprint(
                            repaired_scope.as_deref(),
                            &second,
                        );
                        if first_fingerprint == second_fingerprint {
                            return Err(self
                                .record_capability_failure(
                                    spec,
                                    repaired_scope.as_deref(),
                                    second,
                                    2,
                                )
                                .to_string());
                        }

                        let first = self.record_capability_failure(
                            spec,
                            failure_scope.as_deref(),
                            first,
                            1,
                        );
                        if first.class == CapabilityDiagnosticClass::CircuitOpen {
                            return Err(first.to_string());
                        }
                        Err(self
                            .record_capability_failure(spec, repaired_scope.as_deref(), second, 1)
                            .to_string())
                    }
                }
            }
        }
    }

    fn open_capability_circuit(
        &self,
        spec: &SubAgentSpec,
        failure_scope: Option<&str>,
        diagnostic: &CapabilityDiagnostic,
    ) -> Option<CapabilityDiagnostic> {
        let capability = spec.capability_context.as_ref()?;
        let fingerprint = scoped_capability_failure_fingerprint(failure_scope, diagnostic);
        if let Some(repository) = spec.ultraplan_run_repository.as_ref() {
            if let Ok(state) = repository.load_current() {
                if let Some(attempts) = state.tool_error_attempts.get(&fingerprint) {
                    if *attempts > state.budget.max_tool_error_retries {
                        return Some(circuit_diagnostic(diagnostic, *attempts));
                    }
                }
            }
        }
        self.capability_failures
            .lock()
            .expect("capability failure circuit poisoned")
            .get(&fingerprint)
            .filter(|state| state.attempts > capability.max_tool_error_retries)
            .map(|state| circuit_diagnostic(diagnostic, state.attempts))
    }

    fn record_capability_failure(
        &self,
        spec: &SubAgentSpec,
        failure_scope: Option<&str>,
        diagnostic: CapabilityDiagnostic,
        attempts: u32,
    ) -> CapabilityDiagnostic {
        let fingerprint = scoped_capability_failure_fingerprint(failure_scope, &diagnostic);
        let persisted_count = self
            .persist_capability_failure_attempts(spec, &fingerprint, attempts)
            .ok();
        let mut failures = self
            .capability_failures
            .lock()
            .expect("capability failure circuit poisoned");
        let state = failures
            .entry(fingerprint)
            .or_insert_with(|| CapabilityFailureState {
                attempts: 0,
                diagnostic: diagnostic.clone(),
            });
        state.attempts = state.attempts.saturating_add(attempts);
        state.diagnostic = diagnostic.clone();
        let count = persisted_count
            .unwrap_or(state.attempts)
            .max(state.attempts);
        let retry_limit = spec
            .capability_context
            .as_ref()
            .map(|capability| capability.max_tool_error_retries)
            .unwrap_or(0);
        if count <= retry_limit {
            return diagnostic;
        }
        circuit_diagnostic(&diagnostic, count)
    }

    fn persist_capability_failure_attempts(
        &self,
        spec: &SubAgentSpec,
        fingerprint: &str,
        attempts: u32,
    ) -> Result<u32, String> {
        let repository = spec
            .ultraplan_run_repository
            .as_ref()
            .ok_or_else(|| "ultraplan run repository is unavailable".to_string())?;
        for attempt in 0..=1 {
            let mut state = repository.load_current().map_err(|err| err.to_string())?;
            let expected_revision = state.state_revision;
            let count = state.record_tool_error_attempts(fingerprint.to_string(), attempts);
            match repository.compare_and_swap(expected_revision, &state) {
                Ok(()) => return Ok(count),
                Err(rebon_tool::UltraplanRepositoryError::StaleRevision { .. }) if attempt == 0 => {
                }
                Err(err) => return Err(err.to_string()),
            }
        }
        Err("capability failure budget remained stale after one reload".into())
    }

    pub(super) fn reserve_ultraplan_worker_slot(&self, spec: &SubAgentSpec) -> Result<(), String> {
        let Some(capability) = spec.capability_context.as_ref() else {
            return Ok(());
        };
        let role = metadata_string(&spec.metadata, "ultraplan_role")
            .unwrap_or_else(|| "researcher".to_string());
        let (resource, limit, persisted_used, fallback_to_parent) = match role.as_str() {
            "researcher" => (
                "research_agent",
                capability.max_research_agents,
                capability.research_agents_used,
                true,
            ),
            "reviewer" => (
                "adversarial_review",
                capability.max_adversarial_reviews,
                capability.adversarial_reviews_used,
                false,
            ),
            _ => return Ok(()),
        };
        let pre_reserved = spec
            .metadata
            .get("ultraplan_budget_pre_reserved")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let observed = self
            .task_registry
            .as_ref()
            .map(|registry| {
                registry
                    .snapshots()
                    .into_iter()
                    .filter(|snapshot| {
                        snapshot.ultraplan_id() == Some(capability.run_id.as_str())
                            && snapshot.ultraplan_role() == Some(role.as_str())
                            && snapshot
                                .metadata
                                .get("ultraplan_retry_attempt")
                                .and_then(Value::as_bool)
                                != Some(true)
                    })
                    .count() as u32
            })
            .unwrap_or(0);
        let key = format!("{}:{resource}", capability.run_id);
        let mut reservations = self
            .ultraplan_worker_reservations
            .lock()
            .expect("ultraplan worker budget poisoned");
        let used = reservations
            .entry(key)
            .or_insert(observed.max(persisted_used));
        *used = (*used).max(observed).max(persisted_used);
        if pre_reserved {
            if persisted_used == 0 || persisted_used > limit {
                return Err(CapabilityDiagnostic {
                    class: CapabilityDiagnosticClass::BudgetExhausted,
                    message: format!(
                        "ultraplan {resource} pre-reservation is invalid for run `{}` (used {persisted_used}, limit {limit})",
                        capability.run_id
                    ),
                    run_id: capability.run_id.clone(),
                    ledger_revision: capability.ledger_revision,
                    capability_hash: capability.capability_hash.clone(),
                    role,
                    root: None,
                    capability: Some(resource.to_string()),
                    retryable: false,
                    fallback_to_parent,
                }
                .to_string());
            }
            return Ok(());
        }
        if *used >= limit {
            return Err(CapabilityDiagnostic {
                class: CapabilityDiagnosticClass::BudgetExhausted,
                message: format!(
                    "ultraplan {resource} budget exhausted for run `{}` (limit {limit})",
                    capability.run_id
                ),
                run_id: capability.run_id.clone(),
                ledger_revision: capability.ledger_revision,
                capability_hash: capability.capability_hash.clone(),
                role,
                root: None,
                capability: Some(resource.to_string()),
                retryable: false,
                fallback_to_parent,
            }
            .to_string());
        }
        *used += 1;
        Ok(())
    }

    fn clear_capability_failures(&self, spec: &SubAgentSpec, failure_scope: Option<&str>) {
        let Some(failure_scope) = failure_scope else {
            return;
        };
        let prefix = format!("{failure_scope}:");
        if let Some(repository) = spec.ultraplan_run_repository.as_ref() {
            for attempt in 0..=1 {
                let Ok(mut state) = repository.load_current() else {
                    break;
                };
                let expected_revision = state.state_revision;
                if !state.clear_tool_error_attempts(&prefix) {
                    break;
                }
                match repository.compare_and_swap(expected_revision, &state) {
                    Ok(()) => break,
                    Err(rebon_tool::UltraplanRepositoryError::StaleRevision { .. })
                        if attempt == 0 => {}
                    Err(_) => break,
                }
            }
        }
        self.capability_failures
            .lock()
            .expect("capability failure circuit poisoned")
            .retain(|fingerprint, _| !fingerprint.starts_with(&prefix));
    }
}
