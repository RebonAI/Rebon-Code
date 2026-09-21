//! What a `ProfileSwitch` / `ProfileSave` prompt shows, and what approving it
//! does.
//!
//! The tools carry a proposal and nothing more: a profile name and the model's
//! reason, or the fields it wants written. Everything that makes the prompt
//! worth reading happens here, because only here is there a session to compare
//! against and a `rebon-config` to resolve names through.
//!
//! # The diff is computed, not quoted
//!
//! What the prompt shows for "now" is read out of the live session — its
//! runtime model, its agent backend, its permission mode, its tool filter —
//! and what it shows for "after" comes from the profile on disk. Nothing the
//! model wrote reaches those columns. So a model cannot describe one switch
//! and have another one happen, and it cannot make a change look smaller than
//! it is by omitting a row.
//!
//! # Two refusals happen before the user is asked
//!
//! A profile that does not exist, and a profile carrying
//! `bypassPermissions`, are both turned away with a reason the model reads —
//! no prompt is raised. The first is the model's mistake and not worth the
//! user's attention; the second is the line the profile design draws, and a
//! prompt offering it would be an invitation rather than a refusal. The user
//! can still apply such a profile themselves with
//! `/profile use <name> --allow-bypass`.

use std::path::Path;

use serde_json::{json, Value};

use rebon_config::profile_store::{self, Profile};
use rebon_permissions::permission_mode_title;

use crate::apply::{apply_to_session, ProfileSession};
use crate::render::untouched_surfaces;
use crate::RuntimeRefresh;

/// Which of the two profile proposals a prompt is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfileProposalAction {
    /// Move this session onto a saved profile.
    Switch,
    /// Write a new profile to `~/.rebon/profiles/`.
    Save,
}

/// One line of a profile prompt: a field, and what would become of it.
///
/// For a switch, `from` is what the session is running *now* — read from the
/// live session rather than from anything the model said, so the change the
/// user approves is the change that happens. For a save there is nothing to
/// diff against, and `from` is `None`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileProposalRow {
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub to: String,
    /// Drawn in the warning colour. Set for the permission mode: it is the one
    /// field that changes what *else* can happen without asking, so it must
    /// not read as one more row in a list.
    #[serde(default)]
    pub highlight: bool,
}

/// The whole of what a profile prompt shows.
///
/// Built here, carried to the front end's modal through the permission query's
/// metadata, and rendered there. The tool contributes only the profile name
/// and the model's reason; everything else is resolved against what is
/// actually on disk and in the session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileProposal {
    pub action: ProfileProposalAction,
    pub profile_id: String,
    pub label: String,
    /// The model's case for the change, shown above the rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub rows: Vec<ProfileProposalRow>,
    /// Anything the user should know that is not a field change: a declared
    /// field that would be a no-op, a surface left alone, a caveat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The tool input this proposal was built from.
    ///
    /// Carried so the approval path rebuilds what it writes through the same
    /// function that validated it, rather than reconstructing a profile from
    /// the display rows — those are shaped for reading and would silently drop
    /// anything the rows do not spell out.
    #[serde(default)]
    pub original_input: Value,
}

/// Metadata key carrying a [`ProfileProposal`] on the outbound query.
const PROFILE_PROPOSAL_METADATA_KEY: &str = "profileProposal";

impl ProfileProposal {
    pub fn to_permission_metadata(&self) -> Value {
        json!({ PROFILE_PROPOSAL_METADATA_KEY: self })
    }

    pub fn from_permission_metadata(metadata: &Value) -> Option<Self> {
        serde_json::from_value(metadata.get(PROFILE_PROPOSAL_METADATA_KEY)?.clone()).ok()
    }

    pub fn verb(&self) -> &'static str {
        match self.action {
            ProfileProposalAction::Switch => "switch to",
            ProfileProposalAction::Save => "save",
        }
    }
}

/// Which proposal, if any, a tool name is asking for.
pub fn resolve_proposal_action(tool_name: &str) -> Option<ProfileProposalAction> {
    match tool_name {
        crate::PROFILE_SWITCH_TOOL_NAME => Some(ProfileProposalAction::Switch),
        crate::PROFILE_SAVE_TOOL_NAME => Some(ProfileProposalAction::Save),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Building the proposal
// ---------------------------------------------------------------------------

pub fn switch_proposal(
    session: &dyn ProfileSession,
    config_dir: &Path,
    input: &Value,
) -> Result<ProfileProposal, String> {
    let name = string_field(input, "profile")
        .ok_or_else(|| "ProfileSwitch needs `profile`: the name of a saved profile.".to_string())?;
    let profile = profile_store::load(config_dir, &name).map_err(|err| {
        format!("{err} Only profiles the user has already saved can be switched to; you cannot create one with this tool.")
    })?;

    // Refused before the user sees anything. A prompt offering to turn the
    // prompts off is not a safeguard, it is the ask.
    if profile_store::requires_explicit_confirmation(&profile).is_some() {
        return Err(format!(
            "Profile \"{}\" turns permission prompts off (bypassPermissions), and that is not something to ask for. Only the user can apply it, with `/profile use {} --allow-bypass`.",
            profile.label(),
            profile.id
        ));
    }

    // Checked here rather than at apply time so a stale profile is the model's
    // problem, not an error the user has to read in a prompt they approved.
    let issues = profile_store::validate(config_dir, &profile);
    if !issues.is_empty() {
        return Err(format!(
            "Profile \"{}\" cannot be applied as it stands — {}",
            profile.label(),
            join_issues(&issues)
        ));
    }

    let rows = switch_rows(session, &profile);
    let mut notes = Vec::new();
    if rows.is_empty() {
        notes.push("Every field this profile declares already matches the session.".to_string());
    }
    let untouched = untouched_surfaces(&profile);
    if !untouched.is_empty() {
        notes.push(format!(
            "Left alone: {}. A profile only changes what it declares.",
            untouched.join(", ")
        ));
    }

    Ok(ProfileProposal {
        action: ProfileProposalAction::Switch,
        profile_id: profile.id.clone(),
        label: profile.label().to_string(),
        reason: string_field(input, "reason"),
        rows,
        notes,
        original_input: input.clone(),
    })
}

/// One row per field the profile would actually change.
///
/// A declared field that already matches produces no row: the prompt is a list
/// of what would happen, and padding it with lines that change nothing is how
/// the one line that matters gets skimmed past.
fn switch_rows(session: &dyn ProfileSession, profile: &Profile) -> Vec<ProfileProposalRow> {
    let mut rows = Vec::new();
    if let Some(provider) = profile.provider.as_deref().map(str::trim) {
        let current = rebon_config::get_active_custom_provider_name().unwrap_or_default();
        if !current.eq_ignore_ascii_case(provider) {
            rows.push(row("provider", Some(current), provider, false));
        }
    }
    if let Some(model) = profile.model.as_deref().map(str::trim) {
        let current = session.model_name();
        if !current.eq_ignore_ascii_case(model) {
            rows.push(row("model", Some(current), model, false));
        }
    }
    if let Some(agent) = profile.agent.as_deref().map(str::trim) {
        let current = session.agent_id();
        if !current.eq_ignore_ascii_case(agent) {
            rows.push(row("agent", Some(current), agent, false));
        }
    }
    if let Some(mode) = profile.permission_mode.as_deref() {
        if let Some(mode) = profile_store::parse_permission_mode(mode) {
            let current = session.permission_mode();
            if mode != current {
                rows.push(row(
                    "permission mode",
                    Some(permission_mode_title(current).to_string()),
                    permission_mode_title(mode),
                    // The one field that changes what else can happen without
                    // being asked again.
                    true,
                ));
            }
        }
    }
    if let Some(spec) = profile.tools.as_ref() {
        let current = session.tool_filter();
        let narrowed = current.intersect(&rebon_tool::ToolFilter::from_spec(spec.clone()));
        if narrowed != current {
            rows.push(row(
                "tools",
                Some(current.describe_allowed()),
                narrowed.describe_allowed(),
                false,
            ));
        }
    }
    rows
}

pub fn save_proposal(config_dir: &Path, input: &Value) -> Result<ProfileProposal, String> {
    let name =
        string_field(input, "name").ok_or_else(|| "ProfileSave needs `name`.".to_string())?;
    let profile = profile_from_save_input(&name, input)?;

    // The model wrote these names; check them against the providers that
    // actually exist before offering to write the file. A profile that cannot
    // be applied is worse than no profile — it fails at the moment the user
    // reaches for it, long after this conversation.
    let issues = profile_store::validate(config_dir, &profile);
    if !issues.is_empty() {
        return Err(format!(
            "This profile would not apply — {}",
            join_issues(&issues)
        ));
    }

    let existing = profile_store::load(config_dir, &profile.id).ok();
    let mut notes = Vec::new();
    if let Some(existing) = existing.as_ref() {
        notes.push(format!(
            "Overwrites the existing profile \"{}\".",
            existing.label()
        ));
    }
    notes.push(format!(
        "Saved to ~/.rebon/profiles/{}.json. Nothing about this session changes; apply it later with /profile use {}.",
        profile.id, profile.id
    ));

    Ok(ProfileProposal {
        action: ProfileProposalAction::Save,
        profile_id: profile.id.clone(),
        label: profile.label().to_string(),
        reason: profile.description.clone(),
        rows: save_rows(&profile, existing.as_ref()),
        notes,
        original_input: input.clone(),
    })
}

/// One row per declared field, showing the value that would be written.
///
/// When a profile of the same name already exists this becomes a real diff:
/// overwriting is the case where the user most needs to see what they are
/// losing, not just what they are gaining.
fn save_rows(profile: &Profile, existing: Option<&Profile>) -> Vec<ProfileProposalRow> {
    let mut rows = Vec::new();
    let mut push = |field: &str, to: Option<&str>, from: Option<&str>, highlight: bool| {
        if let Some(to) = to {
            rows.push(row(field, from.map(str::to_string), to, highlight));
        }
    };
    push(
        "provider",
        profile.provider.as_deref(),
        existing.and_then(|p| p.provider.as_deref()),
        false,
    );
    push(
        "model",
        profile.model.as_deref(),
        existing.and_then(|p| p.model.as_deref()),
        false,
    );
    push(
        "agent",
        profile.agent.as_deref(),
        existing.and_then(|p| p.agent.as_deref()),
        false,
    );
    push(
        "permission mode",
        profile.permission_mode.as_deref(),
        existing.and_then(|p| p.permission_mode.as_deref()),
        true,
    );
    if let Some(spec) = profile.tools.as_ref() {
        let described = rebon_tool::ToolFilter::from_spec(spec.clone()).describe_allowed();
        let before = existing
            .and_then(|p| p.tools.as_ref())
            .map(|spec| rebon_tool::ToolFilter::from_spec(spec.clone()).describe_allowed());
        rows.push(row("tools", before, described, false));
    }
    rows
}

/// Turn `ProfileSave`'s input into a [`Profile`].
///
/// The tool already refused an empty declaration and a bypass mode; this
/// repeats neither. What it does check is the permission mode's spelling,
/// because the profile is about to be written and an unparseable mode would
/// sit on disk until someone applied it.
pub fn profile_from_save_input(name: &str, input: &Value) -> Result<Profile, String> {
    let tools = match input.get("tools") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            serde_json::from_value::<rebon_types::ToolFilterSpec>(value.clone())
                .map_err(|err| format!("ProfileSave `tools` is not a tool filter: {err}"))?,
        ),
    };
    if let Some(mode) = string_field(input, "permissionMode") {
        if profile_store::parse_permission_mode(&mode).is_none() {
            return Err(format!(
                "\"{mode}\" is not a permission mode. Use one of: {}.",
                profile_store::KNOWN_PERMISSION_MODES.join(", ")
            ));
        }
    }
    Ok(Profile {
        display_name: Some(name.to_string()),
        description: string_field(input, "description"),
        provider: string_field(input, "provider"),
        model: string_field(input, "model"),
        agent: string_field(input, "agent"),
        permission_mode: string_field(input, "permissionMode"),
        tools,
        ..Profile::new(profile_store::profile_id(name))
    })
}

// ---------------------------------------------------------------------------
// Applying an approved proposal
// ---------------------------------------------------------------------------

/// What approving a proposal did.
///
/// `runtime_refresh` sits outside `result` on purpose: a partial apply is an
/// error that has *already moved the provider on disk*, and a session left
/// pointing at the old client would fail every request after a failure it was
/// told was only partial. So the caller parks the refresh either way, and only
/// then reads the verdict.
pub struct ApprovedOutcome {
    pub result: Result<Value, String>,
    pub runtime_refresh: Option<RuntimeRefresh>,
}

impl ApprovedOutcome {
    fn failed(reason: impl Into<String>) -> Self {
        Self {
            result: Err(reason.into()),
            runtime_refresh: None,
        }
    }
}

/// Carry out an approved proposal and report what happened.
///
/// The `Ok` value is injected into the tool's input as `applied` / `written`
/// (see [`applied_input`]), which is the only thing that makes the tool's
/// `call()` report success — so a proposal approved on a surface that never
/// reaches this function tells the model nothing happened, because nothing did.
///
/// `session` is `None` when the session has gone away between the prompt and
/// the answer. A save does not need one; a switch fails rather than pretending.
pub fn apply_approved_proposal(
    session: Option<&mut dyn ProfileSession>,
    config_dir: &Path,
    proposal: &ProfileProposal,
) -> ApprovedOutcome {
    match proposal.action {
        ProfileProposalAction::Save => {
            // Rebuilt through the same function that validated it, so what is
            // written is what was checked and shown.
            let profile = match profile_from_save_input(&proposal.label, &proposal.original_input) {
                Ok(profile) => profile,
                Err(reason) => return ApprovedOutcome::failed(reason),
            };
            if let Err(err) = profile_store::save(config_dir, &profile) {
                return ApprovedOutcome::failed(err.to_string());
            }
            ApprovedOutcome {
                result: Ok(json!({
                    "profile": profile.id,
                    "path": format!("~/.rebon/profiles/{}.json", profile.id),
                })),
                runtime_refresh: None,
            }
        }
        ProfileProposalAction::Switch => {
            let Some(session) = session else {
                return ApprovedOutcome::failed(
                    "the session is no longer available, so the profile was not applied",
                );
            };
            let profile = match profile_store::load(config_dir, &proposal.profile_id) {
                Ok(profile) => profile,
                Err(err) => return ApprovedOutcome::failed(err.to_string()),
            };
            // `allow_bypass: false` — unconditionally. A model-requested
            // switch never gets the flag, and `switch_proposal` already
            // refused such a profile; this is the second lock on the same
            // door, placed where the write actually happens.
            let outcome = apply_to_session(session, config_dir, &profile, false);
            ApprovedOutcome {
                result: if outcome.report.is_err {
                    Err(outcome.report.text)
                } else {
                    Ok(json!({
                        "profile": profile.id,
                        "summary": outcome.report.text,
                    }))
                },
                runtime_refresh: outcome.runtime_refresh,
            }
        }
    }
}

/// The tool input to answer with once a proposal has been applied.
///
/// The report goes back as a key on the tool's own input, which is how the
/// tool learns that a front end capable of applying it actually did. Without
/// this key `call()` errors rather than reporting a switch that never
/// happened — see [`crate::profile_switch`].
pub fn applied_input(proposal: &ProfileProposal, result: Value) -> Value {
    let key = match proposal.action {
        ProfileProposalAction::Switch => crate::PROFILE_SWITCH_APPLIED_KEY,
        ProfileProposalAction::Save => crate::PROFILE_SAVE_WRITTEN_KEY,
    };
    let mut input = proposal
        .original_input
        .as_object()
        .cloned()
        .unwrap_or_default();
    input.insert(key.to_string(), result);
    Value::Object(input)
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

fn join_issues(issues: &[profile_store::ProfileIssue]) -> String {
    issues
        .iter()
        .map(|issue| format!("{}: {}", issue.field, issue.message))
        .collect::<Vec<_>>()
        .join("; ")
}

fn row(
    field: &str,
    from: Option<String>,
    to: impl Into<String>,
    highlight: bool,
) -> ProfileProposalRow {
    ProfileProposalRow {
        field: field.to_string(),
        from: from.filter(|value| !value.trim().is_empty()),
        to: to.into(),
        highlight,
    }
}

fn string_field(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rebon_permissions::PermissionMode;
    use tempfile::TempDir;

    use super::*;
    use crate::apply::tests::vendor_config;
    use crate::test_session::FakeSession;

    fn writing(dir: &Path) -> Profile {
        let profile = Profile {
            display_name: Some("Writing".into()),
            model: Some("vendor-flash".into()),
            permission_mode: Some("acceptEdits".into()),
            ..Profile::new("writing")
        };
        profile_store::save(dir, &profile).unwrap();
        profile
    }

    #[test]
    fn the_permission_mode_row_is_the_one_that_is_highlighted() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        let profile = writing(tmp.path());
        let mut session = FakeSession::default().with_model("vendor-pro");

        let rows = switch_rows(&session, &profile);

        let mode_row = rows
            .iter()
            .find(|row| row.field == "permission mode")
            .expect("the mode changes");
        assert!(
            mode_row.highlight,
            "the field that decides what else happens unasked must stand out"
        );
        let model_row = rows.iter().find(|row| row.field == "model").unwrap();
        assert!(!model_row.highlight);
        assert_eq!(model_row.from.as_deref(), Some("vendor-pro"));
        assert_eq!(model_row.to, "vendor-flash");

        // Once the session already matches, the row disappears rather than
        // padding the prompt with a change that is not one.
        session.mode = PermissionMode::AcceptEdits;
        let rows = switch_rows(&session, &profile);
        assert!(
            rows.iter().all(|row| row.field != "permission mode"),
            "{rows:?}"
        );
    }

    #[test]
    fn the_now_column_comes_from_the_session_not_from_the_model() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        let profile = writing(tmp.path());
        let session = FakeSession::default().with_model("something-the-model-never-mentioned");

        let rows = switch_rows(&session, &profile);

        let model_row = rows.iter().find(|row| row.field == "model").unwrap();
        assert_eq!(
            model_row.from.as_deref(),
            Some("something-the-model-never-mentioned")
        );
    }

    #[test]
    fn a_tools_row_shows_the_narrowing_that_would_actually_happen() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        let profile = Profile {
            tools: Some(rebon_types::ToolFilterSpec {
                allow: Some(BTreeSet::from(["Read".to_string(), "Bash".to_string()])),
                deny: BTreeSet::new(),
            }),
            ..Profile::new("narrow")
        };
        let session = FakeSession::default()
            .with_filter(rebon_tool::ToolFilter::unrestricted().with_deny(["Bash"]));

        let rows = switch_rows(&session, &profile);

        let tools = rows.iter().find(|row| row.field == "tools").unwrap();
        // Intersection, so the tool the environment removed does not come back
        // in the preview any more than it would in the apply.
        assert!(tools.to.contains("Read"), "{}", tools.to);
        assert!(!tools.to.contains("Bash"), "{}", tools.to);
    }

    #[test]
    fn a_bypass_profile_is_refused_rather_than_offered() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        profile_store::save(
            tmp.path(),
            &Profile {
                permission_mode: Some("bypassPermissions".into()),
                ..Profile::new("risky")
            },
        )
        .unwrap();

        let err = switch_proposal(
            &FakeSession::default(),
            tmp.path(),
            &json!({"profile": "risky", "reason": "faster"}),
        )
        .expect_err("must not become a prompt");

        // The user can still do it themselves; the model just cannot ask.
        assert!(err.contains("--allow-bypass"), "{err}");
    }

    #[test]
    fn an_unknown_profile_is_the_models_problem_not_a_prompt() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());

        let err = switch_proposal(
            &FakeSession::default(),
            tmp.path(),
            &json!({"profile": "ghost", "reason": "why not"}),
        )
        .expect_err("must not raise a prompt for a profile nobody saved");

        assert!(err.contains("ghost"), "{err}");
        assert!(err.contains("cannot create one"), "{err}");
    }

    #[test]
    fn a_save_naming_a_model_the_provider_does_not_serve_is_refused() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());

        let err = save_proposal(
            tmp.path(),
            &json!({"name": "bad", "provider": "vendor", "model": "vendor-ultra"}),
        )
        .expect_err("a profile that cannot apply must not be written");

        assert!(err.contains("vendor-ultra"), "{err}");
    }

    #[test]
    fn a_save_over_an_existing_profile_says_so_and_diffs_against_it() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        writing(tmp.path());

        let proposal = save_proposal(
            tmp.path(),
            &json!({"name": "Writing", "model": "vendor-pro"}),
        )
        .unwrap();

        assert!(
            proposal
                .notes
                .iter()
                .any(|note| note.contains("Overwrites")),
            "{:?}",
            proposal.notes
        );
        let model = proposal
            .rows
            .iter()
            .find(|row| row.field == "model")
            .unwrap();
        assert_eq!(model.from.as_deref(), Some("vendor-flash"));
        assert_eq!(model.to, "vendor-pro");
    }

    #[test]
    fn the_report_reaches_the_tool_as_its_own_input() {
        let proposal = ProfileProposal {
            action: ProfileProposalAction::Switch,
            profile_id: "writing".into(),
            label: "Writing".into(),
            reason: None,
            rows: Vec::new(),
            notes: Vec::new(),
            original_input: json!({"profile": "writing", "reason": "prose"}),
        };

        let updated = applied_input(&proposal, json!({"profile": "writing"}));

        // Without this key the tool refuses to claim the session moved.
        assert!(updated.get(crate::PROFILE_SWITCH_APPLIED_KEY).is_some());
        assert_eq!(updated["profile"], json!("writing"));
    }

    /// The proposal crosses a process boundary as permission metadata, so the
    /// round trip is what any front end actually reads.
    #[test]
    fn a_proposal_survives_the_metadata_round_trip() {
        let proposal = ProfileProposal {
            action: ProfileProposalAction::Save,
            profile_id: "writing".into(),
            label: "Writing".into(),
            reason: Some("prose".into()),
            rows: vec![ProfileProposalRow {
                field: "model".into(),
                from: None,
                to: "vendor-flash".into(),
                highlight: false,
            }],
            notes: vec!["Overwrites".into()],
            original_input: json!({"name": "Writing"}),
        };

        let back =
            ProfileProposal::from_permission_metadata(&proposal.to_permission_metadata()).unwrap();

        assert_eq!(back, proposal);
    }

    #[test]
    fn a_switch_with_no_session_left_fails_rather_than_reporting_success() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        writing(tmp.path());
        let proposal = ProfileProposal {
            action: ProfileProposalAction::Switch,
            profile_id: "writing".into(),
            label: "Writing".into(),
            reason: None,
            rows: Vec::new(),
            notes: Vec::new(),
            original_input: json!({"profile": "writing"}),
        };

        let outcome = apply_approved_proposal(None, tmp.path(), &proposal);

        let err = outcome.result.expect_err("no session, no switch");
        assert!(err.contains("no longer available"), "{err}");
    }
}
