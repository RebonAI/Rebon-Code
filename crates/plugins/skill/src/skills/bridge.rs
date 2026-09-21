//! Bridge from this layer's skill definitions to a runtime-agnostic
//! registry representation.
//!
//! This module provides the conversion layer the loader one directory up uses
//! to register [`SkillCommandDef`]s into the crate's `SkillRegistry`.
//!
//! Because this layer depends on nothing -- the registry included -- the
//! bridge works through a plain-data DTO ([`SkillRegistryEntry`]) that the
//! caller maps into the registry's own `Skill`.

use super::bundled::BundledSkillDefinition;
use super::skill_command::SkillCommandDef;

// ---------------------------------------------------------------------------
// SkillRegistryEntry — plain-data DTO
// ---------------------------------------------------------------------------

/// A plain-data snapshot that carries everything needed to
/// register a skill with the runtime's `SkillRegistry`.
///
/// The loader converts this into the crate's `Skill` and calls
/// `SkillRegistry::register()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRegistryEntry {
    /// Unique skill id (matches `SkillCommandDef::name`).
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Short description.
    pub description: String,
    /// Expanded prompt template body.
    pub prompt_template: String,
    /// Suggested follow-up tools.
    pub suggested_tools: Vec<String>,
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

/// Convert a [`SkillCommandDef`] into a [`SkillRegistryEntry`]
/// ready for runtime registration.
///
/// `session_id` is passed through for `${CLAUDE_SESSION_ID}`
/// replacement during prompt expansion.
pub fn skill_command_to_entry(cmd: &SkillCommandDef, session_id: &str) -> SkillRegistryEntry {
    let prompt = super::skill_command::expand_prompt(cmd, None, session_id);
    SkillRegistryEntry {
        id: cmd.name.clone(),
        title: cmd.user_facing_name().to_string(),
        description: cmd.description.clone(),
        prompt_template: prompt,
        suggested_tools: cmd.allowed_tools.clone(),
    }
}

/// Convert a [`BundledSkillDefinition`] into a
/// [`SkillRegistryEntry`].
pub fn bundled_skill_to_entry(def: &BundledSkillDefinition) -> SkillRegistryEntry {
    SkillRegistryEntry {
        id: def.name.clone(),
        title: def.name.clone(),
        description: def.description.clone(),
        prompt_template: def.prompt_body.clone(),
        suggested_tools: def.allowed_tools.clone(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::skill_command::{CommandSource, LoadedFrom};

    #[test]
    fn skill_command_to_entry_uses_display_name() {
        let cmd = SkillCommandDef {
            name: "my-skill".into(),
            display_name: Some("Pretty Name".into()),
            description: "A skill".into(),
            has_user_specified_description: true,
            allowed_tools: vec!["Read".into()],
            required_tools: Vec::new(),
            argument_hint: None,
            arg_names: Vec::new(),
            when_to_use: None,
            version: None,
            model: None,
            disable_model_invocation: false,
            user_invocable: true,
            context: None,
            agent: None,
            effort: None,
            paths: None,
            content_length: 10,
            is_hidden: false,
            source: CommandSource::ProjectSettings,
            loaded_from: LoadedFrom::Skills,
            skill_root: None,
            shell: None,
            hooks: None,
            disable_non_interactive: false,
            plugin_info: None,
            progress_message: "running".into(),
            markdown_content: "Do the thing.".into(),
        };

        let entry = skill_command_to_entry(&cmd, "sess-1");
        assert_eq!(entry.id, "my-skill");
        assert_eq!(entry.title, "Pretty Name");
        assert_eq!(entry.description, "A skill");
        assert_eq!(entry.prompt_template, "Do the thing.");
        assert_eq!(entry.suggested_tools, vec!["Read"]);
    }

    #[test]
    fn bundled_skill_to_entry_basic() {
        let def = BundledSkillDefinition {
            name: "simplify".into(),
            description: "Code review".into(),
            allowed_tools: vec!["Bash".into(), "Read".into()],
            prompt_body: "Review changed code.".into(),
            ..Default::default()
        };
        let entry = bundled_skill_to_entry(&def);
        assert_eq!(entry.id, "simplify");
        assert_eq!(entry.prompt_template, "Review changed code.");
    }
}
