//! Named profiles — `~/.rebon/profiles/<id>.json`, one file per profile.
//!
//! A profile bundles the things that together make up
//! a working mode: which provider and model the session runs on, which agent
//! backend runs it, the permission mode, and how far the tool surface is
//! narrowed. Every one of those was already switchable inside a live session —
//! what was missing is a name for the combination, so entering "writing mode"
//! is one command instead of four typed in the right order with nothing
//! checking that the fourth one landed.
//!
//! # An omitted field changes nothing
//!
//! Every field is optional, and leaving one out means **that surface is left
//! exactly as it is** — not reset to a default. This is the same rule the
//! per-provider `modelProfiles` table settled on: following is what not
//! declaring means, and there is no sentinel that spells it. So a profile
//! naming only `permissionMode` is a permission-mode profile, and applying it
//! must not quietly move the model out from under the user.
//!
//! # Storage
//!
//! The file name (minus `.json`) is the profile's id, exactly as in
//! [`super::provider_store`], and for the same reason: `displayName` is meant
//! to be edited, so it cannot also be the thing other files point at.
//!
//! # What this module deliberately does not do
//!
//! It never *applies* anything. Applying reaches a live session — its runtime
//! model, its agent backend, its tool-filter handle — and that lives in the
//! front end. Here we only store, read back, and check; that keeps the
//! validation ([`validate`]) and the bypass gate
//! ([`requires_explicit_confirmation`]) shared by every front end that grows a
//! profile surface, rather than re-decided in each one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    config_home_dir, get_active_custom_provider_name_from, list_custom_providers_from,
    provider_model_options, store_file_id,
};

pub const PROFILE_STORE_DIR: &str = "profiles";
pub const PROFILE_SCHEMA_VERSION: u32 = 1;

/// One saved profile.
///
/// `id` is the file name and is not serialized — writing it into the file too
/// would create a second place for it to disagree with reality after a
/// rename on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(skip)]
    pub id: String,
    /// Human-facing name. Free to change: nothing addresses a profile by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Provider to activate, by the name `/provider list` shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model to run on that provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Agent backend — `local` for Rebon's own engine, or a configured
    /// external agent's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Permission mode, in its wire spelling (`default`, `plan`,
    /// `acceptEdits`, `auto`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// How far to narrow the session's tool surface.
    ///
    /// Reuses [`rebon_types::ToolFilterSpec`] rather than inventing a second
    /// spelling of the same thing — this is the shape `ToolFilter` already
    /// serializes to everywhere else in the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<rebon_types::ToolFilterSpec>,
}

fn default_schema_version() -> u32 {
    PROFILE_SCHEMA_VERSION
}

impl Profile {
    /// A profile that declares nothing, under `id`.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            schema_version: PROFILE_SCHEMA_VERSION,
            id: id.into(),
            ..Self::default()
        }
    }

    /// What to call this profile on screen.
    pub fn label(&self) -> &str {
        self.display_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.id)
    }

    /// Whether the profile declares nothing at all to apply.
    ///
    /// Worth refusing at the point of saving: a profile that changes nothing
    /// reports success on every switch while leaving the session untouched,
    /// which reads exactly like a switch that silently failed.
    pub fn declares_nothing(&self) -> bool {
        self.provider.is_none()
            && self.model.is_none()
            && self.agent.is_none()
            && self.permission_mode.is_none()
            && self.tools.is_none()
    }
}

pub fn profile_store_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(PROFILE_STORE_DIR)
}

pub fn profile_file_path(config_dir: &Path, id: &str) -> PathBuf {
    profile_store_dir(config_dir).join(format!("{id}.json"))
}

/// Derive a profile's file-name id from a typed name.
pub fn profile_id(name: &str) -> String {
    store_file_id(name, "profile")
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// Every stored profile, ordered by id.
///
/// A file that will not parse is skipped with a warning rather than failing
/// the listing: one hand-edited profile with a stray comma must not make the
/// other five unreachable.
pub fn list(config_dir: &Path) -> Vec<Profile> {
    let dir = profile_store_dir(config_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut profiles = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        match read_profile_file(&path) {
            Ok(profile) => profiles.push(profile),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "skipping unreadable profile");
            }
        }
    }
    profiles.sort_by(|a, b| a.id.cmp(&b.id));
    profiles
}

/// Load one profile by id or display name, case-insensitively.
///
/// Both spellings resolve because both are what a user has in front of them:
/// the id is what `/profile list` prints on the left, the display name is what
/// they typed when saving it.
pub fn load(config_dir: &Path, name: &str) -> anyhow::Result<Profile> {
    let requested = name.trim();
    if requested.is_empty() {
        anyhow::bail!("Name a profile. `/profile list` shows the saved ones.");
    }
    let direct = profile_file_path(config_dir, &profile_id(requested));
    if direct.is_file() {
        return read_profile_file(&direct);
    }
    let profiles = list(config_dir);
    profiles
        .into_iter()
        .find(|profile| {
            profile.id.eq_ignore_ascii_case(requested)
                || profile
                    .display_name
                    .as_deref()
                    .is_some_and(|display| display.eq_ignore_ascii_case(requested))
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Profile \"{requested}\" not found. `/profile list` shows the saved ones."
            )
        })
}

fn read_profile_file(path: &Path) -> anyhow::Result<Profile> {
    let bytes = std::fs::read(path)
        .map_err(|err| anyhow::Error::from(err).context(format!("read {path:?}")))?;
    let mut profile: Profile = serde_json::from_slice(&bytes)
        .map_err(|err| anyhow::anyhow!("parse {}: {err}", path.display()))?;
    profile.id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string();
    Ok(profile)
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Write a profile, creating the store directory on first use.
pub fn save(config_dir: &Path, profile: &Profile) -> anyhow::Result<()> {
    if profile.id.trim().is_empty() {
        anyhow::bail!("a profile needs an id");
    }
    if profile.declares_nothing() {
        anyhow::bail!(
            "Profile \"{}\" would change nothing. Give it at least one of provider, model, agent, permission mode, or tools.",
            profile.label()
        );
    }
    let dir = profile_store_dir(config_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|err| anyhow::Error::from(err).context(format!("create {dir:?}")))?;
    let path = profile_file_path(config_dir, &profile.id);
    let serialized = serde_json::to_vec_pretty(profile)
        .map_err(|err| anyhow::anyhow!("serialize profile {path:?}: {err}"))?;
    // Same staged write as the provider store: a profile half-written by an
    // interrupted save is one that no longer parses, and the listing would
    // then step over it silently.
    rebon_session::write_file_atomically(&path, &serialized)
        .map_err(|err| anyhow::Error::from(err).context(format!("write {path:?}")))?;
    Ok(())
}

/// Delete a profile. Returns whether one was there.
pub fn remove(config_dir: &Path, name: &str) -> anyhow::Result<bool> {
    let profile = match load(config_dir, name) {
        Ok(profile) => profile,
        Err(_) => return Ok(false),
    };
    let path = profile_file_path(config_dir, &profile.id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(anyhow::Error::from(err).context(format!("remove {path:?}"))),
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// One reason a profile would not apply cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileIssue {
    /// Which field the problem is in, in its stored spelling.
    pub field: &'static str,
    pub message: String,
}

/// Check a profile against what is actually configured.
///
/// This is the reason the provider store is a prerequisite: a profile naming
/// a provider that has since been removed, or a
/// model that provider does not serve, can be caught here — before the switch
/// — instead of surfacing as a 401 on the user's next message, several
/// interactions after the thing that caused it.
///
/// Returns every problem rather than the first, so one `/profile show` reports
/// the whole repair list.
pub fn validate(config_dir: &Path, profile: &Profile) -> Vec<ProfileIssue> {
    let mut issues = Vec::new();
    let providers = list_custom_providers_from(config_dir);

    let matched_provider = profile.provider.as_deref().map(str::trim).and_then(|name| {
        let found = providers
            .iter()
            .find(|provider| provider.name.eq_ignore_ascii_case(name));
        if found.is_none() {
            issues.push(ProfileIssue {
                field: "provider",
                message: format!(
                    "Provider \"{name}\" is not configured. `/provider list` shows the ones that are."
                ),
            });
        }
        found
    });

    if let Some(model) = profile.model.as_deref().map(str::trim) {
        // A model is only checkable against a provider. Which provider that is
        // depends on whether the profile brings its own: one that switches
        // provider and model together must be checked against the provider it
        // is switching *to*, not the one that happens to be active now.
        let provider = matched_provider.or_else(|| {
            if profile.provider.is_some() {
                // Named a provider we could not find — already reported, and
                // checking the model against some other provider would be a
                // second complaint about the same mistake.
                return None;
            }
            get_active_custom_provider_name_from(config_dir).and_then(|active| {
                providers
                    .iter()
                    .find(|provider| provider.name.eq_ignore_ascii_case(&active))
            })
        });
        match provider {
            Some(provider) => {
                let known = provider_model_options(provider);
                // Old provider entries carry only `model` and no list at all.
                // Refusing a model there would reject every profile written
                // against them, so an empty list means "cannot tell".
                if !known.is_empty()
                    && !known
                        .iter()
                        .any(|option| option.eq_ignore_ascii_case(model))
                {
                    issues.push(ProfileIssue {
                        field: "model",
                        message: format!(
                            "Provider \"{}\" does not list model \"{model}\". It serves: {}.",
                            provider.name,
                            known.join(", ")
                        ),
                    });
                }
            }
            None if profile.provider.is_none() => {
                issues.push(ProfileIssue {
                    field: "model",
                    message: format!(
                        "No provider is active, so \"{model}\" cannot be checked. Give the profile a provider too."
                    ),
                });
            }
            None => {}
        }
    }

    if let Some(mode) = profile.permission_mode.as_deref().map(str::trim) {
        if parse_permission_mode(mode).is_none() {
            issues.push(ProfileIssue {
                field: "permissionMode",
                message: format!(
                    "\"{mode}\" is not a permission mode. Known modes: {}.",
                    KNOWN_PERMISSION_MODES.join(", ")
                ),
            });
        }
    }

    if let Some(tools) = profile.tools.as_ref() {
        if tools.allow.as_ref().is_some_and(|allow| allow.is_empty()) {
            issues.push(ProfileIssue {
                field: "tools",
                message: "An empty allow list hides every tool. Remove `tools` to leave the tool surface alone, or name the tools to keep.".into(),
            });
        }
    }

    issues
}

/// The permission modes a profile may name, in the spelling it stores.
pub const KNOWN_PERMISSION_MODES: &[&str] = &[
    "default",
    "plan",
    "acceptEdits",
    "auto",
    "dontAsk",
    "bypassPermissions",
];

/// Parse a stored permission mode, rejecting anything unknown.
///
/// Deliberately not [`rebon_permissions::PermissionMode::from_wire`] on its
/// own: that one answers `Default` for any string it does not know, so a typo
/// in a profile would silently apply the *default* mode and report success.
/// Matching against the known spellings first is what turns that fallback back
/// into a rejection.
pub fn parse_permission_mode(value: &str) -> Option<rebon_permissions::PermissionMode> {
    let requested = value.trim();
    KNOWN_PERMISSION_MODES
        .iter()
        .find(|known| known.eq_ignore_ascii_case(requested))
        .map(|known| rebon_permissions::PermissionMode::from_wire(known))
}

/// Whether applying this profile needs a confirmation beyond naming it.
///
/// A profile that silently carries `bypassPermissions` into effect is ruled
/// out, and that has to be enforced somewhere both front ends pass through.
/// `Some(reason)` means: do not apply until the user
/// has said this specific thing out loud.
pub fn requires_explicit_confirmation(profile: &Profile) -> Option<String> {
    let mode = profile.permission_mode.as_deref()?.trim();
    matches!(
        parse_permission_mode(mode),
        Some(rebon_permissions::PermissionMode::BypassPermissions)
    )
    .then(|| {
        format!(
            "Profile \"{}\" turns permission prompts off entirely (bypassPermissions). \
             A profile does not get to do that just by being named.",
            profile.label()
        )
    })
}

// ---------------------------------------------------------------------------
// Convenience wrappers over the real config home
// ---------------------------------------------------------------------------

pub fn list_profiles() -> Vec<Profile> {
    list(&config_home_dir())
}

pub fn load_profile(name: &str) -> anyhow::Result<Profile> {
    load(&config_home_dir(), name)
}

pub fn save_profile(profile: &Profile) -> anyhow::Result<()> {
    save(&config_home_dir(), profile)
}

pub fn remove_profile(name: &str) -> anyhow::Result<bool> {
    remove(&config_home_dir(), name)
}

pub fn validate_profile(profile: &Profile) -> Vec<ProfileIssue> {
    validate(&config_home_dir(), profile)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use tempfile::TempDir;

    use super::*;

    fn write_config(dir: &Path, contents: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(super::super::config_json_path(dir), contents).unwrap();
    }

    fn provider_fixture(dir: &Path) {
        write_config(
            dir,
            r#"{
                "activeCustomProvider":"vendor",
                "customProviders":[
                    {"name":"vendor","format":"openai","baseUrl":"https://example.com",
                     "apiKey":"sk","model":"vendor-pro",
                     "models":["vendor-pro","vendor-flash"]}
                ]
            }"#,
        );
    }

    fn writing_profile() -> Profile {
        Profile {
            display_name: Some("Writing".into()),
            provider: Some("vendor".into()),
            model: Some("vendor-flash".into()),
            permission_mode: Some("acceptEdits".into()),
            ..Profile::new("writing")
        }
    }

    #[test]
    fn a_profile_round_trips_through_the_store() {
        let tmp = TempDir::new().unwrap();
        let profile = writing_profile();

        save(tmp.path(), &profile).unwrap();
        let loaded = load(tmp.path(), "writing").unwrap();

        assert_eq!(loaded, profile);
        // The display name reaches the same file, so a user who only
        // remembers what they called it still gets there.
        assert_eq!(load(tmp.path(), "Writing").unwrap().id, "writing");
        assert_eq!(list(tmp.path()).len(), 1);
    }

    #[test]
    fn an_omitted_field_is_absent_on_disk_rather_than_stored_as_a_default() {
        // The whole semantic rests on this: "leave that surface alone" has to
        // be an absence, because any stored value would be a second way to
        // say it and the two would drift.
        let tmp = TempDir::new().unwrap();
        save(tmp.path(), &writing_profile()).unwrap();

        let raw = std::fs::read_to_string(profile_file_path(tmp.path(), "writing")).unwrap();

        assert!(!raw.contains("agent"), "{raw}");
        assert!(!raw.contains("tools"), "{raw}");
        assert!(raw.contains("acceptEdits"), "{raw}");
        // And it reads back as an absence, not as some default.
        let loaded = load(tmp.path(), "writing").unwrap();
        assert_eq!(loaded.agent, None);
        assert_eq!(loaded.tools, None);
    }

    #[test]
    fn a_profile_that_would_change_nothing_is_refused() {
        let tmp = TempDir::new().unwrap();
        let empty = Profile {
            display_name: Some("Nothing".into()),
            description: Some("just a label".into()),
            ..Profile::new("nothing")
        };

        assert!(save(tmp.path(), &empty).is_err());
        assert!(list(tmp.path()).is_empty());
    }

    #[test]
    fn removing_reports_whether_anything_was_there() {
        let tmp = TempDir::new().unwrap();
        save(tmp.path(), &writing_profile()).unwrap();

        assert!(remove(tmp.path(), "writing").unwrap());
        assert!(!remove(tmp.path(), "writing").unwrap());
        assert!(list(tmp.path()).is_empty());
    }

    #[test]
    fn an_unparseable_profile_does_not_hide_the_others() {
        let tmp = TempDir::new().unwrap();
        save(tmp.path(), &writing_profile()).unwrap();
        std::fs::write(profile_file_path(tmp.path(), "broken"), "{ not json").unwrap();

        let listed = list(tmp.path());

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "writing");
    }

    #[test]
    fn validation_catches_a_provider_and_model_that_are_no_longer_there() {
        let tmp = TempDir::new().unwrap();
        provider_fixture(tmp.path());

        assert!(validate(tmp.path(), &writing_profile()).is_empty());

        let gone = Profile {
            provider: Some("ghost".into()),
            ..writing_profile()
        };
        let issues = validate(tmp.path(), &gone);
        // One complaint, about the provider. The model is not checked against
        // some other provider just to have something to say.
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].field, "provider");

        let wrong_model = Profile {
            model: Some("vendor-ultra".into()),
            ..writing_profile()
        };
        let issues = validate(tmp.path(), &wrong_model);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].field, "model");
        assert!(issues[0].message.contains("vendor-pro"), "{issues:?}");
    }

    #[test]
    fn a_model_only_profile_is_checked_against_the_active_provider() {
        let tmp = TempDir::new().unwrap();
        provider_fixture(tmp.path());

        let ok = Profile {
            provider: None,
            model: Some("vendor-flash".into()),
            ..Profile::new("cheap")
        };
        assert!(validate(tmp.path(), &ok).is_empty());

        let bad = Profile {
            model: Some("someone-elses-model".into()),
            ..ok
        };
        assert_eq!(validate(tmp.path(), &bad)[0].field, "model");
    }

    #[test]
    fn validation_rejects_a_mistyped_permission_mode_instead_of_defaulting_it() {
        let tmp = TempDir::new().unwrap();
        provider_fixture(tmp.path());
        let typo = Profile {
            permission_mode: Some("acceptedits ".into()),
            ..writing_profile()
        };
        // Case and padding are fine — that spelling is the real mode.
        assert!(validate(tmp.path(), &typo).is_empty());

        let wrong = Profile {
            permission_mode: Some("acceptEdit".into()),
            ..writing_profile()
        };
        let issues = validate(tmp.path(), &wrong);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].field, "permissionMode");
    }

    #[test]
    fn validation_rejects_an_allow_list_that_hides_everything() {
        let tmp = TempDir::new().unwrap();
        provider_fixture(tmp.path());
        let silent = Profile {
            tools: Some(rebon_types::ToolFilterSpec {
                allow: Some(BTreeSet::new()),
                deny: BTreeSet::new(),
            }),
            ..writing_profile()
        };

        assert_eq!(validate(tmp.path(), &silent)[0].field, "tools");
    }

    #[test]
    fn bypass_permissions_needs_saying_out_loud() {
        let bypass = Profile {
            permission_mode: Some("bypassPermissions".into()),
            ..writing_profile()
        };

        assert!(requires_explicit_confirmation(&bypass).is_some());
        // Every other mode applies on the strength of the user naming the
        // profile — this gate is about turning the prompts off, not about
        // changing modes.
        assert!(requires_explicit_confirmation(&writing_profile()).is_none());
        assert!(requires_explicit_confirmation(&Profile {
            permission_mode: Some("auto".into()),
            ..writing_profile()
        })
        .is_none());
    }

    #[test]
    fn profile_ids_are_safe_file_names() {
        assert_eq!(profile_id("Writing Mode"), "writing-mode");
        assert_eq!(profile_id("deep/review"), "deep-review");
        assert_eq!(profile_id("  "), "profile");
    }
}
