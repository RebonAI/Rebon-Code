use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rebon_types::xml_escape;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// Escalation state that [`ToolContext`](crate::ToolContext) carries in its
/// extension bag.
///
/// Storage only — read and written through the unchanged
/// `worker_escalation_client()` / `escalation_resolver()` accessors.
#[derive(Clone, Default)]
pub struct EscalationContext {
    pub worker_client: Option<WorkerEscalationClient>,
    pub resolver: Option<EscalationResolver>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EscalationId(pub String);

impl EscalationId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EscalationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EscalationSource {
    User,
    Coordinator,
}

impl EscalationSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Coordinator => "coordinator",
        }
    }
}

impl std::str::FromStr for EscalationSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "user" => Ok(Self::User),
            "coordinator" => Ok(Self::Coordinator),
            _ => Err("source must be exactly \"user\" or \"coordinator\"".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalationAnswer {
    pub escalation_id: EscalationId,
    pub agent_id: String,
    pub answer: String,
    pub source: EscalationSource,
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionEscalation {
    pub escalation_id: EscalationId,
    pub agent_id: String,
    pub agent_description: Option<String>,
    pub question: String,
    pub options: Option<Vec<serde_json::Value>>,
    pub context: Option<String>,
}

#[derive(Debug)]
struct PendingEscalation {
    escalation: QuestionEscalation,
    answer_tx: oneshot::Sender<EscalationAnswer>,
    notified: bool,
}

#[derive(Debug, Clone)]
pub struct QuestionEscalationNotification {
    pub escalation_id: EscalationId,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct EscalationRegistry {
    inner: Arc<Mutex<EscalationRegistryInner>>,
}

#[derive(Debug, Default)]
struct EscalationRegistryInner {
    pending: HashMap<EscalationId, PendingEscalation>,
    order: VecDeque<EscalationId>,
    next_id: u64,
}

impl Default for EscalationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl EscalationRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(EscalationRegistryInner::default())),
        }
    }

    fn next_escalation_id(&self, agent_id: &str) -> EscalationId {
        let mut guard = self.inner.lock().expect("escalation registry poisoned");
        guard.next_id = guard.next_id.saturating_add(1);
        EscalationId::new(format!(
            "esc-{}-{}",
            sanitize_id_part(agent_id),
            guard.next_id
        ))
    }

    pub fn worker_client(
        &self,
        agent_id: impl Into<String>,
        agent_description: Option<String>,
    ) -> WorkerEscalationClient {
        WorkerEscalationClient {
            registry: self.clone(),
            agent_id: agent_id.into(),
            agent_description,
            answer_timeout: Duration::from_secs(10 * 60),
        }
    }

    pub fn resolver(&self) -> EscalationResolver {
        EscalationResolver {
            registry: self.clone(),
        }
    }

    fn submit(&self, mut escalation: QuestionEscalation) -> oneshot::Receiver<EscalationAnswer> {
        if escalation.escalation_id.as_str().is_empty() {
            escalation.escalation_id = self.next_escalation_id(&escalation.agent_id);
        }
        let (answer_tx, answer_rx) = oneshot::channel();
        let mut guard = self.inner.lock().expect("escalation registry poisoned");
        guard.order.push_back(escalation.escalation_id.clone());
        guard.pending.insert(
            escalation.escalation_id.clone(),
            PendingEscalation {
                escalation,
                answer_tx,
                notified: false,
            },
        );
        answer_rx
    }

    pub fn unnotified_question_notifications(&self) -> Vec<QuestionEscalationNotification> {
        let guard = self.inner.lock().expect("escalation registry poisoned");
        guard
            .order
            .iter()
            .filter_map(|id| {
                guard.pending.get(id).and_then(|pending| {
                    (!pending.notified).then(|| QuestionEscalationNotification {
                        escalation_id: id.clone(),
                        message: format_question_escalation_xml(&pending.escalation),
                    })
                })
            })
            .collect()
    }

    pub fn mark_notifications_delivered(&self, ids: &[EscalationId]) {
        if ids.is_empty() {
            return;
        }
        let mut guard = self.inner.lock().expect("escalation registry poisoned");
        for id in ids {
            if let Some(pending) = guard.pending.get_mut(id) {
                pending.notified = true;
            }
        }
    }

    pub fn resolve(&self, answer: EscalationAnswer) -> Result<(), String> {
        let mut guard = self.inner.lock().expect("escalation registry poisoned");
        let pending = guard.pending.remove(&answer.escalation_id).ok_or_else(|| {
            format!(
                "unknown or already resolved escalation_id `{}`",
                answer.escalation_id
            )
        })?;
        guard.order.retain(|id| id != &answer.escalation_id);
        if pending.escalation.agent_id != answer.agent_id {
            tracing::warn!(
                escalation_id = %answer.escalation_id,
                expected_agent = %pending.escalation.agent_id,
                received_agent = %answer.agent_id,
                "agent_id mismatch in ResolveEscalation"
            );
        }
        pending
            .answer_tx
            .send(answer)
            .map_err(|_| "waiting worker is no longer available; escalation cancelled".to_string())
    }

    pub fn cancel_agent(&self, agent_id: &str, reason: &str) -> usize {
        let mut guard = self.inner.lock().expect("escalation registry poisoned");
        let ids: Vec<_> = guard
            .pending
            .iter()
            .filter_map(|(id, pending)| {
                (pending.escalation.agent_id == agent_id).then(|| id.clone())
            })
            .collect();
        for id in &ids {
            if guard.pending.remove(id).is_some() {
                tracing::debug!(
                    escalation_id = %id,
                    agent_id = %agent_id,
                    reason = %reason,
                    "dropping pending question escalation during agent cancellation"
                );
            }
        }
        guard.order.retain(|id| !ids.contains(id));
        ids.len()
    }

    pub fn cancel_all(&self, reason: &str) -> usize {
        let mut guard = self.inner.lock().expect("escalation registry poisoned");
        let pending = std::mem::take(&mut guard.pending);
        guard.order.clear();
        let count = pending.len();
        for (id, pending) in pending {
            tracing::debug!(
                escalation_id = %id,
                agent_id = %pending.escalation.agent_id,
                reason = %reason,
                "dropping pending question escalation during registry cancellation"
            );
            drop(pending);
        }
        count
    }
}

#[derive(Debug, Clone)]
pub struct WorkerEscalationClient {
    registry: EscalationRegistry,
    agent_id: String,
    agent_description: Option<String>,
    answer_timeout: Duration,
}

impl WorkerEscalationClient {
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn agent_description(&self) -> Option<&str> {
        self.agent_description.as_deref()
    }

    pub fn answer_timeout(&self) -> Duration {
        self.answer_timeout
    }

    pub fn with_answer_timeout(mut self, timeout: Duration) -> Self {
        self.answer_timeout = timeout;
        self
    }

    pub async fn escalate(
        &self,
        question: String,
        options: Option<Vec<serde_json::Value>>,
        context: Option<String>,
    ) -> Result<EscalationAnswer, String> {
        let escalation_id = self.registry.next_escalation_id(&self.agent_id);
        let answer_rx = self.registry.submit(QuestionEscalation {
            escalation_id: escalation_id.clone(),
            agent_id: self.agent_id.clone(),
            agent_description: self.agent_description.clone(),
            question,
            options,
            context,
        });

        match tokio::time::timeout(self.answer_timeout, answer_rx).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_)) => Err(format!(
                "question escalation `{escalation_id}` was cancelled before it was answered"
            )),
            Err(_) => {
                self.registry
                    .cancel_agent(&self.agent_id, "timed out waiting for escalation answer");
                Err(format!(
                    "timed out waiting for coordinator answer to escalation `{escalation_id}` after {} seconds",
                    self.answer_timeout.as_secs()
                ))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct EscalationResolver {
    registry: EscalationRegistry,
}

impl EscalationResolver {
    pub fn resolve(&self, answer: EscalationAnswer) -> Result<(), String> {
        self.registry.resolve(answer)
    }
}

pub fn format_question_escalation_xml(escalation: &QuestionEscalation) -> String {
    let mut message = format!(
        "<question-escalation>\n<escalation-id>{}</escalation-id>\n<agent-id>{}</agent-id>",
        xml_escape(escalation.escalation_id.as_str()),
        xml_escape(&escalation.agent_id)
    );
    if let Some(agent_description) = escalation
        .agent_description
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        message.push_str(&format!(
            "\n<agent-description>{}</agent-description>",
            xml_escape(agent_description)
        ));
    }
    message.push_str(&format!(
        "\n<status>pending</status>\n<question>{}</question>",
        xml_escape(&escalation.question)
    ));
    if let Some(context) = escalation.context.as_deref().filter(|s| !s.is_empty()) {
        message.push_str(&format!("\n<context>{}</context>", xml_escape(context)));
    }
    if let Some(options) = escalation.options.as_ref().filter(|o| !o.is_empty()) {
        if let Ok(json) = serde_json::to_string(options) {
            message.push_str(&format!("\n<options>{}</options>", xml_escape(&json)));
        }
    }
    message.push_str("\n</question-escalation>");
    message
}

fn sanitize_id_part(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "agent".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sequential_escalations_route_by_id() {
        let registry = EscalationRegistry::new();
        let client = registry.worker_client("agent-1", Some("desc".to_string()));
        let resolver = registry.resolver();

        let handle1 = tokio::spawn({
            let client = client.clone();
            async move { client.escalate("first?".into(), None, None).await }
        });
        tokio::task::yield_now().await;
        let first_id = registry.unnotified_question_notifications()[0]
            .escalation_id
            .clone();
        let handle2 = tokio::spawn({
            let client = client.clone();
            async move { client.escalate("second?".into(), None, None).await }
        });
        tokio::task::yield_now().await;
        let ids = registry.unnotified_question_notifications();
        let second_id = ids
            .iter()
            .find(|n| n.escalation_id != first_id)
            .unwrap()
            .escalation_id
            .clone();

        resolver
            .resolve(EscalationAnswer {
                escalation_id: second_id.clone(),
                agent_id: "agent-1".into(),
                answer: "second answer".into(),
                source: EscalationSource::Coordinator,
                instructions: None,
            })
            .unwrap();
        let second = handle2.await.unwrap().unwrap();
        assert_eq!(second.escalation_id, second_id);
        assert_eq!(second.answer, "second answer");

        resolver
            .resolve(EscalationAnswer {
                escalation_id: first_id.clone(),
                agent_id: "agent-1".into(),
                answer: "first answer".into(),
                source: EscalationSource::User,
                instructions: Some("go".into()),
            })
            .unwrap();
        let first = handle1.await.unwrap().unwrap();
        assert_eq!(first.escalation_id, first_id);
        assert_eq!(first.instructions.as_deref(), Some("go"));
    }

    #[tokio::test]
    async fn cancel_all_errors_waiting_escalations() {
        let registry = EscalationRegistry::new();
        let client = registry
            .worker_client("agent-1", Some("desc".to_string()))
            .with_answer_timeout(Duration::from_secs(60));

        let handle = tokio::spawn({
            let client = client.clone();
            async move { client.escalate("blocked?".into(), None, None).await }
        });
        tokio::task::yield_now().await;
        let notifications = registry.unnotified_question_notifications();
        assert_eq!(notifications.len(), 1);

        assert_eq!(registry.cancel_all("session teardown"), 1);
        assert!(registry.unnotified_question_notifications().is_empty());
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("cancelled before it was answered"));
        assert!(err.contains(notifications[0].escalation_id.as_str()));
    }

    #[tokio::test]
    async fn cancel_agent_errors_matching_waiting_escalations() {
        let registry = EscalationRegistry::new();
        let client = registry
            .worker_client("agent-1", Some("desc".to_string()))
            .with_answer_timeout(Duration::from_secs(60));

        let handle = tokio::spawn({
            let client = client.clone();
            async move { client.escalate("blocked?".into(), None, None).await }
        });
        tokio::task::yield_now().await;
        let notifications = registry.unnotified_question_notifications();
        assert_eq!(notifications.len(), 1);

        assert_eq!(registry.cancel_agent("agent-1", "task stopped"), 1);
        assert!(registry.unnotified_question_notifications().is_empty());
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("cancelled before it was answered"));
        assert!(err.contains(notifications[0].escalation_id.as_str()));
    }

    #[test]
    fn xml_formatter_escapes_and_uses_escalation_id() {
        let xml = format_question_escalation_xml(&QuestionEscalation {
            escalation_id: EscalationId::new("esc<&>"),
            agent_id: "agent<&>".into(),
            agent_description: Some("desc<&>".into()),
            question: "Q <tag> &?".into(),
            options: None,
            context: Some("ctx 'quote'".into()),
        });
        assert!(xml.contains("<escalation-id>esc&lt;&amp;&gt;</escalation-id>"));
        assert!(xml.contains("<agent-id>agent&lt;&amp;&gt;</agent-id>"));
        assert!(xml.contains("<question>Q &lt;tag&gt; &amp;?</question>"));
        assert!(xml.contains("<context>ctx &apos;quote&apos;</context>"));
    }
}
