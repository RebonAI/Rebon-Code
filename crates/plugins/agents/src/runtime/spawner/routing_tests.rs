use super::*;
use rebon_agent_core::model_router::ResolvedModelRuntime;
use rebon_api::MockModelClient;
use rebon_core::model_routing::{
    FirstPromptModelRouter, ModelRoutingDecision, ModelRoutingInput, ModelRoutingService,
};
use std::sync::atomic::AtomicUsize;

struct Router {
    calls: AtomicUsize,
    mode: &'static str,
}
#[async_trait::async_trait]
impl FirstPromptModelRouter for Router {
    async fn route(&self, input: ModelRoutingInput) -> anyhow::Result<ModelRoutingDecision> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(input.prompt, "raw task");
        assert_eq!(input.provider_name, "mock");
        match self.mode {
            "error" => anyhow::bail!("mock failure"),
            "pending" => std::future::pending().await,
            _ => Ok(ModelRoutingDecision {
                model: Some("selected".into()),
                reasoning_effort: Some(ReasoningEffort::Low),
            }),
        }
    }
}
fn setup(
    mode: &'static str,
    enabled: bool,
) -> (
    Arc<rebon_kernel::Kernel>,
    Arc<Engine>,
    EngineSubAgentSpawner,
    Arc<Router>,
    ResolvedModelRuntime,
) {
    let kernel = rebon_kernel::Kernel::new();
    let router = Arc::new(Router {
        calls: AtomicUsize::new(0),
        mode,
    });
    if enabled {
        kernel
            .context()
            .provide::<ModelRoutingService>(router.clone())
            .unwrap();
    }
    let engine = Arc::new(Engine::new());
    engine.attach_upstream_tool_context(kernel.context().clone());
    let client: Arc<dyn ModelClient> = Arc::new(MockModelClient::new());
    let current = ResolvedModelRuntime {
        provider_name: "mock".into(),
        client: client.clone(),
        model: "initial".into(),
        reasoning_effort: None,
    };
    let spawner =
        EngineSubAgentSpawner::new(Arc::downgrade(&engine), client).with_default_model("initial");
    (kernel, engine, spawner, router, current)
}
fn spec(parent: &str) -> SubAgentSpec {
    let mut spec = SubAgentSpec::new("raw task");
    spec.metadata = serde_json::json!({"parent_session_id": parent});
    spec
}

#[tokio::test]
async fn automatic_routing_once_per_task_and_parent_isolation() {
    let (kernel, _engine, spawner, router, current) = setup("ok", true);
    let notices = Arc::new(Mutex::new(Vec::new()));
    let capture = notices.clone();
    let bridge = kernel.context().fork("test-routing-notice");
    bridge.on(
        move |notice: &rebon_core::model_routing::ModelRoutingNotice| {
            capture
                .lock()
                .unwrap()
                .push((notice.session_id.clone(), notice.text.clone()));
        },
    );
    for (parent, id) in [("p1", "one"), ("p1", "one"), ("p1", "two"), ("p2", "one")] {
        let (selected, auto) = spawner
            .automatically_route_worker(
                &spec(parent),
                id,
                ModelRouteRequest::default(),
                current.clone(),
            )
            .await
            .unwrap();
        assert!(auto);
        assert_eq!(selected.model, "selected");
        assert_eq!(selected.reasoning_effort, Some(ReasoningEffort::Low));
    }
    assert_eq!(current.model, "initial");
    assert_eq!(router.calls.load(Ordering::SeqCst), 3);
    let notices = notices.lock().unwrap();
    assert_eq!(notices.len(), 3);
    assert_eq!(notices[0].0, "p1");
    assert!(notices[0].1.contains("with low effort"));
}

#[tokio::test]
async fn automatic_routing_disabled_and_explicit_overrides() {
    let (_kernel, _engine, spawner, router, current) = setup("ok", false);
    assert!(
        !spawner
            .automatically_route_worker(&spec("p"), "one", ModelRouteRequest::default(), current)
            .await
            .unwrap()
            .1
    );
    assert_eq!(router.calls.load(Ordering::SeqCst), 0);
    let (_kernel, _engine, spawner, router, current) = setup("ok", true);
    for request in [
        ModelRouteRequest {
            model: Some("manual".into()),
            ..Default::default()
        },
        ModelRouteRequest {
            model_profile: Some("cheap".into()),
            ..Default::default()
        },
        ModelRouteRequest {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        },
    ] {
        assert!(
            !spawner
                .automatically_route_worker(&spec("p"), "one", request, current.clone())
                .await
                .unwrap()
                .1
        );
    }
    assert_eq!(router.calls.load(Ordering::SeqCst), 0);
    spawner
        .automatically_route_worker(
            &spec("p"),
            "two",
            ModelRouteRequest::default(),
            current.clone(),
        )
        .await
        .unwrap();
    let mut manual = current.clone();
    manual.model = "manual".into();
    let request = ModelRouteRequest {
        model: Some("manual".into()),
        ..Default::default()
    };
    assert_eq!(
        spawner
            .automatically_route_worker(&spec("p"), "two", request, manual)
            .await
            .unwrap()
            .0
            .model,
        "manual"
    );
    assert_eq!(
        spawner
            .automatically_route_worker(&spec("p"), "two", ModelRouteRequest::default(), current)
            .await
            .unwrap()
            .0
            .model,
        "manual"
    );
    assert_eq!(router.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn automatic_routing_respects_definition_model_profile_and_effort() {
    for selection in [
        rebon_types::SubAgentModelSelection::model("defined"),
        rebon_types::SubAgentModelSelection::model_profile("cheap"),
        rebon_types::SubAgentModelSelection::new(None, None, Some(ReasoningEffort::High)),
    ] {
        let (_kernel, _engine, mut spawner, router, current) = setup("ok", true);
        spawner.model_config.insert_agent("worker", selection);
        let request = ModelRouteRequest {
            agent_type: Some("worker".into()),
            ..Default::default()
        };
        assert!(
            !spawner
                .automatically_route_worker(&spec("p"), "one", request, current)
                .await
                .unwrap()
                .1
        );
        assert_eq!(router.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn automatic_routing_failure_timeout_and_dropped_future_never_retry() {
    for mode in ["error", "pending"] {
        let (_kernel, _engine, spawner, router, current) = setup(mode, true);
        for _ in 0..2 {
            assert_eq!(
                spawner
                    .automatically_route_worker(
                        &spec("p"),
                        "one",
                        ModelRouteRequest::default(),
                        current.clone()
                    )
                    .await
                    .unwrap()
                    .0
                    .model,
                "initial"
            );
        }
        assert_eq!(router.calls.load(Ordering::SeqCst), 1);
    }
    let (_kernel, _engine, spawner, router, current) = setup("pending", true);
    let task = spec("p");
    {
        let future = spawner.automatically_route_worker(
            &task,
            "one",
            ModelRouteRequest::default(),
            current.clone(),
        );
        tokio::pin!(future);
        tokio::select! { biased; _ = &mut future => panic!("pending classifier completed"), _ = tokio::task::yield_now() => {} }
    }
    assert_eq!(
        spawner
            .automatically_route_worker(&task, "one", ModelRouteRequest::default(), current)
            .await
            .unwrap()
            .0
            .model,
        "initial"
    );
    assert_eq!(router.calls.load(Ordering::SeqCst), 1);
}
