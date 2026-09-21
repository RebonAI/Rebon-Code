//! Assistant text-message projection.
//!
//! The upgrade hint, the default model name, macOS keychain detection, and
//! the nested rate-limit state stay injected as plain inputs.

use crate::rate_limit::{
    project_rate_limit_message, RateLimitMessageInput, RateLimitMessageProjection,
};
use crate::user_text::NO_CONTENT_MESSAGE;

const NO_RESPONSE_REQUESTED: &str = "No response requested.";
const ERROR_MESSAGE_USER_ABORT: &str = "API Error: Request was aborted.";
const API_ERROR_MESSAGE_PREFIX: &str = "API Error";
const PROMPT_TOO_LONG_ERROR_MESSAGE: &str = "Prompt is too long";
const CREDIT_BALANCE_TOO_LOW_ERROR_MESSAGE: &str = "Credit balance is too low";
const INVALID_API_KEY_ERROR_MESSAGE: &str = "Not logged in · Please run /login";
const INVALID_API_KEY_ERROR_MESSAGE_EXTERNAL: &str = "Invalid API key · Fix external API key";
const ORG_DISABLED_ERROR_MESSAGE_ENV_KEY: &str =
    "Your ANTHROPIC_API_KEY belongs to a disabled organization · Update or unset the environment variable";
const ORG_DISABLED_ERROR_MESSAGE_ENV_KEY_WITH_OAUTH: &str =
    "Your ANTHROPIC_API_KEY belongs to a disabled organization · Unset the environment variable to use your subscription instead";
const TOKEN_REVOKED_ERROR_MESSAGE: &str = "OAuth token revoked · Please run /login";
const API_TIMEOUT_ERROR_MESSAGE: &str = "Request timed out";
const CUSTOM_OFF_SWITCH_MESSAGE: &str =
    "Opus is experiencing high load, please use /model to switch to Sonnet";
const MACOS_KEYCHAIN_HINT: &str = "· Run in another terminal: security unlock-keychain";
const RATE_LIMIT_PREFIXES: [&str; 5] = [
    "You've hit your",
    "You've used",
    "You're now using extra usage",
    "You're close to",
    "You're out of extra usage",
];
const STRIPPABLE_TAGS: [&str; 4] = [
    "commit_analysis",
    "context",
    "function_analysis",
    "pr_analysis",
];
const LOGIN_API_ERROR_PREFIX: &str = "Please run /login · API Error";
const PROVIDER_AUTH_FAILURE_PREFIX: &str = "Provider authentication failed · ";
const MESSAGE_ACTIONS_BACKGROUND: &str = "messageActionsBackground";
const DOT_COLOR_SELECTED: &str = "suggestion";
const DOT_COLOR_DEFAULT: &str = "text";

/// Everything the projection needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantTextMessageInput {
    /// Message text.
    pub text: String,
    /// Whether to add a top margin.
    pub add_margin: bool,
    /// Whether to show the leading dot.
    pub should_show_dot: bool,
    /// Whether verbose output is on.
    pub verbose: bool,
    /// Whether the message is selected.
    pub is_selected: bool,
    /// Platform-specific dot glyph.
    pub dot_glyph: String,
    /// Nested rate-limit state, when available.
    pub rate_limit_message: Option<RateLimitMessageInput>,
    /// Warning-level upgrade hint, when one applies.
    pub upgrade_hint: Option<String>,
    /// Display name of the default model.
    pub default_sonnet_model_name: String,
    /// Whether the macOS keychain is locked.
    pub is_keychain_locked: bool,
    /// Optional API timeout setting, in milliseconds.
    pub api_timeout_ms: Option<u64>,
}

/// The projection's outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantTextMessageProjection {
    /// Hidden because the text is empty/no-op.
    Hidden,
    /// Nested rate-limit renderer.
    RateLimit(RateLimitMessageProjection),
    /// Indented response block stack.
    Response(AssistantResponseDisplay),
    /// Plain markdown assistant text row.
    Markdown(AssistantMarkdownDisplay),
}

/// Generic response wrapper used by multiple special-case branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantResponseDisplay {
    /// Optional pinned height, in rows.
    pub height: Option<u16>,
    /// Render-ready block sequence.
    pub blocks: Vec<AssistantResponseBlock>,
}

/// One line inside an [`AssistantResponseDisplay`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantResponseBlock {
    /// Error-colored text.
    ErrorLine(String),
    /// Normal text line.
    TextLine(String),
    /// Dim text line.
    DimTextLine(String),
    /// The interrupted-by-user line
    /// ([`crate::wrappers::InterruptedByUserDisplay`]).
    InterruptedByUser,
    /// The expand hint ([`crate::wrappers::CtrlOToExpandDisplay`]).
    ExpandHint,
}

/// Dot metadata for the default markdown branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantDotDisplay {
    /// Dot glyph.
    pub glyph: String,
    /// Dot color.
    pub color: &'static str,
    /// Minimum width of the non-selectable dot column.
    pub min_width: u8,
    /// Whether the dot column is anchored to the left edge.
    pub from_left_edge: bool,
}

/// Default markdown branch display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantMarkdownDisplay {
    /// Top margin: 1 when the input asks for a margin.
    pub margin_top: u8,
    /// Background color when selected.
    pub background: Option<&'static str>,
    /// Optional leading dot.
    pub dot: Option<AssistantDotDisplay>,
    /// Markdown body text.
    pub markdown: String,
}

/// Whether `text` starts with one of the `RATE_LIMIT_PREFIXES`.
pub fn is_rate_limit_error_message(text: &str) -> bool {
    RATE_LIMIT_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

/// Whether the text carries no visible content to render.
pub fn is_empty_assistant_message_text(text: &str) -> bool {
    strip_prompt_xml_tags(text).trim().is_empty() || text.trim() == NO_CONTENT_MESSAGE
}

/// Project one assistant text message.
pub fn project_assistant_text_message(
    input: &AssistantTextMessageInput,
) -> AssistantTextMessageProjection {
    if is_empty_assistant_message_text(&input.text) {
        return AssistantTextMessageProjection::Hidden;
    }

    if is_rate_limit_error_message(&input.text) {
        return AssistantTextMessageProjection::RateLimit(
            input.rate_limit_message.as_ref().map_or(
                RateLimitMessageProjection {
                    text: input.text.clone(),
                    upsell_message: None,
                    should_auto_open_rate_limit_options_menu: false,
                    next_has_opened_interactive_menu: false,
                },
                project_rate_limit_message,
            ),
        );
    }

    match input.text.as_str() {
        NO_RESPONSE_REQUESTED => AssistantTextMessageProjection::Hidden,
        PROMPT_TOO_LONG_ERROR_MESSAGE => {
            let upgrade_suffix = input
                .upgrade_hint
                .as_ref()
                .map(|hint| format!(" · {hint}"))
                .unwrap_or_default();
            AssistantTextMessageProjection::Response(AssistantResponseDisplay {
                height: Some(1),
                blocks: vec![AssistantResponseBlock::ErrorLine(format!(
                    "Context limit reached · /compact or /clear to continue{}",
                    upgrade_suffix
                ))],
            })
        }
        CREDIT_BALANCE_TOO_LOW_ERROR_MESSAGE => {
            AssistantTextMessageProjection::Response(AssistantResponseDisplay {
                height: Some(1),
                blocks: vec![AssistantResponseBlock::ErrorLine(
                    "Credit balance too low · Add funds: https://platform.claude.com/settings/billing"
                        .to_string(),
                )],
            })
        }
        INVALID_API_KEY_ERROR_MESSAGE => {
            let mut blocks = vec![AssistantResponseBlock::ErrorLine(
                INVALID_API_KEY_ERROR_MESSAGE.to_string(),
            )];
            if input.is_keychain_locked {
                blocks.push(AssistantResponseBlock::DimTextLine(
                    MACOS_KEYCHAIN_HINT.to_string(),
                ));
            }
            AssistantTextMessageProjection::Response(AssistantResponseDisplay {
                height: None,
                blocks,
            })
        }
        INVALID_API_KEY_ERROR_MESSAGE_EXTERNAL => {
            AssistantTextMessageProjection::Response(AssistantResponseDisplay {
                height: Some(1),
                blocks: vec![AssistantResponseBlock::ErrorLine(
                    INVALID_API_KEY_ERROR_MESSAGE_EXTERNAL.to_string(),
                )],
            })
        }
        ORG_DISABLED_ERROR_MESSAGE_ENV_KEY | ORG_DISABLED_ERROR_MESSAGE_ENV_KEY_WITH_OAUTH => {
            AssistantTextMessageProjection::Response(AssistantResponseDisplay {
                height: None,
                blocks: vec![AssistantResponseBlock::ErrorLine(input.text.clone())],
            })
        }
        TOKEN_REVOKED_ERROR_MESSAGE => {
            AssistantTextMessageProjection::Response(AssistantResponseDisplay {
                height: Some(1),
                blocks: vec![AssistantResponseBlock::ErrorLine(
                    TOKEN_REVOKED_ERROR_MESSAGE.to_string(),
                )],
            })
        }
        API_TIMEOUT_ERROR_MESSAGE => {
            let suffix = input
                .api_timeout_ms
                .map(|timeout| format!(" (API_TIMEOUT_MS={timeout}ms, try increasing it)"))
                .unwrap_or_default();
            AssistantTextMessageProjection::Response(AssistantResponseDisplay {
                height: Some(1),
                blocks: vec![AssistantResponseBlock::ErrorLine(format!(
                    "{API_TIMEOUT_ERROR_MESSAGE}{suffix}"
                ))],
            })
        }
        CUSTOM_OFF_SWITCH_MESSAGE => {
            AssistantTextMessageProjection::Response(AssistantResponseDisplay {
                height: None,
                blocks: vec![
                    AssistantResponseBlock::ErrorLine(
                        "We are experiencing high demand for Opus 4.".to_string(),
                    ),
                    AssistantResponseBlock::TextLine(format!(
                        "To continue immediately, use /model to switch to {} and continue coding.",
                        input.default_sonnet_model_name
                    )),
                ],
            })
        }
        ERROR_MESSAGE_USER_ABORT => AssistantTextMessageProjection::Response(AssistantResponseDisplay {
            height: Some(1),
            blocks: vec![AssistantResponseBlock::InterruptedByUser],
        }),
        _ if starts_with_api_error_prefix(&input.text) => {
            let (error_text, truncated) = normalize_api_error_text(&input.text, input.verbose);
            let mut blocks = vec![AssistantResponseBlock::ErrorLine(error_text)];
            if truncated {
                blocks.push(AssistantResponseBlock::ExpandHint);
            }
            AssistantTextMessageProjection::Response(AssistantResponseDisplay {
                height: None,
                blocks,
            })
        }
        _ => AssistantTextMessageProjection::Markdown(AssistantMarkdownDisplay {
            margin_top: u8::from(input.add_margin),
            background: input.is_selected.then_some(MESSAGE_ACTIONS_BACKGROUND),
            dot: input.should_show_dot.then(|| AssistantDotDisplay {
                glyph: input.dot_glyph.clone(),
                color: if input.is_selected {
                    DOT_COLOR_SELECTED
                } else {
                    DOT_COLOR_DEFAULT
                },
                min_width: 2,
                from_left_edge: true,
            }),
            markdown: input.text.clone(),
        }),
    }
}

fn starts_with_api_error_prefix(text: &str) -> bool {
    text.starts_with(API_ERROR_MESSAGE_PREFIX)
        || text.starts_with(LOGIN_API_ERROR_PREFIX)
        || text.starts_with(PROVIDER_AUTH_FAILURE_PREFIX)
}

/// The error body as the transcript shows it, and whether it was cut.
///
/// The cut is [`crate::system_api_error::truncate_api_error`] rather than a
/// second rule: the same error text reaches both projections, and the
/// `ctrl+o` hint below only means anything if the two agree on when there is
/// more to expand.
fn normalize_api_error_text(text: &str, verbose: bool) -> (String, bool) {
    if text == API_ERROR_MESSAGE_PREFIX {
        return (
            format!("{API_ERROR_MESSAGE_PREFIX}: Please wait a moment and try again."),
            false,
        );
    }
    crate::system_api_error::truncate_api_error(text, verbose)
}

fn strip_prompt_xml_tags(content: &str) -> String {
    let mut result = content.to_string();
    for tag in STRIPPABLE_TAGS {
        loop {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            let Some(start) = result.find(&open) else {
                break;
            };
            let Some(rel_end) = result[start..].find(&close) else {
                break;
            };
            let mut end = start + rel_end + close.len();
            if result[end..].starts_with('\n') {
                end += 1;
            }
            result.replace_range(start..end, "");
        }
    }
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(text: &str) -> AssistantTextMessageInput {
        AssistantTextMessageInput {
            text: text.to_string(),
            add_margin: true,
            should_show_dot: true,
            verbose: false,
            is_selected: false,
            dot_glyph: "\u{25cf}".into(),
            rate_limit_message: None,
            upgrade_hint: Some("/model sonnet[1m]".into()),
            default_sonnet_model_name: "Sonnet 4.5".into(),
            is_keychain_locked: false,
            api_timeout_ms: None,
        }
    }

    #[test]
    fn a_long_api_error_is_cut_and_offers_the_expand_hint() {
        // The hint was unreachable while this projection carried its own
        // "never truncated" stub: the block existed, the renderer drew it,
        // and nothing ever produced it.
        let long = format!(
            "{API_ERROR_MESSAGE_PREFIX}: {}",
            "x".repeat(crate::system_api_error::MAX_API_ERROR_CHARS + 50)
        );
        let projection = project_assistant_text_message(&input(&long));

        let AssistantTextMessageProjection::Response(display) = projection else {
            panic!("an API error projects as a response");
        };
        assert!(
            display.blocks.contains(&AssistantResponseBlock::ExpandHint),
            "{:?}",
            display.blocks
        );
    }

    #[test]
    fn verbose_shows_the_whole_api_error_without_a_hint() {
        let long = format!(
            "{API_ERROR_MESSAGE_PREFIX}: {}",
            "x".repeat(crate::system_api_error::MAX_API_ERROR_CHARS + 50)
        );
        let mut verbose = input(&long);
        verbose.verbose = true;
        let projection = project_assistant_text_message(&verbose);

        let AssistantTextMessageProjection::Response(display) = projection else {
            panic!("an API error projects as a response");
        };
        assert!(!display.blocks.contains(&AssistantResponseBlock::ExpandHint));
        match display.blocks.first() {
            Some(AssistantResponseBlock::ErrorLine(line)) => {
                assert_eq!(line.chars().count(), long.chars().count());
            }
            other => panic!("expected an error line, got {other:?}"),
        }
    }

    #[test]
    fn hides_empty_no_content_and_no_response_requested_cases() {
        assert_eq!(
            project_assistant_text_message(&input("   ")),
            AssistantTextMessageProjection::Hidden
        );
        assert_eq!(
            project_assistant_text_message(&input("<context>x</context>\n")),
            AssistantTextMessageProjection::Hidden
        );
        assert_eq!(
            project_assistant_text_message(&input(NO_RESPONSE_REQUESTED)),
            AssistantTextMessageProjection::Hidden
        );
    }

    #[test]
    fn routes_rate_limit_messages_into_nested_projection() {
        let mut value = input("You've hit your session limit");
        value.rate_limit_message = Some(RateLimitMessageInput {
            text: value.text.clone(),
            subscription_type: "pro".into(),
            rate_limit_tier: "other".into(),
            should_process_mock_limits: false,
            is_subscriber: true,
            has_opened_interactive_menu: false,
            subscription_limits_status: "allowed_warning".into(),
            subscription_limits_has_resets_at: true,
            subscription_limits_is_using_overage: false,
            has_open_rate_limit_options_handler: false,
            extra_usage_command_enabled: true,
            has_billing_access: false,
        });

        let AssistantTextMessageProjection::RateLimit(projection) =
            project_assistant_text_message(&value)
        else {
            panic!("expected rate limit branch");
        };

        assert_eq!(projection.text, "You've hit your session limit");
        assert_eq!(
            projection.upsell_message.as_deref(),
            Some("/upgrade or /extra-usage to finish what you\u{2019}re working on.")
        );
    }

    #[test]
    fn prompt_too_long_includes_upgrade_hint_when_present() {
        let AssistantTextMessageProjection::Response(display) =
            project_assistant_text_message(&input(PROMPT_TOO_LONG_ERROR_MESSAGE))
        else {
            panic!("expected response branch");
        };

        assert_eq!(display.height, Some(1));
        assert_eq!(
            display.blocks,
            vec![AssistantResponseBlock::ErrorLine(
                "Context limit reached · /compact or /clear to continue · /model sonnet[1m]".into()
            )]
        );
    }

    #[test]
    fn invalid_api_key_shows_optional_keychain_hint() {
        let mut value = input(INVALID_API_KEY_ERROR_MESSAGE);
        value.is_keychain_locked = true;
        let AssistantTextMessageProjection::Response(display) =
            project_assistant_text_message(&value)
        else {
            panic!("expected response branch");
        };

        assert_eq!(
            display.blocks,
            vec![
                AssistantResponseBlock::ErrorLine(INVALID_API_KEY_ERROR_MESSAGE.into()),
                AssistantResponseBlock::DimTextLine(MACOS_KEYCHAIN_HINT.into()),
            ]
        );
    }

    #[test]
    fn api_timeout_and_capacity_off_switch_branches_are_expected() {
        let mut timeout = input(API_TIMEOUT_ERROR_MESSAGE);
        timeout.api_timeout_ms = Some(30_000);
        let AssistantTextMessageProjection::Response(timeout_display) =
            project_assistant_text_message(&timeout)
        else {
            panic!("expected response branch");
        };
        assert_eq!(
            timeout_display.blocks,
            vec![AssistantResponseBlock::ErrorLine(
                "Request timed out (API_TIMEOUT_MS=30000ms, try increasing it)".into()
            )]
        );

        let AssistantTextMessageProjection::Response(off_switch_display) =
            project_assistant_text_message(&input(CUSTOM_OFF_SWITCH_MESSAGE))
        else {
            panic!("expected response branch");
        };
        assert_eq!(
            off_switch_display.blocks[1],
            AssistantResponseBlock::TextLine(
                "To continue immediately, use /model to switch to Sonnet 4.5 and continue coding."
                    .into()
            )
        );
    }

    #[test]
    fn api_error_prefix_special_cases_keep_full_text() {
        let AssistantTextMessageProjection::Response(display) =
            project_assistant_text_message(&input(API_ERROR_MESSAGE_PREFIX))
        else {
            panic!("expected response branch");
        };
        assert_eq!(
            display.blocks,
            vec![AssistantResponseBlock::ErrorLine(
                "API Error: Please wait a moment and try again.".into()
            )]
        );

        // An error that fits the cap is shown whole and earns no hint. This
        // used to be asserted with a body one character *over*
        // `MAX_API_ERROR_CHARS`, which only passed while this projection
        // carried a stub that never truncated — the cut and the `ctrl+o`
        // hint are covered by their own tests above.
        let prefix = "API Error: ";
        let at_cap = format!(
            "{prefix}{}",
            "x".repeat(crate::system_api_error::MAX_API_ERROR_CHARS - prefix.chars().count())
        );
        assert_eq!(
            at_cap.chars().count(),
            crate::system_api_error::MAX_API_ERROR_CHARS
        );
        let projection = project_assistant_text_message(&input(&at_cap));
        let AssistantTextMessageProjection::Response(display) = projection else {
            panic!("expected response branch");
        };
        assert_eq!(
            display.blocks,
            vec![AssistantResponseBlock::ErrorLine(at_cap)]
        );
    }

    #[test]
    fn user_abort_routes_to_interrupted_component() {
        let AssistantTextMessageProjection::Response(display) =
            project_assistant_text_message(&input(ERROR_MESSAGE_USER_ABORT))
        else {
            panic!("expected response branch");
        };
        assert_eq!(display.height, Some(1));
        assert_eq!(
            display.blocks,
            vec![AssistantResponseBlock::InterruptedByUser]
        );
    }

    #[test]
    fn default_markdown_branch_threads_dot_and_selected_background() {
        let mut value = input("plain markdown");
        value.is_selected = true;
        let AssistantTextMessageProjection::Markdown(display) =
            project_assistant_text_message(&value)
        else {
            panic!("expected markdown branch");
        };

        assert_eq!(display.margin_top, 1);
        assert_eq!(display.background, Some(MESSAGE_ACTIONS_BACKGROUND));
        assert_eq!(
            display.dot,
            Some(AssistantDotDisplay {
                glyph: "\u{25cf}".into(),
                color: DOT_COLOR_SELECTED,
                min_width: 2,
                from_left_edge: true,
            })
        );
        assert_eq!(display.markdown, "plain markdown");
    }
}
