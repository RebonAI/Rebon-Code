//! File-permission path and suggestion helpers.
//!
//! Implements:
//! * case-normalized path comparison.
//! * working-path membership checks (`path_in_allowed_working_path` /
//!   `path_in_working_path`).
//! * read-rule suggestion creation.
//! * write/read session-upgrade suggestion generation.
//! * the `.rebon/` scope detection used by the file-dialog option list.

use crate::types::{
    FileOperationType, PermissionBehavior, PermissionMode, PermissionRuleValue, PermissionUpdate,
    PermissionUpdateDestination, ToolPermissionContext,
};

pub const FILE_EDIT_TOOL_NAME: &str = "Edit";
pub const FILE_READ_TOOL_NAME: &str = "Read";
pub const REBON_FOLDER_PERMISSION_PATTERN: &str = "/.rebon/**";
pub const GLOBAL_REBON_FOLDER_PERMISSION_PATTERN: &str = "~/.rebon/**";

pub fn normalize_case_for_comparison(path: &str) -> String {
    path.to_lowercase()
}

pub fn is_in_rebon_folder(file_path: &str, original_cwd: &str) -> bool {
    let absolute_path = normalize_for_comparison(file_path);
    let rebon_dir = normalize_for_comparison(&format!("{}/.rebon", original_cwd));
    is_inside_prefixed_dir(&absolute_path, &rebon_dir)
}

pub fn is_in_global_rebon_folder(file_path: &str, home_dir: &str) -> bool {
    let absolute_path = normalize_for_comparison(file_path);
    let rebon_dir = normalize_for_comparison(&format!("{home_dir}/.rebon"));
    is_inside_prefixed_dir(&absolute_path, &rebon_dir)
}

pub fn all_working_directories(
    original_cwd: &str,
    tool_permission_context: &ToolPermissionContext,
) -> Vec<String> {
    let mut working_paths = vec![original_cwd.to_string()];
    working_paths.extend(
        tool_permission_context
            .additional_working_directories
            .iter()
            .cloned(),
    );
    working_paths
}

pub fn path_in_allowed_working_path(
    paths_to_check: &[String],
    original_cwd: &str,
    tool_permission_context: &ToolPermissionContext,
) -> bool {
    let working_paths = all_working_directories(original_cwd, tool_permission_context);
    paths_to_check.iter().all(|candidate| {
        working_paths
            .iter()
            .any(|working_path| path_in_working_path(candidate, working_path))
    })
}

pub fn path_in_working_path(path: &str, working_path: &str) -> bool {
    let normalized_path = normalize_for_comparison(path);
    let normalized_working_path = normalize_for_comparison(working_path);

    if normalized_path == normalized_working_path {
        return true;
    }

    if normalized_working_path == "/" {
        return true;
    }

    normalized_path.starts_with(&(normalized_working_path + "/"))
}

pub fn create_read_rule_suggestion(
    dir_path: &str,
    destination: PermissionUpdateDestination,
) -> Option<PermissionUpdate> {
    let path_for_pattern = to_posix_path(dir_path);
    if path_for_pattern == "/" {
        return None;
    }

    let rule_content = if path_for_pattern.starts_with('/') {
        format!("/{path_for_pattern}/**")
    } else {
        format!("{path_for_pattern}/**")
    };

    Some(PermissionUpdate::AddRules {
        destination,
        rules: vec![PermissionRuleValue::new(
            FILE_READ_TOOL_NAME,
            Some(rule_content),
        )],
        behavior: PermissionBehavior::Allow,
    })
}

pub fn generate_suggestions(
    file_path: &str,
    operation_type: FileOperationType,
    original_cwd: &str,
    tool_permission_context: &ToolPermissionContext,
    precomputed_paths_to_check: Option<&[String]>,
    directory_paths_to_add: Option<&[String]>,
) -> Vec<PermissionUpdate> {
    let fallback_paths = vec![file_path.to_string()];
    let paths_to_check = precomputed_paths_to_check.unwrap_or(&fallback_paths);
    let is_outside_working_dir =
        !path_in_allowed_working_path(paths_to_check, original_cwd, tool_permission_context);

    if operation_type == FileOperationType::Read && is_outside_working_dir {
        let default_dir = directory_for_path(file_path);
        let default_dir_paths = vec![default_dir];
        let dir_paths = directory_paths_to_add.unwrap_or(&default_dir_paths);
        return dir_paths
            .iter()
            .filter_map(|dir| {
                create_read_rule_suggestion(dir, PermissionUpdateDestination::Session)
            })
            .collect();
    }

    let should_suggest_accept_edits = matches!(
        tool_permission_context.mode,
        PermissionMode::Default | PermissionMode::Plan
    );

    if matches!(
        operation_type,
        FileOperationType::Write | FileOperationType::Create
    ) {
        let mut updates = if should_suggest_accept_edits {
            vec![PermissionUpdate::SetMode {
                destination: PermissionUpdateDestination::Session,
                mode: PermissionMode::AcceptEdits,
            }]
        } else {
            vec![]
        };

        if is_outside_working_dir {
            let default_dir = directory_for_path(file_path);
            let default_dir_paths = vec![default_dir];
            let dir_paths = directory_paths_to_add.unwrap_or(&default_dir_paths);
            updates.push(PermissionUpdate::AddDirectories {
                destination: PermissionUpdateDestination::Session,
                directories: dir_paths.to_vec(),
            });
        }

        return updates;
    }

    if should_suggest_accept_edits {
        vec![PermissionUpdate::SetMode {
            destination: PermissionUpdateDestination::Session,
            mode: PermissionMode::AcceptEdits,
        }]
    } else {
        vec![]
    }
}

pub fn to_posix_path(path: &str) -> String {
    path.replace('\\', "/")
}

pub fn basename(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

pub fn directory_for_path(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if let Some(idx) = trimmed.rfind(['/', '\\']) {
        if idx == 0 {
            return trimmed[..=idx].to_string();
        }
        return trimmed[..idx].to_string();
    }
    trimmed.to_string()
}

fn is_inside_prefixed_dir(candidate: &str, dir: &str) -> bool {
    candidate.starts_with(&(dir.to_string() + "/"))
}

fn normalize_for_comparison(path: &str) -> String {
    let path = to_posix_path(path);
    let path = if path.starts_with("/private/var/") {
        path.replacen("/private/var/", "/var/", 1)
    } else if path == "/private/tmp" {
        "/tmp".to_string()
    } else if path.starts_with("/private/tmp/") {
        path.replacen("/private/tmp/", "/tmp/", 1)
    } else {
        path
    };
    normalize_case_for_comparison(&lexical_normalize(&path))
}

fn lexical_normalize(path: &str) -> String {
    let (prefix, absolute, rest) = split_prefix(path);
    let mut stack: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if let Some(last) = stack.last() {
                    if *last != ".." {
                        stack.pop();
                    } else if !absolute {
                        stack.push(part);
                    }
                } else if !absolute {
                    stack.push(part);
                }
            }
            _ => stack.push(part),
        }
    }

    let joined = stack.join("/");
    let mut normalized = String::new();
    normalized.push_str(prefix);
    if absolute {
        normalized.push('/');
    }
    normalized.push_str(&joined);

    while normalized.len() > 1 && normalized.ends_with('/') && !is_windows_drive_root(&normalized) {
        normalized.pop();
    }

    if normalized.is_empty() {
        ".".to_string()
    } else {
        normalized
    }
}

fn split_prefix(path: &str) -> (&str, bool, &str) {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        let prefix = &path[..2];
        let rest = &path[2..];
        if let Some(stripped) = rest.strip_prefix('/') {
            (prefix, true, stripped)
        } else {
            (prefix, false, rest)
        }
    } else if let Some(stripped) = path.strip_prefix('/') {
        ("", true, stripped)
    } else {
        ("", false, path)
    }
}

fn is_windows_drive_root(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() == 3 && bytes[1] == b':' && bytes[2] == b'/'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolPermissionContext;

    fn context(mode: PermissionMode, extra: &[&str]) -> ToolPermissionContext {
        let mut context = ToolPermissionContext::new(mode);
        for dir in extra {
            context
                .additional_working_directories
                .insert((*dir).to_string());
        }
        context
    }

    #[test]
    fn detects_project_and_global_claude_folders_case_insensitively() {
        assert!(is_in_rebon_folder(
            r"C:\Repo\.ReBoN\settings.local.json",
            r"C:\Repo"
        ));
        assert!(is_in_global_rebon_folder(
            r"C:\Users\dev\.rebon\skills\demo\main.md",
            r"C:\Users\dev"
        ));
        assert!(!is_in_rebon_folder(r"C:\Repo\.rebon", r"C:\Repo"));
    }

    #[test]
    fn working_path_accepts_same_path_and_descendants() {
        assert!(path_in_working_path(r"C:\Repo\src\main.rs", r"C:\Repo"));
        assert!(path_in_working_path(
            "/private/tmp/demo/file.txt",
            "/tmp/demo"
        ));
        assert!(!path_in_working_path(r"C:\RepoTwo\main.rs", r"C:\Repo"));
    }

    #[test]
    fn allowed_working_paths_include_additional_directories() {
        let ctx = context(PermissionMode::Default, &[r"C:\External"]);
        assert!(path_in_allowed_working_path(
            &[r"C:\External\notes.txt".to_string()],
            r"C:\Repo",
            &ctx
        ));
        assert!(!path_in_allowed_working_path(
            &[r"C:\Other\notes.txt".to_string()],
            r"C:\Repo",
            &ctx
        ));
    }

    #[test]
    fn read_suggestion_uses_double_slash_for_posix_absolute_paths() {
        let update = create_read_rule_suggestion("/tmp/logs", PermissionUpdateDestination::Session)
            .expect("non-root dir");
        assert_eq!(
            update,
            PermissionUpdate::AddRules {
                destination: PermissionUpdateDestination::Session,
                rules: vec![PermissionRuleValue::new("Read", Some("//tmp/logs/**"))],
                behavior: PermissionBehavior::Allow,
            }
        );
        assert_eq!(
            create_read_rule_suggestion("/", PermissionUpdateDestination::Session),
            None
        );
    }

    #[test]
    fn generate_read_suggestions_for_paths_outside_working_dir() {
        let updates = generate_suggestions(
            r"C:\Logs\app\out.txt",
            FileOperationType::Read,
            r"C:\Repo",
            &context(PermissionMode::Default, &[]),
            Some(&[r"C:\Logs\app\out.txt".to_string()]),
            Some(&[
                r"C:\Logs\app".to_string(),
                r"D:\Resolved\Logs\app".to_string(),
            ]),
        );
        assert_eq!(
            updates,
            vec![
                PermissionUpdate::AddRules {
                    destination: PermissionUpdateDestination::Session,
                    rules: vec![PermissionRuleValue::new("Read", Some("C:/Logs/app/**"))],
                    behavior: PermissionBehavior::Allow,
                },
                PermissionUpdate::AddRules {
                    destination: PermissionUpdateDestination::Session,
                    rules: vec![PermissionRuleValue::new(
                        "Read",
                        Some("D:/Resolved/Logs/app/**")
                    )],
                    behavior: PermissionBehavior::Allow,
                }
            ]
        );
    }

    #[test]
    fn generate_write_suggestions_upgrade_default_mode_and_add_directory() {
        let updates = generate_suggestions(
            r"C:\Outside\config.json",
            FileOperationType::Write,
            r"C:\Repo",
            &context(PermissionMode::Default, &[]),
            Some(&[r"C:\Outside\config.json".to_string()]),
            Some(&[r"C:\Outside".to_string()]),
        );
        assert_eq!(
            updates,
            vec![
                PermissionUpdate::SetMode {
                    destination: PermissionUpdateDestination::Session,
                    mode: PermissionMode::AcceptEdits,
                },
                PermissionUpdate::AddDirectories {
                    destination: PermissionUpdateDestination::Session,
                    directories: vec![r"C:\Outside".to_string()],
                }
            ]
        );
    }

    #[test]
    fn auto_mode_does_not_downgrade_to_accept_edits() {
        let updates = generate_suggestions(
            r"C:\Outside\config.json",
            FileOperationType::Create,
            r"C:\Repo",
            &context(PermissionMode::Auto, &[]),
            Some(&[r"C:\Outside\config.json".to_string()]),
            Some(&[r"C:\Outside".to_string()]),
        );
        assert_eq!(
            updates,
            vec![PermissionUpdate::AddDirectories {
                destination: PermissionUpdateDestination::Session,
                directories: vec![r"C:\Outside".to_string()],
            }]
        );
    }

    #[test]
    fn read_inside_working_dir_only_suggests_accept_edits_upgrade() {
        let updates = generate_suggestions(
            r"C:\Repo\README.md",
            FileOperationType::Read,
            r"C:\Repo",
            &context(PermissionMode::Plan, &[]),
            None,
            None,
        );
        assert_eq!(
            updates,
            vec![PermissionUpdate::SetMode {
                destination: PermissionUpdateDestination::Session,
                mode: PermissionMode::AcceptEdits,
            }]
        );
    }
}
