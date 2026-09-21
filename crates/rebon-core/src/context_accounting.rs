use rebon_api::{ContentBlock as ApiContentBlock, Message as ApiMessage, Tool as ApiTool};
use serde_json::Value;

pub(crate) fn approx_tokens_from_bytes(bytes: usize) -> u64 {
    ((bytes as u64).saturating_add(3)) / 4
}

fn estimate_json_tokens(value: &Value) -> u64 {
    serde_json::to_string(value)
        .map(|s| approx_tokens_from_bytes(s.len()))
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSectionReport {
    pub name: String,
    pub kind: String,
    pub estimated_tokens: u64,
    pub byte_count: Option<usize>,
    pub char_count: Option<usize>,
    pub details: Vec<PromptSectionReport>,
}

impl PromptSectionReport {
    pub fn new(name: impl Into<String>, kind: impl Into<String>, estimated_tokens: u64) -> Self {
        Self {
            name: name.into(),
            kind: kind.into(),
            estimated_tokens,
            byte_count: None,
            char_count: None,
            details: Vec::new(),
        }
    }

    pub fn with_counts(mut self, byte_count: usize, char_count: usize) -> Self {
        self.byte_count = Some(byte_count);
        self.char_count = Some(char_count);
        self
    }

    pub fn with_details(mut self, details: Vec<PromptSectionReport>) -> Self {
        self.estimated_tokens = self.estimated_tokens.saturating_add(
            details
                .iter()
                .map(|detail| detail.estimated_tokens)
                .sum::<u64>(),
        );
        self.details = details;
        self
    }

    pub fn top_details(&self, limit: usize) -> Vec<&PromptSectionReport> {
        let mut details = self.details.iter().collect::<Vec<_>>();
        details.sort_by(|left, right| {
            right
                .estimated_tokens
                .cmp(&left.estimated_tokens)
                .then_with(|| left.name.cmp(&right.name))
        });
        details.truncate(limit);
        details
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptCostReport {
    pub sections: Vec<PromptSectionReport>,
    pub estimated_tokens: u64,
}

impl PromptCostReport {
    pub fn new(sections: Vec<PromptSectionReport>) -> Self {
        let estimated_tokens = sections
            .iter()
            .map(|section| section.estimated_tokens)
            .sum::<u64>();
        Self {
            sections,
            estimated_tokens,
        }
    }

    pub fn top_sections(&self, limit: usize) -> Vec<&PromptSectionReport> {
        let mut sections = self.sections.iter().collect::<Vec<_>>();
        sections.sort_by(|left, right| {
            right
                .estimated_tokens
                .cmp(&left.estimated_tokens)
                .then_with(|| left.name.cmp(&right.name))
        });
        sections.truncate(limit);
        sections
    }
}

pub fn prompt_text_section_report(
    name: impl Into<String>,
    kind: impl Into<String>,
    text: &str,
) -> PromptSectionReport {
    PromptSectionReport::new(name, kind, approx_tokens_from_bytes(text.len()))
        .with_counts(text.len(), text.chars().count())
}

pub fn prompt_tool_metadata_report(
    name: impl Into<String>,
    tools: &[ApiTool],
) -> PromptSectionReport {
    let details = tools
        .iter()
        .map(|tool| {
            let schema_tokens = estimate_json_tokens(&tool.input_schema);
            let tokens = approx_tokens_from_bytes(tool.name.len())
                .saturating_add(approx_tokens_from_bytes(tool.description.len()))
                .saturating_add(schema_tokens);
            let bytes = tool
                .name
                .len()
                .saturating_add(tool.description.len())
                .saturating_add(
                    serde_json::to_string(&tool.input_schema)
                        .map(|s| s.len())
                        .unwrap_or_default(),
                );
            PromptSectionReport::new(tool.name.clone(), "tool_metadata", tokens).with_counts(
                bytes,
                tool.name.chars().count() + tool.description.chars().count(),
            )
        })
        .collect::<Vec<_>>();
    PromptSectionReport::new(name, "tool_metadata_collection", 0).with_details(details)
}

fn estimate_content_block_tokens(block: &ApiContentBlock) -> u64 {
    match block {
        ApiContentBlock::Text(t) => approx_tokens_from_bytes(t.text.len()),
        ApiContentBlock::ToolResult(tr) => approx_tokens_from_bytes(tr.content.approx_len()),
        ApiContentBlock::ToolUse(tu) => {
            approx_tokens_from_bytes(tu.name.len()).saturating_add(estimate_json_tokens(&tu.input))
        }
        ApiContentBlock::Thinking(th) => approx_tokens_from_bytes(th.thinking.len())
            .saturating_add(
                th.data
                    .as_ref()
                    .map(|data| approx_tokens_from_bytes(data.len()))
                    .unwrap_or_default(),
            ),
        ApiContentBlock::ServerToolUse(stu) => approx_tokens_from_bytes(stu.name.len())
            .saturating_add(estimate_json_tokens(&stu.input)),
        ApiContentBlock::WebSearchResult(wsr) => {
            let result_tokens = wsr.results.iter().fold(0u64, |acc, result| {
                acc.saturating_add(approx_tokens_from_bytes(result.title.len()))
                    .saturating_add(approx_tokens_from_bytes(result.url.len()))
                    .saturating_add(
                        result
                            .snippet
                            .as_ref()
                            .map(|s| approx_tokens_from_bytes(s.len()))
                            .unwrap_or_default(),
                    )
            });
            result_tokens.saturating_add(
                wsr.raw_content
                    .as_ref()
                    .map(estimate_json_tokens)
                    .unwrap_or_default(),
            )
        }
        ApiContentBlock::Compaction(block) => block
            .content
            .as_ref()
            .map(|s| approx_tokens_from_bytes(s.len()))
            .unwrap_or_default(),
        // Use a fixed estimate for model-visible image input instead
        // of counting base64 bytes, matching codex-ref's discounted
        // image accounting in spirit.
        ApiContentBlock::Image(_) => 2_000,
        // Generated images are persisted to disk and are not replayed
        // into OpenAI Responses input by `emit_assistant_message`.
        ApiContentBlock::GeneratedImage(gi) => gi
            .revised_prompt
            .as_ref()
            .map(|s| approx_tokens_from_bytes(s.len()))
            .unwrap_or_default(),
    }
}

pub(crate) fn estimate_messages_input_tokens(system: Option<&str>, messages: &[ApiMessage]) -> u32 {
    let system_tokens = system
        .map(|s| approx_tokens_from_bytes(s.len()))
        .unwrap_or(0);
    let message_tokens = estimate_message_slice_tokens(messages);
    system_tokens
        .saturating_add(message_tokens)
        .min(u32::MAX as u64) as u32
}

pub fn estimate_message_slice_tokens(messages: &[ApiMessage]) -> u64 {
    messages.iter().fold(0u64, |acc, message| {
        let role_overhead = 4u64;
        let content_tokens = message.content.iter().fold(0u64, |inner, block| {
            inner.saturating_add(estimate_content_block_tokens(block))
        });
        acc.saturating_add(role_overhead)
            .saturating_add(content_tokens)
    })
}

pub fn prompt_messages_report(
    name: impl Into<String>,
    messages: &[ApiMessage],
) -> PromptSectionReport {
    let details = messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let tokens = estimate_message_slice_tokens(std::slice::from_ref(message));
            PromptSectionReport::new(
                format!("message[{index}]:{:?}", message.role),
                "message",
                tokens,
            )
        })
        .collect::<Vec<_>>();
    PromptSectionReport::new(name, "message_collection", 0).with_details(details)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_api::{Message as ApiMessage, Role, TextBlock, ToolResultBlock};

    #[test]
    fn estimate_messages_input_tokens_counts_system_text_and_tool_results() {
        let messages = vec![ApiMessage {
            role: Role::User,
            content: vec![
                ApiContentBlock::Text(TextBlock {
                    text: "abcd".into(),
                }),
                ApiContentBlock::ToolResult(ToolResultBlock {
                    tool_use_id: "toolu_1".into(),
                    content: rebon_api::ToolResultContent::text("x".repeat(4_000)),
                    is_error: false,
                }),
            ],
        }];

        let tokens = estimate_messages_input_tokens(Some("system text"), &messages);

        assert!(
            tokens >= 1_000,
            "large tool result should materially affect pre-turn token estimate, got {tokens}"
        );
    }

    #[test]
    fn estimate_messages_input_tokens_counts_encrypted_reasoning_data() {
        let visible_only = vec![ApiMessage {
            role: Role::Assistant,
            content: vec![ApiContentBlock::Thinking(rebon_api::ThinkingBlock {
                thinking: "summary".into(),
                signature: None,
                data: None,
            })],
        }];
        let encrypted = vec![ApiMessage {
            role: Role::Assistant,
            content: vec![ApiContentBlock::Thinking(rebon_api::ThinkingBlock {
                thinking: "summary".into(),
                signature: None,
                data: Some("x".repeat(4_000)),
            })],
        }];

        let visible_tokens = estimate_messages_input_tokens(None, &visible_only);
        let encrypted_tokens = estimate_messages_input_tokens(None, &encrypted);

        assert!(
            encrypted_tokens >= visible_tokens.saturating_add(1_000),
            "encrypted reasoning must contribute to replay cost: visible={visible_tokens}, encrypted={encrypted_tokens}"
        );
    }

    #[test]
    fn estimate_messages_input_tokens_discounts_images() {
        let base64 = "a".repeat(80_000);
        let messages = vec![ApiMessage {
            role: Role::User,
            content: vec![ApiContentBlock::Image(
                rebon_api::types::ImageBlock::base64("image/png", base64),
            )],
        }];

        let tokens = estimate_messages_input_tokens(None, &messages);

        assert!(
            tokens < 10_000,
            "image estimate should be fixed-cost rather than raw base64-sized, got {tokens}"
        );
    }

    #[test]
    fn estimate_messages_input_tokens_ignores_generated_image_bytes() {
        let messages = vec![ApiMessage {
            role: Role::Assistant,
            content: vec![ApiContentBlock::GeneratedImage(
                rebon_api::types::GeneratedImageBlock {
                    id: "ig_1".into(),
                    status: Some("completed".into()),
                    revised_prompt: Some("small prompt".into()),
                    media_type: "image/png".into(),
                    data: "a".repeat(200_000),
                    saved_path: Some("C:/tmp/ig_1.png".into()),
                },
            )],
        }];

        let tokens = estimate_messages_input_tokens(None, &messages);

        assert!(
            tokens < 100,
            "generated image base64 should not dominate replay estimate, got {tokens}"
        );
    }

    #[test]
    fn prompt_section_report_is_deterministic_and_sorts_top_details() {
        let tools = vec![
            ApiTool {
                name: "small".into(),
                description: "tiny".into(),
                input_schema: serde_json::json!({"type":"object"}),
            },
            ApiTool {
                name: "large".into(),
                description: "x".repeat(400),
                input_schema: serde_json::json!({"type":"object","properties":{"value":{"type":"string","description":"long"}}}),
            },
        ];

        let first = prompt_tool_metadata_report("tools", &tools);
        let second = prompt_tool_metadata_report("tools", &tools);

        assert_eq!(first, second);
        assert_eq!(first.details.len(), 2);
        assert_eq!(first.top_details(1)[0].name, "large");
        assert!(first.estimated_tokens >= first.details.iter().map(|d| d.estimated_tokens).sum());
    }

    #[test]
    fn prompt_text_section_report_counts_bytes_chars_and_tokens() {
        let report = prompt_text_section_report("example", "text", "abcdé");

        assert_eq!(report.byte_count, Some("abcdé".len()));
        assert_eq!(report.char_count, Some(5));
        assert_eq!(
            report.estimated_tokens,
            approx_tokens_from_bytes("abcdé".len())
        );
    }
}
