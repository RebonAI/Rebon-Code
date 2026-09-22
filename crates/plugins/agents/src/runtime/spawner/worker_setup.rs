//! The assembly half of the worker pipeline: the tool context a worker runs
//! in, the prompt, toolkit and cache policy it runs under, where it runs, and
//! how it is filed in the task registry.
//!
//! Split out of `runtime/spawner.rs` for the 10k-line cap; the items are
//! unchanged and in their original order.

use super::*;

#[cfg(test)]
#[path = "routing_tests.rs"]
mod routing_tests;
impl EngineSubAgentSpawner {
    /// The live progress sink an external agent's run reports through.
    ///
    /// One closure, called from the runner's thread for every event the agent
    /// emits. It owns clones of the shared buffers rather than borrowing them,
    /// because the runner outlives this call: the answer and thinking strings
    /// accumulate across events, and `saw_streamed_answer` is what later tells
    /// the caller whether the final text was already shown.
    pub(super) fn external_progress_sink(
        &self,
        turn: &Option<TaskTurnToken>,
        progress: Option<SubAgentProgressSender>,
        streamed_answer: &Arc<Mutex<String>>,
        streamed_thinking: &Arc<Mutex<String>>,
        saw_streamed_answer: &Arc<AtomicBool>,
    ) -> Option<Arc<dyn Fn(ExternalTaskEvent) + Send + Sync>> {
        let progress_sink: Option<Arc<dyn Fn(ExternalTaskEvent) + Send + Sync>> = {
            let registry = self.task_registry.clone();
            let sink_turn = turn.clone();
            let streamed_answer = streamed_answer.clone();
            let streamed_thinking = streamed_thinking.clone();
            let saw_streamed_answer = saw_streamed_answer.clone();
            Some(Arc::new(move |event: ExternalTaskEvent| match event {
                ExternalTaskEvent::AgentText(delta) => {
                    if delta.is_empty() {
                        return;
                    }
                    streamed_thinking.lock().unwrap().clear();
                    saw_streamed_answer.store(true, Ordering::Release);
                    let snapshot = {
                        let mut text = streamed_answer.lock().unwrap();
                        text.push_str(&delta);
                        text.clone()
                    };
                    if let Some(registry) = &registry {
                        if let Some(turn) = &sink_turn {
                            registry.update_task_turn(turn, |snap| {
                                snap.last_progress = Some(snapshot.clone());
                                if let TaskData::LocalAgent(data) = &mut snap.data {
                                    data.streaming_text = Some(snapshot.clone());
                                    match data.transcript.last_mut() {
                                        Some(LocalAgentTranscriptEntry::Assistant { text }) => {
                                            text.push_str(&delta);
                                        }
                                        _ => push_bounded_agent_transcript(
                                            &mut data.transcript,
                                            LocalAgentTranscriptEntry::Assistant {
                                                text: delta.clone(),
                                            },
                                        ),
                                    }
                                }
                            });
                            registry.record_task_turn_event(
                                turn,
                                TaskLiveEventKind::AssistantTextDelta { delta, snapshot },
                            );
                        }
                    }
                }
                ExternalTaskEvent::Thinking(delta) => {
                    if delta.is_empty() {
                        return;
                    }
                    streamed_answer.lock().unwrap().clear();
                    let (snapshot, continues_thinking) = {
                        let mut text = streamed_thinking.lock().unwrap();
                        let continues_thinking = !text.is_empty();
                        text.push_str(&delta);
                        (text.clone(), continues_thinking)
                    };
                    if let Some(registry) = &registry {
                        if let Some(turn) = &sink_turn {
                            registry.update_task_turn(turn, |snap| {
                                snap.last_progress = Some(format!("Thinking: {snapshot}"));
                                if let TaskData::LocalAgent(data) = &mut snap.data {
                                    if continues_thinking {
                                        upsert_bounded_agent_thinking(
                                            &mut data.transcript,
                                            snapshot.clone(),
                                        );
                                    } else {
                                        push_bounded_agent_transcript(
                                            &mut data.transcript,
                                            LocalAgentTranscriptEntry::Thinking {
                                                text: snapshot.clone(),
                                            },
                                        );
                                    }
                                }
                            });
                            registry.record_task_turn_event(
                                turn,
                                TaskLiveEventKind::ThinkingDelta { delta, snapshot },
                            );
                        }
                    }
                }
                ExternalTaskEvent::ThinkingEnd => {
                    streamed_thinking.lock().unwrap().clear();
                    if let Some(registry) = &registry {
                        if let Some(turn) = &sink_turn {
                            registry.record_task_turn_event(turn, TaskLiveEventKind::ThinkingEnd);
                        }
                    }
                }
                ExternalTaskEvent::ToolCall {
                    tool_call_id,
                    title,
                    input,
                } => {
                    streamed_thinking.lock().unwrap().clear();
                    streamed_answer.lock().unwrap().clear();
                    if let Some(progress) = &progress {
                        progress.emit_activity(title.clone());
                    }
                    if let Some(registry) = &registry {
                        if let Some(turn) = &sink_turn {
                            let input = input.unwrap_or(Value::Null);
                            registry.update_task_turn(turn, |snap| {
                                snap.last_progress = Some(title.clone());
                                if let TaskData::LocalAgent(data) = &mut snap.data {
                                    data.tool_use_count = data.tool_use_count.saturating_add(1);
                                    push_bounded_agent_transcript(
                                        &mut data.transcript,
                                        LocalAgentTranscriptEntry::ToolStart {
                                            tool_use_id: tool_call_id.clone(),
                                            name: title.clone(),
                                            input: input.clone(),
                                            activity: title.clone(),
                                        },
                                    );
                                }
                            });
                            registry.record_task_turn_event(
                                turn,
                                TaskLiveEventKind::ToolStart {
                                    tool_use_id: tool_call_id,
                                    name: title,
                                    input,
                                },
                            );
                        }
                    }
                }
                ExternalTaskEvent::ToolCallUpdate {
                    tool_call_id,
                    title,
                    status,
                    output,
                } => {
                    streamed_thinking.lock().unwrap().clear();
                    streamed_answer.lock().unwrap().clear();
                    let status_label = status.map(|status| match status {
                        ToolCallStatus::Pending => "pending",
                        ToolCallStatus::InProgress => "in progress",
                        ToolCallStatus::Completed => "completed",
                        ToolCallStatus::Failed => "failed",
                    });
                    let activity = status_label
                        .map(|status| format!("{title}: {status}"))
                        .unwrap_or_else(|| title.clone());
                    if let Some(progress) = &progress {
                        progress.emit_activity(activity.clone());
                    }
                    if let Some(registry) = &registry {
                        if let Some(turn) = &sink_turn {
                            registry.update_task_turn(turn, |snap| {
                                snap.last_progress = Some(activity.clone());
                                if let TaskData::LocalAgent(data) = &mut snap.data {
                                    match status {
                                        Some(ToolCallStatus::Completed) => {
                                            let outcome = Ok(output.clone().unwrap_or(Value::Null));
                                            push_bounded_agent_transcript(
                                                &mut data.transcript,
                                                LocalAgentTranscriptEntry::ToolFinish {
                                                    tool_use_id: tool_call_id.clone(),
                                                    name: title.clone(),
                                                    ok: true,
                                                    summary: activity.clone(),
                                                    outcome,
                                                },
                                            );
                                        }
                                        Some(ToolCallStatus::Failed) => {
                                            let error = output
                                                .as_ref()
                                                .map(Value::to_string)
                                                .unwrap_or_else(|| "tool failed".to_string());
                                            push_bounded_agent_transcript(
                                                &mut data.transcript,
                                                LocalAgentTranscriptEntry::ToolFinish {
                                                    tool_use_id: tool_call_id.clone(),
                                                    name: title.clone(),
                                                    ok: false,
                                                    summary: activity.clone(),
                                                    outcome: Err(error),
                                                },
                                            );
                                        }
                                        _ => push_bounded_agent_transcript(
                                            &mut data.transcript,
                                            LocalAgentTranscriptEntry::ToolProgress {
                                                tool_use_id: tool_call_id.clone(),
                                                name: title.clone(),
                                                message: status_label
                                                    .unwrap_or("updated")
                                                    .to_string(),
                                            },
                                        ),
                                    }
                                }
                            });
                            let live_event = match status {
                                Some(ToolCallStatus::Completed) => TaskLiveEventKind::ToolFinish {
                                    tool_use_id: tool_call_id,
                                    name: title,
                                    outcome: Ok(output.unwrap_or(Value::Null)),
                                },
                                Some(ToolCallStatus::Failed) => TaskLiveEventKind::ToolFinish {
                                    tool_use_id: tool_call_id,
                                    name: title,
                                    outcome: Err(output
                                        .map(|value| value.to_string())
                                        .unwrap_or_else(|| "tool failed".to_string())),
                                },
                                _ => TaskLiveEventKind::ToolProgress {
                                    tool_use_id: tool_call_id,
                                    name: title,
                                    message: status_label.unwrap_or("updated").to_string(),
                                },
                            };
                            registry.record_task_turn_event(turn, live_event);
                        }
                    }
                }
            }))
        };
        progress_sink
    }
}

impl EngineSubAgentSpawner {
    /// Close the worker's books.
    ///
    /// Record the final state into the `TaskRegistry` snapshot so `/tasks`
    /// shows the terminal status, error and end time, and the detail view can
    /// render a structured `result` payload. The bits it needs are captured
    /// before `result` is consumed by the return below.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn close_worker_books(
        &self,
        inputs: &WorkerClosingInputs<'_>,
        result: &WorkerResult,
        spec: &SubAgentSpec,
        spec_metadata: &serde_json::Value,
        turn: Option<TaskTurnToken>,
        report_validation: Option<rebon_core::coordinator_mode::WorkerReportValidation>,
        structured_output_channel: Option<Arc<rebon_tool::StructuredOutputChannel>>,
        validator_lifecycle: Arc<ValidatorLifecycleDiagnostics>,
    ) -> WorkerClosingState {
        let WorkerClosingInputs {
            task_id,
            task_list_id,
            agent_id,
            status,
            error,
            output_file,
            duration_ms,
            total_tokens,
            usage_json,
            registry_model,
            registry_provider,
            git_metadata,
            keep_runtime_resumable,
            report_invalid,
        } = *inputs;
        // Record the final state into the TaskRegistry snapshot so
        // `/tasks` shows the terminal status/error/end time and the
        // detail view can render a structured `result` payload.
        // Capture the bits we need before `result` is consumed by
        // the `Ok(SubAgentResult { ... })` return below.
        let terminal_last_progress = result.final_text.clone();
        let terminal_tool_use_count = result.tool_calls.len() as u64;
        let sub_agent_tool_calls = worker_tool_calls_json(result);
        let diagnostics = sub_agent_diagnostics(
            spec,
            structured_output_channel.as_ref(),
            report_validation.as_ref(),
            &sub_agent_tool_calls,
            result.context_reset_occurred,
            status,
            error.as_deref(),
            validator_lifecycle.as_ref(),
        );
        let structured_output = structured_output_channel
            .as_ref()
            .and_then(|channel| channel.accepted());
        if let Some(registry) = &self.task_registry {
            let terminal_status = if report_invalid {
                TaskStatus::Failed
            } else {
                match result.status {
                    WorkerStatus::Completed => TaskStatus::Completed,
                    WorkerStatus::Cancelled => TaskStatus::Killed,
                    WorkerStatus::Failed => TaskStatus::Failed,
                    // Shouldn't happen after `wait()` — treat as failure.
                    WorkerStatus::Running => TaskStatus::Failed,
                }
            };
            let terminal_error = error.clone();
            let registry_result_json = serde_json::json!({
                "status": status,
                "final_text": terminal_last_progress.clone(),
                "output_file": output_file.clone(),
                "duration_ms": duration_ms,
                "tool_call_count": terminal_tool_use_count,
                "sub_agent_tool_calls": sub_agent_tool_calls.clone(),
                "subAgentToolCalls": sub_agent_tool_calls.clone(),
                "provider": registry_provider.clone(),
                "model": registry_model.clone(),
                "total_tokens": total_tokens,
                "usage": usage_json.clone(),
                "structured_output": structured_output.clone(),
                "diagnostics": diagnostics.clone(),
                "capability": spec.capability_context.as_ref().map(|context| serde_json::json!({
                    "run_id": context.run_id,
                    "ledger_revision": context.ledger_revision,
                    "requirements_hash": context.requirements_hash,
                    "capability_hash": context.capability_hash,
                    "session_id": context.session_id,
                })),
                "git": git_metadata.as_ref().map(SubAgentGitMetadata::to_json),
            });
            let update = |snap: &mut TaskSnapshot| {
                if snap.status == TaskStatus::Killed {
                    return;
                }
                snap.status = if keep_runtime_resumable {
                    TaskStatus::Running
                } else {
                    terminal_status
                };
                snap.error = terminal_error;
                snap.result = Some(registry_result_json);
                snap.last_progress = Some(terminal_last_progress.clone());
                snap.end_time_ms = (!keep_runtime_resumable).then(now_wall_ms_for_spawner);
                if let TaskData::LocalAgent(data) = &mut snap.data {
                    data.tool_use_count = terminal_tool_use_count;
                    data.token_count = total_tokens;
                }
            };
            if let Some(turn) = turn.as_ref() {
                let updated = registry.update_task_turn(turn, update);
                if keep_runtime_resumable {
                    if updated {
                        registry.record_task_turn_event(
                            turn,
                            TaskLiveEventKind::Finished {
                                status: TaskStatus::Running,
                                error: error.clone(),
                            },
                        );
                    }
                    registry.finish_task_turn(turn);
                } else {
                    if updated {
                        registry.record_task_turn_terminal_event(
                            turn,
                            TaskLiveEventKind::Finished {
                                status: terminal_status,
                                error: error.clone(),
                            },
                        );
                    }
                    registry.finish_terminal_task_turn(turn);
                }
            } else {
                registry.update(task_id, update);
                registry.record_live_event(
                    task_id,
                    TaskLiveEventKind::Finished {
                        status: terminal_status,
                        error: error.clone(),
                    },
                );
            }
        }

        cleanup_agent_created_tasks(
            task_list_id,
            agent_id,
            spec_metadata,
            child_task_cleanup_status(status),
        );
        WorkerClosingState {
            diagnostics,
            sub_agent_tool_calls,
        }
    }
}

impl EngineSubAgentSpawner {
    /// Assemble the `ToolContext` the worker's tools run inside.
    ///
    /// Everything a tool is allowed to see or touch is decided here: the cwd
    /// override, the permission brokers stacked in front of the prompt, the
    /// path scope roots, the worktree isolation flag and the file-history
    /// tracker. It is one phase because the pieces constrain each other -- the
    /// report-file carve-out only makes sense against the scope roots that
    /// would otherwise reject it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_worker_tool_context(
        &self,
        spec: &SubAgentSpec,
        engine: &std::sync::Arc<rebon_core::Engine>,
        agent_id: &String,
        registry_title: &String,
        report_file_path: &Option<String>,
        scratchpad_root: &Option<PathBuf>,
        git_metadata: &Option<SubAgentGitMetadata>,
        structured_output_channel: &Option<Arc<rebon_tool::StructuredOutputChannel>>,
        is_verification_agent: bool,
        auto_report_from_final_text: bool,
        permission_prompts_unavailable: bool,
        registry_run_in_background: bool,
    ) -> WorkerToolContext {
        // Propagate SubAgentSpec.cwd into the worker's ToolContext so
        // tools like Bash/Edit/Write/Glob/Grep execute inside the
        // caller-requested directory (e.g. an agent worktree) instead
        // of leaking out to the parent process cwd — the same cwd
        // override an in-process AgentTool worker gets.
        let task_list_id = spec
            .task_list_id
            .clone()
            .unwrap_or_else(rebon_tool::tasks::current_task_list_id);
        // Keeps the parent's stored permission rules (allow_always,
        // settings allow/deny lists) in front of the prompt broker so
        // they stay effective inside the sub-agent.
        let sensitive_permission_broker = spec
            .permission_broker
            .clone()
            .and_then(|broker| {
                rebon_core::permission::sub_agent_sensitive_permission_broker_from(
                    &broker,
                    rebon_tool::WorkflowNesting::at(spec.workflow_nesting_depth)
                        .is_within_workflow(),
                )
            })
            .unwrap_or_else(|| engine.permission_broker().clone());
        // Sensitive commands delegate to an interactive user prompt
        // that waits without a timeout. A worker running in the
        // background (spawned there, or backgrounded mid-run) has no
        // user watching that prompt — delegation would hang the agent
        // until the user happens to come back. Probe the live registry
        // state at resolve time and deny instead.
        let task_id = TaskId::new(agent_id.clone());
        let background_probe: Arc<dyn Fn() -> bool + Send + Sync> = {
            let registry = self.task_registry.clone();
            let task_id = task_id.clone();
            Arc::new(move || {
                permission_prompts_unavailable
                    || registry_run_in_background
                    || registry.as_ref().is_some_and(|registry| {
                        registry
                            .snapshot(&task_id)
                            .is_some_and(|snap| snap.is_backgrounded)
                    })
            })
        };
        let worker_permission_broker = Arc::new(
            AutoApproveExceptSensitivePermissionBroker::with_background_probe(
                sensitive_permission_broker,
                background_probe,
            )
            .with_exempt_deletion_root(scratchpad_root.clone())
            // Out-of-scratchpad deletions from a backgrounded worker
            // float to the frontend as a permission request when the
            // parent runtime has an interactive prompt surface. Note
            // this reads the parent-level flag, NOT the child context
            // flag below, which ORs in `registry_run_in_background`.
            .with_background_deletion_ask(!permission_prompts_unavailable),
        );
        let mut child_context = rebon_tool::ToolContext::new()
            .with_permission_broker(worker_permission_broker)
            .with_agent_id(agent_id.clone())
            .with_task_list_id(task_list_id.clone())
            .with_workflow_nesting_depth(spec.workflow_nesting_depth)
            .with_permission_prompts_unavailable(
                permission_prompts_unavailable || registry_run_in_background,
            )
            .with_worker_escalation_client(
                self.escalation_registry
                    .worker_client(agent_id.clone(), Some(registry_title.clone())),
            );
        if let Some(resolver) = &self.session_tools {
            child_context = child_context.with_tool_resolver(resolver.clone());
        }
        if let Some(cwd) = spec.cwd.as_deref() {
            child_context = child_context.with_cwd(cwd.to_string());
        }
        // Worktree isolation follows the creation chain, never the shape of
        // `cwd`: `spec.cwd` can come straight from the Agent tool call, so a
        // parent could otherwise name a lookalike `.rebon/worktrees/…` path
        // and lift the shared-worktree Git gate off its own child.
        child_context = child_context.with_isolated_worktree(spec.runtime_isolated_worktree);
        // The mandated report file lives under the config home
        // (`~/.rebon/tasks/`), which is outside every project root and
        // outside any worktree `prepare_runtime_worktree` just created —
        // and that call REPLACES `allowed_roots`, so a coordinator cannot
        // authorize the path from the Agent call either. Without this
        // carve-out the worker is ordered to Write a path its own ACL
        // rejects, then fails for "report file is missing". Scope it to
        // the single file, not the whole tasks directory: one worker has
        // no business reading or rewriting another's report.
        let report_scope_root = report_file_path
            .as_ref()
            .filter(|_| !auto_report_from_final_text)
            .map(PathBuf::from);
        let mut path_scope_roots = effective_path_scope_roots(spec);
        for root in scratchpad_root.iter().chain(report_scope_root.iter()) {
            if !path_scope_roots.contains(root) {
                path_scope_roots.push(root.clone());
            }
        }
        let auto_approved_write_roots = scratchpad_root
            .iter()
            .chain(report_scope_root.iter())
            .cloned()
            .collect::<Vec<_>>();
        child_context = child_context
            .with_path_scope_roots(path_scope_roots)
            .with_auto_approved_write_roots(auto_approved_write_roots);
        if is_verification_agent {
            child_context = child_context.with_write_scope_roots(
                scratchpad_root
                    .iter()
                    .chain(report_scope_root.iter())
                    .cloned()
                    .collect::<Vec<_>>(),
            );
        }
        // Snapshot worker Write/Edit into the session file-history store
        // so `/rewind` can undo sub-agent changes. Skipped for worktree
        // workers: they write into an isolated worktree whose changes are
        // git-tracked and land in the parent tree only via an explicit
        // commit, so session-level snapshots would be misleading.
        if let Some(tracker) = self.file_history_tracker.as_ref() {
            let in_worktree = git_metadata
                .as_ref()
                .is_some_and(|git| git.worktree_path.is_some());
            if !in_worktree {
                child_context = child_context.with_file_history_tracker(tracker.clone());
            }
        }
        if let Some(capability) = spec.capability_context.clone() {
            child_context = child_context
                .with_session_id(capability.session_id.clone())
                .with_capability_context(capability);
        }
        if let Some(policy) = spec.execution_policy.clone() {
            child_context = child_context.with_execution_policy(policy);
        }
        if let Some(channel) = structured_output_channel.as_ref() {
            child_context = child_context.with_structured_output_channel(channel.clone());
        }
        WorkerToolContext {
            child_context,
            task_id,
            task_list_id,
        }
    }
}

impl EngineSubAgentSpawner {
    /// Decide what the worker is told and what it is allowed to use.
    ///
    /// The tool filter, the system prompt, the report path and the final
    /// prompt are one phase because they are decided against the same
    /// coordinator-mode question: a coordinator worker gets the worker
    /// preamble, a mandated report file and the caller's exact tool list,
    /// while a plain sub-agent gets the intersected default filter and no
    /// report directive.
    pub(super) fn build_worker_run_plan(
        &self,
        spec: &SubAgentSpec,
        spec_metadata: &serde_json::Value,
        agent_id: &String,
        agent_type: &Option<String>,
        scratchpad_dir: &Option<String>,
        is_coord: bool,
    ) -> WorkerRunPlan {
        // In coordinator mode, inject the worker system prompt and
        // report file directive.
        let auto_report_from_final_text =
            is_coord && should_auto_report_from_final_text(agent_type.as_deref());

        // Combine the spawner's base filter with the per-spec filter.
        //
        // Coordinator mode: an explicit per-spec filter takes full
        // precedence — the caller already chose the exact tools the
        // worker needs, and those tools may legitimately live outside
        // the default `async_agent_filter()` allow-list (e.g. MCP
        // tools). Only fall back to the base filter when no per-spec
        // filter was given.
        //
        // Non-coordinator mode: intersect both filters so the
        // sub-agent always sees the most restrictive combination.
        // Snapshot the shared base filter once per spawn so a
        // concurrent `/ceo` toggle doesn't mutate the value mid-decision.
        let base_filter_snapshot: Option<ToolFilter> =
            self.base_filter.as_ref().map(|shared| shared.current());
        let spec_tool_filter = spec.tool_filter.clone();
        let tool_filter = if is_coord {
            spec_tool_filter.or(base_filter_snapshot)
        } else {
            match (base_filter_snapshot, spec_tool_filter) {
                (Some(base), Some(spec_f)) => Some(base.intersect(&spec_f)),
                (Some(base), None) => Some(base),
                (None, Some(spec_f)) => Some(spec_f),
                (None, None) => None,
            }
        };
        let effective_cache_strategy = spec
            .cache_strategy
            .unwrap_or(rebon_tool::CacheStrategy::Auto);
        let cache_context_policy = spec
            .context
            .as_ref()
            .map(|context| context.mode.as_str().to_string());
        // Pick the base system prompt: caller override > coordinator
        // worker prompt (when in coordinator mode) > None.
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
        let base_system = prepend_ultraplan_worker_preamble(
            base_system,
            spec.execution_policy.as_ref(),
            spec_metadata,
        );

        // Append the sub-agent notes block to whatever base we picked:
        // every sub-agent gets the "use absolute paths / share file paths /
        // no emojis / no trailing colon" reminders, plus the scratchpad
        // pointer so temp artifacts stay out of the project tree. We skip
        // appending when there is no base prompt: the worker request then
        // carries no system prompt at all (`run_query` uses `params.system`
        // verbatim, with no engine-side fallback), and promoting the suffix
        // alone to `Some` would make it the worker's entire system prompt.
        let prompt_suffix = compose_sub_agent_prompt_suffix(scratchpad_dir.as_deref());
        let system = base_system.map(|base| {
            if base.trim_end().is_empty() {
                prompt_suffix.clone()
            } else {
                format!("{base}\n\n{prompt_suffix}")
            }
        });

        // Generate the coordinator worker report path from the same agent id
        // used by the TaskRegistry and task-notification. This keeps
        // `<task-id>` and `<output-file>` stable and avoids millisecond-based
        // collisions when several workers launch concurrently.
        let report_file_path = if is_coord {
            Some(report_file_path_for_agent_id(agent_id))
        } else {
            None
        };

        // Prepend the report file directive to the prompt so the
        // worker knows where to write its structured report. Explore
        // is read-only, so the harness writes its final text below.
        let prompt = if let Some(ref path) = report_file_path {
            if auto_report_from_final_text {
                spec.prompt.clone()
            } else {
                let directive =
                    rebon_core::coordinator_mode::build_report_file_directive_with_options(
                        path,
                        self.coordinator_use_worktree,
                    );
                format!("{directive}\n{}", spec.prompt)
            }
        } else {
            spec.prompt.clone()
        };
        WorkerRunPlan {
            auto_report_from_final_text,
            tool_filter,
            effective_cache_strategy,
            cache_context_policy,
            system,
            report_file_path,
            prompt,
        }
    }
}

impl EngineSubAgentSpawner {
    /// Settle placement: persistent actor, scratchpad and worktree.
    ///
    /// These three decisions are entangled and have to be taken in this order.
    /// A worktree request rules out the persistent actor, the scratchpad has to
    /// be resolved before worktree preparation rewrites `spec.cwd`, and the
    /// worktree itself is prepared last because it is the only step that
    /// touches the filesystem.
    pub(super) fn resolve_worker_placement(
        &self,
        spec: &mut SubAgentSpec,
        agent_id: &String,
        agent_type: &Option<String>,
        task_kind: SubAgentTaskKind,
    ) -> Result<WorkerPlacement, String> {
        let actor_managed = spec
            .metadata
            .get(PERSISTENT_AGENT_MANAGED_KEY)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let is_coord_for_actor = self
            .coordinator_mode
            .as_ref()
            .map(|mode| mode.get())
            .unwrap_or_else(rebon_core::coordinator_mode::coordinator_mode_from_env_default);
        let worktree_requested = spec
            .metadata
            .get("isolation")
            .and_then(Value::as_str)
            .is_some_and(|mode| mode.eq_ignore_ascii_case("worktree"))
            || spec.metadata.get("__runtime_git").is_some()
            || (is_coord_for_actor
                && self.coordinator_use_worktree
                && task_kind == SubAgentTaskKind::Implementation);
        let should_spawn_persistent_actor = self.task_registry.is_some()
            && !actor_managed
            && !worktree_requested
            && !rebon_tool::WorkflowNesting::at(spec.workflow_nesting_depth).is_within_workflow()
            && !is_explore_agent_type(agent_type.as_deref());
        let keep_runtime_resumable = actor_managed || should_spawn_persistent_actor;
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
        // Scratchpad for this worker's prompt + deletion carve-out.
        // Prefer the value `spawn_detached` stashed before it rewrote
        // `spec.cwd` to a worktree; otherwise resolve it here, before
        // this function's own worktree preparation mutates `spec.cwd`.
        // Either way the worker shares the PARENT session's scratchpad
        // (same key as `scratchpad_dir_for_session`); workers without
        // a known parent session fall back to an agent-scoped
        // directory.
        let scratchpad_dir = metadata_string(&spec.metadata, SCRATCHPAD_DIR_METADATA_KEY)
            .or_else(|| sub_agent_scratchpad_dir(spec, owner_session_id.as_deref(), agent_id));
        let scratchpad_root = scratchpad_dir.as_deref().map(PathBuf::from);
        if let Some(root) = scratchpad_root.as_deref() {
            std::fs::create_dir_all(root).map_err(|err| {
                format!(
                    "failed to prepare sub-agent scratchpad `{}`: {err}",
                    root.display()
                )
            })?;
        }
        let is_verification_agent = agent_type
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("verification"));
        let is_coord = is_coord_for_actor;
        let worktree_policy = runtime_worktree_policy(is_coord, self.coordinator_use_worktree);
        let git_metadata = if let Some(runtime_git) = spec.metadata.get("__runtime_git") {
            let git = SubAgentGitMetadata {
                worktree_path: runtime_git
                    .get("worktree_path")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                worktree_branch: runtime_git
                    .get("worktree_branch")
                    .or_else(|| runtime_git.get("branch"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                base_commit: runtime_git
                    .get("base_commit")
                    .or_else(|| runtime_git.get("base"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                source_worktree: runtime_git
                    .get("source_worktree")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                source_branch: runtime_git
                    .get("source_branch")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                git_root: runtime_git
                    .get("git_root")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                validation_status: Some("pending".to_string()),
                ..Default::default()
            };
            Some(git)
        } else {
            prepare_runtime_worktree(spec, agent_id, task_kind, worktree_policy)?
        };
        Ok(WorkerPlacement {
            should_spawn_persistent_actor,
            keep_runtime_resumable,
            scratchpad_dir,
            scratchpad_root,
            is_verification_agent,
            is_coord,
            git_metadata,
        })
    }
}

impl EngineSubAgentSpawner {
    async fn automatically_route_worker(
        &self,
        spec: &SubAgentSpec,
        agent_id: &str,
        request: ModelRouteRequest,
        current: rebon_agent_core::model_router::ResolvedModelRuntime,
    ) -> Result<(rebon_agent_core::model_router::ResolvedModelRuntime, bool), String> {
        use rebon_core::model_routing::{
            routed_target, run_bounded, selection_notice, ModelRoutingInput, ModelRoutingNotice,
            ModelRoutingService,
        };
        let Some(engine) = self.engine.upgrade() else {
            return Ok((current, false));
        };
        let Some(ctx) = engine.upstream_tool_context() else {
            return Ok((current, false));
        };
        let Some(router) = ctx.get::<ModelRoutingService>() else {
            return Ok((current, false));
        };
        let parent_id = spec
            .metadata
            .get("parent_session_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let key = (parent_id.to_owned(), agent_id.to_owned());
        let entry = self
            .automatic_routes
            .lock()
            .expect("worker model routing lock poisoned")
            .entry(key)
            .or_default()
            .clone();
        let mut cached = entry.lock().await;
        // 后续显式派发设置也必须优先，不能被任务缓存的自动选择覆盖。provider 也
        // 算显式设置：路由现在能换 provider，声明过的就更不能被它顶掉。
        if request.provider.is_some()
            || request.model.is_some()
            || request.model_profile.is_some()
            || request.reasoning_effort.is_some()
            || self
                .model_config
                .selection_for(
                    request.agent_type.as_deref(),
                    request.category.as_deref(),
                    None,
                )
                .is_some_and(|selection| {
                    selection.provider.is_some()
                        || selection.model.is_some()
                        || selection.model_profile.is_some()
                        || selection.reasoning_effort.is_some()
                })
        {
            *cached = Some(current.clone());
            return Ok((current, false));
        }
        if let Some(runtime) = cached.as_ref() {
            return Ok((runtime.clone(), true));
        }
        // 先记原配置，即使分类 future 被取消，同一任务也不能再次付费分类。
        *cached = Some(current.clone());
        let classify = async {
            let cwd = match spec.cwd.as_ref() {
                Some(cwd) => PathBuf::from(cwd),
                None => std::env::current_dir()?,
            };
            let input = ModelRoutingInput {
                prompt: spec.prompt.clone(),
                cwd,
                provider_name: current.provider_name.clone(),
                model: current.model.clone(),
                model_profiles: ModelProfileMap::default(),
                session: SessionHandle::borrowed(current.client.clone()),
            };
            let decision = router.route(input).await?;
            // 换 provider 时 worker 也要换腿，所以目标 runtime 由决策决定而不是当前 provider。
            let (provider, model) =
                routed_target(&decision, &current.provider_name, &current.model);
            let runtime = self
                .model_router()
                .resolve_automatic(ModelRouteRequest {
                    provider: Some(provider.clone()),
                    model: Some(model),
                    reasoning_effort: decision.reasoning_effort.or(current.reasoning_effort),
                    ..Default::default()
                })
                .await?;
            anyhow::ensure!(
                runtime.provider_name == provider,
                "worker router returned a different provider than requested"
            );
            Ok(runtime)
        };
        // 派发 future 随调用方取消而被丢弃；后台任务也必须受同一分类期限约束。
        let result = run_bounded(&PromptCancel::new(), classify)
            .await
            .map_err(|error| error.to_string())?;
        let (runtime, text) = match result {
            Ok(runtime) => {
                let text = selection_notice(
                    &runtime.provider_name,
                    &runtime.model,
                    runtime.reasoning_effort.map(|effort| effort.as_str()),
                );
                *cached = Some(runtime.clone());
                (runtime, text)
            }
            Err(error) => (
                current,
                format!("Experimental model routing skipped for {agent_id}: {error}"),
            ),
        };
        ctx.emit(&ModelRoutingNotice {
            session_id: parent_id.into(),
            text,
        });
        Ok((runtime, true))
    }

    /// Name the worker for the registry and resolve the runtime it will use.
    ///
    /// The registry-facing identity (agent type, title, prompt) and the model
    /// route are settled together because the route request is built from the
    /// same metadata the snapshot is named from, and both have to be fixed
    /// before `spec` is decomposed into a `WorkerSpec`.
    ///
    /// `spec_metadata` is the caller's, not ours: `requested_model_profile`
    /// borrows out of it and has to outlive this call.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn resolve_worker_registry_plan<'a>(
        &self,
        spec: &'a SubAgentSpec,
        spec_metadata: &'a Value,
        agent_id: &String,
        agent_type: &Option<String>,
        git_metadata: &Option<SubAgentGitMetadata>,
        structured_output_schema: Option<Value>,
        should_spawn_persistent_actor: bool,
    ) -> Result<WorkerRegistryPlan<'a>, String> {
        let mut persistent_actor_spec = should_spawn_persistent_actor.then(|| {
            let mut actor_spec = spec.clone();
            if let (Some(metadata), Some(git)) =
                (actor_spec.metadata.as_object_mut(), git_metadata.as_ref())
            {
                metadata.insert("__runtime_git".into(), git.to_json());
            }
            actor_spec
        });
        // Per-agent structured-output channel: carries the schema the
        // `StructuredOutput` tool validates against and records the
        // accepted value so `WorkerDeliveryHook` knows whether the worker
        // actually returned a result.
        let structured_output_channel = structured_output_schema
            .map(|schema| Arc::new(rebon_tool::StructuredOutputChannel::new(Some(schema))));
        let category = metadata_category(spec_metadata);
        let metadata_reasoning_effort = metadata_reasoning_effort(spec_metadata);
        let registry_agent_type = agent_type
            .clone()
            .unwrap_or_else(|| "general-purpose".to_string());
        // Human-readable title for the `/tasks` dialog. Prefer the
        // `description` field AgentTool fills in from the tool call,
        // then fall back to the first ~120 chars of the prompt.
        let registry_title = spec_metadata
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                spec.prompt
                    .lines()
                    .next()
                    .map(|s| s.chars().take(120).collect::<String>())
                    .unwrap_or_default()
            });
        // Clone the bits the snapshot needs to own before `spec` gets
        // decomposed into `WorkerSpec` below.
        let registry_prompt = spec.prompt.clone();
        let requested_model_profile = spec.model_profile.as_deref().or_else(|| {
            spec_metadata
                .get("modelProfile")
                .or_else(|| spec_metadata.get("model_profile"))
                .and_then(Value::as_str)
        });
        let route_request = ModelRouteRequest {
            provider: spec.provider.clone(),
            model: spec.model.clone(),
            model_profile: requested_model_profile.map(str::to_string),
            agent_type: agent_type.clone(),
            category: category.clone(),
            reasoning_effort: metadata_reasoning_effort,
        };
        let resolved_runtime = self
            .model_router()
            .resolve(route_request.clone())
            .await
            .map_err(|err| err.to_string())?;
        let (resolved_runtime, automatic) = self
            .automatically_route_worker(spec, agent_id, route_request, resolved_runtime)
            .await?;
        if let Some(actor) = persistent_actor_spec.as_mut().filter(|_| automatic) {
            actor.provider = Some(resolved_runtime.provider_name.clone());
            actor.model = Some(resolved_runtime.model.clone());
            actor.model_profile = None;
            if let (Some(effort), Some(metadata)) = (
                resolved_runtime.reasoning_effort,
                actor.metadata.as_object_mut(),
            ) {
                metadata.insert(
                    "reasoningEffort".into(),
                    Value::String(effort.as_str().into()),
                );
            }
        }
        let registry_model = resolved_runtime.model.clone();
        let registry_model_for_snapshot = registry_model.clone();
        let registry_provider = resolved_runtime.provider_name.clone();
        tracing::info!(
            agent_id = %agent_id,
            agent_type = agent_type.as_deref().unwrap_or("general-purpose"),
            provider = %registry_provider,
            model = %registry_model,
            model_profile = requested_model_profile.unwrap_or(""),
            "sub-agent model resolved"
        );
        let reasoning_effort = resolved_runtime.reasoning_effort;
        let registry_run_in_background = spec.run_in_background;
        let permission_prompts_unavailable = spec.permission_prompts_unavailable;
        Ok(WorkerRegistryPlan {
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
        })
    }
}
