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

/// 当前 provider（openai）加一个可换过去的 provider（local）。`strong` 画像
/// 声明 high effort，用来盖住画像自带 effort 的那条路径。
fn candidates() -> Candidates {
    Candidates {
        providers: vec![
            ProviderCandidate {
                id: "openai".into(),
                models: BTreeSet::from(["gpt-5.4".into(), "plain-model".into()]),
                profiles: serde_json::from_value(
                    serde_json::json!({"strong": {"model": "gpt-5.4", "reasoningEffort": "high"}}),
                )
                .unwrap(),
            },
            ProviderCandidate {
                id: "local".into(),
                models: BTreeSet::from(["self-hosted".into()]),
                profiles: ModelProfileMap::default(),
            },
        ],
    }
}

fn decision(text: &str) -> anyhow::Result<ModelRoutingDecision> {
    validate(text, &candidates(), "openai", "gpt-5.4")
}

#[test]
fn valid_model_effort_and_profile() {
    assert_eq!(
        decision(r#"{"model":"plain-model"}"#).unwrap(),
        ModelRoutingDecision {
            provider: None,
            model: Some("plain-model".into()),
            reasoning_effort: None,
        }
    );
    assert_eq!(
        decision(r#"{"reasoningEffort":"low"}"#).unwrap(),
        ModelRoutingDecision {
            provider: None,
            model: None,
            reasoning_effort: Some(ReasoningEffort::Low)
        }
    );
    assert_eq!(
        decision(r#"{"model":"strong"}"#).unwrap(),
        ModelRoutingDecision {
            provider: None,
            model: Some("gpt-5.4".into()),
            reasoning_effort: Some(ReasoningEffort::High)
        }
    );
}

#[test]
fn cross_provider_decision_must_name_its_own_model() {
    assert_eq!(
        decision(r#"{"provider":"local","model":"self-hosted"}"#).unwrap(),
        ModelRoutingDecision {
            provider: Some("local".into()),
            model: Some("self-hosted".into()),
            reasoning_effort: None,
        }
    );
    for text in [
        // 只换 provider：model id 属于服务它的 provider，缺了它无从判断。
        r#"{"provider":"local"}"#,
        // 另一个 provider 的模型不能顶到自己名下。
        r#"{"provider":"local","model":"gpt-5.4"}"#,
        r#"{"provider":"openai","model":"self-hosted"}"#,
        // 目录里没有的 provider 选不了。
        r#"{"provider":"missing","model":"self-hosted"}"#,
    ] {
        assert!(decision(text).is_err(), "accepted {text}");
    }
    // 点回当前 provider 与不点 provider 的决策是同一个。
    assert_eq!(
        decision(r#"{"provider":"openai","model":"plain-model"}"#).unwrap(),
        decision(r#"{"model":"plain-model"}"#).unwrap()
    );
}

#[test]
fn effort_is_checked_against_the_models_the_table_knows() {
    // 表里有行的模型：不支持的 effort 依旧拒绝（gpt-5.4 没有 max）。
    assert!(decision(r#"{"model":"gpt-5.4","reasoningEffort":"max"}"#).is_err());
    assert!(
        decision(r#"{"provider":"openai","model":"gpt-5.4","reasoningEffort":"high"}"#).is_ok()
    );
    // 表里没有的模型（自建 provider 自己的 id）：表不为它表态，路由也不拦，
    // 否则定义了自有模型的安装根本用不了路由。
    for text in [
        r#"{"provider":"local","model":"self-hosted","reasoningEffort":"low"}"#,
        r#"{"model":"plain-model","reasoningEffort":"high"}"#,
    ] {
        assert!(decision(text).is_ok(), "refused {text}");
    }
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
        r#"{"model":"gpt-5.4","reasoningEffort":"max"}"#,
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
async fn the_policy_is_handed_to_the_classifier_trimmed() {
    let (result, client) = classify(
        serde_json::json!({
            "routerModel":"active-model",
            "policy":"  hard work goes to the frontier model  "
        }),
        response(
            serde_json::json!([{"type":"text","text":"{\"model\":\"active-model\"}"}]),
            "end_turn",
        ),
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    let system = client.requests.lock().unwrap()[0]
        .system
        .clone()
        .expect("the classifier is given a system prompt");
    assert!(
        system.contains("hard work goes to the frontier model"),
        "{system}"
    );
    assert!(
        system.contains("decides over the cost preference"),
        "a policy has to say it outranks the default bias: {system}"
    );
}

#[tokio::test]
async fn without_a_policy_the_classifier_keeps_the_cost_bias() {
    for policy in [serde_json::Value::Null, serde_json::json!("   ")] {
        let (result, client) = classify(
            serde_json::json!({"routerModel":"active-model","policy":policy}),
            response(
                serde_json::json!([{"type":"text","text":"{\"model\":\"active-model\"}"}]),
                "end_turn",
            ),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        let system = client.requests.lock().unwrap()[0]
            .system
            .clone()
            .expect("the classifier is given a system prompt");
        assert!(system.contains("cheapest"), "{system}");
        assert!(
            !system.contains("routing policy"),
            "a blank policy must not be spelled out: {system}"
        );
    }
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

#[test]
fn candidates_group_usable_providers_and_drop_the_ones_with_nothing_to_offer() {
    use rebon_provider::provider_catalog::{ProviderCatalogEntry, ProviderOrigin};
    let entry = |id: &str, models: &[&str], unusable: bool| ProviderCatalogEntry {
        id: id.into(),
        display_name: id.into(),
        origin: ProviderOrigin::User,
        is_active: false,
        format: None,
        base_url: None,
        api_key_masked: None,
        default_model: None,
        models: models.iter().map(|model| (*model).to_owned()).collect(),
        model_profiles: ModelProfileMap::default(),
        unusable_reason: unusable.then(|| "no credentials".to_string()),
    };
    let catalog = vec![
        entry("openai", &["gpt-5.4"], false),
        entry("local", &["self-hosted"], false),
        entry("plugged", &["whatever"], true),
        entry("nothing", &[], false),
    ];
    let input = ModelRoutingInput {
        prompt: "raw task".into(),
        cwd: ".".into(),
        provider_name: "openai".into(),
        model: "active-model".into(),
        model_profiles: ModelProfileMap::default(),
        session: SessionHandle::new(Arc::new(FakeClient {
            requests: Arc::default(),
            forks: Arc::default(),
            isolated: false,
            output: serde_json::Value::Null,
        })),
    };
    let candidates = Candidates::build(&catalog, &input);
    let ids: Vec<&str> = candidates
        .providers
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect();
    assert_eq!(ids, ["openai", "local"]);
    // 当前模型即使不在目录里也可选；别的 provider 只出自己声明的模型。
    assert!(candidates
        .get("openai")
        .unwrap()
        .models
        .contains("active-model"));
    assert!(candidates
        .get("local")
        .unwrap()
        .models
        .contains("self-hosted"));
    let prompt = candidates.prompt_json().unwrap();
    assert!(prompt.contains("\"provider\":\"local\""), "{prompt}");
    assert!(prompt.contains("self-hosted"), "{prompt}");
    assert!(
        !prompt.contains("plugged") && !prompt.contains("nothing"),
        "{prompt}"
    );
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

/// 后端设置：没写或写了空白就是文字后端（老配置照旧），两个名字各选一个，
/// 别的值要让这一轮路由带着原因跳过而不是悄悄换个后端。
#[test]
fn the_backend_setting_chooses_the_classifier_and_refuses_anything_else() {
    for (settings, expected) in [
        (serde_json::json!({}), Backend::Prompt),
        (serde_json::json!({"backend": "prompt"}), Backend::Prompt),
        (
            serde_json::json!({"backend": "  prompt  "}),
            Backend::Prompt,
        ),
        (serde_json::json!({"backend": ""}), Backend::Prompt),
        (serde_json::json!({"backend": "jev"}), Backend::TypeSafe),
        (
            serde_json::json!({"backend": "typesafe"}),
            Backend::TypeSafe,
        ),
    ] {
        assert_eq!(
            backend(&settings).expect("reads"),
            expected,
            "settings: {settings}"
        );
    }
    for settings in [
        serde_json::json!({"backend": "automatic"}),
        serde_json::json!({"backend": 42}),
    ] {
        let error = backend(&settings).expect_err("refused");
        assert!(error.to_string().contains("backend"), "{error}");
    }
}

/// 文字后端仍然要 routerModel，TypeSafe 后端不读它：同一个设置文件里留着一条
/// 属于另一个后端的值，不该影响这一轮。
#[tokio::test]
async fn the_text_backend_still_needs_a_router_model() {
    let (result, client) = classify(
        serde_json::json!({"backend": "prompt"}),
        serde_json::Value::Null,
    )
    .await;
    let error = result.expect_err("refused");
    assert!(error.to_string().contains("routerModel"), "{error}");
    assert!(client.requests.lock().unwrap().is_empty());
}
