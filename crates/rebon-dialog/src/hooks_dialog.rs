//! Read-only browser for hook event metadata and settings-backed hook
//! configuration.
//!
//! Which events exist and what is configured for each one is a question
//! only the front end can answer — it needs the tool list, the agent
//! registry and the settings files on disk — so the rows arrive already
//! built. The panel is a two-pane browser over them and nothing else: it
//! emits no actions at all, because there is nothing here to change.

use crate::model::{
    DialogKey, DialogModel, DialogOutcome, KeyPress, PanelPane, PanelRow, PanelSplit, PanelView,
    TextSpan, ViewSpec,
};

/// Stable id, used for action routing by the surfaces that track which
/// panel is open. The browser is read-only, so it emits no actions.
pub const DIALOG_ID: &str = "hooks";

const TITLE: &str = " Hooks · read only ";
const FOOTER: &str = "Up/Down select · Esc close";
/// Rows a page key moves by. The list is short and fixed-length, so a
/// page is a fixed jump rather than a screenful.
const PAGE: usize = 8;
/// Columns of the frame the event list takes, as a percentage.
const LIST_PERCENT: u16 = 38;

/// One hook event, as the browser needs it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HookEventRow {
    /// The event's name.
    pub name: String,
    /// One line saying what it is.
    pub summary: String,
    /// The longer explanation, shown under the summary.
    pub description: String,
    /// One line per configured hook, already formatted by the collector.
    pub configured: Vec<String>,
}

/// What opening the browser needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HooksDialogInput {
    /// Every hook event, in the order they are listed.
    pub events: Vec<HookEventRow>,
    /// Problems found while reading the settings files.
    pub warnings: Vec<String>,
    /// The event to start on. An unknown name starts at the first.
    pub initial_event: Option<String>,
}

/// Reducer state for the hook browser.
#[derive(Debug, Clone)]
pub struct HooksDialogState {
    events: Vec<HookEventRow>,
    selected: usize,
    warnings: Vec<String>,
    /// Rows the surface last offered, so the list scrolls to keep the
    /// selection on screen. `None` before the first paint.
    viewport_rows: Option<u16>,
}

impl HooksDialogState {
    /// Open the browser on a collected set of events.
    pub fn open(input: &HooksDialogInput) -> Self {
        let selected = input
            .initial_event
            .as_deref()
            .and_then(|name| {
                input
                    .events
                    .iter()
                    .position(|event| event.name.eq_ignore_ascii_case(name))
            })
            .unwrap_or(0);
        Self {
            events: input.events.clone(),
            selected,
            warnings: input.warnings.clone(),
            viewport_rows: None,
        }
    }

    /// The event list: one row per event with whether it is configured,
    /// scrolled so the selection stays on screen.
    fn list_pane(&self, pane_rows: usize) -> PanelPane {
        let rows = self
            .events
            .iter()
            .enumerate()
            .map(|(index, event)| {
                let status = if event.configured.is_empty() {
                    "not configured"
                } else {
                    "configured"
                };
                let marker = if index == self.selected { ">" } else { " " };
                let row = PanelRow::one(TextSpan::normal(format!(
                    "{marker} {} · {status}",
                    event.name
                )));
                if index == self.selected {
                    row.highlighted()
                } else {
                    row
                }
            })
            .collect();
        PanelPane {
            rows,
            scroll: self
                .selected
                .saturating_sub(pane_rows.saturating_sub(1))
                .min(u16::MAX as usize) as u16,
            ..PanelPane::default()
        }
    }

    /// The selected event's detail: its name, summary and description,
    /// then what is configured for it, then any settings warnings.
    fn detail_pane(&self) -> PanelPane {
        let Some(event) = self.events.get(self.selected) else {
            return PanelPane::default();
        };
        let mut rows = vec![
            PanelRow::one(TextSpan::strong(event.name.clone())),
            PanelRow::one(TextSpan::normal(event.summary.clone())),
            PanelRow::one(TextSpan::dim(event.description.clone())),
            PanelRow::blank(),
            PanelRow::one(TextSpan::strong(if event.configured.is_empty() {
                "Not configured"
            } else {
                "Configured"
            })),
        ];
        if event.configured.is_empty() {
            rows.push(PanelRow::one(TextSpan::dim(
                "No settings.json hook entries for this event.",
            )));
        } else {
            rows.extend(
                event
                    .configured
                    .iter()
                    .map(|hook| PanelRow::one(TextSpan::normal(format!("- {hook}")))),
            );
        }
        if !self.warnings.is_empty() {
            rows.push(PanelRow::blank());
            rows.push(PanelRow::one(TextSpan::strong("Settings warnings")));
            rows.extend(
                self.warnings
                    .iter()
                    .map(|warning| PanelRow::one(TextSpan::dim(format!("- {warning}")))),
            );
        }
        PanelPane {
            rows,
            wrap: true,
            ..PanelPane::default()
        }
    }
}

impl DialogModel for HooksDialogState {
    crate::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn note_viewport(&mut self, rows: u16, _cols: u16) {
        self.viewport_rows = Some(rows);
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        let last = self.events.len().saturating_sub(1);
        match press.key {
            DialogKey::Escape => DialogOutcome::Close,
            DialogKey::Up => {
                self.selected = self.selected.saturating_sub(1);
                DialogOutcome::None
            }
            DialogKey::Down => {
                self.selected = self.selected.saturating_add(1).min(last);
                DialogOutcome::None
            }
            DialogKey::PageUp => {
                self.selected = self.selected.saturating_sub(PAGE);
                DialogOutcome::None
            }
            DialogKey::PageDown => {
                self.selected = self.selected.saturating_add(PAGE).min(last);
                DialogOutcome::None
            }
            DialogKey::Home => {
                self.selected = 0;
                DialogOutcome::None
            }
            DialogKey::End => {
                self.selected = last;
                DialogOutcome::None
            }
            _ => DialogOutcome::None,
        }
    }

    fn view(&self) -> ViewSpec {
        // One row of the frame goes to the footer; the panes get the rest.
        let pane_rows = self
            .viewport_rows
            .map(|rows| usize::from(rows.saturating_sub(1)))
            .unwrap_or(1)
            .max(1);
        ViewSpec::Panel(PanelView {
            title: TITLE.into(),
            body: self.list_pane(pane_rows),
            side: Some(self.detail_pane()),
            split: PanelSplit {
                first_percent: LIST_PERCENT,
                ..PanelSplit::default()
            },
            footer: vec![TextSpan::dim(FOOTER)],
            ..PanelView::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> HooksDialogInput {
        HooksDialogInput {
            events: vec![
                HookEventRow {
                    name: "A".into(),
                    summary: "a".into(),
                    description: String::new(),
                    configured: Vec::new(),
                },
                HookEventRow {
                    name: "B".into(),
                    summary: "b".into(),
                    description: String::new(),
                    configured: vec!["command".into()],
                },
            ],
            warnings: Vec::new(),
            initial_event: None,
        }
    }

    fn state() -> HooksDialogState {
        HooksDialogState::open(&input())
    }

    fn panel(dialog: &HooksDialogState) -> PanelView {
        let ViewSpec::Panel(view) = dialog.view() else {
            panic!("expected a panel view");
        };
        view
    }

    #[test]
    fn navigation_clamps() {
        let mut dialog = state();
        dialog.on_key(DialogKey::End.into());
        assert_eq!(dialog.selected, 1);
        dialog.on_key(DialogKey::Down.into());
        assert_eq!(dialog.selected, 1);
    }

    #[test]
    fn home_returns_to_first_event() {
        let mut dialog = state();
        dialog.selected = 1;
        dialog.on_key(DialogKey::Home.into());
        assert_eq!(dialog.selected, 0);
    }

    #[test]
    fn escape_closes_without_mutating_configuration() {
        let mut dialog = state();
        assert_eq!(
            dialog.on_key(DialogKey::Escape.into()),
            DialogOutcome::Close
        );
        assert_eq!(dialog.events[1].configured, vec!["command"]);
    }

    #[test]
    fn an_initial_event_is_matched_without_regard_to_case_and_falls_back_to_the_first() {
        let mut input = input();
        input.initial_event = Some("b".into());
        assert_eq!(HooksDialogState::open(&input).selected, 1);

        input.initial_event = Some("no-such-event".into());
        assert_eq!(HooksDialogState::open(&input).selected, 0);
    }

    #[test]
    fn the_view_is_a_list_beside_a_detail_pane() {
        let view = panel(&state());

        assert_eq!(view.title, TITLE);
        assert_eq!(view.split.first_percent, LIST_PERCENT);
        assert_eq!(view.footer, vec![TextSpan::dim(FOOTER)]);
        // The selected event is the reversed row; the other is not.
        assert_eq!(view.body.rows[0].text(), "> A · not configured");
        assert!(view.body.rows[0].highlighted);
        assert_eq!(view.body.rows[1].text(), "  B · configured");
        assert!(!view.body.rows[1].highlighted);
        // The detail pane wraps rather than clipping a long description.
        let side = view.side.expect("a detail pane");
        assert!(side.wrap);
        assert_eq!(side.rows[0].text(), "A");
        assert_eq!(side.rows[4].text(), "Not configured");
        assert_eq!(
            side.rows[5].text(),
            "No settings.json hook entries for this event."
        );
    }

    #[test]
    fn a_configured_event_lists_its_hooks_instead_of_the_placeholder() {
        let mut dialog = state();
        dialog.on_key(DialogKey::Down.into());
        let side = panel(&dialog).side.expect("a detail pane");

        assert_eq!(side.rows[4].text(), "Configured");
        assert_eq!(side.rows[5].text(), "- command");
    }

    #[test]
    fn settings_warnings_are_appended_under_their_own_heading() {
        let mut input = input();
        input.warnings = vec!["settings.json: bad matcher".into()];
        let side = panel(&HooksDialogState::open(&input))
            .side
            .expect("a detail pane");

        let texts: Vec<String> = side.rows.iter().map(PanelRow::text).collect();
        assert!(
            texts.contains(&"Settings warnings".to_string()),
            "{texts:?}"
        );
        assert!(
            texts.contains(&"- settings.json: bad matcher".to_string()),
            "{texts:?}"
        );
    }

    #[test]
    fn the_list_scrolls_to_keep_a_selection_below_the_fold_on_screen() {
        let mut input = input();
        input.events = (0..20)
            .map(|index| HookEventRow {
                name: format!("E{index}"),
                ..HookEventRow::default()
            })
            .collect();
        let mut dialog = HooksDialogState::open(&input);
        // Six rows of panes once the footer takes one.
        dialog.note_viewport(7, 100);

        assert_eq!(panel(&dialog).body.scroll, 0);
        for _ in 0..10 {
            dialog.on_key(DialogKey::Down.into());
        }
        assert_eq!(panel(&dialog).body.scroll, 5);
    }

    #[test]
    fn an_empty_browser_has_no_detail_rows_and_does_not_panic() {
        let dialog = HooksDialogState::open(&HooksDialogInput::default());
        let view = panel(&dialog);

        assert!(view.body.rows.is_empty());
        assert!(view.side.expect("a detail pane").rows.is_empty());
    }
}
