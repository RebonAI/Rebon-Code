//! The Anchored Minimal bootstrap profile.
//!
//! The first request of an anchored session is not a Rebon-shaped Minimal
//! request with fewer tools: it reproduces the DeepSeek Harness **Minimal**
//! preset's model-facing surface exactly — the same complete persona, and the
//! same two tool schemas (`bash` + `str_replace_editor`) — because the upstream
//! measurement (`xiaobright/dsh-anchored-standard`, issue #11) found the tool
//! schema identity, not the tool *count* and not an output cap, to be the
//! decisive first-request variable. Standard-family schemas (`pwsh`/`read`,
//! sandboxed `bash`/`read`) failed to anchor 11/11 runs; this exact pair
//! anchored 5/5 at the adapter-default output budget.
//!
//! The persona text and both tool schemas are copied from DeepSeek Harness
//! (MIT). The notice lives in `runtimes/node/plugins/deepseek-responses/NOTICE`.
//!
//! Execution is Rebon's own: `bash` resolves to `BashTool` through its alias
//! and `str_replace_editor` to the translating
//! [`rebon_tool::StrReplaceEditorTool`].

use rebon_api::Tool as ApiTool;

/// The complete Minimal persona. Upstream declares it `complete: true` with
/// runtime context suppressed, so nothing else may be appended for the whole
/// anchored session — not just the bootstrap request.
pub const ANCHORED_MINIMAL_PERSONA: &str = "You are a helpful software engineer assistant.";

/// The Minimal preset's `bash` description override (not the tool package's
/// own default), verbatim.
pub const ANCHORED_BASH_DESCRIPTION: &str = "Run commands in a bash shell\n\
* When invoking this tool, the contents of the \"command\" parameter does NOT need to be XML-escaped.\n\
* You don't have access to the internet via this tool.\n\
* You do have access to a mirror of common linux and python packages via apt and pip.\n\
* State is persistent across command calls and discussions with the user.\n\
* To inspect a particular line range of a file, e.g. lines 10-25, try 'sed -n 10,25p /path/to/the/file'.\n\
* Please avoid commands that may produce a very large amount of output.\n\
* Please run long lived commands in the background, e.g. 'sleep 10 &' or start a server in the background.";

/// Wire name of the anchored shell tool. Resolves to Rebon's `Bash` through
/// that tool's alias list.
pub const ANCHORED_BASH_TOOL_NAME: &str = "bash";

/// The two tools the bootstrap request advertises, in upstream order.
///
/// The schemas carry no `additionalProperties`: the upstream compiler emits
/// `{type, properties, required}` and nothing else, and the whole point of this
/// projection is that the model sees the same bytes it saw upstream.
pub fn anchored_bootstrap_tools() -> Vec<ApiTool> {
    vec![
        ApiTool {
            name: ANCHORED_BASH_TOOL_NAME.to_string(),
            description: ANCHORED_BASH_DESCRIPTION.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to run. Relative path is preferred in the command."
                    }
                },
                "required": ["command"]
            }),
        },
        ApiTool {
            name: rebon_tool::STR_REPLACE_EDITOR_TOOL_NAME.to_string(),
            description: rebon_tool::STR_REPLACE_EDITOR_DESCRIPTION.to_string(),
            input_schema: rebon_tool::str_replace_editor_input_schema(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_pair_is_the_upstream_minimal_composition() {
        let tools = anchored_bootstrap_tools();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["bash", "str_replace_editor"],
            "the anchor is this exact pair in this order"
        );
        for tool in &tools {
            assert!(
                tool.input_schema.get("additionalProperties").is_none(),
                "{} must not gain additionalProperties",
                tool.name
            );
        }
    }

    #[test]
    fn bash_schema_declares_only_command() {
        let tools = anchored_bootstrap_tools();
        let bash = &tools[0];
        assert_eq!(
            bash.input_schema["required"],
            serde_json::json!(["command"])
        );
        let properties = bash.input_schema["properties"]
            .as_object()
            .expect("properties");
        assert_eq!(properties.len(), 1);
        assert!(bash
            .description
            .starts_with("Run commands in a bash shell\n*"));
    }

    #[test]
    fn persona_is_the_complete_minimal_text() {
        assert_eq!(
            ANCHORED_MINIMAL_PERSONA,
            "You are a helpful software engineer assistant."
        );
    }
}
