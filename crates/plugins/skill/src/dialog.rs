//! Inline/screen `/skills` multi-select dialog.
//!
//! Checked rows are enabled skills. The dialog edits only skills loaded in the
//! current session; disabled ids that are not currently loaded are carried
//! through unchanged when the selection is applied.
//!
//! It lives with the skill plugin because the selection it writes is what
//! this plugin's tool reads. The loaded index itself belongs to the front
//! end's `ToolContext`, so the rows arrive as a
//! [`SkillsDialogInput`] rather than as a registry borrow.

use std::collections::BTreeSet;

use rebon_dialog::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, ListAccent, ListRow, ListView,
    ViewSpec,
};
use rebon_ui_seat::ids;
use rebon_ui_seat::input::SkillsDialogInput;

/// Stable id used for action routing.
pub const DIALOG_ID: &str = ids::dialog::SKILLS;
/// Persist and install a selection. The values are the complete
/// disabled-id set, sorted, which is everything the route needs.
pub const ACTION_APPLY: &str = ids::action::APPLY;

/// Largest number of skill rows shown at once; longer lists scroll.
const MAX_VISIBLE: usize = 12;
const DESCRIPTION_MAX_CHARS: usize = 72;
const FOOTER: &str = "↑/↓ or j/k select · Space toggle · Enter apply · Esc cancel";

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillDialogRow {
    id: String,
    source: String,
    description: String,
    checked: bool,
}

/// Cloneable reducer state for the `/skills` selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillsDialogState {
    rows: Vec<SkillDialogRow>,
    selected: usize,
    /// The complete persisted set at open time. Entries absent from `rows`
    /// must survive Apply because they may belong to temporarily-unloaded
    /// project, plugin, or MCP skills.
    original_disabled: BTreeSet<String>,
}

impl SkillsDialogState {
    /// Build a selector from every loaded skill, including disabled ones.
    /// Returns `None` only when the session has no loaded skills.
    pub fn open(input: SkillsDialogInput) -> Option<Self> {
        if input.skills.is_empty() {
            return None;
        }
        let original_disabled: BTreeSet<String> = input.disabled.into_iter().collect();
        let mut skills = input.skills;
        skills.sort_by(|left, right| left.id.cmp(&right.id));
        let rows = skills
            .into_iter()
            .map(|skill| SkillDialogRow {
                checked: !original_disabled.contains(&skill.id),
                id: skill.id,
                source: skill.source,
                description: short_description(&skill.description),
            })
            .collect();
        Some(Self {
            rows,
            selected: 0,
            original_disabled,
        })
    }

    fn disabled_selection(&self) -> BTreeSet<String> {
        let loaded_ids = self
            .rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<BTreeSet<_>>();
        let mut disabled = self
            .original_disabled
            .iter()
            .filter(|id| !loaded_ids.contains(id.as_str()))
            .cloned()
            .collect::<BTreeSet<_>>();
        disabled.extend(
            self.rows
                .iter()
                .filter(|row| !row.checked)
                .map(|row| row.id.clone()),
        );
        disabled
    }

    fn checked_counts(&self) -> (usize, usize) {
        let enabled = self.rows.iter().filter(|row| row.checked).count();
        (enabled, self.rows.len().saturating_sub(enabled))
    }
}

impl DialogModel for SkillsDialogState {
    rebon_dialog::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        let last = self.rows.len().saturating_sub(1);
        match press.key {
            DialogKey::Escape => DialogOutcome::Close,
            DialogKey::Up | DialogKey::Char { value: 'k', .. } => {
                self.selected = if self.selected == 0 {
                    last
                } else {
                    self.selected - 1
                };
                DialogOutcome::None
            }
            DialogKey::Down | DialogKey::Char { value: 'j', .. } => {
                self.selected = if self.selected >= last {
                    0
                } else {
                    self.selected + 1
                };
                DialogOutcome::None
            }
            DialogKey::Char { value: ' ', .. } => {
                if let Some(row) = self.rows.get_mut(self.selected) {
                    row.checked = !row.checked;
                }
                DialogOutcome::None
            }
            // The whole disabled set travels with the action: the host
            // pops the dialog as it fires, so nothing can be read back.
            DialogKey::Enter => DialogOutcome::Action(DialogAction::closing_many(
                DIALOG_ID,
                ACTION_APPLY,
                self.disabled_selection().into_iter().collect(),
            )),
            _ => DialogOutcome::None,
        }
    }

    fn view(&self) -> ViewSpec {
        let (enabled, disabled) = self.checked_counts();
        let rows = self
            .rows
            .iter()
            .map(|row| {
                let mut detail = format!(" [{}]", row.source);
                if !row.description.is_empty() {
                    detail.push_str(&format!(" — {}", row.description));
                }
                ListRow {
                    checked: Some(row.checked),
                    prefix: None,
                    label: row.id.clone(),
                    detail: Some(detail),
                    badge: None,
                }
            })
            .collect();
        ViewSpec::List(ListView {
            title: " Manage skills ".into(),
            header: vec![
                format!("{enabled} enabled · {disabled} disabled"),
                String::new(),
            ],
            rows,
            selected: self.selected,
            footer: vec![FOOTER.into()],
            max_visible: Some(MAX_VISIBLE),
            accent: ListAccent::Brand,
            detail_follows_selection: false,
        })
    }
}

fn short_description(description: &str) -> String {
    let collapsed = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let short = chars
        .by_ref()
        .take(DESCRIPTION_MAX_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_ui_seat::input::SkillRowInput;

    fn skill(id: &str, source: &str, description: &str) -> SkillRowInput {
        SkillRowInput {
            id: id.into(),
            source: source.into(),
            description: description.into(),
        }
    }

    fn input(skills: Vec<SkillRowInput>, disabled: &[&str]) -> SkillsDialogInput {
        SkillsDialogInput {
            skills,
            disabled: disabled.iter().map(|id| (*id).to_string()).collect(),
        }
    }

    fn state(count: usize, disabled: &[&str]) -> SkillsDialogState {
        let skills = (0..count)
            .map(|index| {
                skill(
                    &format!("skill-{index:02}"),
                    "project",
                    &format!("Description for skill {index}"),
                )
            })
            .collect();
        SkillsDialogState::open(input(skills, disabled)).expect("non-empty skills")
    }

    fn list(dialog: &SkillsDialogState) -> ListView {
        match dialog.view() {
            ViewSpec::List(view) => view,
            other => panic!("expected a list view, got {other:?}"),
        }
    }

    #[test]
    fn initial_checkboxes_are_inverse_of_disabled_and_apply_preserves_unloaded_ids() {
        let mut dialog = state(3, &["skill-01", "temporarily-unloaded"]);
        assert!(dialog.rows[0].checked);
        assert!(!dialog.rows[1].checked);
        assert!(dialog.rows[2].checked);

        // Disable skill-00 and re-enable skill-01.
        dialog.on_key(DialogKey::plain(' ').into());
        dialog.on_key(DialogKey::Down.into());
        dialog.on_key(DialogKey::plain(' ').into());
        assert_eq!(
            dialog.on_key(DialogKey::Enter.into()),
            DialogOutcome::Action(DialogAction::closing_many(
                DIALOG_ID,
                ACTION_APPLY,
                vec!["skill-00".to_string(), "temporarily-unloaded".to_string()],
            ))
        );
    }

    #[test]
    fn navigation_wraps_for_arrows_and_jk_and_escape_cancels() {
        let mut dialog = state(3, &[]);
        dialog.on_key(DialogKey::Up.into());
        assert_eq!(dialog.selected, 2);
        dialog.on_key(DialogKey::plain('j').into());
        assert_eq!(dialog.selected, 0);
        dialog.on_key(DialogKey::plain('k').into());
        assert_eq!(dialog.selected, 2);
        assert_eq!(
            dialog.on_key(DialogKey::Escape.into()),
            DialogOutcome::Close
        );
    }

    #[test]
    fn the_view_caps_at_twelve_rows_and_reserves_the_same_chrome() {
        let dialog = state(30, &[]);
        let view = list(&dialog);
        assert_eq!(view.max_visible, Some(MAX_VISIBLE));
        assert_eq!(view.visible_rows(), MAX_VISIBLE);
        assert_eq!(view.desired_height(), MAX_VISIBLE as u16 + 6);
    }

    #[test]
    fn the_view_carries_checkbox_id_source_and_short_description() {
        let dialog = SkillsDialogState::open(input(
            vec![skill(
                "review-pr",
                "user",
                "Review a pull request without submitting a model turn.",
            )],
            &[],
        ))
        .unwrap();
        let view = list(&dialog);
        assert_eq!(view.header[0], "1 enabled · 0 disabled");
        assert_eq!(view.rows[0].checked, Some(true));
        assert_eq!(view.rows[0].label, "review-pr");
        assert_eq!(
            view.rows[0].detail.as_deref(),
            Some(" [user] — Review a pull request without submitting a model turn.")
        );
        assert!(!view.detail_follows_selection);
    }

    #[test]
    fn a_skill_without_a_description_carries_only_its_source() {
        let dialog = SkillsDialogState::open(input(vec![skill("bare", "mcp", "")], &[])).unwrap();
        assert_eq!(list(&dialog).rows[0].detail.as_deref(), Some(" [mcp]"));
    }

    #[test]
    fn toggling_updates_the_header_counts() {
        let mut dialog = state(3, &[]);
        assert_eq!(list(&dialog).header[0], "3 enabled · 0 disabled");
        dialog.on_key(DialogKey::plain(' ').into());
        assert_eq!(list(&dialog).header[0], "2 enabled · 1 disabled");
    }

    #[test]
    fn empty_registry_does_not_open_dialog() {
        assert!(SkillsDialogState::open(SkillsDialogInput::default()).is_none());
    }
}
