use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rebon_api::Message as ApiMessage;
use rebon_core::query::{AttachmentPollRequest, AttachmentPoller};
use rebon_plugin_tasks::{
    runtime::{
        TaskId, TaskNotification, TaskNotificationClaim, TaskNotificationKind, TaskRegistry,
    },
    TaskRegistryResolver,
};

const DEFAULT_TURN_ID: &str = "__default__";
const PARENT_SESSION_ID_KEY: &str = "parent_session_id";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NotificationClaim {
    task_id: TaskId,
    generation: u64,
    kind: TaskNotificationKind,
}

impl NotificationClaim {
    fn from_notification(notification: &TaskNotification) -> Self {
        Self {
            task_id: notification.task_id.clone(),
            generation: notification.generation,
            kind: notification.kind,
        }
    }

    fn registry_claim(&self) -> TaskNotificationClaim {
        TaskNotificationClaim {
            task_id: self.task_id.clone(),
            generation: self.generation,
            kind: self.kind,
        }
    }
}

#[derive(Debug, Default)]
struct TurnDeliveryState {
    pending_claims: HashSet<NotificationClaim>,
    report_paths: Vec<PathBuf>,
}

#[derive(Debug, Default)]
struct DeliveryState {
    turns: HashMap<String, TurnDeliveryState>,
    claimed_by: HashMap<NotificationClaim, String>,
    /// Every report path handed to the coordinator so far, keyed by
    /// session. A worker report stays readable for the rest of the
    /// session: a coordinator routinely revisits an earlier worker's
    /// findings several turns later, and consuming the allowlist on
    /// first read forced it to re-delegate just to re-read a file it
    /// had already been given.
    granted_report_paths: HashMap<String, Vec<PathBuf>>,
}

#[derive(Debug)]
enum TaskRegistrySource {
    Fixed(Arc<TaskRegistry>),
    Resolver {
        resolver: TaskRegistryResolver,
        revision: Option<tokio::sync::watch::Receiver<u64>>,
    },
}

#[derive(Debug)]
pub struct TaskNotificationPoller {
    registry: TaskRegistrySource,
    delivery: Mutex<DeliveryState>,
}

impl TaskNotificationPoller {
    pub fn new(registry: impl Into<Arc<TaskRegistry>>) -> Arc<Self> {
        Arc::new(Self {
            registry: TaskRegistrySource::Fixed(registry.into()),
            delivery: Mutex::new(DeliveryState::default()),
        })
    }

    pub fn new_resolving(resolver: TaskRegistryResolver) -> Arc<Self> {
        Arc::new(Self {
            registry: TaskRegistrySource::Resolver {
                resolver,
                revision: None,
            },
            delivery: Mutex::new(DeliveryState::default()),
        })
    }

    /// Build a fail-closed session consumer while retaining only the registry's
    /// wake signal for the TUI event loop. Reads still resolve the typed seat on
    /// every operation, so disabling or disposing the plugin cannot fall back
    /// to the Arc that supplied this receiver.
    pub(crate) fn new_session_resolving(
        resolver: TaskRegistryResolver,
        revision: tokio::sync::watch::Receiver<u64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry: TaskRegistrySource::Resolver {
                resolver,
                revision: Some(revision),
            },
            delivery: Mutex::new(DeliveryState::default()),
        })
    }

    fn registry(&self, session_id: &str) -> Result<Arc<TaskRegistry>, String> {
        match &self.registry {
            TaskRegistrySource::Fixed(registry) => Ok(registry.clone()),
            TaskRegistrySource::Resolver { resolver, .. } => resolver.resolve(session_id),
        }
    }

    pub fn subscribe_notification_revision(&self) -> tokio::sync::watch::Receiver<u64> {
        match &self.registry {
            TaskRegistrySource::Fixed(registry) => registry.subscribe_notification_revision(),
            TaskRegistrySource::Resolver {
                revision: Some(revision),
                ..
            } => revision.clone(),
            TaskRegistrySource::Resolver { revision: None, .. } => {
                panic!("revision subscription requires a session-bound task registry")
            }
        }
    }

    pub fn reserve_task_ids(
        &self,
        session_id: &str,
        turn_id: &str,
        task_ids: &[TaskId],
    ) -> Vec<TaskId> {
        if task_ids.is_empty() {
            return Vec::new();
        }
        let Ok(registry) = self.registry(session_id) else {
            return Vec::new();
        };
        let candidates = registry
            .unnotified_notifications()
            .into_iter()
            .filter(|notification| {
                Self::matches_session(&registry, &notification.task_id, session_id)
            })
            .map(|notification| (notification.task_id.clone(), notification))
            .collect::<HashMap<_, _>>();
        let mut delivery = self
            .delivery
            .lock()
            .expect("task notification delivery state poisoned");
        let mut claimed_ids = Vec::new();
        for task_id in task_ids {
            let Some(notification) = candidates.get(task_id) else {
                continue;
            };
            let claim = NotificationClaim::from_notification(notification);
            if !registry.notification_claim_is_current(&claim.registry_claim())
                || delivery
                    .claimed_by
                    .get(&claim)
                    .is_some_and(|claimed_turn| claimed_turn != turn_id)
            {
                continue;
            }
            delivery
                .claimed_by
                .insert(claim.clone(), turn_id.to_string());
            delivery
                .turns
                .entry(turn_id.to_string())
                .or_default()
                .pending_claims
                .insert(claim);
            claimed_ids.push(task_id.clone());
        }
        claimed_ids
    }

    pub fn finish_claim(&self, turn_id: &str, succeeded: bool) {
        self.finish(DEFAULT_TURN_ID, turn_id, succeeded);
    }

    pub fn unnotified_notifications_for_session(&self, session_id: &str) -> Vec<TaskNotification> {
        self.unnotified_notifications(session_id, None)
    }

    fn unnotified_notifications_for_query(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Vec<TaskNotification> {
        self.unnotified_notifications(session_id, Some(turn_id))
    }

    fn unnotified_notifications(
        &self,
        session_id: &str,
        turn_id: Option<&str>,
    ) -> Vec<TaskNotification> {
        let delivery = self
            .delivery
            .lock()
            .expect("task notification delivery state poisoned");
        let Ok(registry) = self.registry(session_id) else {
            return Vec::new();
        };
        let notifications = registry.unnotified_notifications();
        notifications
            .into_iter()
            .filter(|notification| {
                Self::matches_session(&registry, &notification.task_id, session_id)
            })
            .filter(|notification| {
                let claim = NotificationClaim::from_notification(notification);
                if claim.kind == TaskNotificationKind::Terminal
                    && delivery.turns.values().any(|turn| {
                        turn.pending_claims.iter().any(|pending| {
                            pending.kind == TaskNotificationKind::MonitorEvent
                                && pending.task_id == claim.task_id
                        })
                    })
                {
                    return false;
                }
                if claim.kind == TaskNotificationKind::MonitorEvent
                    && turn_id.is_some_and(|turn_id| {
                        delivery.turns.get(turn_id).is_some_and(|turn| {
                            turn.pending_claims.iter().any(|pending| {
                                pending.kind == TaskNotificationKind::MonitorEvent
                                    && pending.task_id == claim.task_id
                            })
                        })
                    })
                {
                    return false;
                }
                match delivery.claimed_by.get(&claim) {
                    None => true,
                    Some(claimed_turn) => turn_id.is_some_and(|turn_id| claimed_turn == turn_id),
                }
            })
            .collect()
    }

    fn matches_session(registry: &TaskRegistry, task_id: &TaskId, session_id: &str) -> bool {
        registry
            .snapshot(task_id)
            .and_then(|snapshot| {
                snapshot
                    .metadata
                    .get(PARENT_SESSION_ID_KEY)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .is_none_or(|parent_session_id| parent_session_id == session_id)
    }

    /// Fold this turn's newly announced reports into the session's
    /// grant list and return the whole list. Callers re-apply it to a
    /// fresh `ToolContext` each turn, so returning only the delta would
    /// silently revoke every previously granted report.
    fn take_report_paths(&self, session_key: &str, turn_id: &str) -> Vec<PathBuf> {
        let mut delivery = self
            .delivery
            .lock()
            .expect("task notification delivery state poisoned");
        let fresh = delivery
            .turns
            .get_mut(turn_id)
            .map(|turn| std::mem::take(&mut turn.report_paths))
            .unwrap_or_default();
        let granted = delivery
            .granted_report_paths
            .entry(session_key.to_string())
            .or_default();
        for path in fresh {
            if !granted.contains(&path) {
                granted.push(path);
            }
        }
        granted.clone()
    }

    fn finish(&self, session_id: &str, turn_id: &str, succeeded: bool) {
        // Always release this poller's local claim. If the seat was disabled or
        // disposed mid-turn, the registry notification remains unacknowledged
        // and can be retried after a legitimate re-enable/rebind.
        let registry = self.registry(session_id).ok();
        let mut delivery = self
            .delivery
            .lock()
            .expect("task notification delivery state poisoned");
        let Some(pending_claims) = delivery
            .turns
            .get(turn_id)
            .map(|turn| turn.pending_claims.iter().cloned().collect::<Vec<_>>())
        else {
            return;
        };
        if let Some(registry) = registry.filter(|_| !pending_claims.is_empty()) {
            let claims = pending_claims
                .iter()
                .map(NotificationClaim::registry_claim)
                .collect::<Vec<_>>();
            if succeeded {
                registry.mark_notification_claims_delivered(&claims);
            } else {
                registry.mark_notification_claims_undelivered(&claims);
            }
        }
        let Some(turn) = delivery.turns.remove(turn_id) else {
            return;
        };
        for claim in &turn.pending_claims {
            if delivery
                .claimed_by
                .get(claim)
                .is_some_and(|claimed_turn| claimed_turn == turn_id)
            {
                delivery.claimed_by.remove(claim);
            }
        }
    }
}

impl AttachmentPoller for TaskNotificationPoller {
    fn poll(&self, request: AttachmentPollRequest<'_>) -> Vec<ApiMessage> {
        let Ok(registry) = self.registry(request.session_id) else {
            return Vec::new();
        };
        let notifications =
            self.unnotified_notifications_for_query(request.session_id, request.turn_id);
        if notifications.is_empty() {
            return Vec::new();
        }

        let mut delivery = self
            .delivery
            .lock()
            .expect("task notification delivery state poisoned");
        let mut messages = Vec::new();
        for notification in notifications {
            let claim = NotificationClaim::from_notification(&notification);
            if delivery
                .claimed_by
                .get(&claim)
                .is_some_and(|claimed_turn| claimed_turn != request.turn_id)
                || delivery.turns.get(request.turn_id).is_some_and(|turn| {
                    turn.pending_claims.contains(&claim)
                        || (claim.kind == TaskNotificationKind::MonitorEvent
                            && turn.pending_claims.iter().any(|pending| {
                                pending.kind == TaskNotificationKind::MonitorEvent
                                    && pending.task_id == claim.task_id
                            }))
                })
                || !registry.notification_claim_is_current(&claim.registry_claim())
            {
                continue;
            }
            delivery
                .claimed_by
                .insert(claim.clone(), request.turn_id.to_string());
            let turn = delivery
                .turns
                .entry(request.turn_id.to_string())
                .or_default();
            turn.pending_claims.insert(claim);
            if let Some(output_file) = notification.output_file {
                turn.report_paths.push(PathBuf::from(output_file));
            }
            messages.push(ApiMessage::user_text(notification.message));
        }
        messages
    }

    fn take_coordinator_report_paths(&self) -> Vec<PathBuf> {
        self.take_report_paths(DEFAULT_TURN_ID, DEFAULT_TURN_ID)
    }

    fn take_coordinator_report_paths_for_turn(&self, turn_id: &str) -> Vec<PathBuf> {
        self.take_report_paths(turn_id, turn_id)
    }

    fn take_coordinator_report_paths_for_query(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Vec<PathBuf> {
        self.take_report_paths(session_id, turn_id)
    }

    fn finish_turn(&self, succeeded: bool) {
        self.finish(DEFAULT_TURN_ID, DEFAULT_TURN_ID, succeeded);
    }

    fn finish_turn_for_turn(&self, turn_id: &str, succeeded: bool) {
        self.finish(turn_id, turn_id, succeeded);
    }

    fn finish_turn_for_query(&self, session_id: &str, turn_id: &str, succeeded: bool) {
        self.finish(session_id, turn_id, succeeded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::ContentBlock;
    use rebon_core::query::AttachmentPollPhase;
    // Through the harness's re-export, not a direct edge: a binary that names
    // `rebon-kernel` itself is claiming to assemble kernels, which this does
    // not (see the re-export's own note in `rebon-harness`).
    use rebon_harness::rebon_kernel::{Kernel, PluginState, PluginStateChanged};

    fn request<'a>(
        session_id: &'a str,
        turn_id: &'a str,
        next_iteration: u64,
    ) -> AttachmentPollRequest<'a> {
        AttachmentPollRequest::new(
            session_id,
            turn_id,
            next_iteration,
            AttachmentPollPhase::Regular,
        )
    }

    fn default_request(next_iteration: u64) -> AttachmentPollRequest<'static> {
        request(DEFAULT_TURN_ID, DEFAULT_TURN_ID, next_iteration)
    }

    use rebon_plugin_tasks::runtime::{
        LocalAgentData, MonitorData, MonitorEndReason, MonitorSourceKind, TaskData, TaskKind,
        TaskSnapshot, TaskStatus,
    };
    use rebon_types::PromptCancel;
    use serde_json::json;

    fn insert_failed_agent(
        registry: &TaskRegistry,
        id: &str,
        parent_session_id: Option<&str>,
    ) -> TaskId {
        let task_id = TaskId::new(id);
        let mut metadata = json!({});
        if let Some(parent_session_id) = parent_session_id {
            metadata[PARENT_SESSION_ID_KEY] = json!(parent_session_id);
        }
        registry.insert(
            task_id.clone(),
            TaskSnapshot {
                id: task_id.clone(),
                kind: TaskKind::LocalAgent,
                status: TaskStatus::Failed,
                title: "verification".into(),
                last_progress: Some("requested model is not supported".into()),
                error: Some("requested model is not supported".into()),
                result: Some(json!({
                    "status": "failed",
                    "final_text": "requested model is not supported",
                    "output_file": format!("{id}.report.md"),
                    "tool_call_count": 0,
                })),
                is_backgrounded: true,
                notified: false,
                start_time_ms: 1,
                end_time_ms: Some(2),
                metadata,
                data: TaskData::LocalAgent(LocalAgentData {
                    prompt: "verify changes".into(),
                    agent_type: "verification".into(),
                    model: Some("unsupported-model".into()),
                    system: None,
                    allowed_tools: None,
                    token_count: 0,
                    tool_use_count: 0,
                    transcript: Vec::new(),
                    streaming_text: None,
                    pending_messages: Vec::new(),
                    retrieved: false,
                }),
            },
            PromptCancel::new(),
        );
        task_id
    }

    fn insert_monitor(registry: &TaskRegistry, id: &str, parent_session_id: &str) -> TaskId {
        let task_id = TaskId::new(id);
        registry.insert(
            task_id.clone(),
            TaskSnapshot {
                id: task_id.clone(),
                kind: TaskKind::Monitor,
                status: TaskStatus::Running,
                title: "watch output".into(),
                last_progress: None,
                error: None,
                result: None,
                is_backgrounded: true,
                notified: false,
                start_time_ms: 1,
                end_time_ms: None,
                metadata: json!({ PARENT_SESSION_ID_KEY: parent_session_id }),
                data: TaskData::Monitor(MonitorData {
                    description: "watch output".into(),
                    source: MonitorSourceKind::Command,
                    redacted_target: "command".into(),
                    event_count: 0,
                    suppressed_count: 0,
                    end_reason: None,
                }),
            },
            PromptCancel::new(),
        );
        task_id
    }

    fn message_text(message: &ApiMessage) -> &str {
        match &message.content[0] {
            ContentBlock::Text(text) => &text.text,
            other => panic!("expected text attachment, got {other:?}"),
        }
    }

    #[test]
    fn session_resolving_poller_fails_closed_and_retries_after_reenable() {
        let scopes = rebon_kernel_seats::kernel_services::SessionKernelScopes::new(
            Kernel::new(),
            Arc::new(rebon_core::Engine::new()),
            std::env::temp_dir(),
        );
        let lease = scopes.acquire("session-switch");
        let registry = scopes.host_task_registry("session-switch");
        let poller = TaskNotificationPoller::new_session_resolving(
            scopes.task_registry_resolver(),
            registry.subscribe_notification_revision(),
        );
        insert_failed_agent(&registry, "agent-switch", Some("session-switch"));

        assert!(poller
            .poll(request("session-switch", "before-load", 1))
            .is_empty());
        lease.context().emit(&PluginStateChanged {
            id: rebon_plugin_tasks::PLUGIN_ID.to_string(),
            from: PluginState::Disabled,
            to: PluginState::Loaded,
            generation: 1,
        });
        assert_eq!(poller.poll(request("session-switch", "loaded", 1)).len(), 1);

        lease.context().emit(&PluginStateChanged {
            id: rebon_plugin_tasks::PLUGIN_ID.to_string(),
            from: PluginState::Loaded,
            to: PluginState::Disabled,
            generation: 2,
        });
        poller.finish_turn_for_query("session-switch", "loaded", true);
        assert!(poller
            .poll(request("session-switch", "disabled", 1))
            .is_empty());
        assert_eq!(registry.unnotified_terminal_notifications().len(), 1);

        lease.context().emit(&PluginStateChanged {
            id: rebon_plugin_tasks::PLUGIN_ID.to_string(),
            from: PluginState::Disabled,
            to: PluginState::Loaded,
            generation: 3,
        });
        assert_eq!(
            poller.poll(request("session-switch", "reenabled", 1)).len(),
            1
        );
        poller.finish_turn_for_query("session-switch", "reenabled", true);
        assert!(registry.unnotified_terminal_notifications().is_empty());
    }

    #[test]
    fn terminal_notification_is_emitted_once_per_turn_and_acked_on_success() {
        let registry = TaskRegistry::new();
        insert_failed_agent(&registry, "agent-failed", None);
        let poller = TaskNotificationPoller::new(registry.clone());

        let messages = poller.poll(default_request(1));
        assert_eq!(messages.len(), 1);
        assert!(message_text(&messages[0]).contains("<status>failed</status>"));
        assert!(message_text(&messages[0]).contains("not supported"));
        assert_eq!(
            poller.take_coordinator_report_paths(),
            vec![PathBuf::from("agent-failed.report.md")]
        );
        assert!(poller.poll(default_request(2)).is_empty());

        poller.finish_turn(true);

        assert!(registry.unnotified_terminal_notifications().is_empty());
    }

    #[test]
    fn failed_turn_releases_notification_for_retry() {
        let registry = TaskRegistry::new();
        insert_failed_agent(&registry, "agent-retry", None);
        let poller = TaskNotificationPoller::new(registry);

        assert_eq!(poller.poll(default_request(1)).len(), 1);
        poller.finish_turn(false);

        assert_eq!(poller.poll(default_request(2)).len(), 1);
    }

    #[test]
    fn reserved_idle_notification_is_not_reinjected_and_is_acked_on_success() {
        let registry = TaskRegistry::new();
        let task_id = insert_failed_agent(&registry, "agent-idle", Some("session-idle"));
        let poller = TaskNotificationPoller::new(registry.clone());

        let _ = poller.reserve_task_ids("session-idle", "turn-idle", &[task_id]);
        assert!(poller
            .poll(request("session-idle", "turn-idle", 1))
            .is_empty());

        poller.finish_turn_for_query("session-idle", "turn-idle", true);
        assert!(registry.unnotified_terminal_notifications().is_empty());
    }

    #[test]
    fn reserved_idle_notification_is_retryable_after_failed_turn() {
        let registry = TaskRegistry::new();
        let task_id = insert_failed_agent(&registry, "agent-idle-retry", Some("session-idle"));
        let poller = TaskNotificationPoller::new(registry);

        let _ = poller.reserve_task_ids("session-idle", "turn-idle", &[task_id]);
        poller.finish_turn_for_query("session-idle", "turn-idle", false);

        assert_eq!(
            poller.poll(request("session-idle", "turn-retry", 1)).len(),
            1
        );
    }

    #[test]
    fn failed_claim_restores_notification_observed_during_turn() {
        let registry = TaskRegistry::new();
        let task_id = insert_failed_agent(&registry, "agent-observed", Some("session-idle"));
        let poller = TaskNotificationPoller::new(registry.clone());

        let claimed =
            poller.reserve_task_ids("session-idle", "turn-idle", std::slice::from_ref(&task_id));
        assert_eq!(claimed, vec![task_id.clone()]);
        registry.mark_notifications_delivered(std::slice::from_ref(&task_id));
        poller.finish_turn_for_query("session-idle", "turn-idle", false);

        assert_eq!(
            poller
                .unnotified_notifications_for_session("session-idle")
                .len(),
            1
        );
    }

    #[test]
    fn concurrent_turns_keep_notifications_and_report_paths_isolated() {
        let registry = TaskRegistry::new();
        insert_failed_agent(&registry, "agent-a", Some("session-a"));
        insert_failed_agent(&registry, "agent-b", Some("session-b"));
        let poller = TaskNotificationPoller::new(registry.clone());

        let messages_a = poller.poll(request("session-a", "session-a", 1));
        let messages_b = poller.poll(request("session-b", "session-b", 1));

        assert_eq!(messages_a.len(), 1);
        assert!(message_text(&messages_a[0]).contains("agent-a"));
        assert_eq!(messages_b.len(), 1);
        assert!(message_text(&messages_b[0]).contains("agent-b"));
        assert_eq!(
            poller.take_coordinator_report_paths_for_turn("session-a"),
            vec![PathBuf::from("agent-a.report.md")]
        );
        assert_eq!(
            poller.take_coordinator_report_paths_for_turn("session-b"),
            vec![PathBuf::from("agent-b.report.md")]
        );

        poller.finish_turn_for_turn("session-a", true);
        assert_eq!(registry.unnotified_terminal_notifications().len(), 1);
        poller.finish_turn_for_turn("session-b", false);
        assert_eq!(poller.poll(request("session-b", "session-b", 2)).len(), 1);
    }

    /// The allowlist is re-applied to a fresh `ToolContext` every turn,
    /// so handing out only the newest paths revoked every earlier
    /// report — a coordinator that read a worker report on turn 3 was
    /// denied the same file on turn 5 and had to delegate a worker just
    /// to read it back.
    #[test]
    fn granted_report_paths_stay_readable_in_later_turns() {
        let registry = TaskRegistry::new();
        insert_failed_agent(&registry, "agent-first", Some("session-keep"));
        let poller = TaskNotificationPoller::new(registry.clone());

        assert_eq!(poller.poll(request("session-keep", "turn-1", 1)).len(), 1);
        assert_eq!(
            poller.take_coordinator_report_paths_for_query("session-keep", "turn-1"),
            vec![PathBuf::from("agent-first.report.md")]
        );
        poller.finish_turn_for_query("session-keep", "turn-1", true);

        insert_failed_agent(&registry, "agent-second", Some("session-keep"));
        assert_eq!(poller.poll(request("session-keep", "turn-2", 1)).len(), 1);
        assert_eq!(
            poller.take_coordinator_report_paths_for_query("session-keep", "turn-2"),
            vec![
                PathBuf::from("agent-first.report.md"),
                PathBuf::from("agent-second.report.md"),
            ],
            "an earlier worker's report must still be readable"
        );

        // Repeating the same turn must not duplicate a grant.
        assert_eq!(
            poller
                .take_coordinator_report_paths_for_query("session-keep", "turn-2")
                .len(),
            2
        );
    }

    #[test]
    fn granted_report_paths_do_not_leak_between_sessions() {
        let registry = TaskRegistry::new();
        insert_failed_agent(&registry, "agent-a", Some("session-a"));
        insert_failed_agent(&registry, "agent-b", Some("session-b"));
        let poller = TaskNotificationPoller::new(registry);

        assert_eq!(poller.poll(request("session-a", "turn-a", 1)).len(), 1);
        assert_eq!(poller.poll(request("session-b", "turn-b", 1)).len(), 1);

        assert_eq!(
            poller.take_coordinator_report_paths_for_query("session-a", "turn-a"),
            vec![PathBuf::from("agent-a.report.md")]
        );
        assert_eq!(
            poller.take_coordinator_report_paths_for_query("session-b", "turn-b"),
            vec![PathBuf::from("agent-b.report.md")]
        );
    }

    #[test]
    fn successful_claim_is_not_replayed_to_another_turn() {
        let registry = TaskRegistry::new();
        insert_failed_agent(&registry, "agent-success", None);
        let poller = TaskNotificationPoller::new(registry);

        assert_eq!(poller.poll(request("session-a", "session-a", 1)).len(), 1);
        poller.finish_turn_for_turn("session-a", true);

        assert!(poller.poll(request("session-b", "session-b", 2)).is_empty());
    }

    #[test]
    fn concurrent_turns_in_same_session_do_not_finish_each_others_claims() {
        let registry = TaskRegistry::new();
        let task_id = insert_failed_agent(&registry, "agent-shared", Some("session-shared"));
        let poller = TaskNotificationPoller::new(registry.clone());

        assert_eq!(poller.poll(request("session-shared", "turn-a", 1)).len(), 1);
        assert!(poller
            .poll(request("session-shared", "turn-b", 1))
            .is_empty());

        poller.finish_turn_for_query("session-shared", "turn-b", true);
        assert!(!registry.snapshot(&task_id).expect("task").notified);
        assert!(poller
            .poll(request("session-shared", "turn-b", 2))
            .is_empty());

        poller.finish_turn_for_query("session-shared", "turn-a", false);
        assert_eq!(poller.poll(request("session-shared", "turn-b", 3)).len(), 1);
    }

    #[test]
    fn stale_idle_snapshot_cannot_steal_an_active_turn_claim() {
        let registry = TaskRegistry::new();
        let task_id = insert_failed_agent(&registry, "agent-race", Some("session-race"));
        let poller = TaskNotificationPoller::new(registry.clone());

        let candidate_ids = poller
            .unnotified_notifications_for_session("session-race")
            .into_iter()
            .map(|notification| notification.task_id)
            .collect::<Vec<_>>();
        assert_eq!(candidate_ids, vec![task_id]);
        assert_eq!(
            poller.poll(request("session-race", "turn-active", 1)).len(),
            1
        );

        assert!(poller
            .reserve_task_ids("session-race", "turn-idle", &candidate_ids)
            .is_empty());
        poller.finish_turn_for_query("session-race", "turn-active", true);
        assert!(registry.unnotified_terminal_notifications().is_empty());
    }

    #[test]
    fn monitor_events_batch_by_task_and_ack_only_the_claimed_sequence() {
        let registry = TaskRegistry::new();
        let task_id = insert_monitor(&registry, "monitor-sequence", "session-monitor");
        registry.enqueue_monitor_event(&task_id, "first event");
        registry.enqueue_monitor_event(&task_id, "second event");
        let poller = TaskNotificationPoller::new(registry.clone());

        let messages = poller.poll(request("session-monitor", "turn-one", 1));
        assert_eq!(messages.len(), 1);
        assert!(message_text(&messages[0]).contains("first event"));
        assert!(message_text(&messages[0]).contains("second event"));

        registry.enqueue_monitor_event(&task_id, "third event");
        assert!(poller
            .poll(request("session-monitor", "turn-one", 2))
            .is_empty());
        poller.finish_turn_for_query("session-monitor", "turn-one", true);

        let messages = poller.poll(request("session-monitor", "turn-two", 1));
        assert_eq!(messages.len(), 1);
        assert!(!message_text(&messages[0]).contains("first event"));
        assert!(!message_text(&messages[0]).contains("second event"));
        assert!(message_text(&messages[0]).contains("third event"));
    }

    #[test]
    fn terminal_monitor_waits_for_an_inflight_event_claim() {
        let registry = TaskRegistry::new();
        let task_id = insert_monitor(&registry, "monitor-terminal-race", "session-monitor");
        registry.enqueue_monitor_event(&task_id, "final event");
        let poller = TaskNotificationPoller::new(registry.clone());

        let event = poller.poll(request("session-monitor", "turn-event", 1));
        assert_eq!(event.len(), 1);
        assert!(message_text(&event[0]).contains("<status>event</status>"));
        registry.update(&task_id, |snapshot| {
            snapshot.status = TaskStatus::Completed;
            snapshot.end_time_ms = Some(2);
            let TaskData::Monitor(data) = &mut snapshot.data else {
                unreachable!();
            };
            data.end_reason = Some(MonitorEndReason::Exited);
        });

        assert!(poller
            .poll(request("session-monitor", "turn-event", 2))
            .is_empty());
        assert!(poller
            .poll(request("session-monitor", "turn-concurrent", 1))
            .is_empty());
        poller.finish_turn_for_query("session-monitor", "turn-event", true);

        let terminal = poller.poll(request("session-monitor", "turn-terminal", 1));
        assert_eq!(terminal.len(), 1);
        assert!(message_text(&terminal[0]).contains("<status>exited</status>"));
        assert!(!message_text(&terminal[0]).contains("final event"));
    }

    #[test]
    fn failed_monitor_claim_retries_and_idle_reservation_blocks_active_delivery() {
        let registry = TaskRegistry::new();
        let task_id = insert_monitor(&registry, "monitor-retry", "session-monitor");
        registry.enqueue_monitor_event(&task_id, "retry me");
        let poller = TaskNotificationPoller::new(registry);

        assert_eq!(
            poller.reserve_task_ids(
                "session-monitor",
                "turn-idle",
                std::slice::from_ref(&task_id),
            ),
            vec![task_id]
        );
        assert!(poller
            .poll(request("session-monitor", "turn-active", 1))
            .is_empty());

        poller.finish_turn_for_query("session-monitor", "turn-idle", false);
        let retry = poller.poll(request("session-monitor", "turn-active", 2));
        assert_eq!(retry.len(), 1);
        assert!(message_text(&retry[0]).contains("retry me"));
    }

    #[test]
    fn monitor_notifications_remain_session_scoped() {
        let registry = TaskRegistry::new();
        let monitor_a = insert_monitor(&registry, "monitor-a", "session-a");
        let monitor_b = insert_monitor(&registry, "monitor-b", "session-b");
        registry.enqueue_monitor_event(&monitor_a, "event a");
        registry.enqueue_monitor_event(&monitor_b, "event b");
        let poller = TaskNotificationPoller::new(registry);

        let session_a = poller.poll(request("session-a", "turn-a", 1));
        assert_eq!(session_a.len(), 1);
        assert!(message_text(&session_a[0]).contains("event a"));
        assert!(!message_text(&session_a[0]).contains("event b"));

        let session_b = poller.poll(request("session-b", "turn-b", 1));
        assert_eq!(session_b.len(), 1);
        assert!(message_text(&session_b[0]).contains("event b"));
        assert!(!message_text(&session_b[0]).contains("event a"));
    }

    #[test]
    fn unscoped_notification_is_claimed_by_only_one_concurrent_turn() {
        let registry = TaskRegistry::new();
        insert_failed_agent(&registry, "agent-unscoped", None);
        let poller = TaskNotificationPoller::new(registry);

        assert_eq!(poller.poll(request("session-a", "session-a", 1)).len(), 1);
        assert!(poller.poll(request("session-b", "session-b", 1)).is_empty());

        poller.finish_turn_for_turn("session-a", false);
        assert_eq!(poller.poll(request("session-b", "session-b", 2)).len(), 1);
    }
}
