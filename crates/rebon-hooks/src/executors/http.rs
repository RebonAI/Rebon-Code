//! `reqwest`-backed executor for `HookCommand::Http`.
//!
//! ## Wire format
//!
//! The hook receives the serialized [`HookInvocationInput`] as the
//! JSON request body. We add a `Content-Type: application/json` header
//! (overridable by the hook's own `headers` list) and forward any
//! entries the hook explicitly declared as required.
//!
//! `allowed_env_vars` is the Claude protocol's minimal-exfiltration
//! contract: the hook names specific env vars the host is willing to surface in the
//! request, and we only forward those (as `X-Hook-Env-<NAME>` headers).
//! If the env var is unset we skip it silently — the hook's server-side
//! code has to treat it as optional.
//!
//! ## Response parsing
//!
//! The body is parsed with [`parse_http_hook_output`]: empty body →
//! default [`crate::output_protocol::SyncHookJsonOutput`], JSON-starting body → validated, and
//! non-JSON → `validation_error`. The HTTP status code is stored in
//! `exit_code` so the aggregation layer can treat non-2xx as blocking
//! the same way a non-zero subprocess exit does.
//!
//! ## Timeout
//!
//! Same precedence as the command executor: `ctx.timeout_override` >
//! `hook.timeout` > [`DEFAULT_HTTP_TIMEOUT`]. The `reqwest` client is
//! built fresh per call — the cost is amortized against the network
//! roundtrip and it lets the caller override TLS/proxy config per
//! invocation without a shared mutable builder.

use std::env;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};

use crate::executor::{ExecutedHookResult, HookExecutionError, HookExecutor, HookRuntimeContext};
use crate::hook_command::{display_text, HookCommand, HttpHook};
use crate::individual_hook::IndividualHookConfig;
use crate::invocation::HookInvocationInput;
use crate::output_protocol::parse_http_hook_output;

/// Default HTTP hook timeout: 30 seconds.
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Prefix applied to env-var names when they are forwarded as
/// headers. `ALLOWED_ENV_FOO` → `X-Hook-Env-ALLOWED_ENV_FOO`, giving
/// server-side middleware one stable header prefix to whitelist.
const ENV_HEADER_PREFIX: &str = "X-Hook-Env-";

/// `reqwest`-backed executor for HTTP hooks.
///
/// Zero-sized; cheap to clone. No shared state — each call builds a
/// fresh `reqwest::Client` so the per-hook `timeout` can be honored
/// without cloning a pre-configured client.
#[derive(Debug, Clone, Default)]
pub struct HttpExecutor;

impl HttpExecutor {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl HookExecutor for HttpExecutor {
    async fn execute(
        &self,
        hook: &IndividualHookConfig,
        input: &HookInvocationInput,
        ctx: &HookRuntimeContext,
    ) -> Result<ExecutedHookResult, HookExecutionError> {
        let http = match &hook.config {
            HookCommand::Http(h) => h,
            other => {
                return Err(HookExecutionError::UnsupportedType(
                    other.type_str().to_string(),
                ))
            }
        };

        let timeout = resolve_timeout(ctx, http.timeout);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| HookExecutionError::Transport(format!("build client: {e}")))?;

        let headers = build_headers(http)
            .map_err(|e| HookExecutionError::Transport(format!("build headers: {e}")))?;

        let body = serde_json::to_vec(input)
            .map_err(|e| HookExecutionError::Transport(format!("serialize input: {e}")))?;

        let label = display_text(&hook.config).to_string();

        let response = match client
            .post(&http.url)
            .headers(headers)
            .body(body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return Err(HookExecutionError::Timeout(timeout)),
            Err(e) => return Err(HookExecutionError::Transport(format!("send: {e}"))),
        };

        let status = response.status();
        let body = match response.text().await {
            Ok(body) => body,
            Err(e) if e.is_timeout() => return Err(HookExecutionError::Timeout(timeout)),
            Err(e) => return Err(HookExecutionError::Transport(format!("read body: {e}"))),
        };

        let parsed = parse_http_hook_output(&body);

        Ok(ExecutedHookResult {
            json: parsed.json,
            plain_text: parsed.plain_text,
            validation_error: parsed.validation_error,
            exit_code: i32::from(status.as_u16()),
            stderr: if status.is_success() {
                String::new()
            } else {
                format!("HTTP {} — {}", status.as_u16(), truncate(&body, 512))
            },
            command_label: label,
        })
    }
}

fn build_headers(hook: &HttpHook) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    if let Some(pairs) = hook.headers.as_ref() {
        for (name, value) in pairs {
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| format!("invalid header name `{name}`: {e}"))?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|e| format!("invalid header value for `{name}`: {e}"))?;
            map.insert(header_name, header_value);
        }
    }

    if let Some(allowed) = hook.allowed_env_vars.as_ref() {
        for name in allowed {
            if let Ok(value) = env::var(name) {
                let header_name =
                    HeaderName::from_bytes(format!("{ENV_HEADER_PREFIX}{name}").as_bytes())
                        .map_err(|e| format!("invalid env header name `{name}`: {e}"))?;
                let header_value = HeaderValue::from_str(&value)
                    .map_err(|e| format!("env var `{name}` is not a valid header value: {e}"))?;
                map.insert(header_name, header_value);
            }
        }
    }

    Ok(map)
}

fn resolve_timeout(ctx: &HookRuntimeContext, hook_timeout: Option<u64>) -> Duration {
    ctx.timeout_override
        .or_else(|| hook_timeout.map(Duration::from_secs))
        .unwrap_or(DEFAULT_HTTP_TIMEOUT)
}

fn truncate(body: &str, max: usize) -> String {
    if body.len() <= max {
        body.to_string()
    } else {
        let mut s: String = body.chars().take(max).collect();
        s.push('…');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookEvent;
    use crate::hook_command::{HookCommand, HttpHook};
    use crate::hook_source::HookSource;
    use crate::invocation::{HookEventPayload, HookInvocationContext};
    use serde_json::json;

    fn hook(
        url: &str,
        headers: Option<Vec<(String, String)>>,
        env_vars: Option<Vec<String>>,
    ) -> IndividualHookConfig {
        IndividualHookConfig {
            event: HookEvent::PreToolUse,
            config: HookCommand::Http(HttpHook {
                url: url.into(),
                r#if: None,
                timeout: Some(5),
                headers,
                allowed_env_vars: env_vars,
                status_message: None,
                once: None,
            }),
            matcher: None,
            source: HookSource::UserSettings,
            plugin_name: None,
        }
    }

    fn invocation() -> HookInvocationInput {
        HookInvocationInput::new(
            HookInvocationContext {
                cwd: ".".into(),
                transcript_path: String::new(),
                session_id: "t".into(),
                ..Default::default()
            },
            HookEventPayload::PreToolUse {
                tool_name: "Bash".into(),
                tool_input: json!({"command": "echo hi"}),
                tool_use_id: "x".into(),
            },
        )
    }

    #[test]
    fn build_headers_sets_content_type() {
        let h = HttpHook {
            url: "https://x".into(),
            r#if: None,
            timeout: None,
            headers: None,
            allowed_env_vars: None,
            status_message: None,
            once: None,
        };
        let map = build_headers(&h).unwrap();
        assert_eq!(
            map.get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("application/json")
        );
    }

    #[test]
    fn build_headers_forwards_custom_headers() {
        let h = HttpHook {
            url: "https://x".into(),
            r#if: None,
            timeout: None,
            headers: Some(vec![("X-Foo".into(), "bar".into())]),
            allowed_env_vars: None,
            status_message: None,
            once: None,
        };
        let map = build_headers(&h).unwrap();
        assert_eq!(map.get("X-Foo").unwrap(), HeaderValue::from_static("bar"));
    }

    #[test]
    fn build_headers_rejects_invalid_header_names() {
        let h = HttpHook {
            url: "https://x".into(),
            r#if: None,
            timeout: None,
            headers: Some(vec![("illegal header".into(), "bar".into())]),
            allowed_env_vars: None,
            status_message: None,
            once: None,
        };
        assert!(build_headers(&h).is_err());
    }

    #[test]
    fn build_headers_forwards_allowed_env_var_when_present() {
        let name = "REBON_HOOKS_HTTP_TEST_VAR";
        // SAFETY: test-local; serial within a single process, other
        // tests do not read this name.
        std::env::set_var(name, "present");
        let h = HttpHook {
            url: "https://x".into(),
            r#if: None,
            timeout: None,
            headers: None,
            allowed_env_vars: Some(vec![name.into()]),
            status_message: None,
            once: None,
        };
        let map = build_headers(&h).unwrap();
        let expected = format!("{ENV_HEADER_PREFIX}{name}");
        assert_eq!(map.get(expected.as_str()).unwrap(), "present");
        std::env::remove_var(name);
    }

    #[test]
    fn build_headers_skips_missing_env_var_silently() {
        let h = HttpHook {
            url: "https://x".into(),
            r#if: None,
            timeout: None,
            headers: None,
            allowed_env_vars: Some(vec!["REBON_HOOKS_HTTP_UNSET_VAR".into()]),
            status_message: None,
            once: None,
        };
        let map = build_headers(&h).unwrap();
        assert!(map.get("X-Hook-Env-REBON_HOOKS_HTTP_UNSET_VAR").is_none());
    }

    #[test]
    fn resolve_timeout_override_wins() {
        let ctx = HookRuntimeContext {
            matcher: None,
            timeout_override: Some(Duration::from_secs(1)),
        };
        assert_eq!(resolve_timeout(&ctx, Some(30)), Duration::from_secs(1));
    }

    #[test]
    fn resolve_timeout_falls_back_to_default() {
        let ctx = HookRuntimeContext::default();
        assert_eq!(resolve_timeout(&ctx, None), DEFAULT_HTTP_TIMEOUT);
    }

    #[test]
    fn truncate_preserves_short_bodies() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn truncate_adds_ellipsis_when_over_limit() {
        let s = "a".repeat(600);
        let t = truncate(&s, 512);
        assert!(t.ends_with('…'));
        assert_eq!(t.chars().count(), 513);
    }

    #[tokio::test]
    async fn unsupported_variant_returns_error() {
        use crate::hook_command::{BashCommandHook, HookCommand};
        let exec = HttpExecutor::new();
        let h = IndividualHookConfig {
            event: HookEvent::PreToolUse,
            config: HookCommand::Command(BashCommandHook {
                command: "ls".into(),
                r#if: None,
                shell: None,
                timeout: None,
                status_message: None,
                once: None,
                r#async: None,
                async_rewake: None,
            }),
            matcher: None,
            source: HookSource::UserSettings,
            plugin_name: None,
        };
        let err = exec
            .execute(&h, &invocation(), &HookRuntimeContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, HookExecutionError::UnsupportedType(ref t) if t == "command"));
    }

    #[tokio::test]
    async fn unreachable_url_reports_transport_error() {
        // Use a bogus port on loopback — the connect should fail fast.
        let exec = HttpExecutor::new();
        let h = hook("http://127.0.0.1:1/unreachable", None, None);
        let ctx = HookRuntimeContext {
            matcher: None,
            timeout_override: Some(Duration::from_millis(500)),
        };
        let err = exec.execute(&h, &invocation(), &ctx).await.unwrap_err();
        assert!(matches!(
            err,
            HookExecutionError::Transport(_) | HookExecutionError::Timeout(_)
        ));
    }
}
