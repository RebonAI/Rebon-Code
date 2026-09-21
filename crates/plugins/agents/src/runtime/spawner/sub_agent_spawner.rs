//! The `rebon_tool::SubAgentSpawner` implementation itself: the calls the
//! `Agent` tool makes, on top of the paths in the sibling modules.
//!
//! Split out of `runtime/spawner.rs` for the 10k-line cap; the items are
//! unchanged and in their original order.

use super::*;
use async_trait::async_trait;
#[async_trait]
impl SubAgentSpawner for EngineSubAgentSpawner {
    fn resolve_external_agent(&self, prefix: &str) -> Option<String> {
        self.external_runner
            .as_ref()
            .and_then(|runner| runner.resolve_agent(prefix))
    }

    fn manages_worktree_lifecycle(&self) -> bool {
        true
    }

    fn preflight(&self, spec: &mut SubAgentSpec) -> Result<(), String> {
        self.preflight_spec(spec)
    }

    async fn spawn(&self, mut spec: SubAgentSpec) -> Result<SubAgentResult, String> {
        let scoped = self.for_session_spec(&spec)?;
        scoped.preflight_spec(&mut spec)?;
        scoped.reserve_ultraplan_worker_slot(&spec)?;
        scoped.spawn_inner(spec, None).await
    }

    async fn spawn_with_progress(
        &self,
        mut spec: SubAgentSpec,
        progress: Option<SubAgentProgressSender>,
    ) -> Result<SubAgentResult, String> {
        let scoped = self.for_session_spec(&spec)?;
        scoped.preflight_spec(&mut spec)?;
        scoped.reserve_ultraplan_worker_slot(&spec)?;
        scoped.spawn_inner(spec, progress).await
    }

    async fn spawn_background(&self, mut spec: SubAgentSpec) -> Result<String, String> {
        spec.run_in_background = true;
        self.spawn_detached(spec).await
    }

    async fn spawn_detached(&self, mut spec: SubAgentSpec) -> Result<String, String> {
        let scoped = self.for_session_spec(&spec)?;
        // Fail fast if the parent engine is already gone. The detached task
        // checks again inside `spawn`, but this keeps callers from receiving
        // a fake launch that cannot ever run.
        if self.engine.upgrade().is_none() {
            return Err("engine has been dropped".to_string());
        }

        let agent_id = ensure_agent_id(&mut spec.metadata);
        let task_kind = spec
            .task_kind
            .unwrap_or_else(|| SubAgentTaskKind::from_metadata(&spec.metadata));
        spec.task_kind = Some(task_kind);
        let owner_session_id = spec
            .capability_context
            .as_ref()
            .map(|context| context.session_id.clone())
            .or_else(|| {
                spec.metadata
                    .get("parent_session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|session_id| !session_id.trim().is_empty());
        if let Some(obj) = spec.metadata.as_object_mut() {
            obj.entry("coordinator_task_kind")
                .or_insert_with(|| Value::String(task_kind.as_str().to_string()));
            if !obj.contains_key("parent_session_id") {
                if let Some(owner_session_id) = owner_session_id.clone() {
                    obj.insert("parent_session_id".into(), Value::String(owner_session_id));
                }
            }
        }
        // Resolve the scratchpad now, while `spec.cwd` still points at
        // the real project directory — the worktree preparation below
        // rewrites it before `spawn_inner` runs. The stashed value is
        // what `spawn_inner` uses.
        let scratchpad_dir =
            sub_agent_scratchpad_dir(&spec, owner_session_id.as_deref(), &agent_id);
        if let (Some(dir), Some(obj)) = (scratchpad_dir.as_deref(), spec.metadata.as_object_mut()) {
            obj.entry(SCRATCHPAD_DIR_METADATA_KEY)
                .or_insert_with(|| Value::String(dir.to_string()));
        }
        scoped.preflight_spec(&mut spec)?;
        let start_backgrounded = spec.run_in_background;
        let is_coord = self
            .coordinator_mode
            .as_ref()
            .map(|mode| mode.get())
            .unwrap_or_else(rebon_core::coordinator_mode::coordinator_mode_from_env_default);
        // Externally-routed tasks get no worktree — the agent CLI runs
        // in the real project directory — and no model-router call,
        // which would treat the `id:model` spec as a literal model id.
        // `spawn_inner` re-derives the same route inside the detached
        // task from the same inputs.
        let external_route = self.external_route_for(&spec);
        let preflight_git_metadata = if external_route.is_none() {
            let mut prepared_spec = spec.clone();
            let preflight_git_metadata = prepare_runtime_worktree(
                &mut prepared_spec,
                &agent_id,
                task_kind,
                runtime_worktree_policy(is_coord, self.coordinator_use_worktree),
            )?;
            spec.cwd = prepared_spec.cwd.clone();
            spec.runtime_isolated_worktree = prepared_spec.runtime_isolated_worktree;
            spec.allowed_roots = prepared_spec.allowed_roots.clone();
            spec.metadata = prepared_spec.metadata.clone();
            spec.task_kind = prepared_spec.task_kind;
            scoped.preflight_spec(&mut spec)?;
            preflight_git_metadata
        } else {
            None
        };
        scoped.reserve_ultraplan_worker_slot(&spec)?;
        if let Some(git) = preflight_git_metadata.as_ref() {
            if let Some(obj) = spec.metadata.as_object_mut() {
                obj.insert("__runtime_git".to_string(), git.to_json());
            }
        }

        let task_registry = scoped.task_registry.clone();
        let task_id = TaskId::new(agent_id.clone());
        let spec_metadata = spec.metadata.clone();
        let agent_type = metadata_string(&spec_metadata, "agent_type");
        let registry_agent_type = agent_type
            .clone()
            .unwrap_or_else(|| "general-purpose".to_string());
        let registry_category = metadata_category(&spec_metadata);
        let registry_title = spec_metadata
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                spec.prompt
                    .lines()
                    .next()
                    .map(|s| s.chars().take(120).collect::<String>())
                    .unwrap_or_default()
            });
        let registry_prompt = spec.prompt.clone();
        let requested_model_profile = spec.model_profile.as_deref().or_else(|| {
            spec_metadata
                .get("modelProfile")
                .or_else(|| spec_metadata.get("model_profile"))
                .and_then(Value::as_str)
        });
        let registry_model = match &external_route {
            // The router would resolve the `id:model` spec against a
            // local provider; the label is all the registry needs.
            Some((route, _)) => external_registry_model_label(route),
            None => {
                self.model_router()
                    .resolve(ModelRouteRequest {
                        provider: spec.provider.clone(),
                        model: spec.model.clone(),
                        model_profile: requested_model_profile.map(str::to_string),
                        agent_type: agent_type.clone(),
                        category: registry_category.clone(),
                        reasoning_effort: metadata_reasoning_effort(&spec_metadata),
                    })
                    .await
                    .map_err(|err| err.to_string())?
                    .model
            }
        };
        let base_filter_snapshot: Option<ToolFilter> =
            self.base_filter.as_ref().map(|shared| shared.current());
        let spec_tool_filter = spec.tool_filter.clone();
        let registry_tool_filter = if is_coord {
            spec_tool_filter.or(base_filter_snapshot)
        } else {
            match (base_filter_snapshot, spec_tool_filter) {
                (Some(base), Some(spec_f)) => Some(base.intersect(&spec_f)),
                (Some(base), None) => Some(base),
                (None, Some(spec_f)) => Some(spec_f),
                (None, None) => None,
            }
        };
        let registry_allowed_tools = registry_tool_filter
            .as_ref()
            .and_then(ToolFilter::allow_list);
        let base_system = spec.system.clone().or_else(|| {
            if is_coord {
                Some(
                    rebon_core::coordinator_mode::worker_system_prompt_with_options(
                        self.coordinator_use_worktree,
                    ),
                )
            } else {
                None
            }
        });
        // Mirror the suffix `spawn_inner` will append so the registry
        // snapshot shows the same system prompt the worker receives.
        let snapshot_prompt_suffix = compose_sub_agent_prompt_suffix(scratchpad_dir.as_deref());
        let registry_system = prepend_ultraplan_worker_preamble(
            base_system,
            spec.execution_policy.as_ref(),
            &spec_metadata,
        )
        .map(|base| {
            if base.trim_end().is_empty() {
                snapshot_prompt_suffix.clone()
            } else {
                format!("{base}\n\n{snapshot_prompt_suffix}")
            }
        });
        let start_time_ms = now_wall_ms_for_spawner();
        let cancel = PromptCancel::new();
        if let Some(registry) = &scoped.task_registry {
            let snapshot = local_agent_task_snapshot(
                task_id.clone(),
                TaskStatus::Running,
                registry_title.clone(),
                registry_prompt.clone(),
                registry_agent_type.clone(),
                registry_model.clone(),
                registry_system.clone(),
                registry_allowed_tools.clone(),
                start_backgrounded,
                start_time_ms,
                None,
                None,
                None,
                None,
                spec_metadata.clone(),
            );
            registry.insert(task_id.clone(), snapshot, cancel.clone());
        }
        let spawner = EngineSubAgentSpawner {
            engine: self.engine.clone(),
            client: self.client.clone(),
            default_model: self.default_model.clone(),
            base_filter: self.base_filter.clone(),
            model_config: self.model_config.clone(),
            model_profiles: self.model_profiles.clone(),
            model_router: self.model_router.clone(),
            automatic_routes: self.automatic_routes.clone(),
            task_registry_resolver: scoped.task_registry_resolver.clone(),
            task_registry: scoped.task_registry.clone(),
            escalation_registry: scoped.escalation_registry.clone(),
            file_history_tracker: self.file_history_tracker.clone(),
            capability_failures: self.capability_failures.clone(),
            ultraplan_worker_reservations: self.ultraplan_worker_reservations.clone(),
            policy: self.policy.clone(),
            coordinator_mode: self.coordinator_mode.clone(),
            coordinator_use_worktree: self.coordinator_use_worktree,
            external_runner: self.external_runner.clone(),
        };

        tokio::spawn(async move {
            if let Err(err) = spawner.spawn_inner(spec, None).await {
                if let Some(registry) = task_registry {
                    let mut updated_existing = false;
                    registry.update(&task_id, |snap| {
                        updated_existing = true;
                        if !snap.status.is_terminal() {
                            snap.status = TaskStatus::Failed;
                            snap.error = Some(err.clone());
                            snap.last_progress = Some(err.clone());
                            snap.end_time_ms = Some(now_wall_ms_for_spawner());
                        }
                    });
                    if !updated_existing {
                        let end_time_ms = now_wall_ms_for_spawner();
                        let snapshot = local_agent_task_snapshot(
                            task_id.clone(),
                            TaskStatus::Failed,
                            registry_title,
                            registry_prompt,
                            registry_agent_type,
                            registry_model,
                            registry_system,
                            registry_allowed_tools,
                            start_backgrounded,
                            start_time_ms,
                            Some(end_time_ms),
                            Some(err.clone()),
                            Some(err.clone()),
                            Some(serde_json::json!({
                                "status": "failed",
                                "final_text": err.clone(),
                                "duration_ms": end_time_ms.saturating_sub(start_time_ms),
                                "tool_call_count": 0,
                                "total_tokens": 0,
                            })),
                            spec_metadata.clone(),
                        );
                        registry.insert(task_id.clone(), snapshot, PromptCancel::new());
                    }
                }
                tracing::warn!(agent_id = %task_id.as_str(), error = %err, "background sub-agent failed before completion");
            }
        });

        Ok(agent_id)
    }
}
