//! The run half of the worker pipeline: launching the worker, bridging its
//! events into the task registry, judging the report it was obliged to write,
//! and closing its books.
//!
//! Split out of `runtime/spawner.rs` for the 10k-line cap; the items are
//! unchanged and in their original order.

use super::*;
impl EngineSubAgentSpawner {
    /// Judge the report file the worker was told to write.
    ///
    /// The worker delivery hook already gave the worker one coercion retry
    /// inside the query loop. If the file is still missing or structurally
    /// invalid here the status is downgraded to failed, so the coordinator
    /// gets an honest answer instead of a `completed` pointing at an
    /// unusable `output_file`.
    pub(super) fn judge_worker_report(
        &self,
        result: &WorkerResult,
        report_file_path: &Option<String>,
        pre_turn_report_stamp: Option<ReportFileStamp>,
        task_kind: SubAgentTaskKind,
    ) -> WorkerReportVerdict {
        // Final report-file validation. The delivery hook already
        // gave the worker one coercion retry inside the query loop.
        // If the file is still missing or structurally invalid here,
        // downgrade to "failed" so the coordinator gets an honest
        // status instead of a "completed" pointing at an invalid
        // output_file.
        let report_validation = if let Some(ref path) = report_file_path {
            if result.status == WorkerStatus::Completed {
                let validation = stale_aware_report_validation(
                    path,
                    task_kind,
                    self.coordinator_use_worktree,
                    pre_turn_report_stamp,
                );
                if !validation.ok {
                    tracing::warn!(
                        path = %path,
                        reason = %validation.failure_summary(),
                        "worker completed without a structurally valid report file \
                         (coercion retry was attempted)"
                    );
                }
                Some(validation)
            } else {
                None
            }
        } else {
            None
        };
        let report_invalid = report_validation
            .as_ref()
            .is_some_and(|validation| !validation.ok);

        let status = if report_invalid {
            "failed".to_string()
        } else {
            match result.status {
                WorkerStatus::Completed => "completed",
                WorkerStatus::Cancelled => "cancelled",
                WorkerStatus::Failed => "failed",
                WorkerStatus::Running => "running",
            }
            .to_string()
        };
        WorkerReportVerdict {
            report_validation,
            report_invalid,
            status,
        }
    }
}

impl EngineSubAgentSpawner {
    /// Assemble the `WorkerSpec` and the forked session it runs on.
    ///
    /// This is where the pieces stop being decisions and become one value:
    /// the prompt, the system prompt, the toolkit and the tool context are all
    /// moved into `WorkerSpec` here, and the two the registry snapshot still
    /// needs are cloned out first.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn assemble_worker_launch_spec(
        &self,
        spec: &SubAgentSpec,
        engine: &std::sync::Arc<rebon_core::Engine>,
        resolved_runtime: &rebon_agent_core::model_router::ResolvedModelRuntime,
        agent_id: &str,
        task_id: &TaskId,
        registry_model: &String,
        registry_provider: &String,
        requested_model_profile: Option<&str>,
        prompt: String,
        system: Option<String>,
        tool_filter: Option<ToolFilter>,
        child_context: rebon_tool::ToolContext,
        turn_hook: Option<Arc<dyn TurnHook>>,
        effective_cache_strategy: rebon_tool::CacheStrategy,
        cache_context_policy: Option<String>,
        reasoning_effort: Option<ReasoningEffort>,
        is_coord: bool,
    ) -> WorkerLaunchSpec {
        let start_time_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let attachment_poller = self.task_registry.as_ref().map(|registry| {
            let poller = Arc::new(LocalAgentPendingMessagePoller::new(
                registry.clone(),
                task_id.clone(),
            )) as Arc<dyn AttachmentPoller>;
            let query_id = task_id.to_string();
            AttachmentPollerBinding::new(poller, query_id.clone(), query_id)
        });
        let messages = build_worker_messages(spec, prompt);
        let trace_tools = match tool_filter.as_ref() {
            Some(filter) => filtered_tools_from_engine(engine, filter),
            None => tools_from_engine(engine),
        };
        let trace_tools_hash = rebon_api::tools_hash(&trace_tools);
        let trace_schema_hash = rebon_api::schema_hash(&trace_tools);
        let trace_tools_tokens = estimate_trace_tools_tokens(&trace_tools);
        let cache_trace_context = build_worker_cache_trace_context(
            effective_cache_strategy,
            cache_context_policy,
            registry_provider,
            registry_model,
            requested_model_profile,
            system.as_deref(),
            Some(cache_api_path(resolved_runtime.client.provider_name())),
            trace_tools_hash.clone(),
            trace_schema_hash.clone(),
            spec.frozen_parent_context.as_ref(),
            &messages,
        );
        let child_prompt_cache_key = cache_trace_context.prompt_cache_key.clone();
        // The resolved runtime's client is owned by whoever resolved it
        // (the main session, or the provider runtime cache), so the
        // parent handle here is borrowed. Forking off it yields an
        // owned child when the client can isolate itself, and a
        // borrowed one when it cannot — which is what stops a
        // sub-agent's `endTurn` from resetting a shared provider
        // connection out from under its siblings.
        let parent_session = SessionHandle::borrowed(resolved_runtime.client.clone());
        let session = parent_session.fork_for_sub_agent(child_prompt_cache_key);
        let prune_level = session.context_prune_handle();
        let registry_system = system.clone();
        let registry_allowed_tools = tool_filter.as_ref().and_then(ToolFilter::allow_list);
        let worker_spec = WorkerSpec {
            messages,
            model: registry_model.clone(),
            system,
            tool_filter,
            max_iterations: effective_worker_max_iterations(spec.max_iterations, is_coord),
            max_tokens: crate::runtime::worker::worker_max_tokens_for_client(
                resolved_runtime.client.as_ref(),
            ),
            tool_context: child_context,
            turn_hook,
            // Tagged with the agent so a subscriber can tell delegated
            // work from the session's own; the subscribers themselves are
            // the session's, which is what puts a user's guards in front
            // of a sub-agent's tool calls for the first time.
            policy: self.policy.clone().in_agent(agent_id),
            attachment_poller,
            reasoning_effort,
            prune_level,
            execution_policy: spec.execution_policy.clone(),
            capability_context: spec.capability_context.clone(),
            tools_hash: Some(trace_tools_hash),
            schema_hash: Some(trace_schema_hash),
            tools_token_estimate: Some(trace_tools_tokens),
            cache_trace_context: Some(cache_trace_context),
        };
        WorkerLaunchSpec {
            start_time_ms,
            session,
            registry_system,
            registry_allowed_tools,
            worker_spec,
        }
    }
}

impl EngineSubAgentSpawner {
    /// Start the worker and file it everywhere it has to be visible.
    ///
    /// Registration happens only after `spawn_worker` returns a handle:
    /// spawning can fail during tool filtering (an empty allowed-tools
    /// intersection, say), and inserting first would leave a permanent
    /// `Running` snapshot with no worker left to update it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn launch_worker(
        &self,
        engine: std::sync::Arc<rebon_core::Engine>,
        session: Arc<SessionHandle>,
        worker_spec: WorkerSpec,
        cancel: PromptCancel,
        persistent_actor_spec: Option<SubAgentSpec>,
        agent_id: &String,
        agent_type: &Option<String>,
        spec_metadata: &Value,
        task_id: &TaskId,
        registry_agent_type: String,
        registry_title: String,
        registry_prompt: String,
        registry_model_for_snapshot: String,
        registry_model: &String,
        registry_provider: &String,
        registry_system: Option<String>,
        registry_allowed_tools: Option<Vec<String>>,
        start_time_ms: u64,
        registry_run_in_background: bool,
    ) -> Result<WorkerLaunch, String> {
        // Register the sub-agent as a LocalAgent task only after the
        // worker handle exists. `spawn_worker` can fail during tool
        // filtering (for example, an empty allowed-tools intersection);
        // inserting before that point would leave a permanent Running
        // snapshot with no worker left to update it.
        let start = Instant::now();
        tracing::info!(
            agent_id = %agent_id,
            agent_type = agent_type.as_deref().unwrap_or("general-purpose"),
            provider = %registry_provider,
            model = %registry_model,
            "sub-agent worker spawning"
        );
        let handle = spawn_worker(engine, session, worker_spec, cancel.clone())
            .map_err(|err| err.to_string())?;
        tracing::info!(agent_id = %agent_id, "sub-agent worker spawned");
        if let Some(registry) = &self.task_registry {
            if registry.snapshot(task_id).is_none() {
                let snapshot = local_agent_task_snapshot(
                    task_id.clone(),
                    TaskStatus::Running,
                    registry_title.clone(),
                    registry_prompt,
                    registry_agent_type,
                    registry_model_for_snapshot,
                    registry_system,
                    registry_allowed_tools,
                    registry_run_in_background,
                    start_time_ms,
                    None,
                    None,
                    None,
                    None,
                    spec_metadata.clone(),
                );
                registry.insert(task_id.clone(), snapshot, cancel.clone());
            }
            registry.record_live_event(task_id, TaskLiveEventKind::Started);
        }
        let turn = match &self.task_registry {
            Some(registry) => Some(registry.begin_task_turn(task_id).ok_or_else(|| {
                format!("agent {agent_id} already has an active turn or is closed")
            })?),
            None => None,
        };
        if let Some(actor_spec) = persistent_actor_spec {
            self.spawn_persistent_agent_actor(task_id.clone(), actor_spec);
        }
        let background_request = if registry_run_in_background {
            None
        } else {
            self.task_registry
                .as_ref()
                .and_then(|registry| registry.background_request_receiver(task_id))
        };
        Ok(WorkerLaunch {
            start,
            handle,
            turn,
            background_request,
        })
    }
}

impl EngineSubAgentSpawner {
    /// Raise one sub-agent lifecycle event on the session's subscribers.
    ///
    /// Both are notification events: nothing here has a "do not start" or
    /// "do not stop" branch, so the effects a subscriber returns are logged
    /// rather than applied. They are tagged with the agent so a subscriber
    /// that filters by agent sees the pair it is filtering for.
    pub(super) async fn emit_subagent_event(
        &self,
        payload: rebon_core::policy_seat::HookEventPayload,
    ) {
        let agent_id = match &payload {
            rebon_core::policy_seat::HookEventPayload::SubagentStart { agent_id, .. }
            | rebon_core::policy_seat::HookEventPayload::SubagentStop { agent_id, .. } => {
                agent_id.clone()
            }
            other => {
                debug_assert!(false, "not a sub-agent lifecycle event: {other:?}");
                return;
            }
        };
        let kind = payload.event();
        let verdict = self.policy.clone().in_agent(agent_id).emit(payload).await;
        for effect in verdict.effects() {
            tracing::debug!(
                event = %kind.name(),
                effect = ?effect,
                "sub-agent lifecycle hook effect ignored: neither event has a branch to apply one to"
            );
        }
    }
}

impl EngineSubAgentSpawner {
    /// Compose the delivery rules this worker answers to.
    ///
    /// A coordinator worker owes a report file; a workflow agent owes a
    /// schema-valid `StructuredOutput` call. They are composed rather than
    /// chosen so neither contract shadows the other when both apply.
    pub(super) fn build_worker_turn_contracts(
        &self,
        report_file_path: &Option<String>,
        structured_output_channel: &Option<Arc<rebon_tool::StructuredOutputChannel>>,
        pre_turn_report_stamp: Option<ReportFileStamp>,
        task_kind: SubAgentTaskKind,
        auto_report_from_final_text: bool,
    ) -> WorkerTurnContracts {
        let validator_lifecycle = Arc::new(ValidatorLifecycleDiagnostics::default());
        let mut rules = Vec::new();
        if !auto_report_from_final_text {
            if let Some(path) = report_file_path.as_ref() {
                rules.push(WorkerDeliveryRule::Report(ReportFileContract::new(
                    path.clone(),
                    task_kind,
                    self.coordinator_use_worktree,
                    pre_turn_report_stamp,
                    validator_lifecycle.clone(),
                )));
            }
        }
        // Workflow agents return via the `StructuredOutput` tool. If the
        // worker ends its turn without a schema-valid call, coerce it once
        // with the schema in hand (RFC workflow-v2 §1.5). Compose this with
        // the report validator instead of letting either contract shadow the
        // other.
        if let Some(channel) = structured_output_channel.as_ref() {
            rules.push(WorkerDeliveryRule::StructuredOutput(
                StructuredOutputContract::new(channel.clone(), validator_lifecycle.clone()),
            ));
        }
        let turn_hook = worker_delivery_hook(rules);
        WorkerTurnContracts {
            validator_lifecycle,
            turn_hook,
        }
    }
}

impl EngineSubAgentSpawner {
    /// Settle what the run leaves behind before its books are closed.
    ///
    /// A worker that died with questions still escalated has to release them,
    /// and an Explore-style worker that never writes its own report gets one
    /// written from its final text.
    pub(super) fn settle_worker_aftermath(
        &self,
        result: &WorkerResult,
        agent_id: &String,
        report_file_path: &Option<String>,
        task_kind: SubAgentTaskKind,
        auto_report_from_final_text: bool,
    ) -> Result<(), String> {
        if matches!(
            result.status,
            WorkerStatus::Cancelled | WorkerStatus::Failed
        ) {
            self.escalation_registry.cancel_agent(
                agent_id,
                "worker terminated before pending question escalations were resolved",
            );
        }

        if auto_report_from_final_text && result.status == WorkerStatus::Completed {
            if let Some(ref path) = report_file_path {
                let validation =
                rebon_core::coordinator_mode::validate_worker_report_file_for_task_kind_with_options(
                    path,
                    Some(task_kind.as_str()),
                    self.coordinator_use_worktree,
                );
                if !validation.ok {
                    auto_write_report_file(path, &result.final_text).map_err(|err| {
                    format!(
                        "Worker completed but the harness could not write the report file at {path}: {err}"
                    )
                })?;
                }
            }
        }
        Ok(())
    }
}

impl EngineSubAgentSpawner {
    /// Close the run out into the result the caller gets back.
    ///
    /// Accounting, registry bookkeeping and the returned `SubAgentResult` are
    /// one phase because they read the same finished `result` and have to
    /// agree on it: the status the snapshot records is the status the caller
    /// is told, and both are decided after the report verdict.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_worker_run(
        &self,
        result: WorkerResult,
        spec: &SubAgentSpec,
        spec_metadata: &Value,
        status: String,
        agent_id: String,
        agent_type: Option<String>,
        task_id: &TaskId,
        task_list_id: &String,
        registry_model: String,
        registry_provider: String,
        git_metadata: Option<SubAgentGitMetadata>,
        report_file_path: Option<String>,
        report_invalid: bool,
        report_validation: Option<rebon_core::coordinator_mode::WorkerReportValidation>,
        duration_ms: u64,
        keep_runtime_resumable: bool,
        turn: Option<TaskTurnToken>,
        structured_output_channel: Option<Arc<rebon_tool::StructuredOutputChannel>>,
        validator_lifecycle: Arc<ValidatorLifecycleDiagnostics>,
    ) -> SubAgentResult {
        let WorkerAccounting {
            total_tokens,
            usage_json,
            error,
            output_file,
        } = worker_accounting(
            &result,
            report_file_path,
            report_invalid,
            report_validation.clone(),
        );

        let WorkerClosingState {
            diagnostics,
            sub_agent_tool_calls,
        } = self.close_worker_books(
            &WorkerClosingInputs {
                task_id,
                task_list_id,
                agent_id: &agent_id,
                status: &status,
                error: &error,
                output_file: &output_file,
                duration_ms,
                total_tokens,
                usage_json: &usage_json,
                registry_model: &registry_model,
                registry_provider: &registry_provider,
                git_metadata: &git_metadata,
                keep_runtime_resumable,
                report_invalid,
            },
            &result,
            spec,
            spec_metadata,
            turn,
            report_validation,
            structured_output_channel,
            validator_lifecycle,
        );

        SubAgentResult {
            final_text: result.final_text,
            status: status.clone(),
            tool_call_count: result.tool_calls.len(),
            sub_agent_tool_calls: Some(sub_agent_tool_calls.clone()),
            read_file_count: None,
            stop_reason: result.stop_reason.map(|r| format!("{r:?}")),
            output_file,
            duration_ms: Some(duration_ms),
            error: error.clone(),
            agent_id: Some(agent_id),
            agent_type,
            provider: Some(registry_provider),
            model: Some(registry_model),
            total_tokens: Some(total_tokens),
            output_tokens: Some(result.cumulative_output_tokens),
            usage: Some(usage_json),
            diagnostics: Some(diagnostics),
            git: git_metadata,
        }
    }
}
