//! Compile-time embedded bundled skill definitions.
//!
//! Each skill lives as a `SKILL.md` file under `skills/bundled/<name>/`.
//! This module embeds them via `include_str!` and provides a function to
//! populate a [`BundledSkillRegistry`] with all built-in skills.

use super::bundled::{BundledSkillDefinition, BundledSkillRegistry};

// ---------------------------------------------------------------------------
// Embedded SKILL.md content
// ---------------------------------------------------------------------------

const SIMPLIFY_MD: &str = include_str!("../../../../../skills/bundled/simplify/SKILL.md");
const BATCH_MD: &str = include_str!("../../../../../skills/bundled/batch/SKILL.md");
const STUCK_MD: &str = include_str!("../../../../../skills/bundled/stuck/SKILL.md");
const REMEMBER_MD: &str = include_str!("../../../../../skills/bundled/remember/SKILL.md");
const SKILLIFY_MD: &str = include_str!("../../../../../skills/bundled/skillify/SKILL.md");
const LOOP_MD: &str = include_str!("../../../../../skills/bundled/loop/SKILL.md");
const INSTALL_MD: &str = include_str!("../../../../../skills/bundled/install/SKILL.md");
// Always on rather than owned by the model-routing plugin: that plugin is off
// by default, and a skill it owned would not exist when the user asks to turn
// routing on in the first place.
const MODEL_ROUTING_MD: &str = include_str!("../../../../../skills/bundled/model-routing/SKILL.md");

// ---------------------------------------------------------------------------
// Frontmatter parsing (minimal, for embedded skills only)
// ---------------------------------------------------------------------------

/// Split a SKILL.md file into its YAML frontmatter block and the
/// markdown body. Returns `("", content)` if no frontmatter is found.
fn split_frontmatter(content: &str) -> (&str, &str) {
    let trimmed = content.trim_start_matches('\u{feff}');
    if !trimmed.starts_with("---") {
        return ("", content);
    }

    let after_opening = &trimmed[3..];
    let after_opening = after_opening.strip_prefix('\n').unwrap_or(after_opening);

    if let Some(pos) = after_opening.find("\n---") {
        let yaml = &after_opening[..pos];
        let rest = &after_opening[pos + 4..];
        let rest = rest.strip_prefix('\n').unwrap_or(rest);
        (yaml, rest)
    } else {
        ("", content)
    }
}

/// Extract a simple `key: value` string from YAML frontmatter.
fn yaml_str<'a>(yaml: &'a str, key: &str) -> Option<&'a str> {
    for line in yaml.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(key) {
            if let Some(value) = rest.strip_prefix(':') {
                let value = value.trim();
                // Strip quotes
                if value.len() >= 2
                    && ((value.starts_with('"') && value.ends_with('"'))
                        || (value.starts_with('\'') && value.ends_with('\'')))
                {
                    return Some(&value[1..value.len() - 1]);
                }
                if value.is_empty() {
                    return None;
                }
                return Some(value);
            }
        }
    }
    None
}

/// Extract a boolean `key: true/false` from YAML frontmatter.
fn yaml_bool(yaml: &str, key: &str) -> Option<bool> {
    yaml_str(yaml, key).map(|v| matches!(v, "true" | "yes"))
}

/// Extract a list `key:\n  - item1\n  - item2` from YAML frontmatter.
fn yaml_list(yaml: &str, key: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut in_list = false;

    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(&format!("{key}:")) {
            in_list = true;
            continue;
        }
        if in_list {
            let indent = line.len() - line.trim_start().len();
            if indent == 0 && !trimmed.is_empty() {
                break;
            }
            if let Some(item) = trimmed.strip_prefix("- ") {
                let item = item.trim();
                // Strip quotes
                let item = if item.len() >= 2
                    && ((item.starts_with('"') && item.ends_with('"'))
                        || (item.starts_with('\'') && item.ends_with('\'')))
                {
                    &item[1..item.len() - 1]
                } else {
                    item
                };
                result.push(item.to_string());
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Skill definition builder
// ---------------------------------------------------------------------------

/// Parse a SKILL.md file into a [`BundledSkillDefinition`].
fn parse_skill_md(content: &str) -> BundledSkillDefinition {
    let (yaml, body) = split_frontmatter(content);

    let name = yaml_str(yaml, "name").unwrap_or("").to_string();
    let description = yaml_str(yaml, "description").unwrap_or("").to_string();
    let when_to_use = yaml_str(yaml, "when_to_use").map(String::from);
    let argument_hint = yaml_str(yaml, "argument-hint").map(String::from);
    let user_invocable = yaml_bool(yaml, "user-invocable").unwrap_or(true);
    let disable_model_invocation = yaml_bool(yaml, "disable-model-invocation").unwrap_or(false);
    let allowed_tools = yaml_list(yaml, "allowed-tools");

    BundledSkillDefinition {
        name,
        description,
        when_to_use,
        argument_hint,
        user_invocable,
        disable_model_invocation,
        allowed_tools,
        prompt_body: body.to_string(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// All built-in skill definitions, parsed from embedded SKILL.md files.
pub fn built_in_skills() -> Vec<BundledSkillDefinition> {
    vec![
        parse_skill_md(SIMPLIFY_MD),
        parse_skill_md(BATCH_MD),
        parse_skill_md(STUCK_MD),
        parse_skill_md(REMEMBER_MD),
        parse_skill_md(SKILLIFY_MD),
        parse_skill_md(LOOP_MD),
        parse_skill_md(INSTALL_MD),
        parse_skill_md(MODEL_ROUTING_MD),
    ]
}

/// Register all built-in skills into the given registry.
pub fn register_built_in_skills(registry: &mut BundledSkillRegistry) {
    for skill in built_in_skills() {
        registry.register(skill);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_embedded_skills_parse_successfully() {
        let skills = built_in_skills();
        assert_eq!(skills.len(), 8);

        for skill in &skills {
            assert!(!skill.name.is_empty(), "skill name must not be empty");
            assert!(
                !skill.description.is_empty(),
                "skill {} has empty description",
                skill.name
            );
            assert!(
                !skill.prompt_body.is_empty(),
                "skill {} has empty prompt_body",
                skill.name
            );
        }
    }

    #[test]
    fn simplify_skill_parses_correctly() {
        let skills = built_in_skills();
        let simplify = skills.iter().find(|s| s.name == "simplify").unwrap();
        assert_eq!(
            simplify.description,
            "Review changed code for reuse, quality, and efficiency, then fix any issues found."
        );
        assert!(simplify.user_invocable);
        assert!(!simplify.disable_model_invocation);
        assert!(simplify.allowed_tools.is_empty());
        assert!(simplify.prompt_body.contains("Phase 1: Identify Changes"));
        assert!(simplify
            .prompt_body
            .contains("Phase 2: Launch Three Review Agents"));
        assert!(simplify.prompt_body.contains("Phase 3: Fix Issues"));
    }

    #[test]
    fn batch_skill_parses_correctly() {
        let skills = built_in_skills();
        let batch = skills.iter().find(|s| s.name == "batch").unwrap();
        assert!(batch.disable_model_invocation);
        assert!(batch.user_invocable);
        assert_eq!(batch.argument_hint.as_deref(), Some("<instruction>"));
        assert!(batch.when_to_use.is_some());
        assert!(batch.prompt_body.contains("Parallel Work Orchestration"));
    }

    #[test]
    fn stuck_skill_parses_correctly() {
        let skills = built_in_skills();
        let stuck = skills.iter().find(|s| s.name == "stuck").unwrap();
        assert!(stuck.user_invocable);
        assert!(stuck.prompt_body.contains("diagnose frozen/slow"));
    }

    #[test]
    fn remember_skill_parses_correctly() {
        let skills = built_in_skills();
        let remember = skills.iter().find(|s| s.name == "remember").unwrap();
        assert!(remember.user_invocable);
        assert!(remember.when_to_use.is_some());
        assert!(remember.prompt_body.contains("Memory Review"));
    }

    #[test]
    fn skillify_skill_parses_correctly() {
        let skills = built_in_skills();
        let skillify = skills.iter().find(|s| s.name == "skillify").unwrap();
        assert!(skillify.disable_model_invocation);
        assert!(skillify.user_invocable);
        assert_eq!(skillify.allowed_tools.len(), 7);
        assert!(skillify.allowed_tools.contains(&"Read".to_string()));
        assert!(skillify
            .allowed_tools
            .contains(&"AskUserQuestion".to_string()));
        assert!(skillify.prompt_body.contains("{{sessionMemory}}"));
    }

    #[test]
    fn register_built_in_populates_registry() {
        let mut registry = BundledSkillRegistry::new();
        register_built_in_skills(&mut registry);
        assert_eq!(registry.len(), 8);
        assert!(registry.get_by_name("simplify").is_some());
        assert!(registry.get_by_name("batch").is_some());
        assert!(registry.get_by_name("stuck").is_some());
        assert!(registry.get_by_name("remember").is_some());
        assert!(registry.get_by_name("skillify").is_some());
        assert!(registry.get_by_name("imagegen").is_none());
        assert!(registry.get_by_name("loop").is_some());
        assert!(registry.get_by_name("install").is_some());
        assert!(registry.get_by_name("model-routing").is_some());
    }

    #[test]
    fn model_routing_skill_parses_correctly() {
        let skills = built_in_skills();
        let routing = skills.iter().find(|s| s.name == "model-routing").unwrap();
        assert!(routing.user_invocable);
        assert!(!routing.disable_model_invocation);
        assert_eq!(
            routing.argument_hint.as_deref(),
            Some("[what should go where, or the routing error you saw]")
        );
        // The trigger words live in the description, the only field the
        // model's skill listing shows.
        for trigger in ["Jev", "TypeSafe", "Vercel AI Gateway", "分流", "自动路由"] {
            assert!(routing.description.contains(trigger), "{trigger}");
        }
        for tool in ["Read", "Edit", "AskUserQuestion"] {
            assert!(routing.allowed_tools.contains(&tool.to_string()), "{tool}");
        }
        // The skill edits settings.json by hand, so it has to carry the checks
        // the /settings rows would have made.
        for key in [
            "backend",
            "routerModel",
            "classifierModel",
            "classifierEndpoint",
            "policy",
            "AI_GATEWAY_API_KEY",
            "TYPESAFE_API_KEY",
            "routerModel must belong to the current provider",
        ] {
            assert!(routing.prompt_body.contains(key), "{key}");
        }
    }

    #[test]
    fn install_skill_parses_correctly() {
        let skills = built_in_skills();
        let install = skills.iter().find(|s| s.name == "install").unwrap();
        assert!(install.user_invocable);
        assert!(!install.disable_model_invocation);
        assert_eq!(
            install.argument_hint.as_deref(),
            Some("[skill source or search terms]")
        );
        assert!(install.allowed_tools.contains(&"WebSearch".to_string()));
        assert!(install.allowed_tools.contains(&"WebFetch".to_string()));
        assert!(install.allowed_tools.contains(&"Bash".to_string()));
        assert!(install.allowed_tools.contains(&"Read".to_string()));
        assert!(install.allowed_tools.contains(&"Write".to_string()));
        assert!(install.allowed_tools.contains(&"Edit".to_string()));
        assert!(install.allowed_tools.contains(&"Glob".to_string()));
        assert!(install
            .prompt_body
            .contains("~/.rebon/skills/<skill-name>/SKILL.md"));
        assert!(install.prompt_body.contains(".claude/skills"));
        assert!(install.prompt_body.contains(".codex/skills"));
    }

    #[test]
    fn loop_skill_parses_correctly() {
        let skills = built_in_skills();
        let loop_skill = skills.iter().find(|s| s.name == "loop").unwrap();
        assert!(loop_skill.user_invocable);
        assert_eq!(
            loop_skill.argument_hint.as_deref(),
            Some("[interval] <prompt>")
        );
        assert!(loop_skill.prompt_body.contains("CronCreate"));
        assert!(loop_skill
            .prompt_body
            .contains("ToolSearch` with query `select:CronCreate"));
        assert!(loop_skill.prompt_body.contains("immediately execute"));
    }

    #[test]
    fn split_frontmatter_works() {
        let (yaml, body) = split_frontmatter("---\nname: test\n---\n# Body\n");
        assert!(yaml.contains("name: test"));
        assert_eq!(body, "# Body\n");
    }

    #[test]
    fn split_frontmatter_no_yaml() {
        let (yaml, body) = split_frontmatter("# Just body");
        assert!(yaml.is_empty());
        assert_eq!(body, "# Just body");
    }

    #[test]
    fn yaml_str_extraction() {
        let yaml = "name: My Skill\ndescription: Does things";
        assert_eq!(yaml_str(yaml, "name"), Some("My Skill"));
        assert_eq!(yaml_str(yaml, "description"), Some("Does things"));
        assert_eq!(yaml_str(yaml, "missing"), None);
    }

    #[test]
    fn yaml_str_quoted() {
        let yaml = "description: \"Quoted value\"";
        assert_eq!(yaml_str(yaml, "description"), Some("Quoted value"));
    }

    #[test]
    fn yaml_list_extraction() {
        let yaml = "allowed-tools:\n  - Read\n  - Write\n  - \"Bash(mkdir:*)\"";
        let tools = yaml_list(yaml, "allowed-tools");
        assert_eq!(tools, vec!["Read", "Write", "Bash(mkdir:*)"]);
    }

    #[test]
    fn yaml_bool_extraction() {
        let yaml = "user-invocable: true\ndisable-model-invocation: false";
        assert_eq!(yaml_bool(yaml, "user-invocable"), Some(true));
        assert_eq!(yaml_bool(yaml, "disable-model-invocation"), Some(false));
    }
}
