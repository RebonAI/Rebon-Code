//! Inline/screen `/model` picker.
//!
//! A bare `/model` opens this compact list picker over the *active custom
//! provider's* models: the ones its `models[]` lists (with the current
//! `model` guaranteed present), then the rest of that vendor's catalogue,
//! marked as such. Selecting a row switches the provider's active model and
//! persists it — the same side effect as `/model <name>`, but chosen from a
//! list instead of typed.
//!
//! `open` returns `None` when no custom provider is active or it has no
//! models, in which case the caller falls back to the text `/model` command
//! whose message tells the user how to activate a provider first.
//!
//! A [`DialogModel`]: the host stack routes its keys and paints its
//! [`ViewSpec`], in a centred overlay in screen mode and in the prompt
//! area in inline mode.
//!
//! Reading the active provider is the host's job: this crate stays
//! dependency-free, so the picker is handed its model list already built.

use crate::model::{
    DialogAction, DialogKey, DialogModel, DialogOutcome, KeyPress, ListAccent, ListBadge, ListRow,
    ListView, ViewSpec,
};

/// Stable id used for action routing and native view lookup.
pub const DIALOG_ID: &str = "model";
/// The only action this dialog emits: switch to the highlighted model.
pub const ACTION_SELECT: &str = "select";

/// Largest number of model rows shown at once; longer lists scroll.
const MAX_VISIBLE: usize = 10;
const FOOTER: &str = "↑/↓ select · Enter apply · Esc cancel";

/// One selectable model, and what the picker says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    /// The id sent on the wire and written to config.
    pub id: String,
    /// Context window and price, when a catalogue knows them.
    pub detail: Option<String>,
    /// `false` for a row the catalogue supplied rather than the user.
    pub configured: bool,
}

impl ModelOption {
    /// A model the provider entry lists.
    pub fn configured(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            detail: None,
            configured: true,
        }
    }
}

/// Cloneable reducer state for the `/model` picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDialogState {
    /// Active custom provider whose model is being switched.
    provider_name: String,
    /// Selectable models (current model guaranteed present).
    options: Vec<ModelOption>,
    /// The provider's currently-active model, tagged in the list.
    current_model: String,
    /// Highlighted row.
    selected: usize,
}

impl ModelDialogState {
    /// Build the picker over one provider's models.
    ///
    /// Returns `None` when there is nothing to pick from, which is the
    /// caller's cue to fall back to the text `/model` command. The
    /// current model stays selectable even for a legacy config that only
    /// stored the scalar `model` field.
    pub fn new(
        provider_name: impl Into<String>,
        mut options: Vec<ModelOption>,
        current_model: impl Into<String>,
    ) -> Option<Self> {
        let current_model = current_model.into();
        if !current_model.is_empty() && !options.iter().any(|model| model.id == current_model) {
            options.insert(0, ModelOption::configured(current_model.clone()));
        }
        if options.is_empty() {
            return None;
        }
        let selected = options
            .iter()
            .position(|model| model.id == current_model)
            .unwrap_or(0);
        Some(Self {
            provider_name: provider_name.into(),
            options,
            current_model,
            selected,
        })
    }
}

impl DialogModel for ModelDialogState {
    crate::dialog_plumbing!();

    fn id(&self) -> &'static str {
        DIALOG_ID
    }

    fn on_key(&mut self, press: KeyPress) -> DialogOutcome {
        let last = self.options.len().saturating_sub(1);
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
            DialogKey::Enter => self
                .options
                .get(self.selected)
                .map(|model| {
                    let model = &model.id;
                    // The route needs the provider this picker was
                    // opened for, and the host pops the dialog as the
                    // action fires, so it travels with the model.
                    DialogOutcome::Action(DialogAction::closing_many(
                        DIALOG_ID,
                        ACTION_SELECT,
                        vec![model.clone(), self.provider_name.clone()],
                    ))
                })
                .unwrap_or(DialogOutcome::Close),
            _ => DialogOutcome::None,
        }
    }

    fn view(&self) -> ViewSpec {
        let rows = self
            .options
            .iter()
            .map(|model| ListRow {
                checked: None,
                prefix: None,
                label: model.id.clone(),
                detail: model.detail.clone(),
                // The current model wins the badge: which model is running
                // matters more than where the row came from.
                badge: if model.id == self.current_model {
                    Some(ListBadge {
                        text: " (current)".into(),
                        bold: false,
                    })
                } else if !model.configured {
                    Some(ListBadge {
                        text: " (catalogue)".into(),
                        bold: false,
                    })
                } else {
                    None
                },
            })
            .collect();
        ViewSpec::List(ListView {
            title: " Select model ".into(),
            header: vec![
                format!("Active provider: {}", self.provider_name),
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

    /// The picker sizes itself to its content and, inline, sits in the
    /// prompt area rather than taking the viewport, so it does not claim
    /// focus from the surface behind it.
    fn is_fullscreen(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(models: &[&str], current: &str) -> ModelDialogState {
        ModelDialogState {
            provider_name: "prov".into(),
            options: models
                .iter()
                .map(|model| ModelOption::configured(*model))
                .collect(),
            current_model: current.into(),
            selected: models.iter().position(|m| *m == current).unwrap_or(0),
        }
    }

    fn list(dialog: &ModelDialogState) -> ListView {
        match dialog.view() {
            ViewSpec::List(view) => view,
            other => panic!("expected a list view, got {other:?}"),
        }
    }

    fn select(model: &str) -> DialogOutcome {
        DialogOutcome::Action(DialogAction::closing_many(
            DIALOG_ID,
            ACTION_SELECT,
            vec![model.into(), "prov".into()],
        ))
    }

    #[test]
    fn enter_selects_highlighted_model() {
        let mut s = state(&["a", "b", "c"], "a");
        assert_eq!(s.on_key(DialogKey::Down.into()), DialogOutcome::None);
        assert_eq!(s.on_key(DialogKey::Enter.into()), select("b"));
    }

    #[test]
    fn esc_closes() {
        let mut s = state(&["a", "b"], "a");
        assert_eq!(s.on_key(DialogKey::Escape.into()), DialogOutcome::Close);
    }

    #[test]
    fn the_picker_is_not_a_fullscreen_modal() {
        // It sits in the prompt area inline rather than owning the viewport.
        assert!(!state(&["a"], "a").is_fullscreen());
    }

    #[test]
    fn navigation_wraps() {
        let mut s = state(&["a", "b", "c"], "a");
        // Up from the top wraps to the last row.
        s.on_key(DialogKey::Up.into());
        assert_eq!(s.selected, 2);
        // Down from the last row wraps back to the top.
        s.on_key(DialogKey::Down.into());
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn vim_keys_navigate_like_the_arrows() {
        let mut s = state(&["a", "b", "c"], "a");
        s.on_key(DialogKey::plain('k').into());
        assert_eq!(s.selected, 2);
        s.on_key(DialogKey::plain('j').into());
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn preselects_current_model() {
        let s = state(&["a", "b", "c"], "b");
        assert_eq!(s.selected, 1);
    }

    #[test]
    fn the_view_names_the_provider_and_badges_the_current_model() {
        let s = state(&["a", "b", "c"], "b");
        let view = list(&s);
        assert_eq!(view.title, " Select model ");
        assert_eq!(
            view.header,
            vec!["Active provider: prov".to_string(), String::new()]
        );
        assert_eq!(view.accent, ListAccent::Brand);
        assert_eq!(view.max_visible, Some(MAX_VISIBLE));
        let badged: Vec<&str> = view
            .rows
            .iter()
            .filter(|row| row.badge.is_some())
            .map(|row| row.label.as_str())
            .collect();
        assert_eq!(badged, vec!["b"]);
        assert!(view.rows.iter().all(|row| row.prefix.is_none()));
        assert!(view.rows.iter().all(|row| row.detail.is_none()));
    }

    /// A picker that only ever showed `models[]` could not say where a row
    /// came from. Now that the vendor's catalogue fills in the rest, a row
    /// the user did not configure has to say so — and carry the context and
    /// price that make it pickable in the first place.
    #[test]
    fn catalogue_rows_are_badged_and_carry_their_detail() {
        let dialog = ModelDialogState {
            provider_name: "openai".into(),
            options: vec![
                ModelOption::configured("gpt-6-astra"),
                ModelOption {
                    id: "gpt-5.6-luna".into(),
                    detail: Some("1.05M ctx · $0.2 / $1.2 per M".into()),
                    configured: false,
                },
            ],
            current_model: "gpt-6-astra".into(),
            selected: 0,
        };
        let view = list(&dialog);
        assert_eq!(view.rows[0].badge.as_ref().unwrap().text, " (current)");
        assert!(view.rows[0].detail.is_none());
        assert_eq!(view.rows[1].badge.as_ref().unwrap().text, " (catalogue)");
        assert!(view.rows[1].detail.as_deref().unwrap().contains("ctx"));
    }

    #[test]
    fn desired_height_grows_with_options_up_to_cap() {
        let small = state(&["a", "b"], "a");
        assert_eq!(list(&small).desired_height(), 8);
        let many: Vec<ModelOption> = (0..20)
            .map(|i| ModelOption::configured(format!("m{i}")))
            .collect();
        let big = ModelDialogState {
            provider_name: "p".into(),
            options: many,
            current_model: "m0".into(),
            selected: 0,
        };
        // Capped at MAX_VISIBLE rows + chrome.
        assert_eq!(list(&big).desired_height(), (MAX_VISIBLE as u16) + 6);
    }
}
