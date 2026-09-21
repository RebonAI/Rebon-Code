//! Structured `/doctor` diagnostics panel.
//!
//! The probes that produce a report run where the things being probed
//! are — a front end's session half, which can read the filesystem and
//! ask the tool layer questions. This panel takes the finished report as
//! data and never learns what a check is, which is also why re-running
//! one is an action handed back rather than a call made from here.

use crate::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, PanelPane, PanelRow, PanelView,
    TextSpan, ViewSpec,
};

/// Stable id used for action routing.
pub const DIALOG_ID: &str = "doctor";
/// Recompute the report. The host writes the result back through
/// `top_as_mut`, because the panel stays open.
pub const ACTION_RERUN: &str = "rerun";

const TITLE: &str = " Doctor Diagnostics ";
const FOOTER: &str = "Up/Down/PageUp/PageDown scroll · r rerun · Esc close";

/// How one diagnostic check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorStatus {
    /// Nothing to do.
    Pass,
    /// Works, but something is worth looking at.
    Warn,
    /// Broken.
    Fail,
}

impl DoctorStatus {
    /// The word shown in the report's status column.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

/// One check's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorItem {
    /// How it came out.
    pub status: DoctorStatus,
    /// What was checked.
    pub label: String,
    /// What was found.
    pub detail: String,
    /// What to do about it.
    pub suggestion: String,
}

/// A group of checks under one heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorSection {
    /// The heading.
    pub title: String,
    /// Its checks, in display order.
    pub items: Vec<DoctorItem>,
}

/// Everything one diagnostics run found.
///
/// Plain data on purpose: the panel renders it, the textual `/doctor`
/// command formats the same value, and neither of them collects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    /// Headline facts, as label/value pairs.
    pub summary: Vec<(String, String)>,
    /// The grouped checks.
    pub sections: Vec<DoctorSection>,
}

/// Reducer state for the diagnostics panel: the report, already laid out
/// as rows, and how far down it the reader has scrolled.
#[derive(Debug, Clone)]
pub struct DoctorDialogState {
    rows: Vec<PanelRow>,
    scroll: u16,
}

impl DoctorDialogState {
    /// Open the panel on one report.
    pub fn open(report: &DoctorReport) -> Self {
        Self {
            rows: report_rows(report),
            scroll: 0,
        }
    }

    /// Replace the report with a freshly collected one, back at the top:
    /// a rerun is a new reading, not a continuation of the old one.
    pub fn replace_report(&mut self, report: &DoctorReport) {
        self.rows = report_rows(report);
        self.scroll = 0;
    }

    /// The first body row currently shown.
    #[cfg(test)]
    fn scroll(&self) -> u16 {
        self.scroll
    }

    /// Every row's text, joined by newlines.
    #[cfg(test)]
    fn text(&self) -> String {
        self.rows
            .iter()
            .map(PanelRow::text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn max_scroll(&self, viewport: u16) -> u16 {
        self.rows.len().saturating_sub(viewport as usize) as u16
    }

    /// Apply one key against an explicit viewport height. Split out from
    /// [`DialogModel::on_key`] so scrolling can be driven against a
    /// chosen height; the real one is only known after a render.
    fn apply_key(&mut self, key: DialogKey, viewport: u16) -> DialogOutcome {
        let viewport = viewport.max(1);
        let page = viewport.saturating_sub(1).max(1);
        match key {
            DialogKey::Escape => DialogOutcome::Close,
            // A chord must not rerun; only a bare `r` does.
            DialogKey::Char {
                value: 'r',
                plain: true,
            } => DialogOutcome::Action(DialogAction::staying_many(
                DIALOG_ID,
                ACTION_RERUN,
                Vec::new(),
            )),
            DialogKey::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                DialogOutcome::None
            }
            DialogKey::Down => {
                self.scroll = self.max_scroll(viewport).min(self.scroll.saturating_add(1));
                DialogOutcome::None
            }
            DialogKey::PageUp => {
                self.scroll = self.scroll.saturating_sub(page);
                DialogOutcome::None
            }
            DialogKey::PageDown => {
                self.scroll = self
                    .max_scroll(viewport)
                    .min(self.scroll.saturating_add(page));
                DialogOutcome::None
            }
            DialogKey::Home => {
                self.scroll = 0;
                DialogOutcome::None
            }
            DialogKey::End => {
                self.scroll = self.max_scroll(viewport);
                DialogOutcome::None
            }
            _ => DialogOutcome::None,
        }
    }
}

/// The report as rows: the summary pairs, then each section's heading
/// with its checks and the fix advice indented under each one.
fn report_rows(report: &DoctorReport) -> Vec<PanelRow> {
    let mut rows = Vec::new();
    for (label, value) in &report.summary {
        rows.push(PanelRow::spans(vec![
            TextSpan::strong(format!("{label}: ")),
            TextSpan::normal(value.clone()),
        ]));
    }
    for section in &report.sections {
        rows.push(PanelRow::blank());
        rows.push(PanelRow::one(TextSpan::strong(section.title.clone())));
        for item in &section.items {
            rows.push(PanelRow::spans(vec![
                TextSpan::strong(format!("[{}] ", item.status.label())),
                TextSpan::normal(format!("{} — {}", item.label, item.detail)),
            ]));
            rows.push(PanelRow::one(TextSpan::dim(format!(
                "       Fix: {}",
                item.suggestion
            ))));
        }
    }
    rows
}

impl DialogModel for DoctorDialogState {
    crate::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        self.apply_key(press.key, press.viewport_rows_or(20))
    }

    fn view(&self) -> ViewSpec {
        ViewSpec::Panel(PanelView {
            title: TITLE.into(),
            body: PanelPane {
                rows: self.rows.clone(),
                scroll: self.scroll,
                ..PanelPane::default()
            },
            footer: vec![TextSpan::dim(FOOTER)],
            ..PanelView::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RowTone;

    fn report(detail: &str) -> DoctorReport {
        DoctorReport {
            summary: vec![("version".into(), "1.0".into())],
            sections: vec![DoctorSection {
                title: "Config".into(),
                items: vec![DoctorItem {
                    status: DoctorStatus::Warn,
                    label: "settings".into(),
                    detail: detail.into(),
                    suggestion: "Review the settings file.".into(),
                }],
            }],
        }
    }

    #[test]
    fn r_requests_a_fresh_diagnostic_run_and_keeps_the_panel_open() {
        let mut dialog = DoctorDialogState::open(&report("old"));
        let outcome = dialog.on_key(DialogKey::plain('r').into());
        assert_eq!(
            outcome,
            DialogOutcome::Action(DialogAction::staying_many(
                DIALOG_ID,
                ACTION_RERUN,
                Vec::new()
            ))
        );
    }

    #[test]
    fn control_chords_do_not_request_a_rerun() {
        let mut dialog = DoctorDialogState::open(&report("old"));
        let outcome = dialog.on_key(
            DialogKey::Char {
                value: 'r',
                plain: false,
            }
            .into(),
        );
        assert_eq!(outcome, DialogOutcome::None);
    }

    #[test]
    fn paging_moves_further_than_a_single_arrow() {
        let mut dialog = DoctorDialogState::open(&report("ok"));
        dialog.apply_key(DialogKey::End, 4);
        let bottom = dialog.scroll();
        dialog.apply_key(DialogKey::Home, 4);
        assert_eq!(dialog.scroll(), 0);
        dialog.apply_key(DialogKey::Down, 4);
        assert_eq!(dialog.scroll(), 1.min(bottom));
        dialog.apply_key(DialogKey::PageDown, 4);
        assert_eq!(dialog.scroll(), bottom.min(1 + 3));
        dialog.apply_key(DialogKey::PageUp, 4);
        assert_eq!(dialog.scroll(), 0);
    }

    #[test]
    fn the_view_is_a_scrolling_panel_with_a_hint_footer() {
        let dialog = DoctorDialogState::open(&report("ok"));
        let ViewSpec::Panel(view) = dialog.view() else {
            panic!("expected a panel view");
        };
        assert_eq!(view.title, TITLE);
        assert!(view.tabs.is_empty());
        assert!(view.side.is_none());
        assert_eq!(view.body.scroll, 0);
        assert_eq!(view.footer, vec![TextSpan::dim(FOOTER)]);
    }

    #[test]
    fn replacement_resets_report_to_new_results() {
        let mut dialog = DoctorDialogState::open(&report("old"));
        dialog.scroll = 3;
        dialog.replace_report(&report("new"));
        assert!(dialog.text().contains("settings — new"));
        assert!(!dialog.text().contains("settings — old"));
        assert_eq!(dialog.scroll(), 0);
    }

    #[test]
    fn rendered_rows_include_status_and_fix_advice() {
        let rows = report_rows(&report("needs attention"));
        let text = rows
            .iter()
            .map(PanelRow::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("[WARN] settings — needs attention"));
        assert!(text.contains("Fix: Review the settings file."));
    }

    #[test]
    fn a_label_is_emphasised_ahead_of_its_value() {
        // The status word and the summary label are the only emphasis in
        // the report; flattening a row to one tone would lose them.
        let rows = report_rows(&report("ok"));
        assert_eq!(rows[0].spans[0].tone, RowTone::Strong);
        assert_eq!(rows[0].spans[1].tone, RowTone::Normal);
        let status = rows
            .iter()
            .find(|row| row.text().starts_with("[WARN]"))
            .expect("the check row");
        assert_eq!(status.spans[0].tone, RowTone::Strong);
    }

    #[test]
    fn escape_closes_panel() {
        let mut dialog = DoctorDialogState::open(&report("ok"));
        assert_eq!(
            dialog.on_key(DialogKey::Escape.into()),
            DialogOutcome::Close
        );
    }
}
