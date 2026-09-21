//! Inline/screen `/provider` switcher.
//!
//! A bare `/provider` opens this compact list picker over the configured
//! custom providers plus a "Default (env)" row. It mirrors the `/model`
//! picker: `↑/↓` to move, `Enter` to *activate* the highlighted provider
//! (switch the runtime to it), `Esc` to cancel. Add / edit / remove are
//! secondary actions that escalate to the full provider form (the
//! onboarding provider panel) or mutate config directly:
//!
//! * `a` — add a new provider (opens the form in add mode).
//! * `e` — edit the highlighted provider (opens the form in edit mode).
//! * `d` — remove the highlighted provider.
//!
//! `/provider add` bypasses this switcher and opens the form directly;
//! the other `/provider …` subcommands keep the text CRUD surface.
//!
//! Reading the configured providers is the host's job: this crate stays
//! dependency-free, so the switcher is handed its rows already built.

use crate::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, ListAccent, ListBadge, ListRow,
    ListView, ViewSpec,
};

/// Stable id used for action routing.
pub const DIALOG_ID: &str = "provider";
/// Activate the highlighted provider. One value, or none for the env row.
pub const ACTION_ACTIVATE: &str = "activate";
/// Open the provider form in add mode. No values.
pub const ACTION_ADD: &str = "add";
/// Open the provider form editing the named provider.
pub const ACTION_EDIT: &str = "edit";
/// Remove the named provider, then rebuild the still-open switcher.
pub const ACTION_REMOVE: &str = "remove";

/// Largest number of rows shown at once; longer lists scroll.
const MAX_VISIBLE: usize = 10;
const FOOTER_TOP: &str = "↑/↓ select · Enter switch · a add";
const FOOTER_BOTTOM: &str = "e edit · d remove · Esc cancel";

/// One row in the switcher. `name == None` is the "Default (env)" row that
/// deactivates any custom provider.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderRow {
    name: Option<String>,
    subtitle: String,
}

/// Cloneable reducer state for the `/provider` switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDialogState {
    rows: Vec<ProviderRow>,
    /// Active provider name, or `None` when the env fallback is active.
    active: Option<String>,
    /// Highlighted row.
    selected: usize,
}

impl ProviderDialogState {
    /// Build the switcher from the configured providers, newest row last.
    ///
    /// `rows` is `(name, subtitle)` per configured provider; the
    /// "Default (env)" row is appended here so every host spells it the
    /// same way. `active` is the provider in use, or `None` for the
    /// environment fallback.
    pub fn new<I, N, S>(providers: I, active: Option<String>) -> Self
    where
        I: IntoIterator<Item = (N, S)>,
        N: Into<String>,
        S: Into<String>,
    {
        let mut rows: Vec<ProviderRow> = providers
            .into_iter()
            .map(|(name, subtitle)| ProviderRow {
                name: Some(name.into()),
                subtitle: subtitle.into(),
            })
            .collect();
        rows.push(ProviderRow {
            name: None,
            subtitle: "environment credentials".to_string(),
        });
        let selected = match &active {
            Some(name) => rows
                .iter()
                .position(|row| row.name.as_deref() == Some(name.as_str()))
                .unwrap_or(rows.len() - 1),
            None => rows.len() - 1,
        };
        Self {
            rows,
            active,
            selected,
        }
    }

    fn selected_provider_name(&self) -> Option<String> {
        self.rows
            .get(self.selected)
            .and_then(|row| row.name.clone())
    }

    /// A closing action carrying the highlighted provider's name, or
    /// `None` on the env row where the action does not apply.
    fn on_selected(&self, action: &'static str) -> DialogOutcome {
        match self.selected_provider_name() {
            Some(name) => DialogOutcome::Action(DialogAction::closing(DIALOG_ID, action, name)),
            None => DialogOutcome::None,
        }
    }
}

impl DialogModel for ProviderDialogState {
    crate::dialog_plumbing!();

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
            // The env row activates with no value at all, which is what
            // distinguishes it from a provider that happens to be named
            // with an empty string.
            DialogKey::Enter => DialogOutcome::Action(DialogAction::closing_many(
                DIALOG_ID,
                ACTION_ACTIVATE,
                self.selected_provider_name().into_iter().collect(),
            )),
            DialogKey::Char {
                value: 'a' | 'A', ..
            } => DialogOutcome::Action(DialogAction::closing_many(
                DIALOG_ID,
                ACTION_ADD,
                Vec::new(),
            )),
            DialogKey::Char {
                value: 'e' | 'E', ..
            } => self.on_selected(ACTION_EDIT),
            DialogKey::Char {
                value: 'd' | 'D', ..
            } => self.on_selected(ACTION_REMOVE),
            _ => DialogOutcome::None,
        }
    }

    fn view(&self) -> ViewSpec {
        let rows = self
            .rows
            .iter()
            .map(|row| {
                let label = row.name.as_deref().unwrap_or("Default (env)");
                ListRow {
                    checked: None,
                    prefix: None,
                    label: format!("{label:<16}"),
                    detail: Some(row.subtitle.clone()),
                    badge: (row.name == self.active).then(|| ListBadge {
                        text: "  (active)".into(),
                        bold: false,
                    }),
                }
            })
            .collect();
        let active_label = self.active.as_deref().unwrap_or("environment credentials");
        ViewSpec::List(ListView {
            title: " Select provider ".into(),
            header: vec![format!("Active: {active_label}"), String::new()],
            rows,
            selected: self.selected,
            footer: vec![FOOTER_TOP.into(), FOOTER_BOTTOM.into()],
            max_visible: Some(MAX_VISIBLE),
            accent: ListAccent::Brand,
            detail_follows_selection: false,
        })
    }

    /// Compact like the `/model` picker: it sits in the prompt area
    /// inline rather than claiming the viewport.
    fn is_fullscreen(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(names: &[&str], active: Option<&str>) -> ProviderDialogState {
        let mut rows: Vec<ProviderRow> = names
            .iter()
            .map(|n| ProviderRow {
                name: Some(n.to_string()),
                subtitle: "model".into(),
            })
            .collect();
        rows.push(ProviderRow {
            name: None,
            subtitle: "environment credentials".into(),
        });
        let active = active.map(str::to_string);
        let selected = match &active {
            Some(name) => rows
                .iter()
                .position(|r| r.name.as_deref() == Some(name.as_str()))
                .unwrap_or(rows.len() - 1),
            None => rows.len() - 1,
        };
        ProviderDialogState {
            rows,
            active,
            selected,
        }
    }

    fn list(dialog: &ProviderDialogState) -> ListView {
        match dialog.view() {
            ViewSpec::List(view) => view,
            other => panic!("expected a list view, got {other:?}"),
        }
    }

    fn action(name: &'static str, values: Vec<String>) -> DialogOutcome {
        DialogOutcome::Action(DialogAction::closing_many(DIALOG_ID, name, values))
    }

    #[test]
    fn enter_activates_highlighted_provider() {
        let mut s = state(&["deepseek", "glm"], Some("deepseek"));
        assert_eq!(
            s.on_key(DialogKey::Enter.into()),
            action(ACTION_ACTIVATE, vec!["deepseek".into()])
        );
        s.on_key(DialogKey::Down.into());
        assert_eq!(
            s.on_key(DialogKey::Enter.into()),
            action(ACTION_ACTIVATE, vec!["glm".into()])
        );
    }

    #[test]
    fn enter_on_env_row_activates_default_with_no_value() {
        let mut s = state(&["deepseek"], Some("deepseek"));
        // env row is last; move down from index 0 to it.
        s.on_key(DialogKey::Down.into());
        assert_eq!(
            s.on_key(DialogKey::Enter.into()),
            action(ACTION_ACTIVATE, Vec::new())
        );
    }

    #[test]
    fn edit_and_remove_only_target_providers_not_env() {
        let mut s = state(&["deepseek"], Some("deepseek"));
        assert_eq!(
            s.on_key(DialogKey::plain('e').into()),
            action(ACTION_EDIT, vec!["deepseek".into()])
        );
        assert_eq!(
            s.on_key(DialogKey::plain('d').into()),
            action(ACTION_REMOVE, vec!["deepseek".into()])
        );
        // On the env row, edit/remove are no-ops.
        s.on_key(DialogKey::Down.into());
        assert_eq!(s.on_key(DialogKey::plain('e').into()), DialogOutcome::None);
        assert_eq!(s.on_key(DialogKey::plain('d').into()), DialogOutcome::None);
    }

    #[test]
    fn uppercase_shortcuts_work_too() {
        let mut s = state(&["deepseek"], Some("deepseek"));
        assert_eq!(
            s.on_key(DialogKey::plain('E').into()),
            action(ACTION_EDIT, vec!["deepseek".into()])
        );
        assert_eq!(
            s.on_key(DialogKey::plain('D').into()),
            action(ACTION_REMOVE, vec!["deepseek".into()])
        );
        assert_eq!(
            s.on_key(DialogKey::plain('A').into()),
            action(ACTION_ADD, Vec::new())
        );
    }

    #[test]
    fn a_adds_and_esc_closes() {
        let mut s = state(&["deepseek"], None);
        assert_eq!(
            s.on_key(DialogKey::plain('a').into()),
            action(ACTION_ADD, Vec::new())
        );
        assert_eq!(s.on_key(DialogKey::Escape.into()), DialogOutcome::Close);
    }

    #[test]
    fn no_active_selects_env_row() {
        let s = state(&["deepseek", "glm"], None);
        assert_eq!(s.selected, 2); // env row is last
        assert_eq!(s.selected_provider_name(), None);
    }

    #[test]
    fn the_view_pads_labels_badges_the_active_row_and_keeps_details_dim() {
        let s = state(&["deepseek", "glm"], Some("glm"));
        let view = list(&s);
        assert_eq!(view.title, " Select provider ");
        assert_eq!(view.header, vec!["Active: glm".to_string(), String::new()]);
        assert_eq!(
            view.footer,
            vec![FOOTER_TOP.to_string(), FOOTER_BOTTOM.to_string()]
        );
        assert!(!view.detail_follows_selection);
        assert_eq!(view.rows[0].label, format!("{:<16}", "deepseek"));
        assert_eq!(view.rows[2].label, format!("{:<16}", "Default (env)"));
        let badged: Vec<&str> = view
            .rows
            .iter()
            .filter(|row| row.badge.is_some())
            .map(|row| row.label.trim_end())
            .collect();
        assert_eq!(badged, vec!["glm"]);
        assert!(view.rows.iter().all(|row| row.checked.is_none()));
    }

    #[test]
    fn the_env_row_is_badged_when_nothing_is_active() {
        let view = list(&state(&["deepseek"], None));
        assert!(view.rows[0].badge.is_none());
        assert!(view.rows[1].badge.is_some());
    }

    #[test]
    fn desired_height_grows_with_rows_up_to_cap() {
        // 1 provider + env row, plus border(2) + header(2) + blank(1)
        // + two footer lines.
        assert_eq!(list(&state(&["a"], None)).desired_height(), 9);
        let names: Vec<String> = (0..20).map(|i| format!("p{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(
            list(&state(&refs, None)).desired_height(),
            (MAX_VISIBLE as u16) + 7
        );
    }
}
