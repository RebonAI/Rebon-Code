use std::{io, path::Path};

use serde::{Deserialize, Serialize};

const KEY: &str = "firstPromptModelRouting";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionModelSelection {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    #[serde(default)]
    pub manual_override: bool,
}

pub fn load(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> io::Result<Option<SessionModelSelection>> {
    let path = crate::session_storage::session_meta_path(projects_root, cwd, session_id);
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let object: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    object.get(KEY).map(decode).transpose()
}

fn decode(value: &serde_json::Value) -> io::Result<SessionModelSelection> {
    let choice: SessionModelSelection = serde_json::from_value(value.clone())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if choice.provider.is_some() != choice.model.is_some()
        || choice.effort.as_deref().is_some_and(|value| {
            value != "auto" && rebon_types::ReasoningEffort::from_wire_exact(value).is_none()
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid session model selection",
        ));
    }
    Ok(choice)
}

// 开始标记独立于 transcript，失败、取消和 rewind 都不能恢复分类资格。
pub fn claim(projects_root: &Path, cwd: &str, session_id: &str) -> io::Result<bool> {
    let mut claimed = false;
    crate::update_session_metadata(projects_root, cwd, session_id, |object| {
        if !object.contains_key(KEY) {
            object.insert(KEY.into(), serde_json::json!({}));
            claimed = true;
        }
    })?;
    Ok(claimed)
}

// 手动设置可以与分类并发；只允许替换自己读到的版本。
pub fn compare_exchange(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    expected: &SessionModelSelection,
    selected: &SessionModelSelection,
) -> io::Result<bool> {
    let value = serde_json::json!(selected);
    let mut changed = false;
    let mut decode_error = None;
    crate::update_session_metadata(projects_root, cwd, session_id, |object| {
        match object.get(KEY).map(decode).transpose() {
            Ok(Some(current)) if &current == expected => {
                object.insert(KEY.into(), value);
                changed = true;
            }
            Ok(_) => {}
            Err(error) => decode_error = Some(error),
        }
    })?;
    match decode_error {
        Some(error) => Err(error),
        None => Ok(changed),
    }
}

pub fn save_manual_model(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    provider: &str,
    model: &str,
) -> io::Result<()> {
    update_existing(projects_root, cwd, session_id, |choice| {
        choice.provider = Some(provider.into());
        choice.model = Some(model.into());
    })
}

pub fn save_manual_effort(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    effort: Option<rebon_types::ReasoningEffort>,
) -> io::Result<()> {
    update_existing(projects_root, cwd, session_id, |choice| {
        choice.effort = Some(effort.map_or("auto", |level| level.as_str()).into());
    })
}

// 分类期间 IPC 可能还没有解析 provider；先废弃自动结果，模型由原有 host 状态在下轮安装。
pub fn supersede_pending_route(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
) -> io::Result<()> {
    update_existing(projects_root, cwd, session_id, |_| {})
}

fn update_existing(
    projects_root: &Path,
    cwd: &str,
    session_id: &str,
    mutate: impl FnOnce(&mut SessionModelSelection),
) -> io::Result<()> {
    if load(projects_root, cwd, session_id)?.is_none() {
        return Ok(());
    }
    let mut decode_error = None;
    crate::update_session_metadata(projects_root, cwd, session_id, |object| {
        let Some(value) = object.get(KEY) else { return };
        match decode(value) {
            Ok(mut choice) => {
                mutate(&mut choice);
                choice.manual_override = true;
                object.insert(KEY.into(), serde_json::json!(choice));
            }
            Err(error) => decode_error = Some(error),
        }
    })?;
    match decode_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
