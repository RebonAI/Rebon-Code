//! What this session can invoke, answered from the live index.
//!
//! The engine asks the catalogue two questions and this answers both from one
//! [`SkillRegistry`]: which skills the `skill_listing` attachment should name,
//! and whether a `/<name> args` prompt nobody resolved upstream is an
//! invocation. Resolved per call rather than snapshotted at bind time, because
//! progressive discovery grows the index mid-turn and a skill found under a
//! path a tool just touched has to be nameable on the next round.

use std::sync::Arc;

use rebon_agent_core::SkillInvocationRequest;
use rebon_core::attachment_seat::{AvailableSkill, TurnSkillCatalog};

use crate::skill::SkillRegistry;
use crate::skills::skill_command::parse_user_skill_invocation;

/// The session's loaded index, read as a catalogue.
pub struct RegistrySkillCatalog {
    registry: Arc<SkillRegistry>,
}

impl RegistrySkillCatalog {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }
}

impl TurnSkillCatalog for RegistrySkillCatalog {
    fn available_skills(&self) -> Vec<AvailableSkill> {
        self.registry
            .entries()
            .into_iter()
            .map(|skill| {
                AvailableSkill::new(skill.id, skill.description)
                    .with_required_tools(skill.required_tools)
            })
            .collect()
    }

    fn typed_invocation(&self, user_text: &str) -> Option<SkillInvocationRequest> {
        let (skill, args) = parse_user_skill_invocation(user_text)?;
        // `get` answers `None` for a disabled skill, so a name the user turned
        // off reaches the model as ordinary text rather than as a call that
        // would fail.
        let definition = self.registry.get(&skill)?;
        if !definition.user_invocable {
            return None;
        }
        Some(SkillInvocationRequest { skill, args })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{Skill, SkillSource};

    fn registry_with(id: &str, user_invocable: bool) -> Arc<SkillRegistry> {
        let registry = SkillRegistry::new();
        registry.register(Skill {
            id: id.into(),
            title: id.into(),
            description: format!("{id} description"),
            prompt_template: String::new(),
            suggested_tools: Vec::new(),
            source: SkillSource::Project,
            argument_hint: None,
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable,
            disable_model_invocation: false,
            required_tools: Vec::new(),
        });
        Arc::new(registry)
    }

    #[test]
    fn the_listing_names_every_enabled_skill_with_its_description() {
        let catalog = RegistrySkillCatalog::new(registry_with("commit", true));
        assert_eq!(
            catalog.available_skills(),
            vec![AvailableSkill::new("commit", "commit description")]
        );
    }

    #[test]
    fn a_typed_slash_command_resolves_to_an_invocation_with_its_arguments() {
        let catalog = RegistrySkillCatalog::new(registry_with("commit", true));
        let request = catalog
            .typed_invocation("/commit -m fix")
            .expect("a registered user-invocable skill resolves");
        assert_eq!(request.skill, "commit");
        assert_eq!(request.args.as_deref(), Some("-m fix"));
    }

    #[test]
    fn an_unregistered_name_stays_ordinary_text() {
        let catalog = RegistrySkillCatalog::new(registry_with("commit", true));
        assert!(catalog.typed_invocation("/help how do I commit").is_none());
    }

    #[test]
    fn a_disabled_skill_does_not_resolve_and_is_not_listed() {
        let registry = registry_with("commit", true);
        registry.set_disabled_skills(["commit"]);
        let catalog = RegistrySkillCatalog::new(registry);
        assert!(catalog.available_skills().is_empty());
        assert!(catalog.typed_invocation("/commit").is_none());
    }

    /// A model-only skill is listed for the model and refused to the user:
    /// the listing is what tells the model it exists, and the resolver is
    /// what keeps a person from calling it by typing its name.
    #[test]
    fn a_model_only_skill_is_listed_but_not_user_invocable() {
        let catalog = RegistrySkillCatalog::new(registry_with("internal", false));
        assert_eq!(catalog.available_skills().len(), 1);
        assert!(catalog.typed_invocation("/internal").is_none());
    }
}
