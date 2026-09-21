use super::*;
use rebon_api::{ModelClient, ModelResult, SessionHandle, StreamEventStream};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

#[test]
fn disabled_by_default_and_marked_experimental() {
    assert!(!PLUGIN.default_enabled);
    assert!(PLUGIN.title.contains("Experimental"));
}

fn decision(text: &str) -> anyhow::Result<ModelRoutingDecision> {
    validate(
        text,
        &BTreeSet::from(["gpt-5.4".into(), "plain-model".into()]),
        &serde_json::from_value(
            serde_json::json!({"strong": {"model": "gpt-5.4", "reasoningEffort": "high"}}),
        )
        .unwrap(),
        "openai",
        "gpt-5.4",
    )
}

#[test]
fn valid_model_effort_and_profile() {
    assert_eq!(
        decision(r#"{"model":"plain-model"}"#)
            .unwrap()
            .model
            .as_deref(),
        Some("plain-model")
    );
    assert_eq!(
        decision(r#"{"reasoningEffort":"low"}"#).unwrap(),
        ModelRoutingDecision {
            model: None,
            reasoning_effort: Some(ReasoningEffort::Low)
        }
    );
    assert_eq!(
        decision(r#"{"model":"strong"}"#).unwrap(),
        ModelRoutingDecision {
            model: Some("gpt-5.4".into()),
            reasoning_effort: Some(ReasoningEffort::High)
        }
    );
}

#[test]
fn rejects_invalid_unknown_and_unsupported_outputs() {
    for text in [
        "",
        "{}",
        "[]",
        "```json\n{}\n```",
        r#"{"model":null}"#,
        r#"{"model":42}"#,
        r#"{"model":"missing"}"#,
        r#"{"provider":"other","model":"gpt-5.4"}"#,
        r#"{"model":"gpt-5.4","model":"plain-model"}"#,
        r#"{"reasoningEffort":"HIGH"}"#,
        r#"{"reasoningEffort":"auto"}"#,
        r#"{"model":"plain-model","reasoningEffort":"high"}"#,
    ] {
        assert!(decision(text).is_err(), "accepted {text}");
    }
    assert!(decision(&" ".repeat(OUTPUT_LIMIT + 1)).is_err());
}

#[derive(Clone)]
struct FakeClient {
    requests: Arc<Mutex<Vec<CreateMessageRequest>>>,
    forks: Arc<AtomicUsize>,
    isolated: bool,
    output: serde_json::Value,
}
#[async_trait]
impl ModelClient for FakeClient {
    fn provider_name(&self) -> &'static str {
        "routing-test-provider"
    }
    fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
        self.forks.fetch_add(1, Ordering::SeqCst);
        Some(Arc::new(Self {
            isolated: true,
            ..self.clone()
        }))
    }
    async fn create_message_stream(
        &self,
        _: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        unreachable!()
    }
    async fn create_message(
        &self,
        request: CreateMessageRequest,
    ) -> ModelResult<rebon_api::AssistantMessage> {
        assert!(self.isolated);
        self.requests.lock().unwrap().push(request);
        Ok(serde_json::from_value(self.output.clone()).unwrap())
    }
}

async fn classify(
    settings: serde_json::Value,
    output: serde_json::Value,
) -> (anyhow::Result<ModelRoutingDecision>, FakeClient) {
    let _home = rebon_tool::tasks::test_support::TestConfigHome::new("model-routing-classify");
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("settings.json"),
        serde_json::to_vec(&serde_json::json!({"plugins":{"model-routing":settings}})).unwrap(),
    )
    .unwrap();
    let kernel = rebon_kernel::Kernel::new();
    kernel
        .load(vec![
            Box::new(
                rebon_kernel_seats::kernel_config_seats::ConfigSeatsPlugin::new(root.path().into())
                    .with_cwd(root.path().into()),
            ),
            Box::new(ModelRoutingPlugin),
        ])
        .unwrap();
    let client = FakeClient {
        requests: Arc::default(),
        forks: Arc::default(),
        isolated: false,
        output,
    };
    let router = kernel.context().get::<ModelRoutingService>().unwrap();
    let result = router
        .route(ModelRoutingInput {
            prompt: "raw task only".into(),
            cwd: root.path().into(),
            provider_name: "routing-test-provider".into(),
            model: "active-model".into(),
            model_profiles: ModelProfileMap::default(),
            session: SessionHandle::new(Arc::new(client.clone())),
        })
        .await;
    (result, client)
}

fn response(content: serde_json::Value, stop: &str) -> serde_json::Value {
    serde_json::json!({"id":"routing", "type":"message", "role":"assistant", "model":"active-model", "content":content, "stop_reason":stop, "usage":{"input_tokens":1,"output_tokens":1}})
}

#[tokio::test]
async fn request_is_isolated_bounded_toolless_and_contains_only_raw_prompt() {
    let (result, client) = classify(
        serde_json::json!({"routerModel":"active-model"}),
        response(
            serde_json::json!([{"type":"text","text":"{\"model\":\"active-model\"}"}]),
            "end_turn",
        ),
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(client.forks.load(Ordering::SeqCst), 1);
    let requests = client.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(request.model, "active-model");
    assert_eq!(request.max_tokens, 256);
    assert!(request.tools.is_empty());
    assert!(request.tool_choice.is_none());
    assert!(!request.stream);
    assert_eq!(request.messages.len(), 1);
    assert_eq!(
        request.messages[0].content[0].as_text(),
        Some("raw task only")
    );
}

#[tokio::test]
async fn reasoning_blocks_do_not_replace_or_invalidate_final_json() {
    let thinking = serde_json::json!({
        "type": "thinking",
        "thinking": "{\"model\":\"not-a-candidate\"}"
    });
    let answer = serde_json::json!({
        "type": "text",
        "text": "{\"model\":\"active-model\"}"
    });
    let tool = serde_json::json!({
        "type": "tool_use", "id": "call", "name": "Read", "input": {}
    });
    for (content, succeeds) in [
        (serde_json::json!([thinking.clone(), answer.clone()]), true),
        (serde_json::json!([thinking.clone()]), false),
        (serde_json::json!([thinking, answer, tool]), false),
    ] {
        let (result, _) = classify(
            serde_json::json!({"routerModel":"active-model"}),
            response(content, "end_turn"),
        )
        .await;
        if succeeds {
            assert_eq!(
                result
                    .expect("推理块之外的最终 JSON 应可用于路由")
                    .model
                    .as_deref(),
                Some("active-model")
            );
        } else {
            assert!(result.is_err());
        }
    }
}

#[tokio::test]
async fn bad_configuration_never_calls_provider() {
    for settings in [
        serde_json::json!({}),
        serde_json::json!({"routerModel":""}),
        serde_json::json!({"routerModel":42}),
        serde_json::json!({"routerModel":"other-provider-model"}),
    ] {
        let (result, client) = classify(settings, serde_json::Value::Null).await;
        assert!(result.is_err());
        assert!(client.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn rejects_truncated_and_tool_outputs() {
    for output in [
        response(
            serde_json::json!([{"type":"text","text":"{}"}]),
            "max_tokens",
        ),
        response(
            serde_json::json!([{"type":"tool_use","id":"call","name":"Read","input":{}}]),
            "end_turn",
        ),
    ] {
        assert!(
            classify(serde_json::json!({"routerModel":"active-model"}), output)
                .await
                .0
                .is_err()
        );
    }
}
