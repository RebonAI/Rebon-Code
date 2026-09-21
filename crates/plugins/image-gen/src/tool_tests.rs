use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use super::*;
use crate::endpoint::{endpoint_test_lock, ImagesEndpoint};
use crate::set_provider_endpoint;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A PNG signature followed by filler: enough for `sniff_media_type`.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-a-png";
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3];

// ── fixtures ─────────────────────────────────────────────────────

/// One request the fake Images API received.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    authorization: String,
    body: Value,
}

/// A loopback HTTP server that answers each request with the next canned
/// `(status, body)` and records what it was sent.
struct FakeImagesApi {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeImagesApi {
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
            .unwrap_or(Value::Null),
    }
}

fn image_response(bytes: &[u8]) -> (u16, String) {
    (
        200,
        json!({
            "created": 1,
            "background": "opaque",
            "data": [{ "b64_json": BASE64.encode(bytes) }]
        })
        .to_string(),
    )
}

/// The process-wide state a call reads: the endpoint cell and the config
/// home. Held for the whole test so parallel tests cannot swap either.
struct Harness {
    _endpoint: MutexGuard<'static, ()>,
    _env: MutexGuard<'static, ()>,
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    previous_home: Option<std::ffi::OsString>,
}

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Harness {
    fn new() -> Self {
        let endpoint = endpoint_test_lock();
        let env = env_lock();
        let home = tempfile::TempDir::new().expect("config home");
        let previous_home = std::env::var_os("REBON_CONFIG_DIR");
        std::env::set_var("REBON_CONFIG_DIR", home.path());
        set_provider_endpoint(None);
        Self {
            _endpoint: endpoint,
            _env: env,
            home,
            workspace: tempfile::TempDir::new().expect("workspace"),
            previous_home,
        }
    }

    fn publish(
        &self,
        base: &str,
        token: &str,
        refresher: Option<Arc<dyn rebon_api::TokenRefresher>>,
    ) {
        set_provider_endpoint(Some(Arc::new(ImagesEndpoint::new(base, token, refresher))));
    }

    fn context(&self) -> ToolContext {
        ToolContext::new()
            .with_cwd(self.workspace.path().display().to_string())
            .with_session_id("sess-1")
            .with_tool_use_id("call_1")
    }

    fn saved_path(&self, file: &str) -> PathBuf {
        self.home
            .path()
            .join("generated_images")
            .join("sess-1")
            .join(file)
    }

    fn write_image(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.workspace.path().join(name);
        std::fs::write(&path, bytes).expect("write reference");
        path
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        set_provider_endpoint(None);
        match self.previous_home.take() {
            Some(previous) => std::env::set_var("REBON_CONFIG_DIR", previous),
            None => std::env::remove_var("REBON_CONFIG_DIR"),
        }
    }
}

#[derive(Debug)]
struct CountingRefresher {
    calls: AtomicUsize,
    token: Result<String, String>,
}

#[async_trait]
impl rebon_api::TokenRefresher for CountingRefresher {
    async fn refresh(&self) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.token.clone()
    }
}

fn execution_message(result: ToolResult<Value>) -> String {
    match result {
        Err(ToolError::Execution { source, .. }) => source.to_string(),
        other => panic!("expected an execution error, got {other:?}"),
    }
}

fn image_block(content: ToolResultContent) -> (String, ImageBlock) {
    let ToolResultContent::Blocks(blocks) = content else {
        panic!("expected blocks, got {content:?}");
    };
    match blocks.as_slice() {
        [ToolResultContentBlock::Text(text), ToolResultContentBlock::Image(image)] => {
            (text.text.clone(), image.clone())
        }
        other => panic!("expected a summary and an image, got {other:?}"),
    }
}

// ── availability ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn the_tool_is_enabled_only_while_an_endpoint_is_published() {
    let harness = Harness::new();
    assert!(!ImageGenTool.is_enabled());
    harness.publish("https://api.openai.com/v1", "sk", None);
    assert!(ImageGenTool.is_enabled());
    set_provider_endpoint(None);
    assert!(!ImageGenTool.is_enabled());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_without_an_endpoint_says_why_and_sends_nothing() {
    let harness = Harness::new();
    let message = execution_message(
        ImageGenTool
            .call(json!({ "prompt": "a cat" }), &harness.context())
            .await,
    );
    assert!(
        message.contains("not a first-party OpenAI route"),
        "{message}"
    );
}

// ── generation ───────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_generation_posts_the_codex_defaults_and_saves_the_image() {
    let harness = Harness::new();
    let api = FakeImagesApi::start(vec![image_response(PNG)]).await;
    harness.publish(&format!("{}/v1", api.base), "sk-test", None);

    let value = ImageGenTool
        .call(json!({ "prompt": "a red potion" }), &harness.context())
        .await
        .expect("generated");

    let seen = api.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/v1/images/generations");
    assert_eq!(seen[0].authorization, "Bearer sk-test");
    assert_eq!(
        seen[0].body,
        json!({
            "prompt": "a red potion",
            "model": "gpt-image-2",
            "background": "auto",
            "quality": "auto",
            "size": "auto",
        })
    );

    let saved = harness.saved_path("call_1.png");
    assert_eq!(value["file_path"], json!(saved.display().to_string()));
    assert_eq!(value["operation"], "generate");
    assert_eq!(value["background"], "opaque");
    assert!(
        value.get("image_base64").is_none(),
        "bytes stay out of a saved result"
    );
    assert_eq!(std::fs::read(&saved).expect("saved"), PNG);

    let (summary, image) = image_block(
        ImageGenTool
            .project_result_for_model(&value)
            .expect("projects"),
    );
    assert!(summary.contains(&saved.display().to_string()), "{summary}");
    assert!(summary.contains("copy it"), "{summary}");
    assert_eq!(image.source.media_type, "image/png");
    assert_eq!(image.source.data, BASE64.encode(PNG));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_codex_oauth_route_posts_beside_its_responses_path() {
    let harness = Harness::new();
    let api = FakeImagesApi::start(vec![image_response(PNG)]).await;
    harness.publish(
        &format!("{}/backend-api/codex/responses", api.base),
        "oauth-token",
        None,
    );
    ImageGenTool
        .call(json!({ "prompt": "a cat" }), &harness.context())
        .await
        .expect("generated");
    assert_eq!(api.seen()[0].path, "/backend-api/codex/images/generations");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_saved_extension_follows_the_returned_format() {
    let harness = Harness::new();
    let api = FakeImagesApi::start(vec![image_response(JPEG)]).await;
    harness.publish(&format!("{}/v1", api.base), "sk", None);
    let value = ImageGenTool
        .call(json!({ "prompt": "a cat" }), &harness.context())
        .await
        .expect("generated");
    assert_eq!(value["media_type"], "image/jpeg");
    assert!(harness.saved_path("call_1.jpg").exists());
}

// ── edits ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_sends_every_reference_as_a_data_url() {
    let harness = Harness::new();
    let api = FakeImagesApi::start(vec![image_response(PNG)]).await;
    harness.publish(&format!("{}/v1", api.base), "sk", None);
    let target = harness.write_image("target.png", PNG);
    // Named .png but a JPEG: the signature decides, not the extension.
    let style = harness.write_image("style.png", JPEG);

    let value = ImageGenTool
        .call(
            json!({
                "prompt": "put the subject of image 1 in the style of image 2",
                "referenced_image_paths": [target, style],
            }),
            &harness.context(),
        )
        .await
        .expect("edited");

    let seen = api.seen();
    assert_eq!(seen[0].path, "/v1/images/edits");
    assert_eq!(
        seen[0].body["images"],
        json!([
            { "image_url": format!("data:image/png;base64,{}", BASE64.encode(PNG)) },
            { "image_url": format!("data:image/jpeg;base64,{}", BASE64.encode(JPEG)) },
        ])
    );
    assert_eq!(seen[0].body["model"], "gpt-image-2");
    assert_eq!(value["operation"], "edit");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reference_that_is_not_an_image_is_refused_before_anything_is_sent() {
    let harness = Harness::new();
    let api = FakeImagesApi::start(vec![image_response(PNG)]).await;
    harness.publish(&format!("{}/v1", api.base), "sk", None);
    let text = harness.write_image("notes.png", b"just text");
    let message = execution_message(
        ImageGenTool
            .call(
                json!({ "prompt": "edit", "referenced_image_paths": [text] }),
                &harness.context(),
            )
            .await,
    );
    assert!(
        message.contains("not a PNG, JPEG, WebP or GIF"),
        "{message}"
    );

    let missing = harness.workspace.path().join("missing.png");
    let message = execution_message(
        ImageGenTool
            .call(
                json!({ "prompt": "edit", "referenced_image_paths": [missing] }),
                &harness.context(),
            )
            .await,
    );
    assert!(
        message.contains("cannot read referenced image"),
        "{message}"
    );
    assert!(api.seen().is_empty());
}

// ── failures ─────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_token_is_refreshed_and_the_request_retried_once() {
    let harness = Harness::new();
    let api = FakeImagesApi::start(vec![
        (401, r#"{"error":{"message":"expired"}}"#.into()),
        image_response(PNG),
    ])
    .await;
    let refresher = Arc::new(CountingRefresher {
        calls: AtomicUsize::new(0),
        token: Ok("fresh".into()),
    });
    harness.publish(
        &format!("{}/v1", api.base),
        "stale",
        Some(refresher.clone()),
    );

    ImageGenTool
        .call(json!({ "prompt": "a cat" }), &harness.context())
        .await
        .expect("retried with the fresh token");
    let seen = api.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].authorization, "Bearer stale");
    assert_eq!(seen[1].authorization, "Bearer fresh");
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_rejection_is_final() {
    let harness = Harness::new();
    let api = FakeImagesApi::start(vec![
        (401, "expired".into()),
        (403, "still no".into()),
        image_response(PNG),
    ])
    .await;
    let refresher = Arc::new(CountingRefresher {
        calls: AtomicUsize::new(0),
        token: Ok("fresh".into()),
    });
    harness.publish(
        &format!("{}/v1", api.base),
        "stale",
        Some(refresher.clone()),
    );
    let message = execution_message(
        ImageGenTool
            .call(json!({ "prompt": "a cat" }), &harness.context())
            .await,
    );
    assert!(message.contains("HTTP 403: still no"), "{message}");
    assert_eq!(api.seen().len(), 2);
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejection_without_a_refresher_or_with_a_failing_one_is_reported() {
    let harness = Harness::new();
    let api = FakeImagesApi::start(vec![(401, "bad key".into())]).await;
    harness.publish(&format!("{}/v1", api.base), "sk", None);
    let message = execution_message(
        ImageGenTool
            .call(json!({ "prompt": "a cat" }), &harness.context())
            .await,
    );
    assert!(message.contains("HTTP 401: bad key"), "{message}");
    assert_eq!(api.seen().len(), 1);

    let api = FakeImagesApi::start(vec![(401, "expired".into())]).await;
    let refresher = Arc::new(CountingRefresher {
        calls: AtomicUsize::new(0),
        token: Err("no refresh token".into()),
    });
    harness.publish(&format!("{}/v1", api.base), "sk", Some(refresher));
    let message = execution_message(
        ImageGenTool
            .call(json!({ "prompt": "a cat" }), &harness.context())
            .await,
    );
    assert!(
        message.contains("token refresh failed: no refresh token"),
        "{message}"
    );
    assert_eq!(api.seen().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_error_is_not_retried_and_names_status_and_body() {
    let harness = Harness::new();
    let api = FakeImagesApi::start(vec![
        (400, r#"{"error":{"message":"safety system"}}"#.into()),
        image_response(PNG),
    ])
    .await;
    harness.publish(&format!("{}/v1", api.base), "sk", None);
    let message = execution_message(
        ImageGenTool
            .call(json!({ "prompt": "a cat" }), &harness.context())
            .await,
    );
    assert!(
        message.contains("image generation returned HTTP 400"),
        "{message}"
    );
    assert!(message.contains("safety system"), "{message}");
    assert_eq!(
        api.seen().len(),
        1,
        "a billed request is never resent blindly"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsaved_image_travels_inline_so_the_generation_is_not_lost() {
    let harness = Harness::new();
    let api = FakeImagesApi::start(vec![image_response(PNG)]).await;
    harness.publish(&format!("{}/v1", api.base), "sk", None);
    // Something already sits where the image would be written.
    let occupied = harness.saved_path("call_1.png");
    std::fs::create_dir_all(occupied.parent().unwrap()).unwrap();
    std::fs::write(&occupied, b"someone else's file").unwrap();

    let value = ImageGenTool
        .call(json!({ "prompt": "a cat" }), &harness.context())
        .await
        .expect("the generation itself succeeded");
    assert!(value.get("file_path").is_none());
    assert!(value["save_error"]
        .as_str()
        .unwrap()
        .contains("cannot create"));
    assert_eq!(std::fs::read(&occupied).unwrap(), b"someone else's file");

    let (summary, image) = image_block(
        ImageGenTool
            .project_result_for_model(&value)
            .expect("projects"),
    );
    assert!(summary.contains("could not be saved"), "{summary}");
    assert_eq!(image.source.data, BASE64.encode(PNG));
}

#[test]
fn a_saved_image_that_is_gone_projects_as_text() {
    let value = json!({
        "media_type": "image/png",
        "summary": "Generated image saved to /nowhere/x.png.",
        "file_path": "/nowhere/definitely/missing/x.png",
    });
    let Some(ToolResultContent::Text(text)) = ImageGenTool.project_result_for_model(&value) else {
        panic!("expected a text projection");
    };
    assert!(text.contains("could not be read back"), "{text}");
    assert!(ImageGenTool
        .project_result_for_model(&json!({ "unrelated": true }))
        .is_none());
}

// ── input ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn malformed_input_is_invalid() {
    let harness = Harness::new();
    let absolute = harness.write_image("a.png", PNG);
    let context = harness.context();
    for input in [
        json!({}),
        json!({ "prompt": "   " }),
        json!({ "prompt": "x", "size": "1024x1024" }),
        json!({ "prompt": "x", "referenced_image_paths": ["relative.png"] }),
        json!({ "prompt": "x", "referenced_image_paths": vec![absolute.clone(); 6] }),
    ] {
        let outcome = ImageGenTool
            .validate_input(&input, &context)
            .await
            .expect("validation answers");
        assert!(!outcome.is_valid(), "{input} should be invalid");
    }
    let outcome = ImageGenTool
        .validate_input(
            &json!({ "prompt": "x", "referenced_image_paths": vec![absolute; 5] }),
            &context,
        )
        .await
        .expect("validation answers");
    assert!(outcome.is_valid());
}

#[test]
fn media_types_are_read_from_signatures() {
    assert_eq!(sniff_media_type(PNG), Some("image/png"));
    assert_eq!(sniff_media_type(JPEG), Some("image/jpeg"));
    assert_eq!(
        sniff_media_type(b"RIFF\0\0\0\0WEBPVP8 "),
        Some("image/webp")
    );
    assert_eq!(sniff_media_type(b"GIF89a..."), Some("image/gif"));
    assert_eq!(sniff_media_type(b"RIFF\0\0\0\0WAVE"), None);
    assert_eq!(sniff_media_type(b""), None);
    assert_eq!(extension_for("image/jpeg"), "jpg");
    assert_eq!(extension_for("image/webp"), "webp");
    assert_eq!(extension_for("image/png"), "png");
}

#[test]
fn ids_become_single_path_segments() {
    assert_eq!(sanitize_segment("call_AbC-9", "x"), "call_AbC-9");
    assert_eq!(sanitize_segment("../etc/passwd", "x"), "___etc_passwd");
    assert_eq!(sanitize_segment("a:b\\c", "x"), "a_b_c");
    assert_eq!(sanitize_segment("", "image"), "image");
}
