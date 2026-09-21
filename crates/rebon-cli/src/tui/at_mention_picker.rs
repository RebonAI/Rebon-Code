//! `@` mention picker — wires `rebon-customselect`'s navigation
//! reducer into the `@` mention overlay.
//!
//! Triggered when the user types `@` at the start of input or after
//! whitespace, this picker shows matching files from the async
//! [`crate::file_scanner::FileIndex`] and lets the user navigate
//! them with the `NavigationState` reducer from `rebon-customselect`.
//!
//! ## Non-blocking architecture
//!
//! A 50ms debounce avoids blocking cursor movement, implemented
//! with a timestamp-based approach:
//!
//! * Each keystroke records `last_query_change` = `Instant::now()`.
//! * `sync()` only performs the fuzzy search when 50ms have elapsed
//!   since the last query change — until then, the picker stays in
//!   its previous state (no lag on cursor movement).
//! * The file list itself is populated asynchronously by the
//!   [`crate::file_scanner`] — the event loop is never blocked.
//!
//! ## Mention types
//!
//! * **Files** — `@src/main.rs`, `@Cargo.toml` (primary, from FileIndex)
//! * **Agents / team members** — `@researcher` (future: from agent registry)
//! * **MCP resources** — `@server:resource/path` (future: from MCP client)

use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use rebon_customselect::{
    NavigationAction, NavigationProps, NavigationState, OptionWithDescription,
    DEFAULT_VISIBLE_OPTION_COUNT,
};
use rebon_tui::promptinput::clamp_cursor_offset;

use crate::file_scanner::{local_dir_candidates, FileIndex, FileScanStatus};

/// Maximum visible rows in the mention picker overlay.
const MAX_VISIBLE: usize = 8;
/// Maximum fuzzy search results.
const MAX_RESULTS: usize = 15;
/// Maximum direct filesystem results for empty/path-like queries.
const LOCAL_DIR_RESULT_LIMIT: usize = 100;
/// Debounce delay in milliseconds for picker updates.
const DEBOUNCE_MS: u64 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MentionEmptyState {
    Loading,
    NoMatches,
    ScanFailed,
}

impl MentionEmptyState {
    fn label(self) -> &'static str {
        match self {
            Self::Loading => "Indexing files…",
            Self::NoMatches => "No matching files",
            Self::ScanFailed => "File scan unavailable",
        }
    }
}

fn picker_visible_count() -> usize {
    DEFAULT_VISIBLE_OPTION_COUNT.max(MAX_VISIBLE)
}

/// State for the `@` mention picker overlay.
#[derive(Debug, Clone)]
pub struct AtMentionPickerState {
    /// Navigation reducer from `rebon-customselect`.
    nav: NavigationState<String>,
    /// Start byte offset of the `@token` in the input buffer.
    pub token_start: usize,
    /// The query that produced the current navigation options.
    current_query: String,
    /// When the query last changed (for debounce).
    last_query_change: Instant,
    /// Whether the current results are up-to-date with the query.
    results_fresh: bool,
    /// Empty-state row to render when there are no real file options.
    empty_state: MentionEmptyState,
}

impl AtMentionPickerState {
    fn new(
        results: &[PickerItem],
        token_start: usize,
        query: String,
        empty_state: MentionEmptyState,
    ) -> Self {
        let options = items_to_options(results);
        let nav = NavigationState::new(NavigationProps {
            visible_option_count: Some(picker_visible_count()),
            options,
            initial_focus_value: None,
            focus_value: None,
        });
        Self {
            nav,
            token_start,
            current_query: query,
            last_query_change: Instant::now(),
            results_fresh: true,
            empty_state,
        }
    }

    fn dispatch_nav(&mut self, action: NavigationAction<String>) {
        self.nav = self.nav.clone().dispatch(action);
    }

    /// The currently focused item value (file path).
    pub fn focused_value(&self) -> Option<String> {
        self.nav.validated_focused_value()
    }
}

/// Internal item shape for the picker (projected from FileIndex results).
#[derive(Debug, Clone)]
struct PickerItem {
    /// Value used as the option id (the file path).
    value: String,
    /// Display text (same as path, or path/ for dirs).
    display: String,
}

fn items_to_options(items: &[PickerItem]) -> Vec<OptionWithDescription<String>> {
    items
        .iter()
        .map(|item| OptionWithDescription::text(item.display.clone(), item.value.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Token detection
// ---------------------------------------------------------------------------

/// Find the `@token` in the input that the cursor is inside.
///
/// Triggers when `@` appears at position 0 or after whitespace.
/// Returns `Some((byte_offset_of_at, query_after_at))`.
pub fn find_at_token(input: &str, cursor: usize) -> Option<(usize, String)> {
    let cursor = clamp_cursor_offset(input, cursor);
    let before_cursor = &input[..cursor];
    let mut at_pos = None;
    for (i, ch) in before_cursor.char_indices().rev() {
        if ch == '@' {
            if i == 0 || before_cursor[..i].ends_with(char::is_whitespace) {
                at_pos = Some(i);
                break;
            } else {
                return None;
            }
        }
        if ch.is_whitespace() {
            return None;
        }
    }
    let at_pos = at_pos?;
    let query = before_cursor[at_pos + 1..].to_string();
    Some((at_pos, query))
}

// ---------------------------------------------------------------------------
// Sync (called each frame)
// ---------------------------------------------------------------------------

/// Sync the `@` mention picker state each frame.
///
/// Uses a 50ms debounce: when the query changes, the picker waits
/// before performing the fuzzy search so cursor movement stays snappy.
/// The file index is queried only after the debounce period elapses.
pub fn sync(
    picker: &mut Option<AtMentionPickerState>,
    input: &str,
    cursor: usize,
    cwd: &str,
    file_index: &FileIndex,
    file_scan_status: &FileScanStatus,
) {
    let Some((at_pos, query)) = find_at_token(input, cursor) else {
        *picker = None;
        return;
    };

    if is_local_dir_query(&query) {
        tracing::debug!(cwd, query, "@ mention: sync local-only query");
    }

    match picker {
        Some(state) => {
            state.token_start = at_pos;
            if state.current_query != query {
                // Query changed — record the change time but don't search yet.
                state.current_query = query.clone();
                state.last_query_change = Instant::now();
                state.results_fresh = false;
            }

            // Check debounce: only search after DEBOUNCE_MS. Also refresh an
            // already-open empty picker when scanner status changes its empty
            // state (e.g. Scanning -> Complete/Failed/TimedOut), even if the
            // query and index are unchanged.
            let visible_empty = state.nav.visible_options().is_empty();
            let next_empty_state = visible_empty
                .then(|| empty_state_for(file_index, file_scan_status, &state.current_query, &[]));
            let should_refresh_empty = state.results_fresh
                && visible_empty
                && (!file_index.is_empty()
                    || next_empty_state.is_some_and(|next| next != state.empty_state));
            if (!state.results_fresh
                && state.last_query_change.elapsed().as_millis() >= DEBOUNCE_MS as u128)
                || should_refresh_empty
            {
                let results = search_files(cwd, file_index, &query);
                state.empty_state = empty_state_for(file_index, file_scan_status, &query, &results);
                let options = items_to_options(&results);
                let current_focus = state.nav.validated_focused_value();
                state.dispatch_nav(NavigationAction::Reset {
                    options,
                    visible_option_count: Some(picker_visible_count()),
                    focus_value: current_focus,
                });
                state.results_fresh = true;
            }
        }
        None => {
            // First open — search immediately (no debounce on open). Keep the picker
            // open even before the async index has produced results so `@` gives
            // visible, refreshable state instead of appearing stuck.
            let results = search_files(cwd, file_index, &query);
            let empty_state = empty_state_for(file_index, file_scan_status, &query, &results);
            *picker = Some(AtMentionPickerState::new(
                &results,
                at_pos,
                query,
                empty_state,
            ));
        }
    }
}

fn empty_state_for(
    file_index: &FileIndex,
    file_scan_status: &FileScanStatus,
    query: &str,
    results: &[PickerItem],
) -> MentionEmptyState {
    if !results.is_empty() || !file_index.is_empty() || is_local_dir_query(query) {
        return MentionEmptyState::NoMatches;
    }

    match file_scan_status {
        FileScanStatus::Scanning => MentionEmptyState::Loading,
        FileScanStatus::Complete => MentionEmptyState::NoMatches,
        FileScanStatus::Failed(_) | FileScanStatus::TimedOut(_) => MentionEmptyState::ScanFailed,
    }
}

/// Query local directory fast path and/or file index, then convert results to picker items.
fn search_files(cwd: &str, file_index: &FileIndex, query: &str) -> Vec<PickerItem> {
    if is_local_dir_query(query) {
        let local_results = local_dir_candidates(cwd, query, LOCAL_DIR_RESULT_LIMIT)
            .unwrap_or_else(|err| {
                tracing::debug!(
                    %err,
                    cwd,
                    query,
                    result_count = 0usize,
                    "@ mention: local directory completion failed"
                );
                Vec::new()
            });
        if local_results.is_empty() {
            tracing::debug!(
                cwd,
                query,
                result_count = 0usize,
                "@ mention: local directory completion returned no results"
            );
        } else {
            tracing::debug!(
                cwd,
                query,
                result_count = local_results.len(),
                "@ mention: local directory completion returned results"
            );
        }
        return local_results
            .into_iter()
            .map(|r| picker_item_from_path(r.path, r.is_directory))
            .collect();
    }

    file_index
        .search(query, MAX_RESULTS)
        .into_iter()
        .map(|r| picker_item_from_path(r.path, r.is_directory))
        .collect()
}

fn is_local_dir_query(query: &str) -> bool {
    query.is_empty() || query.contains(['/', '\\'])
}

fn picker_item_from_path(path: String, is_directory: bool) -> PickerItem {
    let path = path.replace('\\', "/");
    let display = if is_directory {
        format!("{}/", path.trim_end_matches('/'))
    } else {
        path
    };
    let value = display.clone();
    PickerItem { value, display }
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

/// Result of handling a key in the `@` mention picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionKeyResult {
    /// Navigation consumed the key.
    Consumed,
    /// An item was selected. The caller should apply it with
    /// [`apply_replacement`] so stale picker offsets remain recoverable.
    Selected {
        /// Replacement text (e.g. `"@src/main.rs "`).
        replacement: String,
        /// Byte offset where the picker last observed the `@token`.
        token_start: usize,
    },
    /// Tab-completed (same as selected).
    Completed {
        /// Replacement text.
        replacement: String,
        /// Byte offset where the picker last observed the `@token`.
        token_start: usize,
    },
    /// Picker closed without selection.
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MentionApplyError {
    NoActiveToken,
    StaleTokenStart { cached: usize, current: usize },
}

pub(crate) fn apply_replacement(
    input: &str,
    cursor: usize,
    cached_token_start: usize,
    replacement: &str,
) -> Result<(String, usize), MentionApplyError> {
    let cursor = clamp_cursor_offset(input, cursor);
    let Some((token_start, _)) = find_at_token(input, cursor) else {
        return Err(MentionApplyError::NoActiveToken);
    };
    if token_start != cached_token_start {
        return Err(MentionApplyError::StaleTokenStart {
            cached: cached_token_start,
            current: token_start,
        });
    }

    let mut next = String::with_capacity(input.len() - (cursor - token_start) + replacement.len());
    next.push_str(&input[..token_start]);
    next.push_str(replacement);
    next.push_str(&input[cursor..]);
    Ok((next, token_start + replacement.len()))
}

/// Handle a key event in the `@` mention picker.
pub fn handle_key(
    picker: &mut Option<AtMentionPickerState>,
    key: &KeyEvent,
) -> Option<MentionKeyResult> {
    let state = picker.as_mut()?;

    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    match key.code {
        KeyCode::Up => {
            state.dispatch_nav(NavigationAction::FocusPreviousOption);
            Some(MentionKeyResult::Consumed)
        }
        KeyCode::Down => {
            state.dispatch_nav(NavigationAction::FocusNextOption);
            Some(MentionKeyResult::Consumed)
        }
        KeyCode::PageUp => {
            state.dispatch_nav(NavigationAction::FocusPreviousPage);
            Some(MentionKeyResult::Consumed)
        }
        KeyCode::PageDown => {
            state.dispatch_nav(NavigationAction::FocusNextPage);
            Some(MentionKeyResult::Consumed)
        }
        KeyCode::Enter => {
            let result = make_selection_result(state, false);
            *picker = None;
            Some(result)
        }
        KeyCode::Tab => {
            let result = make_selection_result(state, true);
            *picker = None;
            Some(result)
        }
        KeyCode::Esc => {
            *picker = None;
            Some(MentionKeyResult::Closed)
        }
        _ => None,
    }
}

fn make_selection_result(state: &AtMentionPickerState, is_tab: bool) -> MentionKeyResult {
    let path = match state.focused_value() {
        Some(p) => p,
        None => return MentionKeyResult::Closed,
    };
    let replacement = if path.ends_with('/') {
        format!("@{path}")
    } else {
        format!("@{path} ")
    };
    let token_start = state.token_start;
    if is_tab {
        MentionKeyResult::Completed {
            replacement,
            token_start,
        }
    } else {
        MentionKeyResult::Selected {
            replacement,
            token_start,
        }
    }
}

// ---------------------------------------------------------------------------
// Render data
// ---------------------------------------------------------------------------

/// One row in the mention picker overlay.
#[derive(Debug, Clone)]
pub struct MentionPickerRow {
    /// Display text for this item (file path).
    pub display: String,
    /// Whether this row is focused.
    pub is_focused: bool,
    /// Whether this row represents a selectable file/directory option.
    pub is_selectable: bool,
}

/// Data needed to render the `@` mention picker overlay.
#[derive(Debug, Clone)]
pub struct MentionRenderData {
    /// Visible rows.
    pub rows: Vec<MentionPickerRow>,
}

/// Project render data for the mention picker.
pub fn render_data(picker: &Option<AtMentionPickerState>) -> Option<MentionRenderData> {
    let state = picker.as_ref()?;
    let focused = state.focused_value();
    let visible = state.nav.visible_options();
    if visible.is_empty() {
        return Some(MentionRenderData {
            rows: vec![MentionPickerRow {
                display: state.empty_state.label().to_string(),
                is_focused: false,
                is_selectable: false,
            }],
        });
    }

    let rows = visible
        .iter()
        .map(|vo| {
            let value = vo.option.value().clone();
            MentionPickerRow {
                display: vo.option.label().to_string(),
                is_focused: focused.as_ref() == Some(&value),
                is_selectable: true,
            }
        })
        .collect();

    Some(MentionRenderData { rows })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_index() -> FileIndex {
        let mut idx = FileIndex::default();
        idx.merge(vec![
            "Cargo.toml".into(),
            "src/main.rs".into(),
            "README.md".into(),
            "src/lib.rs".into(),
            "crates/rebon-cli/src/main.rs".into(),
        ]);
        idx
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE)
    }

    // -- token detection --

    #[test]
    fn find_at_token_at_start() {
        assert_eq!(find_at_token("@Car", 4), Some((0, "Car".into())));
    }

    #[test]
    fn find_at_token_after_space() {
        assert_eq!(find_at_token("hello @Car", 10), Some((6, "Car".into())));
    }

    #[test]
    fn find_at_token_just_at() {
        assert_eq!(find_at_token("@", 1), Some((0, "".into())));
    }

    #[test]
    fn find_at_token_keeps_nested_local_path_query() {
        assert_eq!(
            find_at_token("@mods_src/watcher", "@mods_src/watcher".len()),
            Some((0, "mods_src/watcher".into()))
        );
    }

    #[test]
    fn find_at_token_no_at() {
        assert_eq!(find_at_token("hello", 5), None);
    }

    #[test]
    fn find_at_token_mid_word_no_trigger() {
        assert_eq!(find_at_token("email@host", 10), None);
    }

    #[test]
    fn find_at_token_clamps_cursor_past_end() {
        assert_eq!(find_at_token("@Car", usize::MAX), Some((0, "Car".into())));
    }

    #[test]
    fn find_at_token_handles_cursor_inside_multibyte_character() {
        assert_eq!(find_at_token("你 @Car", 2), None);
    }

    // -- sync with file index --

    const MISSING_TEST_CWD: &str = "./__rebon_missing_at_mention_test_dir__";

    fn sync_picker(
        picker: &mut Option<AtMentionPickerState>,
        input: &str,
        cursor: usize,
        idx: &FileIndex,
    ) {
        sync(
            picker,
            input,
            cursor,
            MISSING_TEST_CWD,
            idx,
            &FileScanStatus::Scanning,
        );
    }

    fn sync_picker_with_status(
        picker: &mut Option<AtMentionPickerState>,
        input: &str,
        cursor: usize,
        idx: &FileIndex,
        status: &FileScanStatus,
    ) {
        sync(picker, input, cursor, MISSING_TEST_CWD, idx, status);
    }

    fn sync_picker_in_cwd(
        picker: &mut Option<AtMentionPickerState>,
        input: &str,
        cursor: usize,
        cwd: &str,
        idx: &FileIndex,
        status: &FileScanStatus,
    ) {
        sync(picker, input, cursor, cwd, idx, status);
    }

    #[test]
    fn sync_opens_on_at_prefix() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "@", 1, &idx);
        assert!(picker.is_some());
    }

    #[test]
    fn sync_closes_without_at() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "@", 1, &idx);
        assert!(picker.is_some());
        sync_picker(&mut picker, "hello", 5, &idx);
        assert!(picker.is_none());
    }

    #[test]
    fn sync_refreshes_token_start_when_query_is_unchanged() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "text @", 6, &idx);
        assert_eq!(picker.as_ref().unwrap().token_start, 5);

        sync_picker(&mut picker, "@", 1, &idx);
        assert_eq!(picker.as_ref().unwrap().token_start, 0);
    }

    #[test]
    fn sync_shows_loading_state_with_empty_index_for_fuzzy_query() {
        let idx = FileIndex::default();
        let mut picker = None;
        sync_picker(&mut picker, "@zz", 3, &idx);
        assert!(picker.is_some());

        let data = render_data(&picker).expect("empty picker should render loading row");
        assert_eq!(data.rows.len(), 1);
        assert!(data.rows[0].display.contains("Indexing files"));
        assert!(!data.rows[0].is_focused);
        assert!(!data.rows[0].is_selectable);

        let result = handle_key(&mut picker, &key(KeyCode::Enter));
        assert_eq!(result, Some(MentionKeyResult::Closed));
    }

    #[test]
    fn sync_shows_scan_failed_state_after_empty_terminal_failure() {
        let idx = FileIndex::default();
        let mut picker = None;
        sync_picker_with_status(
            &mut picker,
            "@zz",
            3,
            &idx,
            &FileScanStatus::TimedOut("git ls-files".to_string()),
        );

        let data = render_data(&picker).expect("failed picker should render empty row");
        assert_eq!(data.rows.len(), 1);
        assert_eq!(data.rows[0].display, "File scan unavailable");
        assert!(!data.rows[0].is_focused);
        assert!(!data.rows[0].is_selectable);
    }

    #[test]
    fn sync_shows_no_match_after_empty_complete_scan() {
        let idx = FileIndex::default();
        let mut picker = None;
        sync_picker_with_status(&mut picker, "@zz", 3, &idx, &FileScanStatus::Complete);

        let data = render_data(&picker).expect("complete empty picker should render empty row");
        assert_eq!(data.rows.len(), 1);
        assert_eq!(data.rows[0].display, "No matching files");
        assert!(!data.rows[0].is_focused);
        assert!(!data.rows[0].is_selectable);
    }

    #[test]
    fn sync_filters_by_query_on_open() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "@main", 5, &idx);
        assert!(picker.is_some());
        // Should have "src/main.rs" as the top result.
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("crates/rebon-cli/src/main.rs".into())
        );
    }

    #[test]
    fn sync_can_select_directory_results() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("crates/rebon-cli")).unwrap();
        let idx = test_index();
        let mut picker = None;
        sync_picker_in_cwd(
            &mut picker,
            "@crates/rebon-cli",
            17,
            temp.path().to_str().unwrap(),
            &idx,
            &FileScanStatus::Scanning,
        );

        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("crates/rebon-cli/".to_string())
        );
    }

    #[test]
    fn sync_empty_query_uses_local_cwd_while_indexing() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let idx = FileIndex::default();
        let mut picker = None;

        sync_picker_in_cwd(
            &mut picker,
            "@",
            1,
            temp.path().to_str().unwrap(),
            &idx,
            &FileScanStatus::Scanning,
        );

        let data = render_data(&picker).unwrap();
        let displays: Vec<_> = data.rows.into_iter().map(|r| r.display).collect();
        assert_eq!(displays, vec!["src/".to_string(), "Cargo.toml".to_string()]);
    }

    #[test]
    fn sync_empty_query_does_not_fallback_to_file_index_when_local_cwd_has_no_candidates() {
        let temp = tempfile::tempdir().unwrap();
        // `node_modules` is skipped by local_dir_candidates, so the local fast path
        // returns no selectable candidates even though the directory is readable.
        std::fs::create_dir(temp.path().join("node_modules")).unwrap();
        let mut idx = FileIndex::default();
        idx.merge(
            (0..500)
                .map(|i| format!("global/path/file_{i}.rs"))
                .collect(),
        );
        let mut picker = None;

        sync_picker_in_cwd(
            &mut picker,
            "@",
            1,
            temp.path().to_str().unwrap(),
            &idx,
            &FileScanStatus::Complete,
        );

        let data = render_data(&picker).unwrap();
        assert_eq!(data.rows.len(), 1);
        assert_eq!(data.rows[0].display, "No matching files");
        assert!(!data.rows[0].is_selectable);
        assert!(!data
            .rows
            .iter()
            .any(|row| row.display.starts_with("global/path")));
        assert!(picker.as_ref().unwrap().focused_value().is_none());
    }

    #[test]
    fn sync_path_query_uses_local_direct_children_with_prefix_filter() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src/foo/deep")).unwrap();
        std::fs::create_dir(temp.path().join("src/foobar")).unwrap();
        std::fs::write(temp.path().join("src/foo.rs"), "\n").unwrap();
        std::fs::write(temp.path().join("src/foo/deep/file.rs"), "\n").unwrap();
        std::fs::write(temp.path().join("src/bar.rs"), "\n").unwrap();
        let idx = FileIndex::default();
        let mut picker = None;

        sync_picker_in_cwd(
            &mut picker,
            "@src/foo",
            8,
            temp.path().to_str().unwrap(),
            &idx,
            &FileScanStatus::Scanning,
        );

        let data = render_data(&picker).unwrap();
        let displays: Vec<_> = data.rows.into_iter().map(|r| r.display).collect();
        assert_eq!(
            displays,
            vec![
                "src/foo/".to_string(),
                "src/foobar/".to_string(),
                "src/foo.rs".to_string(),
            ]
        );
        assert!(!displays.iter().any(|path| path.contains("deep/file.rs")));
    }

    #[test]
    fn sync_path_query_matches_real_windows_case_while_indexing() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("mods_src/Watcher")).unwrap();
        std::fs::create_dir_all(temp.path().join("mods_src/Watcher.bak_mojibake")).unwrap();
        std::fs::write(temp.path().join("mods_src/Watcher/inner.rs"), "\n").unwrap();
        let idx = FileIndex::default();
        let mut picker = None;

        sync_picker_in_cwd(
            &mut picker,
            "@mods_src/watcher",
            17,
            temp.path().to_str().unwrap(),
            &idx,
            &FileScanStatus::Scanning,
        );

        let data = render_data(&picker).unwrap();
        let displays: Vec<_> = data.rows.into_iter().map(|r| r.display).collect();
        assert_eq!(
            displays,
            vec![
                "mods_src/Watcher/".to_string(),
                "mods_src/Watcher.bak_mojibake/".to_string(),
            ]
        );
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("mods_src/Watcher/".to_string())
        );
        assert!(!displays
            .iter()
            .any(|display| display.contains("Indexing files")));
    }

    #[test]
    fn sync_path_query_finds_watcher_after_many_non_matching_mods_src_children_while_indexing() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("mods_src")).unwrap();
        for i in 0..(LOCAL_DIR_RESULT_LIMIT * 3) {
            std::fs::create_dir(temp.path().join(format!("mods_src/aaa_nonmatch_{i:03}"))).unwrap();
            std::fs::write(
                temp.path().join(format!("mods_src/bbb_nonmatch_{i:03}.rs")),
                "\n",
            )
            .unwrap();
        }
        std::fs::create_dir_all(temp.path().join("mods_src/Watcher")).unwrap();
        std::fs::write(temp.path().join("mods_src/Watcher/inner.rs"), "\n").unwrap();
        let idx = FileIndex::default();
        let mut picker = None;

        sync_picker_in_cwd(
            &mut picker,
            "@mods_src/watcher",
            "@mods_src/watcher".len(),
            temp.path().to_str().unwrap(),
            &idx,
            &FileScanStatus::Scanning,
        );

        let data = render_data(&picker).unwrap();
        assert_eq!(data.rows.len(), 1);
        assert!(data.rows[0].is_selectable);
        assert_eq!(data.rows[0].display, "mods_src/Watcher/".to_string());
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("mods_src/Watcher/".to_string())
        );
        assert!(!data
            .rows
            .iter()
            .any(|row| row.display.contains("Indexing files")));
    }

    #[test]
    fn sync_path_query_matches_real_windows_case_after_scanner_failure() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("mods_src/Watcher")).unwrap();
        std::fs::write(temp.path().join("mods_src/Watcher/inner.rs"), "\n").unwrap();
        let idx = FileIndex::default();
        let mut picker = None;

        sync_picker_in_cwd(
            &mut picker,
            "@mods_src/watcher",
            "@mods_src/watcher".len(),
            temp.path().to_str().unwrap(),
            &idx,
            &FileScanStatus::Failed(
                "git ls-files failed; rg --files failed (rg not found)".to_string(),
            ),
        );

        let data = render_data(&picker).unwrap();
        assert_eq!(data.rows.len(), 1);
        assert!(data.rows[0].is_selectable);
        assert_eq!(data.rows[0].display, "mods_src/Watcher/".to_string());
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("mods_src/Watcher/".to_string())
        );
        assert!(!data.rows[0].display.contains("File scan unavailable"));
        assert!(!data.rows[0].display.contains("Indexing files"));
    }

    #[test]
    fn sync_path_query_supports_backslash_separator_while_indexing() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("mods_src/Watcher")).unwrap();
        let idx = FileIndex::default();
        let mut picker = None;

        sync_picker_in_cwd(
            &mut picker,
            r"@mods_src\watcher",
            r"@mods_src\watcher".len(),
            temp.path().to_str().unwrap(),
            &idx,
            &FileScanStatus::Scanning,
        );

        let data = render_data(&picker).unwrap();
        assert_eq!(data.rows.len(), 1);
        assert!(data.rows[0].is_selectable);
        assert_eq!(data.rows[0].display, "mods_src/Watcher/".to_string());
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("mods_src/Watcher/".to_string())
        );
    }

    #[test]
    fn sync_path_query_no_local_matches_does_not_show_loading_while_indexing() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("mods_src/Watcher")).unwrap();
        let idx = FileIndex::default();
        let mut picker = None;

        sync_picker_in_cwd(
            &mut picker,
            "@mods_src/defect",
            16,
            temp.path().to_str().unwrap(),
            &idx,
            &FileScanStatus::Scanning,
        );

        let data = render_data(&picker).unwrap();
        assert_eq!(data.rows.len(), 1);
        assert_eq!(data.rows[0].display, "No matching files");
        assert!(!data.rows[0].display.contains("Indexing files"));
        assert!(!data.rows[0].is_selectable);
        assert!(picker.as_ref().unwrap().focused_value().is_none());
    }

    #[test]
    fn sync_parent_query_keeps_directory_open_for_browsing_and_spaces_files() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("work/project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(temp.path().join("work/shared/nested")).unwrap();
        std::fs::write(temp.path().join("work/shared/Main.rs"), "\n").unwrap();
        let idx = FileIndex::default();
        let mut picker = None;

        let input = "@../sh";
        sync_picker_in_cwd(
            &mut picker,
            input,
            input.len(),
            cwd.to_str().unwrap(),
            &idx,
            &FileScanStatus::Scanning,
        );
        assert_eq!(
            picker.as_ref().unwrap().focused_value().as_deref(),
            Some("../shared/")
        );
        let result = handle_key(&mut picker, &key(KeyCode::Enter));
        let Some(MentionKeyResult::Selected {
            replacement,
            token_start,
        }) = result
        else {
            panic!("expected parent directory selection");
        };
        assert_eq!(replacement, "@../shared/");
        assert!(!replacement.ends_with(' '));

        let (input, cursor) =
            apply_replacement(input, input.len(), token_start, &replacement).unwrap();
        sync_picker_in_cwd(
            &mut picker,
            &input,
            cursor,
            cwd.to_str().unwrap(),
            &idx,
            &FileScanStatus::Scanning,
        );
        let displays: Vec<_> = render_data(&picker)
            .unwrap()
            .rows
            .into_iter()
            .map(|row| row.display)
            .collect();
        assert_eq!(
            displays,
            vec![
                "../shared/nested/".to_string(),
                "../shared/Main.rs".to_string()
            ]
        );

        let file_input = "@../shared/ma";
        picker = None;
        sync_picker_in_cwd(
            &mut picker,
            file_input,
            file_input.len(),
            cwd.to_str().unwrap(),
            &idx,
            &FileScanStatus::Scanning,
        );
        let result = handle_key(&mut picker, &key(KeyCode::Tab));
        assert!(matches!(
            result,
            Some(MentionKeyResult::Completed { replacement, .. })
                if replacement == "@../shared/Main.rs "
        ));
    }

    #[test]
    fn sync_parent_windows_query_inserts_forward_slashes() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("work/project");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(temp.path().join("work/shared")).unwrap();
        std::fs::write(temp.path().join("work/shared/Main.rs"), "\n").unwrap();
        let idx = FileIndex::default();
        let mut picker = None;
        let input = r"@..\shared\ma";

        sync_picker_in_cwd(
            &mut picker,
            input,
            input.len(),
            cwd.to_str().unwrap(),
            &idx,
            &FileScanStatus::Scanning,
        );
        assert_eq!(
            picker.as_ref().unwrap().focused_value().as_deref(),
            Some("../shared/Main.rs")
        );
        let result = handle_key(&mut picker, &key(KeyCode::Enter));
        assert!(matches!(
            result,
            Some(MentionKeyResult::Selected { replacement, .. })
                if replacement == "@../shared/Main.rs "
        ));
    }

    #[test]
    fn sync_path_escape_returns_no_local_results() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "\n").unwrap();
        std::fs::write(temp.path().join("inside.rs"), "\n").unwrap();
        let idx = FileIndex::default();

        for input in ["@nested/../", "@/tmp", "@C:/"] {
            let mut picker = None;
            sync_picker_in_cwd(
                &mut picker,
                input,
                input.len(),
                temp.path().to_str().unwrap(),
                &idx,
                &FileScanStatus::Scanning,
            );
            let data = render_data(&picker).unwrap();
            assert_eq!(data.rows.len(), 1, "input {input}");
            assert!(!data.rows[0].is_selectable, "input {input}");
        }
    }

    #[test]
    fn sync_path_query_shows_subtree_candidates_and_selectable_directory() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src/tools")).unwrap();
        std::fs::write(temp.path().join("src/main.rs"), "\n").unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "\n").unwrap();
        let mut idx = FileIndex::default();
        idx.merge(vec![
            "src/main.rs".into(),
            "src/tools/task.rs".into(),
            "src/lib.rs".into(),
            "scripts/src_alias.rs".into(),
        ]);
        let mut picker = None;
        sync_picker_in_cwd(
            &mut picker,
            "@src/",
            5,
            temp.path().to_str().unwrap(),
            &idx,
            &FileScanStatus::Scanning,
        );

        let data = render_data(&picker).unwrap();
        let displays: Vec<_> = data.rows.into_iter().map(|r| r.display).collect();
        assert_eq!(
            displays,
            vec![
                "src/tools/".to_string(),
                "src/lib.rs".to_string(),
                "src/main.rs".to_string(),
            ]
        );
        assert_eq!(
            picker.as_ref().unwrap().focused_value(),
            Some("src/tools/".to_string())
        );
    }

    #[test]
    fn sync_shows_no_match_state_with_non_empty_index() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "@zzzzzz", 7, &idx);
        assert!(picker.is_some());

        let data = render_data(&picker).expect("no-match picker should render empty row");
        assert_eq!(data.rows.len(), 1);
        assert_eq!(data.rows[0].display, "No matching files");
        assert!(!data.rows[0].is_focused);
        assert!(!data.rows[0].is_selectable);

        let result = handle_key(&mut picker, &key(KeyCode::Tab));
        assert_eq!(result, Some(MentionKeyResult::Closed));
    }

    #[test]
    fn sync_refreshes_empty_state_after_index_merge() {
        let mut idx = FileIndex::default();
        let mut picker = None;
        sync_picker(&mut picker, "@zz", 3, &idx);
        assert!(render_data(&picker).unwrap().rows[0]
            .display
            .contains("Indexing files"));

        idx.merge(vec!["src/main.rs".into(), "Cargo.toml".into()]);
        sync_picker(&mut picker, "@zz", 3, &idx);

        let data = render_data(&picker).unwrap();
        assert_eq!(data.rows.len(), 1);
        assert_eq!(data.rows[0].display, "No matching files");
        assert!(!data.rows[0].is_selectable);
        assert!(picker.as_ref().unwrap().focused_value().is_none());
    }

    #[test]
    fn sync_refreshes_open_empty_picker_on_terminal_status_transitions() {
        for (status, expected) in [
            (FileScanStatus::Complete, "No matching files"),
            (
                FileScanStatus::Failed("git ls-files failed".to_string()),
                "File scan unavailable",
            ),
            (
                FileScanStatus::TimedOut("git ls-files".to_string()),
                "File scan unavailable",
            ),
        ] {
            let idx = FileIndex::default();
            let mut picker = None;
            sync_picker_with_status(&mut picker, "@zz", 3, &idx, &FileScanStatus::Scanning);
            assert!(render_data(&picker).unwrap().rows[0]
                .display
                .contains("Indexing files"));

            sync_picker_with_status(&mut picker, "@zz", 3, &idx, &status);

            let data = render_data(&picker).expect("terminal status should refresh empty row");
            assert_eq!(data.rows.len(), 1);
            assert_eq!(data.rows[0].display, expected);
            assert!(!data.rows[0].is_selectable);
        }
    }

    #[test]
    fn apply_replacement_splices_current_mid_input_token() {
        let replacement = "@Cargo.toml ";
        let (input, cursor) =
            apply_replacement("fix @Carlater", 8, 4, replacement).expect("valid mention");

        assert_eq!(input, "fix @Cargo.toml later");
        assert_eq!(cursor, 4 + replacement.len());
    }

    #[test]
    fn apply_replacement_rejects_empty_input_instead_of_panicking() {
        assert_eq!(
            apply_replacement("", 0, 5, "@Cargo.toml "),
            Err(MentionApplyError::NoActiveToken)
        );
    }

    #[test]
    fn apply_replacement_rejects_stale_token_start() {
        assert_eq!(
            apply_replacement("@Car", 4, 5, "@Cargo.toml "),
            Err(MentionApplyError::StaleTokenStart {
                cached: 5,
                current: 0,
            })
        );
    }

    #[test]
    fn apply_replacement_clamps_cursor_past_end() {
        let replacement = "@Cargo.toml ";
        let (input, cursor) = apply_replacement("@Car", usize::MAX, 0, replacement)
            .expect("past-end cursor should recover");

        assert_eq!(input, replacement);
        assert_eq!(cursor, replacement.len());
    }

    // -- key handling --

    #[test]
    fn picker_items_normalize_windows_separators_and_directory_suffixes() {
        let directory = picker_item_from_path(r"..\shared".to_string(), true);
        assert_eq!(directory.value, "../shared/");
        assert_eq!(directory.display, "../shared/");

        let file = picker_item_from_path(r"..\shared\Main.rs".to_string(), false);
        assert_eq!(file.value, "../shared/Main.rs");
        assert_eq!(file.display, "../shared/Main.rs");
    }

    #[test]
    fn key_down_moves_focus() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "@s", 2, &idx);

        let first = picker.as_ref().unwrap().focused_value();
        let result = handle_key(&mut picker, &key(KeyCode::Down));
        assert_eq!(result, Some(MentionKeyResult::Consumed));
        let second = picker.as_ref().unwrap().focused_value();
        assert_ne!(first, second);
    }

    #[test]
    fn key_enter_selects() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "@main", 5, &idx);

        let result = handle_key(&mut picker, &key(KeyCode::Enter));
        match result {
            Some(MentionKeyResult::Selected {
                replacement,
                token_start,
                ..
            }) => {
                assert!(replacement.starts_with("@"));
                assert!(replacement.contains("main"));
                assert_eq!(token_start, 0);
            }
            other => panic!("expected Selected, got {other:?}"),
        }
        assert!(picker.is_none());
    }

    #[test]
    fn key_tab_completes() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "@READ", 5, &idx);

        let result = handle_key(&mut picker, &key(KeyCode::Tab));
        match result {
            Some(MentionKeyResult::Completed { replacement, .. }) => {
                assert!(replacement.starts_with("@"));
                assert!(replacement.contains("README"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(picker.is_none());
    }

    #[test]
    fn key_esc_closes() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "@", 1, &idx);

        let result = handle_key(&mut picker, &key(KeyCode::Esc));
        assert_eq!(result, Some(MentionKeyResult::Closed));
        assert!(picker.is_none());
    }

    #[test]
    fn key_returns_none_when_closed() {
        let mut picker = None;
        assert!(handle_key(&mut picker, &key(KeyCode::Down)).is_none());
    }

    // -- wrap-around --

    #[test]
    fn key_up_wraps_at_top() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "@s", 2, &idx);
        let first = picker.as_ref().unwrap().focused_value();

        handle_key(&mut picker, &key(KeyCode::Up)); // wrap to last
        let wrapped = picker.as_ref().unwrap().focused_value();
        assert_ne!(first, wrapped);
    }

    // -- render data --

    #[test]
    fn render_data_returns_rows() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "@s", 2, &idx);

        let data = render_data(&picker).unwrap();
        assert!(!data.rows.is_empty());
        assert!(data.rows.iter().any(|r| r.is_focused));
    }

    // -- mid-input --

    #[test]
    fn mention_mid_input() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "fix @Car", 8, &idx);
        assert!(picker.is_some());
        assert_eq!(picker.as_ref().unwrap().token_start, 4);
    }

    // -- debounce --

    #[test]
    fn debounce_delays_search_on_query_change() {
        let idx = test_index();
        let mut picker = None;
        sync_picker(&mut picker, "@", 1, &idx); // immediate on first open
        assert!(picker.is_some());

        let initial_query = picker.as_ref().unwrap().current_query.clone();
        assert_eq!(initial_query, "");

        // Simulate typing — query changes but debounce not elapsed.
        sync_picker(&mut picker, "@m", 2, &idx);
        assert!(picker.is_some());
        // Results should NOT be fresh yet (debounce pending).
        assert!(!picker.as_ref().unwrap().results_fresh);
    }
}
