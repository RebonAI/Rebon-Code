//! MCP skill builder type bridge.
//!
//! The Rust module tree is a DAG by construction, so no write-once
//! registry is needed to break a circular import. This module provides the
//! **marker types** and a builder trait so the MCP layer can
//! create skill commands without depending on internals.

use super::frontmatter::{ParsedSkillFrontmatter, RawFrontmatter};
use super::skill_command::{CommandSource, LoadedFrom, SkillCommandDef};

/// Trait that the MCP layer uses to create skill commands from
/// parsed frontmatter.
///
/// The canonical implementation delegates to
/// [`super::frontmatter::parse_skill_frontmatter_fields`] and
/// [`super::skill_command::create_skill_command`].
///
/// This trait exists so an MCP server that supplies skills can reach this
/// pure layer without pulling in the full loading/discovery machinery.
pub trait McpSkillBuilder: Send + Sync {
    /// Parse raw YAML frontmatter into typed fields.
    fn parse_frontmatter(
        &self,
        frontmatter: &RawFrontmatter,
        markdown_content: &str,
        resolved_name: &str,
    ) -> ParsedSkillFrontmatter;

    /// Create a full skill command definition from parsed data.
    #[allow(clippy::too_many_arguments)]
    fn create_command(
        &self,
        skill_name: &str,
        parsed: &ParsedSkillFrontmatter,
        markdown_content: &str,
        source: CommandSource,
        loaded_from: LoadedFrom,
        base_dir: Option<&str>,
        paths: Option<Vec<String>>,
    ) -> SkillCommandDef;
}

/// Default builder that delegates to the crate's own functions.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultMcpSkillBuilder;

impl McpSkillBuilder for DefaultMcpSkillBuilder {
    fn parse_frontmatter(
        &self,
        frontmatter: &RawFrontmatter,
        markdown_content: &str,
        resolved_name: &str,
    ) -> ParsedSkillFrontmatter {
        super::frontmatter::parse_skill_frontmatter_fields(
            frontmatter,
            markdown_content,
            resolved_name,
            "Skill",
        )
    }

    fn create_command(
        &self,
        skill_name: &str,
        parsed: &ParsedSkillFrontmatter,
        markdown_content: &str,
        source: CommandSource,
        loaded_from: LoadedFrom,
        base_dir: Option<&str>,
        paths: Option<Vec<String>>,
    ) -> SkillCommandDef {
        super::skill_command::create_skill_command(
            skill_name,
            parsed,
            markdown_content,
            source,
            loaded_from,
            base_dir,
            paths,
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::frontmatter::FrontmatterValue;
    use std::collections::HashMap;

    #[test]
    fn default_builder_round_trips() {
        let builder = DefaultMcpSkillBuilder;
        let mut fm = HashMap::new();
        fm.insert(
            "description".to_string(),
            FrontmatterValue::String("An MCP skill".into()),
        );

        let parsed = builder.parse_frontmatter(&fm, "# body", "mcp-skill");
        assert_eq!(parsed.description, "An MCP skill");

        let cmd = builder.create_command(
            "mcp-skill",
            &parsed,
            "# body",
            CommandSource::Mcp,
            LoadedFrom::Mcp,
            None,
            None,
        );
        assert_eq!(cmd.name, "mcp-skill");
        assert_eq!(cmd.source, CommandSource::Mcp);
        assert_eq!(cmd.loaded_from, LoadedFrom::Mcp);
    }
}
