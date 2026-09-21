//! Tool-activity projection.
//!
//! The projection turns a tool's name and raw input into the
//! `name(args)` label the transcript shows. The tool registry is
//! modelled as a small trait the consumer wires up; the trait returns
//! the projected display string already, so renderer primitives stay
//! outside this crate.

/// Tool activity event input. This model only reads `tool_name` and
/// `input_raw`.
#[derive(Debug, Clone)]
pub struct ToolActivity {
    /// The tool name as recorded by the runtime (e.g. `"Bash"`,
    /// `"Read"`).
    pub tool_name: String,
    /// Raw JSON-like input. Modeled as a string so the crate doesn't
    /// pull in `serde_json`. The consumer's `ToolRegistry` impl is
    /// expected to do its own parsing.
    pub input_raw: String,
}

/// Outbound seam: project a `(tool_name, input)` pair into a final
/// user-facing label.
///
/// The contract, in order:
///
/// 1. Look the tool up by name → if it is missing, return `None`.
/// 2. Parse the raw input against the tool's input schema → if that
/// fails, treat the input as empty.
/// 3. Derive the tool's user-facing name → if it is missing or empty,
/// return `None`.
/// 4. Derive the tool's arguments → when they render non-empty the
/// label is `name(args)`.
///
/// Implementations should return `None` for any of: tool not found,
/// schema parse error, empty user-facing name. The fallback to
/// `tool_name` happens in [`render_tool_activity`].
pub trait ToolRegistry {
    /// Project the activity to a (label, optional args) pair.
    /// `Some((name, Some(args)))` means render `name(args)`,
    /// `Some((name, None))` means render `name`, and `None` means the
    /// caller should fall back to `activity.tool_name`.
    fn project(&self, activity: &ToolActivity) -> Option<(String, Option<String>)>;
}

/// Render a tool activity as a single display string: `name(args)`
/// when both parts are non-empty, the bare `name` when only the name
/// is, and `tool_name` otherwise.
pub fn render_tool_activity<R: ToolRegistry + ?Sized>(
    activity: &ToolActivity,
    registry: &R,
) -> String {
    match registry.project(activity) {
        Some((name, Some(args))) if !name.is_empty() && !args.is_empty() => {
            format!("{name}({args})")
        }
        Some((name, _)) if !name.is_empty() => name,
        _ => activity.tool_name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticRegistry {
        responses: std::collections::HashMap<String, Option<(String, Option<String>)>>,
    }

    impl StaticRegistry {
        fn new() -> Self {
            Self {
                responses: std::collections::HashMap::new(),
            }
        }

        fn with(mut self, name: &str, value: Option<(String, Option<String>)>) -> Self {
            self.responses.insert(name.into(), value);
            self
        }
    }

    impl ToolRegistry for StaticRegistry {
        fn project(&self, activity: &ToolActivity) -> Option<(String, Option<String>)> {
            self.responses.get(&activity.tool_name).cloned().flatten()
        }
    }

    fn act(name: &str) -> ToolActivity {
        ToolActivity {
            tool_name: name.into(),
            input_raw: "{}".into(),
        }
    }

    #[test]
    fn unknown_tool_returns_tool_name() {
        let r = StaticRegistry::new();
        assert_eq!(render_tool_activity(&act("Bash"), &r), "Bash");
    }

    #[test]
    fn registry_returns_none_falls_back() {
        // Registry explicitly maps the name to None
        let r = StaticRegistry::new().with("Bash", None);
        assert_eq!(render_tool_activity(&act("Bash"), &r), "Bash");
    }

    #[test]
    fn name_only_returned_as_is() {
        let r = StaticRegistry::new().with("Read", Some(("Read file".into(), None)));
        assert_eq!(render_tool_activity(&act("Read"), &r), "Read file");
    }

    #[test]
    fn name_with_args_renders_paren_form() {
        let r = StaticRegistry::new().with("Bash", Some(("Bash".into(), Some("ls -la".into()))));
        assert_eq!(render_tool_activity(&act("Bash"), &r), "Bash(ls -la)");
    }

    #[test]
    fn empty_args_uses_name_only_form() {
        // An empty arguments string takes the name-only form.
        let r = StaticRegistry::new().with("Bash", Some(("Bash".into(), Some("".into()))));
        assert_eq!(render_tool_activity(&act("Bash"), &r), "Bash");
    }

    #[test]
    fn empty_name_falls_back_to_tool_name() {
        // An empty user-facing name falls back to the tool name.
        let r = StaticRegistry::new().with("Bash", Some(("".into(), Some("ls".into()))));
        assert_eq!(render_tool_activity(&act("Bash"), &r), "Bash");
    }
}
