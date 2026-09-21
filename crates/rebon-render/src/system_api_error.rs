//! Projection for the API-error system message: the retry countdown, the
//! truncation of the error body, and the lines a renderer draws from both.
//!
//! The error text arrives already formatted, as a plain string, so this
//! projection stays independent of both the renderer and the API client.

/// Hard cap on how many characters of an error body are displayed.
pub const MAX_API_ERROR_CHARS: usize = 1000;

const ERROR_WRAPPER_PREFIXES: &[&str] = &[
    "prompt turn failed:",
    "prompt executor failed:",
    "model stream start failed:",
    "model stream error:",
    "model client unauthorized:",
    "model client bad request:",
    "model client permanent failure:",
    "model client transient failure:",
    "model client overloaded:",
    "model client protocol error:",
    "model client http error:",
    "model client io error:",
    "model client error:",
    "ws connect:",
    "http error:",
];

/// Everything the projection needs from one API-error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemApiErrorInput {
    /// How many retries have already been attempted.
    pub retry_attempt: usize,
    /// The error body, already formatted as display text.
    pub formatted_error: String,
    /// Delay before the next retry, in milliseconds.
    pub retry_in_ms: u64,
    /// Total number of retries allowed for this message.
    pub max_retries: usize,
    /// Whether the verbose view is on, which suppresses truncation.
    pub verbose: bool,
    /// Milliseconds elapsed on the countdown, advanced one tick at a time.
    pub countdown_ms: u64,
    /// Optional API-timeout setting read from the environment.
    pub api_timeout_ms: Option<u64>,
}

/// What a renderer draws for one API-error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemApiErrorProjection {
    /// Whether the external-build early-retry gate hides the message.
    pub hidden: bool,
    /// Tick period while the countdown still has time left, otherwise `None`.
    pub tick_interval_ms: Option<u64>,
    /// Error text after optional truncation.
    pub displayed_error: Option<String>,
    /// Whether the expand hint should appear under the error.
    pub show_expand_hint: bool,
    /// Rounded/clamped live retry countdown in seconds.
    pub retry_in_seconds_live: Option<u64>,
    /// Full dim retry text line.
    pub retry_text: Option<String>,
}

/// Advance the countdown by one tick, from `ms` to `ms + 1000`.
pub fn advance_system_api_error_countdown(countdown_ms: u64) -> u64 {
    countdown_ms + 1000
}

/// Project one API-error message into the fields a renderer draws.
pub fn project_system_api_error(input: &SystemApiErrorInput) -> SystemApiErrorProjection {
    let has_retry = input.max_retries > 0;
    let hidden = has_retry && input.retry_attempt < 4;
    let done = input.countdown_ms >= input.retry_in_ms;
    let tick_interval_ms = if !has_retry || hidden || done {
        None
    } else {
        Some(1000)
    };

    if hidden {
        return SystemApiErrorProjection {
            hidden: true,
            tick_interval_ms,
            displayed_error: None,
            show_expand_hint: false,
            retry_in_seconds_live: None,
            retry_text: None,
        };
    }

    let formatted_error = normalize_system_api_error_text(&input.formatted_error);
    let (displayed_error, show_expand_hint) = truncate_api_error(&formatted_error, input.verbose);
    let retry_in_seconds_live = has_retry.then(|| {
        (((input.retry_in_ms as i64 - input.countdown_ms as i64) as f64) / 1000.0).round() as i64
    });
    let retry_in_seconds_live = retry_in_seconds_live.map(|seconds| seconds.max(0) as u64);
    let retry_text = retry_in_seconds_live.map(|retry_in_seconds_live| {
        let unit = if retry_in_seconds_live == 1 {
            "second"
        } else {
            "seconds"
        };
        let timeout_suffix = input
            .api_timeout_ms
            .map(|timeout| format!(" \u{00b7} API_TIMEOUT_MS={timeout}ms, try increasing it"))
            .unwrap_or_default();
        format!(
            "Retrying in {retry_in_seconds_live} {unit}\u{2026} (attempt {}/{}){}",
            input.retry_attempt, input.max_retries, timeout_suffix
        )
    });

    SystemApiErrorProjection {
        hidden: false,
        tick_interval_ms,
        displayed_error: Some(displayed_error),
        show_expand_hint,
        retry_in_seconds_live,
        retry_text,
    }
}

/// Strip generic prompt/model transport wrappers so the displayed error
/// starts at the actionable cause.
pub fn normalize_system_api_error_text(text: &str) -> String {
    let original = text.trim();
    let mut current = original;

    loop {
        let before = current;
        let lower = current.to_ascii_lowercase();

        for prefix in ERROR_WRAPPER_PREFIXES {
            if lower.starts_with(prefix) {
                current = current[prefix.len()..].trim_start();
                break;
            }
        }

        if before == current {
            if let Some(rest) = strip_http_ws_error_prefix(current) {
                current = rest;
            }
        }

        if before == current {
            break;
        }
    }

    if current.is_empty() {
        original.to_string()
    } else {
        current.to_string()
    }
}

fn strip_http_ws_error_prefix(text: &str) -> Option<&str> {
    let rest = strip_status_code_prefix(text)?;
    let lower = rest.to_ascii_lowercase();
    if lower.starts_with("ws error ") {
        let ws_rest = &rest["ws error ".len()..];
        if let Some(after_code) = ws_rest.strip_prefix('(') {
            if let Some(end) = after_code.find("):") {
                return Some(after_code[end + 2..].trim_start());
            }
        }
    }
    Some(rest)
}

fn strip_status_code_prefix(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.len() >= 5
        && bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2].is_ascii_digit()
        && bytes[3] == b':'
        && bytes[4].is_ascii_whitespace()
    {
        Some(text[5..].trim_start())
    } else {
        None
    }
}

/// Cap an error body at [`MAX_API_ERROR_CHARS`] unless verbose is on.
///
/// `pub(crate)` because the assistant-text projection shows the same error
/// bodies and needs the same cut: it used to carry a stub that returned
/// "not truncated" unconditionally, which left its expand hint unreachable.
pub(crate) fn truncate_api_error(text: &str, verbose: bool) -> (String, bool) {
    if verbose || text.chars().count() <= MAX_API_ERROR_CHARS {
        return (text.to_string(), false);
    }

    let end = text
        .char_indices()
        .nth(MAX_API_ERROR_CHARS)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    (format!("{}\u{2026}", &text[..end]), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> SystemApiErrorInput {
        SystemApiErrorInput {
            retry_attempt: 4,
            formatted_error: "connection failed".into(),
            retry_in_ms: 5_000,
            max_retries: 8,
            verbose: false,
            countdown_ms: 1_000,
            api_timeout_ms: None,
        }
    }

    #[test]
    fn hidden_for_early_retries_and_no_interval() {
        let mut value = input();
        value.retry_attempt = 3;
        let projection = project_system_api_error(&value);
        assert!(projection.hidden);
        assert_eq!(projection.tick_interval_ms, None);
        assert_eq!(projection.displayed_error, None);
    }

    #[test]
    fn interval_runs_until_done_and_countdown_helper_adds_one_second() {
        let projection = project_system_api_error(&input());
        assert_eq!(projection.tick_interval_ms, Some(1000));
        assert_eq!(advance_system_api_error_countdown(2_000), 3_000);

        let mut done = input();
        done.countdown_ms = 6_000;
        let projection = project_system_api_error(&done);
        assert_eq!(projection.tick_interval_ms, None);
        assert_eq!(projection.retry_in_seconds_live, Some(0));
    }

    #[test]
    fn strips_prompt_and_model_wrapper_prefixes_from_terminal_errors() {
        let mut value = input();
        value.max_retries = 0;
        value.retry_attempt = 0;
        value.formatted_error = "Prompt turn failed: prompt executor failed: model stream error: model client bad request: 400: ws error (websocket_connection_limit_reached): Responses websocket connection limit reached".into();

        let projection = project_system_api_error(&value);

        assert_eq!(
            projection.displayed_error.as_deref(),
            Some("Responses websocket connection limit reached")
        );
        assert_eq!(projection.retry_text, None);
        assert_eq!(projection.tick_interval_ms, None);

        value.formatted_error =
            "model client transient failure: 429: rate limited, retry later".into();
        let projection = project_system_api_error(&value);
        assert_eq!(
            projection.displayed_error.as_deref(),
            Some("rate limited, retry later")
        );
    }

    #[test]
    fn keeps_non_wrapped_error_text() {
        let mut value = input();
        value.formatted_error = "plain failure".into();

        let projection = project_system_api_error(&value);

        assert_eq!(projection.displayed_error.as_deref(), Some("plain failure"));
    }

    #[test]
    fn truncates_non_verbose_errors_and_shows_expand_hint() {
        let mut value = input();
        value.formatted_error = "x".repeat(MAX_API_ERROR_CHARS + 10);
        let projection = project_system_api_error(&value);
        assert!(projection.show_expand_hint);
        assert_eq!(
            projection.displayed_error.as_ref().unwrap().chars().count(),
            MAX_API_ERROR_CHARS + 1
        );
    }

    #[test]
    fn verbose_mode_keeps_full_error_without_expand_hint() {
        let mut value = input();
        value.verbose = true;
        let full_error = "x".repeat(MAX_API_ERROR_CHARS + 10);
        value.formatted_error = full_error.clone();
        let projection = project_system_api_error(&value);
        assert!(!projection.show_expand_hint);
        assert_eq!(
            projection.displayed_error.as_deref(),
            Some(full_error.as_str())
        );
    }

    #[test]
    fn retry_text_uses_pluralization_and_api_timeout_suffix() {
        let mut value = input();
        value.countdown_ms = 4_000;
        value.api_timeout_ms = Some(60_000);
        let projection = project_system_api_error(&value);
        assert_eq!(projection.retry_in_seconds_live, Some(1));
        assert_eq!(
            projection.retry_text.as_deref(),
            Some("Retrying in 1 second\u{2026} (attempt 4/8) \u{00b7} API_TIMEOUT_MS=60000ms, try increasing it")
        );
    }
}
