//! Capabilities section — advertised MCP capabilities.
//!
//! The builder takes three counts (tools, resources, prompts) and
//! builds a list of capability labels in a fixed order:
//!
//! Capabilities are included when their corresponding count is greater
//! than zero, in this fixed order: tools, resources, then prompts.
//!
//! Note the order: **tools, resources, prompts**. Pinning
//! this in the test table prevents a silent shuffle.
//!
//! If the final list is empty the display line uses the literal
//! string `none`; otherwise the labels are joined with `", "`.

/// A single advertised MCP capability. Variants are pinned in the
/// order [`build_capabilities`] pushes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// The server exposes one or more tools (`tools/call` support).
    Tools,
    /// The server exposes one or more resources (`resources/read` support).
    Resources,
    /// The server exposes one or more prompts (`prompts/get` support).
    Prompts,
}

impl Capability {
    /// The lowercase label used for the capability.
    pub fn label(&self) -> &'static str {
        match self {
            Capability::Tools => "tools",
            Capability::Resources => "resources",
            Capability::Prompts => "prompts",
        }
    }
}

/// The fallback string used when no capability is
/// advertised. Pinned by copy tests.
pub const CAPABILITIES_EMPTY_LABEL: &str = "none";

/// Build the capabilities list for a server given its tool / resource
/// / prompt counts. Only counts greater than zero contribute a label.
///
/// Returns a `Vec` in the exact order this function pushes them:
/// **tools → resources → prompts**. Consumers that want a different
/// order (e.g. alphabetical) must reorder downstream; this crate's
/// contract is to preserve the push order exactly.
pub fn build_capabilities(
    server_tools_count: usize,
    server_resources_count: usize,
    server_prompts_count: usize,
) -> Vec<Capability> {
    let mut out = Vec::new();
    if server_tools_count > 0 {
        out.push(Capability::Tools);
    }
    if server_resources_count > 0 {
        out.push(Capability::Resources);
    }
    if server_prompts_count > 0 {
        out.push(Capability::Prompts);
    }
    out
}

/// Build the final one-line display:
/// `Capabilities: <comma-joined-labels>` or `Capabilities: none`.
///
/// The join uses `", "` (comma + space). A consumer that wants a
/// different separator should call [`build_capabilities`] directly and
/// format the labels itself.
pub fn build_capabilities_line(
    server_tools_count: usize,
    server_resources_count: usize,
    server_prompts_count: usize,
) -> String {
    let caps = build_capabilities(
        server_tools_count,
        server_resources_count,
        server_prompts_count,
    );
    if caps.is_empty() {
        return format!("Capabilities: {CAPABILITIES_EMPTY_LABEL}");
    }
    let joined: Vec<&str> = caps.iter().map(|c| c.label()).collect();
    format!("Capabilities: {}", joined.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- build_capabilities (pure builder) ---

    #[test]
    fn all_zero_returns_empty() {
        assert!(build_capabilities(0, 0, 0).is_empty());
    }

    #[test]
    fn only_tools() {
        assert_eq!(build_capabilities(1, 0, 0), vec![Capability::Tools]);
    }

    #[test]
    fn only_resources() {
        assert_eq!(build_capabilities(0, 1, 0), vec![Capability::Resources]);
    }

    #[test]
    fn only_prompts() {
        assert_eq!(build_capabilities(0, 0, 1), vec![Capability::Prompts]);
    }

    #[test]
    fn push_order_is_tools_resources_prompts_not_declaration_order() {
        // This is the critical pin: the push order is
        // tools / resources / prompts. Any refactor that reorders the
        // pushes would shuffle the display.
        let all = build_capabilities(1, 1, 1);
        assert_eq!(
            all,
            vec![
                Capability::Tools,
                Capability::Resources,
                Capability::Prompts
            ],
        );
    }

    #[test]
    fn tools_and_prompts_no_resources() {
        // Cross-check: the middle slot is the per-feature gate, not
        // a fixed position.
        assert_eq!(
            build_capabilities(5, 0, 3),
            vec![Capability::Tools, Capability::Prompts],
        );
    }

    #[test]
    fn large_counts_treated_as_non_zero() {
        // The comparison is `> 0`, so any positive count is equivalent.
        assert_eq!(
            build_capabilities(usize::MAX, 1_000_000, 1),
            vec![
                Capability::Tools,
                Capability::Resources,
                Capability::Prompts
            ],
        );
    }

    // --- build_capabilities_line (the display string) ---

    #[test]
    fn line_empty_is_none_label() {
        assert_eq!(build_capabilities_line(0, 0, 0), "Capabilities: none");
    }

    #[test]
    fn line_all_three_joined_with_comma_space() {
        assert_eq!(
            build_capabilities_line(1, 1, 1),
            "Capabilities: tools, resources, prompts",
        );
    }

    #[test]
    fn line_tools_only() {
        assert_eq!(build_capabilities_line(1, 0, 0), "Capabilities: tools");
    }

    #[test]
    fn line_prompts_only() {
        assert_eq!(build_capabilities_line(0, 0, 1), "Capabilities: prompts");
    }

    #[test]
    fn capability_label_round_trip() {
        assert_eq!(Capability::Tools.label(), "tools");
        assert_eq!(Capability::Resources.label(), "resources");
        assert_eq!(Capability::Prompts.label(), "prompts");
    }

    #[test]
    fn none_label_constant_pin() {
        // A refactor that localizes "none" would silently change the
        // output string; pinning here makes a change visible.
        assert_eq!(CAPABILITIES_EMPTY_LABEL, "none");
    }
}
