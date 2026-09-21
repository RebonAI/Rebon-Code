//! Read-modify-write of a project's own settings files.
//!
//! `<cwd>/.rebon/settings.json` is the project's checked-in settings and
//! `<cwd>/.rebon/settings.local.json` its gitignored companion. Both are
//! edited the same way: read the object, change one key, write it back with
//! every other key intact — a person's hand-written settings must survive a
//! dialog's answer.
//!
//! This is file IO, not a screen. It lives outside `tui` because two callers
//! that are not the terminal need it: the permission policy persists an
//! "allow always" answer here (`session::permission_policy`), and the same
//! answer arrives from the desktop app and from ACP, where no preflight
//! screen has ever run. `rebon-config` owns the *user's* config home, so it
//! is not this file's home; what is here is exactly the project half.

use std::path::Path;

use rebon_permissions::{rule_value::permission_rule_value_to_string, types::PermissionRuleValue};

pub fn load_local_settings_object(
    cwd: &Path,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    load_settings_object(&cwd.join(".rebon").join("settings.local.json"))
}

pub fn write_local_settings_object(
    cwd: &Path,
    settings: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    write_settings_object(&cwd.join(".rebon").join("settings.local.json"), settings)
}

fn load_project_settings_object(
    cwd: &Path,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    load_settings_object(&cwd.join(".rebon").join("settings.json"))
}

fn write_project_settings_object(
    cwd: &Path,
    settings: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    write_settings_object(&cwd.join(".rebon").join("settings.json"), settings)
}

/// A settings file that is missing, or holds something other than an object,
/// reads as an empty object: the caller is about to write one key, and
/// refusing to do that because the file is not there yet would mean no
/// project could ever record its first answer.
fn load_settings_object(path: &Path) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::Map::new());
        }
        Err(err) => return Err(err.into()),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    match value {
        serde_json::Value::Object(object) => Ok(object),
        _ => Ok(serde_json::Map::new()),
    }
}

fn write_settings_object(
    path: &Path,
    settings: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_string_pretty(settings)?;
    std::fs::write(path, format!("{payload}\n"))?;
    Ok(())
}

/// Record an "allow always" answer in the project's settings, so the rule
/// outlives the session that answered.
pub(crate) fn add_project_permission_allow_rule(
    cwd: &Path,
    rule_value: &PermissionRuleValue,
) -> anyhow::Result<()> {
    let mut settings = load_project_settings_object(cwd)?;
    let permissions = settings
        .entry("permissions")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !permissions.is_object() {
        *permissions = serde_json::Value::Object(serde_json::Map::new());
    }
    let permissions_obj = permissions
        .as_object_mut()
        .expect("permissions must be object after normalization");
    merge_string_list(
        permissions_obj,
        "allow",
        &[permission_rule_value_to_string(rule_value)],
    );
    write_project_settings_object(cwd, &settings)
}

/// Add to a string-list key without disturbing what is already in it, and
/// without adding an entry twice — answering the same question twice must not
/// grow the file.
pub fn merge_string_list(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    additions: &[String],
) {
    let mut current = settings
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for item in additions {
        if !current.iter().any(|existing| existing == item) {
            current.push(item.clone());
        }
    }
    settings.insert(
        key.to_string(),
        serde_json::Value::Array(current.into_iter().map(serde_json::Value::String).collect()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_project_permission_allow_rule_merges_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let rebon_dir = dir.path().join(".rebon");
        std::fs::create_dir_all(&rebon_dir).unwrap();
        std::fs::write(
            rebon_dir.join("settings.json"),
            r#"{"permissions":{"allow":["Read(src/**)"]}}"#,
        )
        .unwrap();

        let rule = PermissionRuleValue::new("Read", Some("src/**"));
        add_project_permission_allow_rule(dir.path(), &rule).unwrap();
        add_project_permission_allow_rule(dir.path(), &rule).unwrap();

        let written = std::fs::read_to_string(rebon_dir.join("settings.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            json.get("permissions")
                .and_then(|value| value.get("allow"))
                .and_then(serde_json::Value::as_array)
                .unwrap(),
            &vec![serde_json::Value::String("Read(src/**)".into())]
        );
    }
}
