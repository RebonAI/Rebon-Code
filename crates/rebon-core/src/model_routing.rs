use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use async_trait::async_trait;
use rebon_api::SessionHandle;
use rebon_kernel::Service;

use crate::query::RuntimeModelConfig;

pub const MODEL_ROUTING_SERVICE: &str = "first-prompt-model-routing";
/// The id of the plugin that provides [`MODEL_ROUTING_SERVICE`]. Defined here
/// because the engine has to name it (see [`routing_withheld`]) and cannot
/// depend on the plugin crate; the plugin takes its id from this constant.
pub const MODEL_ROUTING_PLUGIN_ID: &str = "model-routing";

/// Whether this process keeps first-prompt routing out altogether — the
/// entry point withheld the plugin, rather than the user switching it off.
///
/// The two differ in what a session already routed keeps. A user who turns
/// routing off mid-conversation still gets the model the session was routed
/// onto, from the session's `firstPromptModelRouting` sidecar, so the
/// conversation does not jump models. An entry point that withheld routing
/// (`rebon exec`, `--acp`) was handed its model by its caller, so a routed
/// choice made earlier on another surface is not restored there. The sidecar
/// is left as it is: going back to the terminal continues where it was.
pub fn routing_withheld(upstream: Option<&rebon_kernel::Context>) -> bool {
    upstream.is_some_and(|ctx| ctx.is_plugin_withheld(MODEL_ROUTING_PLUGIN_ID))
}

pub struct ModelRoutingNotice {
    pub session_id: String,
    pub text: String,
}

pub fn notice_update(text: String) -> rebon_types::SessionUpdate {
    rebon_types::SessionUpdate::SessionInfoUpdate {
        title: None,
        updated_at: None,
        meta: Some(std::collections::HashMap::from([(
            UI_NOTICE_META_KEY.into(),
            serde_json::Value::String(text),
        )])),
    }
}

/// `SessionInfoUpdate._meta` key of a line for the user's transcript.
pub const UI_NOTICE_META_KEY: &str = "uiNotice";
/// `SessionInfoUpdate._meta` key of a routing decision just made, beside its
/// notice. Whoever shows the session's model reads it: the notice alone is
/// prose, and a status bar still naming the model the session left is what
/// made a switch look like it had not happened.
pub const MODEL_SELECTION_META_KEY: &str = "modelSelection";

/// What a first-prompt routing decision moved the session onto.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoutedSelection {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// The notice for a routing decision, carrying the decision itself.
pub fn selection_update(text: String, selection: &RoutedSelection) -> rebon_types::SessionUpdate {
    let mut update = notice_update(text);
    if let rebon_types::SessionUpdate::SessionInfoUpdate {
        meta: Some(meta), ..
    } = &mut update
    {
        meta.insert(
            MODEL_SELECTION_META_KEY.into(),
            serde_json::to_value(selection).expect("a routed selection serialises"),
        );
    }
    update
}

/// The routing decision an update carries, if it is one.
pub fn routed_selection(update: &rebon_types::SessionUpdate) -> Option<RoutedSelection> {
    let rebon_types::SessionUpdate::SessionInfoUpdate {
        meta: Some(meta), ..
    } = update
    else {
        return None;
    };
    serde_json::from_value(meta.get(MODEL_SELECTION_META_KEY)?.clone()).ok()
}

/// The transcript line an update carries, if any.
pub fn ui_notice(update: &rebon_types::SessionUpdate) -> Option<&str> {
    let rebon_types::SessionUpdate::SessionInfoUpdate {
        meta: Some(meta), ..
    } = update
    else {
        return None;
    };
    meta.get(UI_NOTICE_META_KEY)?.as_str()
}

// 分类只是前置步骤，限制等待时间以免轻量路由阻塞真实任务。
const ROUTING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn run_bounded<T>(
    cancel: &rebon_types::PromptCancel,
    future: impl Future<Output = anyhow::Result<T>>,
) -> Result<anyhow::Result<T>, rebon_agent_core::PromptExecutorError> {
    if cancel.is_cancelled() {
        return Err(rebon_agent_core::PromptExecutorError::Cancelled);
    }
    let result = tokio::select! {
        biased;
        _ = cancel.notified() => return Err(rebon_agent_core::PromptExecutorError::Cancelled),
        result = tokio::time::timeout(ROUTING_TIMEOUT, future) => result.unwrap_or_else(|_| Err(anyhow::anyhow!("model routing timed out"))),
    };
    if cancel.is_cancelled() {
        return Err(rebon_agent_core::PromptExecutorError::Cancelled);
    }
    Ok(result)
}

pub fn selection_notice(provider: &str, model: &str, effort: Option<&str>) -> String {
    match effort {
        Some(effort) => format!(
            "Auto switched to {provider} / {model} with {effort} effort; continuing the task."
        ),
        None => format!("Auto switched to {provider} / {model}; continuing the task."),
    }
}

pub struct ModelRoutingInput {
    pub prompt: String,
    pub cwd: PathBuf,
    pub provider_name: String,
    pub model: String,
    pub model_profiles: rebon_types::ModelProfileMap,
    pub session: Arc<SessionHandle>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRoutingDecision {
    /// The provider to run on, when it is not the one the session is on.
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<rebon_types::ReasoningEffort>,
}

/// The provider/model pair a decision asks for, read against the pair the
/// session is on now.
///
/// A named provider replaces the current one and needs a model with it: a model
/// id belongs to the provider that serves it, so a provider-only decision would
/// ask the new provider for a model it may not have. Every caller that turns a
/// decision into a runtime reads it here, so the rule holds for the main session
/// and for a spawned worker alike.
pub fn routed_target(
    decision: &ModelRoutingDecision,
    provider: &str,
    model: &str,
) -> (String, String) {
    let target_model = decision.model.clone().unwrap_or_else(|| model.to_owned());
    match decision
        .provider
        .as_deref()
        .filter(|named| !named.is_empty())
    {
        Some(named) => (named.to_owned(), target_model),
        None => (provider.to_owned(), target_model),
    }
}

#[async_trait]
pub trait FirstPromptModelRouter: Send + Sync {
    async fn route(&self, input: ModelRoutingInput) -> anyhow::Result<ModelRoutingDecision>;
}

pub struct ModelRoutingService;

impl Service for ModelRoutingService {
    type Interface = dyn FirstPromptModelRouter;
    const NAME: &'static str = MODEL_ROUTING_SERVICE;
}

// 必须重用装配好的构造器，才能一起更新模型相关的预算、中间件和 compact 配置。
pub type ModelRuntimeResolver = Arc<
    dyn Fn(
            String,
            String,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<RuntimeModelConfig>> + Send>>
        + Send
        + Sync,
>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selection_update_carries_its_notice_and_its_decision() {
        let selection = RoutedSelection {
            provider: "deepseek".into(),
            model: "deepseek-flash".into(),
            effort: Some("low".into()),
        };
        let update = selection_update("switched".into(), &selection);

        assert_eq!(ui_notice(&update), Some("switched"));
        assert_eq!(routed_selection(&update), Some(selection));
    }

    #[test]
    fn a_plain_notice_carries_no_decision() {
        let update = notice_update("Experimental model routing skipped: no key".into());

        assert!(ui_notice(&update).is_some());
        assert_eq!(routed_selection(&update), None);
    }
}
