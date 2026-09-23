//! The TypeSafe backend, against a loopback stand-in for the API.
//!
//! The fake is the smallest thing that can answer one HTTP request and record
//! it — the same shape `image-gen`'s tool tests use: no mock-server crate, no
//! network, and every assertion is about bytes this process sent or received.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rebon_api::{ModelClient, ModelResult, SessionHandle, StreamEventStream};
use rebon_types::ModelProfileMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use rebon_api::typesafe::{api_key_from, SystemOneClient, DEFAULT_MODEL};

/// One request the fake received.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    authorization: String,
    body: serde_json::Value,
}

/// A loopback server that answers each request with the next canned
/// `(status, body)` and records what it was sent.
///
/// A status of `0` holds the connection open without answering, which is how a
/// timeout is provoked; the test's own client timeout ends it.
struct FakeApi {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeApi {
    async fn start(responses: Vec<(u16, String)>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let recorded = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let request = read_request(&mut socket).await;
                recorded.lock().expect("seen").push(request);
                let (status, body) = responses
                    .lock()
                    .expect("responses")
                    .pop_front()
                    .unwrap_or((500, "no canned response left".into()));
                if status == 0 {
                    // Stall: the client's own timeout is what ends this.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        Self { base, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("seen").clone()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Seen {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.expect("read request");
        assert!(read > 0, "connection closed before the headers ended");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let header = |name: &str| {
        head.lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_string())
            })
            .unwrap_or_default()
    };
    let length: usize = header("content-length").parse().unwrap_or(0);
    while buffer.len() < header_end + length {
        let read = socket.read(&mut chunk).await.expect("read body");
        assert!(read > 0, "connection closed before the body ended");
        buffer.extend_from_slice(&chunk[..read]);
    }
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    Seen {
        path,
        authorization: header("authorization"),
        body: serde_json::from_slice(&buffer[header_end..header_end + length])
            .unwrap_or(serde_json::Value::Null),
    }
}

// ── fixtures ─────────────────────────────────────────────────────

const PATH: &str = "/v1/systemone";

/// 路由的候选：provider 名加它列的模型。
fn candidates_of(providers: &[(&str, &[&str])]) -> Candidates {
    Candidates {
        providers: providers
            .iter()
            .map(|(id, models)| ProviderCandidate {
                id: (*id).to_string(),
                models: models.iter().map(|model| (*model).to_string()).collect(),
                profiles: ModelProfileMap::default(),
            })
            .collect(),
    }
}

/// TypeSafe 后端不碰会话：这个 client 除了证明"没被调用"什么也不做。
struct UnusedSession;

#[async_trait]
impl ModelClient for UnusedSession {
    fn provider_name(&self) -> &'static str {
        "unused-session-provider"
    }
    fn fork_for_sub_agent(&self) -> Option<Arc<dyn ModelClient>> {
        unreachable!("the TypeSafe backend asks no provider")
    }
    async fn create_message_stream(
        &self,
        _: CreateMessageRequest,
    ) -> ModelResult<StreamEventStream> {
        unreachable!("the TypeSafe backend asks no provider")
    }
    async fn create_message(
        &self,
        _: CreateMessageRequest,
    ) -> ModelResult<rebon_api::AssistantMessage> {
        unreachable!("the TypeSafe backend asks no provider")
    }
}

fn input(prompt: &str) -> ModelRoutingInput {
    ModelRoutingInput {
        prompt: prompt.into(),
        cwd: ".".into(),
        provider_name: "openai".into(),
        model: "gpt-5.4".into(),
        model_profiles: ModelProfileMap::default(),
        session: SessionHandle::new(Arc::new(UnusedSession)),
    }
}

/// A client pointed at the fake, with a timeout short enough for a test to
/// reach the timeout branch.
fn client(api: &FakeApi) -> SystemOneClient {
    SystemOneClient::new(
        &format!("{}{PATH}", api.base),
        Some("test-key".into()),
        Duration::from_millis(300),
    )
    .expect("a key was given")
}

fn answer(choice: &str, confidence: f64) -> serde_json::Value {
    serde_json::json!({
        "type": "choice",
        "choice": choice,
        "confidence": confidence,
        "probabilities": {"a": confidence, "b": 1.0 - confidence}
    })
}

fn body(target: &str, effort: &str, confidence: f64) -> String {
    serde_json::json!({
        "model": "jev-1.13.0",
        "answers": {
            crate::jev::TARGET_QUESTION: answer(target, confidence),
            crate::jev::EFFORT_QUESTION: answer(effort, confidence),
        },
        "usage": {"input_tokens": 10, "output_tokens": 2}
    })
    .to_string()
}

#[test]
fn gateway_and_typesafe_keys_stay_bound_to_their_endpoints() {
    use rebon_api::typesafe::{api_key_for_endpoint, VERCEL_ENDPOINT};
    let keys = |name: &str| match name {
        "TYPESAFE_API_KEY" => Some("official-key".to_string()),
        "AI_GATEWAY_API_KEY" => Some("gateway-key".to_string()),
        _ => None,
    };
    assert_eq!(
        api_key_for_endpoint(rebon_api::typesafe::DEFAULT_ENDPOINT, keys),
        Some("official-key".into())
    );
    assert_eq!(
        api_key_for_endpoint(VERCEL_ENDPOINT, keys),
        Some("gateway-key".into())
    );
    assert_eq!(
        api_key_for_endpoint(VERCEL_ENDPOINT, |name| {
            (name == "TYPESAFE_API_KEY").then(|| "official-key".to_string())
        }),
        None
    );
    assert_eq!(
        api_key_for_endpoint(rebon_api::typesafe::DEFAULT_ENDPOINT, |name| {
            (name == "AI_GATEWAY_API_KEY").then(|| "gateway-key".to_string())
        }),
        None
    );
    assert_eq!(
        api_key_for_endpoint(VERCEL_ENDPOINT, |name| {
            match name {
                "REBON_AI_GATEWAY_API_KEY" => Some("  preferred  ".to_string()),
                "AI_GATEWAY_API_KEY" => Some("gateway-key".to_string()),
                _ => None,
            }
        }),
        Some("preferred".into())
    );
}

#[test]
fn classifier_endpoint_uses_official_default_or_configured_https_url() {
    assert_eq!(
        classifier_endpoint(&serde_json::json!({})).unwrap(),
        rebon_api::typesafe::DEFAULT_ENDPOINT
    );
    assert_eq!(
        classifier_endpoint(&serde_json::json!({
            "classifierEndpoint": " https://ai-gateway.vercel.sh/typesafe/v1/systemone "
        }))
        .unwrap(),
        "https://ai-gateway.vercel.sh/typesafe/v1/systemone"
    );
    for invalid in [
        serde_json::json!(""),
        serde_json::json!("http://example.com/v1/systemone"),
        serde_json::json!("not-a-url"),
        serde_json::json!(42),
    ] {
        let err =
            classifier_endpoint(&serde_json::json!({"classifierEndpoint": invalid})).unwrap_err();
        assert!(err.to_string().contains("classifierEndpoint"));
    }
}

#[tokio::test]
async fn vercel_typesafe_wire_uses_gateway_model_endpoint_and_bearer_key() {
    let response = serde_json::json!({
        "model": "typesafe-ai/jev",
        "answers": {
            crate::jev::TARGET_QUESTION: answer("keep", 1.0),
            crate::jev::EFFORT_QUESTION: answer("keep", 1.0),
        },
        "provider_metadata": {"gateway": {"cost": "0.00001"}}
    })
    .to_string();
    let api = FakeApi::start(vec![(200, response)]).await;
    let endpoint = format!("{}/typesafe/v1/systemone", api.base);
    let client = SystemOneClient::new(
        &endpoint,
        Some("gateway-key".into()),
        Duration::from_secs(1),
    )
    .unwrap();
    let candidates = candidates_of(&[("openai", &["gpt-5.4"])]);
    let decision = crate::jev::route(
        &client,
        &input("choose a model"),
        &candidates,
        None,
        "typesafe-ai/jev",
    )
    .await
    .unwrap();
    assert!(decision.provider.is_none());
    assert!(decision.model.is_none());
    let seen = api.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/typesafe/v1/systemone");
    assert_eq!(seen[0].authorization, "Bearer gateway-key");
    assert_eq!(seen[0].body["model"], "typesafe-ai/jev");
}

/// 一次成功的往返：请求形状、鉴权头、状态与题目。
#[tokio::test]
async fn the_request_carries_the_key_the_state_and_both_questions() {
    let api = FakeApi::start(vec![(200, body("keep", "keep", 1.0))]).await;
    let candidates = candidates_of(&[("openai", &["gpt-5.4"]), ("local", &["self-hosted"])]);
    let request = crate::jev::request(
        &input("raw task only"),
        &candidates,
        Some("hard work goes to the frontier model"),
        DEFAULT_MODEL,
    )
    .expect("builds");
    let response = client(&api).ask(&request).await.expect("answers");

    let seen = api.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, PATH);
    assert_eq!(seen[0].authorization, "Bearer test-key");
    assert_eq!(seen[0].body["model"], DEFAULT_MODEL);
    assert_eq!(seen[0].body["state"]["prompt"], "raw task only");
    assert_eq!(seen[0].body["state"]["current"]["provider"], "openai");
    assert_eq!(seen[0].body["state"]["current"]["model"], "gpt-5.4");

    let questions = &seen[0].body["questions"];
    assert_eq!(questions[crate::jev::TARGET_QUESTION]["type"], "choice");
    assert_eq!(questions[crate::jev::EFFORT_QUESTION]["type"], "choice");
    let instructions = &questions[crate::jev::TARGET_QUESTION]["instructions"];
    assert_eq!(instructions[0], "hard work goes to the frontier model");
    assert!(
        instructions[1]
            .as_str()
            .expect("the question")
            .contains("provider and model"),
        "{instructions}"
    );

    // 答案按题号读回，选择、概率与置信度都在。
    let answer = &response.answers[crate::jev::TARGET_QUESTION];
    assert_eq!(answer.kind(), "choice");
    assert_eq!(answer.chosen().expect("a choice").0, "keep");
    assert_eq!(response.model, "jev-1.13.0");
}

/// 文档要求对 429 退避重试；一次就够，第二次仍是 429 就没有第三次。
#[tokio::test]
async fn a_retryable_status_is_tried_once_more() {
    let api = FakeApi::start(vec![(429, "{}".into()), (200, body("keep", "keep", 1.0))]).await;
    let request = crate::jev::request(
        &input("raw task"),
        &candidates_of(&[("openai", &["gpt-5.4"])]),
        None,
        DEFAULT_MODEL,
    )
    .expect("builds");
    assert!(client(&api).ask(&request).await.is_ok());
    assert_eq!(api.seen().len(), 2, "one retry, not a loop");
}

#[tokio::test]
async fn a_second_retryable_status_is_reported() {
    let api = FakeApi::start(vec![(429, "slow down".into()), (529, "overloaded".into())]).await;
    let request = crate::jev::request(
        &input("raw task"),
        &candidates_of(&[("openai", &["gpt-5.4"])]),
        None,
        DEFAULT_MODEL,
    )
    .expect("builds");
    let error = client(&api).ask(&request).await.expect_err("gives up");
    assert!(error.to_string().contains("529"), "{error}");
    assert_eq!(api.seen().len(), 2);
}

/// 401/422 是"请求本身不对"，重试只会重复同一个错误。
#[tokio::test]
async fn a_request_error_is_not_retried() {
    for status in [401, 422] {
        let api = FakeApi::start(vec![
            (status, "nope".into()),
            (200, body("keep", "keep", 1.0)),
        ])
        .await;
        let request = crate::jev::request(
            &input("raw task"),
            &candidates_of(&[("openai", &["gpt-5.4"])]),
            None,
            DEFAULT_MODEL,
        )
        .expect("builds");
        let error = client(&api)
            .ask(&request)
            .await
            .expect_err("refuses the answer");
        assert!(error.to_string().contains(&status.to_string()), "{error}");
        assert_eq!(api.seen().len(), 1, "{status} must not be retried");
    }
}

#[tokio::test]
async fn a_body_that_is_not_a_response_is_an_error() {
    let api = FakeApi::start(vec![(200, "not json at all".into())]).await;
    let request = crate::jev::request(
        &input("raw task"),
        &candidates_of(&[("openai", &["gpt-5.4"])]),
        None,
        DEFAULT_MODEL,
    )
    .expect("builds");
    let error = client(&api).ask(&request).await.expect_err("refuses");
    assert!(error.to_string().contains("System One response"), "{error}");
}

#[tokio::test]
async fn a_stalled_endpoint_times_out() {
    let api = FakeApi::start(vec![(0, String::new())]).await;
    let request = crate::jev::request(
        &input("raw task"),
        &candidates_of(&[("openai", &["gpt-5.4"])]),
        None,
        DEFAULT_MODEL,
    )
    .expect("builds");
    let error = client(&api).ask(&request).await.expect_err("times out");
    assert!(error.to_string().contains("TypeSafe"), "{error}");
}

// ── the key ──────────────────────────────────────────────────────

/// 本机可能整套 key 都导出了，所以取 key 只测查找顺序，不动进程环境。
#[test]
fn the_key_is_read_from_the_environment_with_the_rebon_name_first() {
    let env = |pairs: Vec<(&str, &str)>, name: &str| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_string())
    };
    assert_eq!(
        api_key_from(|name| env(
            vec![
                ("REBON_TYPESAFE_API_KEY", "  rebon-key  "),
                ("TYPESAFE_API_KEY", "sdk-key"),
            ],
            name
        )),
        Some("rebon-key".into()),
        "the scoped name wins, trimmed"
    );
    assert_eq!(
        api_key_from(|name| env(vec![("TYPESAFE_API_KEY", "sdk-key")], name)),
        Some("sdk-key".into())
    );
    for blank in ["", "   "] {
        assert_eq!(
            api_key_from(|name| env(
                vec![
                    ("REBON_TYPESAFE_API_KEY", blank),
                    ("TYPESAFE_API_KEY", "sdk-key"),
                ],
                name
            )),
            Some("sdk-key".into()),
            "a blank variable is not a key"
        );
    }
    assert_eq!(api_key_from(|_| None), None);
}

#[test]
fn a_missing_key_names_the_variable_to_set() {
    let Err(error) = SystemOneClient::new(
        "http://127.0.0.1:1/v1/systemone",
        None,
        Duration::from_millis(50),
    ) else {
        panic!("a client cannot exist without a key");
    };
    assert!(error.to_string().contains("TYPESAFE_API_KEY"), "{error}");
}

// ── the decision ─────────────────────────────────────────────────

async fn decide(
    candidates: &Candidates,
    target: &str,
    effort: &str,
    confidence: f64,
) -> (anyhow::Result<ModelRoutingDecision>, usize) {
    let api = FakeApi::start(vec![(200, body(target, effort, confidence))]).await;
    let result = crate::jev::route(
        &client(&api),
        &input("raw task only"),
        candidates,
        None,
        DEFAULT_MODEL,
    )
    .await;
    (result, api.seen().len())
}

#[tokio::test]
async fn the_answered_pair_is_the_decision() {
    let (result, requests) = decide(
        &candidates_of(&[("openai", &["gpt-5.4"]), ("local", &["self-hosted"])]),
        "local/self-hosted",
        "keep",
        0.9,
    )
    .await;
    assert_eq!(
        result.expect("decides"),
        ModelRoutingDecision {
            provider: Some("local".into()),
            model: Some("self-hosted".into()),
            reasoning_effort: None,
        }
    );
    assert_eq!(requests, 1, "one call decides the whole route");
}

#[tokio::test]
async fn keep_leaves_the_session_as_it_is() {
    let (result, _) = decide(
        &candidates_of(&[("openai", &["gpt-5.4"])]),
        "keep",
        "keep",
        1.0,
    )
    .await;
    assert_eq!(
        result.expect("decides"),
        ModelRoutingDecision {
            provider: None,
            model: None,
            reasoning_effort: None,
        }
    );
}

#[tokio::test]
async fn an_answered_effort_reaches_the_decision() {
    let (result, _) = decide(
        &candidates_of(&[("openai", &["gpt-5.4"])]),
        "keep",
        "high",
        0.9,
    )
    .await;
    assert_eq!(
        result.expect("decides").reasoning_effort,
        Some(rebon_types::ReasoningEffort::High)
    );
}

/// 题面外的答案只能是错的：Jev 答得再确定也不能把一个不存在的模型写进会话。
#[tokio::test]
async fn an_answer_outside_the_criteria_is_refused() {
    let (result, _) = decide(
        &candidates_of(&[("openai", &["gpt-5.4"])]),
        "openai/not-in-the-catalog",
        "keep",
        1.0,
    )
    .await;
    let error = result.expect_err("refuses");
    assert!(error.to_string().contains("not-in-the-catalog"), "{error}");
}

#[tokio::test]
async fn an_effort_the_chosen_model_does_not_take_is_refused() {
    // gpt-5.4 在模型表里有行且没有 max：同一个规则提示词后端也在用。
    let (result, _) = decide(
        &candidates_of(&[("openai", &["gpt-5.4"])]),
        "openai/gpt-5.4",
        "max",
        1.0,
    )
    .await;
    let error = result.expect_err("refuses");
    assert!(error.to_string().contains("reasoning effort"), "{error}");
}

#[tokio::test]
async fn a_low_confidence_is_still_acted_on() {
    let (result, _) = decide(
        &candidates_of(&[("openai", &["gpt-5.4"]), ("local", &["self-hosted"])]),
        "local/self-hosted",
        "keep",
        0.01,
    )
    .await;
    assert_eq!(
        result.expect("decides").provider.as_deref(),
        Some("local"),
        "confidence is reported, not consulted"
    );
}

#[tokio::test]
async fn a_missing_or_foreign_answer_is_refused() {
    for answers in [
        serde_json::json!({}),
        serde_json::json!({"target": {"type": "score", "score": 1.0, "confidence": 1.0}}),
        serde_json::json!({"target": {"type": "choice", "confidence": 1.0}}),
        serde_json::json!({"other": answer("keep", 1.0)}),
    ] {
        let api = FakeApi::start(vec![(
            200,
            serde_json::json!({"model": "jev-1.13.0", "answers": answers}).to_string(),
        )])
        .await;
        let result = crate::jev::route(
            &client(&api),
            &input("raw task"),
            &candidates_of(&[("openai", &["gpt-5.4"])]),
            None,
            DEFAULT_MODEL,
        )
        .await;
        assert!(result.is_err(), "accepted {answers}");
    }
}

// ── the questions themselves ─────────────────────────────────────

#[test]
fn the_target_question_offers_every_pair_and_a_keep() {
    let candidates = candidates_of(&[("openai", &["gpt-5.4", "plain"]), ("local", &["self"])]);
    let request =
        crate::jev::request(&input("task"), &candidates, None, DEFAULT_MODEL).expect("builds");
    assert!(request.questions[crate::jev::TARGET_QUESTION]
        .instructions
        .as_str()
        .expect("default rule")
        .contains("cheapest provider and model that suit the task"));
    assert_eq!(request.model, DEFAULT_MODEL);
    let question = &request.questions[crate::jev::TARGET_QUESTION];
    let options: Vec<&str> = question.criteria.keys().map(String::as_str).collect();
    assert_eq!(
        options,
        ["keep", "local/self", "openai/gpt-5.4", "openai/plain"]
    );
    assert!(
        question.criteria[crate::jev::KEEP]
            .as_deref()
            .expect("described")
            .contains("current"),
        "the keep option has to say what it keeps"
    );
    // 模型表里的行会变成描述的一部分：上下文窗口与它接受的 effort。
    let described = question.criteria["openai/gpt-5.4"]
        .as_deref()
        .expect("described");
    assert!(described.contains("context"), "{described}");
    assert!(described.contains("effort"), "{described}");
}

#[test]
fn a_candidate_the_catalogue_never_heard_of_is_still_described() {
    let request = crate::jev::request(
        &input("task"),
        &candidates_of(&[("local", &["self-hosted"])]),
        None,
        DEFAULT_MODEL,
    )
    .expect("builds");
    let described = request.questions[crate::jev::TARGET_QUESTION].criteria["local/self-hosted"]
        .as_deref()
        .expect("described");
    assert!(described.contains("no catalogue row"), "{described}");
}

#[test]
fn more_pairs_than_the_api_takes_are_refused() {
    let models: Vec<String> = (0..255).map(|index| format!("model-{index}")).collect();
    let borrowed: Vec<&str> = models.iter().map(String::as_str).collect();
    let candidates = candidates_of(&[("openai", &borrowed)]);
    let error =
        crate::jev::request(&input("task"), &candidates, None, DEFAULT_MODEL).expect_err("refuses");
    assert!(
        error.to_string().contains("254") && error.to_string().contains("255"),
        "the limit has to be named, not just hit: {error}"
    );
}

#[test]
fn the_effort_question_leaves_out_levels_no_candidate_takes() {
    // gpt-5.4 的表行没有 max，所以只有一个候选时 max 不该出现在题面上。
    let known = candidates_of(&[("openai", &["gpt-5.4"])]);
    let request = crate::jev::request(&input("task"), &known, None, DEFAULT_MODEL).expect("builds");
    let options: Vec<&str> = request.questions[crate::jev::EFFORT_QUESTION]
        .criteria
        .keys()
        .map(String::as_str)
        .collect();
    assert!(options.contains(&crate::jev::KEEP), "{options:?}");
    assert!(options.contains(&"high"), "{options:?}");
    assert!(!options.contains(&"max"), "{options:?}");

    // 表里没有行的自建模型不受表约束，于是 max 又可选。
    let with_custom = candidates_of(&[("openai", &["gpt-5.4"]), ("local", &["self"])]);
    let request =
        crate::jev::request(&input("task"), &with_custom, None, DEFAULT_MODEL).expect("builds");
    assert!(request.questions[crate::jev::EFFORT_QUESTION]
        .criteria
        .contains_key("max"));
}

#[test]
fn a_configured_classifier_model_is_sent_without_changing_the_choices() {
    let candidates = candidates_of(&[("openai", &["gpt-5.4"])]);
    let request = crate::jev::request(&input("task"), &candidates, None, "another-systemone-id")
        .expect("builds");
    assert_eq!(request.model, "another-systemone-id");
    assert!(request.questions[crate::jev::TARGET_QUESTION]
        .criteria
        .contains_key("openai/gpt-5.4"));
}

#[test]
fn a_user_policy_overrides_the_default_cost_preference() {
    let candidates = candidates_of(&[("openai", &["gpt-5.4"])]);
    let request = crate::jev::request(
        &input("task"),
        &candidates,
        Some("prioritize quality"),
        DEFAULT_MODEL,
    )
    .expect("builds");
    let instructions = &request.questions[crate::jev::TARGET_QUESTION].instructions;
    assert_eq!(instructions[0], "prioritize quality");
    assert!(instructions[1]
        .as_str()
        .unwrap()
        .contains("cheapest provider and model"));
}

#[test]
fn the_policy_is_an_extra_instruction_and_its_absence_is_one_instruction() {
    let candidates = candidates_of(&[("openai", &["gpt-5.4"])]);
    let with = crate::jev::request(
        &input("task"),
        &candidates,
        Some("always plan first"),
        DEFAULT_MODEL,
    )
    .expect("builds");
    let instructions = with.questions[crate::jev::TARGET_QUESTION]
        .instructions
        .clone();
    assert_eq!(instructions[0], "always plan first");
    assert!(instructions.as_array().expect("a list").len() == 2);

    let without =
        crate::jev::request(&input("task"), &candidates, None, DEFAULT_MODEL).expect("builds");
    assert!(
        without.questions[crate::jev::TARGET_QUESTION]
            .instructions
            .is_string(),
        "no policy means no empty first instruction"
    );
}

#[test]
fn a_long_prompt_is_cut_to_its_head() {
    let head = "h".repeat(crate::jev::MAX_STATE_CHARS);
    let prompt = format!("{head}tail-that-must-not-be-sent");
    let request = crate::jev::request(
        &input(&prompt),
        &candidates_of(&[("openai", &["gpt-5.4"])]),
        None,
        DEFAULT_MODEL,
    )
    .expect("builds");
    let state = request.state["prompt"].as_str().expect("state text");
    assert_eq!(state.chars().count(), crate::jev::MAX_STATE_CHARS);
    assert!(!state.contains("tail-that-must-not-be-sent"));
}
