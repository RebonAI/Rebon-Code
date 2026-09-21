//! Durable memory save/delete service for scoped user/repo memory.
#![allow(missing_docs)]

use std::fs;
use std::path::{Component, Path, PathBuf};

use rebon_instructions::frontmatter::parse_frontmatter;
use rebon_session::memory_paths::{self, MemoryScope, ENTRYPOINT_NAME};

const INVALID_SCOPE_MESSAGE: &str = "storage scope must be `user` or `repo`; `task`, `session`, `coordinator`, and `project` are not durable storage scopes. Use scope `repo` with type `project` for project memories.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveMemoryAction {
    Upsert,
    Delete,
}

impl SaveMemoryAction {
    pub fn parse(raw: &str) -> Result<Self, SaveMemoryError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "upsert" => Ok(Self::Upsert),
            "delete" => Ok(Self::Delete),
            other => Err(SaveMemoryError::Validation(format!(
                "unsupported action `{other}`; expected `upsert` or `delete`"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upsert => "upsert",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryContentType {
    User,
    Feedback,
    Project,
    Reference,
}

impl MemoryContentType {
    pub fn parse(raw: &str) -> Result<Self, SaveMemoryError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "user" => Ok(Self::User),
            "feedback" => Ok(Self::Feedback),
            "project" => Ok(Self::Project),
            "reference" => Ok(Self::Reference),
            other => Err(SaveMemoryError::Validation(format!(
                "unsupported memory type `{other}`; expected `user`, `feedback`, `project`, or `reference`"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySource {
    ExplicitUserRequest,
    DurableFeedback,
    CoordinatorSynthesis,
    AssistantInferred,
    ForgetRequest,
}

impl MemorySource {
    pub fn parse(raw: &str) -> Result<Self, SaveMemoryError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "explicit_user_request" => Ok(Self::ExplicitUserRequest),
            "durable_feedback" => Ok(Self::DurableFeedback),
            "coordinator_synthesis" => Ok(Self::CoordinatorSynthesis),
            "assistant_inferred" => Ok(Self::AssistantInferred),
            "forget_request" => Ok(Self::ForgetRequest),
            other => Err(SaveMemoryError::Validation(format!(
                "unsupported source `{other}`; expected one of explicit_user_request, durable_feedback, coordinator_synthesis, assistant_inferred, forget_request"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitUserRequest => "explicit_user_request",
            Self::DurableFeedback => "durable_feedback",
            Self::CoordinatorSynthesis => "coordinator_synthesis",
            Self::AssistantInferred => "assistant_inferred",
            Self::ForgetRequest => "forget_request",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveMemoryRequest {
    pub action: SaveMemoryAction,
    pub scope: MemoryScope,
    pub memory_type: Option<MemoryContentType>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub content: Option<String>,
    pub source: Option<MemorySource>,
    pub reason: Option<String>,
    pub dedupe_key: Option<String>,
    pub target_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveMemoryOutcome {
    pub status: String,
    pub action: SaveMemoryAction,
    pub scope: MemoryScope,
    pub memory_path: Option<PathBuf>,
    pub index_path: PathBuf,
    pub message: String,
    pub dedupe_matched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveMemoryError {
    Validation(String),
    Io(String),
}

impl std::fmt::Display for SaveMemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(message) | Self::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for SaveMemoryError {}

pub fn parse_scope(raw: &str) -> Result<MemoryScope, SaveMemoryError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "user" => Ok(MemoryScope::User),
        "repo" => Ok(MemoryScope::Repo),
        "task" | "session" | "coordinator" | "project" => Err(SaveMemoryError::Validation(
            INVALID_SCOPE_MESSAGE.to_string(),
        )),
        other => Err(SaveMemoryError::Validation(format!(
            "unsupported storage scope `{other}`; {INVALID_SCOPE_MESSAGE}"
        ))),
    }
}

pub fn save_memory(
    cwd: &str,
    input: SaveMemoryRequest,
) -> Result<SaveMemoryOutcome, SaveMemoryError> {
    match input.action {
        SaveMemoryAction::Upsert => upsert_memory(cwd, input),
        SaveMemoryAction::Delete => delete_memory(cwd, input),
    }
}

pub fn delete_memory(
    cwd: &str,
    input: SaveMemoryRequest,
) -> Result<SaveMemoryOutcome, SaveMemoryError> {
    let memory_dir = memory_dir(cwd, input.scope)?;
    let index_path = memory_dir.join(ENTRYPOINT_NAME);

    let target = resolve_delete_target(&memory_dir, &input)?;
    if !target.exists() {
        return Err(SaveMemoryError::Validation(format!(
            "memory file `{}` does not exist",
            target.display()
        )));
    }

    fs::remove_file(&target).map_err(|err| SaveMemoryError::Io(err.to_string()))?;
    remove_index_entries(
        &index_path,
        target.file_name().and_then(|s| s.to_str()).unwrap_or(""),
    )?;

    Ok(SaveMemoryOutcome {
        status: "deleted".to_string(),
        action: SaveMemoryAction::Delete,
        scope: input.scope,
        memory_path: Some(target.clone()),
        index_path,
        message: format!("Deleted durable memory `{}`", target.display()),
        dedupe_matched: input.target_file.is_none(),
    })
}

fn upsert_memory(
    cwd: &str,
    input: SaveMemoryRequest,
) -> Result<SaveMemoryOutcome, SaveMemoryError> {
    let memory_type = input.memory_type.ok_or_else(|| {
        SaveMemoryError::Validation(
            "upsert requires `type` (user, feedback, project, or reference)".to_string(),
        )
    })?;
    let title = required_trimmed(input.title.as_deref(), "title")?;
    let description = required_trimmed(input.description.as_deref(), "description")?;
    let content = required_trimmed(input.content.as_deref(), "content")?;

    let memory_dir = memory_dir(cwd, input.scope)?;
    fs::create_dir_all(&memory_dir).map_err(|err| SaveMemoryError::Io(err.to_string()))?;
    let index_path = memory_dir.join(ENTRYPOINT_NAME);

    let generated_name = format!("{}-{}.md", memory_type.as_str(), slugify(&title));
    let target_name = match input.target_file.as_deref() {
        Some(raw) => validate_target_file(raw)?,
        None => match find_dedupe_match(
            &memory_dir,
            input.dedupe_key.as_deref(),
            &title,
            &generated_name,
        )? {
            DedupeMatch::None => generated_name.clone(),
            DedupeMatch::One(name) => name,
            DedupeMatch::Many(matches) => {
                return Err(SaveMemoryError::Validation(format!(
                    "ambiguous duplicate memories matched: {}; provide target_file",
                    matches.join(", ")
                )))
            }
        },
    };
    let dedupe_matched = target_name != generated_name || memory_dir.join(&target_name).exists();
    let memory_path = memory_dir.join(&target_name);
    ensure_child(&memory_dir, &memory_path)?;

    let markdown = render_memory_file(
        &title,
        &description,
        memory_type,
        input.scope,
        input.dedupe_key.as_deref(),
        input.source,
        &content,
    );
    fs::write(&memory_path, markdown).map_err(|err| SaveMemoryError::Io(err.to_string()))?;
    upsert_index_entry(&index_path, &target_name, &title, &description)?;

    Ok(SaveMemoryOutcome {
        status: if dedupe_matched { "updated" } else { "created" }.to_string(),
        action: SaveMemoryAction::Upsert,
        scope: input.scope,
        memory_path: Some(memory_path.clone()),
        index_path,
        message: format!(
            "Saved durable memory `{title}` to `{}`",
            memory_path.display()
        ),
        dedupe_matched,
    })
}

fn memory_dir(cwd: &str, scope: MemoryScope) -> Result<PathBuf, SaveMemoryError> {
    memory_paths::memory_dir_for_scope(scope, Some(cwd)).ok_or_else(|| {
        SaveMemoryError::Validation(
            "could not resolve memory directory for requested scope".to_string(),
        )
    })
}

fn resolve_delete_target(
    memory_dir: &Path,
    input: &SaveMemoryRequest,
) -> Result<PathBuf, SaveMemoryError> {
    if let Some(raw) = input.target_file.as_deref() {
        return Ok(memory_dir.join(validate_target_file(raw)?));
    }

    let title = input
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if input
        .dedupe_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
        && title.is_none()
    {
        return Err(SaveMemoryError::Validation(
            "delete requires `target_file`, `dedupe_key`, or `title`".to_string(),
        ));
    }

    match find_dedupe_match(
        memory_dir,
        input.dedupe_key.as_deref(),
        title.unwrap_or(""),
        "",
    )? {
        DedupeMatch::One(name) => Ok(memory_dir.join(name)),
        DedupeMatch::None => Err(SaveMemoryError::Validation(
            "no memory matched delete request".to_string(),
        )),
        DedupeMatch::Many(matches) => Err(SaveMemoryError::Validation(format!(
            "ambiguous delete matched multiple memories: {}; provide target_file",
            matches.join(", ")
        ))),
    }
}

enum DedupeMatch {
    None,
    One(String),
    Many(Vec<String>),
}

fn find_dedupe_match(
    memory_dir: &Path,
    dedupe_key: Option<&str>,
    title: &str,
    generated_name: &str,
) -> Result<DedupeMatch, SaveMemoryError> {
    let dedupe_key = dedupe_key.map(str::trim).filter(|s| !s.is_empty());
    let title = title.trim();
    let mut matches = Vec::new();

    if memory_dir.exists() {
        for entry in fs::read_dir(memory_dir).map_err(|err| SaveMemoryError::Io(err.to_string()))? {
            let entry = entry.map_err(|err| SaveMemoryError::Io(err.to_string()))?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name == ENTRYPOINT_NAME || !name.ends_with(".md") || !path.is_file() {
                continue;
            }
            let raw = fs::read_to_string(&path).unwrap_or_default();
            let frontmatter = parse_frontmatter(&raw).frontmatter;
            let mut matched = false;
            if let Some(key) = dedupe_key {
                matched |= frontmatter
                    .fields
                    .get("dedupe_key")
                    .is_some_and(|v| v == key);
            }
            if !title.is_empty() {
                matched |= frontmatter.name().is_some_and(|v| v == title);
            }
            if matched {
                matches.push(name.to_string());
            }
        }
    }

    if !generated_name.is_empty() && memory_dir.join(generated_name).exists() {
        let generated = generated_name.to_string();
        if !matches.iter().any(|m| m == &generated) {
            matches.push(generated);
        }
    }

    matches.sort();
    matches.dedup();
    Ok(match matches.len() {
        0 => DedupeMatch::None,
        1 => DedupeMatch::One(matches.remove(0)),
        _ => DedupeMatch::Many(matches),
    })
}

fn required_trimmed(value: Option<&str>, field: &str) -> Result<String, SaveMemoryError> {
    let value = value.unwrap_or("").trim();
    if value.is_empty() {
        Err(SaveMemoryError::Validation(format!(
            "upsert requires non-empty `{field}`"
        )))
    } else {
        Ok(value.to_string())
    }
}

pub fn validate_target_file(raw: &str) -> Result<String, SaveMemoryError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SaveMemoryError::Validation(
            "target_file must not be empty".to_string(),
        ));
    }
    if trimmed.eq_ignore_ascii_case(ENTRYPOINT_NAME) {
        return Err(SaveMemoryError::Validation(
            "target_file must not be MEMORY.md (case-insensitive)".to_string(),
        ));
    }
    if !trimmed.ends_with(".md") {
        return Err(SaveMemoryError::Validation(
            "target_file must end with .md".to_string(),
        ));
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains("..") {
        return Err(SaveMemoryError::Validation(
            "target_file must be a basename only with no path separators or `..`".to_string(),
        ));
    }
    if trimmed.starts_with("//") || trimmed.starts_with("\\\\") || has_windows_drive_prefix(trimmed)
    {
        return Err(SaveMemoryError::Validation(
            "target_file must not be absolute, UNC, or Windows drive-prefixed".to_string(),
        ));
    }
    if Path::new(trimmed)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(SaveMemoryError::Validation(
            "target_file must be a simple basename".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

fn has_windows_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

fn ensure_child(parent: &Path, child: &Path) -> Result<(), SaveMemoryError> {
    let joined = normalize_path(child);
    let parent = normalize_path(parent);
    if joined.parent() == Some(parent.as_path()) {
        Ok(())
    } else {
        Err(SaveMemoryError::Validation(
            "resolved memory path escaped memory directory".to_string(),
        ))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(component.as_os_str()),
        }
    }
    out
}

fn render_memory_file(
    title: &str,
    description: &str,
    memory_type: MemoryContentType,
    scope: MemoryScope,
    dedupe_key: Option<&str>,
    source: Option<MemorySource>,
    content: &str,
) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format_yaml_field("name", title));
    out.push_str(&format_yaml_field("description", description));
    out.push_str(&format!("type: {}\n", memory_type.as_str()));
    out.push_str(&format!("scope: {}\n", scope_as_str(scope)));
    if let Some(key) = dedupe_key.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format_yaml_field("dedupe_key", key));
    }
    if let Some(source) = source {
        out.push_str(&format!("source: {}\n", source.as_str()));
    }
    out.push_str("---\n\n");
    out.push_str(content.trim());
    out.push('\n');
    out
}

fn format_yaml_field(key: &str, value: &str) -> String {
    format!("{key}: {}\n", yaml_single_quoted_scalar(value))
}

fn yaml_single_quoted_scalar(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("'{}'", normalized.replace('\'', "''").trim())
}

fn scope_as_str(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::User => "user",
        MemoryScope::Repo => "repo",
    }
}

fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "memory".to_string()
    } else {
        slug.to_string()
    }
}

fn upsert_index_entry(
    index_path: &Path,
    file_name: &str,
    title: &str,
    description: &str,
) -> Result<(), SaveMemoryError> {
    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent).map_err(|err| SaveMemoryError::Io(err.to_string()))?;
    }
    let new_line = format!("- [{title}]({file_name}) — {description}");
    let old = fs::read_to_string(index_path).unwrap_or_default();
    let mut lines = Vec::new();
    let mut replaced = false;
    for line in old.lines() {
        if index_line_matches(line, file_name, title) {
            if !replaced {
                lines.push(new_line.clone());
                replaced = true;
            }
        } else {
            lines.push(line.to_string());
        }
    }
    if !replaced {
        lines.push(new_line);
    }
    let mut content = lines.join("\n");
    content.push('\n');
    fs::write(index_path, content).map_err(|err| SaveMemoryError::Io(err.to_string()))
}

fn remove_index_entries(index_path: &Path, file_name: &str) -> Result<(), SaveMemoryError> {
    let old = match fs::read_to_string(index_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(SaveMemoryError::Io(err.to_string())),
    };
    let mut content = old
        .lines()
        .filter(|line| !line.contains(&format!("]({file_name})")))
        .map(str::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    fs::write(index_path, content).map_err(|err| SaveMemoryError::Io(err.to_string()))
}

fn index_line_matches(line: &str, file_name: &str, title: &str) -> bool {
    line.contains(&format!("]({file_name})")) || line.starts_with(&format!("- [{title}]"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        _temp: tempfile::TempDir,
        prev_home: Option<std::ffi::OsString>,
        prev_userprofile: Option<std::ffi::OsString>,
        prev_rebon_config_dir: Option<std::ffi::OsString>,
        config_home_path: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let _lock = crate::memory::test_env::env_test_lock()
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let temp = tempfile::tempdir().expect("temp home");
            let config_home_path = temp.path().join(".rebon-test");
            let prev_home = std::env::var_os("HOME");
            let prev_userprofile = std::env::var_os("USERPROFILE");
            let prev_rebon_config_dir = std::env::var_os("REBON_CONFIG_DIR");
            std::env::set_var("HOME", temp.path());
            std::env::set_var("USERPROFILE", temp.path());
            std::env::set_var("REBON_CONFIG_DIR", &config_home_path);
            Self {
                _temp: temp,
                prev_home,
                prev_userprofile,
                prev_rebon_config_dir,
                config_home_path,
                _lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev_rebon_config_dir.take() {
                Some(v) => std::env::set_var("REBON_CONFIG_DIR", v),
                None => std::env::remove_var("REBON_CONFIG_DIR"),
            }
            match self.prev_home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match self.prev_userprofile.take() {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    fn upsert(scope: MemoryScope, title: &str) -> SaveMemoryRequest {
        SaveMemoryRequest {
            action: SaveMemoryAction::Upsert,
            scope,
            memory_type: Some(MemoryContentType::User),
            title: Some(title.to_string()),
            description: Some("one-line hook".to_string()),
            content: Some("Durable body".to_string()),
            source: Some(MemorySource::ExplicitUserRequest),
            reason: None,
            dedupe_key: Some("stable-key".to_string()),
            target_file: None,
        }
    }

    #[test]
    fn creates_user_scope_memory_file_and_index() {
        let env = EnvGuard::new();
        let outcome = save_memory("/repo", upsert(MemoryScope::User, "User Pref")).unwrap();
        let path = outcome.memory_path.unwrap();
        assert_eq!(
            path,
            env.config_home_path.join("memory/user/user-user-pref.md")
        );
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("name: 'User Pref'"));
        assert!(content.contains("scope: user"));
        assert!(content.contains("source: explicit_user_request"));
        let index = fs::read_to_string(outcome.index_path).unwrap();
        assert_eq!(index, "- [User Pref](user-user-pref.md) — one-line hook\n");
    }

    #[test]
    fn creates_repo_scope_project_type_memory() {
        let env = EnvGuard::new();
        let mut req = upsert(MemoryScope::Repo, "Launch Plan");
        req.memory_type = Some(MemoryContentType::Project);
        req.dedupe_key = None;
        let outcome = save_memory("/definitely/not/a/repo", req).unwrap();
        let path = outcome.memory_path.unwrap();
        assert!(path.starts_with(env.config_home_path.join("projects")));
        assert_eq!(path.file_name().unwrap(), "project-launch-plan.md");
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("type: project"));
        assert!(content.contains("scope: repo"));
    }

    #[test]
    fn updating_same_title_or_dedupe_does_not_duplicate_index() {
        let _env = EnvGuard::new();
        save_memory("/repo", upsert(MemoryScope::User, "User Pref")).unwrap();
        let outcome = save_memory("/repo", upsert(MemoryScope::User, "User Pref")).unwrap();
        assert_eq!(outcome.status, "updated");
        let index = fs::read_to_string(outcome.index_path).unwrap();
        assert_eq!(index.lines().count(), 1);
    }

    #[test]
    fn rejects_non_durable_storage_scopes() {
        for scope in ["task", "session", "coordinator", "project"] {
            let err = parse_scope(scope).unwrap_err().to_string();
            assert!(err.contains("storage scope must be `user` or `repo`"));
        }
    }

    #[test]
    fn rejects_unsafe_target_files() {
        for target in [
            "../x.md",
            "dir/x.md",
            "dir\\x.md",
            "C:x.md",
            "MEMORY.md",
            "memory.md",
            "Memory.md",
            "MeMoRy.md",
            "x.txt",
        ] {
            assert!(
                validate_target_file(target).is_err(),
                "{target} should fail"
            );
        }
    }

    #[test]
    fn yaml_frontmatter_preserves_significant_scalars_for_dedupe() {
        let _env = EnvGuard::new();
        let mut req = upsert(MemoryScope::User, "Plan: alpha #1 [draft] {x}");
        req.description = Some("Use colon: value # literal [a] {b}".to_string());
        req.dedupe_key = Some("dedupe: key # one".to_string());

        let outcome = save_memory("/repo", req.clone()).unwrap();
        let raw = fs::read_to_string(outcome.memory_path.as_ref().unwrap()).unwrap();
        assert!(raw.contains("name: 'Plan: alpha #1 [draft] {x}'"));
        assert!(raw.contains("description: 'Use colon: value # literal [a] {b}'"));
        assert!(raw.contains("dedupe_key: 'dedupe: key # one'"));

        let parsed = parse_frontmatter(&raw).frontmatter;
        assert_eq!(parsed.name(), req.title.as_deref());
        assert_eq!(parsed.description(), req.description.as_deref());
        assert_eq!(parsed.fields.get("dedupe_key"), req.dedupe_key.as_ref());

        let updated = save_memory("/repo", req).unwrap();
        assert_eq!(updated.status, "updated");
    }

    #[test]
    fn yaml_frontmatter_preserves_quotes_and_backslashes() {
        let _env = EnvGuard::new();
        let mut req = upsert(MemoryScope::User, "Bob's \"quoted\" C:\\path\\file");
        req.description = Some("Keep \\slashes\\ and 'single' plus \"double\" quotes".to_string());
        req.dedupe_key = Some("key's \\path".to_string());

        let outcome = save_memory("/repo", req.clone()).unwrap();
        let raw = fs::read_to_string(outcome.memory_path.unwrap()).unwrap();
        assert!(raw.contains("name: 'Bob''s \"quoted\" C:\\path\\file'"));
        assert!(
            raw.contains("description: 'Keep \\slashes\\ and ''single'' plus \"double\" quotes'")
        );

        let parsed = parse_frontmatter(&raw).frontmatter;
        assert_eq!(parsed.name(), req.title.as_deref());
        assert_eq!(parsed.description(), req.description.as_deref());
        assert_eq!(parsed.fields.get("dedupe_key"), req.dedupe_key.as_ref());
    }

    #[test]
    fn yaml_frontmatter_preserves_leading_special_characters() {
        let _env = EnvGuard::new();
        for title in ["*anchor", "- list item", "# heading", "[link]", "{flow}"] {
            let req = upsert(MemoryScope::User, title);
            let outcome = save_memory("/repo", req.clone()).unwrap();
            let raw = fs::read_to_string(outcome.memory_path.unwrap()).unwrap();
            assert!(raw.contains(&format!("name: '{title}'")));
            assert_eq!(parse_frontmatter(&raw).frontmatter.name(), Some(title));
        }
    }

    #[test]
    fn yaml_frontmatter_normalizes_newlines_deterministically() {
        let _env = EnvGuard::new();
        let mut req = upsert(MemoryScope::User, "Line one\nLine two\r\nLine three");
        req.description = Some("Desc one\n\nDesc two".to_string());
        req.dedupe_key = Some("key one\nkey two".to_string());

        let outcome = save_memory("/repo", req).unwrap();
        let raw = fs::read_to_string(outcome.memory_path.unwrap()).unwrap();
        let parsed = parse_frontmatter(&raw).frontmatter;
        assert_eq!(parsed.name(), Some("Line one Line two Line three"));
        assert_eq!(parsed.description(), Some("Desc one Desc two"));
        assert_eq!(
            parsed.fields.get("dedupe_key").map(String::as_str),
            Some("key one key two")
        );
    }

    #[test]
    fn deleting_target_file_removes_index_entry() {
        let _env = EnvGuard::new();
        let outcome = save_memory("/repo", upsert(MemoryScope::User, "User Pref")).unwrap();
        let path = outcome.memory_path.clone().unwrap();
        let delete = SaveMemoryRequest {
            action: SaveMemoryAction::Delete,
            scope: MemoryScope::User,
            memory_type: None,
            title: None,
            description: None,
            content: None,
            source: Some(MemorySource::ForgetRequest),
            reason: Some("forget requested".to_string()),
            dedupe_key: None,
            target_file: Some("user-user-pref.md".to_string()),
        };
        let deleted = save_memory("/repo", delete).unwrap();
        assert_eq!(deleted.status, "deleted");
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(outcome.index_path).unwrap(), "");
    }
}
