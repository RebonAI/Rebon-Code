use super::super::*;
use rebon_plugin_tasks::{runtime as task_runtime, TaskRegistryResolver};

pub(crate) fn ensure_task_store_bridge_after_activity(
    runtime: &BackgroundTeammateRuntime,
    store: &BackgroundStore,
    job_id: &str,
) {
    let Some(cursor) = runtime.task_bridge.claim_after_activity() else {
        return;
    };
    let _runtime_guard = runtime.handle.enter();
    let (stop, bridge) = spawn_task_store_bridge_from_cursor(
        Arc::clone(&runtime.registry),
        store.clone(),
        job_id.to_string(),
        runtime.session_id.clone(),
        cursor,
        Some(runtime.task_bridge.clone()),
    );
    let _ = stop.send(());
    drop(bridge);
}

#[derive(Clone)]
pub(crate) struct BackgroundTaskRuntimeController {
    task_registry_resolver: Arc<Mutex<Option<TaskRegistryResolver>>>,
    teammate_runtimes: Arc<Mutex<Vec<BackgroundTeammateRuntime>>>,
    store: BackgroundStore,
    job_id: String,
}

impl BackgroundTaskRuntimeController {
    pub(crate) fn new(
        task_registry_resolver: Arc<Mutex<Option<TaskRegistryResolver>>>,
        teammate_runtimes: Arc<Mutex<Vec<BackgroundTeammateRuntime>>>,
        store: BackgroundStore,
        job_id: String,
    ) -> Self {
        Self {
            task_registry_resolver,
            teammate_runtimes,
            store,
            job_id,
        }
    }

    fn current_resolver(&self) -> Result<TaskRegistryResolver, String> {
        self.task_registry_resolver
            .lock()
            .expect("background task registry resolver poisoned")
            .clone()
            .ok_or_else(|| "task-registry is unavailable for this background session".to_string())
    }

    fn runtime_for_registry(
        &self,
        registry: &Arc<TaskRegistry>,
    ) -> Option<BackgroundTeammateRuntime> {
        self.teammate_runtimes
            .lock()
            .expect("poisoned")
            .iter()
            .find(|runtime| Arc::ptr_eq(&runtime.registry, registry))
            .cloned()
    }
}

#[async_trait::async_trait]
impl rebon_tool::TaskRuntimeController for BackgroundTaskRuntimeController {
    async fn stop_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<rebon_tool::StopTaskOutcome, String> {
        let id = TaskId::new(task_id);
        let resolver = self.current_resolver()?;
        let registry = resolver.resolve(session_id)?;
        if registry.snapshot(&id).is_none() {
            return Ok(rebon_tool::StopTaskOutcome::NotFound);
        }
        let controller = task_runtime::TaskRegistryRuntimeController::new(resolver);
        let outcome =
            rebon_tool::TaskRuntimeController::stop_task(&controller, session_id, task_id).await;
        if matches!(outcome, Ok(rebon_tool::StopTaskOutcome::Stopped { .. })) {
            ensure_bridge_for_registry(
                &self.teammate_runtimes,
                &registry,
                &self.store,
                &self.job_id,
            );
        }
        outcome
    }

    async fn send_message_to_task(
        &self,
        session_id: &str,
        task_id: &str,
        message: String,
    ) -> Result<(), rebon_tools_core::ToolErrorPresentation> {
        let registry = self
            .current_resolver()
            .and_then(|resolver| resolver.resolve(session_id))
            .map_err(|error| {
                rebon_tools_core::ToolErrorPresentation::new(
                    "task_registry_unavailable",
                    "Task runtime is unavailable for this session.",
                    error,
                )
            })?;
        if let Some((registry, resolved_id)) = resolve_local_agent_task(Some(&registry), task_id)? {
            task_runtime::send_message_to_local_agent_task(
                registry.as_ref(),
                resolved_id.as_str(),
                message,
            )?;
            if let Some(runtime) = self.runtime_for_registry(&registry) {
                ensure_task_store_bridge_after_activity(&runtime, &self.store, &self.job_id);
            }
            return Ok(());
        }

        Err(rebon_tools_core::ToolErrorPresentation::new(
            "agent_not_found",
            format!("Agent \"{task_id}\" was not found."),
            format!(
                "Agent \"{task_id}\" has no active task. Verify the agent id or display name returned by the Agent tool; if it completed and expired, spawn a fresh worker with the follow-up instructions."
            ),
        ))
    }

    fn background_shell_started(
        &self,
        session_id: &str,
        spec: rebon_tool::BackgroundShellTaskSpec,
        cancel: rebon_types::PromptCancel,
    ) {
        if let Ok(resolver) = self.current_resolver() {
            let controller = task_runtime::TaskRegistryRuntimeController::new(resolver);
            rebon_tool::TaskRuntimeController::background_shell_started(
                &controller,
                session_id,
                spec,
                cancel,
            );
        }
    }

    fn background_shell_finished(
        &self,
        session_id: &str,
        completion: rebon_tool::BackgroundShellTaskCompletion,
    ) {
        if let Ok(resolver) = self.current_resolver() {
            let controller = task_runtime::TaskRegistryRuntimeController::new(resolver);
            rebon_tool::TaskRuntimeController::background_shell_finished(
                &controller,
                session_id,
                completion,
            );
        }
    }

    fn background_shell_observed(&self, session_id: &str, shell_id: &str) {
        if let Ok(resolver) = self.current_resolver() {
            let controller = task_runtime::TaskRegistryRuntimeController::new(resolver);
            rebon_tool::TaskRuntimeController::background_shell_observed(
                &controller,
                session_id,
                shell_id,
            );
        }
    }
}

/// A task id, or the display name of a local agent, resolved against the
/// worker's registry.
///
/// Ambiguity is judged within that one registry. It used to be judged across
/// every registry the worker had ever built, because each turn built a new
/// one — so two turns of the same session could each hold an agent of the
/// same name and read as a collision. One session, one table, is the
/// comparison the message was always describing.
fn resolve_local_agent_task(
    registry: Option<&Arc<TaskRegistry>>,
    task_id: &str,
) -> Result<Option<(Arc<TaskRegistry>, TaskId)>, rebon_tools_core::ToolErrorPresentation> {
    let Some(registry) = registry else {
        return Ok(None);
    };
    let requested_id = TaskId::new(task_id);
    if registry.snapshot(&requested_id).is_some() {
        return Ok(Some((Arc::clone(registry), requested_id)));
    }

    let aliases = registry
        .snapshots()
        .into_iter()
        .filter(|snapshot| {
            matches!(&snapshot.data, TaskData::LocalAgent(_))
                && snapshot
                    .metadata_str("display_name")
                    .or_else(|| snapshot.metadata_str("name"))
                    .is_some_and(|name| name.eq_ignore_ascii_case(task_id))
        })
        .collect::<Vec<_>>();
    let active = aliases
        .iter()
        .filter(|snapshot| !snapshot.status.is_terminal())
        .collect::<Vec<_>>();
    if active.len() > 1 {
        return Err(rebon_tools_core::ToolErrorPresentation::new(
            "agent_ambiguous",
            format!("More than one Agent is named \"{task_id}\"."),
            format!(
                "Multiple active Agent tasks use the display name \"{task_id}\". Retry SendMessage with the exact agent_id returned by the Agent tool."
            ),
        ));
    }
    if let Some(snapshot) = active.first() {
        return Ok(Some((Arc::clone(registry), snapshot.id.clone())));
    }
    Ok(aliases
        .first()
        .map(|snapshot| (Arc::clone(registry), snapshot.id.clone())))
}

/// Teammates are routed through their owning `TeamManager`, including terminal
/// teammates that need the manager's revive path. Local-agent tasks keep the
/// registry lookup so display-name aliases and tombstone diagnostics remain
/// available.
pub(crate) fn reply_to_registered_background_task(
    registry: &Arc<TaskRegistry>,
    teammate_runtimes: &Arc<Mutex<Vec<BackgroundTeammateRuntime>>>,
    store: &BackgroundStore,
    job_id: &str,
    task_id: &str,
    message: String,
) -> Result<(), String> {
    let id = TaskId::new(task_id.to_string());
    let teammate_runtime = teammate_runtimes
        .lock()
        .expect("poisoned")
        .iter()
        .find(|runtime| {
            Arc::ptr_eq(&runtime.registry, registry)
                && runtime.registry.snapshot(&id).is_some_and(|snapshot| {
                    matches!(&snapshot.data, TaskData::InProcessTeammate(_))
                })
        })
        .cloned();
    if let Some(runtime) = teammate_runtime {
        let manager = Arc::clone(&runtime.manager);
        let task_id = task_id.to_string();
        let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
        runtime.handle.spawn(async move {
            let result = manager
                .send_message_to_task(&task_id, message)
                .await
                .map_err(|error| error.display_message);
            let _ = response_tx.send(result);
        });
        let result = response_rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| "background teammate reply timed out".to_string())?;
        if result.is_ok() {
            ensure_task_store_bridge_after_activity(&runtime, store, job_id);
        }
        return result;
    }

    let registries = Some(Arc::clone(registry));
    if let Some((registry, snapshot)) = registries.iter().find_map(|registry| {
        registry
            .snapshot(&id)
            .map(|snapshot| (Arc::clone(registry), snapshot))
    }) {
        if matches!(&snapshot.data, TaskData::InProcessTeammate(_)) {
            if snapshot.status.is_terminal() {
                return Err(format!("Agent \"{task_id}\" has already finished."));
            }
            return task_runtime::inject_user_message_to_teammate(registry.as_ref(), &id, message)
                .then_some(())
                .ok_or_else(|| format!("Agent \"{task_id}\" could not accept the message."));
        }
    }

    if let Some((registry, resolved_id)) = resolve_local_agent_task(registries.as_ref(), task_id)
        .map_err(|error| error.display_message)?
    {
        task_runtime::send_message_to_local_agent_task(
            registry.as_ref(),
            resolved_id.as_str(),
            message,
        )
        .map_err(|error| error.display_message)?;
        let runtime = teammate_runtimes
            .lock()
            .expect("poisoned")
            .iter()
            .find(|runtime| Arc::ptr_eq(&runtime.registry, &registry))
            .cloned();
        if let Some(runtime) = runtime {
            ensure_task_store_bridge_after_activity(&runtime, store, job_id);
        }
        return Ok(());
    }

    let mut not_found = None;
    if let Some(registry) = registries {
        match task_runtime::send_message_to_local_agent_task(
            registry.as_ref(),
            task_id,
            message.clone(),
        ) {
            Ok(()) => {
                let runtime = teammate_runtimes
                    .lock()
                    .expect("poisoned")
                    .iter()
                    .find(|runtime| Arc::ptr_eq(&runtime.registry, &registry))
                    .cloned();
                if let Some(runtime) = runtime {
                    ensure_task_store_bridge_after_activity(&runtime, store, job_id);
                }
                return Ok(());
            }
            Err(error) if error.code == "agent_not_found" => {
                not_found.get_or_insert(error);
            }
            Err(error) => return Err(error.display_message),
        }
    }
    Err(not_found
        .map(|error| error.display_message)
        .unwrap_or_else(|| format!("Agent \"{task_id}\" was not found.")))
}

pub(crate) struct RegisteredTaskCancellation {
    pub(crate) stopped_task_ids: Vec<String>,
    pub(crate) errors: Vec<String>,
    /// Registries a stop actually landed in. A registry whose tasks had all
    /// settled has no bridge draining it any more, so the kill would never
    /// reach the store — the caller replays these through
    /// [`ensure_task_store_bridge_after_activity`] once it is out of the
    /// job-state critical section.
    pub(crate) stopped_registries: Vec<Arc<TaskRegistry>>,
    /// Ids no live registry has ever heard of. A worker restart — an app
    /// update quiescing the old one is the common way — takes its registries
    /// with it, but the rows those tasks produced live on in the job's event
    /// history, so clients still offer a stop button for them. Nothing in this
    /// process can stop what it does not hold, and returning an error leaves
    /// the row exactly as it was: unstoppable forever. The caller settles them
    /// in the store instead, which is the one place every client agrees on.
    pub(crate) unknown_task_ids: Vec<String>,
}

pub(crate) fn cancel_registered_background_tasks(
    registry: Option<&Arc<TaskRegistry>>,
    task_ids: &[String],
) -> RegisteredTaskCancellation {
    let registries = registry.cloned();
    let mut stopped_task_ids = Vec::new();
    let mut errors = Vec::new();
    let mut stopped_registries: Vec<Arc<TaskRegistry>> = Vec::new();
    let mut unknown_task_ids = Vec::new();
    for task_id in task_ids {
        let id = TaskId::new(task_id.clone());
        let Some(registry) = registries
            .iter()
            .find(|registry| registry.snapshot(&id).is_some())
        else {
            unknown_task_ids.push(task_id.clone());
            continue;
        };
        match stop_task(registry, &id) {
            Ok(_) => {
                registry
                    .escalation_registry()
                    .cancel_agent(task_id, "background task was stopped by attached client");
                stopped_task_ids.push(task_id.clone());
                if !stopped_registries
                    .iter()
                    .any(|stopped| Arc::ptr_eq(stopped, registry))
                {
                    stopped_registries.push(Arc::clone(registry));
                }
            }
            Err(error) => errors.push(error.to_string()),
        }
    }
    RegisteredTaskCancellation {
        stopped_task_ids,
        errors,
        stopped_registries,
        unknown_task_ids,
    }
}

/// Settle a task no live registry holds by writing its terminal event into the
/// job's own stream.
///
/// This is the authoritative place: the store is what every client projects
/// from, so a row settled here settles everywhere and stays settled across
/// restarts. The event deliberately carries no descriptor — this process has
/// no snapshot to describe — which leaves every other field the projection
/// already holds untouched and changes only the outcome.
pub(crate) fn settle_unknown_task_in_store(
    store: &BackgroundStore,
    job_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    let now = rebon_session_host::now_ms();
    store.append_task_event_batch(
        job_id,
        &rebon_session_host::BackgroundTaskEventBatch {
            schema: 1,
            stream_id: format!("orphan-settle/{job_id}"),
            from_cursor: 0,
            through_cursor: 0,
            cursor_was_stale: false,
            reset_tasks: Vec::new(),
            events: vec![rebon_session_host::BackgroundTaskEvent {
                cursor: 0,
                task_id: task_id.to_string(),
                timestamp_ms: now,
                task: None,
                event: rebon_session_host::BackgroundTaskEventKind::Finished {
                    status: task_runtime::TaskStatus::Killed.as_str().to_string(),
                    error: Some(
                        "the worker that owned this agent is gone; the row was closed out"
                            .to_string(),
                    ),
                },
            }],
        },
    )
}

/// Restart the store bridge for `registry` if nothing is draining it.
///
/// Any IPC-driven mutation needs this: the bridge exits once every task in a
/// registry has settled, so a later stop (or follow-up) would otherwise
/// change the registry with no one left to publish the change.
pub(crate) fn ensure_bridge_for_registry(
    teammate_runtimes: &Arc<Mutex<Vec<BackgroundTeammateRuntime>>>,
    registry: &Arc<TaskRegistry>,
    store: &BackgroundStore,
    job_id: &str,
) {
    let runtime = teammate_runtimes
        .lock()
        .expect("poisoned")
        .iter()
        .find(|runtime| Arc::ptr_eq(&runtime.registry, registry))
        .cloned();
    if let Some(runtime) = runtime {
        ensure_task_store_bridge_after_activity(&runtime, store, job_id);
    }
}
