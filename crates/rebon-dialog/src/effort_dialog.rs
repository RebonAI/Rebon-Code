//! `/effort` picker over the supported reasoning levels.
//!
//! A [`DialogModel`]: it reduces keys and describes a list, and the
//! host paints it, and nothing here knows about the renderer.

use crate::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, ListAccent, ListBadge, ListRow,
    ListView, ViewSpec,
};

/// Stable id used for action routing.
pub const DIALOG_ID: &str = "effort";
/// The only action this dialog emits: apply the highlighted level.
pub const ACTION_SELECT: &str = "select";

/// The level the picker preselects when the session has no explicit one.
///
/// Levels travel as their ids rather than as an enum from a sibling crate:
/// this crate stays dependency-free, the action already carried the id as a
/// string, and the host that persists the choice is the one that owns the
/// enum anyway.
pub const DEFAULT_LEVEL: &str = "high";
const FOOTER: &str = "↑/↓ select · 1-5 choose · Enter apply · Esc cancel";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EffortOption {
    /// The id this level is persisted and routed as.
    id: &'static str,
    label: &'static str,
    description: &'static str,
}

const OPTIONS: [EffortOption; 5] = [
    EffortOption {
        id: "low",
        label: "Low",
        description: "Fast responses with lighter reasoning",
    },
    EffortOption {
        id: "medium",
        label: "Medium",
        description: "Balances speed and reasoning depth for everyday tasks",
    },
    EffortOption {
        id: "high",
        label: "High",
        description: "Greater reasoning depth for complex problems",
    },
    EffortOption {
        id: "xhigh",
        label: "Extra high",
        description: "Extra high reasoning depth for complex problems",
    },
    EffortOption {
        id: "max",
        label: "Max",
        description: "Maximum reasoning depth for the hardest problems",
    },
];

/// Whether `id` names a level this picker offers. The host resolves it
/// back to its own enum; this is the table both sides agree on.
pub fn is_known_level(id: &str) -> bool {
    OPTIONS.iter().any(|option| option.id == id)
}

/// Every level id the picker offers, in display order.
pub fn level_ids() -> [&'static str; OPTIONS.len()] {
    [
        OPTIONS[0].id,
        OPTIONS[1].id,
        OPTIONS[2].id,
        OPTIONS[3].id,
        OPTIONS[4].id,
    ]
}

/// Reducer state for the `/effort` picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortDialogState {
    model: String,
    current: Option<String>,
    selected: usize,
}

impl EffortDialogState {
    /// Preselects the level `current` names, or the effective default
    /// when it names none. An unknown id is treated as none.
    pub fn open(model: impl Into<String>, current: Option<&str>) -> Self {
        let current = current.filter(|id| is_known_level(id)).map(str::to_string);
        let effective = current.as_deref().unwrap_or(DEFAULT_LEVEL);
        let selected = OPTIONS
            .iter()
            .position(|option| option.id == effective)
            .unwrap_or(0);
        Self {
            model: model.into(),
            current,
            selected,
        }
    }
}

impl DialogModel for EffortDialogState {
    crate::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        let last = OPTIONS.len() - 1;
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
                self.selected = if self.selected == last {
                    0
                } else {
                    self.selected + 1
                };
                DialogOutcome::None
            }
            DialogKey::Char {
                value: value @ '1'..='5',
                ..
            } => {
                let index = value as usize - '1' as usize;
                DialogOutcome::Action(DialogAction::closing(
                    DIALOG_ID,
                    ACTION_SELECT,
                    OPTIONS[index].id,
                ))
            }
            DialogKey::Enter => DialogOutcome::Action(DialogAction::closing(
                DIALOG_ID,
                ACTION_SELECT,
                OPTIONS[self.selected].id,
            )),
            _ => DialogOutcome::None,
        }
    }

    fn view(&self) -> ViewSpec {
        let effective_current = self.current.as_deref().unwrap_or(DEFAULT_LEVEL);
        let rows = OPTIONS
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let default_suffix = if option.id == DEFAULT_LEVEL {
                    " (default)"
                } else {
                    ""
                };
                let label = format!("{}{}", option.label, default_suffix);
                ListRow {
                    checked: None,
                    prefix: Some(format!("{}. ", index + 1)),
                    label: format!("{label:<21}"),
                    detail: Some(option.description.to_string()),
                    badge: (option.id == effective_current).then(|| ListBadge {
                        text: "  (current)".into(),
                        bold: true,
                    }),
                }
            })
            .collect();
        ViewSpec::List(ListView {
            title: format!(" Select Reasoning Level for {} ", self.model),
            header: Vec::new(),
            rows,
            selected: self.selected,
            footer: vec![FOOTER.into()],
            max_visible: None,
            accent: ListAccent::Success,
            detail_follows_selection: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(dialog: &EffortDialogState) -> ListView {
        match dialog.view() {
            ViewSpec::List(view) => view,
            other => panic!("expected a list view, got {other:?}"),
        }
    }

    #[test]
    fn options_are_the_five_supported_levels_without_ultra() {
        assert_eq!(OPTIONS.len(), 5);
        assert_eq!(level_ids(), ["low", "medium", "high", "xhigh", "max"]);
        assert!(OPTIONS.iter().all(|option| {
            !option.label.to_ascii_lowercase().contains("ultra")
                && !option.description.to_ascii_lowercase().contains("ultra")
        }));
    }

    #[test]
    fn auto_preselects_the_effective_default() {
        let dialog = EffortDialogState::open("model", None);
        assert_eq!(OPTIONS[dialog.selected].id, "high");
    }

    #[test]
    fn explicit_current_level_is_preselected() {
        let dialog = EffortDialogState::open("model", Some("xhigh"));
        assert_eq!(OPTIONS[dialog.selected].id, "xhigh");
    }

    #[test]
    fn an_unknown_level_id_falls_back_to_the_default() {
        let dialog = EffortDialogState::open("model", Some("ultra"));
        assert_eq!(OPTIONS[dialog.selected].id, DEFAULT_LEVEL);
        assert!(!is_known_level("ultra"));
    }

    #[test]
    fn navigation_wraps() {
        let mut dialog = EffortDialogState::open("model", Some("low"));
        assert_eq!(dialog.on_key(DialogKey::Up.into()), DialogOutcome::None);
        assert_eq!(dialog.selected, OPTIONS.len() - 1);
        assert_eq!(dialog.on_key(DialogKey::Down.into()), DialogOutcome::None);
        assert_eq!(dialog.selected, 0);
    }

    #[test]
    fn vim_keys_navigate_like_the_arrows() {
        let mut dialog = EffortDialogState::open("model", Some("low"));
        dialog.on_key(DialogKey::plain('k').into());
        assert_eq!(dialog.selected, OPTIONS.len() - 1);
        dialog.on_key(DialogKey::plain('j').into());
        assert_eq!(dialog.selected, 0);
    }

    #[test]
    fn enter_selects_the_highlighted_level() {
        let mut dialog = EffortDialogState::open("model", Some("medium"));
        dialog.on_key(DialogKey::Down.into());
        assert_eq!(
            dialog.on_key(DialogKey::Enter.into()),
            DialogOutcome::Action(DialogAction::closing(DIALOG_ID, ACTION_SELECT, "high"))
        );
    }

    #[test]
    fn number_shortcut_selects_immediately() {
        let mut dialog = EffortDialogState::open("model", None);
        assert_eq!(
            dialog.on_key(DialogKey::plain('4').into()),
            DialogOutcome::Action(DialogAction::closing(DIALOG_ID, ACTION_SELECT, "xhigh"))
        );
        assert_eq!(
            dialog.on_key(DialogKey::plain('5').into()),
            DialogOutcome::Action(DialogAction::closing(DIALOG_ID, ACTION_SELECT, "max"))
        );
    }

    #[test]
    fn escape_closes_without_selecting() {
        let mut dialog = EffortDialogState::open("model", None);
        assert_eq!(
            dialog.on_key(DialogKey::Escape.into()),
            DialogOutcome::Close
        );
    }

    #[test]
    fn every_option_id_is_recognised_and_nothing_else_is() {
        for option in OPTIONS {
            assert!(is_known_level(option.id));
        }
        assert!(!is_known_level("ultra"));
    }

    #[test]
    fn desired_height_fits_all_rows_and_footer() {
        let dialog = EffortDialogState::open("model", None);
        assert_eq!(list(&dialog).desired_height(), 9);
    }

    #[test]
    fn the_view_names_the_model_and_marks_the_current_level() {
        let dialog = EffortDialogState::open("gpt-5.6-sol", Some("max"));
        let view = list(&dialog);
        assert_eq!(view.title, " Select Reasoning Level for gpt-5.6-sol ");
        assert_eq!(view.rows.len(), 5);
        assert_eq!(view.selected, 4);
        assert_eq!(view.accent, ListAccent::Success);
        assert!(view.header.is_empty());
        assert_eq!(view.footer, vec![FOOTER.to_string()]);
        // Only the current level is badged, and "High" keeps its
        // "(default)" suffix inside the padded label column.
        let badged: Vec<&str> = view
            .rows
            .iter()
            .filter(|row| row.badge.is_some())
            .map(|row| row.label.trim_end())
            .collect();
        assert_eq!(badged, vec!["Max"]);
        assert_eq!(view.rows[2].label, format!("{:<21}", "High (default)"));
        assert_eq!(view.rows[0].prefix.as_deref(), Some("1. "));
    }
}
