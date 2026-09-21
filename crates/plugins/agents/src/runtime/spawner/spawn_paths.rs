//! The spawn entry points themselves: the local worker path, the external
//! ACP path, and the persistent actor that keeps a named teammate alive
//! between turns.
//!
//! Split out of `runtime/spawner.rs` for the 10k-line cap; the items are
//! unchanged and in their original order.

use super::*;
impl EngineSubAgentSpawner {
    pub(super) fn spawn_persistent_agent_actor(
        &self,
        task_id: TaskId,
        mut base_spec: SubAgentSpec,
    ) {
        let Some(registry) = self.task_registry.clone() else {
            return;
        };
        if let Some(metadata) = base_spec.metadata.as_object_mut() {
            metadata.insert(PERSISTENT_AGENT_MANAGED_KEY.into(), Value::Bool(true));
        }
        let spawner = self.clone();
        tokio::spawn(async move {
            run_persistent_agent_actor(spawner, registry, task_id, base_spec).await;
        });
    }

    /// The external route for `spec`, if the runner is wired and the
    /// effective model spec names a declared agent.
    pub(super) fn external_route_for(
        &self,
        spec: &SubAgentSpec,
    ) -> Option<(ExternalRoute, Arc<dyn ExternalSubAgentRunner>)> {
        let runner = self.external_runner.clone()?;
        let agent_type = metadata_string(&spec.metadata, "agent_type");
        let category = metadata_category(&spec.metadata);
        let route = external_route_for_spec(
            &self.model_config,
            agent_type.as_deref(),
            category.as_deref(),
            spec.model.as_deref(),
            runner.as_ref(),
        )?;
        if let Some(provider) = spec
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|provider| !provider.is_empty() && !is_inherit_sentinel(provider))
        {
            tracing::warn!(
                provider,
                agent = %route.agent_id,
                "sub-agent provider override ignored: the external agent brings its own provider"
            );
        }
        Some((route, runner))
    }

    /// Run one sub-agent task on its external agent: registry snapshot
    /// in, one `run_task` through the runner, honest terminal state
    /// out. Deliberately skips the local worker machinery — worktrees,
    /// tool filters, terminal delivery hooks, persistent actors — because
    /// none of it can bind on somebody else's process.
    async fn spawn_external_inner(
        &self,
        spec: SubAgentSpec,
        route: ExternalRoute,
        runner: Arc<dyn ExternalSubAgentRunner>,
        agent_id: String,
        agent_type: Option<String>,
        progress: Option<SubAgentProgressSender>,
    ) -> Result<SubAgentResult, String> {
        // Workflow structured output needs a schema-bound final answer
        // the external agent cannot be forced to produce. Refuse loudly
        // rather than returning free text where JSON was promised.
        if spec
            .metadata
            .get(rebon_tool::WORKFLOW_STRUCTURED_OUTPUT_SCHEMA_KEY)
            .filter(|value| !value.is_null())
            .is_some()
        {
            return Err(format!(
                "agent `{}` runs on the external `{}` agent and cannot produce workflow \
                 structured output; use a local agent for schema-bound stages",
                agent_type.as_deref().unwrap_or("general-purpose"),
                route.agent_id
            ));
        }

        let start = Instant::now();
        let start_time_ms = now_wall_ms_for_spawner();
        let task_id = TaskId::new(agent_id.clone());
        let task_list_id = spec
            .task_list_id
            .clone()
            .unwrap_or_else(rebon_tool::tasks::current_task_list_id);
        let registry_model = external_registry_model_label(&route);
        let registry_agent_type = agent_type
            .clone()
            .unwrap_or_else(|| "general-purpose".to_string());
        let registry_title = spec
            .metadata
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                spec.prompt
                    .lines()
                    .next()
                    .map(|s| s.chars().take(120).collect::<String>())
                    .unwrap_or_default()
            });
        let is_coord = self
            .coordinator_mode
            .as_ref()
            .map(|mode| mode.get())
            .unwrap_or_else(rebon_core::coordinator_mode::coordinator_mode_from_env_default);

        let cancel = self
            .task_registry
            .as_ref()
            .and_then(|registry| registry.cancel_handle(&task_id))
            .unwrap_or_else(PromptCancel::new);
        if let Some(registry) = &self.task_registry {
            if registry.snapshot(&task_id).is_none() {
                let snapshot = local_agent_task_snapshot(
                    task_id.clone(),
                    TaskStatus::Running,
                    registry_title.clone(),
                    spec.prompt.clone(),
                    registry_agent_type.clone(),
                    registry_model.clone(),
                    spec.system.clone(),
                    None,
                    spec.run_in_background,
                    start_time_ms,
                    None,
                    None,
                    None,
                    None,
                    spec.metadata.clone(),
                );
                registry.insert(task_id.clone(), snapshot, cancel.clone());
            }
            registry.record_live_event(&task_id, TaskLiveEventKind::Started);
        }
        let turn = match &self.task_registry {
            Some(registry) => Some(registry.begin_task_turn(&task_id).ok_or_else(|| {
                format!("agent {agent_id} already has an active turn or is closed")
            })?),
            None => None,
        };

        tracing::info!(
            agent_id = %agent_id,
            agent = %route.agent_id,
            model_hint = route.model_hint.as_deref().unwrap_or("agent-default"),
            "external sub-agent task starting"
        );

        let streamed_answer = Arc::new(Mutex::new(String::new()));
        let streamed_thinking = Arc::new(Mutex::new(String::new()));
        let saw_streamed_answer = Arc::new(AtomicBool::new(false));
        let progress_sink = self.external_progress_sink(
            &turn,
            progress.clone(),
            &streamed_answer,
            &streamed_thinking,
            &saw_streamed_answer,
        );

        let request = ExternalTaskRequest {
            agent_id: route.agent_id.clone(),
            model_hint: route.model_hint.clone(),
            task_session_id: format!("subagent-{agent_id}"),
            prompt: compose_external_first_prompt(
                spec.system.as_deref(),
                &spec.prompt,
                agent_type.as_deref(),
            ),
            cwd: spec.cwd.clone(),
            cancel: cancel.clone(),
            progress: progress_sink,
            metadata: spec.metadata.clone(),
        };
        let outcome = runner.run_task(request).await;

        let duration_ms = start.elapsed().as_millis() as u64;
        let end_time_ms = now_wall_ms_for_spawner();
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                if let Some(registry) = &self.task_registry {
                    registry.update(&task_id, |snap| {
                        if !snap.status.is_terminal() {
                            snap.status = TaskStatus::Failed;
                            snap.error = Some(err.clone());
                            snap.last_progress = Some(err.clone());
                            snap.end_time_ms = Some(end_time_ms);
                        }
                    });
                    if let Some(turn) = &turn {
                        registry.record_task_turn_terminal_event(
                            turn,
                            TaskLiveEventKind::Finished {
                                status: TaskStatus::Failed,
                                error: Some(err.clone()),
                            },
                        );
                        registry.finish_terminal_task_turn(turn);
                    }
                }
                cleanup_agent_created_tasks(
                    &task_list_id,
                    &agent_id,
                    &spec.metadata,
                    rebon_tool::tasks::TaskListStatus::Pending,
                );
                return Err(format!("external agent `{}` failed: {err}", route.agent_id));
            }
        };

        let (status_str, task_status, error) = match &outcome.status {
            ExternalTaskStatus::Completed => ("completed", TaskStatus::Completed, None),
            ExternalTaskStatus::Cancelled => (
                "cancelled",
                TaskStatus::Killed,
                Some("cancelled".to_string()),
            ),
            ExternalTaskStatus::Failed(err) => ("failed", TaskStatus::Failed, Some(err.clone())),
        };

        // The report-file contract: external agents are not taught the
        // report directive, so the harness writes the report from the
        // final text — the same road Explore workers take.
        let mut output_file = None;
        if is_coord && matches!(outcome.status, ExternalTaskStatus::Completed) {
            let path = report_file_path_for_agent_id(&agent_id);
            match auto_write_report_file(&path, &outcome.final_text) {
                Ok(()) => output_file = Some(path),
                Err(err) => tracing::warn!(
                    path = %path,
                    error = %err,
                    "external sub-agent report file could not be written"
                ),
            }
        }

        if let Some(registry) = &self.task_registry {
            let streamed_answer = saw_streamed_answer.load(Ordering::Acquire);
            if !streamed_answer && !outcome.final_text.trim().is_empty() {
                if let Some(turn) = &turn {
                    registry.record_task_turn_event(
                        turn,
                        TaskLiveEventKind::AssistantTurnComplete {
                            text: outcome.final_text.clone(),
                        },
                    );
                }
            }
            let result_json = serde_json::json!({
                "status": status_str,
                "final_text": outcome.final_text.clone(),
                "output_file": output_file.clone(),
                "duration_ms": duration_ms,
                "tool_call_count": outcome.tool_call_count,
            });
            registry.update(&task_id, |snap| {
                snap.status = task_status;
                snap.error = error.clone();
                snap.end_time_ms = Some(end_time_ms);
                snap.result = Some(result_json.clone());
                if !outcome.final_text.trim().is_empty() {
                    snap.last_progress = Some(outcome.final_text.clone());
                }
                if let TaskData::LocalAgent(data) = &mut snap.data {
                    data.streaming_text = None;
                    if !streamed_answer && !outcome.final_text.trim().is_empty() {
                        push_bounded_agent_transcript(
                            &mut data.transcript,
                            LocalAgentTranscriptEntry::Assistant {
                                text: outcome.final_text.clone(),
                            },
                        );
                    }
                }
            });
            if let Some(turn) = &turn {
                registry.record_task_turn_terminal_event(
                    turn,
                    TaskLiveEventKind::Finished {
                        status: task_status,
                        error: error.clone(),
                    },
                );
                registry.finish_terminal_task_turn(turn);
            }
        }
        tracing::info!(
            agent_id = %agent_id,
            agent = %route.agent_id,
            status = status_str,
            duration_ms,
            tool_calls = outcome.tool_call_count,
            "external sub-agent task finished"
        );
        cleanup_agent_created_tasks(
            &task_list_id,
            &agent_id,
            &spec.metadata,
            child_task_cleanup_status(status_str),
        );

        Ok(SubAgentResult {
            final_text: outcome.final_text,
            status: status_str.to_string(),
            tool_call_count: outcome.tool_call_count,
            sub_agent_tool_calls: None,
            read_file_count: None,
            stop_reason: None,
            output_file,
            duration_ms: Some(duration_ms),
            error,
            agent_id: Some(agent_id),
            agent_type,
            provider: Some(format!("acp:{}", route.agent_id)),
            model: Some(
                route
                    .model_hint
                    .clone()
                    .unwrap_or_else(|| "agent-default".to_string()),
            ),
            // Honest absence, not zero: the agent's token usage is
            // between it and its provider.
            total_tokens: None,
            output_tokens: None,
            usage: Some(serde_json::json!({
                "reported": false,
                "reason": "external ACP agent does not report token usage",
                "agent": route.agent_id,
            })),
            diagnostics: Some(serde_json::json!({
                "external": {
                    "agent": route.agent_id,
                    "modelHint": route.model_hint,
                    "acpSessionId": outcome.acp_session_id,
                    "sessionMode": "created",
                    "writesThroughHostFs": false,
                    "rewindCoverage": "routed-writes-only",
                }
            })),
            git: None,
        })
    }

    pub(super) async fn spawn_inner(
        &self,
        mut spec: SubAgentSpec,
        progress: Option<SubAgentProgressSender>,
    ) -> Result<SubAgentResult, String> {
        self.preflight_spec(&mut spec)?;
        let engine = self
            .engine
            .upgrade()
            .ok_or_else(|| "engine has been dropped".to_string())?;

        // Mint and persist the `agent_id` at the top so we can use it
        // as the TaskRegistry key *before* kicking off the worker. The
        // cloned metadata below is the normalized object snapshots keep.
        let agent_id = ensure_agent_id(&mut spec.metadata);
        let task_kind = spec
            .task_kind
            .unwrap_or_else(|| SubAgentTaskKind::from_metadata(&spec.metadata));
        spec.task_kind = Some(task_kind);
        let agent_type = metadata_string(&spec.metadata, "agent_type");

        // External route: the whole task runs on a declared ACP agent.
        // Decided before the persistent-actor and worktree machinery —
        // none of it applies to somebody else's process — and before
        // the model router, which would forward an unrecognised
        // `id:model` spec to the local provider as a literal model id.
        if let Some((route, runner)) = self.external_route_for(&spec) {
            return self
                .spawn_external_inner(spec, route, runner, agent_id, agent_type, progress)
                .await;
        }

        // The sub-agent lifecycle pair. Raised after the external route is
        // settled, so the id and type in them name a worker that is really
        // about to run here, and before any of its work.
        self.emit_subagent_event(rebon_core::policy_seat::HookEventPayload::SubagentStart {
            agent_id: agent_id.clone(),
            agent_type: agent_type.clone().unwrap_or_default(),
            task: Some(spec.prompt.clone()),
        })
        .await;

        let WorkerPlacement {
            should_spawn_persistent_actor,
            keep_runtime_resumable,
            scratchpad_dir,
            scratchpad_root,
            is_verification_agent,
            is_coord,
            mut git_metadata,
        } = self.resolve_worker_placement(&mut spec, &agent_id, &agent_type, task_kind)?;
        self.preflight_spec(&mut spec)?;
        let structured_output_schema = spec
            .metadata
            .get(rebon_tool::WORKFLOW_STRUCTURED_OUTPUT_SCHEMA_KEY)
            .filter(|value| !value.is_null())
            .cloned();
        if let Some(obj) = spec.metadata.as_object_mut() {
            obj.remove("__runtime_git");
            obj.remove(rebon_tool::WORKFLOW_STRUCTURED_OUTPUT_SCHEMA_KEY);
        }
        let spec_metadata = spec.metadata.clone();
        let WorkerRegistryPlan {
            persistent_actor_spec,
            structured_output_channel,
            registry_agent_type,
            registry_title,
            registry_prompt,
            requested_model_profile,
            resolved_runtime,
            registry_model,
            registry_model_for_snapshot,
            registry_provider,
            reasoning_effort,
            registry_run_in_background,
            permission_prompts_unavailable,
        } = self
            .resolve_worker_registry_plan(
                &spec,
                &spec_metadata,
                &agent_id,
                &agent_type,
                &git_metadata,
                structured_output_schema,
                should_spawn_persistent_actor,
            )
            .await?;

        let WorkerRunPlan {
            auto_report_from_final_text,
            tool_filter,
            effective_cache_strategy,
            cache_context_policy,
            system,
            report_file_path,
            prompt,
        } = self.build_worker_run_plan(
            &spec,
            &spec_metadata,
            &agent_id,
            &agent_type,
            &scratchpad_dir,
            is_coord,
        );

        // A follow-up turn on an idle worker reuses the same report path,
        // and the previous turn's report is structurally valid — so
        // structure alone cannot tell "wrote a fresh report" from "left
        // last turn's report in place". Remember what was there before
        // the turn so an unchanged file counts as no deliverable.
        let pre_turn_report_stamp = report_file_path.as_deref().and_then(report_file_stamp);

        let WorkerTurnContracts {
            validator_lifecycle,
            turn_hook,
        } = self.build_worker_turn_contracts(
            &report_file_path,
            &structured_output_channel,
            pre_turn_report_stamp,
            task_kind,
            auto_report_from_final_text,
        );

        let WorkerToolContext {
            child_context,
            task_id,
            task_list_id,
        } = self.build_worker_tool_context(
            &spec,
            &engine,
            &agent_id,
            &registry_title,
            &report_file_path,
            &scratchpad_root,
            &git_metadata,
            &structured_output_channel,
            is_verification_agent,
            auto_report_from_final_text,
            permission_prompts_unavailable,
            registry_run_in_background,
        );
        let WorkerLaunchSpec {
            start_time_ms,
            session,
            registry_system,
            registry_allowed_tools,
            worker_spec,
        } = self.assemble_worker_launch_spec(
            &spec,
            &engine,
            &resolved_runtime,
            &agent_id,
            &task_id,
            &registry_model,
            &registry_provider,
            requested_model_profile,
            prompt,
            system,
            tool_filter,
            child_context,
            turn_hook,
            effective_cache_strategy,
            cache_context_policy,
            reasoning_effort,
            is_coord,
        );

        let cancel = self
            .task_registry
            .as_ref()
            .and_then(|registry| registry.cancel_handle(&task_id))
            .unwrap_or_else(PromptCancel::new);

        let WorkerLaunch {
            start,
            handle,
            turn,
            background_request,
        } = self.launch_worker(
            engine,
            session,
            worker_spec,
            cancel,
            persistent_actor_spec,
            &agent_id,
            &agent_type,
            &spec_metadata,
            &task_id,
            registry_agent_type,
            registry_title,
            registry_prompt,
            registry_model_for_snapshot,
            &registry_model,
            &registry_provider,
            registry_system,
            registry_allowed_tools,
            start_time_ms,
            registry_run_in_background,
        )?;
        tracing::info!(
            agent_id = %agent_id,
            background = registry_run_in_background,
            "sub-agent worker wait starting"
        );
        let (mut result, detached) = wait_worker_and_mirror_progress_detachable(
            handle,
            self.task_registry.clone(),
            task_id.clone(),
            turn.clone(),
            progress,
            background_request,
            !registry_run_in_background,
            keep_runtime_resumable,
        )
        .await;
        tracing::info!(
            agent_id = %agent_id,
            status = ?result.status,
            detached,
            error = ?result.error,
            "sub-agent worker wait finished"
        );
        if detached {
            // No `SubagentStop`: the call is over, the worker is not. The
            // detached run reports its own end through the task registry.
            return Ok(detached_launch_result(
                &spec,
                start,
                agent_id,
                agent_type,
                &registry_provider,
                &registry_model,
                git_metadata,
            ));
        }
        self.emit_subagent_event(rebon_core::policy_seat::HookEventPayload::SubagentStop {
            agent_id: agent_id.clone(),
            agent_type: agent_type.clone().unwrap_or_default(),
            stop_reason: result.error.clone(),
        })
        .await;
        let duration_ms = start.elapsed().as_millis() as u64;
        self.settle_worker_aftermath(
            &result,
            &agent_id,
            &report_file_path,
            task_kind,
            auto_report_from_final_text,
        )?;

        let WorkerReportVerdict {
            report_validation,
            report_invalid,
            mut status,
        } = self.judge_worker_report(&result, &report_file_path, pre_turn_report_stamp, task_kind);

        finalize_agent_worktree_integration(&mut result, &mut git_metadata, &mut status, &agent_id);

        Ok(self.finish_worker_run(
            result,
            &spec,
            &spec_metadata,
            status,
            agent_id,
            agent_type,
            &task_id,
            &task_list_id,
            registry_model,
            registry_provider,
            git_metadata,
            report_file_path,
            report_invalid,
            report_validation,
            duration_ms,
            keep_runtime_resumable,
            turn,
            structured_output_channel,
            validator_lifecycle,
        ))
    }
}
