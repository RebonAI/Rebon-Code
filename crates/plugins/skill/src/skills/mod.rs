//! The state-machine skill system: what a skill *is*, with no I/O.
//!
//! | Rust module | responsibility |
//! |---|---|
//! | [`skills_menu`] | skills menu projection |
//! | [`bundled`] | bundled skill registry |
//! | [`frontmatter`] | frontmatter parsing |
//! | [`skill_command`] | command model |
//! | [`namespace`] | naming |
//! | [`discovery`] | dynamic discovery |
//! | [`mcp_builders`] | MCP skill builders |
//! | [`bridge`] | integration DTO for [`SkillRegistry`](crate::SkillRegistry) |
//!
//! This was a crate of its own, whose whole point was an empty
//! `[dependencies]` table: pure logic, no `rebon-*` and no external crates,
//! with filesystem I/O and async left to the layer above. Folding it into the
//! plugin that owns skills is what keeps the engine from depending on a crate
//! to build a system prompt — and the purity is worth having on its own:
//! these modules import nothing, and the `the_pure_layer_imports_nothing`
//! canary below fails if one starts to.
//! The I/O layer is [`loader`](crate::loader) and
//! [`bundled_cache`](crate::bundled_cache), one directory up.

// -- modules --

/// Runtime bridge (DTO for `SkillRegistry` integration).
pub mod bridge;
/// Compile-time embedded bundled skill definitions from SKILL.md files.
pub mod built_in;
/// Bundled skill definitions and registry.
pub mod bundled;
/// Dynamic skill discovery, deduplication, conditional activation.
pub mod discovery;
/// Frontmatter parsing types and field validation.
pub mod frontmatter;
/// MCP skill builder trait and default implementation.
pub mod mcp_builders;
/// File-based skill naming and namespace resolution.
pub mod namespace;
/// Full skill command model, builder, and prompt expansion.
pub mod skill_command;
/// Skills menu projection (UI data model).
pub mod skills_menu;

// -- re-exports: skills_menu --

pub use skills_menu::{
    filter_skill_commands, get_source_subtitle, get_source_title, project_skills_menu,
    SkillCommand, SkillGroupProjection, SkillLoadedFrom, SkillMenuInput, SkillMenuProjection,
    SkillRowProjection, SkillSource,
};

// -- re-exports: built_in --

pub use built_in::{built_in_skills, register_built_in_skills};

// -- re-exports: bundled --

pub use bundled::{
    prepend_base_dir, resolve_skill_file_path, BundledSkillDefinition, BundledSkillRegistry,
    ExecutionContext, SkillPathError,
};

// -- re-exports: frontmatter --

pub use frontmatter::{
    coerce_description, extract_description_from_markdown, parse_allowed_tools,
    parse_argument_names, parse_boolean, parse_effort, parse_shell, parse_skill_frontmatter_fields,
    parse_skill_paths, parse_user_specified_model, EffortLevel, EffortValue, FrontmatterShell,
    FrontmatterValue, ParsedSkillFrontmatter, RawFrontmatter,
};

// -- re-exports: skill_command --

pub use skill_command::{
    create_skill_command, estimate_skill_frontmatter_tokens, expand_prompt, parse_arguments,
    rough_token_count_estimation, substitute_arguments, CommandSource, LoadedFrom, PluginInfo,
    SkillCommandDef,
};

// -- re-exports: namespace --

pub use namespace::{
    build_namespace, get_command_name, get_regular_command_name, get_skill_command_name,
    get_skills_path, is_skill_file, transform_skill_files, MarkdownFileEntry, SkillPathDir,
    SkillPathSource,
};

// -- re-exports: discovery --

pub use discovery::{
    activate_conditional_skills, candidate_skill_dirs, deduplicate_by_file_identity,
    skill_matches_paths, split_conditional_skills, DeduplicationResult, SkillWithPath, SplitResult,
};

// -- re-exports: mcp_builders --

pub use mcp_builders::{DefaultMcpSkillBuilder, McpSkillBuilder};

// -- re-exports: bridge --

pub use bridge::{bundled_skill_to_entry, skill_command_to_entry, SkillRegistryEntry};

// ---------------------------------------------------------------------------
// Compatibility canary
// ---------------------------------------------------------------------------

#[cfg(test)]
mod compatibility {
    /// Compatibility canary. As its own crate this layer proved its purity
    /// with an empty `[dependencies]`; as a module inside a plugin that
    /// depends on half the tree, the Cargo file says nothing, so the check
    /// reads the source instead. Anything these modules need from outside is
    /// passed in — that is what keeps skill parsing testable without a
    /// filesystem, a kernel or a registry.
    #[test]
    fn the_pure_layer_imports_nothing() {
        // Split so this file does not trip its own check.
        let banned = concat!("rebon", "_");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/skills");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("the pure skills layer is a directory") {
            let path = entry.expect("readable directory entry").path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("readable module");
            for line in source.lines() {
                assert!(
                    !line.contains(banned),
                    "{} must stay pure logic; found: {line}",
                    path.display()
                );
            }
            checked += 1;
        }
        assert!(checked >= 9, "expected the whole layer, scanned {checked}");
    }
}
