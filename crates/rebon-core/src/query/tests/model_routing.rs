use super::*;
use crate::model_routing::{
    FirstPromptModelRouter, ModelRoutingDecision, ModelRoutingInput, ModelRoutingService,
};
use rebon_session::model_selection::{self, SessionModelSelection};
use rebon_types::ReasoningEffort;

struct Router {
    calls: AtomicUsize,
    mode: &'static str,
}
#[async_trait]
impl FirstPromptModelRouter for Router {
    async fn route(&self, input: ModelRoutingInput) -> anyhow::Result<ModelRoutingDecision> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(input.prompt, "raw task");
        match self.mode {
            "error" => anyhow::bail!("mock classification failure"),
            "pending" => std::future::pending().await,
            _ => Ok(ModelRoutingDecision {
                provider: (self.mode == "provider").then(|| "other".into()),
                model: (self.mode != "effort").then(|| "selected".into()),
                reasoning_effort: Some(ReasoningEffort::High),
            }),
        }
    }
}
fn router(mode: &'static str) -> Arc<Router> {
    Arc::new(Router {
        calls: AtomicUsize::new(0),
        mode,
    })
}
fn config(client: Arc<dyn ModelClient>, model: &str) -> RuntimeModelConfig {
    RuntimeModelConfig {
        provider_name: "mock".into(),
        client,
        model: model.into(),
        model_profiles: Default::default(),
        title_model: "title".into(),
        model_marketing_name: Some(format!("runtime-{model}")),
        knowledge_cutoff: None,
        prune_level: None,
        compact_provider: None,
        compact_fallback_provider: None,
        context_management: None,
        reasoning_mode: None,
    }
}
fn runtime(client: Arc<dyn ModelClient>) -> SharedRuntimeModel {
    let runtime = SharedRuntimeModel::new(config(client.clone(), "initial"));
    runtime.set_runtime_resolver(Arc::new(move |provider, model| {
        assert_eq!(provider, "mock");
        let value = config(client.clone(), &model);
        Box::pin(async move { Ok(value) })
    }));
    runtime
}
/// 一个把请求的 provider 都答出来的 runtime，用来看路由真的换了腿。
fn switching_runtime(
    client: Arc<dyn ModelClient>,
    asked: Arc<std::sync::Mutex<Vec<String>>>,
) -> SharedRuntimeModel {
    let runtime = SharedRuntimeModel::new(config(client.clone(), "initial"));
    runtime.set_runtime_resolver(Arc::new(move |provider, model| {
        asked.lock().unwrap().push(provider.clone());
        let mut value = config(client.clone(), &model);
        value.provider_name = provider;
        Box::pin(async move { Ok(value) })
    }));
    runtime
}
fn request(id: &str, cwd: &str) -> PromptRequest {
    let mut request = test_prompt_request(id, cwd, "raw task with attachments", None);
    request.user_prompt = Some("raw task".into());
    request.effort_is_session_default = true;
    request
}

#[tokio::test]
async fn disabled_and_synthetic_requests_have_no_routing_writes() {
    let root = temp_projects_root("routing_disabled");
    let runtime = runtime(Arc::new(MockModelClient::new()));
    let mut req = request("disabled", "work");
    let result = runtime
        .prepare_first_prompt(root.path().into(), &mut req, false, None)
        .await
        .unwrap();
    assert!(result.runtime.is_none() && result.notice.is_none());
    assert!(std::fs::read_dir(root.path()).unwrap().next().is_none());
    let router = router("ok");
    req.user_prompt = None;
    runtime
        .prepare_first_prompt(root.path().into(), &mut req, false, Some(router.clone()))
        .await
        .unwrap();
    assert_eq!(router.calls.load(Ordering::SeqCst), 0);
    assert!(model_selection::load(root.path(), "work", "disabled")
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn once_only_full_runtime_rebuild_resume_rewind_and_manual_isolation() {
    let root = temp_projects_root("routing_once");
    let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
    let base = runtime(client.clone());
    let fork = base.fork_session();
    let router = router("ok");
    for id in ["one", "two"] {
        let mut req = request(id, "work");
        let selected = fork
            .prepare_first_prompt(root.path().into(), &mut req, false, Some(router.clone()))
            .await
            .unwrap();
        let config = selected.runtime.unwrap().get();
        assert_eq!(config.model, "selected");
        assert_eq!(
            config.model_marketing_name.as_deref(),
            Some("runtime-selected")
        );
        assert_eq!(req.reasoning_effort_ordinal, Some(2));
        assert!(selected.notice.unwrap().contains("with high effort"));
    }
    assert_eq!(base.get().model, "initial");
    assert_eq!(fork.get().model, "initial");
    model_selection::save_manual_model(root.path(), "work", "one", "mock", "manual").unwrap();
    model_selection::save_manual_effort(root.path(), "work", "one", Some(ReasoningEffort::Low))
        .unwrap();
    let restored = runtime(client);
    for (id, model, effort) in [("one", "manual", 0), ("two", "selected", 2)] {
        let mut req = request(id, "work");
        let result = restored
            .prepare_first_prompt(root.path().into(), &mut req, false, Some(router.clone()))
            .await
            .unwrap();
        assert_eq!(result.runtime.unwrap().get().model, model);
        assert_eq!(req.reasoning_effort_ordinal, Some(effort));
        assert!(result.notice.is_none());
    }
    assert_eq!(router.calls.load(Ordering::SeqCst), 2);

    let unavailable = runtime(Arc::new(MockModelClient::new()));
    unavailable.set_runtime_resolver(Arc::new(|_, _| {
        Box::pin(async { anyhow::bail!("selected model unavailable") })
    }));
    let mut req = request("two", "work");
    req.reasoning_effort_ordinal = Some(0);
    let result = unavailable
        .prepare_first_prompt(root.path().into(), &mut req, false, None)
        .await
        .unwrap();
    assert!(result.runtime.is_none());
    assert!(result.notice.unwrap().contains("Cannot restore"));
    assert_eq!(req.reasoning_effort_ordinal, Some(0));
}

#[tokio::test]
async fn effort_only_and_explicit_turn_overrides_win() {
    let root = temp_projects_root("routing_effort");
    let runtime = runtime(Arc::new(MockModelClient::new()));
    let router = router("effort");
    let mut req = request("one", "work");
    assert_eq!(
        runtime
            .prepare_first_prompt(root.path().into(), &mut req, false, Some(router.clone()))
            .await
            .unwrap()
            .runtime
            .unwrap()
            .get()
            .model,
        "initial"
    );
    let mut req = request("one", "work");
    req.effort_is_session_default = false;
    req.thinking_budget = Some(1111);
    req.max_tokens = Some(2222);
    req.reasoning_effort_ordinal = Some(0);
    runtime
        .prepare_first_prompt(root.path().into(), &mut req, false, Some(router.clone()))
        .await
        .unwrap();
    assert_eq!(
        (
            req.thinking_budget,
            req.max_tokens,
            req.reasoning_effort_ordinal
        ),
        (Some(1111), Some(2222), Some(0))
    );
    model_selection::save_manual_effort(root.path(), "work", "one", None).unwrap();
    let mut req = request("one", "work");
    runtime
        .prepare_first_prompt(root.path().into(), &mut req, false, None)
        .await
        .unwrap();
    assert_eq!(req.reasoning_effort_ordinal, Some(2));
    assert_eq!(router.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn errors_and_timeouts_warn_and_never_retry() {
    for mode in ["error", "pending"] {
        let root = temp_projects_root("routing_failure");
        let runtime = runtime(Arc::new(MockModelClient::new()));
        let router = router(mode);
        let mut req = request("one", "work");
        let result = runtime
            .prepare_first_prompt(root.path().into(), &mut req, false, Some(router.clone()))
            .await
            .unwrap();
        assert!(result.notice.unwrap().contains(if mode == "error" {
            "mock classification failure"
        } else {
            "timed out"
        }));
        assert!(result.runtime.is_none());
        runtime
            .prepare_first_prompt(root.path().into(), &mut req, false, Some(router.clone()))
            .await
            .unwrap();
        assert_eq!(router.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn cross_provider_decision_switches_the_session_runtime() {
    let root = temp_projects_root("routing_provider");
    let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    let runtime = switching_runtime(client, asked.clone());
    let router = router("provider");
    let mut req = request("one", "work");
    let selected = runtime
        .prepare_first_prompt(root.path().into(), &mut req, false, Some(router.clone()))
        .await
        .unwrap();
    let config = selected.runtime.unwrap().get();
    assert_eq!(config.provider_name, "other");
    assert_eq!(config.model, "selected");
    assert_eq!(asked.lock().unwrap().as_slice(), ["other".to_string()]);
    let notice = selected.notice.unwrap();
    assert!(notice.contains("other / selected"), "{notice}");
    let saved = model_selection::load(root.path(), "work", "one")
        .unwrap()
        .unwrap();
    assert_eq!(saved.provider.as_deref(), Some("other"));
    assert_eq!(saved.model.as_deref(), Some("selected"));
    // 同一会话的后续 turn 继续用换过去的腿，不再分类。
    let mut next = request("one", "work");
    let again = runtime
        .prepare_first_prompt(root.path().into(), &mut next, false, Some(router.clone()))
        .await
        .unwrap();
    assert_eq!(again.runtime.unwrap().get().provider_name, "other");
    assert_eq!(router.calls.load(Ordering::SeqCst), 1);
    assert_eq!(asked.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn resolver_returning_another_provider_is_refused() {
    let root = temp_projects_root("routing_provider_drift");
    let runtime = runtime(Arc::new(MockModelClient::new()));
    runtime.set_runtime_resolver(Arc::new(|_, model| {
        let mut value = config(Arc::new(MockModelClient::new()), &model);
        value.provider_name = "drifted".into();
        Box::pin(async move { Ok(value) })
    }));
    let mut req = request("one", "work");
    let result = runtime
        .prepare_first_prompt(
            root.path().into(),
            &mut req,
            false,
            Some(router("provider")),
        )
        .await
        .unwrap();
    assert!(result.runtime.is_none());
    let notice = result.notice.unwrap();
    assert!(
        notice.contains("resolver returned a different provider than requested"),
        "{notice}"
    );
    // 分类没成功，选型就没有写进 sidecar。
    let saved = model_selection::load(root.path(), "work", "one")
        .unwrap()
        .unwrap();
    assert_eq!(saved.provider, None);
}

#[tokio::test]
async fn cancellation_is_not_swallowed_and_consumes_attempt() {
    let root = temp_projects_root("routing_cancel");
    let runtime = runtime(Arc::new(MockModelClient::new()));
    let router = router("pending");
    let mut req = request("one", "work");
    let cancel = req.cancel.clone();
    let run =
        runtime.prepare_first_prompt(root.path().into(), &mut req, false, Some(router.clone()));
    let cancel_task = async {
        tokio::task::yield_now().await;
        cancel.cancel();
    };
    let (result, _) = tokio::join!(run, cancel_task);
    assert!(matches!(result, Err(PromptExecutorError::Cancelled)));
    let mut next = request("one", "work");
    runtime
        .prepare_first_prompt(root.path().into(), &mut next, false, Some(router.clone()))
        .await
        .unwrap();
    assert_eq!(router.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn existing_user_history_suppresses_classification_even_after_rewind() {
    let root = temp_projects_root("routing_prior");
    let runtime = runtime(Arc::new(MockModelClient::new()));
    let router = router("ok");
    let mut req = request("one", "work");
    runtime
        .prepare_first_prompt(root.path().into(), &mut req, true, Some(router.clone()))
        .await
        .unwrap();
    runtime
        .prepare_first_prompt(root.path().into(), &mut req, false, Some(router.clone()))
        .await
        .unwrap();
    assert_eq!(router.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn executor_notice_is_ui_only_and_loaded_history_does_not_route() {
    let root = temp_projects_root("routing_notice");
    let cwd = root.path().to_string_lossy().into_owned();
    let kernel = rebon_kernel::Kernel::new();
    let router = router("ok");
    kernel
        .context()
        .provide::<ModelRoutingService>(router.clone())
        .unwrap();
    let engine = Arc::new(Engine::new());
    engine.attach_upstream_tool_context(kernel.context().clone());
    let client = MockModelClient::new();
    for id in ["a", "b", "c"] {
        client.push_turn(text_turn(id, "done"));
    }
    let publisher = Arc::new(MemorySessionUpdatePublisher::new());
    let state = Arc::new(rebon_session_state::ServerState::new());
    insert_session_with_transcript(&state, "new", &cwd, vec![]);
    insert_session_with_transcript(
        &state,
        "old",
        &cwd,
        vec![make_user_entry("old-user", "old task")],
    );
    state.set_session_title("new", "new task".into());
    state.set_session_title("old", "old task".into());
    let runtime = runtime(Arc::new(client.clone()));
    let executor =
        EngineQueryExecutor::new(engine, Arc::new(client.clone()), root.path(), "initial")
            .with_shared_runtime_model(runtime)
            .with_server_state(state);
    for id in ["new", "new", "old"] {
        let mut req = request(id, &cwd);
        req.update_publisher = Some(publisher.clone());
        executor.execute(req).await.unwrap();
    }
    let updates = serde_json::to_string(&publisher.snapshot()).unwrap();
    assert!(updates.contains("Auto switched to mock / selected with high effort"));
    assert_eq!(router.calls.load(Ordering::SeqCst), 1);
    let requests = client.captured_requests();
    assert_eq!(requests[0].model, "selected");
    assert_eq!(requests[1].model, "selected");
    assert_eq!(requests[2].model, "initial");
    for req in requests {
        assert!(!serde_json::to_string(&req.messages)
            .unwrap()
            .contains("Auto switched"));
    }
    let path = rebon_session::transcript_file_path(root.path(), &cwd, "new");
    assert!(!std::fs::read_to_string(path)
        .unwrap()
        .contains("Auto switched"));
}

#[tokio::test]
async fn cancellation_never_runs_main_request_and_concurrent_turns_route_once() {
    let root = temp_projects_root("routing_concurrent");
    let kernel = rebon_kernel::Kernel::new();
    let pending = router("pending");
    kernel
        .context()
        .provide::<ModelRoutingService>(pending.clone())
        .unwrap();
    let engine = Arc::new(Engine::new());
    engine.attach_upstream_tool_context(kernel.context().clone());
    let client = Arc::new(MockModelClient::new());
    let runtime = runtime(client.clone());
    let executor = EngineQueryExecutor::new(engine, client.clone(), root.path(), "initial")
        .with_shared_runtime_model(runtime.clone());
    let req = request("cancelled", "work");
    let cancel = req.cancel.clone();
    let (result, _) = tokio::join!(executor.execute(req), async {
        tokio::task::yield_now().await;
        cancel.cancel();
    });
    assert!(matches!(result, Err(PromptExecutorError::Cancelled)));
    assert!(client.captured_requests().is_empty());
    let router = router("ok");
    let mut first = request("concurrent", "work");
    let mut second = request("concurrent", "work");
    let (a, b) = tokio::join!(
        runtime.prepare_first_prompt(root.path().into(), &mut first, false, Some(router.clone())),
        runtime.prepare_first_prompt(root.path().into(), &mut second, false, Some(router.clone()))
    );
    assert_eq!(a.unwrap().runtime.unwrap().get().model, "selected");
    assert_eq!(b.unwrap().runtime.unwrap().get().model, "selected");
    assert_eq!(router.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn child_routing_notice_bridge_targets_only_parent_session() {
    let kernel = rebon_kernel::Kernel::new();
    let runtime = runtime(Arc::new(MockModelClient::new()));
    let one = Arc::new(MemorySessionUpdatePublisher::new());
    let two = Arc::new(MemorySessionUpdatePublisher::new());
    for (id, publisher) in [("one", one.clone()), ("two", two.clone())] {
        let mut req = request(id, "work");
        req.update_publisher = Some(publisher);
        runtime.bind_routing_notices(kernel.context(), &req);
        runtime.bind_routing_notices(kernel.context(), &req);
    }
    kernel
        .context()
        .emit(&crate::model_routing::ModelRoutingNotice {
            session_id: "one".into(),
            text: "child route".into(),
        });
    tokio::task::yield_now().await;
    assert_eq!(one.snapshot().len(), 1);
    assert!(two.snapshot().is_empty());
    assert!(matches!(
        &one.snapshot()[0].update,
        rebon_types::SessionUpdate::SessionInfoUpdate { .. }
    ));
}

#[test]
fn corrupt_metadata_is_reported_and_manual_cas_wins() {
    let root = temp_projects_root("routing_metadata");
    let empty = SessionModelSelection::default();
    assert!(model_selection::claim(root.path(), "work", "one").unwrap());
    model_selection::save_manual_model(root.path(), "work", "one", "mock", "manual").unwrap();
    assert!(
        !model_selection::compare_exchange(root.path(), "work", "one", &empty, &empty).unwrap()
    );
    rebon_session::update_session_metadata(root.path(), "work", "one", |object| {
        object.insert(
            "firstPromptModelRouting".into(),
            json!({"effort":"invalid"}),
        );
    })
    .unwrap();
    assert_eq!(
        model_selection::compare_exchange(root.path(), "work", "one", &empty, &empty)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
}

/// A routing plugin that registers nothing: what matters to a resumed session
/// is whether the process withholds the plugin, not what it provides.
struct InertRoutingPlugin;
impl rebon_kernel::Plugin for InertRoutingPlugin {
    fn meta(&self) -> rebon_kernel::PluginMeta {
        rebon_kernel::PluginMeta::new(crate::model_routing::MODEL_ROUTING_PLUGIN_ID)
    }
    fn apply(&self, _ctx: &rebon_kernel::Context) -> Result<(), rebon_kernel::KernelError> {
        Ok(())
    }
}

/// The session a terminal routed and another surface now resumes: a routed
/// sidecar on disk, and an executor on a kernel whose routing plugin is off
/// (by a switch) or withheld (by the entry point).
async fn resume_routed_session(withheld: bool) -> String {
    let root = temp_projects_root(if withheld {
        "routing_resume_withheld"
    } else {
        "routing_resume_switched_off"
    });
    let routed = SessionModelSelection {
        provider: Some("mock".into()),
        model: Some("routed".into()),
        effort: Some("low".into()),
        manual_override: false,
    };
    assert!(model_selection::claim(root.path(), "work", "resumed").unwrap());
    assert!(model_selection::compare_exchange(
        root.path(),
        "work",
        "resumed",
        &SessionModelSelection::default(),
        &routed,
    )
    .unwrap());

    let kernel = rebon_kernel::Kernel::new();
    let registry = rebon_kernel::PluginRegistry::new(
        kernel.clone(),
        &[rebon_kernel::PluginDef {
            id: crate::model_routing::MODEL_ROUTING_PLUGIN_ID,
            title: "routing",
            kind: rebon_kernel::PluginKind::Feature,
            default_enabled: false,
            factory: |_| Ok(Box::new(InertRoutingPlugin)),
        }],
        rebon_kernel::PluginHost {
            kernel: kernel.clone(),
            config_dir: root.path().to_path_buf(),
        },
    );
    if withheld {
        registry
            .withhold(&[crate::model_routing::MODEL_ROUTING_PLUGIN_ID])
            .unwrap();
    }
    registry.reconcile(&rebon_kernel::DesiredSet::new());

    let engine = Arc::new(Engine::new());
    engine.attach_upstream_tool_context(kernel.context().clone());
    let client = MockModelClient::new();
    for id in ["resumed", "title", "spare"] {
        client.push_turn(text_turn(id, "done"));
    }
    let executor =
        EngineQueryExecutor::new(engine, Arc::new(client.clone()), root.path(), "initial")
            .with_shared_runtime_model(runtime(Arc::new(client.clone())));
    executor.execute(request("resumed", "work")).await.unwrap();

    // The new session also asks the title model for a title; the turn is the
    // other request.
    let turns: Vec<String> = client
        .captured_requests()
        .into_iter()
        .map(|request| request.model)
        .filter(|model| model != "title")
        .collect();
    assert_eq!(turns.len(), 1, "{turns:?}");
    let model = turns[0].clone();
    assert_eq!(
        model_selection::load(root.path(), "work", "resumed").unwrap(),
        Some(routed),
        "the sidecar was rewritten"
    );
    model
}

#[tokio::test]
async fn a_withheld_router_does_not_restore_a_routed_session_and_leaves_its_sidecar() {
    let model = resume_routed_session(true).await;
    assert_eq!(
        model, "initial",
        "the routed choice overrode the caller's model"
    );
}

#[tokio::test]
async fn a_router_switched_off_still_restores_the_routed_session() {
    let model = resume_routed_session(false).await;
    assert_eq!(model, "routed");
}
