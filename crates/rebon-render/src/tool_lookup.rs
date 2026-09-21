//! Look up the tool behind a tool-use id.

use std::collections::HashMap;

/// Minimal tool descriptor used for pure lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupTool {
    /// Tool name.
    pub name: String,
}

/// Minimal `tool_use` block shape keyed by tool-use ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupToolUse {
    /// Tool-use ID.
    pub id: String,
    /// Tool name referenced by the block.
    pub name: String,
}

/// Lookup tables built once per message batch.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolMessageLookups {
    /// Tool-use blocks keyed by tool-use id.
    pub tool_use_by_tool_use_id: HashMap<String, LookupToolUse>,
}

/// A resolved tool together with its tool-use block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFromMessages {
    /// Matching tool.
    pub tool: LookupTool,
    /// Matching tool-use block.
    pub tool_use: LookupToolUse,
}

/// Resolve the tool and tool-use block behind an id.
pub fn get_tool_from_messages(
    tool_use_id: &str,
    tools: &[LookupTool],
    lookups: &ToolMessageLookups,
) -> Option<ToolFromMessages> {
    let tool_use = lookups.tool_use_by_tool_use_id.get(tool_use_id)?.clone();
    let tool = tools
        .iter()
        .find(|tool| tool.name == tool_use.name)?
        .clone();
    Some(ToolFromMessages { tool, tool_use })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_tool_and_tool_use_when_both_exist() {
        let lookups = ToolMessageLookups {
            tool_use_by_tool_use_id: HashMap::from([(
                "u1".into(),
                LookupToolUse {
                    id: "u1".into(),
                    name: "Read".into(),
                },
            )]),
        };
        let tools = vec![
            LookupTool {
                name: "Write".into(),
            },
            LookupTool {
                name: "Read".into(),
            },
        ];

        let resolved = get_tool_from_messages("u1", &tools, &lookups).unwrap();
        assert_eq!(resolved.tool.name, "Read");
        assert_eq!(resolved.tool_use.id, "u1");
    }

    #[test]
    fn returns_none_when_tool_use_or_tool_is_missing() {
        let lookups = ToolMessageLookups::default();
        let tools = vec![LookupTool {
            name: "Read".into(),
        }];
        assert_eq!(get_tool_from_messages("u1", &tools, &lookups), None);

        let lookups = ToolMessageLookups {
            tool_use_by_tool_use_id: HashMap::from([(
                "u1".into(),
                LookupToolUse {
                    id: "u1".into(),
                    name: "Write".into(),
                },
            )]),
        };
        assert_eq!(get_tool_from_messages("u1", &tools, &lookups), None);
    }
}
