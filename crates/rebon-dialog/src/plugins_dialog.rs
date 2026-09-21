//! Interactive `/plugin` management panel.
//!
//! The panel deliberately owns no plugin store or installer. Its rows are
//! a projection of `/plugin list`, and every mutation goes back out as a
//! textual `/plugin` command, so the existing command path stays the one
//! source of plugin behaviour — and this reducer stays a reducer.

use crate::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, PanelPane, PanelRow, PanelView,
    TextSpan, ViewSpec,
};

/// Stable id used for action routing.
pub const DIALOG_ID: &str = "plugins";
/// Run a textual `/plugin` command. The panel owns no plugin store, so
/// every mutation goes back out through the existing command path.
pub const ACTION_EXECUTE: &str = "execute";

const TITLE: &str = " Manage plugins ";
const FOOTER: &str =
    "↑/↓ or j/k select · Enter/Space enable or disable · u uninstall (twice) · Esc close";
const EMPTY: &str = "No plugins installed.";
const MAX_VISIBLE: usize = 12;

/// Rows of chrome the list shares its pane with: the summary line above
/// it and the blank separator below.
const PANE_CHROME: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginDialogRow {
    name: String,
    version: String,
    scope: String,
    enabled: bool,
    source: String,
}

/// Reducer state for the plugin manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginsDialogState {
    rows: Vec<PluginDialogRow>,
    selected: usize,
    feedback: Option<(String, bool)>,
    /// Name armed by a first uninstall press. Uninstalling is irreversible,
    /// so the second press on the same plugin is what actually runs the
    /// command.
    pending_uninstall: Option<String>,
    /// Rows the surface last offered the panel, so a terminal too short
    /// for the height it asked for still scrolls rather than clipping the
    /// selection off the bottom. `None` before the first paint.
    viewport_rows: Option<u16>,
}

impl PluginsDialogState {
    /// Build the panel from the result of the existing `/plugin list` path.
    pub fn open(list_text: &str, is_err: bool) -> Self {
        let rows = if is_err {
            Vec::new()
        } else {
            parse_plugin_list(list_text)
        };
        Self {
            rows,
            selected: 0,
            feedback: is_err.then(|| (list_text.to_string(), true)),
            pending_uninstall: None,
            viewport_rows: None,
        }
    }

    /// Replace rows after a mutation by running `/plugin list` again.
    pub fn refresh(&mut self, list_text: &str, is_err: bool) {
        if is_err {
            self.feedback = Some((list_text.to_string(), true));
            return;
        }
        self.rows = parse_plugin_list(list_text);
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.pending_uninstall = None;
    }

    /// Show one line of result text under the list.
    pub fn set_feedback(&mut self, text: String, is_err: bool) {
        self.feedback = Some((text, is_err));
    }

    /// Height the panel wants: border, summary, the rows it can show,
    /// the blank separator and footer, plus a row each for feedback and
    /// for an armed uninstall.
    pub fn desired_height(&self) -> u16 {
        let rows = self.rows.len().clamp(1, MAX_VISIBLE) as u16;
        rows.saturating_add(6 + self.extra_rows() as u16)
    }

    /// Feedback and the uninstall confirmation each take one row below
    /// the list.
    fn extra_rows(&self) -> usize {
        usize::from(self.feedback.is_some()) + usize::from(self.pending_uninstall.is_some())
    }

    fn move_selection(&mut self, forward: bool) {
        let last = self.rows.len().saturating_sub(1);
        self.selected = if forward {
            if self.selected >= last {
                0
            } else {
                self.selected + 1
            }
        } else if self.selected == 0 {
            last
        } else {
            self.selected - 1
        };
    }

    fn selected_command(&self, uninstall: bool) -> Option<String> {
        let row = self.rows.get(self.selected)?;
        let action = if uninstall {
            "uninstall"
        } else if row.enabled {
            "disable"
        } else {
            "enable"
        };
        Some(format!(
            "/plugin {action} {} --scope {}",
            quote_plugin_name(&row.name),
            row.scope
        ))
    }

    fn toggle_selected(&mut self) -> DialogOutcome {
        self.execute(false)
    }

    fn arm_or_confirm_uninstall(&mut self, armed: Option<String>) -> DialogOutcome {
        let Some(name) = self.rows.get(self.selected).map(|row| row.name.clone()) else {
            return DialogOutcome::None;
        };
        if armed.as_deref() == Some(name.as_str()) {
            return self.execute(true);
        }
        self.pending_uninstall = Some(name);
        DialogOutcome::None
    }

    /// The `/plugin` command for the highlighted row, as a staying
    /// action: the manager remains open so the user can make several
    /// changes in one visit.
    fn execute(&self, uninstall: bool) -> DialogOutcome {
        self.selected_command(uninstall)
            .map(|command| {
                DialogOutcome::Action(DialogAction::staying(DIALOG_ID, ACTION_EXECUTE, command))
            })
            .unwrap_or(DialogOutcome::None)
    }

    fn visible_window(&self, available_rows: usize) -> (usize, usize) {
        let visible = available_rows.clamp(1, MAX_VISIBLE).min(self.rows.len());
        let start = if visible > 0 && self.selected >= visible {
            self.selected + 1 - visible
        } else {
            0
        };
        (start, (start + visible).min(self.rows.len()))
    }

    /// Rows for the list, given the pane height the surface offered.
    ///
    /// Split out from [`DialogModel::view`] so the window can be driven
    /// against a chosen height instead of the last painted one.
    fn body_rows(&self, pane_rows: usize) -> Vec<PanelRow> {
        let enabled_count = self.rows.iter().filter(|row| row.enabled).count();
        let mut rows = vec![PanelRow::one(TextSpan::dim(format!(
            "{} installed · {} enabled · changes take effect next startup",
            self.rows.len(),
            enabled_count
        )))];

        if self.rows.is_empty() {
            rows.push(PanelRow::one(TextSpan::dim(EMPTY)));
        } else {
            let available = pane_rows
                .saturating_sub(PANE_CHROME + self.extra_rows())
                .max(1);
            let (start, end) = self.visible_window(available);
            for (offset, row) in self.rows[start..end].iter().enumerate() {
                rows.push(self.list_row(row, start + offset == self.selected));
            }
        }

        if let Some((message, is_err)) = &self.feedback {
            let one_line = message.lines().next().unwrap_or_default().to_string();
            rows.push(PanelRow::one(if *is_err {
                TextSpan::new(one_line, crate::model::RowTone::Error)
            } else {
                TextSpan::dim(one_line)
            }));
        }
        if let Some(name) = &self.pending_uninstall {
            rows.push(PanelRow::one(TextSpan::new(
                format!("Press u again to uninstall {name} · any other key cancels"),
                crate::model::RowTone::Warning,
            )));
        }
        rows.push(PanelRow::blank());
        rows
    }

    /// One plugin's row: a marker and its name in the brand colour when
    /// it is selected, the version and scope dim, and the state word in
    /// the colour of what it says.
    fn list_row(&self, row: &PluginDialogRow, selected: bool) -> PanelRow {
        use crate::model::RowTone;
        let name_tone = if selected {
            RowTone::Brand
        } else {
            RowTone::Normal
        };
        PanelRow::spans(vec![
            TextSpan::new(
                if selected { "❯ " } else { "  " },
                if selected {
                    RowTone::Brand
                } else {
                    RowTone::Dim
                },
            ),
            TextSpan::new(row.name.clone(), name_tone),
            TextSpan::dim(format!(" {} · {} · ", row.version, row.scope)),
            TextSpan::new(
                if row.enabled { "enabled" } else { "disabled" },
                if row.enabled {
                    RowTone::Success
                } else {
                    RowTone::Warning
                },
            ),
            TextSpan::dim(format!(" · {}", row.source)),
        ])
    }
}

/// Read `/plugin list`'s columns back. A line that is not five columns
/// with a known scope and state is not a plugin row — it is a heading or
/// a note — and is skipped rather than shown as a broken entry.
fn parse_plugin_list(text: &str) -> Vec<PluginDialogRow> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?;
            let version = fields.next()?;
            let scope = fields.next()?;
            if !matches!(scope, "user" | "project") {
                return None;
            }
            let state = fields.next()?;
            if !matches!(state, "enabled" | "disabled") {
                return None;
            }
            let source = fields.collect::<Vec<_>>().join(" ");
            Some(PluginDialogRow {
                name: name.to_string(),
                version: version.to_string(),
                scope: scope.to_string(),
                enabled: state == "enabled",
                source,
            })
        })
        .collect()
}

fn quote_plugin_name(name: &str) -> String {
    if name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '@' | '/'))
    {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('\\', "\\\\").replace('\"', "\\\""))
    }
}

impl DialogModel for PluginsDialogState {
    crate::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn note_viewport(&mut self, rows: u16, _cols: u16) {
        self.viewport_rows = Some(rows);
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        // Anything but another uninstall press abandons a pending confirmation.
        let armed = self.pending_uninstall.take();
        if matches!(
            press.key,
            DialogKey::Delete
                | DialogKey::Char {
                    value: 'u',
                    plain: true
                }
        ) {
            // A held key repeats on its own; only a fresh press may confirm.
            return self.arm_or_confirm_uninstall(armed.filter(|_| !press.repeat));
        }
        let move_up = matches!(
            press.key,
            DialogKey::Up
                | DialogKey::Char {
                    value: 'k',
                    plain: true
                }
        );
        let move_down = matches!(
            press.key,
            DialogKey::Down
                | DialogKey::Char {
                    value: 'j',
                    plain: true
                }
        );
        if !self.rows.is_empty() && (move_up || move_down) {
            self.move_selection(move_down);
            return DialogOutcome::None;
        }
        match press.key {
            DialogKey::Escape => DialogOutcome::Close,
            DialogKey::Enter => self.toggle_selected(),
            DialogKey::Char {
                value: ' ',
                plain: true,
            } => self.toggle_selected(),
            _ => DialogOutcome::None,
        }
    }

    fn view(&self) -> ViewSpec {
        // The panel asks to be sized to its content, so the height it was
        // offered is the height it wanted unless the terminal was short.
        let pane_rows = self
            .viewport_rows
            .map(usize::from)
            .unwrap_or_else(|| usize::from(self.desired_height()));
        ViewSpec::Panel(PanelView {
            title: TITLE.into(),
            body: PanelPane::rows(self.body_rows(pane_rows.saturating_sub(1))),
            footer: vec![TextSpan::dim(FOOTER)],
            desired_height: Some(self.desired_height()),
            ..PanelView::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RowTone;

    const LIST: &str = "alpha 1.0.0 user enabled https://example.com/alpha
                        beta 0.2.0 project disabled /srv/beta";

    fn dialog() -> PluginsDialogState {
        PluginsDialogState::open(LIST, false)
    }

    fn chord(value: char) -> KeyPress {
        DialogKey::Char {
            value,
            plain: false,
        }
        .into()
    }

    fn execute(command: &str) -> DialogOutcome {
        DialogOutcome::Action(DialogAction::staying(DIALOG_ID, ACTION_EXECUTE, command))
    }

    fn panel(dialog: &PluginsDialogState) -> PanelView {
        let ViewSpec::Panel(view) = dialog.view() else {
            panic!("expected a panel view");
        };
        view
    }

    #[test]
    fn enter_toggles_the_selected_plugin_and_keeps_the_manager_open() {
        let outcome = dialog().on_key(DialogKey::Enter.into());
        assert_eq!(outcome, execute("/plugin disable alpha --scope user"));
        let DialogOutcome::Action(action) = outcome else {
            unreachable!()
        };
        assert!(!action.close);
    }

    #[test]
    fn space_toggles_like_enter() {
        assert_eq!(
            dialog().on_key(DialogKey::plain(' ').into()),
            execute("/plugin disable alpha --scope user")
        );
    }

    #[test]
    fn uninstall_needs_a_second_press_on_the_same_plugin() {
        let mut dialog = dialog();

        assert_eq!(
            dialog.on_key(DialogKey::plain('u').into()),
            DialogOutcome::None
        );
        assert_eq!(dialog.pending_uninstall.as_deref(), Some("alpha"));
        assert_eq!(
            dialog.on_key(DialogKey::plain('u').into()),
            execute("/plugin uninstall alpha --scope user")
        );
    }

    #[test]
    fn moving_the_selection_cancels_a_pending_uninstall() {
        let mut dialog = dialog();
        dialog.on_key(DialogKey::plain('u').into());

        dialog.on_key(DialogKey::Down.into());

        assert!(dialog.pending_uninstall.is_none());
        assert_eq!(dialog.on_key(DialogKey::Delete.into()), DialogOutcome::None);
        assert_eq!(dialog.pending_uninstall.as_deref(), Some("beta"));
    }

    #[test]
    fn a_held_key_repeat_does_not_confirm_the_uninstall() {
        let mut dialog = dialog();
        dialog.on_key(DialogKey::plain('u').into());
        let repeat = KeyPress {
            key: DialogKey::plain('u'),
            repeat: true,
            viewport_rows: None,
        };

        assert_eq!(dialog.on_key(repeat), DialogOutcome::None);
        assert_eq!(dialog.pending_uninstall.as_deref(), Some("alpha"));
    }

    #[test]
    fn escape_cancels_a_pending_uninstall() {
        let mut dialog = dialog();
        dialog.on_key(DialogKey::plain('u').into());

        assert_eq!(
            dialog.on_key(DialogKey::Escape.into()),
            DialogOutcome::Close
        );
        assert!(dialog.pending_uninstall.is_none());
    }

    #[test]
    fn control_chords_do_not_trigger_letter_shortcuts() {
        let mut dialog = dialog();

        assert_eq!(dialog.on_key(chord('u')), DialogOutcome::None);
        dialog.on_key(chord('j'));

        assert!(dialog.pending_uninstall.is_none());
        assert_eq!(dialog.selected, 0);
    }

    #[test]
    fn the_view_is_a_content_sized_panel_with_a_summary_and_a_hint_footer() {
        let view = panel(&dialog());

        assert_eq!(view.title, TITLE);
        assert!(view.side.is_none());
        assert!(view.tabs.is_empty());
        assert_eq!(view.desired_height, Some(8));
        assert_eq!(
            view.body.rows[0].text(),
            "2 installed · 1 enabled · changes take effect next startup"
        );
        assert_eq!(view.footer, vec![TextSpan::dim(FOOTER)]);
    }

    #[test]
    fn a_row_carries_its_state_in_the_colour_of_what_it_says() {
        let view = panel(&dialog());
        let selected = &view.body.rows[1];
        let other = &view.body.rows[2];

        assert_eq!(
            selected.text(),
            "❯ alpha 1.0.0 · user · enabled · https://example.com/alpha"
        );
        // The marker and name follow the selection; the state word never
        // does, because it is reporting a fact rather than a highlight.
        assert_eq!(selected.spans[0].tone, RowTone::Brand);
        assert_eq!(selected.spans[1].tone, RowTone::Brand);
        assert_eq!(selected.spans[3].tone, RowTone::Success);
        assert_eq!(other.spans[0].tone, RowTone::Dim);
        assert_eq!(other.spans[1].tone, RowTone::Normal);
        assert_eq!(other.spans[3].tone, RowTone::Warning);
    }

    #[test]
    fn an_error_from_the_command_path_is_shown_in_the_error_role() {
        let mut dialog = dialog();
        dialog.set_feedback("could not reach the registry\nsecond line".into(), true);
        let view = panel(&dialog);

        let feedback = view
            .body
            .rows
            .iter()
            .find(|row| row.text().starts_with("could not"))
            .expect("the feedback row");
        // Only the first line: the panel is one row tall, not a log.
        assert_eq!(feedback.text(), "could not reach the registry");
        assert_eq!(feedback.spans[0].tone, RowTone::Error);
        assert_eq!(view.desired_height, Some(9), "feedback costs a row");
    }

    #[test]
    fn an_armed_uninstall_warns_in_place_and_costs_a_row() {
        let mut dialog = dialog();
        dialog.on_key(DialogKey::plain('u').into());
        let view = panel(&dialog);

        let armed = view
            .body
            .rows
            .iter()
            .find(|row| row.text().starts_with("Press u again"))
            .expect("the confirmation row");
        assert_eq!(
            armed.text(),
            "Press u again to uninstall alpha · any other key cancels"
        );
        assert_eq!(armed.spans[0].tone, RowTone::Warning);
        assert_eq!(view.desired_height, Some(9));
    }

    #[test]
    fn an_empty_list_says_so_instead_of_showing_rows() {
        let view = panel(&PluginsDialogState::open("", false));

        assert_eq!(view.body.rows[1].text(), EMPTY);
        assert_eq!(view.desired_height, Some(7));
    }

    #[test]
    fn a_short_frame_scrolls_the_window_to_keep_the_selection_visible() {
        let mut dialog = PluginsDialogState::open(
            &(0..20)
                .map(|index| format!("p{index} 1.0.0 user enabled src\n"))
                .collect::<String>(),
            false,
        );
        // Six rows of pane: the summary, the blank, and four list rows.
        dialog.note_viewport(7, 80);
        for _ in 0..9 {
            dialog.on_key(DialogKey::Down.into());
        }
        let view = panel(&dialog);

        let listed: Vec<String> = view.body.rows[1..view.body.rows.len() - 1]
            .iter()
            .map(PanelRow::text)
            .collect();
        assert_eq!(listed.len(), 4, "{listed:?}");
        assert!(listed[3].contains("❯ p9 "), "{listed:?}");
        assert!(listed[0].contains("p6 "), "{listed:?}");
    }

    #[test]
    fn a_name_needing_quotes_gets_them_in_the_command() {
        let dialog = PluginsDialogState::open("my plugin", false);
        // A name with a space is not a five-column row, so it is not a
        // plugin at all — the quoting rule is exercised directly.
        assert!(dialog.rows.is_empty());
        assert_eq!(quote_plugin_name("ok-name.v2"), "ok-name.v2");
        assert_eq!(quote_plugin_name("has space"), "\"has space\"");
        assert_eq!(quote_plugin_name("has\"quote"), "\"has\\\"quote\"");
    }
}
