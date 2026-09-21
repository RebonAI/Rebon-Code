//! Moving a live session onto a profile, and capturing one back off it.
//!
//! # Applying is checked first, not rolled back after
//!
//! A profile is validated in full before anything is touched, because the
//! failure that actually happens — a provider that was removed, a model it no
//! longer serves — is knowable up front. What is *not* attempted is a rollback
//! after a partial apply: switching the provider writes to `config.json`, and
//! an "undo" that writes again is another chance to fail with even less
//! context. So a step that fails says which steps had already landed, rather
//! than claiming a cleanliness it cannot deliver.
//!
//! # What a profile does not get to do
//!
//! `bypassPermissions` never applies from the profile alone — the user has to
//! say it again on the command line. The profile design rules out a profile
//! that silently carries the prompts away, and this is where that is enforced.

use std::path::Path;

use rebon_config::profile_store::{self, Profile};
use rebon_permissions::{permission_mode_title, PermissionMode};
use rebon_tool::ToolFilter;

use crate::render::{declared_lines, unchanged_note};
use crate::{ProfileApplyOutcome, ProfileCommandResult, RuntimeRefresh};

/// The live session a profile is applied to, or captured from.
///
/// Every method is over a handle the front end already holds. Nothing here
/// draws, and nothing here decides *whether* a step should happen — that
/// order lives in [`apply_to_session`], so it is decided once rather than once
/// per front end.
pub trait ProfileSession {
    /// The model this session is actually running, as its footer shows it.
    fn model_name(&self) -> String;

    /// The agent backend currently driving the session.
    fn agent_id(&self) -> String;

    /// Every backend this session could switch to. Used for a note, never for
    /// a refusal: a profile is global and an agent can be configured per
    /// project, so "this session has never heard of it" is worth saying and
    /// not worth blocking on.
    fn known_agent_ids(&self) -> Vec<String>;

    /// Hand the session to another backend, returning its label.
    fn switch_agent(&self, id: &str) -> Result<String, String>;

    fn permission_mode(&self) -> PermissionMode;

    /// Move the session onto `mode`, wherever the front end mirrors it.
    ///
    /// Not persisted as the global default, which is the one thing the
    /// terminal's Shift+Tab does that this does not: a profile's mode belongs
    /// to the sessions that profile is applied to, and writing it to the
    /// default would leave every unrelated session that opened afterwards in a
    /// mode nobody chose for it.
    fn set_permission_mode(&mut self, mode: PermissionMode);

    fn tool_filter(&self) -> ToolFilter;

    fn set_tool_filter(&self, filter: ToolFilter);

    /// The surface this session would have with no profile applied — what
    /// `/profile reset` puts back.
    fn default_tool_filter(&self) -> ToolFilter;
}

/// Apply everything a profile declares, in an order a failure can report from.
pub fn apply_to_session(
    session: &mut dyn ProfileSession,
    config_dir: &Path,
    profile: &Profile,
    allow_bypass: bool,
) -> ProfileApplyOutcome {
    // Everything checkable is checked here, before the first write. This is
    // what the provider store bought: a profile naming a provider that has
    // since been deleted fails now, with the name of the thing to fix, rather
    // than as a 401 on the user's next message.
    let issues = profile_store::validate(config_dir, profile);
    if !issues.is_empty() {
        let mut text = format!("Profile \"{}\" cannot be applied:\n", profile.label());
        for issue in &issues {
            text.push_str(&format!("  {}: {}\n", issue.field, issue.message));
        }
        return ProfileApplyOutcome::nothing_applied(ProfileCommandResult::err(
            text.trim_end().to_string(),
        ));
    }

    if let Some(reason) = profile_store::requires_explicit_confirmation(profile) {
        if !allow_bypass {
            return ProfileApplyOutcome::nothing_applied(ProfileCommandResult::err(format!(
                "{reason}\n\nIf that is what you want, say so on the line:\n  /profile use {} --allow-bypass",
                profile.id
            )));
        }
    }

    let mut applied: Vec<String> = Vec::new();

    // 1. Provider and model. Both persist to config, and the runtime is then
    //    re-resolved from disk in one pass — the same route `/provider use`
    //    and `/model` take, so a profile cannot end up somewhere neither of
    //    those commands could reach.
    let mut runtime_update = None;
    if let Some(provider) = profile.provider.as_deref().map(str::trim) {
        match rebon_config::set_active_custom_provider(provider) {
            Ok(info) => {
                applied.push(format!("provider → {}", info.name));
                runtime_update = Some(RuntimeRefresh {
                    provider_name: info.name.clone(),
                    model_name: rebon_config::resolve_env_value(&info.model),
                });
            }
            Err(err) => return partial(&applied, &format!("provider: {err}"), runtime_update),
        }
    }
    if let Some(model) = profile.model.as_deref().map(str::trim) {
        let provider = profile
            .provider
            .as_deref()
            .map(str::trim)
            .map(str::to_string)
            .or_else(rebon_config::get_active_custom_provider_name);
        let Some(provider) = provider else {
            return partial(
                &applied,
                "model: no provider is active, so there is nothing to set a model on.",
                runtime_update,
            );
        };
        match rebon_config::set_custom_provider_model(&provider, model) {
            Ok(info) => {
                // Same reason `/model` does this: the legacy global override
                // shadows every provider, so leaving it set would let it undo
                // this switch the next time the provider changes.
                if let Err(err) = rebon_config::save_user_model(None) {
                    tracing::warn!(error = %err, "failed to clear the legacy global model override");
                }
                applied.push(format!("model → {}", info.model));
                runtime_update = Some(RuntimeRefresh {
                    provider_name: info.name.clone(),
                    model_name: rebon_config::resolve_env_value(&info.model),
                });
            }
            Err(err) => return partial(&applied, &format!("model: {err}"), runtime_update),
        }
    }
    // 2. Agent backend.
    if let Some(agent) = profile.agent.as_deref().map(str::trim) {
        match session.switch_agent(agent) {
            Ok(label) => applied.push(format!("agent → {label}")),
            Err(err) => return partial(&applied, &format!("agent: {err}"), runtime_update),
        }
    }

    // 3. Permission mode.
    if let Some(mode) = profile.permission_mode.as_deref() {
        let Some(mode) = profile_store::parse_permission_mode(mode) else {
            return partial(
                &applied,
                &format!("permission mode: \"{mode}\" is unknown"),
                runtime_update,
            );
        };
        session.set_permission_mode(mode);
        applied.push(format!("permission mode → {}", permission_mode_title(mode)));
    }

    // 4. Tool surface. The executor re-reads this handle every iteration, so
    //    a narrowing lands on the next tool call without rebuilding anything.
    //
    //    Intersected with this session's current surface rather than replacing
    //    it, so a profile can only ever take tools away. Replacing would have
    //    made a profile the one way to hand back a tool the environment had
    //    removed — `REBON_DENY_TOOLS=Bash` would survive `/ceo` and every other
    //    switch and then quietly lose to a file in `~/.rebon/profiles`.
    //
    //    The sub-agent filter is deliberately left alone: it is `/ceo`'s to
    //    pair with this one, and a writing profile that also narrowed workers
    //    would disarm sub-agents that the session is still allowed to spawn.
    if let Some(spec) = profile.tools.as_ref() {
        let narrowed = session
            .tool_filter()
            .intersect(&ToolFilter::from_spec(spec.clone()));
        session.set_tool_filter(narrowed.clone());
        applied.push(format!("tools → {}", narrowed.describe_allowed()));
    }

    let mut text = format!("Switched to profile \"{}\".", profile.label());
    if let Some(description) = profile.description.as_deref().filter(|d| !d.is_empty()) {
        text.push_str(&format!(" {description}"));
    }
    for line in &applied {
        text.push_str(&format!("\n  {line}"));
    }
    text.push_str(&unchanged_note(profile));
    ProfileApplyOutcome {
        report: ProfileCommandResult::ok(text),
        runtime_refresh: runtime_update,
    }
}

fn partial(
    applied: &[String],
    failure: &str,
    runtime_refresh: Option<RuntimeRefresh>,
) -> ProfileApplyOutcome {
    let mut text = format!("Profile only partly applied — {failure}");
    if applied.is_empty() {
        text.push_str("\n\nNothing was changed.");
    } else {
        text.push_str("\n\nAlready changed before this, and left in place:");
        for line in applied {
            text.push_str(&format!("\n  {line}"));
        }
    }
    ProfileApplyOutcome {
        report: ProfileCommandResult::err(text),
        // Still handed back: the provider may already have moved on disk, and
        // a session left pointing at the old client would fail every request
        // after a failure it was told was only partial.
        runtime_refresh,
    }
}

/// `/profile reset` — only the tool surface.
///
/// Provider, model, agent and permission mode each have a command of their own
/// to move them back; the tool filter is the one a profile can narrow with no
/// other way out.
pub fn reset_tool_surface(session: &dyn ProfileSession) -> ProfileCommandResult {
    let default = session.default_tool_filter();
    session.set_tool_filter(default.clone());
    ProfileCommandResult::ok(format!(
        "Tool surface back to this session's default: {}.\n\nProvider, model, agent and permission mode are unchanged — /provider use, /model, /backend and Shift+Tab move those.",
        default.describe_allowed()
    ))
}

/// `/profile save <name>` — capture all five surfaces off the live session.
pub fn save_current_session(
    config_dir: &Path,
    session: &dyn ProfileSession,
    name: &str,
    description: Option<String>,
) -> ProfileCommandResult {
    if name.trim().is_empty() {
        return ProfileCommandResult::err("Usage: /profile save <name> [description]");
    }
    let filter = session.tool_filter();
    let profile = Profile {
        display_name: Some(name.trim().to_string()),
        description,
        provider: rebon_config::get_active_custom_provider_name_from(config_dir),
        model: Some(session.model_name()).filter(|model| !model.is_empty()),
        agent: Some(session.agent_id()),
        permission_mode: Some(session.permission_mode().as_wire().to_string()),
        // An unrestricted filter is the absence of a tool declaration, not a
        // declaration of "everything" — storing the latter would make every
        // saved profile re-widen the tools of whatever it is applied over.
        tools: (!filter.is_unrestricted()).then(|| filter.to_spec()),
        ..Profile::new(profile_store::profile_id(name))
    };
    if let Err(err) = profile_store::save(config_dir, &profile) {
        return ProfileCommandResult::err(err.to_string());
    }
    ProfileCommandResult::ok(format!(
        "Saved this session as profile \"{}\".\n\n{}\n\nSwitch to it later with /profile use {}.",
        profile.label(),
        declared_lines(&profile).join("\n"),
        profile.id
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeSet;

    use tempfile::TempDir;

    use super::*;
    use crate::test_session::FakeSession;

    /// A provider entry the profile store can validate a model against.
    pub(crate) fn vendor_config(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            rebon_config::config_json_path(dir),
            r#"{
                "activeCustomProvider":"vendor",
                "customProviders":[
                    {"name":"vendor","format":"openai","baseUrl":"https://example.com",
                     "apiKey":"sk","model":"vendor-pro",
                     "models":["vendor-pro","vendor-flash"]}
                ]
            }"#,
        )
        .unwrap();
    }

    #[test]
    fn a_tools_profile_can_only_take_tools_away() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        // Something the environment took away before any profile existed.
        let mut session =
            FakeSession::default().with_filter(ToolFilter::unrestricted().with_deny(["Bash"]));
        let profile = Profile {
            tools: Some(rebon_types::ToolFilterSpec {
                allow: Some(BTreeSet::from(["Read".to_string(), "Bash".to_string()])),
                deny: BTreeSet::new(),
            }),
            ..Profile::new("narrow")
        };

        let outcome = apply_to_session(&mut session, tmp.path(), &profile, false);

        assert!(!outcome.report.is_err, "{}", outcome.report.text);
        let filter = session.tool_filter();
        assert!(filter.allows("Read", &[]));
        // Named in the profile's allow list and still gone: a profile is not a
        // way to hand back what the environment removed.
        assert!(!filter.allows("Bash", &[]));
        assert!(!filter.allows("Write", &[]));
    }

    #[test]
    fn a_permission_mode_profile_moves_the_mode_and_says_what_it_left() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        let mut session = FakeSession::default();
        let profile = Profile {
            permission_mode: Some("acceptEdits".into()),
            ..Profile::new("edits")
        };

        let outcome = apply_to_session(&mut session, tmp.path(), &profile, false);

        assert!(!outcome.report.is_err, "{}", outcome.report.text);
        assert_eq!(session.permission_mode(), PermissionMode::AcceptEdits);
        assert!(
            outcome.report.text.contains("Left as they were"),
            "{}",
            outcome.report.text
        );
    }

    #[test]
    fn bypass_needs_the_flag_and_changes_nothing_without_it() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        let mut session = FakeSession::default();
        let profile = Profile {
            permission_mode: Some("bypassPermissions".into()),
            ..Profile::new("risky")
        };

        let refused = apply_to_session(&mut session, tmp.path(), &profile, false);

        assert!(refused.report.is_err);
        assert!(
            refused.report.text.contains("--allow-bypass"),
            "{}",
            refused.report.text
        );
        assert_eq!(
            session.permission_mode(),
            PermissionMode::Default,
            "a refused profile must not have moved the mode on its way out"
        );

        let allowed = apply_to_session(&mut session, tmp.path(), &profile, true);

        assert!(!allowed.report.is_err, "{}", allowed.report.text);
        assert_eq!(session.permission_mode(), PermissionMode::BypassPermissions);
    }

    #[test]
    fn a_profile_naming_a_provider_that_is_gone_is_refused_before_anything_moves() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        let mut session = FakeSession::default();
        let profile = Profile {
            provider: Some("ghost".into()),
            permission_mode: Some("acceptEdits".into()),
            ..Profile::new("stale")
        };

        let outcome = apply_to_session(&mut session, tmp.path(), &profile, false);

        assert!(outcome.report.is_err);
        assert!(
            outcome.report.text.contains("ghost"),
            "{}",
            outcome.report.text
        );
        // The permission mode came *after* the provider in the apply order, so
        // this is the check that validation runs first rather than the steps
        // failing one at a time on the way through.
        assert_eq!(session.permission_mode(), PermissionMode::Default);
    }

    #[test]
    fn reset_puts_the_tool_surface_back_and_says_what_it_did_not_touch() {
        let session = FakeSession::default().with_filter(ToolFilter::allow_only(["Read"]));

        let result = reset_tool_surface(&session);

        assert!(!result.is_err, "{}", result.text);
        assert!(session.tool_filter().allows("Write", &[]));
        assert!(result.text.contains("/provider use"), "{}", result.text);
    }

    /// A backend that refuses reports which earlier steps had already landed,
    /// rather than claiming a cleanliness the apply cannot deliver.
    #[test]
    fn a_failed_step_names_what_already_happened() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        let mut session = FakeSession {
            refuse_switch: Some("that agent is not attached here".into()),
            ..FakeSession::default()
        };
        let profile = Profile {
            agent: Some("ghost-cli".into()),
            permission_mode: Some("acceptEdits".into()),
            ..Profile::new("handoff")
        };

        let outcome = apply_to_session(&mut session, tmp.path(), &profile, false);

        assert!(outcome.report.is_err);
        assert!(
            outcome.report.text.contains("Nothing was changed"),
            "{}",
            outcome.report.text
        );
        // The mode came after the agent, so it must not have moved.
        assert_eq!(session.permission_mode(), PermissionMode::Default);
    }

    #[test]
    fn save_captures_all_five_surfaces_off_the_session() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        let session = FakeSession::default()
            .with_model("vendor-flash")
            .with_filter(ToolFilter::allow_only(["Read"]));

        let result = save_current_session(tmp.path(), &session, "writing", None);

        assert!(!result.is_err, "{}", result.text);
        let stored = profile_store::load(tmp.path(), "writing").unwrap();
        assert_eq!(stored.model.as_deref(), Some("vendor-flash"));
        assert_eq!(stored.agent.as_deref(), Some("local"));
        assert_eq!(stored.permission_mode.as_deref(), Some("default"));
        assert!(stored.tools.is_some());
    }

    /// An unrestricted surface is the *absence* of a tool declaration. Storing
    /// it as "everything" would make every saved profile re-widen whatever it
    /// was applied over.
    #[test]
    fn an_unrestricted_surface_is_not_saved_as_a_tool_declaration() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        let session = FakeSession::default();

        assert!(!save_current_session(tmp.path(), &session, "wide", None).is_err);

        assert_eq!(profile_store::load(tmp.path(), "wide").unwrap().tools, None);
    }
}
