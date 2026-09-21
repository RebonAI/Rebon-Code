use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use async_trait::async_trait;
use rebon_api::SessionHandle;
use rebon_kernel::Service;

use crate::query::RuntimeModelConfig;

pub const MODEL_ROUTING_SERVICE: &str = "first-prompt-model-routing";

pub struct ModelRoutingNotice {
    pub session_id: String,
    pub text: String,
}

pub fn notice_update(text: String) -> rebon_types::SessionUpdate {
    rebon_types::SessionUpdate::SessionInfoUpdate {
        title: None,
        updated_at: None,
        meta: Some(std::collections::HashMap::from([(
            "uiNotice".into(),
            serde_json::Value::String(text),
        )])),
    }
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

pub fn selection_notice(model: &str, effort: Option<rebon_types::ReasoningEffort>) -> String {
    match effort {
        Some(effort) => format!(
            "Auto switched model to {model} with {} effort; continuing the task.",
            effort.as_str()
        ),
        None => format!("Auto switched model to {model}; continuing the task."),
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
    pub model: Option<String>,
    pub reasoning_effort: Option<rebon_types::ReasoningEffort>,
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
