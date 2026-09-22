use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};

use crate::model_routing::run_bounded as bounded;
use rebon_agent_core::{PromptExecutorError, PromptRequest};
use rebon_session::model_selection::{self, SessionModelSelection};

use super::SharedRuntimeModel;
use crate::model_routing::{
    routed_target, selection_notice, FirstPromptModelRouter, ModelRoutingInput,
    ModelRuntimeResolver,
};

type Key = (PathBuf, String, String);

#[derive(Default)]
pub(super) struct SessionModelRouting {
    pub resolver: RwLock<Option<ModelRuntimeResolver>>,
    entries: Mutex<HashMap<Key, Arc<tokio::sync::Mutex<RouteState>>>>,
    notice_bridges: Mutex<HashMap<String, rebon_kernel::Context>>,
}

#[derive(Default)]
struct RouteState {
    started: bool,
    selected: Option<SessionModelSelection>,
    runtime: Option<SharedRuntimeModel>,
}

pub(crate) struct PreparedModel {
    pub runtime: Option<SharedRuntimeModel>,
    pub notice: Option<String>,
}

impl SharedRuntimeModel {
    pub async fn rebuild_model(
        &self,
        provider: String,
        model: String,
    ) -> anyhow::Result<super::RuntimeModelConfig> {
        let resolve = self
            .routing
            .resolver
            .read()
            .expect("model runtime resolver lock poisoned")
            .clone()
            .ok_or_else(|| anyhow::anyhow!("runtime model resolver unavailable"))?;
        resolve(provider, model).await
    }

    pub(crate) fn bind_routing_notices(
        &self,
        ctx: &rebon_kernel::Context,
        request: &PromptRequest,
    ) {
        let Some(publisher) = request.update_publisher.as_ref() else {
            return;
        };
        let publisher = Arc::downgrade(publisher);
        let id = request.session_id.clone();
        let bridge = ctx.fork(&format!("model-routing-notices-{id}"));
        let runtime = tokio::runtime::Handle::current();
        bridge.on(move |notice: &crate::model_routing::ModelRoutingNotice| {
            if notice.session_id != id {
                return;
            }
            if let Some(publisher) = publisher.upgrade() {
                let params = rebon_types::SessionUpdateParams {
                    session_id: id.clone(),
                    update: crate::model_routing::notice_update(notice.text.clone()),
                };
                runtime.spawn(async move {
                    publisher.publish_owned(params).await;
                });
            }
        });
        // 订阅绑定具体会话且替换时注销，避免 ACP 多 session 重复展示或串线。
        if let Some(old) = self
            .routing
            .notice_bridges
            .lock()
            .expect("routing notice lock poisoned")
            .insert(request.session_id.clone(), bridge)
        {
            old.dispose();
        }
    }

    pub(crate) async fn prepare_first_prompt(
        &self,
        root: PathBuf,
        request: &mut PromptRequest,
        has_prior_user: bool,
        router: Option<Arc<dyn FirstPromptModelRouter>>,
    ) -> Result<PreparedModel, PromptExecutorError> {
        let saved = model_selection::load(&root, &request.cwd, &request.session_id);
        if router.is_none() {
            match &saved {
                Ok(None) => {
                    return Ok(PreparedModel {
                        runtime: None,
                        notice: None,
                    })
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot read optional session model selection");
                    return Ok(PreparedModel {
                        runtime: None,
                        notice: None,
                    });
                }
                Ok(Some(_)) => {}
            }
        }
        let key = (root, request.cwd.clone(), request.session_id.clone());
        let entry = self
            .routing
            .entries
            .lock()
            .expect("model routing entries lock poisoned")
            .entry(key.clone())
            .or_default()
            .clone();
        let mut state = tokio::select! {
            biased;
            _ = request.cancel.notified() => return Err(PromptExecutorError::Cancelled),
            state = entry.lock() => state,
        };
        let (root, cwd, id) = &key;
        let mut notice = None;
        let eligible = router.is_some() && !state.started && request.user_prompt.is_some();
        if eligible {
            state.started = true;
        }
        // 等待同 session 的首次分类后再读，避免并发请求沿用加锁前的旧选型。
        let mut selection = match model_selection::load(root, cwd, id) {
            Ok(value) => value,
            Err(error) => {
                state.started = true;
                return Ok(PreparedModel {
                    runtime: None,
                    notice: Some(format!("Experimental model routing skipped: {error}")),
                });
            }
        };
        if eligible && selection.is_none() {
            match model_selection::claim(root, cwd, id) {
                Ok(true) => {
                    selection = Some(SessionModelSelection::default());
                    if !has_prior_user
                        && !request
                            .user_prompt
                            .as_deref()
                            .unwrap_or_default()
                            .trim()
                            .is_empty()
                    {
                        if let Some(router) = router {
                            let (current, session) = self.snapshot();
                            let resolver = self
                                .routing
                                .resolver
                                .read()
                                .expect("model runtime resolver lock poisoned")
                                .clone();
                            let route = async {
                                let decision = router
                                    .route(ModelRoutingInput {
                                        prompt: request.user_prompt.clone().unwrap_or_default(),
                                        cwd: cwd.into(),
                                        provider_name: current.provider_name.clone(),
                                        model: current.model.clone(),
                                        model_profiles: current.model_profiles.clone(),
                                        session,
                                    })
                                    .await?;
                                let (provider, model) = routed_target(
                                    &decision,
                                    &current.provider_name,
                                    &current.model,
                                );
                                let runtime = if provider == current.provider_name
                                    && model == current.model
                                {
                                    current.clone()
                                } else {
                                    let resolve = resolver.ok_or_else(|| {
                                        anyhow::anyhow!("runtime model resolver unavailable")
                                    })?;
                                    resolve(provider.clone(), model).await?
                                };
                                anyhow::ensure!(
                                    runtime.provider_name == provider,
                                    "resolver returned a different provider than requested"
                                );
                                let choice = SessionModelSelection {
                                    provider: Some(runtime.provider_name.clone()),
                                    model: Some(runtime.model.clone()),
                                    effort: decision
                                        .reasoning_effort
                                        .map(|effort| effort.as_str().to_owned()),
                                    manual_override: false,
                                };
                                Ok((choice, runtime))
                            };
                            match bounded(&request.cancel, route).await? {
                                Ok((choice, runtime)) => {
                                    match model_selection::compare_exchange(
                                        root,
                                        cwd,
                                        id,
                                        &SessionModelSelection::default(),
                                        &choice,
                                    ) {
                                        Ok(true) => {
                                            notice = Some(selection_notice(
                                                &runtime.provider_name,
                                                &runtime.model,
                                                choice.effort.as_deref(),
                                            ));
                                            state.runtime = Some(SharedRuntimeModel::new(runtime));
                                            state.selected = Some(choice.clone());
                                            selection = Some(choice);
                                        }
                                        Ok(false) => {
                                            match model_selection::load(root, cwd, id) {
                                                Ok(value) => selection = value,
                                                Err(error) => {
                                                    selection = None;
                                                    notice = Some(format!("Experimental model routing skipped: {error}"));
                                                }
                                            }
                                        }
                                        Err(error) => {
                                            notice = Some(format!(
                                                "Experimental model routing skipped: {error}"
                                            ))
                                        }
                                    }
                                }
                                Err(error) => {
                                    notice =
                                        Some(format!("Experimental model routing skipped: {error}"))
                                }
                            }
                        }
                    }
                }
                Ok(false) => match model_selection::load(root, cwd, id) {
                    Ok(value) => selection = value,
                    Err(error) => {
                        selection = None;
                        notice = Some(format!("Experimental model routing skipped: {error}"));
                    }
                },
                Err(error) => notice = Some(format!("Experimental model routing skipped: {error}")),
            }
        }
        if let Some(choice) = selection {
            state.started = true;
            if state.selected.as_ref() != Some(&choice) {
                let current = self.get();
                let runtime = match (&choice.provider, &choice.model) {
                    (Some(provider), Some(model))
                        if *provider != current.provider_name || *model != current.model =>
                    {
                        let resolver = self
                            .routing
                            .resolver
                            .read()
                            .expect("model runtime resolver lock poisoned")
                            .clone();
                        match resolver {
                            Some(resolve) => match bounded(
                                &request.cancel,
                                resolve(provider.clone(), model.clone()),
                            )
                            .await?
                            {
                                Ok(config) => Some(SharedRuntimeModel::new(config)),
                                Err(error) => {
                                    notice = Some(format!(
                                        "Cannot restore session model selection: {error}"
                                    ));
                                    None
                                }
                            },
                            None => {
                                notice = Some("Cannot restore session model selection: runtime resolver unavailable".into());
                                None
                            }
                        }
                    }
                    (Some(_), Some(_)) => Some(SharedRuntimeModel::new(current)),
                    _ => None,
                };
                state.runtime = runtime;
                state.selected = Some(choice.clone());
            }
            if let Some(effort) = choice.effort.as_deref().filter(|_| {
                (choice.model.is_none() || state.runtime.is_some())
                    && (request.effort_is_session_default
                        || (request.thinking_budget.is_none()
                            && request.max_tokens.is_none()
                            && request.reasoning_effort_ordinal.is_none()))
            }) {
                let runtime = state.runtime.as_ref().unwrap_or(self).get();
                let kind = if runtime.client.provider_name() == "anthropic" {
                    rebon_types::effort_indicator::EffortProviderKind::Anthropic
                } else {
                    rebon_types::effort_indicator::EffortProviderKind::OpenAi
                };
                let value = if effort == "auto" {
                    None
                } else {
                    rebon_types::ReasoningEffort::from_wire_exact(effort)
                };
                let overrides = rebon_api::effort::resolve_thinking_from_effort(value, kind);
                request.thinking_budget = overrides.thinking_budget;
                request.max_tokens = overrides.max_tokens;
                request.reasoning_effort_ordinal = overrides.reasoning_effort_ordinal;
            }
        }
        if request.cancel.is_cancelled() {
            return Err(PromptExecutorError::Cancelled);
        }
        Ok(PreparedModel {
            runtime: state.runtime.clone(),
            notice,
        })
    }
}
