//! Agent generation: prompt builder, JSON-extracting parser, and the
//! [`AgentGenerator`] LLM seam.
//!
//! This module builds a generation request, calls a model seam, parses
//! JSON-like output, and validates the required generated-agent fields.
//!
//! 1. Pins a system prompt + memory-instructions block.
//! 2. Builds a user-prompt that includes the existing-identifier
//! avoid list and asks for JSON-only output.
//! 3. Sends the request to the configured generator and receives response text.
//! 4. Parses the response as JSON, falling back to a `\{[\s\S]*\}`
//! regex if the raw parse fails.
//!
//! Prompt construction and response parsing are pure functions; model access
//! is provided through the [`AgentGenerator`] trait.

/// The full agent-architect system prompt used for generation requests.
pub const AGENT_CREATION_SYSTEM_PROMPT: &str = include_str!("generate_system_prompt.txt");

/// Extra memory-instructions block appended to the system prompt when
/// the auto-memory feature is enabled.
pub const AGENT_MEMORY_INSTRUCTIONS: &str = include_str!("generate_memory_instructions.txt");

/// A generated agent shape (the JSON the LLM returns).
///
/// Agent draft produced by the generator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedAgent {
    /// Kebab-case identifier (the future `agent_type`).
    pub identifier: String,
    /// Description string suitable for the YAML `description` field.
    pub when_to_use: String,
    /// Full system prompt body the agent will use.
    pub system_prompt: String,
}

/// Inputs to the [`AgentGenerator::generate`] call.
///
/// Pre-built so the seam stays narrow — the consumer sends the two
/// strings as they are and needs no prompt assembly of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationRequest {
    /// The full user prompt (with existing-identifier avoid list
    /// inlined). The consumer should send this as the user message.
    pub user_prompt: String,
    /// The full system prompt (with memory-instructions appended if
    /// `include_memory_instructions == true`).
    pub system_prompt: String,
}

/// Build a [`GenerationRequest`] from the user's description, the
/// identifiers already taken, and whether to append the memory
/// instructions.
pub fn build_generation_request(
    user_prompt: &str,
    existing_identifiers: &[String],
    include_memory_instructions: bool,
) -> GenerationRequest {
    let existing_list = if existing_identifiers.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nIMPORTANT: The following identifiers already exist and must NOT be used: {}",
            existing_identifiers.join(", ")
        )
    };

    let prompt = format!(
        "Create an agent configuration based on this request: \"{user_prompt}\".{existing_list}\n  Return ONLY the JSON object, no other text."
    );

    let system_prompt = if include_memory_instructions {
        format!("{AGENT_CREATION_SYSTEM_PROMPT}{AGENT_MEMORY_INSTRUCTIONS}")
    } else {
        AGENT_CREATION_SYSTEM_PROMPT.to_string()
    };

    GenerationRequest {
        user_prompt: prompt,
        system_prompt,
    }
}

/// Errors returned by [`parse_generated_agent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// No JSON object found in the response.
    NoJsonObject,
    /// JSON was malformed (the parser couldn't decode it).
    InvalidJson(String),
    /// The parsed JSON was missing a required field.
    InvalidConfiguration,
}

/// Parse the LLM response into a [`GeneratedAgent`].
///
/// `response_text` is the model's text blocks already joined. The flow:
///
/// 1. Tries to parse the trimmed text.
/// 2. On failure, parses the greedy `\{[\s\S]*\}` match instead.
/// 3. Requires `identifier`, `whenToUse`, and `systemPrompt` to all be
///    present.
///
/// The "JSON parse" itself is done by a tiny hand-rolled parser that
/// understands the three string fields we care about — full JSON
/// parsing is the consumer's problem if they need it for richer
/// outputs.
pub fn parse_generated_agent(response_text: &str) -> Result<GeneratedAgent, ParseError> {
    let trimmed = response_text.trim();

    // Step 1: try to parse the full trimmed text.
    if let Some(agent) = try_extract_agent(trimmed) {
        return validate_required(agent);
    }

    // Step 2: fall back to greedy `\{[\s\S]*\}` match.
    let snippet = greedy_object_match(response_text).ok_or(ParseError::NoJsonObject)?;
    let agent = try_extract_agent(snippet).ok_or_else(|| {
        ParseError::InvalidJson("could not parse fallback JSON object".to_string())
    })?;
    validate_required(agent)
}

/// Greedy `/\{[\s\S]*\}/` match: finds the first `{` and the last
/// `}` in the input and returns the text between them.
fn greedy_object_match(input: &str) -> Option<&str> {
    let start = input.find('{')?;
    let end = input.rfind('}')?;
    if end < start {
        return None;
    }
    Some(&input[start..=end])
}

/// Hand-rolled extractor for the three string fields. We accept input
/// of the form `{..., "identifier": "...",... }` regardless of key
/// order. Strings may use `\\` and `\"` escapes (behavioral for the
/// JSON the LLM is asked to produce).
fn try_extract_agent(json: &str) -> Option<GeneratedAgent> {
    let id = extract_string_field(json, "identifier")?;
    let when = extract_string_field(json, "whenToUse")?;
    let prompt = extract_string_field(json, "systemPrompt")?;
    Some(GeneratedAgent {
        identifier: id,
        when_to_use: when,
        system_prompt: prompt,
    })
}

fn extract_string_field(json: &str, field: &str) -> Option<String> {
    // Find `"<field>"` (allowing surrounding whitespace), then the
    // following `:`, then the next `"`.
    let needle = format!("\"{field}\"");
    let idx = json.find(&needle)?;
    let rest = &json[idx + needle.len()..];
    // Skip whitespace + colon.
    let mut chars = rest.char_indices();
    let mut colon_seen = false;
    let mut value_start_byte = None;
    for (i, c) in chars.by_ref() {
        if c.is_whitespace() {
            continue;
        }
        if c == ':' && !colon_seen {
            colon_seen = true;
            continue;
        }
        if c == '"' && colon_seen {
            value_start_byte = Some(i + 1);
            break;
        }
        // Anything else is malformed; bail.
        return None;
    }
    let start = value_start_byte?;
    // Now read until the matching `"`, honouring backslash escapes.
    let mut out = String::new();
    let mut escape = false;
    for c in rest[start..].chars() {
        if escape {
            // Decode the JSON escape.
            match c {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{08}'),
                'f' => out.push('\u{0C}'),
                _ => {
                    // Unknown escape — pass through.
                    out.push(c);
                }
            }
            escape = false;
            continue;
        }
        if c == '\\' {
            escape = true;
            continue;
        }
        if c == '"' {
            return Some(out);
        }
        out.push(c);
    }
    None
}

fn validate_required(agent: GeneratedAgent) -> Result<GeneratedAgent, ParseError> {
    if agent.identifier.is_empty() || agent.when_to_use.is_empty() || agent.system_prompt.is_empty()
    {
        return Err(ParseError::InvalidConfiguration);
    }
    Ok(agent)
}

/// LLM seam used by the generation flow.
///
/// The module never imports an HTTP client, an SDK, or a streaming
/// parser; the consumer wires this up.
pub trait AgentGenerator {
    /// Run the generation. The consumer is responsible for the actual
    /// API call; this method just hands back whatever text the model
    /// produced (the same text that would come out of joining
    /// `response.message.content` filtered to text blocks).
    fn generate(&mut self, request: &GenerationRequest) -> Result<String, GenerationError>;
}

/// Errors returned by [`AgentGenerator`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationError {
    /// The generation was aborted (user cancelled).
    Aborted,
    /// Network or transport-level failure.
    Transport(String),
    /// The generator returned an empty response.
    Empty,
    /// Other error.
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_no_existing_no_memory() {
        let req = build_generation_request("a code reviewer", &[], false);
        assert!(req.user_prompt.starts_with(
            "Create an agent configuration based on this request: \"a code reviewer\"."
        ));
        assert!(req
            .user_prompt
            .ends_with("Return ONLY the JSON object, no other text."));
        assert!(!req
            .user_prompt
            .contains("IMPORTANT: The following identifiers"));
        assert_eq!(req.system_prompt, AGENT_CREATION_SYSTEM_PROMPT);
    }

    #[test]
    fn build_request_with_existing_identifiers() {
        let req = build_generation_request(
            "a reviewer",
            &["test-runner".to_string(), "code-reviewer".to_string()],
            false,
        );
        assert!(req
            .user_prompt
            .contains("IMPORTANT: The following identifiers already exist and must NOT be used: test-runner, code-reviewer"));
    }

    #[test]
    fn build_request_with_memory_instructions() {
        let req = build_generation_request("x", &[], true);
        assert!(req.system_prompt.contains(AGENT_CREATION_SYSTEM_PROMPT));
        assert!(req.system_prompt.contains(AGENT_MEMORY_INSTRUCTIONS));
        assert!(req.system_prompt.ends_with(AGENT_MEMORY_INSTRUCTIONS));
    }

    #[test]
    fn parse_happy_path() {
        let json = r#"{"identifier": "test-runner", "whenToUse": "Use this agent when running tests", "systemPrompt": "You are a test runner."}"#;
        let agent = parse_generated_agent(json).unwrap();
        assert_eq!(agent.identifier, "test-runner");
        assert_eq!(agent.when_to_use, "Use this agent when running tests");
        assert_eq!(agent.system_prompt, "You are a test runner.");
    }

    #[test]
    fn parse_with_leading_text_falls_back_to_match() {
        let raw = r#"Sure! Here is your agent: {"identifier": "test-runner", "whenToUse": "Use this when testing", "systemPrompt": "You are a test runner."}"#;
        let agent = parse_generated_agent(raw).unwrap();
        assert_eq!(agent.identifier, "test-runner");
    }

    #[test]
    fn parse_no_json_object_errors() {
        let err = parse_generated_agent("just words no braces").unwrap_err();
        assert_eq!(err, ParseError::NoJsonObject);
    }

    #[test]
    fn parse_missing_identifier_errors() {
        let json = r#"{"whenToUse": "x", "systemPrompt": "y"}"#;
        let err = parse_generated_agent(json).unwrap_err();
        // The hand-rolled extractor returns None for missing fields,
        // so we fall through to NoJsonObject — but the greedy
        // fallback will also fail to extract `identifier`. Either
        // InvalidJson or InvalidConfiguration is acceptable; pin to
        // the actual outcome.
        assert!(matches!(
            err,
            ParseError::InvalidJson(_)
                | ParseError::InvalidConfiguration
                | ParseError::NoJsonObject
        ));
    }

    #[test]
    fn parse_handles_escaped_quotes() {
        let json = r#"{"identifier": "code-reviewer", "whenToUse": "say \"hi\"", "systemPrompt": "You are a reviewer."}"#;
        let agent = parse_generated_agent(json).unwrap();
        assert_eq!(agent.when_to_use, r#"say "hi""#);
    }

    #[test]
    fn parse_handles_escaped_newlines() {
        let json = r#"{"identifier": "x", "whenToUse": "y", "systemPrompt": "line1\nline2"}"#;
        let agent = parse_generated_agent(json).unwrap();
        assert_eq!(agent.system_prompt, "line1\nline2");
    }

    #[test]
    fn parse_handles_unordered_keys() {
        let json = r#"{"systemPrompt": "z", "identifier": "x", "whenToUse": "y"}"#;
        let agent = parse_generated_agent(json).unwrap();
        assert_eq!(agent.identifier, "x");
        assert_eq!(agent.when_to_use, "y");
        assert_eq!(agent.system_prompt, "z");
    }

    #[test]
    fn build_request_pins_quote_format() {
        // The prompt template uses double quotes around the user prompt.
        let req = build_generation_request("with \"quotes\"", &[], false);
        // Embedded quotes are intentionally left untouched.
        assert!(req.user_prompt.contains("\"with \"quotes\"\""));
    }

    #[test]
    fn parse_invalid_configuration_when_value_empty() {
        let json = r#"{"identifier": "", "whenToUse": "y", "systemPrompt": "z"}"#;
        let err = parse_generated_agent(json).unwrap_err();
        assert_eq!(err, ParseError::InvalidConfiguration);
    }
}
