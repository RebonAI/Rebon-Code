//! The prose a profile surface prints.
//!
//! Plain strings, no styling: a terminal wraps them in its own transcript, a
//! desktop window puts them in a card. Keeping them here is what stops the two
//! from describing the same profile differently.

use std::path::Path;

use rebon_config::profile_store::{self, Profile};

use crate::apply::ProfileSession;

pub fn usage_text() -> String {
    [
        "Usage:",
        "  /profile                             List saved profiles",
        "  /profile <name>                      Switch to a profile",
        "  /profile show <name>                 Show what a profile declares",
        "  /profile save <name> [note]          Save this session's setup as a profile",
        "  /profile set <name> <field> <value>  Write one field of one profile",
        "  /profile remove <name>               Delete a profile",
        "  /profile reset                       Put the tool surface back to the default",
        "",
        "A profile bundles provider, model, agent backend, permission mode and tool surface.",
        "It only changes what it declares — anything it leaves out is left exactly as it is.",
        "`save` captures all five off this session; `set` is how a profile comes to declare",
        "only the one or two that matter.",
    ]
    .join("\n")
}

pub fn set_usage_text() -> String {
    [
        "Usage:",
        "  /profile set <name> <field> <value>   Declare it",
        "  /profile set <name> <field> -         Stop declaring it",
        "",
        "Fields: provider, model, agent, permissionMode, tools, description, displayName",
        "Clearing: -, follow, inherit, clear, unset  (not `default` — that is a real mode)",
        "",
        "  /profile set writing model vendor-flash",
        "  /profile set writing tools Read,Edit,Grep      keep only these",
        "  /profile set writing tools -Bash               keep everything but this",
        "  /profile set writing agent -                   leave the agent alone on switch",
        "",
        "Naming a profile that does not exist creates it, declaring only that one field.",
    ]
    .join("\n")
}

pub fn render_list(config_dir: &Path) -> String {
    let profiles = profile_store::list(config_dir);
    if profiles.is_empty() {
        return format!(
            "No profiles saved.\n\nSave this session's provider, model, agent, permission mode and tool surface under a name:\n  /profile save writing\n\n{}",
            usage_text()
        );
    }
    let mut lines = vec!["Profiles:".to_string(), String::new()];
    for profile in &profiles {
        lines.push(format!("  {}", profile.id));
        if let Some(description) = profile.description.as_deref().filter(|d| !d.is_empty()) {
            lines.push(format!("    {description}"));
        }
        for line in declared_lines(profile) {
            lines.push(format!("  {line}"));
        }
        lines.push(String::new());
    }
    lines.push("/profile <name> switches; /profile show <name> shows one in full.".into());
    lines.join("\n")
}

pub fn render_profile(config_dir: &Path, profile: &Profile) -> String {
    let mut lines = vec![format!("Profile \"{}\" ({})", profile.label(), profile.id)];
    if let Some(description) = profile.description.as_deref().filter(|d| !d.is_empty()) {
        lines.push(format!("  {description}"));
    }
    lines.push(String::new());
    lines.extend(declared_lines(profile));
    lines.push(String::new());
    lines.push(unchanged_note(profile).trim_start().to_string());

    let issues = profile_store::validate(config_dir, profile);
    if !issues.is_empty() {
        lines.push(String::new());
        lines.push("Will not apply as it stands:".into());
        for issue in issues {
            lines.push(format!("  {}: {}", issue.field, issue.message));
        }
    }
    if let Some(reason) = profile_store::requires_explicit_confirmation(profile) {
        lines.push(String::new());
        lines.push(reason);
        lines.push(format!(
            "  Applying it needs: /profile use {} --allow-bypass",
            profile.id
        ));
    }
    lines.join("\n")
}

/// One line per declared field. Absent fields produce no line at all — the
/// listing should look like what the file says, not like a form with blanks.
pub fn declared_lines(profile: &Profile) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(provider) = profile.provider.as_deref() {
        lines.push(format!("  provider:        {provider}"));
    }
    if let Some(model) = profile.model.as_deref() {
        lines.push(format!("  model:           {model}"));
    }
    if let Some(agent) = profile.agent.as_deref() {
        lines.push(format!("  agent:           {agent}"));
    }
    if let Some(mode) = profile.permission_mode.as_deref() {
        lines.push(format!("  permission mode: {mode}"));
    }
    if let Some(spec) = profile.tools.as_ref() {
        let filter = rebon_tool::ToolFilter::from_spec(spec.clone());
        lines.push(format!("  tools:           {}", filter.describe_allowed()));
    }
    lines
}

/// What the profile left alone, so "it didn't change my model" reads as the
/// design rather than as a bug.
pub fn unchanged_note(profile: &Profile) -> String {
    let untouched = untouched_surfaces(profile);
    if untouched.is_empty() {
        return String::new();
    }
    format!(
        "\n\nLeft as they were: {}. A profile only changes what it declares.",
        untouched.join(", ")
    )
}

/// The surfaces a profile declares nothing for, named in apply order.
pub fn untouched_surfaces(profile: &Profile) -> Vec<&'static str> {
    let mut untouched = Vec::new();
    if profile.provider.is_none() && profile.model.is_none() {
        untouched.push("provider/model");
    }
    if profile.agent.is_none() {
        untouched.push("agent");
    }
    if profile.permission_mode.is_none() {
        untouched.push("permission mode");
    }
    if profile.tools.is_none() {
        untouched.push("tools");
    }
    untouched
}

/// Everything standing between this profile and a clean `/profile use`.
///
/// `issues` is the store's verdict; the agent check is not in it because
/// `rebon-config` cannot see a session's agent registry. It is a note rather
/// than a refusal on purpose: a profile is global and an agent can be
/// configured per project, so "this session has never heard of it" is worth
/// saying and not worth blocking on.
pub fn unapplied_notes(
    session: &dyn ProfileSession,
    profile: &Profile,
    issues: &[profile_store::ProfileIssue],
) -> Vec<String> {
    let mut lines = Vec::new();
    if !issues.is_empty() {
        lines.push(String::new());
        lines.push("Will not apply as it stands:".into());
        for issue in issues {
            lines.push(format!("  {}: {}", issue.field, issue.message));
        }
    }
    if let Some(agent) = profile.agent.as_deref().map(str::trim) {
        let known = session
            .known_agent_ids()
            .iter()
            .any(|id| id.eq_ignore_ascii_case(agent));
        if !known {
            lines.push(String::new());
            lines.push(format!(
                "No agent `{agent}` is configured in this session — `/backend` lists the ones that are. Written anyway: agents can be configured per project, and this profile is not."
            ));
        }
    }
    if let Some(reason) = profile_store::requires_explicit_confirmation(profile) {
        lines.push(String::new());
        lines.push(reason);
        lines.push(format!(
            "  Applying it needs: /profile use {} --allow-bypass",
            profile.id
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::apply::tests::vendor_config;

    fn writing() -> Profile {
        Profile {
            display_name: Some("Writing".into()),
            provider: Some("vendor".into()),
            model: Some("vendor-flash".into()),
            permission_mode: Some("acceptEdits".into()),
            ..Profile::new("writing")
        }
    }

    #[test]
    fn the_listing_shows_only_what_each_profile_declares() {
        let tmp = TempDir::new().unwrap();
        profile_store::save(tmp.path(), &writing()).unwrap();

        let rendered = render_list(tmp.path());

        assert!(rendered.contains("writing"), "{rendered}");
        assert!(rendered.contains("vendor-flash"), "{rendered}");
        assert!(rendered.contains("acceptEdits"), "{rendered}");
        // Nothing was declared for the agent, so no agent row is invented.
        assert!(!rendered.contains("agent:"), "{rendered}");
    }

    #[test]
    fn show_names_the_surfaces_the_profile_leaves_alone() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        profile_store::save(tmp.path(), &writing()).unwrap();
        let profile = profile_store::load(tmp.path(), "writing").unwrap();

        let rendered = render_profile(tmp.path(), &profile);

        assert!(rendered.contains("Left as they were"), "{rendered}");
        assert!(rendered.contains("agent"), "{rendered}");
        assert!(rendered.contains("tools"), "{rendered}");
        assert!(!rendered.contains("Will not apply"), "{rendered}");
    }

    #[test]
    fn show_reports_a_profile_that_would_fail_and_how_bypass_is_gated() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        let broken = Profile {
            provider: Some("ghost".into()),
            permission_mode: Some("bypassPermissions".into()),
            ..writing()
        };
        profile_store::save(tmp.path(), &broken).unwrap();
        let profile = profile_store::load(tmp.path(), "writing").unwrap();

        let rendered = render_profile(tmp.path(), &profile);

        assert!(rendered.contains("Will not apply"), "{rendered}");
        assert!(rendered.contains("ghost"), "{rendered}");
        assert!(rendered.contains("--allow-bypass"), "{rendered}");
    }

    #[test]
    fn a_tools_declaration_renders_as_the_surface_it_leaves() {
        let narrow = Profile {
            tools: Some(rebon_types::ToolFilterSpec {
                allow: Some(std::collections::BTreeSet::from([
                    "Read".to_string(),
                    "Edit".to_string(),
                ])),
                deny: std::collections::BTreeSet::new(),
            }),
            ..writing()
        };

        let lines = declared_lines(&narrow).join("\n");

        assert!(lines.contains("Read"), "{lines}");
        assert!(lines.contains("Edit"), "{lines}");
    }
}
