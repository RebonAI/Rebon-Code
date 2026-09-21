//! Full-screen settings panel: tabs over a session's status, its editable
//! config options, and its usage.
//!
//! The panel owns three things and no more: which tab is on screen, which
//! config row is selected, and any text edit in progress. Everything it
//! shows is a projection the surface hands in — the informational tabs
//! arrive as finished rows, because what a status line says depends on a
//! live session this module cannot see, and the options arrive as plain
//! rows. Persisting a change is an action handed back, never a write
//! from here.
//!
//! The row builders those projections are made of live in
//! [`crate::settings_status`], [`crate::settings_tabs`] and
//! [`crate::settings_usage`].

use crate::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, PanelPane, PanelRow, PanelSplit,
    PanelTab, PanelView, RowTone, TextSpan, ViewSpec,
};

/// Stable id used for action routing.
pub const DIALOG_ID: &str = "settings";
/// Persist one config option. Values are `[config_id, value]`.
pub const ACTION_APPLY_CONFIG: &str = "apply-config";

const TITLE: &str = " Settings ";
const CONFIG_LIST_TITLE: &str = " Config options ";
const CONFIG_DETAIL_TITLE: &str = " Details ";
const NO_OPTIONS: &str = "(no config options available)";
const NO_SELECTION: &str = "Select an option to inspect it.";
const TEXT_EDIT_HINT: &str = "Enter to edit, Enter to apply, Esc to cancel";
const AVAILABLE_VALUES: &str = "Available values";
/// The cursor shown at the end of a value being typed.
const CURSOR: char = '█';

/// How the config tab divides its frame: a list wide enough to read
/// option names beside a detail pane, stacking on a narrow terminal.
const CONFIG_SPLIT: PanelSplit = PanelSplit {
    first_percent: 33,
    first_min_columns: 34,
    gap: 1,
    stack_below: 80,
    stacked_second_rows: 7,
};

/// One value a cycling config option can take.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigChoice {
    /// The value that gets persisted.
    pub value: String,
    /// What to show for it. The collector falls back to the value when
    /// there is no name, so this is never empty in practice.
    pub label: String,
}

/// One editable config option, as the panel needs it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigOptionRow {
    /// The id an apply action carries.
    pub id: String,
    /// The option's name, shown in the list and above the detail.
    pub name: String,
    /// What it does, shown under the name.
    pub description: Option<String>,
    /// Whether it is edited by typing rather than by cycling values.
    pub free_text: bool,
    /// The value in force right now.
    pub current_value: String,
    /// The values it cycles through. Empty for a free-text option.
    pub choices: Vec<ConfigChoice>,
}

impl ConfigOptionRow {
    /// The label for the current value, or the raw value when it is not
    /// one of the choices — which is what a free-text option always is.
    fn current_label(&self) -> String {
        self.choices
            .iter()
            .find(|choice| choice.value == self.current_value)
            .map(|choice| choice.label.clone())
            .unwrap_or_else(|| self.current_value.clone())
    }

    /// The value after the current one, wrapping round. `None` when
    /// there is nothing to cycle: a free-text option, or no choices.
    fn next_value(&self) -> Option<String> {
        if self.free_text || self.choices.is_empty() {
            return None;
        }
        let current = self
            .choices
            .iter()
            .position(|choice| choice.value == self.current_value);
        // An unrecognised current value cycles to the first choice
        // rather than staying stuck on a value not in the list.
        let next = match current {
            Some(index) if index + 1 < self.choices.len() => index + 1,
            _ => 0,
        };
        Some(self.choices[next].value.clone())
    }
}

/// The tab strip plus everything behind the tab currently on screen.
///
/// Rebuilt by the surface every frame, so a value changed elsewhere shows
/// up while the panel is open without the panel knowing anything about a
/// session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SettingsProjection {
    /// Tab names, in strip order.
    pub tabs: Vec<String>,
    /// Finished rows for the active tab, when it is not the config one.
    pub body: Vec<PanelRow>,
    /// Which tab shows the config list, when this build has one.
    pub config_tab: Option<usize>,
    /// The editable options.
    pub config: Vec<ConfigOptionRow>,
}

/// What opening the settings panel needs: which tab, and the first
/// projection so the opening frame paints live values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SettingsDialogOpen {
    /// Index into [`SettingsProjection::tabs`] of the tab to start on.
    pub active_tab: usize,
    /// The first projection.
    pub projection: SettingsProjection,
}

/// Reducer state for the settings panel.
#[derive(Debug, Clone)]
pub struct SettingsDialogState {
    active_tab: usize,
    config_selected_index: usize,
    editing_config_id: Option<String>,
    editing_value: String,
    projection: SettingsProjection,
}

impl SettingsDialogState {
    /// Open the panel on a tab, with its first projection.
    pub fn open(opened: &SettingsDialogOpen) -> Self {
        let mut state = Self {
            active_tab: opened.active_tab,
            config_selected_index: 0,
            editing_config_id: None,
            editing_value: String::new(),
            projection: SettingsProjection::default(),
        };
        state.set_projection(opened.projection.clone());
        state
    }

    /// Which tab is on screen. The surface reads this to know which
    /// tab's rows to build for the next frame.
    pub fn active_tab(&self) -> usize {
        self.active_tab
    }

    /// Hand the panel this frame's projection, clamping anything the
    /// panel remembers against it.
    pub fn set_projection(&mut self, projection: SettingsProjection) {
        self.projection = projection;
        self.sync();
    }

    /// Clamp tab and config state against the latest projection.
    ///
    /// A tab that went away, an option list that shrank, or an option
    /// being edited that is no longer there all have to be absorbed here:
    /// the panel is showing last frame's choices against this frame's
    /// data.
    fn sync(&mut self) {
        if !self.projection.tabs.is_empty() {
            self.active_tab = self.active_tab.min(self.projection.tabs.len() - 1);
        }
        self.config_selected_index = if self.projection.config.is_empty() {
            0
        } else {
            self.config_selected_index
                .min(self.projection.config.len() - 1)
        };
        if let Some(editing_id) = self.editing_config_id.as_ref() {
            if !self
                .projection
                .config
                .iter()
                .any(|option| option.id == *editing_id)
            {
                self.cancel_text_edit();
            }
        }
    }

    fn is_editing_text(&self) -> bool {
        self.editing_config_id.is_some()
    }

    fn on_config_tab(&self) -> bool {
        self.projection.config_tab == Some(self.active_tab)
    }

    fn cancel_text_edit(&mut self) {
        self.editing_config_id = None;
        self.editing_value.clear();
    }

    /// Apply one key against an explicit option list. Split out from
    /// [`DialogModel::on_key`] so the reducer can be driven with a chosen
    /// list instead of the last projection's.
    fn apply_key(&mut self, key: DialogKey, config: &[ConfigOptionRow]) -> DialogOutcome {
        if self.is_editing_text() {
            return self.apply_editing_key(key);
        }
        match key {
            DialogKey::Escape => DialogOutcome::Close,
            DialogKey::Left | DialogKey::BackTab => {
                self.switch_tab(-1);
                DialogOutcome::None
            }
            DialogKey::Right | DialogKey::Tab => {
                self.switch_tab(1);
                DialogOutcome::None
            }
            DialogKey::Up if self.on_config_tab() => {
                self.move_config_selection(-1, config.len());
                DialogOutcome::None
            }
            DialogKey::Down if self.on_config_tab() => {
                self.move_config_selection(1, config.len());
                DialogOutcome::None
            }
            DialogKey::Enter if self.on_config_tab() => {
                if self.begin_text_edit(config) {
                    DialogOutcome::None
                } else {
                    self.advance_config_value(config)
                }
            }
            // A space cycles whether or not a modifier was held: there is
            // no chord on this panel it could be mistaken for.
            DialogKey::Char { value: ' ', .. } if self.on_config_tab() => {
                self.advance_config_value(config)
            }
            _ => DialogOutcome::None,
        }
    }

    /// Keys while a free-text value is being typed. Escape abandons the
    /// edit rather than closing the panel, which is the one place the
    /// two disagree.
    fn apply_editing_key(&mut self, key: DialogKey) -> DialogOutcome {
        match key {
            DialogKey::Escape => {
                self.cancel_text_edit();
                DialogOutcome::None
            }
            DialogKey::Enter => {
                let config_id = self.editing_config_id.take().unwrap_or_default();
                let value = std::mem::take(&mut self.editing_value);
                apply_config(config_id, value)
            }
            DialogKey::Backspace => {
                self.editing_value.pop();
                DialogOutcome::None
            }
            DialogKey::Char { value, plain: true } => {
                self.editing_value.push(value);
                DialogOutcome::None
            }
            _ => DialogOutcome::None,
        }
    }

    fn switch_tab(&mut self, delta: isize) {
        let count = self.projection.tabs.len();
        if count == 0 {
            return;
        }
        self.active_tab = (self.active_tab as isize + delta).rem_euclid(count as isize) as usize;
    }

    fn move_config_selection(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.config_selected_index = 0;
            return;
        }
        let next = (self.config_selected_index as isize + delta)
            .clamp(0, len.saturating_sub(1) as isize) as usize;
        self.config_selected_index = next;
    }

    /// Start typing a free-text option's value. `false` for anything
    /// else, which is the caller's cue to cycle instead.
    fn begin_text_edit(&mut self, config: &[ConfigOptionRow]) -> bool {
        let Some(option) = config.get(self.config_selected_index) else {
            return false;
        };
        if !option.free_text {
            return false;
        }
        self.editing_config_id = Some(option.id.clone());
        self.editing_value = option.current_value.clone();
        true
    }

    fn advance_config_value(&mut self, config: &[ConfigOptionRow]) -> DialogOutcome {
        let Some(option) = config.get(self.config_selected_index) else {
            return DialogOutcome::None;
        };
        option
            .next_value()
            .map(|value| apply_config(option.id.clone(), value))
            .unwrap_or(DialogOutcome::None)
    }

    /// What to show as one option's value: the pending text while it is
    /// being typed, its label otherwise.
    fn value_label(&self, option: &ConfigOptionRow) -> String {
        if self.editing_config_id.as_deref() == Some(option.id.as_str()) {
            format!("{}{CURSOR}", self.editing_value)
        } else {
            option.current_label()
        }
    }

    fn config_list_pane(&self) -> PanelPane {
        let rows = self
            .projection
            .config
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let selected = index == self.config_selected_index;
                PanelRow::one(TextSpan::new(
                    format!(
                        "{} {}: {}",
                        if selected { ">" } else { " " },
                        option.name,
                        self.value_label(option)
                    ),
                    if selected {
                        RowTone::Focus
                    } else {
                        RowTone::Normal
                    },
                ))
            })
            .collect();
        PanelPane {
            title: Some(CONFIG_LIST_TITLE.into()),
            rows,
            empty_text: Some(NO_OPTIONS.into()),
            ..PanelPane::default()
        }
    }

    fn config_detail_pane(&self) -> PanelPane {
        let mut pane = PanelPane {
            title: Some(CONFIG_DETAIL_TITLE.into()),
            empty_text: Some(NO_SELECTION.into()),
            ..PanelPane::default()
        };
        let Some(option) = self.projection.config.get(self.config_selected_index) else {
            return pane;
        };
        pane.rows
            .push(PanelRow::one(TextSpan::strong(option.name.clone())));
        if let Some(description) = option.description.as_ref() {
            pane.rows
                .push(PanelRow::one(TextSpan::dim(description.clone())));
        }
        pane.rows.push(PanelRow::blank());
        pane.rows.push(PanelRow::spans(vec![
            TextSpan::dim("Current: "),
            TextSpan::normal(self.value_label(option)),
        ]));
        pane.rows.push(PanelRow::blank());
        if option.free_text {
            pane.rows.push(PanelRow::one(TextSpan::dim(TEXT_EDIT_HINT)));
            return pane;
        }
        pane.rows
            .push(PanelRow::one(TextSpan::dim(AVAILABLE_VALUES)));
        for choice in &option.choices {
            let current = choice.value == option.current_value;
            let tone = if current {
                RowTone::Focus
            } else {
                RowTone::Normal
            };
            pane.rows.push(PanelRow::spans(vec![
                TextSpan::new(
                    if current { "* " } else { "  " },
                    if current {
                        RowTone::Focus
                    } else {
                        RowTone::Dim
                    },
                ),
                TextSpan::new(choice.label.clone(), tone),
            ]));
        }
        pane
    }

    /// The key hints, which say what the keys do here rather than what
    /// they do in general: editing text has its own two.
    fn footer(&self) -> Vec<TextSpan> {
        let key = |text: &str| TextSpan::new(text, RowTone::Focus);
        if self.is_editing_text() {
            return vec![
                key(" Enter "),
                TextSpan::dim("apply"),
                key(" · Esc "),
                TextSpan::dim("cancel"),
            ];
        }
        if self.on_config_tab() {
            return vec![
                key(" ←/→ "),
                TextSpan::dim("switch tabs"),
                key(" · ↑/↓ "),
                TextSpan::dim("move"),
                key(" · Enter "),
                TextSpan::dim("edit/cycle"),
                key(" · Esc "),
                TextSpan::dim("close"),
            ];
        }
        vec![
            key(" ←/→ "),
            TextSpan::dim("switch tabs"),
            key(" · Esc "),
            TextSpan::dim("close"),
        ]
    }
}

/// An [`ACTION_APPLY_CONFIG`] action for one option. It stays open: the
/// panel remains up so the user can change several settings in a visit.
fn apply_config(config_id: String, value: String) -> DialogOutcome {
    DialogOutcome::Action(DialogAction::staying_many(
        DIALOG_ID,
        ACTION_APPLY_CONFIG,
        vec![config_id, value],
    ))
}

impl DialogModel for SettingsDialogState {
    crate::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        // The option list comes from the last projection, which the
        // surface refreshes every frame — so it is never older than the
        // frame the user was looking at when they pressed the key.
        // Lifting it out and back avoids cloning it per key.
        let config = std::mem::take(&mut self.projection.config);
        let outcome = self.apply_key(press.key, &config);
        self.projection.config = config;
        outcome
    }

    fn view(&self) -> ViewSpec {
        let tabs = self
            .projection
            .tabs
            .iter()
            .enumerate()
            .map(|(index, label)| PanelTab {
                label: label.clone(),
                active: index == self.active_tab,
            })
            .collect();
        let (body, side, split) = if self.on_config_tab() {
            (
                self.config_list_pane(),
                Some(self.config_detail_pane()),
                CONFIG_SPLIT,
            )
        } else {
            (
                PanelPane::rows(self.projection.body.clone()),
                None,
                PanelSplit::default(),
            )
        };
        ViewSpec::Panel(PanelView {
            title: TITLE.into(),
            tabs,
            body,
            side,
            split,
            footer: self.footer(),
            desired_height: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(value: &str, label: &str) -> ConfigChoice {
        ConfigChoice {
            value: value.into(),
            label: label.into(),
        }
    }

    fn sample_config() -> Vec<ConfigOptionRow> {
        vec![
            ConfigOptionRow {
                id: "uiMode".into(),
                name: "UI mode".into(),
                description: Some("Choose screen or inline rendering".into()),
                free_text: false,
                current_value: "screen".into(),
                choices: vec![choice("screen", "Screen"), choice("inline", "Inline")],
            },
            ConfigOptionRow {
                id: "permissions".into(),
                name: "Permissions".into(),
                description: None,
                free_text: false,
                current_value: "default".into(),
                choices: vec![choice("default", "Default"), choice("plan", "Plan")],
            },
            ConfigOptionRow {
                id: "title".into(),
                name: "Session title".into(),
                description: None,
                free_text: true,
                current_value: "old".into(),
                choices: Vec::new(),
            },
        ]
    }

    /// Three tabs with the config list second, which is the shape the
    /// terminal registers.
    fn projection() -> SettingsProjection {
        SettingsProjection {
            tabs: vec!["status".into(), "config".into(), "usage".into()],
            body: vec![PanelRow::one(TextSpan::normal("Session: abc"))],
            config_tab: Some(1),
            config: sample_config(),
        }
    }

    fn state(active_tab: usize) -> SettingsDialogState {
        SettingsDialogState::open(&SettingsDialogOpen {
            active_tab,
            projection: projection(),
        })
    }

    fn panel(dialog: &SettingsDialogState) -> PanelView {
        let ViewSpec::Panel(view) = dialog.view() else {
            panic!("expected a panel view");
        };
        view
    }

    fn applied(id: &str, value: &str) -> DialogOutcome {
        DialogOutcome::Action(DialogAction::staying_many(
            DIALOG_ID,
            ACTION_APPLY_CONFIG,
            vec![id.into(), value.into()],
        ))
    }

    #[test]
    fn left_and_right_cycle_tabs_and_wrap() {
        let mut dialog = state(1);
        assert_eq!(dialog.active_tab(), 1);
        dialog.on_key(DialogKey::Right.into());
        assert_eq!(dialog.active_tab(), 2);
        dialog.on_key(DialogKey::Right.into());
        assert_eq!(dialog.active_tab(), 0, "the strip wraps");
        dialog.on_key(DialogKey::Left.into());
        assert_eq!(dialog.active_tab(), 2);
        dialog.on_key(DialogKey::Tab.into());
        assert_eq!(dialog.active_tab(), 0);
        dialog.on_key(DialogKey::BackTab.into());
        assert_eq!(dialog.active_tab(), 2);
    }

    #[test]
    fn config_selection_clamps_to_the_latest_list() {
        let mut dialog = state(1);
        dialog.config_selected_index = 99;
        dialog.set_projection(projection());
        assert_eq!(dialog.config_selected_index, 2);

        let mut shorter = projection();
        shorter.config.truncate(1);
        dialog.set_projection(shorter);
        assert_eq!(dialog.config_selected_index, 0);
    }

    #[test]
    fn enter_cycles_the_selected_option_and_wraps_past_the_last_value() {
        let mut dialog = state(1);
        assert_eq!(
            dialog.on_key(DialogKey::Enter.into()),
            applied("uiMode", "inline")
        );

        // An option already on its last value cycles back to the first.
        let mut projection = projection();
        projection.config[0].current_value = "inline".into();
        dialog.set_projection(projection);
        assert_eq!(
            dialog.on_key(DialogKey::Enter.into()),
            applied("uiMode", "screen")
        );
    }

    #[test]
    fn space_cycles_like_enter_including_with_a_modifier_held() {
        let mut dialog = state(1);
        assert_eq!(
            dialog.on_key(DialogKey::plain(' ').into()),
            applied("uiMode", "inline")
        );
        assert_eq!(
            dialog.on_key(
                DialogKey::Char {
                    value: ' ',
                    plain: false
                }
                .into()
            ),
            applied("uiMode", "inline")
        );
    }

    #[test]
    fn arrows_move_the_config_selection_only_on_the_config_tab() {
        let mut dialog = state(1);
        dialog.on_key(DialogKey::Down.into());
        assert_eq!(dialog.config_selected_index, 1);

        let mut elsewhere = state(0);
        elsewhere.on_key(DialogKey::Down.into());
        assert_eq!(elsewhere.config_selected_index, 0);
        // Nor does Enter apply anything from an informational tab.
        assert_eq!(
            elsewhere.on_key(DialogKey::Enter.into()),
            DialogOutcome::None
        );
    }

    #[test]
    fn a_free_text_option_is_typed_then_applied() {
        let mut dialog = state(1);
        dialog.config_selected_index = 2;

        assert_eq!(dialog.on_key(DialogKey::Enter.into()), DialogOutcome::None);
        assert!(dialog.is_editing_text());
        // The edit starts from the current value.
        assert_eq!(dialog.editing_value, "old");
        dialog.on_key(DialogKey::Backspace.into());
        dialog.on_key(DialogKey::plain('x').into());
        // The list and the detail both show the pending text with a cursor.
        let view = panel(&dialog);
        assert_eq!(view.body.rows[2].text(), "> Session title: olx█");

        assert_eq!(
            dialog.on_key(DialogKey::Enter.into()),
            applied("title", "olx")
        );
        assert!(!dialog.is_editing_text());
    }

    #[test]
    fn escape_abandons_a_text_edit_before_it_closes_the_panel() {
        let mut dialog = state(1);
        dialog.config_selected_index = 2;
        dialog.on_key(DialogKey::Enter.into());

        assert_eq!(dialog.on_key(DialogKey::Escape.into()), DialogOutcome::None);
        assert!(!dialog.is_editing_text());
        assert_eq!(
            dialog.on_key(DialogKey::Escape.into()),
            DialogOutcome::Close
        );
    }

    #[test]
    fn an_option_that_disappears_cancels_the_edit_it_was_carrying() {
        let mut dialog = state(1);
        dialog.config_selected_index = 2;
        dialog.on_key(DialogKey::Enter.into());
        assert!(dialog.is_editing_text());

        let mut without = projection();
        without.config.retain(|option| option.id != "title");
        dialog.set_projection(without);

        assert!(!dialog.is_editing_text());
        assert!(dialog.editing_value.is_empty());
    }

    #[test]
    fn the_config_tab_is_a_list_beside_a_detail_pane() {
        let view = panel(&state(1));

        assert_eq!(view.title, TITLE);
        assert_eq!(view.split, CONFIG_SPLIT);
        assert_eq!(view.body.title.as_deref(), Some(CONFIG_LIST_TITLE));
        assert_eq!(view.body.rows[0].text(), "> UI mode: Screen");
        assert_eq!(view.body.rows[0].spans[0].tone, RowTone::Focus);
        assert_eq!(view.body.rows[1].spans[0].tone, RowTone::Normal);

        let side = view.side.expect("a detail pane");
        assert_eq!(side.title.as_deref(), Some(CONFIG_DETAIL_TITLE));
        assert_eq!(side.rows[0].text(), "UI mode");
        assert_eq!(side.rows[1].text(), "Choose screen or inline rendering");
        assert_eq!(side.rows[3].text(), "Current: Screen");
        assert_eq!(side.rows[5].text(), AVAILABLE_VALUES);
        assert_eq!(side.rows[6].text(), "* Screen");
        assert_eq!(side.rows[6].spans[0].tone, RowTone::Focus);
        assert_eq!(side.rows[7].text(), "  Inline");
        assert_eq!(side.rows[7].spans[0].tone, RowTone::Dim);
    }

    #[test]
    fn a_free_text_option_says_how_to_edit_it_instead_of_listing_values() {
        let mut dialog = state(1);
        dialog.config_selected_index = 2;
        let side = panel(&dialog).side.expect("a detail pane");

        // No description on this one, so the value comes a row earlier.
        assert_eq!(side.rows[0].text(), "Session title");
        assert_eq!(side.rows[2].text(), "Current: old");
        assert_eq!(side.rows[4].text(), TEXT_EDIT_HINT);
    }

    #[test]
    fn an_informational_tab_shows_the_rows_it_was_handed_and_no_side_pane() {
        let view = panel(&state(0));

        assert!(view.side.is_none());
        assert!(view.body.title.is_none());
        assert_eq!(view.body.rows[0].text(), "Session: abc");
        assert_eq!(
            view.tabs
                .iter()
                .map(|tab| (tab.label.clone(), tab.active))
                .collect::<Vec<_>>(),
            vec![
                ("status".into(), true),
                ("config".into(), false),
                ("usage".into(), false)
            ]
        );
    }

    #[test]
    fn an_empty_option_list_says_so_in_both_panes() {
        let mut projection = projection();
        projection.config.clear();
        let dialog = SettingsDialogState::open(&SettingsDialogOpen {
            active_tab: 1,
            projection,
        });
        let view = panel(&dialog);

        assert!(view.body.rows.is_empty());
        assert_eq!(view.body.empty_text.as_deref(), Some(NO_OPTIONS));
        let side = view.side.expect("a detail pane");
        assert!(side.rows.is_empty());
        assert_eq!(side.empty_text.as_deref(), Some(NO_SELECTION));
        // With nothing to cycle, Enter applies nothing.
        let mut dialog = dialog;
        assert_eq!(dialog.on_key(DialogKey::Enter.into()), DialogOutcome::None);
    }

    #[test]
    fn the_footer_says_what_the_keys_do_on_this_tab() {
        let joined = |dialog: &SettingsDialogState| {
            dialog
                .footer()
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>()
        };
        assert_eq!(joined(&state(0)), " ←/→ switch tabs · Esc close");
        assert_eq!(
            joined(&state(1)),
            " ←/→ switch tabs · ↑/↓ move · Enter edit/cycle · Esc close"
        );
        let mut editing = state(1);
        editing.config_selected_index = 2;
        editing.on_key(DialogKey::Enter.into());
        assert_eq!(joined(&editing), " Enter apply · Esc cancel");
        // Key names are emphasised; what they do is not.
        assert_eq!(editing.footer()[0].tone, RowTone::Focus);
        assert_eq!(editing.footer()[1].tone, RowTone::Dim);
    }

    #[test]
    fn esc_closes_from_an_informational_tab() {
        let mut dialog = state(0);
        assert_eq!(
            dialog.on_key(DialogKey::Escape.into()),
            DialogOutcome::Close
        );
    }

    #[test]
    fn a_tab_that_went_away_does_not_leave_the_panel_on_a_missing_one() {
        let mut dialog = state(2);
        let mut shorter = projection();
        shorter.tabs.truncate(2);
        dialog.set_projection(shorter);

        assert_eq!(dialog.active_tab(), 1);
    }
}
