//! File-permission option builders and reducers.
//!
//! Implements:
//! * file permission option shapes and builders.
//! * accept/reject dispatch and session-suggestion rules.
//! * focus/input-mode reducer, cycle-mode lookup, and feedback-mode
//!   persistence flags.

use crate::filesystem::{
    basename, directory_for_path, generate_suggestions, is_in_global_rebon_folder,
    is_in_rebon_folder, path_in_allowed_working_path, FILE_EDIT_TOOL_NAME,
    GLOBAL_REBON_FOLDER_PERMISSION_PATTERN, REBON_FOLDER_PERMISSION_PATTERN,
};
use crate::types::{
    FileOperationType, PermissionBehavior, PermissionRuleValue, PermissionSubmitKind,
    PermissionUpdate, PermissionUpdateDestination, SelectOption, ToolPermissionContext,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    RebonFolder,
    GlobalRebonFolder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePermissionOption {
    AcceptOnce,
    AcceptSession { scope: Option<SessionScope> },
    Reject,
}

impl FilePermissionOption {
    pub fn id(&self) -> &'static str {
        match self {
            FilePermissionOption::AcceptOnce => "yes",
            FilePermissionOption::AcceptSession {
                scope: Some(SessionScope::RebonFolder),
            }
            | FilePermissionOption::AcceptSession {
                scope: Some(SessionScope::GlobalRebonFolder),
                // Persisted option ID retained for settings/session compatibility.
            } => "yes-claude-folder",
            FilePermissionOption::AcceptSession { scope: None } => "yes-session",
            FilePermissionOption::Reject => "no",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackModeEvent {
    AcceptEntered,
    AcceptCollapsed,
    RejectEntered,
    RejectCollapsed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePermissionDialogState {
    pub accept_feedback: String,
    pub reject_feedback: String,
    pub focused_option: String,
    pub yes_input_mode: bool,
    pub no_input_mode: bool,
    pub yes_feedback_mode_entered: bool,
    pub no_feedback_mode_entered: bool,
}

impl Default for FilePermissionDialogState {
    fn default() -> Self {
        Self {
            accept_feedback: String::new(),
            reject_feedback: String::new(),
            focused_option: "yes".to_string(),
            yes_input_mode: false,
            no_input_mode: false,
            yes_feedback_mode_entered: false,
            no_feedback_mode_entered: false,
        }
    }
}

impl FilePermissionDialogState {
    pub fn toggle_input_mode(&mut self, option: &str) -> Option<FeedbackModeEvent> {
        match option {
            "yes" => {
                self.yes_input_mode = !self.yes_input_mode;
                if self.yes_input_mode {
                    self.yes_feedback_mode_entered = true;
                    Some(FeedbackModeEvent::AcceptEntered)
                } else {
                    Some(FeedbackModeEvent::AcceptCollapsed)
                }
            }
            "no" => {
                self.no_input_mode = !self.no_input_mode;
                if self.no_input_mode {
                    self.no_feedback_mode_entered = true;
                    Some(FeedbackModeEvent::RejectEntered)
                } else {
                    Some(FeedbackModeEvent::RejectCollapsed)
                }
            }
            _ => None,
        }
    }

    pub fn focus(&mut self, next_value: &str) -> bool {
        let changed = next_value != self.focused_option;
        if next_value != "yes" && self.yes_input_mode && self.accept_feedback.trim().is_empty() {
            self.yes_input_mode = false;
        }
        if next_value != "no" && self.no_input_mode && self.reject_feedback.trim().is_empty() {
            self.no_input_mode = false;
        }
        self.focused_option = next_value.to_string();
        changed
    }
}

pub fn file_permission_options(
    file_path: &str,
    tool_permission_context: &ToolPermissionContext,
    operation_type: FileOperationType,
    original_cwd: &str,
    home_dir: &str,
    path_separator: char,
    mode_cycle_shortcut: &str,
    state: &FilePermissionDialogState,
) -> Vec<SelectOption<FilePermissionOption>> {
    let mut options = Vec::new();
    if state.yes_input_mode {
        options.push(SelectOption::input(
            "Yes",
            FilePermissionOption::AcceptOnce,
            "and tell Rebon what to do next",
            None,
            false,
            None,
            false,
        ));
    } else {
        options.push(SelectOption::choice(
            "Yes",
            FilePermissionOption::AcceptOnce,
        ));
    }

    let in_allowed_path = path_in_allowed_working_path(
        &[file_path.to_string()],
        original_cwd,
        tool_permission_context,
    );
    let in_rebon_folder = is_in_rebon_folder(file_path, original_cwd);
    let in_global_rebon_folder = is_in_global_rebon_folder(file_path, home_dir);

    let mode_cycle_shortcut = format_shortcut(mode_cycle_shortcut);
    if (in_rebon_folder || in_global_rebon_folder) && operation_type != FileOperationType::Read {
        let scope = if in_global_rebon_folder {
            SessionScope::GlobalRebonFolder
        } else {
            SessionScope::RebonFolder
        };
        options.push(SelectOption::choice(
            "Yes, and allow Rebon to edit its own settings for this session",
            FilePermissionOption::AcceptSession { scope: Some(scope) },
        ));
    } else {
        let session_label = if in_allowed_path {
            if operation_type == FileOperationType::Read {
                "Yes, during this session".to_string()
            } else {
                format!("Yes, allow all edits during this session ({mode_cycle_shortcut})")
            }
        } else {
            let dir_path = directory_for_path(file_path);
            let dir_name = basename(&dir_path);
            let dir_name = if dir_name.is_empty() {
                "this directory".to_string()
            } else {
                dir_name
            };
            if operation_type == FileOperationType::Read {
                format!("Yes, allow reading from {dir_name}{path_separator} during this session")
            } else {
                format!(
                    "Yes, allow all edits in {dir_name}{path_separator} during this session ({mode_cycle_shortcut})"
                )
            }
        };
        options.push(SelectOption::choice(
            session_label,
            FilePermissionOption::AcceptSession { scope: None },
        ));
    }

    if state.no_input_mode {
        options.push(SelectOption::input(
            "No",
            FilePermissionOption::Reject,
            "and tell Rebon what to do differently",
            None,
            false,
            None,
            false,
        ));
    } else {
        options.push(SelectOption::choice("No", FilePermissionOption::Reject));
    }

    options
}

pub fn cycle_mode_target(
    options: &[SelectOption<FilePermissionOption>],
) -> Option<FilePermissionOption> {
    options.iter().find_map(|option| {
        matches!(option.value, FilePermissionOption::AcceptSession { .. })
            .then_some(option.value.clone())
    })
}

fn format_shortcut(shortcut: &str) -> String {
    shortcut
        .split_whitespace()
        .map(|chord| {
            chord
                .split('+')
                .map(format_shortcut_part)
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_shortcut_part(part: &str) -> String {
    let lower = part.to_ascii_lowercase();
    match lower.as_str() {
        "shift" => "shift".to_string(),
        "tab" => "tab".to_string(),
        "ctrl" | "control" => "Ctrl".to_string(),
        "alt" | "option" => "Alt".to_string(),
        "cmd" | "command" => "Cmd".to_string(),
        "meta" | "super" => "Meta".to_string(),
        "esc" | "escape" => "Esc".to_string(),
        "return" => "Return".to_string(),
        "enter" => "Enter".to_string(),
        _ if lower.len() == 1 && lower.as_bytes()[0].is_ascii_alphabetic() => {
            lower.to_ascii_uppercase()
        }
        _ => part.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAction {
    Allow {
        updates: Vec<PermissionUpdate>,
        feedback: Option<String>,
    },
    Reject {
        feedback: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePermissionHandleResult {
    pub submit_kind: PermissionSubmitKind,
    pub has_feedback: bool,
    pub entered_feedback_mode: bool,
    pub action: PermissionAction,
}

pub fn handle_file_permission_option(
    option: &FilePermissionOption,
    file_path: Option<&str>,
    operation_type: FileOperationType,
    original_cwd: &str,
    tool_permission_context: &ToolPermissionContext,
    precomputed_paths_to_check: Option<&[String]>,
    directory_paths_to_add: Option<&[String]>,
    feedback: Option<&str>,
    entered_feedback_mode: bool,
) -> FilePermissionHandleResult {
    let trimmed_feedback = feedback
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    match option {
        FilePermissionOption::AcceptOnce => FilePermissionHandleResult {
            submit_kind: PermissionSubmitKind::Accept,
            has_feedback: trimmed_feedback.is_some(),
            entered_feedback_mode,
            action: PermissionAction::Allow {
                updates: vec![],
                feedback: trimmed_feedback,
            },
        },
        FilePermissionOption::AcceptSession { scope } => {
            let updates = match scope {
                Some(SessionScope::RebonFolder) => vec![PermissionUpdate::AddRules {
                    destination: PermissionUpdateDestination::Session,
                    rules: vec![PermissionRuleValue::new(
                        FILE_EDIT_TOOL_NAME,
                        Some(REBON_FOLDER_PERMISSION_PATTERN),
                    )],
                    behavior: PermissionBehavior::Allow,
                }],
                Some(SessionScope::GlobalRebonFolder) => vec![PermissionUpdate::AddRules {
                    destination: PermissionUpdateDestination::Session,
                    rules: vec![PermissionRuleValue::new(
                        FILE_EDIT_TOOL_NAME,
                        Some(GLOBAL_REBON_FOLDER_PERMISSION_PATTERN),
                    )],
                    behavior: PermissionBehavior::Allow,
                }],
                None => file_path.map_or_else(Vec::new, |file_path| {
                    generate_suggestions(
                        file_path,
                        operation_type,
                        original_cwd,
                        tool_permission_context,
                        precomputed_paths_to_check,
                        directory_paths_to_add,
                    )
                }),
            };

            FilePermissionHandleResult {
                submit_kind: PermissionSubmitKind::Accept,
                has_feedback: false,
                entered_feedback_mode,
                action: PermissionAction::Allow {
                    updates,
                    feedback: None,
                },
            }
        }
        FilePermissionOption::Reject => FilePermissionHandleResult {
            submit_kind: PermissionSubmitKind::Reject,
            has_feedback: trimmed_feedback.is_some(),
            entered_feedback_mode,
            action: PermissionAction::Reject {
                feedback: trimmed_feedback,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PermissionMode;

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
    fn options_show_yes_session_and_no_by_default() {
        let options = file_permission_options(
            r"C:\Repo\src\main.rs",
            &context(PermissionMode::Default, &[]),
            FileOperationType::Write,
            r"C:\Repo",
            r"C:\Users\dev",
            '\\',
            "shift+tab",
            &FilePermissionDialogState::default(),
        );
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].label, "Yes");
        assert_eq!(
            options[1].label,
            "Yes, allow all edits during this session (shift+tab)"
        );
        assert_eq!(options[2].label, "No");
    }

    #[test]
    fn options_use_special_rebon_scope_for_settings_edits() {
        let options = file_permission_options(
            r"C:\Repo\.rebon\settings.local.json",
            &context(PermissionMode::Default, &[]),
            FileOperationType::Write,
            r"C:\Repo",
            r"C:\Users\dev",
            '\\',
            "shift+tab",
            &FilePermissionDialogState::default(),
        );
        assert_eq!(
            options[1].value,
            FilePermissionOption::AcceptSession {
                scope: Some(SessionScope::RebonFolder)
            }
        );
    }

    #[test]
    fn options_name_outside_directory_for_reads() {
        let options = file_permission_options(
            r"C:\Logs\app\server.log",
            &context(PermissionMode::Default, &[]),
            FileOperationType::Read,
            r"C:\Repo",
            r"C:\Users\dev",
            '\\',
            "shift+tab",
            &FilePermissionDialogState::default(),
        );
        assert_eq!(
            options[1].label,
            r"Yes, allow reading from app\ during this session"
        );
    }

    #[test]
    fn focus_resets_empty_input_modes_but_preserves_typed_feedback() {
        let mut state = FilePermissionDialogState::default();
        state.yes_input_mode = true;
        state.no_input_mode = true;
        state.reject_feedback = "typed".to_string();
        assert!(state.focus("no"));
        assert!(!state.yes_input_mode);
        assert!(state.no_input_mode);
    }

    #[test]
    fn toggle_input_mode_tracks_first_entry() {
        let mut state = FilePermissionDialogState::default();
        assert_eq!(
            state.toggle_input_mode("yes"),
            Some(FeedbackModeEvent::AcceptEntered)
        );
        assert!(state.yes_feedback_mode_entered);
        assert_eq!(
            state.toggle_input_mode("yes"),
            Some(FeedbackModeEvent::AcceptCollapsed)
        );
    }

    #[test]
    fn cycle_mode_picks_accept_session_option() {
        let options = file_permission_options(
            r"C:\Repo\src\main.rs",
            &context(PermissionMode::Default, &[]),
            FileOperationType::Write,
            r"C:\Repo",
            r"C:\Users\dev",
            '\\',
            "shift+tab",
            &FilePermissionDialogState::default(),
        );
        assert_eq!(
            cycle_mode_target(&options),
            Some(FilePermissionOption::AcceptSession { scope: None })
        );
    }

    #[test]
    fn accept_once_and_reject_trim_feedback() {
        let accept = handle_file_permission_option(
            &FilePermissionOption::AcceptOnce,
            Some(r"C:\Repo\src\main.rs"),
            FileOperationType::Write,
            r"C:\Repo",
            &context(PermissionMode::Default, &[]),
            None,
            None,
            Some("  do this  "),
            true,
        );
        assert_eq!(
            accept.action,
            PermissionAction::Allow {
                updates: vec![],
                feedback: Some("do this".to_string()),
            }
        );

        let reject = handle_file_permission_option(
            &FilePermissionOption::Reject,
            Some(r"C:\Repo\src\main.rs"),
            FileOperationType::Write,
            r"C:\Repo",
            &context(PermissionMode::Default, &[]),
            None,
            None,
            Some("   "),
            false,
        );
        assert_eq!(reject.action, PermissionAction::Reject { feedback: None });
    }

    #[test]
    fn accept_session_generates_claude_folder_rule() {
        let result = handle_file_permission_option(
            &FilePermissionOption::AcceptSession {
                scope: Some(SessionScope::GlobalRebonFolder),
            },
            Some(r"C:\Users\dev\.rebon\settings.json"),
            FileOperationType::Write,
            r"C:\Repo",
            &context(PermissionMode::Default, &[]),
            None,
            None,
            None,
            false,
        );
        assert_eq!(
            result.action,
            PermissionAction::Allow {
                updates: vec![PermissionUpdate::AddRules {
                    destination: PermissionUpdateDestination::Session,
                    rules: vec![PermissionRuleValue::new(
                        "Edit",
                        Some(GLOBAL_REBON_FOLDER_PERMISSION_PATTERN),
                    )],
                    behavior: PermissionBehavior::Allow,
                }],
                feedback: None,
            }
        );
    }

    #[test]
    fn accept_session_threads_generated_suggestions_for_outside_paths() {
        let result = handle_file_permission_option(
            &FilePermissionOption::AcceptSession { scope: None },
            Some(r"C:\Outside\config.json"),
            FileOperationType::Write,
            r"C:\Repo",
            &context(PermissionMode::Default, &[]),
            Some(&[r"C:\Outside\config.json".to_string()]),
            Some(&[r"C:\Outside".to_string()]),
            None,
            false,
        );
        assert_eq!(
            result.action,
            PermissionAction::Allow {
                updates: vec![
                    PermissionUpdate::SetMode {
                        destination: PermissionUpdateDestination::Session,
                        mode: PermissionMode::AcceptEdits,
                    },
                    PermissionUpdate::AddDirectories {
                        destination: PermissionUpdateDestination::Session,
                        directories: vec![r"C:\Outside".to_string()],
                    }
                ],
                feedback: None,
            }
        );
    }
}
