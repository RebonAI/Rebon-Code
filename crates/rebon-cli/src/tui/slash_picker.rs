//! Slash command picker — wires `rebon-customselect`'s navigation
//! reducer into the `/` command overlay.
//!
//! This module replaces the hand-rolled index + viewport math that
//! previously lived inline in the runner with a proper
//! `NavigationState<String>` from `rebon-customselect`. The navigation
//! reducer provides wrap-around, page-up/down, viewport clamping, and
//! validated-focus fallback for free.
//!
//! ## Design
//!
//! * **`SlashPickerState`** wraps a `NavigationState<String>` whose
//!   option values are slash command names (e.g. `"help"`, `"commit"`).
//! * **`sync`** is called each frame to open/close/reset the picker
//!   based on the current input buffer and available commands.
//! * **`handle_key`** translates raw crossterm key events into
//!   `NavigationAction` dispatches + selection/completion results.
//! * **`render_data`** projects the visible options window and focused
//!   value for the ratatui render pass.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use rebon_customselect::{
    NavigationAction, NavigationProps, NavigationState, OptionWithDescription,
    DEFAULT_VISIBLE_OPTION_COUNT,
};
use rebon_types::{SlashCommand, SlashCommandCategory};

/// Maximum number of visible rows in the picker overlay.
const MAX_VISIBLE: usize = 8;

/// Visible option count passed to the navigation reducer.
fn picker_visible_count() -> usize {
    DEFAULT_VISIBLE_OPTION_COUNT.max(MAX_VISIBLE)
}

/// State for the slash command picker overlay, backed by
/// `rebon-customselect`'s navigation reducer.
#[derive(Debug, Clone)]
pub struct SlashPickerState {
    /// Navigation reducer from `rebon-customselect`.
    nav: NavigationState<String>,
    /// Owned synthetic slash commands used when a provider sub-menu
    /// is active. `None` during normal slash-command filtering.
    synthetic_commands: Option<Vec<SlashCommand>>,
}

impl SlashPickerState {
    /// Build a new picker from a pre-filtered list of slash commands.
    fn new(commands: &[&SlashCommand]) -> Self {
        let options = commands_to_options(commands);
        let nav = NavigationState::new(NavigationProps {
            visible_option_count: Some(picker_visible_count()),
            options,
            initial_focus_value: None,
            focus_value: None,
        });
        Self {
            nav,
            synthetic_commands: None,
        }
    }

    /// Dispatch a navigation action on the inner reducer.
    fn dispatch_nav(&mut self, action: NavigationAction<String>) {
        self.nav = self.nav.clone().dispatch(action);
    }

    /// The currently focused command name (validated).
    pub fn focused_value(&self) -> Option<String> {
        self.nav.validated_focused_value()
    }

    /// Borrow the navigation state for render projections.
    #[cfg(test)]
    pub fn nav(&self) -> &NavigationState<String> {
        &self.nav
    }

    /// Build an empty picker for testing. Not available outside `cfg(test)`.
    #[cfg(test)]
    pub fn empty_for_test() -> Self {
        Self::new(&[])
    }
}

// ---------------------------------------------------------------------------
// Option conversion
// ---------------------------------------------------------------------------

fn commands_to_options(commands: &[&SlashCommand]) -> Vec<OptionWithDescription<String>> {
    commands
        .iter()
        .map(|cmd| {
            let mut opt = OptionWithDescription::text(format!("/{}", cmd.name), cmd.name.clone());
            opt.base.description = Some(cmd.description.clone());
            opt
        })
        .collect()
}

fn command_matches_query(cmd: &SlashCommand, query: &str) -> Option<i32> {
    let desc_lower = cmd.description.to_lowercase();
    let mut score = score_canonical_name_match(&cmd.name.to_lowercase(), query);

    for alias in &cmd.aliases {
        if let Some(alias_score) = score_alias_match(&alias.to_lowercase(), query) {
            score = Some(score.unwrap_or(i32::MIN).max(alias_score));
        }
    }

    if desc_lower.contains(query) {
        score = Some(score.unwrap_or(i32::MIN).max(100));
    }

    score
}

fn score_canonical_name_match(name_lower: &str, query: &str) -> Option<i32> {
    // Tier 1: exact canonical name match → score 1000
    if name_lower == query {
        return Some(1000);
    }
    // Tier 2: canonical prefix match → 500 + bonus for shorter names
    if name_lower.starts_with(query) {
        let len_bonus = 100i32.saturating_sub(name_lower.len() as i32);
        return Some(500 + len_bonus);
    }
    // Tier 3: substring in canonical name → 200
    if name_lower.contains(query) {
        return Some(200);
    }
    // Tier 5: fuzzy match in canonical name (all query chars present in order)
    if fuzzy_matches(name_lower, query) {
        return Some(50);
    }
    None
}

fn score_alias_match(alias_lower: &str, query: &str) -> Option<i32> {
    // Exact aliases should strongly select their canonical command.
    if alias_lower == query {
        return Some(900);
    }
    // Non-exact alias matches are intentionally below canonical prefix matches
    // so aliases do not reorder existing command-name prefix results.
    if alias_lower.starts_with(query) {
        let len_bonus = 100i32.saturating_sub(alias_lower.len() as i32);
        return Some(400 + len_bonus);
    }
    if alias_lower.contains(query) {
        return Some(150);
    }
    if fuzzy_matches(alias_lower, query) {
        return Some(25);
    }
    None
}

// ---------------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------------

/// Filter and rank slash commands by the query after "/".
///
/// Uses multi-tier ranking:
/// 1. Exact name match (highest priority)
/// 2. Prefix match (shorter names preferred)
/// 3. Substring/fuzzy match in name or description
///
/// When the query is empty (just "/"), returns all commands sorted
/// alphabetically.
pub fn filtered_slash_commands<'a>(
    input: &str,
    commands: &'a [SlashCommand],
) -> Vec<&'a SlashCommand> {
    let query = input.strip_prefix('/').unwrap_or("").to_lowercase();
    if query.is_empty() {
        let mut all: Vec<&SlashCommand> = commands.iter().collect();
        all.sort_by(|a, b| a.name.cmp(&b.name));
        return all;
    }

    // Score each command: higher is better.
    let mut scored: Vec<(&SlashCommand, i32)> = commands
        .iter()
        .filter_map(|cmd| command_matches_query(cmd, &query).map(|score| (cmd, score)))
        .collect();

    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.name.cmp(&b.0.name)));
    scored.into_iter().map(|(cmd, _)| cmd).collect()
}

/// Check if all chars of `needle` appear in `haystack` in order.
fn fuzzy_matches(haystack: &str, needle: &str) -> bool {
    let mut hay_iter = haystack.chars();
    for nc in needle.chars() {
        loop {
            match hay_iter.next() {
                Some(hc) if hc == nc => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Provider sub-menu
// ---------------------------------------------------------------------------

/// Check whether the input text is a provider command (i.e. the user typed
/// `/provider` or `/provider ` followed by optional text).
fn is_provider_command_input(input: &str) -> bool {
    input == "/provider" || input.starts_with("/provider ") || input.starts_with("/provider:")
}

/// Generate synthetic [`SlashCommand`]s for the `/provider` sub-menu.
///
/// Includes both static subcommands (list, add, remove, use, add-model)
/// and dynamic quick-switch entries for each configured custom provider.
fn generate_provider_subcommands(custom_providers: &[String]) -> Vec<SlashCommand> {
    let mut cmds = vec![
        SlashCommand {
            name: "provider".into(),
            description: "Open provider configuration panel".into(),
            input: None,
            category: Some(SlashCommandCategory::Command),
            aliases: Vec::new(),
        },
        SlashCommand {
            name: "provider list".into(),
            description: "List configured providers".into(),
            input: None,
            category: Some(SlashCommandCategory::Command),
            aliases: Vec::new(),
        },
        SlashCommand {
            name: "provider add".into(),
            description: "Open the protected provider setup form".into(),
            input: None,
            category: Some(SlashCommandCategory::Command),
            aliases: Vec::new(),
        },
        SlashCommand {
            name: "provider remove <name>".into(),
            description: "Remove a provider".into(),
            input: None,
            category: Some(SlashCommandCategory::Command),
            aliases: Vec::new(),
        },
        SlashCommand {
            name: "provider use <name>".into(),
            description: "Switch to a provider (use default for built-in)".into(),
            input: None,
            category: Some(SlashCommandCategory::Command),
            aliases: Vec::new(),
        },
        SlashCommand {
            name: "provider add-model <name> <model>".into(),
            description: "Add a model to a provider".into(),
            input: None,
            category: Some(SlashCommandCategory::Command),
            aliases: Vec::new(),
        },
    ];

    // Append quick-switch entries for each configured custom provider.
    for name in custom_providers {
        cmds.push(SlashCommand {
            name: format!("provider use {name}"),
            description: format!("Switch to {name}"),
            input: None,
            category: Some(SlashCommandCategory::Command),
            aliases: Vec::new(),
        });
    }

    cmds
}

// ---------------------------------------------------------------------------
// Sync (called each frame)
// ---------------------------------------------------------------------------

/// Sync the slash picker state each frame: open when "/" prefix is
/// present and there are matching commands, close otherwise.
///
/// When the picker is already open and the filtered list changes (the
/// user typed more characters), the navigation reducer is reset with
/// the new options while preserving focus when possible.
pub fn sync(picker: &mut Option<SlashPickerState>, input: &str, commands: &[SlashCommand]) {
    if !input.starts_with('/') || commands.is_empty() {
        *picker = None;
        return;
    }

    // Provider sub-menu: intercept `/provider` and show subcommands.
    if is_provider_command_input(input) {
        let custom_provider_names: Vec<String> = crate::rebon_config::list_custom_providers()
            .into_iter()
            .map(|p| p.name)
            .collect();
        let synthetic = generate_provider_subcommands(&custom_provider_names);
        // Clone so the owned list can be stored in state while
        // `filtered_slash_commands` borrows the original.
        let synthetic_for_filter = synthetic.clone();
        let matches = filtered_slash_commands(input, &synthetic_for_filter);
        if matches.is_empty() {
            *picker = None;
            return;
        }
        match picker {
            Some(state) => {
                state.synthetic_commands = Some(synthetic);
                let options = commands_to_options(&matches);
                let current_focus = state.nav.validated_focused_value();
                state.dispatch_nav(NavigationAction::Reset {
                    options,
                    visible_option_count: Some(picker_visible_count()),
                    focus_value: current_focus,
                });
            }
            None => {
                let mut state = SlashPickerState::new(&matches);
                state.synthetic_commands = Some(synthetic);
                *picker = Some(state);
            }
        }
        return;
    }

    // Normal slash-command path.
    let matches = filtered_slash_commands(input, commands);
    if matches.is_empty() {
        *picker = None;
        return;
    }

    match picker {
        Some(state) => {
            // Reset navigation with updated options, preserving focus.
            state.synthetic_commands = None;
            let options = commands_to_options(&matches);
            let current_focus = state.nav.validated_focused_value();
            state.dispatch_nav(NavigationAction::Reset {
                options,
                visible_option_count: Some(picker_visible_count()),
                focus_value: current_focus,
            });
        }
        None => {
            *picker = Some(SlashPickerState::new(&matches));
        }
    }
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

/// Result of handling a key event in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerKeyResult {
    /// The event was consumed by navigation (no buffer change).
    Consumed,
    /// Enter: the focused command was completed with a trailing space.
    Selected {
        /// New input buffer value (e.g. `"/help"`).
        input: String,
        /// Cursor offset to set.
        cursor: usize,
    },
    /// Tab: the focused command was tab-completed with a trailing space.
    Completed {
        /// New input buffer value (e.g. `"/help "`).
        input: String,
        /// Cursor offset to set.
        cursor: usize,
    },
    /// Esc: the picker was closed without selection.
    Closed,
}

/// Handle a keyboard event when the slash picker is active.
///
/// Returns `Some(result)` if the event was consumed, `None` if the
/// picker is closed or the key is not relevant.
pub fn handle_key(
    picker: &mut Option<SlashPickerState>,
    key: &KeyEvent,
) -> Option<PickerKeyResult> {
    let state = picker.as_mut()?;

    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    match key.code {
        KeyCode::Up => {
            state.dispatch_nav(NavigationAction::FocusPreviousOption);
            Some(PickerKeyResult::Consumed)
        }
        KeyCode::Down => {
            state.dispatch_nav(NavigationAction::FocusNextOption);
            Some(PickerKeyResult::Consumed)
        }
        KeyCode::PageUp => {
            state.dispatch_nav(NavigationAction::FocusPreviousPage);
            Some(PickerKeyResult::Consumed)
        }
        KeyCode::PageDown => {
            state.dispatch_nav(NavigationAction::FocusNextPage);
            Some(PickerKeyResult::Consumed)
        }
        KeyCode::Enter => {
            let result = if let Some(name) = state.focused_value() {
                let input = format!("/{name} ");
                let cursor = input.len();
                PickerKeyResult::Selected { input, cursor }
            } else {
                PickerKeyResult::Closed
            };
            *picker = None;
            Some(result)
        }
        KeyCode::Tab => {
            let result = if let Some(name) = state.focused_value() {
                let input = format!("/{name} ");
                let cursor = input.len();
                PickerKeyResult::Completed { input, cursor }
            } else {
                PickerKeyResult::Closed
            };
            *picker = None;
            Some(result)
        }
        KeyCode::Esc => {
            *picker = None;
            Some(PickerKeyResult::Closed)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Render data projection
// ---------------------------------------------------------------------------

/// One row in the picker overlay, projected for rendering.
#[derive(Debug, Clone)]
pub struct PickerRow {
    /// Command name without the leading `/`.
    pub name: String,
    /// Command description.
    pub description: String,
    /// Whether this row is the focused row.
    pub is_focused: bool,
    /// Optional category badge (skill / agent / cmd).
    pub category: Option<rebon_types::SlashCommandCategory>,
}

/// Data needed to render the picker overlay.
#[derive(Debug, Clone)]
pub struct PickerRenderData {
    /// Visible rows in the current viewport window.
    pub rows: Vec<PickerRow>,
}

/// Project the render data for the picker overlay.
///
/// Returns `None` when the picker is closed or has no visible options.
pub fn render_data(
    picker: &Option<SlashPickerState>,
    commands: &[SlashCommand],
    input: &str,
) -> Option<PickerRenderData> {
    let state = picker.as_ref()?;
    let focused = state.focused_value();
    let visible = state.nav.visible_options();
    if visible.is_empty() {
        return None;
    }

    // When the picker is showing provider subcommands, look up
    // descriptions and categories from the synthetic command list.
    let rows: Vec<PickerRow> = match &state.synthetic_commands {
        Some(synthetic) => {
            let matches = filtered_slash_commands(input, synthetic);
            visible
                .iter()
                .map(|vo| {
                    let name = vo.option.value().clone();
                    let matched = matches.iter().find(|cmd| cmd.name == name);
                    let description = matched
                        .map(|cmd| cmd.description.clone())
                        .unwrap_or_default();
                    let category = matched.and_then(|cmd| cmd.category);
                    let is_focused = focused.as_ref() == Some(&name);
                    PickerRow {
                        name,
                        description,
                        is_focused,
                        category,
                    }
                })
                .collect()
        }
        None => {
            let matches = filtered_slash_commands(input, commands);
            visible
                .iter()
                .map(|vo| {
                    let name = vo.option.value().clone();
                    let matched = matches.iter().find(|cmd| cmd.name == name);
                    let description = matched
                        .map(|cmd| cmd.description.clone())
                        .unwrap_or_default();
                    let category = matched.and_then(|cmd| cmd.category);
                    let is_focused = focused.as_ref() == Some(&name);
                    PickerRow {
                        name,
                        description,
                        is_focused,
                        category,
                    }
                })
                .collect()
        }
    };

    Some(PickerRenderData { rows })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_commands() -> Vec<SlashCommand> {
        vec![
            SlashCommand {
                name: "help".into(),
                description: "Show help".into(),
                input: None,
                category: None,
                aliases: Vec::new(),
            },
            SlashCommand {
                name: "clear".into(),
                description: "Clear screen".into(),
                input: None,
                category: None,
                aliases: Vec::new(),
            },
            SlashCommand {
                name: "commit".into(),
                description: "Commit changes".into(),
                input: None,
                category: None,
                aliases: vec!["ci".into()],
            },
        ]
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE)
    }

    fn app_with_commands() -> crate::tui::app::AppState {
        let mut app = crate::tui::app::AppState::new();
        app.slash_commands = test_commands();
        app
    }

    fn sync_app_picker(app: &mut crate::tui::app::AppState) {
        sync(&mut app.slash_picker, &app.input, &app.slash_commands);
    }

    fn handle_app_picker_key(
        app: &mut crate::tui::app::AppState,
        key: &KeyEvent,
    ) -> Option<PickerKeyResult> {
        let result = handle_key(&mut app.slash_picker, key)?;
        match &result {
            PickerKeyResult::Selected { input, cursor }
            | PickerKeyResult::Completed { input, cursor } => {
                app.input = input.clone();
                app.cursor_offset = *cursor;
            }
            PickerKeyResult::Consumed | PickerKeyResult::Closed => {}
        }
        Some(result)
    }

    #[test]
    fn slash_picker_activates_on_slash_prefix() {
        let mut app = app_with_commands();
        app.input = "/".into();
        sync_app_picker(&mut app);
        assert!(app.slash_picker.is_some());
    }

    #[test]
    fn slash_picker_inactive_without_slash_prefix() {
        let mut app = app_with_commands();
        app.input = "hello".into();
        sync_app_picker(&mut app);
        assert!(app.slash_picker.is_none());
    }

    #[test]
    fn slash_picker_filters_by_query() {
        let mut app = app_with_commands();
        app.input = "/c".into();
        sync_app_picker(&mut app);
        let matches = filtered_slash_commands(&app.input, &app.slash_commands);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].name, "clear");
        assert_eq!(matches[1].name, "commit");
    }

    #[test]
    fn slash_picker_navigate_down_and_up() {
        let mut app = app_with_commands();
        app.input = "/".into();
        sync_app_picker(&mut app);
        assert!(app.slash_picker.is_some());

        assert!(handle_app_picker_key(&mut app, &key(KeyCode::Down)).is_some());
        assert_eq!(
            app.slash_picker.as_ref().unwrap().focused_value(),
            Some("commit".into())
        );

        assert!(handle_app_picker_key(&mut app, &key(KeyCode::Up)).is_some());
        assert_eq!(
            app.slash_picker.as_ref().unwrap().focused_value(),
            Some("clear".into())
        );
    }

    #[test]
    fn slash_picker_enter_completes_command() {
        let mut app = app_with_commands();
        app.input = "/".into();
        sync_app_picker(&mut app);

        handle_app_picker_key(&mut app, &key(KeyCode::Down));
        assert!(handle_app_picker_key(&mut app, &key(KeyCode::Enter)).is_some());
        assert_eq!(app.input, "/commit ");
        assert!(app.slash_picker.is_none());
    }

    #[test]
    fn slash_picker_esc_closes_without_selection() {
        let mut app = app_with_commands();
        app.input = "/".into();
        sync_app_picker(&mut app);
        assert!(app.slash_picker.is_some());

        assert!(handle_app_picker_key(&mut app, &key(KeyCode::Esc)).is_some());
        assert!(app.slash_picker.is_none());
    }

    #[test]
    fn slash_picker_tab_completes_command() {
        let mut app = app_with_commands();
        app.input = "/he".into();
        sync_app_picker(&mut app);

        assert!(handle_app_picker_key(&mut app, &key(KeyCode::Tab)).is_some());
        assert_eq!(app.input, "/help ");
        assert!(app.slash_picker.is_none());
    }

    #[test]
    fn slash_picker_inactive_when_no_commands() {
        let mut app = crate::tui::app::AppState::new();
        app.input = "/".into();
        sync_app_picker(&mut app);
        assert!(app.slash_picker.is_none());
    }

    // -- filtering --

    #[test]
    fn filter_returns_all_when_just_slash() {
        let cmds = test_commands();
        let matches = filtered_slash_commands("/", &cmds);
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn filter_narrows_by_prefix() {
        let cmds = test_commands();
        let matches = filtered_slash_commands("/c", &cmds);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].name, "clear");
        assert_eq!(matches[1].name, "commit");
    }

    #[test]
    fn filter_case_insensitive() {
        let cmds = test_commands();
        let matches = filtered_slash_commands("/H", &cmds);
        // "H" prefix-matches "help", and fuzzy-matches "Show help" description
        // for others, but "help" should be the top (prefix match).
        assert!(!matches.is_empty());
        assert_eq!(matches[0].name, "help");
    }

    #[test]
    fn filter_matches_alias_but_returns_canonical_command() {
        let cmds = test_commands();
        let matches = filtered_slash_commands("/ci", &cmds);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "commit");
    }

    #[test]
    fn filter_alias_prefix_does_not_outrank_canonical_prefix() {
        let cmds = test_commands();
        let matches = filtered_slash_commands("/c", &cmds);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].name, "clear");
        assert_eq!(matches[1].name, "commit");
    }

    #[test]
    fn filter_exact_ultrawork_alias_matches_only_canonical_command() {
        let cmds = vec![
            SlashCommand {
                name: "ultrawork".into(),
                description: "Enable coordinator mode".into(),
                input: None,
                category: None,
                aliases: vec!["ulw".into()],
            },
            SlashCommand {
                name: "update".into(),
                description: "Update dependencies".into(),
                input: None,
                category: None,
                aliases: Vec::new(),
            },
        ];

        let matches = filtered_slash_commands("/ulw", &cmds);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "ultrawork");
    }

    #[test]
    fn sync_selecting_alias_match_completes_to_canonical_command() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/ci", &cmds);

        let result = handle_key(&mut picker, &key(KeyCode::Tab));
        assert_eq!(
            result,
            Some(PickerKeyResult::Completed {
                input: "/commit ".into(),
                cursor: 8,
            })
        );
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let cmds = test_commands();
        let matches = filtered_slash_commands("/zzz", &cmds);
        assert!(matches.is_empty());
    }

    // -- sync --

    #[test]
    fn sync_opens_on_slash_prefix() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);
        assert!(picker.is_some());
        let state = picker.as_ref().unwrap();
        // Alphabetical sort: clear < commit < help.
        assert_eq!(state.focused_value(), Some("clear".into()));
    }

    #[test]
    fn sync_closes_without_slash() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);
        assert!(picker.is_some());
        sync(&mut picker, "hello", &cmds);
        assert!(picker.is_none());
    }

    #[test]
    fn sync_closes_when_no_commands() {
        let mut picker = None;
        sync(&mut picker, "/", &[]);
        assert!(picker.is_none());
    }

    #[test]
    fn sync_closes_when_no_matches() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/zzz", &cmds);
        assert!(picker.is_none());
    }

    #[test]
    fn sync_preserves_focus_on_reset() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);

        // Move focus to "commit" (second item: clear→commit).
        handle_key(&mut picker, &key(KeyCode::Down));
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("commit".into())
        );

        // User types more — filter narrows but "commit" is still present.
        sync(&mut picker, "/c", &cmds);
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("commit".into())
        );
    }

    #[test]
    fn sync_falls_back_to_first_when_focus_gone() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);

        // Focus on "clear" (first alphabetically).
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("clear".into())
        );

        // User types "/he" — "clear" is gone, should fall back to first match.
        sync(&mut picker, "/he", &cmds);
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("help".into())
        );
    }

    // -- key handling --

    #[test]
    fn key_returns_none_when_picker_closed() {
        let mut picker = None;
        assert!(handle_key(&mut picker, &key(KeyCode::Down)).is_none());
    }

    #[test]
    fn key_down_moves_focus_next() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);
        // Alphabetical: clear(0), commit(1), help(2).
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("clear".into())
        );

        let result = handle_key(&mut picker, &key(KeyCode::Down));
        assert_eq!(result, Some(PickerKeyResult::Consumed));
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("commit".into())
        );
    }

    #[test]
    fn key_up_moves_focus_previous() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);

        handle_key(&mut picker, &key(KeyCode::Down));
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("commit".into())
        );

        let result = handle_key(&mut picker, &key(KeyCode::Up));
        assert_eq!(result, Some(PickerKeyResult::Consumed));
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("clear".into())
        );
    }

    #[test]
    fn key_down_wraps_at_bottom() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);

        // Move to last item: clear→commit→help.
        handle_key(&mut picker, &key(KeyCode::Down)); // commit
        handle_key(&mut picker, &key(KeyCode::Down)); // help
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("help".into())
        );

        // Wrap to first.
        handle_key(&mut picker, &key(KeyCode::Down));
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("clear".into())
        );
    }

    #[test]
    fn key_up_wraps_at_top() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("clear".into())
        );

        // Wrap to last.
        handle_key(&mut picker, &key(KeyCode::Up));
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("help".into())
        );
    }

    #[test]
    fn key_enter_completes_focused() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);
        handle_key(&mut picker, &key(KeyCode::Down)); // focus "commit"

        let result = handle_key(&mut picker, &key(KeyCode::Enter));
        assert_eq!(
            result,
            Some(PickerKeyResult::Selected {
                input: "/commit ".into(),
                cursor: 8,
            })
        );
        assert!(picker.is_none()); // picker closes
    }

    #[test]
    fn key_tab_completes_with_trailing_space() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/he", &cmds);
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("help".into())
        );

        let result = handle_key(&mut picker, &key(KeyCode::Tab));
        assert_eq!(
            result,
            Some(PickerKeyResult::Completed {
                input: "/help ".into(),
                cursor: 6,
            })
        );
        assert!(picker.is_none());
    }

    #[test]
    fn key_esc_closes_without_selection() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);
        assert!(picker.is_some());

        let result = handle_key(&mut picker, &key(KeyCode::Esc));
        assert_eq!(result, Some(PickerKeyResult::Closed));
        assert!(picker.is_none());
    }

    #[test]
    fn key_other_returns_none() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);

        let result = handle_key(&mut picker, &key(KeyCode::Char('a')));
        assert!(result.is_none());
    }

    #[test]
    fn key_page_down_jumps() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("clear".into())
        );

        let result = handle_key(&mut picker, &key(KeyCode::PageDown));
        assert_eq!(result, Some(PickerKeyResult::Consumed));
        // Jumps to last: help.
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("help".into())
        );
    }

    #[test]
    fn key_page_up_at_top_stays() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);

        let result = handle_key(&mut picker, &key(KeyCode::PageUp));
        assert_eq!(result, Some(PickerKeyResult::Consumed));
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("clear".into())
        );
    }

    // -- render data --

    #[test]
    fn render_data_returns_none_when_closed() {
        let cmds = test_commands();
        assert!(render_data(&None, &cmds, "/").is_none());
    }

    #[test]
    fn render_data_returns_visible_rows() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);

        let data = render_data(&picker, &cmds, "/").unwrap();
        assert_eq!(data.rows.len(), 3);
        // Alphabetical: clear, commit, help.
        assert_eq!(data.rows[0].name, "clear");
        assert!(data.rows[0].is_focused);
        assert_eq!(data.rows[1].name, "commit");
        assert!(!data.rows[1].is_focused);
        assert_eq!(data.rows[2].name, "help");
        assert!(!data.rows[2].is_focused);
    }

    #[test]
    fn render_data_tracks_focus_change() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/", &cmds);
        handle_key(&mut picker, &key(KeyCode::Down));

        let data = render_data(&picker, &cmds, "/").unwrap();
        assert!(!data.rows[0].is_focused);
        assert!(data.rows[1].is_focused); // "commit" now focused
    }

    #[test]
    fn render_data_filters_match_input() {
        let cmds = test_commands();
        let mut picker = None;
        sync(&mut picker, "/c", &cmds);

        let data = render_data(&picker, &cmds, "/c").unwrap();
        assert_eq!(data.rows.len(), 2);
        assert_eq!(data.rows[0].name, "clear");
        assert_eq!(data.rows[1].name, "commit");
    }

    // -- viewport with many commands --

    #[test]
    fn viewport_scrolls_with_many_commands() {
        let mut cmds = Vec::new();
        for i in 0..20 {
            cmds.push(SlashCommand {
                name: format!("cmd{i:02}"),
                description: format!("Command {i}"),
                input: None,
                category: None,
                aliases: Vec::new(),
            });
        }
        let mut picker = None;
        sync(&mut picker, "/", &cmds);

        let state = picker.as_ref().unwrap();
        let visible = state.nav().visible_options();
        // Should show at most picker_visible_count() options.
        assert_eq!(visible.len(), picker_visible_count());

        // Navigate down past the viewport.
        for _ in 0..picker_visible_count() {
            handle_key(&mut picker, &key(KeyCode::Down));
        }

        let state = picker.as_ref().unwrap();
        let visible = state.nav().visible_options();
        assert_eq!(visible.len(), picker_visible_count());
        // The first visible option should have scrolled.
        assert_eq!(state.nav().visible_from_index(), 1);
    }
}
