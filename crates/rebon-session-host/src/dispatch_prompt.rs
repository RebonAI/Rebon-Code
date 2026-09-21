use std::path::{Path, PathBuf};

use super::BackgroundDispatchPrompt;

pub fn resolve_background_dispatch_prompt_with_skills(
    raw_prompt: &str,
    cwd: &Path,
    skills: &[String],
    config_home_dir: &Path,
) -> BackgroundDispatchPrompt {
    let registry = rebon_tool::AgentRegistry::load(cwd, config_home_dir);
    resolve_background_dispatch_prompt_with_registry_and_skills(raw_prompt, cwd, &registry, skills)
}

#[cfg(test)]
fn resolve_background_dispatch_prompt_with_registry(
    raw_prompt: &str,
    registry: &rebon_tool::AgentRegistry,
) -> BackgroundDispatchPrompt {
    resolve_background_dispatch_prompt_with_registry_and_skills(
        raw_prompt,
        Path::new("."),
        registry,
        &[],
    )
}

fn resolve_background_dispatch_prompt_with_registry_and_skills(
    raw_prompt: &str,
    cwd: &Path,
    registry: &rebon_tool::AgentRegistry,
    skills: &[String],
) -> BackgroundDispatchPrompt {
    let trimmed = raw_prompt.trim();
    let mut skill_name = None;
    let after_skill = strip_dispatch_skill_token(trimmed, skills, &mut skill_name);
    let mut parts = after_skill.splitn(2, char::is_whitespace);
    if let Some(first) = parts.next() {
        if !first.starts_with('@') {
            if let Some(def) = registry.resolve(first) {
                return BackgroundDispatchPrompt {
                    prompt: skill_dispatch_prompt(
                        skill_name.as_deref(),
                        parts.next().unwrap_or_default().trim(),
                    ),
                    agent_type: Some(def.agent_type.clone()),
                    cwd: None,
                    skill_name,
                };
            }
        }
    }

    let mut resolved_agent = None;
    let mut resolved_cwd = None;
    let prompt = after_skill
        .split_whitespace()
        .filter(|token| {
            if let Some(name) = token.strip_prefix('@') {
                if resolved_agent.is_none() {
                    if let Some(def) = registry.resolve(name) {
                        resolved_agent = Some(def.agent_type.clone());
                        return false;
                    }
                }
                if resolved_cwd.is_none() {
                    if let Some(repo) = resolve_sibling_repo(cwd, name) {
                        resolved_cwd = Some(repo);
                        return false;
                    }
                }
            }
            true
        })
        .collect::<Vec<_>>()
        .join(" ");

    BackgroundDispatchPrompt {
        prompt: skill_dispatch_prompt(skill_name.as_deref(), &prompt),
        agent_type: resolved_agent,
        cwd: resolved_cwd,
        skill_name,
    }
}

fn skill_dispatch_prompt(skill_name: Option<&str>, prompt: &str) -> String {
    match skill_name {
        Some(skill) if prompt.trim().is_empty() => format!("/{skill}"),
        Some(skill) => format!("/{skill} {}", prompt.trim()),
        None => prompt.to_string(),
    }
}

fn strip_dispatch_skill_token<'a>(
    prompt: &'a str,
    skills: &[String],
    skill_name: &mut Option<String>,
) -> &'a str {
    let trimmed = prompt.trim_start();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return prompt;
    };
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let candidate = &rest[..end];
    if candidate.is_empty() {
        return prompt;
    }
    if skills.iter().any(|skill| skill == candidate) {
        *skill_name = Some(candidate.to_string());
        return rest[end..].trim_start();
    }
    prompt
}

fn resolve_sibling_repo(cwd: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return None;
    }
    let candidate = cwd.join(name);
    if !candidate.is_dir() || !candidate.join(".git").exists() {
        return None;
    }
    Some(
        candidate
            .canonicalize()
            .map(rebon_tools_core::strip_windows_verbatim_prefix)
            .unwrap_or(candidate),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_prompt_extracts_bare_or_at_agent_prefix() {
        let registry = rebon_tool::AgentRegistry::from_groups(rebon_tool::AgentGroups {
            user: vec![rebon_tool::ResolvedAgentDef {
                agent_type: "reviewer".to_string(),
                when_to_use: "review code".to_string(),
                system_prompt: "review".to_string(),
                tool_filter: rebon_tool::ToolFilter::unrestricted(),
                model: None,
                model_profile: None,
                provider: None,
                effort: None,
                background: false,
                isolation: None,
                memory: None,
                permission_mode: None,
                runtime: rebon_tool::AgentRuntime::Local,
                source: rebon_tool::AgentSource::Settings(rebon_tool::SettingSource::UserSettings),
                file_stem: None,
            }],
            ..Default::default()
        });

        assert_eq!(
            resolve_background_dispatch_prompt_with_registry("reviewer inspect diff", &registry),
            BackgroundDispatchPrompt {
                prompt: "inspect diff".into(),
                agent_type: Some("reviewer".into()),
                cwd: None,
                skill_name: None,
            }
        );
        assert_eq!(
            resolve_background_dispatch_prompt_with_registry_and_skills(
                "/simplify reviewer inspect diff",
                Path::new("."),
                &registry,
                &["simplify".to_string()]
            ),
            BackgroundDispatchPrompt {
                prompt: "/simplify inspect diff".into(),
                agent_type: Some("reviewer".into()),
                cwd: None,
                skill_name: Some("simplify".into()),
            }
        );
    }

    #[test]
    fn dispatch_prompt_extracts_sibling_repo_after_agents_take_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo-a");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let registry = rebon_tool::AgentRegistry::from_groups(rebon_tool::AgentGroups {
            user: vec![rebon_tool::ResolvedAgentDef {
                agent_type: "repo-b".to_string(),
                when_to_use: "review code".to_string(),
                system_prompt: "review".to_string(),
                tool_filter: rebon_tool::ToolFilter::unrestricted(),
                model: None,
                model_profile: None,
                provider: None,
                effort: None,
                background: false,
                isolation: None,
                memory: None,
                permission_mode: None,
                runtime: rebon_tool::AgentRuntime::Local,
                source: rebon_tool::AgentSource::Settings(rebon_tool::SettingSource::UserSettings),
                file_stem: None,
            }],
            ..Default::default()
        });

        let repo_dispatch = resolve_background_dispatch_prompt_with_registry_and_skills(
            "fix @repo-a bug",
            dir.path(),
            &registry,
            &[],
        );
        let expected_repo =
            rebon_tools_core::strip_windows_verbatim_prefix(repo.canonicalize().unwrap());
        assert_eq!(repo_dispatch.prompt, "fix bug");
        assert_eq!(repo_dispatch.cwd.as_ref(), Some(&expected_repo));
        assert_eq!(repo_dispatch.agent_type, None);

        let agent_dispatch = resolve_background_dispatch_prompt_with_registry_and_skills(
            "fix @repo-b bug",
            dir.path(),
            &registry,
            &[],
        );
        assert_eq!(agent_dispatch.prompt, "fix bug");
        assert_eq!(agent_dispatch.agent_type.as_deref(), Some("repo-b"));
        assert_eq!(agent_dispatch.cwd, None);
    }
}
