//! `/profile set <name> <field> <value>` — change one field and leave the rest
//! of the file alone.
//!
//! The gap this closes: [`crate::save_current_session`] always captures all
//! five surfaces off the live session, so it can neither change one field
//! without dragging the other four along nor produce a profile that declares
//! only one or two of them — and "declares one or two" is the shape
//! the profile design is built around. It was also the one thing the model
//! could do (`ProfileSave` takes any subset) and the user could not.

use std::path::Path;

use rebon_config::profile_store::{self, Profile};

use crate::apply::ProfileSession;
use crate::render::{declared_lines, set_usage_text, unapplied_notes, unchanged_note};
use crate::ProfileCommandResult;

/// A field `/profile set` can write, and the vocabulary that reaches it.
///
/// Kept as a type rather than as string comparisons because
/// [`Self::stored_name`] has to line up exactly with
/// [`profile_store::ProfileIssue::field`]: that is what lets a set be refused
/// for a complaint about the field the user just typed, and only that one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileField {
    Provider,
    Model,
    Agent,
    PermissionMode,
    Tools,
    Description,
    DisplayName,
}

impl ProfileField {
    pub const ALL: &'static [ProfileField] = &[
        ProfileField::Provider,
        ProfileField::Model,
        ProfileField::Agent,
        ProfileField::PermissionMode,
        ProfileField::Tools,
        ProfileField::Description,
        ProfileField::DisplayName,
    ];

    pub fn parse(word: &str) -> Option<Self> {
        // `-` and `_` are eaten so `permission-mode`, `permission_mode` and
        // `permissionMode` are one field rather than three near-misses.
        let key: String = word
            .trim()
            .to_lowercase()
            .chars()
            .filter(|ch| *ch != '-' && *ch != '_')
            .collect();
        Some(match key.as_str() {
            "provider" => Self::Provider,
            "model" => Self::Model,
            "agent" | "backend" => Self::Agent,
            "permission" | "permissions" | "permissionmode" | "mode" => Self::PermissionMode,
            "tool" | "tools" => Self::Tools,
            "description" | "desc" | "note" => Self::Description,
            "name" | "displayname" | "label" => Self::DisplayName,
            _ => return None,
        })
    }

    /// The field's spelling on disk — also how the profile store names it when
    /// it complains.
    pub fn stored_name(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Model => "model",
            Self::Agent => "agent",
            Self::PermissionMode => "permissionMode",
            Self::Tools => "tools",
            Self::Description => "description",
            Self::DisplayName => "displayName",
        }
    }

    /// Write `value` into `profile`, returning it as it will be stored.
    fn write(self, profile: &mut Profile, value: &str) -> Result<String, String> {
        let value = value.trim();
        match self {
            Self::Provider => profile.provider = Some(value.to_string()),
            Self::Model => profile.model = Some(value.to_string()),
            Self::Agent => profile.agent = Some(value.to_string()),
            Self::Description => profile.description = Some(value.to_string()),
            Self::DisplayName => profile.display_name = Some(value.to_string()),
            Self::PermissionMode => {
                let mode = profile_store::parse_permission_mode(value).ok_or_else(|| {
                    format!(
                        "\"{value}\" is not a permission mode. Known modes: {}.",
                        profile_store::KNOWN_PERMISSION_MODES.join(", ")
                    )
                })?;
                // Stored canonically rather than as typed. The file is read
                // back by `/profile show` and by whatever grows a profile
                // surface next, and `acceptedits` would render as a mode
                // nobody can name.
                let wire = mode.as_wire().to_string();
                profile.permission_mode = Some(wire.clone());
                return Ok(wire);
            }
            Self::Tools => {
                let spec = parse_tools_value(value)?;
                let described = rebon_tool::ToolFilter::from_spec(spec.clone()).describe_allowed();
                profile.tools = Some(spec);
                return Ok(described);
            }
        }
        Ok(value.to_string())
    }

    fn clear(self, profile: &mut Profile) {
        match self {
            Self::Provider => profile.provider = None,
            Self::Model => profile.model = None,
            Self::Agent => profile.agent = None,
            Self::PermissionMode => profile.permission_mode = None,
            Self::Tools => profile.tools = None,
            Self::Description => profile.description = None,
            Self::DisplayName => profile.display_name = None,
        }
    }
}

/// Words that mean "stop declaring this field".
///
/// `follow` is `/provider profile`'s word for the same act and is kept so the
/// two surfaces read alike. `default` is deliberately *not* here, unlike over
/// there: `default` is a real permission mode, and a word that clears a field
/// on one line and sets it on the next is worse than no shorthand at all.
const CLEAR_WORDS: &[&str] = &["-", "follow", "inherit", "clear", "unset"];

pub fn is_clear_word(value: &str) -> bool {
    CLEAR_WORDS
        .iter()
        .any(|word| word.eq_ignore_ascii_case(value.trim()))
}

/// Read a tool surface off the command line.
///
/// `Read,Edit,Grep` is an allow list; a `-` or `!` prefix moves an entry to
/// the deny list, so `Read,Edit,-Bash` and a bare `-Bash` are both sayable.
/// A list with no allow entries leaves `allow` absent rather than empty —
/// "take Bash away" is not "keep nothing but Bash".
pub fn parse_tools_value(value: &str) -> Result<rebon_types::ToolFilterSpec, String> {
    let mut allow = std::collections::BTreeSet::new();
    let mut deny = std::collections::BTreeSet::new();
    for token in value
        .split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        match token.strip_prefix(['-', '!']) {
            Some(name) if !name.is_empty() => deny.insert(name.to_string()),
            Some(_) => {
                return Err(format!(
                    "\"{token}\" names no tool. Write `-Bash` to remove one, or `-` on its own to stop declaring tools at all."
                ))
            }
            None => allow.insert(token.to_string()),
        };
    }
    if allow.is_empty() && deny.is_empty() {
        return Err(
            "Name the tools to keep (`Read,Edit,Grep`), or the ones to remove (`-Bash`)."
                .to_string(),
        );
    }
    Ok(rebon_types::ToolFilterSpec {
        allow: (!allow.is_empty()).then_some(allow),
        deny,
    })
}

/// Write one field of one profile.
///
/// # Naming a profile that is not there creates it
///
/// Refusing would have left "write a profile that declares only a model and a
/// tool surface" with no route at all: `/profile save` cannot express it, and
/// this command would then only be able to trim what `save` had already
/// over-captured. The reply says "Created" rather than "Updated" so a typo'd
/// name is visible on the line that made it.
pub fn set_profile_field(
    config_dir: &Path,
    session: &dyn ProfileSession,
    name: &str,
    field: &str,
    value: &str,
) -> ProfileCommandResult {
    if name.trim().is_empty() || field.trim().is_empty() {
        return ProfileCommandResult::err(set_usage_text());
    }
    let Some(field) = ProfileField::parse(field) else {
        return ProfileCommandResult::err(format!(
            "\"{field}\" is not a profile field. Settable: {}.\n\n{}",
            ProfileField::ALL
                .iter()
                .map(|field| field.stored_name())
                .collect::<Vec<_>>()
                .join(", "),
            set_usage_text()
        ));
    };
    if value.trim().is_empty() {
        return ProfileCommandResult::err(format!(
            "Give `{}` a value, or `-` to stop declaring it.\n\n{}",
            field.stored_name(),
            set_usage_text()
        ));
    }

    let existing = profile_store::load(config_dir, name).ok();
    let created = existing.is_none();
    let clearing = is_clear_word(value);
    if created && clearing {
        return ProfileCommandResult::err(format!(
            "Profile \"{}\" does not exist, so there is no `{}` on it to clear. `/profile list` shows the saved ones.",
            name.trim(),
            field.stored_name()
        ));
    }
    let mut profile = existing.unwrap_or_else(|| Profile {
        display_name: Some(name.trim().to_string()),
        ..Profile::new(profile_store::profile_id(name))
    });

    let written = if clearing {
        field.clear(&mut profile);
        None
    } else {
        match field.write(&mut profile, value) {
            Ok(stored) => Some(stored),
            Err(message) => {
                return ProfileCommandResult::err(format!("{message}\n\nNothing was written."))
            }
        }
    };

    if profile.declares_nothing() {
        return ProfileCommandResult::err(if clearing {
            format!(
                "Clearing `{}` would leave \"{}\" declaring nothing, and a profile that declares nothing reports success on every switch while changing nothing.\n\nDelete it with /profile remove {}, or declare another surface first.",
                field.stored_name(),
                profile.label(),
                profile.id
            )
        } else {
            format!(
                "`{}` is a label, not a surface, so \"{}\" would still declare nothing to apply. Give it one of provider, model, agent, permissionMode or tools too.",
                field.stored_name(),
                profile.label()
            )
        });
    }

    // Checked against what is actually configured, but only the complaint
    // about *this* field refuses the write. The rest are reported and left in
    // the file, because the profile is being edited one field at a time and
    // moving a profile from one provider to another has to pass through a
    // moment where the model belongs to the old one.
    let issues = profile_store::validate(config_dir, &profile);
    if let Some(issue) = issues
        .iter()
        .find(|issue| issue.field == field.stored_name())
    {
        return ProfileCommandResult::err(format!("{}\n\nNothing was written.", issue.message));
    }
    if let Err(err) = profile_store::save(config_dir, &profile) {
        return ProfileCommandResult::err(err.to_string());
    }

    let mut lines = Vec::new();
    if created {
        lines.push(format!("Created profile \"{}\".", profile.label()));
    }
    lines.push(match &written {
        Some(stored) => format!(
            "`{}` on \"{}\" → {stored}",
            field.stored_name(),
            profile.label()
        ),
        None => format!(
            "`{}` on \"{}\" cleared — applying it now leaves that surface as it is.",
            field.stored_name(),
            profile.label()
        ),
    });
    lines.push(String::new());
    lines.extend(declared_lines(&profile));
    let untouched = unchanged_note(&profile);
    if !untouched.is_empty() {
        lines.push(String::new());
        lines.push(untouched.trim_start().to_string());
    }
    lines.extend(unapplied_notes(session, &profile, &issues));
    ProfileCommandResult::ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use tempfile::TempDir;

    use super::*;
    use crate::apply::tests::vendor_config;
    use crate::test_session::FakeSession;

    fn writing() -> Profile {
        Profile {
            display_name: Some("Writing".into()),
            provider: Some("vendor".into()),
            model: Some("vendor-flash".into()),
            permission_mode: Some("acceptEdits".into()),
            ..Profile::new("writing")
        }
    }

    /// Two providers, so a field can be moved out from under another one.
    fn two_vendor_config(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            rebon_config::config_json_path(dir),
            r#"{
                "activeCustomProvider":"vendor",
                "customProviders":[
                    {"name":"vendor","format":"openai","baseUrl":"https://example.com",
                     "apiKey":"sk","model":"vendor-pro",
                     "models":["vendor-pro","vendor-flash"]},
                    {"name":"other","format":"openai","baseUrl":"https://other.example",
                     "apiKey":"sk","model":"other-1","models":["other-1"]}
                ]
            }"#,
        )
        .unwrap();
    }

    fn set(config_dir: &Path, name: &str, field: &str, value: &str) -> ProfileCommandResult {
        set_profile_field(config_dir, &FakeSession::default(), name, field, value)
    }

    #[test]
    fn setting_one_field_leaves_every_other_field_in_the_file_alone() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        profile_store::save(tmp.path(), &writing()).unwrap();

        let result = set(tmp.path(), "writing", "model", "vendor-pro");

        assert!(!result.is_err, "{}", result.text);
        let stored = profile_store::load(tmp.path(), "writing").unwrap();
        assert_eq!(stored.model.as_deref(), Some("vendor-pro"));
        // The three fields nobody named are exactly as they were — this is the
        // whole point of `set` over a re-`save`.
        assert_eq!(stored.provider.as_deref(), Some("vendor"));
        assert_eq!(stored.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(stored.display_name.as_deref(), Some("Writing"));
        assert_eq!(stored.agent, None);
        assert_eq!(stored.tools, None);
    }

    /// The canonical example profile — narrow the tools, move the
    /// model, touch nothing else. `/profile save` cannot express it because it
    /// always captures all five surfaces off the live session.
    #[test]
    fn set_can_author_a_profile_that_declares_only_what_it_was_given() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());

        let created = set(tmp.path(), "writing", "model", "vendor-flash");
        assert!(!created.is_err, "{}", created.text);
        assert!(created.text.contains("Created profile"), "{}", created.text);
        let narrowed = set(tmp.path(), "writing", "tools", "Read,Edit,Grep");
        assert!(!narrowed.is_err, "{}", narrowed.text);

        let stored = profile_store::load(tmp.path(), "writing").unwrap();
        assert_eq!(stored.model.as_deref(), Some("vendor-flash"));
        assert_eq!(
            stored.tools.as_ref().unwrap().allow.as_ref().unwrap(),
            &BTreeSet::from(["Read".to_string(), "Edit".to_string(), "Grep".to_string()])
        );
        assert_eq!(stored.provider, None);
        assert_eq!(stored.agent, None);
        assert_eq!(stored.permission_mode, None);
        // And the reply says so, rather than letting "it didn't change my
        // agent" read as a failure.
        assert!(
            narrowed.text.contains("Left as they were"),
            "{}",
            narrowed.text
        );
    }

    #[test]
    fn a_clear_word_stops_the_profile_declaring_that_field() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        profile_store::save(
            tmp.path(),
            &Profile {
                agent: Some("local".into()),
                ..writing()
            },
        )
        .unwrap();

        let result = set(tmp.path(), "writing", "agent", "-");

        assert!(!result.is_err, "{}", result.text);
        let stored = profile_store::load(tmp.path(), "writing").unwrap();
        assert_eq!(stored.agent, None);
        assert_eq!(stored.model.as_deref(), Some("vendor-flash"));
        // Absent on disk, not stored as some "follow" sentinel — the absence
        // *is* how "leave it alone" is spelled.
        let raw = std::fs::read_to_string(profile_store::profile_file_path(tmp.path(), "writing"))
            .unwrap();
        assert!(!raw.contains("agent"), "{raw}");
        // `follow` is the other surface's word for the same act.
        profile_store::save(
            tmp.path(),
            &Profile {
                agent: Some("local".into()),
                ..writing()
            },
        )
        .unwrap();
        assert!(!set(tmp.path(), "writing", "agent", "follow").is_err);
        assert_eq!(
            profile_store::load(tmp.path(), "writing").unwrap().agent,
            None
        );
    }

    /// `default` clears a role in `/provider profile`. Here it is a permission
    /// mode, and taking it as a clear word would make one of the six modes
    /// unsayable.
    #[test]
    fn default_sets_the_permission_mode_rather_than_clearing_it() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        profile_store::save(tmp.path(), &writing()).unwrap();

        let result = set(tmp.path(), "writing", "permission", "default");

        assert!(!result.is_err, "{}", result.text);
        assert_eq!(
            profile_store::load(tmp.path(), "writing")
                .unwrap()
                .permission_mode
                .as_deref(),
            Some("default")
        );
    }

    #[test]
    fn a_permission_mode_is_stored_in_its_canonical_spelling() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        profile_store::save(tmp.path(), &writing()).unwrap();

        assert!(!set(tmp.path(), "writing", "permission-mode", "acceptedits").is_err);

        assert_eq!(
            profile_store::load(tmp.path(), "writing")
                .unwrap()
                .permission_mode
                .as_deref(),
            Some("acceptEdits")
        );
    }

    #[test]
    fn clearing_the_last_declared_field_points_at_remove_instead() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        let only_mode = Profile {
            permission_mode: Some("plan".into()),
            ..Profile::new("planning")
        };
        profile_store::save(tmp.path(), &only_mode).unwrap();

        let result = set(tmp.path(), "planning", "permission", "-");

        assert!(result.is_err);
        assert!(result.text.contains("/profile remove"), "{}", result.text);
        // Refused before the write, so the profile is still usable.
        assert_eq!(
            profile_store::load(tmp.path(), "planning")
                .unwrap()
                .permission_mode
                .as_deref(),
            Some("plan")
        );
    }

    #[test]
    fn a_bad_value_for_the_field_being_set_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        profile_store::save(tmp.path(), &writing()).unwrap();

        let unknown_model = set(tmp.path(), "writing", "model", "vendor-ultra");
        assert!(unknown_model.is_err);
        assert!(
            unknown_model.text.contains("vendor-pro"),
            "{}",
            unknown_model.text
        );

        let unknown_mode = set(tmp.path(), "writing", "permission", "acceptEdit");
        assert!(unknown_mode.is_err);

        let unknown_field = set(tmp.path(), "writing", "colour", "blue");
        assert!(unknown_field.is_err);
        assert!(
            unknown_field.text.contains("permissionMode"),
            "{}",
            unknown_field.text
        );

        // Three refusals, and the file is byte-for-byte what it was.
        assert_eq!(
            profile_store::load(tmp.path(), "writing").unwrap(),
            writing()
        );
    }

    /// The other half of that rule: a complaint about a *different* field is
    /// reported, not refused. Moving a profile from one provider to another
    /// has to pass through a moment where the model still belongs to the old
    /// one, and refusing there would leave no way through.
    #[test]
    fn a_complaint_about_another_field_is_reported_rather_than_refused() {
        let tmp = TempDir::new().unwrap();
        two_vendor_config(tmp.path());
        profile_store::save(tmp.path(), &writing()).unwrap();

        let moved = set(tmp.path(), "writing", "provider", "other");

        assert!(!moved.is_err, "{}", moved.text);
        assert!(moved.text.contains("Will not apply"), "{}", moved.text);
        assert!(moved.text.contains("vendor-flash"), "{}", moved.text);
        assert_eq!(
            profile_store::load(tmp.path(), "writing")
                .unwrap()
                .provider
                .as_deref(),
            Some("other")
        );

        // And the way out is the next `set`, which then reports clean.
        let fixed = set(tmp.path(), "writing", "model", "other-1");
        assert!(!fixed.is_err, "{}", fixed.text);
        assert!(!fixed.text.contains("Will not apply"), "{}", fixed.text);
    }

    #[test]
    fn a_tools_value_reads_a_list_and_a_minus_prefix_removes() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        profile_store::save(tmp.path(), &writing()).unwrap();

        assert!(!set(tmp.path(), "writing", "tools", "Read, Edit,-Bash").is_err);
        let spec = profile_store::load(tmp.path(), "writing")
            .unwrap()
            .tools
            .unwrap();
        assert_eq!(
            spec.allow.as_ref().unwrap(),
            &BTreeSet::from(["Read".to_string(), "Edit".to_string()])
        );
        assert_eq!(spec.deny, BTreeSet::from(["Bash".to_string()]));

        // Removals alone leave `allow` absent: "take Bash away" is not "keep
        // nothing but Bash".
        assert!(!set(tmp.path(), "writing", "tools", "-Bash").is_err);
        let spec = profile_store::load(tmp.path(), "writing")
            .unwrap()
            .tools
            .unwrap();
        assert_eq!(spec.allow, None);
        assert_eq!(spec.deny, BTreeSet::from(["Bash".to_string()]));
    }

    #[test]
    fn an_agent_this_session_has_never_heard_of_is_a_note_not_a_refusal() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());
        profile_store::save(tmp.path(), &writing()).unwrap();

        let result = set(tmp.path(), "writing", "agent", "ghost-cli");

        assert!(!result.is_err, "{}", result.text);
        assert!(result.text.contains("ghost-cli"), "{}", result.text);
        assert!(result.text.contains("/backend"), "{}", result.text);
        assert_eq!(
            profile_store::load(tmp.path(), "writing")
                .unwrap()
                .agent
                .as_deref(),
            Some("ghost-cli")
        );
        // The local engine is one this session does know, and says nothing.
        let local = set(tmp.path(), "writing", "agent", "local");
        assert!(!local.text.contains("/backend"), "{}", local.text);
    }

    #[test]
    fn a_label_on_its_own_is_not_a_profile() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());

        let result = set(tmp.path(), "writing", "description", "for prose");

        assert!(result.is_err);
        assert!(result.text.contains("declare nothing"), "{}", result.text);
        assert!(profile_store::list(tmp.path()).is_empty());
    }

    #[test]
    fn clearing_a_field_on_a_profile_that_is_not_there_says_so() {
        let tmp = TempDir::new().unwrap();
        vendor_config(tmp.path());

        let result = set(tmp.path(), "ghost", "model", "-");

        assert!(result.is_err);
        assert!(result.text.contains("does not exist"), "{}", result.text);
        assert!(profile_store::list(tmp.path()).is_empty());
    }
}
