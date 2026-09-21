//! TUI host for the `/onboarding` and `/provider` slash commands.
//!
//! Presents the onboarding wizard as a full-screen dialog overlay:
//!
//! 1. **Theme** — interactive 6-option picker via
//!    `rebon-picker::theme_picker`.
//! 2. **UI mode** — chooses full-screen or inline terminal rendering.
//! 3. **Setup** (conditional on first run) — provider setup and review.
//!    Reopened onboarding first shows `ExistingSetup` when prior state exists;
//!    explicit reconfiguration then opens `LoginMethods` or `ProviderPanel`.
//! 4. **Import** (conditional) — copies supported skills and agents without overwriting.
//! 5. **Security** — concrete review and prompt-injection guidance.
//! 6. **Ready** — truthful summary that remains visible until Enter.
//!
//! Trust directory is handled separately as a blocking pre-startup
//! dialog in `startup_dialog.rs`, not part of the onboarding flow.
//!
//! [`run_startup_onboarding`] drives the same state machine from a
//! standalone blocking event loop, used by `tui::run` when neither
//! `~/.rebon/config.json` nor the env-var fallback yields a provider.
//! It matches the `app.onboarding_dialog` dispatch in
//! `runner/mod.rs`, calling the same `rebon_config` side-effect
//! helpers so the on-disk state ends up identical regardless of
//! which entry point the user took.

use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use rebon_design_system::theme::get_theme;
use rebon_picker::theme_picker;
use rebon_tui::parse_theme_color;

use crate::ui_config::UiMode;
#[cfg(test)]
use rebon_plugin_onboarding::migrate::DiscoverySnapshot;
use rebon_plugin_onboarding::migrate::ImportCategory;
#[cfg(test)]
use rebon_plugin_onboarding::migrate::ImportSummary;
#[cfg(test)]
use rebon_plugin_onboarding::onboarding::actionable_login_options;
use rebon_plugin_onboarding::{
    apply_add_provider, apply_add_provider_model, apply_run_migration, apply_update_provider,
    dialog::{
        state::{
            ExistingSetupChoice, OAuthView, OnboardingDialogState, PanelStatus, ProviderFormState,
            ProviderPresetSelection, PROVIDER_FORMATS,
        },
        OnboardingDialogOutcome,
    },
    has_any_provider, load_provider_snapshot,
    onboarding::{
        grouped_login_pane_rows, login_row_for_focus, preset_window, LoginPaneRow, ProviderField,
        ProviderFormMode, SetupPane, Step, TextField,
    },
};
#[cfg(test)]
use rebon_plugin_onboarding::{
    dialog::{state::ProviderSnapshot, OnboardingStepTransition},
    OnboardingOpenInputs,
};

// ── State ────────────────────────────────────────────────────────

fn step_navigation_outcome(
    state: &mut OnboardingDialogState,
    key: &KeyEvent,
) -> Option<OnboardingDialogOutcome> {
    if state.dialog_title != "Setup guide"
        || !key.modifiers.is_empty()
        || !matches!(key.kind, KeyEventKind::Press)
        || matches!(state.current_step(), Some(Step::Provider))
            && (state.setup_pane != SetupPane::LoginMethods || state.login_from_existing_setup)
    {
        return None;
    }

    match key.code {
        KeyCode::Left => {
            let transition = state.retreat();
            if transition.previous_step == transition.next_step {
                Some(OnboardingDialogOutcome::None)
            } else {
                Some(OnboardingDialogOutcome::Advanced(transition))
            }
        }
        KeyCode::Right => Some(state.advance_step_outcome()),
        _ => None,
    }
}

// ── Key handling ─────────────────────────────────────────────

pub fn handle_key(state: &mut OnboardingDialogState, key: &KeyEvent) -> OnboardingDialogOutcome {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return OnboardingDialogOutcome::None;
    }

    // The OAuth sub-view completely owns the Provider step while
    // active. Esc cancels and bubbles up to the runner so the
    // listener / in-flight POST can be torn down before we fall
    // back to LoginMethods.
    if state.oauth_view.is_some() {
        if key.code == KeyCode::Esc {
            state.oauth_view = None;
            return OnboardingDialogOutcome::CancelOpenAIOAuth;
        }
        return handle_oauth_key(state, key);
    }

    if key.code == KeyCode::Esc {
        if matches!(state.current_step(), Some(Step::Provider))
            && state.setup_pane == SetupPane::ProviderPanel
        {
            if state.dialog_title == "Provider" {
                return OnboardingDialogOutcome::Close;
            }
            state.setup_pane = SetupPane::LoginMethods;
            state.add_provider_status = PanelStatus::None;
            state.add_model_status = PanelStatus::None;
            return OnboardingDialogOutcome::None;
        }
        if matches!(state.current_step(), Some(Step::Provider))
            && state.setup_pane == SetupPane::LoginMethods
            && state.login_from_existing_setup
        {
            state.setup_pane = SetupPane::ExistingSetup;
            state.login_from_existing_setup = false;
            state.add_provider_status = PanelStatus::None;
            return OnboardingDialogOutcome::None;
        }
        return OnboardingDialogOutcome::Close;
    }

    if let Some(outcome) = step_navigation_outcome(state, key) {
        return outcome;
    }

    match state.current_step() {
        Some(Step::Theme) => handle_theme_key(state, key),
        Some(Step::UiMode) => handle_ui_mode_key(state, key),
        Some(Step::Provider) => handle_provider_key(state, key),
        Some(Step::Migration) => handle_migration_key(state, key),
        Some(Step::Security) => handle_security_key(state, key),
        Some(Step::Done) => handle_done_key(state, key),
        None => OnboardingDialogOutcome::Close,
    }
}

fn handle_theme_key(state: &mut OnboardingDialogState, key: &KeyEvent) -> OnboardingDialogOutcome {
    let count = state.theme_options.len();
    if count == 0 {
        return OnboardingDialogOutcome::None;
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.theme_focus = if state.theme_focus == 0 {
                count.saturating_sub(1)
            } else {
                state.theme_focus - 1
            };
            OnboardingDialogOutcome::ThemePreview(state.theme_options[state.theme_focus].1)
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.theme_focus = if state.theme_focus + 1 >= count {
                0
            } else {
                state.theme_focus + 1
            };
            OnboardingDialogOutcome::ThemePreview(state.theme_options[state.theme_focus].1)
        }
        KeyCode::Char(ch @ '1'..='6') => {
            let idx = (ch as usize) - ('1' as usize);
            if idx < count {
                state.theme_focus = idx;
                OnboardingDialogOutcome::ThemePreview(state.theme_options[state.theme_focus].1)
            } else {
                OnboardingDialogOutcome::None
            }
        }
        KeyCode::Enter => {
            let setting = state.theme_options[state.theme_focus].1;
            let transition = state.advance();
            OnboardingDialogOutcome::ThemeSelected {
                setting,
                transition,
            }
        }
        _ => OnboardingDialogOutcome::None,
    }
}

fn handle_ui_mode_key(
    state: &mut OnboardingDialogState,
    key: &KeyEvent,
) -> OnboardingDialogOutcome {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
            state.ui_mode = match state.ui_mode {
                UiMode::Screen => UiMode::Inline,
                UiMode::Inline => UiMode::Screen,
            };
            OnboardingDialogOutcome::None
        }
        KeyCode::Char('1') => {
            state.ui_mode = UiMode::Screen;
            OnboardingDialogOutcome::None
        }
        KeyCode::Char('2') => {
            state.ui_mode = UiMode::Inline;
            OnboardingDialogOutcome::None
        }
        KeyCode::Enter => state.advance_step_outcome(),
        _ => OnboardingDialogOutcome::None,
    }
}

fn handle_provider_key(
    state: &mut OnboardingDialogState,
    key: &KeyEvent,
) -> OnboardingDialogOutcome {
    match state.setup_pane {
        SetupPane::ExistingSetup => handle_existing_setup_key(state, key),
        SetupPane::LoginMethods => handle_login_pane_key(state, key),
        SetupPane::ProviderPanel => handle_provider_panel_key(state, key),
    }
}

fn handle_existing_setup_key(
    state: &mut OnboardingDialogState,
    key: &KeyEvent,
) -> OnboardingDialogOutcome {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
            state.existing_setup_choice = match state.existing_setup_choice {
                ExistingSetupChoice::Preserve => ExistingSetupChoice::Reconfigure,
                ExistingSetupChoice::Reconfigure => ExistingSetupChoice::Preserve,
            };
            OnboardingDialogOutcome::None
        }
        KeyCode::Enter | KeyCode::Right => match state.existing_setup_choice {
            ExistingSetupChoice::Preserve => state.advance_step_outcome(),
            ExistingSetupChoice::Reconfigure => {
                state.setup_pane = SetupPane::LoginMethods;
                state.login_from_existing_setup = true;
                state.login_focus = 0;
                state.add_provider_status = PanelStatus::None;
                state.add_model_status = PanelStatus::None;
                OnboardingDialogOutcome::None
            }
        },
        KeyCode::Left => {
            let transition = state.retreat();
            if transition.previous_step == transition.next_step {
                OnboardingDialogOutcome::None
            } else {
                OnboardingDialogOutcome::Advanced(transition)
            }
        }
        _ => OnboardingDialogOutcome::None,
    }
}

fn handle_login_pane_key(
    state: &mut OnboardingDialogState,
    key: &KeyEvent,
) -> OnboardingDialogOutcome {
    let total = state.login_options.len() + 1; // +1 for the "Add custom provider" CTA row.
    match key.code {
        KeyCode::Left if state.login_from_existing_setup => {
            state.setup_pane = SetupPane::ExistingSetup;
            state.login_from_existing_setup = false;
            state.add_provider_status = PanelStatus::None;
            OnboardingDialogOutcome::None
        }
        KeyCode::Right if state.login_from_existing_setup => state.advance_step_outcome(),
        KeyCode::Up | KeyCode::Char('k') => {
            state.login_focus = if state.login_focus == 0 {
                total - 1
            } else {
                state.login_focus - 1
            };
            OnboardingDialogOutcome::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.login_focus = if state.login_focus + 1 >= total {
                0
            } else {
                state.login_focus + 1
            };
            OnboardingDialogOutcome::None
        }
        KeyCode::Enter => match login_row_for_focus(&state.login_options, state.login_focus) {
            LoginPaneRow::Option(option_idx) => {
                let option_value = state.login_options[option_idx].value;
                if option_value == "openai" {
                    OnboardingDialogOutcome::StartOpenAIOAuth
                } else {
                    state.add_provider_status =
                        PanelStatus::Err("This login method is unavailable in this build.".into());
                    OnboardingDialogOutcome::None
                }
            }
            LoginPaneRow::CustomProviderCta | LoginPaneRow::Header(_) => {
                state.setup_pane = SetupPane::ProviderPanel;
                state.provider_form = ProviderFormState::default();
                state.provider_form_mode = ProviderFormMode::Add;
                state.refresh_provider_snapshot(load_provider_snapshot());
                state.selected_provider = None;
                state.provider_field_focus = ProviderField::Preset;
                state.add_provider_status = PanelStatus::None;
                state.add_model_status = PanelStatus::None;
                OnboardingDialogOutcome::None
            }
        },
        _ => OnboardingDialogOutcome::None,
    }
}

fn handle_provider_panel_key(
    state: &mut OnboardingDialogState,
    key: &KeyEvent,
) -> OnboardingDialogOutcome {
    // Tab / BackTab: move focus across form+models.
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        state.cycle_panel_focus(matches!(key.code, KeyCode::Tab));
        return OnboardingDialogOutcome::None;
    }

    if state.provider_field_focus == ProviderField::ProviderList {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                state.cycle_provider_list_selection(false);
                return OnboardingDialogOutcome::None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.cycle_provider_list_selection(true);
                return OnboardingDialogOutcome::None;
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                if state.selected_provider.is_some() {
                    state.start_edit_selected_provider();
                } else {
                    state.start_add_provider_from_list();
                }
                return OnboardingDialogOutcome::None;
            }
            _ => return OnboardingDialogOutcome::None,
        }
    }

    // Enter: submit whichever button is focused, otherwise cycle
    // to the next field.
    if key.code == KeyCode::Enter {
        return match state.provider_field_focus {
            ProviderField::ProviderList => {
                if state.selected_provider.is_some() {
                    state.start_edit_selected_provider();
                } else {
                    state.start_add_provider_from_list();
                }
                OnboardingDialogOutcome::None
            }
            ProviderField::Preset => {
                state.provider_form.apply_selected_preset();
                if state.provider_form.selected_provider_preset().is_some() {
                    state.provider_field_focus = ProviderField::ApiKey;
                } else {
                    state.provider_field_focus = ProviderField::Name;
                }
                state.add_provider_status = PanelStatus::None;
                OnboardingDialogOutcome::None
            }
            ProviderField::SaveProviderButton => state.submit_save_provider(),
            ProviderField::AddModelButton => state.submit_add_model(),
            _ => {
                // Convenience: Enter in a text field advances focus
                // (like Tab) so the user can walk through the form.
                state.cycle_panel_focus(true);
                OnboardingDialogOutcome::None
            }
        };
    }

    if state.provider_field_focus == ProviderField::Preset {
        match key.code {
            KeyCode::Up | KeyCode::Left | KeyCode::Char('k') | KeyCode::Char('h') => {
                state.provider_form.cycle_preset(false);
                state.add_provider_status = PanelStatus::None;
                return OnboardingDialogOutcome::None;
            }
            KeyCode::Down
            | KeyCode::Right
            | KeyCode::Char('j')
            | KeyCode::Char('l')
            | KeyCode::Char(' ') => {
                state.provider_form.cycle_preset(true);
                state.add_provider_status = PanelStatus::None;
                return OnboardingDialogOutcome::None;
            }
            _ => return OnboardingDialogOutcome::None,
        }
    }

    // Format field is a toggler, not a text input.
    if state.provider_field_focus == ProviderField::Format {
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                state.provider_form.cycle_format(false);
                state.add_provider_status = PanelStatus::None;
                return OnboardingDialogOutcome::None;
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                state.provider_form.cycle_format(true);
                state.add_provider_status = PanelStatus::None;
                return OnboardingDialogOutcome::None;
            }
            _ => return OnboardingDialogOutcome::None,
        }
    }

    // Buttons ignore text editing keys except Enter (handled above).
    if matches!(
        state.provider_field_focus,
        ProviderField::SaveProviderButton | ProviderField::AddModelButton
    ) {
        return OnboardingDialogOutcome::None;
    }

    // Up/Down while focused on the new-model input cycles the
    // active provider so the user can target models at a
    // different existing provider.
    if state.provider_field_focus == ProviderField::NewModelInput {
        if let Some(next) = cycle_selected_provider(state, key.code) {
            state.selected_provider = Some(next);
            if let Some(snapshot) = state.selected_provider_snapshot().cloned() {
                state.load_provider_form_from_snapshot(&snapshot);
                state.provider_field_focus = ProviderField::NewModelInput;
            }
            return OnboardingDialogOutcome::None;
        }
    }

    // Text editing for the currently-focused field.
    let field = match state.provider_field_focus {
        ProviderField::Name => Some(&mut state.provider_form.name),
        ProviderField::ApiKey => Some(&mut state.provider_form.api_key),
        ProviderField::BaseUrl => Some(&mut state.provider_form.base_url),
        ProviderField::Model => Some(&mut state.provider_form.model),
        ProviderField::NewModelInput => Some(&mut state.provider_form.new_model),
        _ => None,
    };
    let edited_field = state.provider_field_focus;
    let Some(field) = field else {
        return OnboardingDialogOutcome::None;
    };
    let is_form_field = !matches!(state.provider_field_focus, ProviderField::NewModelInput);
    let mut mutated = true;
    match key.code {
        KeyCode::Home => field.home(),
        KeyCode::End => field.end(),
        KeyCode::Left => field.left(),
        KeyCode::Right => field.right(),
        KeyCode::Backspace => field.backspace(),
        KeyCode::Delete => field.delete(),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => field.clear(),
        KeyCode::Char(ch)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            field.insert(ch);
        }
        _ => mutated = false,
    }
    if mutated {
        // Clear the relevant status banner so stale messages don't
        // confuse the user while they edit.
        if is_form_field {
            state.add_provider_status = PanelStatus::None;
            if matches!(
                edited_field,
                ProviderField::Name | ProviderField::BaseUrl | ProviderField::Model
            ) {
                state.provider_form.clear_preset_coupling();
            }
        } else {
            state.add_model_status = PanelStatus::None;
        }
    }
    OnboardingDialogOutcome::None
}

fn cycle_selected_provider(state: &OnboardingDialogState, code: KeyCode) -> Option<String> {
    if state.provider_snapshot.is_empty() {
        return None;
    }
    let names: Vec<&str> = state
        .provider_snapshot
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    let current = state
        .selected_provider
        .as_deref()
        .and_then(|name| names.iter().position(|n| *n == name))
        .unwrap_or(0);
    let len = names.len();
    let next_idx = match code {
        KeyCode::Up => (current + len - 1) % len,
        KeyCode::Down => (current + 1) % len,
        _ => return None,
    };
    Some(names[next_idx].to_string())
}

fn handle_oauth_key(state: &mut OnboardingDialogState, key: &KeyEvent) -> OnboardingDialogOutcome {
    match state.oauth_view.as_mut() {
        Some(OAuthView::AwaitingPaste { input, error, .. }) => match key.code {
            KeyCode::Enter => {
                let value = input.value.trim().to_string();
                if value.is_empty() {
                    *error = Some("Paste the callback URL first.".into());
                    OnboardingDialogOutcome::None
                } else {
                    OnboardingDialogOutcome::SubmitOpenAIOAuthPaste(value)
                }
            }
            KeyCode::Home => {
                input.home();
                OnboardingDialogOutcome::None
            }
            KeyCode::End => {
                input.end();
                OnboardingDialogOutcome::None
            }
            KeyCode::Left => {
                input.left();
                OnboardingDialogOutcome::None
            }
            KeyCode::Right => {
                input.right();
                OnboardingDialogOutcome::None
            }
            KeyCode::Backspace => {
                input.backspace();
                *error = None;
                OnboardingDialogOutcome::None
            }
            KeyCode::Delete => {
                input.delete();
                *error = None;
                OnboardingDialogOutcome::None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.clear();
                *error = None;
                OnboardingDialogOutcome::None
            }
            KeyCode::Char(ch)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                input.insert(ch);
                *error = None;
                OnboardingDialogOutcome::None
            }
            _ => OnboardingDialogOutcome::None,
        },
        Some(OAuthView::Error { can_retry, .. }) => {
            if key.code == KeyCode::Enter && *can_retry {
                state.oauth_view = None;
                OnboardingDialogOutcome::StartOpenAIOAuth
            } else {
                OnboardingDialogOutcome::None
            }
        }
        Some(OAuthView::StartingBrowser { .. })
        | Some(OAuthView::WaitingCallback { .. })
        | Some(OAuthView::Exchanging)
        | None => OnboardingDialogOutcome::None,
    }
}

/// Clear the OAuth sub-view without emitting an outcome. Runner
/// calls this after it has already torn down the listener /
/// in-flight request in response to
/// [`OnboardingDialogOutcome::CancelOpenAIOAuth`].
fn handle_security_key(
    state: &mut OnboardingDialogState,
    key: &KeyEvent,
) -> OnboardingDialogOutcome {
    if key.code == KeyCode::Enter {
        let transition = state.advance();
        if state.done {
            return OnboardingDialogOutcome::Completed {
                transition: Some(transition),
            };
        }
        if transition.previous_step != transition.next_step {
            return OnboardingDialogOutcome::Advanced(transition);
        }
    }
    OnboardingDialogOutcome::None
}

fn handle_done_key(state: &mut OnboardingDialogState, key: &KeyEvent) -> OnboardingDialogOutcome {
    if key.code != KeyCode::Enter {
        return OnboardingDialogOutcome::None;
    }
    let transition = state.advance();
    OnboardingDialogOutcome::Completed {
        transition: Some(transition),
    }
}

fn handle_migration_key(
    state: &mut OnboardingDialogState,
    key: &KeyEvent,
) -> OnboardingDialogOutcome {
    // After the runner reports a result, Enter advances while `r`
    // returns to the checklist so failed items can be retried.
    if state.migration_status.is_some() {
        if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) {
            state.migration_status = None;
            return OnboardingDialogOutcome::None;
        }
        if key.code == KeyCode::Enter {
            let transition = state.advance();
            if state.done {
                return OnboardingDialogOutcome::Completed {
                    transition: Some(transition),
                };
            }
            if transition.previous_step != transition.next_step {
                return OnboardingDialogOutcome::Advanced(transition);
            }
        }
        return OnboardingDialogOutcome::None;
    }

    let total_rows = ImportCategory::ALL.len() + 1; // +1 = Skip row
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.migration_focus = if state.migration_focus == 0 {
                total_rows - 1
            } else {
                state.migration_focus - 1
            };
            OnboardingDialogOutcome::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.migration_focus = if state.migration_focus + 1 >= total_rows {
                0
            } else {
                state.migration_focus + 1
            };
            OnboardingDialogOutcome::None
        }
        KeyCode::Char(' ') => {
            if state.migration_focus < ImportCategory::ALL.len() {
                // Only allow toggling categories that actually
                // have discovered items.
                let category = ImportCategory::ALL[state.migration_focus];
                if state.migration_snapshot.count(category) > 0 {
                    state.migration_selected[state.migration_focus] =
                        !state.migration_selected[state.migration_focus];
                }
            }
            OnboardingDialogOutcome::None
        }
        KeyCode::Enter => {
            // "Skip" row, or no categories selected: advance
            // without running an import.
            if state.migration_focus == ImportCategory::ALL.len()
                || state.selected_categories().is_empty()
            {
                let transition = state.advance();
                if state.done {
                    return OnboardingDialogOutcome::Completed {
                        transition: Some(transition),
                    };
                }
                if transition.previous_step != transition.next_step {
                    return OnboardingDialogOutcome::Advanced(transition);
                }
                return OnboardingDialogOutcome::None;
            }
            OnboardingDialogOutcome::RunMigration(state.selected_categories())
        }
        _ => OnboardingDialogOutcome::None,
    }
}

// ── Rendering ────────────────────────────────────────────────

pub fn render(state: &OnboardingDialogState, frame: &mut Frame, area: Rect) {
    let ds = get_theme(state.current_theme_name());
    let border = Style::default().fg(parse_theme_color(ds.subtle));
    let title_style = Style::default()
        .fg(parse_theme_color(ds.text))
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(parse_theme_color(ds.inactive));
    let normal = Style::default().fg(parse_theme_color(ds.text));
    let accent = Style::default()
        .fg(parse_theme_color(ds.suggestion))
        .add_modifier(Modifier::BOLD);
    let focused = Style::default()
        .fg(parse_theme_color(ds.suggestion))
        .add_modifier(Modifier::BOLD);
    let success = Style::default().fg(parse_theme_color(ds.success));
    let error = Style::default().fg(parse_theme_color(ds.error));

    frame.render_widget(Clear, area);

    let step_label = state.current_step().map(|s| s.label()).unwrap_or("done");
    let step_num = state.current_step_index + 1;
    let total = state.steps.len();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(
            format!(
                " {} ({step_num}/{total}) — {step_label} ",
                state.dialog_title
            ),
            title_style,
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let welcome_height: u16 = if state.show_welcome { 2 } else { 0 };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(welcome_height),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(inner);

    if state.show_welcome {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Welcome to Rebon, your command-line coding agent",
                    accent,
                )),
                Line::from(""),
            ]),
            sections[0],
        );
    }

    match state.current_step() {
        Some(Step::Theme) => {
            render_theme_step(state, frame, sections[1], normal, dim, focused);
        }
        Some(Step::UiMode) => {
            render_ui_mode_step(state, frame, sections[1], normal, dim, focused);
        }
        Some(Step::Provider) => {
            render_provider_step(
                state,
                frame,
                sections[1],
                normal,
                dim,
                accent,
                focused,
                success,
                error,
            );
        }
        Some(Step::Migration) => {
            render_migration_step(
                state,
                frame,
                sections[1],
                normal,
                dim,
                accent,
                focused,
                success,
                error,
            );
        }
        Some(Step::Security) => {
            render_security_step(frame, sections[1], normal, dim, accent);
        }
        Some(Step::Done) => {
            render_done_step(
                state,
                frame,
                sections[1],
                normal,
                dim,
                accent,
                success,
                error,
            );
        }
        None => {}
    }

    let footer = state.footer_text();
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(footer, dim))),
        sections[2],
    );
}

fn render_theme_step(
    state: &OnboardingDialogState,
    frame: &mut Frame,
    area: Rect,
    normal: Style,
    dim: Style,
    focused: Style,
) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        theme_picker::resolve_title(true),
        normal.add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(theme_picker::SUBTITLE, dim)));
    lines.push(Line::from(""));

    for (idx, (label, _)) in state.theme_options.iter().enumerate() {
        let is_focused = idx == state.theme_focus;
        let prefix = if is_focused { "> " } else { "  " };
        let style = if is_focused { focused } else { normal };
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(format!("{}. {}", idx + 1, label), style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "To change this later, run /theme",
        dim,
    )));

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_ui_mode_step(
    state: &OnboardingDialogState,
    frame: &mut Frame,
    area: Rect,
    normal: Style,
    dim: Style,
    focused: Style,
) {
    let mut lines = vec![
        Line::from(Span::styled(
            "Choose how Rebon uses your terminal",
            normal.add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "You can change this later from /settings.",
            dim,
        )),
        Line::from(""),
    ];

    for (index, mode, label, description) in [
        (
            1,
            UiMode::Screen,
            "Screen",
            "Full-screen interface in the terminal's alternate screen.",
        ),
        (
            2,
            UiMode::Inline,
            "Inline",
            "Keep terminal scrollback visible and render below your prompt.",
        ),
    ] {
        let is_focused = state.ui_mode == mode;
        let style = if is_focused { focused } else { normal };
        let prefix = if is_focused { "> " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(format!("{index}. {label}"), style),
        ]));
        lines.push(Line::from(vec![
            Span::raw("     "),
            Span::styled(description, dim),
        ]));
        lines.push(Line::from(""));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

#[allow(clippy::too_many_arguments)]
fn render_provider_step(
    state: &OnboardingDialogState,
    frame: &mut Frame,
    area: Rect,
    normal: Style,
    dim: Style,
    accent: Style,
    focused: Style,
    success: Style,
    error: Style,
) {
    if let Some(view) = &state.oauth_view {
        render_oauth_view(frame, area, view, normal, dim, accent, focused, error);
        return;
    }
    match state.setup_pane {
        SetupPane::ExistingSetup => {
            render_existing_setup_pane(
                state, frame, area, normal, dim, accent, focused, success, error,
            );
        }
        SetupPane::LoginMethods => {
            render_login_pane(
                state, frame, area, normal, dim, accent, focused, success, error,
            );
        }
        SetupPane::ProviderPanel => {
            render_provider_panel(
                state, frame, area, normal, dim, accent, focused, success, error,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_oauth_view(
    frame: &mut Frame,
    area: Rect,
    view: &OAuthView,
    normal: Style,
    dim: Style,
    accent: Style,
    focused: Style,
    error_style: Style,
) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled("OpenAI account login", accent)));
    lines.push(Line::from(""));

    match view {
        OAuthView::StartingBrowser { authorize_url } => {
            lines.push(Line::from(Span::styled(
                "Opening your default browser…",
                normal,
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "If it doesn't open, copy this URL into any browser:",
                dim,
            )));
            lines.push(Line::from(Span::styled(authorize_url.clone(), normal)));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Esc cancels the login flow.", dim)));
        }
        OAuthView::WaitingCallback { authorize_url } => {
            lines.push(Line::from(Span::styled(
                "Waiting for the browser to redirect back…",
                normal,
            )));
            lines.push(Line::from(Span::styled(
                "(listening on http://localhost:1455/auth/callback)",
                dim,
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Authorization URL (if the browser didn't launch):",
                dim,
            )));
            lines.push(Line::from(Span::styled(authorize_url.clone(), normal)));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Esc cancels. The listener times out after 5 minutes.",
                dim,
            )));
        }
        OAuthView::AwaitingPaste {
            authorize_url,
            input,
            error,
        } => {
            lines.push(Line::from(Span::styled(
                "Automatic callback is unavailable. Paste the full redirect URL from your browser below.",
                normal,
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Authorization URL:", dim)));
            lines.push(Line::from(Span::styled(authorize_url.clone(), normal)));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "After approving, copy the full URL from the browser address bar and paste it below.",
                dim,
            )));
            lines.push(Line::from(""));
            // Input line — always focused while AwaitingPaste.
            let cursor_style = focused.add_modifier(Modifier::REVERSED);
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::styled("  Callback: ", focused));
            if input.value.is_empty() {
                spans.push(Span::styled(" ", cursor_style));
                spans.push(Span::styled(" (paste URL, ?query, or code#state)", dim));
            } else {
                let (before, after) = input.value.split_at(input.cursor);
                spans.push(Span::styled(before.to_string(), normal));
                if let Some(ch) = after.chars().next() {
                    let ch_len = ch.len_utf8();
                    spans.push(Span::styled(ch.to_string(), cursor_style));
                    spans.push(Span::styled(after[ch_len..].to_string(), normal));
                } else {
                    spans.push(Span::styled(" ", cursor_style));
                }
            }
            lines.push(Line::from(spans));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Enter submits · Esc cancels", dim)));
            if let Some(msg) = error {
                lines.push(Line::from(Span::styled(msg.clone(), error_style)));
            }
        }
        OAuthView::Exchanging => {
            lines.push(Line::from(Span::styled(
                "Exchanging authorization code for an access token…",
                normal,
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "(POST https://auth.openai.com/oauth/token)",
                dim,
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "This should take a second or two.",
                dim,
            )));
        }
        OAuthView::Error { message, can_retry } => {
            lines.push(Line::from(Span::styled("Login failed.", error_style)));
            lines.push(Line::from(Span::styled(message.clone(), normal)));
            lines.push(Line::from(""));
            let hint = if *can_retry {
                "Enter retries · Esc goes back"
            } else {
                "Esc goes back"
            };
            lines.push(Line::from(Span::styled(hint, dim)));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

#[allow(clippy::too_many_arguments)]
fn render_existing_setup_pane(
    state: &OnboardingDialogState,
    frame: &mut Frame,
    area: Rect,
    normal: Style,
    dim: Style,
    accent: Style,
    focused: Style,
    success: Style,
    error: Style,
) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Existing provider setup found",
        accent.add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "Your current configuration will be kept unless you explicitly reopen setup.",
        normal,
    )));
    lines.push(Line::from(""));

    if state.provider_setup_status.onboarding_completed {
        lines.push(Line::from(Span::styled(
            "Onboarding was completed previously.",
            success,
        )));
    }
    if state.provider_setup_status.providers.is_empty() {
        if state.provider_setup_status.environment_providers.is_empty() {
            lines.push(Line::from(Span::styled(
                "No usable provider is currently visible; preserving still leaves the config untouched.",
                dim,
            )));
        }
    } else {
        lines.push(Line::from(Span::styled("Saved providers:", normal)));
        for provider in state.provider_setup_status.providers.iter().take(4) {
            let marker = if provider.active { "*" } else { " " };
            let model = if provider.model.trim().is_empty() {
                "no model set".to_string()
            } else {
                provider.model.clone()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {marker} "), accent),
                Span::styled(provider.name.clone(), normal.add_modifier(Modifier::BOLD)),
                Span::raw(" — "),
                Span::styled(model, dim),
            ]));
        }
        if state.provider_setup_status.providers.len() > 4 {
            lines.push(Line::from(Span::styled(
                format!(
                    "  … and {} more saved provider(s)",
                    state.provider_setup_status.providers.len() - 4
                ),
                dim,
            )));
        }
    }
    if !state.provider_setup_status.environment_providers.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "Environment credentials: {}",
                state
                    .provider_setup_status
                    .environment_providers
                    .iter()
                    .map(|provider| format!("{} ({})", provider.provider, provider.variable))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            dim,
        )));
    }
    if state.provider_setup_status.config_error.is_some() {
        lines.push(Line::from(Span::styled(
            "An existing config file could not be read. Preserve avoids rewriting it.",
            error,
        )));
    }

    lines.push(Line::from(""));
    for (choice, label, description) in [
        (
            ExistingSetupChoice::Preserve,
            "Keep current provider configuration",
            "continue without changing providers",
        ),
        (
            ExistingSetupChoice::Reconfigure,
            "Reconfigure or add providers",
            "open login and custom provider setup",
        ),
    ] {
        let selected = state.existing_setup_choice == choice;
        let style = if selected { focused } else { normal };
        lines.push(Line::from(vec![
            Span::styled(if selected { "> " } else { "  " }, style),
            Span::styled(label, style.add_modifier(Modifier::BOLD)),
            Span::raw(" — "),
            Span::styled(description, dim),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if state.provider_ready {
            "Keep current continues safely. Reconfigure requires another explicit selection before any provider write."
        } else {
            "A working provider is still required before continuing; choose reconfigure to repair or add one."
        },
        dim,
    )));
    if let Some(banner) = status_line(&state.add_provider_status, success, error) {
        lines.push(banner);
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_login_pane(
    state: &OnboardingDialogState,
    frame: &mut Frame,
    area: Rect,
    normal: Style,
    dim: Style,
    accent: Style,
    focused: Style,
    success: Style,
    error: Style,
) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled("Authentication", accent)));
    lines.push(Line::from(Span::styled("Select a login method:", normal)));
    lines.push(Line::from(""));

    let focused_row = login_row_for_focus(&state.login_options, state.login_focus);
    for row in grouped_login_pane_rows(&state.login_options) {
        match row {
            LoginPaneRow::Header("") => lines.push(Line::from("")),
            LoginPaneRow::Header(label) => {
                lines.push(Line::from(Span::styled(
                    label,
                    accent.add_modifier(Modifier::BOLD),
                )));
            }
            LoginPaneRow::Option(idx) => {
                let option = &state.login_options[idx];
                let is_focused = focused_row == LoginPaneRow::Option(idx);
                let prefix = if is_focused { "> " } else { "  " };
                let style = if is_focused { focused } else { normal };
                lines.push(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(option.primary_label, style.add_modifier(Modifier::BOLD)),
                    Span::raw(" — "),
                    Span::styled(option.secondary_label, dim),
                ]));
            }
            LoginPaneRow::CustomProviderCta => {
                let is_cta_focused = focused_row == LoginPaneRow::CustomProviderCta;
                let prefix = if is_cta_focused { "> " } else { "  " };
                let cta_style = if is_cta_focused { focused } else { normal };
                lines.push(Line::from(vec![
                    Span::styled(prefix, cta_style),
                    Span::styled(
                        "Add custom provider",
                        cta_style.add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" — "),
                    Span::styled("configure an API endpoint + models", dim),
                ]));
            }
        }
    }

    lines.push(Line::from(""));
    let hint = if state.provider_ready {
        "Provider ready · Right continues · Enter reconnects OpenAI or opens custom setup."
    } else {
        "Enter connects OpenAI; \"Add custom provider\" opens the protected setup form."
    };
    lines.push(Line::from(Span::styled(hint, dim)));
    if let Some(banner) = status_line(&state.add_provider_status, success, error) {
        lines.push(banner);
    }

    frame.render_widget(Paragraph::new(lines), area);
}

#[allow(clippy::too_many_arguments)]
fn render_provider_panel(
    state: &OnboardingDialogState,
    frame: &mut Frame,
    area: Rect,
    normal: Style,
    dim: Style,
    accent: Style,
    focused: Style,
    success: Style,
    error: Style,
) {
    if area.width < 68 {
        if state.provider_field_focus == ProviderField::ProviderList {
            render_provider_list_pane(state, frame, area, normal, dim, accent, focused);
        } else {
            render_provider_detail_pane(
                state, frame, area, normal, dim, accent, focused, success, error,
            );
        }
        return;
    }

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(32)])
        .split(area);
    render_provider_list_pane(state, frame, panes[0], normal, dim, accent, focused);
    render_provider_detail_pane(
        state, frame, panes[1], normal, dim, accent, focused, success, error,
    );
}

fn render_provider_list_pane(
    state: &OnboardingDialogState,
    frame: &mut Frame,
    area: Rect,
    normal: Style,
    dim: Style,
    accent: Style,
    focused: Style,
) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled("Providers", accent)));
    lines.push(Line::from(Span::styled("Up/Down switches selection", dim)));
    lines.push(Line::from(""));

    let list_focused = state.provider_field_focus == ProviderField::ProviderList;
    for provider in &state.provider_snapshot {
        let is_selected = state.selected_provider.as_deref() == Some(provider.name.as_str());
        let row_style = if list_focused && is_selected {
            focused
        } else if is_selected {
            normal.add_modifier(Modifier::BOLD)
        } else {
            normal
        };
        let prefix = if list_focused && is_selected {
            "> "
        } else {
            "  "
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, row_style),
            Span::styled(
                provider.name.clone(),
                row_style.add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("    ", dim),
            Span::styled(provider.active_model.clone(), dim),
        ]));
    }

    if state.provider_snapshot.is_empty() {
        lines.push(Line::from(Span::styled("  No providers yet", dim)));
    }

    lines.push(Line::from(""));
    let add_selected = state.selected_provider.is_none();
    let add_style = if list_focused && add_selected {
        focused
    } else if add_selected {
        normal.add_modifier(Modifier::BOLD)
    } else {
        normal
    };
    let prefix = if list_focused && add_selected {
        "> "
    } else {
        "  "
    };
    lines.push(Line::from(vec![
        Span::styled(prefix, add_style),
        Span::styled("Add new provider", add_style.add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(Span::styled(
        "  Press Enter to edit; Tab moves through fields",
        dim,
    )));

    let scroll = focused_line_scroll_offset(&lines, "> ", area.height);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
}

#[allow(clippy::too_many_arguments)]
fn render_provider_detail_pane(
    state: &OnboardingDialogState,
    frame: &mut Frame,
    area: Rect,
    normal: Style,
    dim: Style,
    accent: Style,
    focused: Style,
    success: Style,
    error: Style,
) {
    let mut lines: Vec<Line> = Vec::new();

    if let Some(snap) = state.selected_provider_snapshot() {
        lines.push(Line::from(Span::styled("Edit provider", accent)));
        lines.push(Line::from(Span::styled(
            "Edit fields below, then save. Use New model to add extra models.",
            dim,
        )));
        lines.push(Line::from(""));
        lines.push(text_field_line(
            state,
            ProviderField::Name,
            &state.provider_form.name,
            "provider name",
            normal,
            dim,
            focused,
        ));
        lines.push(text_field_line(
            state,
            ProviderField::ApiKey,
            &state.provider_form.api_key,
            "$MY_KEY or literal",
            normal,
            dim,
            focused,
        ));
        lines.push(text_field_line(
            state,
            ProviderField::BaseUrl,
            &state.provider_form.base_url,
            "https://…",
            normal,
            dim,
            focused,
        ));
        lines.push(format_field_line(state, normal, dim, focused));
        lines.push(text_field_line(
            state,
            ProviderField::Model,
            &state.provider_form.model,
            "active model name",
            normal,
            dim,
            focused,
        ));
        lines.push(Line::from(""));
        lines.push(button_line(
            state,
            ProviderField::SaveProviderButton,
            normal,
            focused,
        ));
        if let Some(banner) = status_line(&state.add_provider_status, success, error) {
            lines.push(banner);
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Models", accent)));
        if snap.models.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("  • ", dim),
                Span::styled(snap.active_model.clone(), normal),
                Span::styled("  (active)", dim),
            ]));
        } else {
            for model in &snap.models {
                let suffix = if *model == snap.active_model {
                    "  (active)"
                } else {
                    ""
                };
                lines.push(Line::from(vec![
                    Span::styled("  • ", dim),
                    Span::styled(model.clone(), normal),
                    Span::styled(suffix, dim),
                ]));
            }
        }
        lines.push(Line::from(""));
        lines.push(text_field_line(
            state,
            ProviderField::NewModelInput,
            &state.provider_form.new_model,
            "additional model name",
            normal,
            dim,
            focused,
        ));
        lines.push(Line::from(""));
        lines.push(button_line(
            state,
            ProviderField::AddModelButton,
            normal,
            focused,
        ));
        if let Some(banner) = status_line(&state.add_model_status, success, error) {
            lines.push(banner);
        }
    } else {
        lines.push(Line::from(Span::styled("Add provider", accent)));
        lines.push(Line::from(Span::styled(
            "Pick a quick setup or Custom, then edit any field below.",
            normal,
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Quick setup:", normal)));
        lines.extend(preset_lines(state, normal, dim, focused));
        lines.push(Line::from(""));

        lines.push(text_field_line(
            state,
            ProviderField::Name,
            &state.provider_form.name,
            "e.g. deepseek",
            normal,
            dim,
            focused,
        ));
        lines.push(text_field_line(
            state,
            ProviderField::ApiKey,
            &state.provider_form.api_key,
            "$MY_KEY or literal",
            normal,
            dim,
            focused,
        ));
        lines.push(text_field_line(
            state,
            ProviderField::BaseUrl,
            &state.provider_form.base_url,
            "https://…",
            normal,
            dim,
            focused,
        ));
        lines.push(format_field_line(state, normal, dim, focused));
        lines.push(text_field_line(
            state,
            ProviderField::Model,
            &state.provider_form.model,
            "first model name",
            normal,
            dim,
            focused,
        ));
        lines.push(Line::from(""));
        lines.push(button_line(
            state,
            ProviderField::SaveProviderButton,
            normal,
            focused,
        ));
        if let Some(banner) = status_line(&state.add_provider_status, success, error) {
            lines.push(banner);
        }
    }

    let marker = match state.provider_field_focus {
        ProviderField::ProviderList => None,
        ProviderField::Preset => Some("> "),
        field => Some(field.label()),
    };
    let scroll = marker
        .map(|marker| focused_line_scroll_offset(&lines, marker, area.height))
        .unwrap_or(0);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
}

/// The quick-setup rows. The catalogue is longer than a form should
/// be, so only [`rebon_plugin_onboarding::onboarding::PRESET_ROWS_VISIBLE`]
/// rows show at once, kept around the selection; the first and last visible
/// rows say how many more there are in each direction.
fn preset_lines(
    state: &OnboardingDialogState,
    normal: Style,
    dim: Style,
    focused: Style,
) -> Vec<Line<'static>> {
    let is_focused = state.provider_field_focus == ProviderField::Preset;
    let total = state.provider_form.preset_options.len();
    let (start, end) = preset_window(total, state.provider_form.preset_idx);
    state
        .provider_form
        .preset_options
        .iter()
        .enumerate()
        .skip(start)
        .take(end - start)
        .map(|(idx, selection)| {
            let more = if idx == start && start > 0 {
                Some(format!("  ↑ {start} more"))
            } else if idx + 1 == end && end < total {
                Some(format!("  ↓ {} more", total - end))
            } else {
                None
            };
            let is_selected = idx == state.provider_form.preset_idx;
            let row_style = if is_focused && is_selected {
                focused
            } else if is_selected {
                normal.add_modifier(Modifier::BOLD)
            } else {
                normal
            };
            let prefix = if is_focused && is_selected {
                "> "
            } else {
                "  "
            };
            match selection {
                ProviderPresetSelection::Preset(id) => {
                    let preset = crate::rebon_config::provider_preset_by_id(id);
                    let label = preset
                        .map(|preset| preset.display_name)
                        .unwrap_or_else(|| selection.display_name());
                    let description = preset.map(|preset| preset.description).unwrap_or("");
                    let mut spans = vec![
                        Span::styled(prefix, row_style),
                        Span::styled(label.to_string(), row_style.add_modifier(Modifier::BOLD)),
                        Span::raw(" — "),
                        Span::styled(description.to_string(), dim),
                    ];
                    if let Some(more) = more {
                        spans.push(Span::styled(more, dim));
                    }
                    Line::from(spans)
                }
                ProviderPresetSelection::Custom => Line::from(vec![
                    Span::styled(prefix, row_style),
                    Span::styled("Custom", row_style.add_modifier(Modifier::BOLD)),
                    Span::raw(" — "),
                    Span::styled("fill everything manually", dim),
                ]),
            }
        })
        .collect()
}

fn text_field_line(
    state: &OnboardingDialogState,
    field: ProviderField,
    text: &TextField,
    placeholder: &str,
    normal: Style,
    dim: Style,
    focused: Style,
) -> Line<'static> {
    let is_focused = state.provider_field_focus == field;
    let label_style = if is_focused { focused } else { normal };
    let value_style = normal;
    let cursor_style = focused.add_modifier(Modifier::REVERSED);

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(format!("  {}: ", field.label()), label_style));
    if text.value.is_empty() {
        if is_focused {
            spans.push(Span::styled(" ", cursor_style));
            spans.push(Span::styled(format!(" ({})", placeholder), dim));
        } else {
            spans.push(Span::styled(format!("({})", placeholder), dim));
        }
        return Line::from(spans);
    }

    if field == ProviderField::ApiKey {
        if is_focused {
            let before_len = text.value[..text.cursor].chars().count();
            let after_len = text.value[text.cursor..].chars().count();
            spans.push(Span::styled("•".repeat(before_len), value_style));
            if after_len > 0 {
                spans.push(Span::styled("•", cursor_style));
                spans.push(Span::styled("•".repeat(after_len - 1), value_style));
            } else {
                spans.push(Span::styled(" ", cursor_style));
            }
        } else {
            spans.push(Span::styled(
                "•".repeat(text.value.chars().count()),
                value_style,
            ));
        }
        return Line::from(spans);
    }

    if is_focused {
        let (before, after) = text.value.split_at(text.cursor);
        spans.push(Span::styled(before.to_string(), value_style));
        if let Some(ch) = after.chars().next() {
            let ch_len = ch.len_utf8();
            spans.push(Span::styled(ch.to_string(), cursor_style));
            spans.push(Span::styled(after[ch_len..].to_string(), value_style));
        } else {
            spans.push(Span::styled(" ", cursor_style));
        }
    } else {
        spans.push(Span::styled(text.value.clone(), value_style));
    }
    Line::from(spans)
}

fn format_field_line(
    state: &OnboardingDialogState,
    normal: Style,
    dim: Style,
    focused: Style,
) -> Line<'static> {
    let is_focused = state.provider_field_focus == ProviderField::Format;
    let label_style = if is_focused { focused } else { normal };
    let selected = state.provider_form.format();
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        format!("  {}: ", ProviderField::Format.label()),
        label_style,
    ));
    for (idx, fmt) in PROVIDER_FORMATS.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled(" / ", dim));
        }
        let style = if *fmt == selected {
            if is_focused {
                focused.add_modifier(Modifier::REVERSED)
            } else {
                normal.add_modifier(Modifier::BOLD)
            }
        } else {
            dim
        };
        spans.push(Span::styled(fmt.to_string(), style));
    }
    if is_focused {
        spans.push(Span::styled("  (Left/Right to cycle)", dim));
    }
    Line::from(spans)
}

fn button_line(
    state: &OnboardingDialogState,
    field: ProviderField,
    normal: Style,
    focused: Style,
) -> Line<'static> {
    let is_focused = state.provider_field_focus == field;
    let style = if is_focused {
        focused.add_modifier(Modifier::REVERSED)
    } else {
        normal.add_modifier(Modifier::BOLD)
    };
    let prefix = if is_focused { "> " } else { "  " };
    Line::from(vec![
        Span::styled(prefix, style),
        Span::styled(field.label(), style),
    ])
}

#[allow(clippy::too_many_arguments)]
fn render_migration_step(
    state: &OnboardingDialogState,
    frame: &mut Frame,
    area: Rect,
    normal: Style,
    dim: Style,
    accent: Style,
    focused: Style,
    success: Style,
    error: Style,
) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Bring skills & agents with you",
        accent,
    )));
    lines.push(Line::from(Span::styled(
        "Rebon found existing Claude / Codex content in your home directory.",
        normal,
    )));
    lines.push(Line::from(Span::styled(
        "Select the categories to copy into ~/.rebon/{skills,agents}.",
        dim,
    )));
    lines.push(Line::from(""));

    for (idx, category) in ImportCategory::ALL.iter().enumerate() {
        let count = state.migration_snapshot.count(*category);
        let is_focused = state.migration_focus == idx && state.migration_status.is_none();
        let is_selected = state.migration_selected[idx];
        let row_style = if is_focused {
            focused
        } else if count == 0 {
            dim
        } else {
            normal
        };
        let prefix = if is_focused { "> " } else { "  " };
        let checkbox = if count == 0 {
            "[-]"
        } else if is_selected {
            "[x]"
        } else {
            "[ ]"
        };
        let dest = category.dest_label();
        let suffix = if count == 0 {
            "  (nothing found)".to_string()
        } else {
            format!(
                "  ({count} item{plural} → ~/.rebon/{dest})",
                plural = if count == 1 { "" } else { "s" }
            )
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, row_style),
            Span::styled(format!("{checkbox} "), row_style),
            Span::styled(category.label(), row_style.add_modifier(Modifier::BOLD)),
            Span::styled(suffix, dim),
        ]));
    }

    // "Do not import" row — advances without copying anything.
    lines.push(Line::from(""));
    let skip_idx = ImportCategory::ALL.len();
    let is_skip_focused = state.migration_focus == skip_idx && state.migration_status.is_none();
    let skip_style = if is_skip_focused { focused } else { normal };
    let prefix = if is_skip_focused { "> " } else { "  " };
    lines.push(Line::from(vec![
        Span::styled(prefix, skip_style),
        Span::styled("Do not import", skip_style.add_modifier(Modifier::BOLD)),
        Span::raw(" — "),
        Span::styled("leave ~/.rebon unchanged and continue", dim),
    ]));

    lines.push(Line::from(""));

    // Status banner (after user confirms).
    if let Some(result) = &state.migration_status {
        match result {
            Ok(summary) if summary.failed == 0 => {
                let msg = format!(
                    "Import complete: {copied} copied · {skipped} kept existing. \
                     Press Enter to continue.",
                    copied = summary.copied,
                    skipped = summary.skipped_existing,
                );
                lines.push(Line::from(Span::styled(msg, success)));
            }
            Ok(summary) => {
                let msg = format!(
                    "Import partially completed: {copied} copied · {skipped} kept existing · \
                     {failed} failed.",
                    copied = summary.copied,
                    skipped = summary.skipped_existing,
                    failed = summary.failed,
                );
                lines.push(Line::from(Span::styled(msg, error)));
                lines.push(Line::from(Span::styled(
                    "Press R to retry failed items, or Enter to continue.",
                    dim,
                )));
            }
            Err(msg) => {
                lines.push(Line::from(Span::styled(
                    format!("Import failed: {msg}"),
                    error,
                )));
                lines.push(Line::from(Span::styled(
                    "Press R to retry, or Enter to continue without importing.",
                    dim,
                )));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "Space toggles · Enter confirms / skips · Esc closes",
            dim,
        )));
        lines.push(Line::from(Span::styled(
            "Existing destinations are preserved — re-running is safe.",
            dim,
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_security_step(frame: &mut Frame, area: Rect, normal: Style, dim: Style, accent: Style) {
    let lines = vec![
        Line::from(Span::styled("Security notes:", accent)),
        Line::from(""),
        Line::from(vec![
            Span::styled("1. ", normal.add_modifier(Modifier::BOLD)),
            Span::styled(
                "Rebon can make mistakes or take an unintended action",
                normal.add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            "   Review file changes, commands, and generated code before applying them.",
            dim,
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("2. ", normal.add_modifier(Modifier::BOLD)),
            Span::styled(
                "Project files, web pages, and tool output can contain prompt injections",
                normal.add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            "   Verify the source and impact before allowing sensitive file, network, or command access.",
            dim,
        )),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

#[allow(clippy::too_many_arguments)]
fn render_done_step(
    state: &OnboardingDialogState,
    frame: &mut Frame,
    area: Rect,
    normal: Style,
    dim: Style,
    accent: Style,
    success: Style,
    error: Style,
) {
    let theme = state
        .theme_options
        .get(state.theme_focus)
        .map(|(label, _)| label.as_str())
        .unwrap_or("current theme");
    let mut lines = vec![
        Line::from(Span::styled(
            "Setup complete",
            accent.add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Theme: ", normal.add_modifier(Modifier::BOLD)),
            Span::styled(theme.to_string(), normal),
        ]),
    ];

    if state.provider_ready {
        lines.push(Line::from(vec![
            Span::styled("Model access: ", normal.add_modifier(Modifier::BOLD)),
            Span::styled("ready", success),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Model access: ", normal.add_modifier(Modifier::BOLD)),
            Span::styled("not configured", error),
        ]));
        lines.push(Line::from(Span::styled(
            "Run /login or /provider add before sending a prompt.",
            dim,
        )));
    }

    if let Some(result) = &state.migration_status {
        match result {
            Ok(summary) if summary.failed == 0 => lines.push(Line::from(Span::styled(
                format!(
                    "Import: {} copied · {} kept existing",
                    summary.copied, summary.skipped_existing
                ),
                success,
            ))),
            Ok(summary) => lines.push(Line::from(Span::styled(
                format!(
                    "Import: {} copied · {} kept existing · {} failed",
                    summary.copied, summary.skipped_existing, summary.failed
                ),
                error,
            ))),
            Err(message) => lines.push(Line::from(Span::styled(
                format!("Import: failed — {message}"),
                error,
            ))),
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press Enter to start. Run /onboarding any time to review setup.",
        dim,
    )));
    frame.render_widget(Paragraph::new(lines), area);
}

/// Whether a config home that has never finished setup should be shown the
/// wizard.
///
/// Asked of the plugin rather than of `config.json` directly: a user who
/// switched `plugins.onboarding.enabled` off has said they do not want the
/// wizard, and a front end that only read the flag would open it anyway.
pub fn first_run_wizard_due() -> bool {
    rebon_plugin_onboarding::first_run_gate(Some(
        rebon_harness::kernel_bootstrap::process_kernel().context(),
    ))
}

/// Whether there is a wizard to open at all — the plugin is loaded.
pub fn wizard_available() -> bool {
    rebon_plugin_onboarding::wizard_available(Some(
        rebon_harness::kernel_bootstrap::process_kernel().context(),
    ))
}

/// Outcome of the pre-startup onboarding flow driven by
/// [`run_startup_onboarding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupOnboardingOutcome {
    /// User finished onboarding and at least one provider is now
    /// reachable (either newly added during the dialog or already
    /// present via env vars). Caller should continue startup with the
    /// selected TUI mode.
    Completed { ui_mode: UiMode },
    /// User closed the dialog without configuring a provider —
    /// caller should exit with guidance.
    Aborted,
}

/// Drive the onboarding wizard as a blocking pre-startup dialog.
///
/// Owns its own [`crate::tui::terminal::TerminalGuard`] so it can run
/// before [`crate::session::build::build_tui_session`]. The dispatch on
/// `OnboardingDialogOutcome` variants matches the `app.onboarding_dialog`
/// block in `runner/mod.rs`, so the on-disk side-effects
/// (`add_custom_provider`, `save_theme`, `complete_onboarding`) are
/// identical to the in-session `/onboarding` path.
pub fn run_startup_onboarding(
    runtime: tokio::runtime::Handle,
    initial_ui_mode: UiMode,
) -> anyhow::Result<StartupOnboardingOutcome> {
    let mut guard = crate::tui::terminal::TerminalGuard::enter()?;
    let mut state = OnboardingDialogState::open_with_ui_mode(initial_ui_mode);
    let mut provider_added = false;

    // Drain buffered input so the Enter keystroke that ran `rebon`
    // doesn't instantly dismiss the first step. Matches
    // `startup_dialog::run_select_dialog`.
    while event::poll(Duration::from_millis(0))? {
        let _ = event::read()?;
    }

    loop {
        guard.terminal().draw(|frame| {
            render(&state, frame, frame.area());
        })?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        let outcome = match event::read()? {
            Event::Key(key) => handle_key(&mut state, &key),
            Event::Paste(text) => {
                state.handle_paste(&text);
                OnboardingDialogOutcome::None
            }
            _ => continue,
        };

        match outcome {
            OnboardingDialogOutcome::None => {}
            OnboardingDialogOutcome::Close => {
                return Ok(finish_startup_outcome(provider_added, state.ui_mode));
            }
            OnboardingDialogOutcome::Completed { .. } => {
                crate::rebon_config::complete_onboarding();
                return Ok(finish_startup_outcome(provider_added, state.ui_mode));
            }
            OnboardingDialogOutcome::Advanced(_) => {}
            OnboardingDialogOutcome::ThemePreview(_) => {}
            OnboardingDialogOutcome::ThemeSelected { setting, .. } => {
                crate::rebon_config::save_theme(setting.id());
                if state.done {
                    crate::rebon_config::complete_onboarding();
                    return Ok(finish_startup_outcome(provider_added, state.ui_mode));
                }
            }
            OnboardingDialogOutcome::UiModeSelected { mode, .. } => {
                crate::rebon_config::save_ui_mode(&mode.to_string())?;
            }
            OnboardingDialogOutcome::AddProvider {
                preset_id,
                name,
                api_key,
                base_url,
                format,
                model,
            } => {
                let (added, _) = apply_add_provider(
                    &mut state,
                    preset_id.as_deref(),
                    &name,
                    &format,
                    &base_url,
                    &api_key,
                    &model,
                );
                provider_added |= added;
            }
            OnboardingDialogOutcome::UpdateProvider {
                original_name,
                name,
                api_key,
                base_url,
                format,
                model,
            } => {
                let _ = apply_update_provider(
                    &mut state,
                    &original_name,
                    &name,
                    &format,
                    &base_url,
                    &api_key,
                    &model,
                );
            }
            OnboardingDialogOutcome::AddProviderModel {
                provider_name,
                model,
            } => {
                apply_add_provider_model(&mut state, &provider_name, &model);
            }
            OnboardingDialogOutcome::StartOpenAIOAuth => {
                let terminal = guard.terminal();
                let redraw = |s: &OnboardingDialogState| -> std::io::Result<()> {
                    terminal.draw(|frame| {
                        render(s, frame, frame.area());
                    })?;
                    Ok(())
                };
                let outcome =
                    crate::oauth_drive::drive_oauth_flow_blocking(&mut state, &runtime, redraw)?;
                match outcome {
                    crate::oauth_drive::Outcome::Success { .. } => {
                        provider_added = true;
                        if state.done {
                            crate::rebon_config::complete_onboarding();
                            return Ok(finish_startup_outcome(provider_added, state.ui_mode));
                        }
                    }
                    crate::oauth_drive::Outcome::Cancelled
                    | crate::oauth_drive::Outcome::Failed => {
                        // Keep the dialog open — user can pick
                        // another method or Esc out.
                    }
                }
            }
            OnboardingDialogOutcome::SubmitOpenAIOAuthPaste(_)
            | OnboardingDialogOutcome::CancelOpenAIOAuth => {
                // These only bubble out of `handle_key` while the
                // OAuth driver owns the event loop; the main
                // startup loop should never observe them directly.
            }
            OnboardingDialogOutcome::RunMigration(categories) => {
                apply_run_migration(&mut state, &categories);
            }
        }
    }
}

fn finish_startup_outcome(provider_added: bool, ui_mode: UiMode) -> StartupOnboardingOutcome {
    if provider_added || has_any_provider() {
        StartupOnboardingOutcome::Completed { ui_mode }
    } else {
        StartupOnboardingOutcome::Aborted
    }
}

fn status_line(status: &PanelStatus, success: Style, error: Style) -> Option<Line<'static>> {
    if status.is_none() {
        return None;
    }
    match status {
        PanelStatus::None => None,
        PanelStatus::Ok(msg) => Some(Line::from(Span::styled(msg.clone(), success))),
        PanelStatus::Err(msg) => Some(Line::from(Span::styled(msg.clone(), error))),
    }
}

fn focused_line_scroll_offset(lines: &[Line<'_>], marker: &str, height: u16) -> u16 {
    let visible = usize::from(height);
    if visible == 0 {
        return 0;
    }
    let Some(target) = lines.iter().position(|line| {
        line.spans
            .iter()
            .any(|span| span.content.as_ref().contains(marker))
    }) else {
        return 0;
    };
    let desired = target.saturating_add(2).saturating_sub(visible);
    desired
        .min(lines.len().saturating_sub(visible))
        .min(u16::MAX as usize) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn fresh_provider_setup_status() -> crate::rebon_config::ProviderSetupStatus {
        crate::rebon_config::ProviderSetupStatus {
            onboarding_completed: false,
            providers: Vec::new(),
            environment_providers: Vec::new(),
            config_error: None,
        }
    }

    fn existing_provider_setup_status() -> crate::rebon_config::ProviderSetupStatus {
        crate::rebon_config::ProviderSetupStatus {
            onboarding_completed: true,
            providers: vec![crate::rebon_config::ProviderSetupProvider {
                name: "existing".into(),
                model: "existing-model".into(),
                active: true,
                credential: crate::rebon_config::ProviderSetupCredential::Stored,
            }],
            environment_providers: Vec::new(),
            config_error: None,
        }
    }

    /// A wizard opened on fixed facts: no providers on disk, no saved
    /// theme, nothing to import. The theme list is the picker's own, which
    /// is a pure table; the tests never read configuration.
    fn open_inputs(setup_status: crate::rebon_config::ProviderSetupStatus) -> OnboardingOpenInputs {
        OnboardingOpenInputs {
            setup_status,
            providers: Vec::new(),
            theme_options: theme_picker::build_options(false)
                .into_iter()
                .map(|option| (option.label, option.value))
                .collect(),
            saved_theme: None,
            migration: DiscoverySnapshot::default(),
        }
    }

    fn state_on_login_pane(_legacy_seed_mode: bool) -> OnboardingDialogState {
        OnboardingDialogState {
            steps: vec![Step::Provider],
            current_step_index: 0,
            done: false,
            dialog_title: "Setup guide",
            show_welcome: false,
            allow_advance_from_login: true,
            is_theme_only: false,
            ui_mode_applies_immediately: false,
            theme_focus: 0,
            theme_options: Vec::new(),
            ui_mode: UiMode::default(),
            setup_pane: SetupPane::LoginMethods,
            provider_setup_status: fresh_provider_setup_status(),
            existing_setup_choice: ExistingSetupChoice::Preserve,
            login_from_existing_setup: false,
            login_options: actionable_login_options(),
            login_focus: 0,
            provider_form: ProviderFormState::default(),
            provider_form_mode: ProviderFormMode::Add,
            provider_field_focus: ProviderField::Preset,
            provider_snapshot: Vec::new(),
            selected_provider: None,
            provider_ready: false,
            add_provider_status: PanelStatus::None,
            add_model_status: PanelStatus::None,
            oauth_view: None,
            migration_snapshot: DiscoverySnapshot::default(),
            migration_selected: [false; 5],
            migration_focus: 0,
            migration_status: None,
        }
    }

    fn state_on_provider_panel() -> OnboardingDialogState {
        let mut s = state_on_login_pane(false);
        s.setup_pane = SetupPane::ProviderPanel;
        s.provider_field_focus = ProviderField::Preset;
        s
    }

    fn state_on_existing_setup() -> OnboardingDialogState {
        let mut state = state_on_login_pane(false);
        state.steps = vec![Step::Theme, Step::Provider, Step::Security];
        state.current_step_index = 1;
        state.setup_pane = SetupPane::ExistingSetup;
        state.provider_setup_status = existing_provider_setup_status();
        state.existing_setup_choice = ExistingSetupChoice::Preserve;
        state.provider_ready = true;
        state
    }

    fn login_focus_for_value(state: &OnboardingDialogState, value: &str) -> usize {
        grouped_login_pane_rows(&state.login_options)
            .into_iter()
            .filter(|row| !matches!(row, LoginPaneRow::Header(_)))
            .position(|row| match row {
                LoginPaneRow::Option(idx) => state.login_options[idx].value == value,
                LoginPaneRow::CustomProviderCta | LoginPaneRow::Header(_) => false,
            })
            .unwrap()
    }

    #[test]
    fn open_for_command_includes_ui_mode_before_setup() {
        let mut state = OnboardingDialogState::open_for_command_with_status_and_ui_mode(
            open_inputs(fresh_provider_setup_status()),
            UiMode::Inline,
        );
        assert_eq!(state.current_step(), Some(Step::Theme));
        assert_eq!(state.ui_mode, UiMode::Inline);
        assert!(state.ui_mode_change_requires_restart());
        let _ = state.advance();
        assert_eq!(state.current_step(), Some(Step::UiMode));
        let _ = state.advance();
        assert_eq!(state.current_step(), Some(Step::Provider));
        assert_eq!(state.setup_pane, SetupPane::LoginMethods);
    }

    #[test]
    fn dedicated_commands_do_not_include_ui_mode_step() {
        for state in [
            OnboardingDialogState::open_for_login_pane(),
            OnboardingDialogState::open_for_provider_command(),
            OnboardingDialogState::open_for_theme_command(),
            OnboardingDialogState::open_for_migrate_command(),
        ] {
            assert!(!state.steps.contains(&Step::UiMode));
            assert!(state.ui_mode_change_requires_restart());
        }
    }

    #[test]
    fn migrate_command_opens_only_the_import_step() {
        let state = OnboardingDialogState::open_for_migrate_command();

        assert_eq!(state.steps, vec![Step::Migration]);
        assert_eq!(state.current_step(), Some(Step::Migration));
        assert_eq!(state.dialog_title, "Import");
        assert!(!state.show_welcome);
        assert!(state.is_migrate_only());
        assert!(!state.is_theme_only());
        assert_eq!(state.onboarding_source(), "slash_migrate");
    }

    /// `/migrate` and `/theme` are single-step commands. Treating either as a
    /// finished setup run would flip `onboardingCompleted` for a user who never
    /// saw the wizard, permanently suppressing first-run onboarding.
    #[test]
    fn single_step_commands_never_claim_onboarding_completed() {
        assert!(OnboardingDialogState::open_for_migrate_command().skips_completion_flag());
        assert!(OnboardingDialogState::open_for_theme_command().skips_completion_flag());

        for state in [
            OnboardingDialogState::open_for_command_with_ui_mode(UiMode::Screen),
            OnboardingDialogState::open_for_login_pane(),
            OnboardingDialogState::open_for_provider_command(),
        ] {
            assert!(!state.skips_completion_flag());
        }
    }

    /// The wizard hides the import step when nothing is discovered; the
    /// explicit command keeps it, so the user gets an answer instead of a
    /// dialog that opens and closes.
    #[test]
    fn migrate_command_keeps_its_step_with_nothing_to_import() {
        let mut state = OnboardingDialogState::open_for_migrate_command();
        state.migration_snapshot = DiscoverySnapshot::default();
        state.migration_selected = [false; 5];

        assert_eq!(state.current_step(), Some(Step::Migration));

        // Enter with no selection finishes the dialog rather than advancing
        // into wizard steps this dialog does not own.
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert!(matches!(outcome, OnboardingDialogOutcome::Completed { .. }));
        assert!(state.done);
    }

    #[test]
    fn migrate_command_selects_every_discovered_category() {
        let state = OnboardingDialogState::open_for_migrate_command();

        for (idx, category) in ImportCategory::ALL.iter().enumerate() {
            assert_eq!(
                state.migration_selected[idx],
                state.migration_snapshot.count(*category) > 0,
                "selection must mirror discovery for {category:?}"
            );
        }
    }

    #[test]
    fn first_run_ui_mode_can_apply_without_restart() {
        let state = OnboardingDialogState::open_with_setup(
            true,
            open_inputs(fresh_provider_setup_status()),
            false,
            UiMode::Inline,
            true,
        );

        assert_eq!(state.ui_mode, UiMode::Inline);
        assert!(!state.ui_mode_change_requires_restart());
    }

    #[test]
    fn reopened_onboarding_starts_with_existing_setup_confirmation() {
        let mut state = OnboardingDialogState::open_for_command_with_status_and_ui_mode(
            open_inputs(existing_provider_setup_status()),
            UiMode::default(),
        );
        let _ = state.advance();
        assert_eq!(state.current_step(), Some(Step::UiMode));
        let _ = state.advance();

        assert_eq!(state.current_step(), Some(Step::Provider));
        assert_eq!(state.setup_pane, SetupPane::ExistingSetup);
        assert_eq!(state.existing_setup_choice, ExistingSetupChoice::Preserve);
        assert!(state.provider_ready);
    }

    #[test]
    fn existing_setup_preserve_advances_without_provider_mutation_outcome() {
        let mut state = state_on_existing_setup();

        let outcome = handle_key(&mut state, &key(KeyCode::Enter));

        assert_eq!(
            outcome,
            OnboardingDialogOutcome::Advanced(OnboardingStepTransition {
                previous_step: Some("setup"),
                next_step: Some("security"),
            })
        );
        assert_eq!(state.current_step(), Some(Step::Security));
    }

    #[test]
    fn existing_setup_requires_two_explicit_choices_before_oauth() {
        let mut state = state_on_existing_setup();

        assert_eq!(
            handle_key(&mut state, &key(KeyCode::Down)),
            OnboardingDialogOutcome::None
        );
        assert_eq!(
            state.existing_setup_choice,
            ExistingSetupChoice::Reconfigure
        );
        assert_eq!(
            handle_key(&mut state, &key(KeyCode::Enter)),
            OnboardingDialogOutcome::None
        );
        assert_eq!(state.setup_pane, SetupPane::LoginMethods);
        assert!(state.login_from_existing_setup);

        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(outcome, OnboardingDialogOutcome::StartOpenAIOAuth);
    }

    #[test]
    fn completed_onboarding_without_provider_keeps_preserve_blocked() {
        let mut status = fresh_provider_setup_status();
        status.onboarding_completed = true;
        let mut state = OnboardingDialogState::open_for_command_with_status_and_ui_mode(
            open_inputs(status),
            UiMode::default(),
        );
        let _ = state.advance();
        let _ = state.advance();

        let outcome = handle_key(&mut state, &key(KeyCode::Enter));

        assert_eq!(outcome, OnboardingDialogOutcome::None);
        assert_eq!(state.current_step(), Some(Step::Provider));
        assert!(matches!(state.add_provider_status, PanelStatus::Err(_)));
    }

    #[test]
    fn existing_setup_right_uses_the_safe_preserve_default() {
        let mut state = state_on_existing_setup();

        let outcome = handle_key(&mut state, &key(KeyCode::Right));

        assert!(matches!(outcome, OnboardingDialogOutcome::Advanced(_)));
        assert_eq!(state.current_step(), Some(Step::Security));
    }

    #[test]
    fn existing_setup_left_returns_to_the_previous_onboarding_step() {
        let mut state = state_on_existing_setup();

        let outcome = handle_key(&mut state, &key(KeyCode::Left));

        assert_eq!(
            outcome,
            OnboardingDialogOutcome::Advanced(OnboardingStepTransition {
                previous_step: Some("setup"),
                next_step: Some("theme"),
            })
        );
        assert_eq!(state.current_step(), Some(Step::Theme));
    }

    #[test]
    fn esc_from_guarded_login_returns_to_existing_setup() {
        let mut state = state_on_existing_setup();
        let _ = handle_key(&mut state, &key(KeyCode::Down));
        let _ = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(state.setup_pane, SetupPane::LoginMethods);

        let outcome = handle_key(&mut state, &key(KeyCode::Esc));

        assert_eq!(outcome, OnboardingDialogOutcome::None);
        assert_eq!(state.setup_pane, SetupPane::ExistingSetup);
        assert!(!state.login_from_existing_setup);
    }

    #[test]
    fn existing_setup_does_not_accept_provider_form_paste() {
        let mut state = state_on_existing_setup();

        assert!(!state.handle_paste("sk-should-not-be-used"));
        assert!(state.provider_form.api_key.value.is_empty());
    }

    #[test]
    fn existing_setup_renders_saved_and_environment_provider_summaries() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = state_on_existing_setup();
        state
            .provider_setup_status
            .providers
            .push(crate::rebon_config::ProviderSetupProvider {
                name: "secondary".into(),
                model: "secondary-model".into(),
                active: false,
                credential: crate::rebon_config::ProviderSetupCredential::Environment {
                    variable: "SECONDARY_API_KEY".into(),
                    available: true,
                },
            });
        state.provider_setup_status.environment_providers =
            vec![crate::rebon_config::ProviderSetupEnvironment {
                provider: "DeepSeek".into(),
                variable: "DEEPSEEK_API_KEY".into(),
            }];
        let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
        terminal
            .draw(|frame| render(&state, frame, frame.area()))
            .unwrap();
        let rendered = (0..28)
            .map(|y| {
                (0..100)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("Existing provider setup found"),
            "{rendered}"
        );
        assert!(rendered.contains("existing"), "{rendered}");
        assert!(rendered.contains("existing-model"), "{rendered}");
        assert!(rendered.contains("secondary"), "{rendered}");
        assert!(rendered.contains("DEEPSEEK_API_KEY"), "{rendered}");
        assert!(
            rendered.contains("Keep current provider configuration"),
            "{rendered}"
        );
    }

    #[test]
    fn provider_panel_desired_height_sizes_to_content_not_fullscreen() {
        // Default add form: a 6-row window onto the quick-setup catalogue
        // + 5 fields + chrome. The catalogue itself is 16 rows; showing it
        // all would be the balloon this test exists to prevent.
        let add = state_on_provider_panel();
        assert_eq!(
            add.provider_panel_desired_height(),
            21,
            "add form grows only as tall as its content"
        );
        // It must never balloon toward a full-screen host — that was the
        // bug where the inline panel scrolled the conversation away.
        assert!(add.provider_panel_desired_height() < 40);

        // Editing a provider shows the extra models section, so it is
        // taller than the add form but still content-bounded.
        let mut edit = state_on_provider_panel();
        edit.provider_snapshot = vec![ProviderSnapshot {
            name: "deepseek".into(),
            format: "openai".into(),
            base_url: "https://api.deepseek.com".into(),
            api_key: "sk-x".into(),
            active_model: "deepseek-v4-flash".into(),
            models: vec!["deepseek-v4-flash".into(), "deepseek-v4-pro".into()],
        }];
        edit.selected_provider = Some("deepseek".into());
        // 16 fixed + 2 models + 0 banners + 3 chrome = 21.
        assert_eq!(edit.provider_panel_desired_height(), 21);
    }

    /// Render `state` into exactly `height` rows and return the frame as
    /// one string, so a test can prove nothing the pane emits is clipped
    /// at the height the inline host reserves for it.
    fn render_at(state: &OnboardingDialogState, width: u16, height: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render(&state, frame, frame.area()))
            .unwrap();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Inline mode hosts the dedicated command dialogs in the prompt
    /// area. Sending `/login` through the alternate screen instead threw
    /// away the user's scrollback for what is a one-row choice.
    #[test]
    fn dedicated_command_dialogs_are_inline_hosted_but_the_wizard_is_not() {
        assert!(OnboardingDialogState::open_for_login_pane().is_inline_hosted());
        assert!(OnboardingDialogState::open_for_provider_command().is_inline_hosted());
        assert!(
            !OnboardingDialogState::open_for_command_with_ui_mode(UiMode::Inline)
                .is_inline_hosted()
        );
        assert!(!OnboardingDialogState::open_for_theme_command().is_inline_hosted());

        assert!(OnboardingDialogState::open_for_login_pane().is_login_pane());
        assert!(!OnboardingDialogState::open_for_provider_command().is_login_pane());
    }

    #[test]
    fn inline_host_height_fits_the_login_pane_without_clipping() {
        let state = state_on_login_pane(false);
        let height = state.inline_host_desired_height();

        // Content-bounded, not a full-screen takeover.
        assert!((6..24).contains(&height), "height: {height}");
        let rendered = render_at(&state, 100, height);
        assert!(rendered.contains("Select a login method:"), "{rendered}");
        assert!(rendered.contains("Add custom provider"), "{rendered}");
        // The last line the pane emits still lands inside the host.
        assert!(rendered.contains("Enter connects OpenAI"), "{rendered}");
    }

    /// The OAuth sub-views are the tallest thing `/login` shows. Sizing
    /// the inline host from the login pane alone would clip the row that
    /// tells the user how to cancel — or, in the paste fallback, the
    /// input line itself.
    #[test]
    fn inline_host_height_fits_every_oauth_sub_view() {
        let url = "https://auth.openai.com/oauth/authorize?client_id=x";

        let mut waiting = state_on_login_pane(false);
        waiting.report_oauth_prepared(url.into(), true);
        waiting.report_oauth_listening();
        let rendered = render_at(&waiting, 100, waiting.inline_host_desired_height());
        assert!(rendered.contains("Waiting for the browser"), "{rendered}");
        assert!(rendered.contains("times out after 5 minutes"), "{rendered}");

        let mut paste = state_on_login_pane(false);
        paste.report_oauth_prepared(url.into(), false);
        paste.report_oauth_paste_error("bad state".into());
        let rendered = render_at(&paste, 100, paste.inline_host_desired_height());
        assert!(rendered.contains("Callback:"), "{rendered}");
        assert!(rendered.contains("Enter submits"), "{rendered}");
        assert!(rendered.contains("bad state"), "{rendered}");

        let mut exchanging = state_on_login_pane(false);
        exchanging.report_oauth_exchanging();
        let rendered = render_at(&exchanging, 100, exchanging.inline_host_desired_height());
        assert!(rendered.contains("take a second or two"), "{rendered}");

        let mut failed = state_on_login_pane(false);
        failed.report_oauth_error("token exchange failed".into(), true);
        let rendered = render_at(&failed, 100, failed.inline_host_desired_height());
        assert!(rendered.contains("token exchange failed"), "{rendered}");
        assert!(rendered.contains("Enter retries"), "{rendered}");
    }

    /// The provider panel keeps its own content-fitted height when the
    /// login dialog escalates into "Add custom provider".
    #[test]
    fn inline_host_height_follows_the_provider_panel_when_it_opens() {
        let state = state_on_provider_panel();
        assert_eq!(
            state.inline_host_desired_height(),
            state.provider_panel_desired_height()
        );
    }

    #[test]
    fn inline_host_height_fits_the_existing_setup_pane_without_clipping() {
        let state = state_on_existing_setup();

        let rendered = render_at(&state, 110, state.inline_host_desired_height());
        assert!(
            rendered.contains("Existing provider setup found"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Reconfigure or add providers"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Keep current continues safely"),
            "{rendered}"
        );
    }

    #[test]
    fn provider_panel_renders_at_content_height_without_panic() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut s = state_on_provider_panel();
        s.dialog_title = "Provider";
        let h = s.provider_panel_desired_height();
        for width in [40u16, 60, 80] {
            for height in [8u16, h, h + 6] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| render(&s, frame, frame.area()))
                    .unwrap();
            }
        }
    }

    #[test]
    fn login_dialog_openai_starts_oauth_without_seeding_prompt() {
        let mut state = OnboardingDialogState::open_for_login_pane();
        state.login_focus = login_focus_for_value(&state, "openai");

        let outcome = handle_key(&mut state, &key(KeyCode::Enter));

        assert_eq!(outcome, OnboardingDialogOutcome::StartOpenAIOAuth);
    }

    #[test]
    fn unsupported_login_methods_are_not_rendered_or_focusable() {
        let state = OnboardingDialogState::open_for_login_pane();
        let values: Vec<&str> = state
            .login_options
            .iter()
            .map(|option| option.value)
            .collect();

        assert_eq!(values, vec!["openai"]);
    }

    #[test]
    fn provider_dialog_opens_directly_on_provider_panel() {
        let state = OnboardingDialogState::open_for_provider_command();
        assert_eq!(state.current_step(), Some(Step::Provider));
        assert_eq!(state.dialog_title, "Provider");
        assert!(!state.show_welcome);
        assert_eq!(state.setup_pane, SetupPane::ProviderPanel);
        assert_eq!(
            state.provider_field_focus,
            if state.provider_snapshot.is_empty() {
                ProviderField::Preset
            } else {
                ProviderField::ProviderList
            }
        );
        if state.provider_snapshot.is_empty() {
            assert_eq!(state.provider_form.name.value, "deepseek");
            assert_eq!(
                state.provider_form.base_url.value,
                "https://api.deepseek.com"
            );
            assert_eq!(state.provider_form.model.value, "deepseek-flash");
        } else {
            assert!(state.selected_provider.is_some());
        }
    }

    #[test]
    fn onboarding_provider_requires_model_access_before_continuing() {
        let mut state = state_on_login_pane(false);
        state.steps = vec![Step::Theme, Step::Provider, Step::Security, Step::Done];
        state.current_step_index = 1;
        state.provider_ready = false;

        let blocked = handle_key(&mut state, &key(KeyCode::Right));

        assert_eq!(blocked, OnboardingDialogOutcome::None);
        assert_eq!(state.current_step(), Some(Step::Provider));
        assert!(matches!(state.add_provider_status, PanelStatus::Err(_)));

        state.provider_ready = true;
        let advanced = handle_key(&mut state, &key(KeyCode::Right));
        assert_eq!(
            advanced,
            OnboardingDialogOutcome::Advanced(OnboardingStepTransition {
                previous_step: Some("setup"),
                next_step: Some("security"),
            })
        );
        assert_eq!(state.current_step(), Some(Step::Security));
    }

    #[test]
    fn completion_page_is_visible_before_onboarding_closes() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = state_on_login_pane(false);
        state.steps = vec![Step::Security, Step::Done];
        state.current_step_index = 0;
        state.provider_ready = true;

        let advanced = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(
            advanced,
            OnboardingDialogOutcome::Advanced(OnboardingStepTransition {
                previous_step: Some("security"),
                next_step: Some("ready"),
            })
        );
        assert_eq!(state.current_step(), Some(Step::Done));
        assert!(!state.done);

        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal
            .draw(|frame| render(&state, frame, frame.area()))
            .unwrap();
        let rendered = (0..16)
            .map(|y| {
                (0..80)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Setup complete"), "{rendered}");
        assert!(rendered.contains("Model access: ready"), "{rendered}");

        let completed = handle_key(&mut state, &key(KeyCode::Enter));
        assert!(matches!(
            completed,
            OnboardingDialogOutcome::Completed { .. }
        ));
        assert!(state.done);
    }

    #[test]
    fn onboarding_left_right_navigate_steps() {
        let mut state = state_on_login_pane(false);
        state.steps = vec![Step::Theme, Step::Provider, Step::Security];
        state.current_step_index = 1;
        state.provider_ready = true;

        let outcome = handle_key(&mut state, &key(KeyCode::Right));
        assert_eq!(
            outcome,
            OnboardingDialogOutcome::Advanced(OnboardingStepTransition {
                previous_step: Some("setup"),
                next_step: Some("security"),
            })
        );
        assert_eq!(state.current_step(), Some(Step::Security));

        let outcome = handle_key(&mut state, &key(KeyCode::Left));
        assert_eq!(
            outcome,
            OnboardingDialogOutcome::Advanced(OnboardingStepTransition {
                previous_step: Some("security"),
                next_step: Some("setup"),
            })
        );
        assert_eq!(state.current_step(), Some(Step::Provider));
    }

    #[test]
    fn onboarding_left_at_first_step_stays_put() {
        let mut state = state_on_login_pane(false);
        state.steps = vec![Step::Theme, Step::Provider, Step::Security];
        state.current_step_index = 0;

        let outcome = handle_key(&mut state, &key(KeyCode::Left));
        assert_eq!(outcome, OnboardingDialogOutcome::None);
        assert_eq!(state.current_step(), Some(Step::Theme));
    }

    #[test]
    fn onboarding_right_from_theme_selects_theme_and_advances() {
        let mut state = OnboardingDialogState::open_for_command_with_status_and_ui_mode(
            open_inputs(fresh_provider_setup_status()),
            UiMode::default(),
        );
        assert_eq!(state.current_step(), Some(Step::Theme));

        match handle_key(&mut state, &key(KeyCode::Right)) {
            OnboardingDialogOutcome::ThemeSelected { transition, .. } => {
                assert_eq!(
                    transition,
                    OnboardingStepTransition {
                        previous_step: Some("theme"),
                        next_step: Some("ui mode"),
                    }
                );
            }
            other => panic!("expected theme selection, got {other:?}"),
        }
        assert_eq!(state.current_step(), Some(Step::UiMode));
    }

    #[test]
    fn ui_mode_step_selects_and_submits_inline() {
        let mut state = state_on_login_pane(false);
        state.steps = vec![Step::UiMode, Step::Provider];
        state.current_step_index = 0;
        state.ui_mode = UiMode::Screen;

        assert_eq!(
            handle_key(&mut state, &key(KeyCode::Down)),
            OnboardingDialogOutcome::None
        );
        assert_eq!(state.ui_mode, UiMode::Inline);
        assert_eq!(
            handle_key(&mut state, &key(KeyCode::Enter)),
            OnboardingDialogOutcome::UiModeSelected {
                mode: UiMode::Inline,
                transition: OnboardingStepTransition {
                    previous_step: Some("ui mode"),
                    next_step: Some("setup"),
                },
            }
        );
        assert_eq!(state.current_step(), Some(Step::Provider));
    }

    #[test]
    fn ui_mode_step_right_submits_current_selection() {
        let mut state = state_on_login_pane(false);
        state.steps = vec![Step::UiMode, Step::Security];
        state.current_step_index = 0;
        state.ui_mode = UiMode::Screen;

        assert_eq!(
            handle_key(&mut state, &key(KeyCode::Right)),
            OnboardingDialogOutcome::UiModeSelected {
                mode: UiMode::Screen,
                transition: OnboardingStepTransition {
                    previous_step: Some("ui mode"),
                    next_step: Some("security"),
                },
            }
        );
        assert_eq!(state.current_step(), Some(Step::Security));
    }

    #[test]
    fn ui_mode_step_left_returns_to_theme() {
        let mut state = state_on_login_pane(false);
        state.steps = vec![Step::Theme, Step::UiMode, Step::Provider];
        state.current_step_index = 1;

        assert_eq!(
            handle_key(&mut state, &key(KeyCode::Left)),
            OnboardingDialogOutcome::Advanced(OnboardingStepTransition {
                previous_step: Some("ui mode"),
                next_step: Some("theme"),
            })
        );
        assert_eq!(state.current_step(), Some(Step::Theme));
    }

    #[test]
    fn ui_mode_step_renders_both_options_and_current_selection() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = state_on_login_pane(false);
        state.steps = vec![Step::UiMode];
        state.current_step_index = 0;
        state.ui_mode = UiMode::Inline;
        let mut terminal = Terminal::new(TestBackend::new(84, 16)).unwrap();
        terminal
            .draw(|frame| render(&state, frame, frame.area()))
            .unwrap();
        let rendered = (0..16)
            .map(|y| {
                (0..84)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Choose how Rebon uses your terminal"));
        assert!(rendered.contains("1. Screen"));
        assert!(rendered.contains("> 2. Inline"));
        assert!(rendered.contains("/settings"));
    }

    #[test]
    fn onboarding_provider_panel_keeps_local_arrow_behavior() {
        let mut state = state_on_provider_panel();
        state.steps = vec![Step::Theme, Step::Provider, Step::Security];
        state.current_step_index = 1;
        assert_eq!(state.provider_field_focus, ProviderField::Preset);

        let outcome = handle_key(&mut state, &key(KeyCode::Right));
        assert_eq!(outcome, OnboardingDialogOutcome::None);
        assert_eq!(state.current_step(), Some(Step::Provider));
        assert_ne!(
            state.provider_form.selected_preset(),
            ProviderPresetSelection::Preset("deepseek")
        );
    }

    #[test]
    fn cta_opens_provider_panel() {
        let mut state = state_on_login_pane(true);
        // Walk focus past the executable login methods to the CTA.
        for _ in 0..state.login_options.len() {
            let _ = handle_key(&mut state, &key(KeyCode::Down));
        }
        assert_eq!(state.login_focus, state.login_options.len());
        assert_eq!(state.setup_pane, SetupPane::LoginMethods);
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(outcome, OnboardingDialogOutcome::None);
        assert_eq!(state.setup_pane, SetupPane::ProviderPanel);
        assert_eq!(state.provider_field_focus, ProviderField::Preset);
        assert_eq!(state.provider_form.name.value, "deepseek");
        assert_eq!(state.provider_form.api_key.value, "");
        assert_eq!(
            state.provider_form.base_url.value,
            "https://api.deepseek.com"
        );
        assert_eq!(state.provider_form.format(), "openai");
        assert_eq!(state.provider_form.model.value, "deepseek-flash");
    }

    #[test]
    fn esc_on_provider_panel_returns_to_login_pane() {
        let mut state = state_on_provider_panel();
        let outcome = handle_key(&mut state, &key(KeyCode::Esc));
        assert_eq!(outcome, OnboardingDialogOutcome::None);
        assert_eq!(state.setup_pane, SetupPane::LoginMethods);
    }

    #[test]
    fn esc_on_standalone_provider_panel_closes_dialog() {
        let mut state = OnboardingDialogState::open_for_provider_command();
        let outcome = handle_key(&mut state, &key(KeyCode::Esc));
        assert_eq!(outcome, OnboardingDialogOutcome::Close);
        assert_eq!(state.setup_pane, SetupPane::ProviderPanel);
    }

    #[test]
    fn esc_on_login_pane_closes_dialog() {
        let mut state = state_on_login_pane(true);
        let outcome = handle_key(&mut state, &key(KeyCode::Esc));
        assert_eq!(outcome, OnboardingDialogOutcome::Close);
    }

    #[test]
    fn reopened_onboarding_uses_supported_oauth_after_reconfiguration_confirmation() {
        let mut state = state_on_existing_setup();
        let _ = handle_key(&mut state, &key(KeyCode::Down));
        let _ = handle_key(&mut state, &key(KeyCode::Enter));
        state.login_focus = login_focus_for_value(&state, "openai");
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(outcome, OnboardingDialogOutcome::StartOpenAIOAuth);
    }

    #[test]
    fn deepseek_preset_fills_form_and_focuses_api_key() {
        let mut state = state_on_provider_panel();
        assert_eq!(state.provider_field_focus, ProviderField::Preset);
        assert_eq!(state.provider_form.name.value, "deepseek");
        assert_eq!(state.provider_form.api_key.value, "");
        assert_eq!(
            state.provider_form.base_url.value,
            "https://api.deepseek.com"
        );
        assert_eq!(state.provider_form.format(), "openai");
        assert_eq!(state.provider_form.model.value, "deepseek-flash");

        let outcome = handle_key(&mut state, &key(KeyCode::Enter));

        assert_eq!(outcome, OnboardingDialogOutcome::None);
        assert_eq!(state.provider_field_focus, ProviderField::ApiKey);
    }

    fn select_custom_preset(state: &mut OnboardingDialogState) {
        while state.provider_form.selected_preset() != ProviderPresetSelection::Custom {
            let _ = handle_key(state, &key(KeyCode::Right));
        }
    }

    #[test]
    fn custom_preset_clears_form_for_manual_entry() {
        let mut state = state_on_provider_panel();
        select_custom_preset(&mut state);
        assert_eq!(
            state.provider_form.selected_preset(),
            ProviderPresetSelection::Custom
        );
        assert!(state.provider_form.name.value.is_empty());
        assert!(state.provider_form.base_url.value.is_empty());
        assert_eq!(state.provider_form.format(), "openai");
        assert!(state.provider_form.model.value.is_empty());
    }

    #[test]
    fn editing_preset_field_switches_to_custom_but_keeps_values() {
        let mut state = state_on_provider_panel();
        state.provider_field_focus = ProviderField::Model;
        let _ = handle_key(&mut state, &key(KeyCode::Char('2')));
        assert_eq!(
            state.provider_form.selected_preset(),
            ProviderPresetSelection::Custom
        );
        assert_eq!(state.provider_form.model.value, "deepseek-flash2");
        assert_eq!(state.provider_form.name.value, "deepseek");
    }

    #[test]
    fn editing_preset_field_removes_preset_id_from_submit() {
        let mut state = state_on_provider_panel();
        state.provider_form.api_key.value = "$DS".into();
        state.provider_form.model.set("deepseek-v4-pro");
        state.provider_form.clear_preset_coupling();
        state.provider_field_focus = ProviderField::SaveProviderButton;
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        match outcome {
            OnboardingDialogOutcome::AddProvider {
                preset_id, model, ..
            } => {
                assert_eq!(preset_id, None);
                assert_eq!(model, "deepseek-v4-pro");
            }
            other => panic!("expected add provider outcome, got {other:?}"),
        }
    }

    #[test]
    fn add_provider_button_submits_outcome_when_form_valid() {
        let mut state = state_on_provider_panel();
        state.provider_form.api_key.value = "$DS".into();
        state.provider_field_focus = ProviderField::SaveProviderButton;
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(
            outcome,
            OnboardingDialogOutcome::AddProvider {
                preset_id: Some("deepseek".into()),
                name: "deepseek".into(),
                api_key: "$DS".into(),
                base_url: "https://api.deepseek.com".into(),
                format: "openai".into(),
                model: "deepseek-flash".into(),
            }
        );
    }

    #[test]
    fn add_provider_rejects_empty_fields_with_inline_status() {
        let mut state = state_on_provider_panel();
        select_custom_preset(&mut state);
        state.provider_field_focus = ProviderField::SaveProviderButton;
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(outcome, OnboardingDialogOutcome::None);
        assert!(matches!(state.add_provider_status, PanelStatus::Err(_)));
        assert_eq!(state.provider_field_focus, ProviderField::Name);
    }

    #[test]
    fn add_model_button_submits_when_provider_selected() {
        let mut state = state_on_provider_panel();
        state.provider_snapshot = vec![ProviderSnapshot {
            name: "deepseek".into(),
            format: "openai".into(),
            base_url: "https://api.deepseek.com".into(),
            api_key: "$DS".into(),
            active_model: "deepseek-v4-flash".into(),
            models: vec!["deepseek-v4-flash".into()],
        }];
        state.selected_provider = Some("deepseek".into());
        state.provider_form.new_model.value = "deepseek-coder".into();
        state.provider_field_focus = ProviderField::AddModelButton;
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(
            outcome,
            OnboardingDialogOutcome::AddProviderModel {
                provider_name: "deepseek".into(),
                model: "deepseek-coder".into(),
            }
        );
    }

    #[test]
    fn add_model_rejects_empty_model_name() {
        let mut state = state_on_provider_panel();
        state.provider_snapshot = vec![ProviderSnapshot {
            name: "deepseek".into(),
            format: "openai".into(),
            base_url: "https://api.deepseek.com".into(),
            api_key: "$DS".into(),
            active_model: "deepseek-v4-flash".into(),
            models: vec!["deepseek-v4-flash".into()],
        }];
        state.selected_provider = Some("deepseek".into());
        state.provider_field_focus = ProviderField::AddModelButton;
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(outcome, OnboardingDialogOutcome::None);
        assert!(matches!(state.add_model_status, PanelStatus::Err(_)));
    }

    #[test]
    fn report_add_provider_success_selects_new_provider_and_clears_form() {
        let mut state = state_on_provider_panel();
        state.provider_form.clear_preset_coupling();
        state.provider_form.name.value = "new".into();
        state.provider_form.api_key.value = "k".into();
        state.provider_form.base_url.value = "u".into();
        state.provider_form.model.value = "m".into();
        // Inject a snapshot change so we can verify the refresh ran.
        // (The real implementation calls list_custom_providers, but in
        // unit tests we just assert on the effects we can check.)
        let _ = state.report_add_provider_result(load_provider_snapshot(), Ok("new".into()));
        assert!(state.provider_form.name.value.is_empty());
        match &state.add_provider_status {
            PanelStatus::Ok(message) => {
                assert_eq!(
                    message,
                    "Provider \"new\" saved and activated; connection will be checked on first request."
                )
            }
            other => panic!("expected ok status, got {other:?}"),
        }
        assert_eq!(state.provider_field_focus, ProviderField::ProviderList);
    }

    #[test]
    fn report_add_provider_success_mentions_preset() {
        let mut state = state_on_provider_panel();
        let _ = state.report_add_provider_result(load_provider_snapshot(), Ok("deepseek".into()));
        match &state.add_provider_status {
            PanelStatus::Ok(message) => {
                assert_eq!(
                    message,
                    "DeepSeek provider saved and activated; connection will be checked on first request."
                )
            }
            other => panic!("expected ok status, got {other:?}"),
        }
    }

    #[test]
    fn provider_save_advances_wizard_after_persistence_succeeds() {
        let mut state = state_on_provider_panel();
        state.steps = vec![Step::Provider, Step::Security, Step::Done];
        state.current_step_index = 0;

        let transition =
            state.report_add_provider_result(load_provider_snapshot(), Ok("deepseek".into()));

        assert!(state.provider_ready);
        assert_eq!(state.current_step(), Some(Step::Security));
        assert_eq!(
            transition,
            Some(OnboardingStepTransition {
                previous_step: Some("setup"),
                next_step: Some("security"),
            })
        );
    }

    #[test]
    fn report_add_provider_error_keeps_form_values() {
        let mut state = state_on_provider_panel();
        state.provider_form.name.value = "new".into();
        let _ = state
            .report_add_provider_result(load_provider_snapshot(), Err("already exists".into()));
        assert!(matches!(state.add_provider_status, PanelStatus::Err(_)));
        assert_eq!(state.provider_form.name.value, "new");
        assert_eq!(
            state.provider_field_focus,
            ProviderField::SaveProviderButton
        );
    }

    #[test]
    fn provider_list_enter_loads_existing_provider_for_editing() {
        let mut state = state_on_provider_panel();
        state.provider_snapshot = vec![ProviderSnapshot {
            name: "ds".into(),
            format: "openai-responses".into(),
            base_url: "https://api.example.com".into(),
            api_key: "$KEY".into(),
            active_model: "gpt-5.4".into(),
            models: vec!["gpt-5.4".into()],
        }];
        state.selected_provider = Some("ds".into());
        state.provider_field_focus = ProviderField::ProviderList;

        let outcome = handle_key(&mut state, &key(KeyCode::Enter));

        assert_eq!(outcome, OnboardingDialogOutcome::None);
        assert_eq!(state.provider_field_focus, ProviderField::Name);
        assert_eq!(
            state.provider_form_mode,
            ProviderFormMode::Edit {
                original_name: "ds".into()
            }
        );
        assert_eq!(state.provider_form.name.value, "ds");
        assert_eq!(state.provider_form.api_key.value, "$KEY");
        assert_eq!(
            state.provider_form.base_url.value,
            "https://api.example.com"
        );
        assert_eq!(state.provider_form.format(), "openai-responses");
        assert_eq!(state.provider_form.model.value, "gpt-5.4");
    }

    #[test]
    fn existing_provider_save_submits_update_outcome() {
        let mut state = state_on_provider_panel();
        state.provider_snapshot = vec![ProviderSnapshot {
            name: "ds".into(),
            format: "openai".into(),
            base_url: "https://old.example.com".into(),
            api_key: "$OLD".into(),
            active_model: "old-model".into(),
            models: vec!["old-model".into()],
        }];
        state.selected_provider = Some("ds".into());
        state.start_edit_selected_provider();
        state.provider_form.name.set("ds2");
        state.provider_form.api_key.set("$NEW");
        state.provider_form.base_url.set("https://new.example.com");
        state.provider_form.model.set("new-model");
        state.provider_field_focus = ProviderField::SaveProviderButton;

        let outcome = handle_key(&mut state, &key(KeyCode::Enter));

        assert_eq!(
            outcome,
            OnboardingDialogOutcome::UpdateProvider {
                original_name: "ds".into(),
                name: "ds2".into(),
                api_key: "$NEW".into(),
                base_url: "https://new.example.com".into(),
                format: "openai".into(),
                model: "new-model".into(),
            }
        );
    }

    #[test]
    fn report_update_provider_success_keeps_edit_mode_with_new_name() {
        let mut state = state_on_provider_panel();
        state.provider_snapshot = vec![ProviderSnapshot {
            name: "ds2".into(),
            format: "openai".into(),
            base_url: "https://new.example.com".into(),
            api_key: "$NEW".into(),
            active_model: "new-model".into(),
            models: vec!["new-model".into()],
        }];

        let _ = state.report_update_provider_result(load_provider_snapshot(), Ok("ds2".into()));

        assert_eq!(state.selected_provider.as_deref(), Some("ds2"));
        assert_eq!(
            state.provider_form_mode,
            ProviderFormMode::Edit {
                original_name: "ds2".into()
            }
        );
        assert_eq!(state.provider_form.name.value, "ds2");
        assert!(matches!(state.add_provider_status, PanelStatus::Ok(_)));
        assert_eq!(state.provider_field_focus, ProviderField::ProviderList);
    }

    #[test]
    fn report_add_model_success_clears_input() {
        let mut state = state_on_provider_panel();
        state.provider_form.new_model.value = "coder".into();
        state.report_add_model_result(load_provider_snapshot(), Ok(("ds".into(), "coder".into())));
        assert!(matches!(state.add_model_status, PanelStatus::Ok(_)));
        assert!(state.provider_form.new_model.value.is_empty());
    }

    #[test]
    fn tab_cycles_focus_across_form_and_model_fields() {
        let mut state = state_on_provider_panel();
        state.provider_snapshot = vec![ProviderSnapshot {
            name: "ds".into(),
            format: "openai".into(),
            base_url: "https://api.example.com".into(),
            api_key: "$KEY".into(),
            active_model: "m".into(),
            models: vec!["m".into()],
        }];
        state.selected_provider = Some("ds".into());
        state.provider_field_focus = ProviderField::ProviderList;
        let _ = handle_key(&mut state, &key(KeyCode::Tab));
        assert_eq!(state.provider_field_focus, ProviderField::Name);
        for _ in 0..ProviderField::FORM_ORDER.len() {
            let _ = handle_key(&mut state, &key(KeyCode::Tab));
        }
        assert_eq!(state.provider_field_focus, ProviderField::NewModelInput);
        let _ = handle_key(&mut state, &key(KeyCode::Tab));
        assert_eq!(state.provider_field_focus, ProviderField::AddModelButton);
        let _ = handle_key(&mut state, &key(KeyCode::Tab));
        assert_eq!(state.provider_field_focus, ProviderField::ProviderList);

        state.selected_provider = None;
        let _ = handle_key(&mut state, &key(KeyCode::Tab));
        assert_eq!(state.provider_field_focus, ProviderField::Preset);
        for _ in 0..ProviderField::ADD_FORM_ORDER.len() {
            let _ = handle_key(&mut state, &key(KeyCode::Tab));
        }
        assert_eq!(state.provider_field_focus, ProviderField::ProviderList);
    }

    #[test]
    fn format_field_cycles_on_left_right() {
        let mut state = state_on_provider_panel();
        state.provider_field_focus = ProviderField::Format;
        assert_eq!(state.provider_form.format(), "openai");
        let _ = handle_key(&mut state, &key(KeyCode::Right));
        assert_eq!(state.provider_form.format(), "openai-responses");
        let _ = handle_key(&mut state, &key(KeyCode::Right));
        assert_eq!(state.provider_form.format(), "anthropic");
        let _ = handle_key(&mut state, &key(KeyCode::Right));
        assert_eq!(state.provider_form.format(), "openai");
        let _ = handle_key(&mut state, &key(KeyCode::Left));
        assert_eq!(state.provider_form.format(), "anthropic");
    }

    #[test]
    fn typing_into_name_field_updates_value_and_clears_status() {
        let mut state = state_on_provider_panel();
        state.add_provider_status = PanelStatus::Err("old".into());
        state.provider_field_focus = ProviderField::Name;
        state.provider_form.name.clear();
        let _ = handle_key(&mut state, &key(KeyCode::Char('d')));
        let _ = handle_key(&mut state, &key(KeyCode::Char('s')));
        assert_eq!(state.provider_form.name.value, "ds");
        assert_eq!(state.add_provider_status, PanelStatus::None);
    }

    #[test]
    fn provider_paste_trims_line_endings_and_clears_status() {
        let mut state = state_on_provider_panel();
        state.add_provider_status = PanelStatus::Err("old".into());
        state.provider_field_focus = ProviderField::ApiKey;
        state.provider_form.api_key.clear();

        assert!(state.handle_paste("  sk-secret\r\n  "));
        assert_eq!(state.provider_form.api_key.value, "sk-secret");
        assert_eq!(state.add_provider_status, PanelStatus::None);
    }

    #[test]
    fn api_key_render_is_masked_while_underlying_value_is_preserved() {
        let mut state = state_on_provider_panel();
        state.provider_field_focus = ProviderField::ApiKey;
        state.provider_form.api_key.set("sk-super-secret");

        let line = text_field_line(
            &state,
            ProviderField::ApiKey,
            &state.provider_form.api_key,
            "secret",
            Style::default(),
            Style::default(),
            Style::default(),
        );
        let rendered = line.spans.iter().fold(String::new(), |mut text, span| {
            text.push_str(span.content.as_ref());
            text
        });

        assert!(!rendered.contains("sk-super-secret"));
        assert!(rendered.contains('•'));
        assert_eq!(state.provider_form.api_key.value, "sk-super-secret");
    }

    #[test]
    fn oauth_paste_uses_bracketed_paste_body_without_newlines() {
        let mut state = state_in_oauth_view(OAuthView::AwaitingPaste {
            authorize_url: "https://auth.example".into(),
            input: TextField::default(),
            error: Some("old".into()),
        });

        assert!(state.handle_paste("  https://localhost/callback?code=ABC&state=XYZ\r\n"));
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));

        assert_eq!(
            outcome,
            OnboardingDialogOutcome::SubmitOpenAIOAuthPaste(
                "https://localhost/callback?code=ABC&state=XYZ".into()
            )
        );
    }

    #[test]
    fn cycle_selected_provider_with_up_down_in_new_model_input() {
        let mut state = state_on_provider_panel();
        state.provider_snapshot = vec![
            ProviderSnapshot {
                name: "a".into(),
                format: "openai".into(),
                base_url: "https://a.example.com".into(),
                api_key: "$A".into(),
                active_model: "ma".into(),
                models: vec!["ma".into()],
            },
            ProviderSnapshot {
                name: "b".into(),
                format: "openai".into(),
                base_url: "https://b.example.com".into(),
                api_key: "$B".into(),
                active_model: "mb".into(),
                models: vec!["mb".into()],
            },
        ];
        state.selected_provider = Some("a".into());
        state.provider_field_focus = ProviderField::NewModelInput;
        let _ = handle_key(&mut state, &key(KeyCode::Down));
        assert_eq!(state.selected_provider.as_deref(), Some("b"));
        assert_eq!(state.provider_field_focus, ProviderField::NewModelInput);
        assert_eq!(state.provider_form.name.value, "b");
        let _ = handle_key(&mut state, &key(KeyCode::Down));
        assert_eq!(state.selected_provider.as_deref(), Some("a"));
        assert_eq!(state.provider_field_focus, ProviderField::NewModelInput);
        assert_eq!(state.provider_form.name.value, "a");
        let _ = handle_key(&mut state, &key(KeyCode::Up));
        assert_eq!(state.selected_provider.as_deref(), Some("b"));
        assert_eq!(state.provider_field_focus, ProviderField::NewModelInput);
        assert_eq!(state.provider_form.name.value, "b");
    }

    // ── OAuth sub-view coverage matrix ───────────────────────────
    //
    // * `enter_on_openai_option_emits_start_outcome` — the whole
    //   point of the feature. OpenAI row is no longer decorative.
    // * `login_pane_contains_only_executable_methods_plus_custom_setup` —
    //   unsupported decorative login methods never become focusable.
    // * `report_oauth_prepared_routes_on_port_state` — prepared
    //   with port free → StartingBrowser; port busy → AwaitingPaste.
    // * `report_oauth_listening_transitions_from_starting` —
    //   StartingBrowser → WaitingCallback carries the authorize_url.
    // * `report_oauth_paste_required_transitions_mid_flight` — used
    //   when the listener errors out after we already moved past
    //   prepare; must not lose authorize_url.
    // * `report_oauth_exchanging_sets_intermediate_view`.
    // * `report_oauth_success_clears_view_and_advances_wizard`.
    // * `report_oauth_error_enters_error_view`.
    // * `esc_during_oauth_view_emits_cancel_and_clears_view` —
    //   matches the "Esc in OAuth view cancels" user decision.
    // * `paste_view_enter_emits_submit_outcome`.
    // * `paste_view_enter_rejects_empty_input`.
    // * `paste_view_typing_updates_input_and_clears_error`.
    // * `paste_view_report_paste_error_surfaces_inline_without_clearing_input`.
    // * `error_view_enter_triggers_retry_when_retriable`.
    // * `error_view_enter_ignored_when_not_retriable`.

    fn state_in_oauth_view(view: OAuthView) -> OnboardingDialogState {
        let mut s = state_on_login_pane(false);
        s.oauth_view = Some(view);
        s
    }

    #[test]
    fn enter_on_openai_option_emits_start_outcome() {
        let mut state = state_on_login_pane(false);
        state.login_focus = 0;
        assert_eq!(
            login_row_for_focus(&state.login_options, 0),
            LoginPaneRow::Option(0)
        );
        assert_eq!(state.login_options[0].value, "openai");
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(outcome, OnboardingDialogOutcome::StartOpenAIOAuth);
    }

    #[test]
    fn login_pane_contains_only_executable_methods_plus_custom_setup() {
        let state = state_on_login_pane(true);
        let interactive_rows: Vec<LoginPaneRow> = grouped_login_pane_rows(&state.login_options)
            .into_iter()
            .filter(|row| !matches!(row, LoginPaneRow::Header(_)))
            .collect();

        assert_eq!(
            interactive_rows,
            vec![LoginPaneRow::Option(0), LoginPaneRow::CustomProviderCta]
        );
    }

    #[test]
    fn report_oauth_prepared_routes_on_port_state() {
        let mut state = state_on_login_pane(false);
        state.report_oauth_prepared("https://x/".into(), true);
        assert!(matches!(
            state.oauth_view,
            Some(OAuthView::StartingBrowser { .. })
        ));

        let mut state = state_on_login_pane(false);
        state.report_oauth_prepared("https://x/".into(), false);
        assert!(matches!(
            state.oauth_view,
            Some(OAuthView::AwaitingPaste { .. })
        ));
    }

    #[test]
    fn report_oauth_listening_transitions_from_starting() {
        let mut state = state_in_oauth_view(OAuthView::StartingBrowser {
            authorize_url: "https://x/".into(),
        });
        state.report_oauth_listening();
        match state.oauth_view {
            Some(OAuthView::WaitingCallback { authorize_url }) => {
                assert_eq!(authorize_url, "https://x/");
            }
            other => panic!("expected WaitingCallback, got {other:?}"),
        }
    }

    #[test]
    fn report_oauth_paste_required_transitions_mid_flight() {
        let mut state = state_in_oauth_view(OAuthView::WaitingCallback {
            authorize_url: "https://x/".into(),
        });
        state.report_oauth_paste_required();
        match state.oauth_view {
            Some(OAuthView::AwaitingPaste { authorize_url, .. }) => {
                assert_eq!(authorize_url, "https://x/");
            }
            other => panic!("expected AwaitingPaste, got {other:?}"),
        }
    }

    #[test]
    fn report_oauth_exchanging_sets_intermediate_view() {
        let mut state = state_in_oauth_view(OAuthView::WaitingCallback {
            authorize_url: "https://x/".into(),
        });
        state.report_oauth_exchanging();
        assert!(matches!(state.oauth_view, Some(OAuthView::Exchanging)));
    }

    #[test]
    fn report_oauth_success_clears_view_and_advances_wizard() {
        let mut state = OnboardingDialogState {
            steps: vec![Step::Theme, Step::Provider, Step::Security],
            current_step_index: 1,
            done: false,
            dialog_title: "Setup guide",
            show_welcome: false,
            allow_advance_from_login: true,
            is_theme_only: false,
            ui_mode_applies_immediately: false,
            theme_focus: 0,
            theme_options: Vec::new(),
            ui_mode: UiMode::default(),
            setup_pane: SetupPane::LoginMethods,
            provider_setup_status: fresh_provider_setup_status(),
            existing_setup_choice: ExistingSetupChoice::Preserve,
            login_from_existing_setup: false,
            login_options: actionable_login_options(),
            login_focus: 0,
            provider_form: ProviderFormState::default(),
            provider_form_mode: ProviderFormMode::Add,
            provider_field_focus: ProviderField::Preset,
            provider_snapshot: Vec::new(),
            selected_provider: None,
            provider_ready: false,
            add_provider_status: PanelStatus::None,
            add_model_status: PanelStatus::None,
            oauth_view: Some(OAuthView::Exchanging),
            migration_snapshot: DiscoverySnapshot::default(),
            migration_selected: [false; 5],
            migration_focus: 0,
            migration_status: None,
        };
        let transition = state.report_oauth_success(load_provider_snapshot());
        assert!(state.oauth_view.is_none());
        assert_eq!(state.current_step(), Some(Step::Security));
        assert_eq!(
            transition,
            Some(OnboardingStepTransition {
                previous_step: Some("setup"),
                next_step: Some("security"),
            })
        );
        assert!(matches!(state.add_provider_status, PanelStatus::Ok(_)));
        assert!(state.provider_ready);
    }

    #[test]
    fn report_oauth_error_enters_error_view() {
        let mut state = state_in_oauth_view(OAuthView::Exchanging);
        state.report_oauth_error("network down".into(), true);
        match &state.oauth_view {
            Some(OAuthView::Error { message, can_retry }) => {
                assert_eq!(message, "network down");
                assert!(can_retry);
            }
            other => panic!("expected Error view, got {other:?}"),
        }
    }

    #[test]
    fn esc_during_oauth_view_emits_cancel_and_clears_view() {
        let mut state = state_in_oauth_view(OAuthView::WaitingCallback {
            authorize_url: "https://x/".into(),
        });
        let outcome = handle_key(&mut state, &key(KeyCode::Esc));
        assert_eq!(outcome, OnboardingDialogOutcome::CancelOpenAIOAuth);
        assert!(state.oauth_view.is_none());
    }

    #[test]
    fn paste_view_enter_emits_submit_outcome() {
        let mut state = state_in_oauth_view(OAuthView::AwaitingPaste {
            authorize_url: "https://x/".into(),
            input: TextField {
                value: "code=ABC&state=XYZ".into(),
                cursor: 18,
            },
            error: None,
        });
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(
            outcome,
            OnboardingDialogOutcome::SubmitOpenAIOAuthPaste("code=ABC&state=XYZ".to_string())
        );
    }

    #[test]
    fn paste_view_enter_rejects_empty_input() {
        let mut state = state_in_oauth_view(OAuthView::AwaitingPaste {
            authorize_url: "https://x/".into(),
            input: TextField::default(),
            error: None,
        });
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(outcome, OnboardingDialogOutcome::None);
        match &state.oauth_view {
            Some(OAuthView::AwaitingPaste { error, .. }) => {
                assert!(error.is_some(), "empty paste must set inline error");
            }
            other => panic!("expected AwaitingPaste, got {other:?}"),
        }
    }

    #[test]
    fn paste_view_typing_updates_input_and_clears_error() {
        let mut state = state_in_oauth_view(OAuthView::AwaitingPaste {
            authorize_url: "https://x/".into(),
            input: TextField::default(),
            error: Some("stale".into()),
        });
        let _ = handle_key(&mut state, &key(KeyCode::Char('c')));
        let _ = handle_key(&mut state, &key(KeyCode::Char('o')));
        match &state.oauth_view {
            Some(OAuthView::AwaitingPaste { input, error, .. }) => {
                assert_eq!(input.value, "co");
                assert!(error.is_none(), "typing must clear stale error");
            }
            other => panic!("expected AwaitingPaste, got {other:?}"),
        }
    }

    #[test]
    fn paste_view_report_paste_error_surfaces_inline_without_clearing_input() {
        let mut state = state_in_oauth_view(OAuthView::AwaitingPaste {
            authorize_url: "https://x/".into(),
            input: TextField {
                value: "bad".into(),
                cursor: 3,
            },
            error: None,
        });
        state.report_oauth_paste_error("parse failed".into());
        match &state.oauth_view {
            Some(OAuthView::AwaitingPaste { input, error, .. }) => {
                assert_eq!(input.value, "bad"); // preserved
                assert_eq!(error.as_deref(), Some("parse failed"));
            }
            other => panic!("expected AwaitingPaste, got {other:?}"),
        }
    }

    #[test]
    fn error_view_enter_triggers_retry_when_retriable() {
        let mut state = state_in_oauth_view(OAuthView::Error {
            message: "net".into(),
            can_retry: true,
        });
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(outcome, OnboardingDialogOutcome::StartOpenAIOAuth);
        assert!(state.oauth_view.is_none());
    }

    #[test]
    fn error_view_enter_ignored_when_not_retriable() {
        let mut state = state_in_oauth_view(OAuthView::Error {
            message: "persist failed".into(),
            can_retry: false,
        });
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(outcome, OnboardingDialogOutcome::None);
        assert!(matches!(state.oauth_view, Some(OAuthView::Error { .. })));
    }

    // ── Migration step coverage matrix ───────────────────────────
    //
    // * `state_on_migration_step` — fixture with 2 categories
    //   populated and 3 empty, so toggle + focus tests are meaningful.
    // * `space_toggles_only_non_empty_categories` — empty categories
    //   must stay on `[-]` (not `[x]`) when Space is pressed.
    // * `enter_on_skip_advances_without_outcome` — the Skip row is
    //   a no-op that just advances.
    // * `enter_with_selection_emits_run_migration_with_categories`.
    // * `enter_with_no_selection_advances` — zero selected = skip.
    // * `report_migration_result_shows_banner_and_blocks_toggle`.
    // * `enter_after_result_advances_past_migration_step`.
    // * `esc_on_migration_step_closes_dialog`.

    fn state_on_migration_step() -> OnboardingDialogState {
        let mut snap = DiscoverySnapshot::default();
        snap.claude_skills = vec![rebon_plugin_onboarding::migrate::DiscoveredItem {
            name: "review".into(),
            source: std::path::PathBuf::from("/tmp/review.md"),
        }];
        snap.codex_prompts = vec![rebon_plugin_onboarding::migrate::DiscoveredItem {
            name: "commit".into(),
            source: std::path::PathBuf::from("/tmp/commit.md"),
        }];
        // Default selections mirror open_with_setup logic: non-empty
        // categories start selected, empty categories stay off.
        let mut selected = [false; 5];
        for (idx, c) in ImportCategory::ALL.iter().enumerate() {
            selected[idx] = snap.count(*c) > 0;
        }
        OnboardingDialogState {
            steps: vec![Step::Migration, Step::Security],
            current_step_index: 0,
            done: false,
            dialog_title: "Setup guide",
            show_welcome: false,
            allow_advance_from_login: true,
            is_theme_only: false,
            ui_mode_applies_immediately: false,
            theme_focus: 0,
            theme_options: Vec::new(),
            ui_mode: UiMode::default(),
            setup_pane: SetupPane::LoginMethods,
            provider_setup_status: fresh_provider_setup_status(),
            existing_setup_choice: ExistingSetupChoice::Preserve,
            login_from_existing_setup: false,
            login_options: actionable_login_options(),
            login_focus: 0,
            provider_form: ProviderFormState::default(),
            provider_form_mode: ProviderFormMode::Add,
            provider_field_focus: ProviderField::Preset,
            provider_snapshot: Vec::new(),
            selected_provider: None,
            provider_ready: false,
            add_provider_status: PanelStatus::None,
            add_model_status: PanelStatus::None,
            oauth_view: None,
            migration_snapshot: snap,
            migration_selected: selected,
            migration_focus: 0,
            migration_status: None,
        }
    }

    #[test]
    fn space_toggles_only_non_empty_categories() {
        let mut state = state_on_migration_step();
        // Focus[0] = ClaudeSkills (non-empty, starts selected).
        assert!(state.migration_selected[0]);
        let _ = handle_key(&mut state, &key(KeyCode::Char(' ')));
        assert!(!state.migration_selected[0]);
        let _ = handle_key(&mut state, &key(KeyCode::Char(' ')));
        assert!(state.migration_selected[0]);

        // Move to ClaudeAgents (empty in fixture).
        let _ = handle_key(&mut state, &key(KeyCode::Down));
        assert_eq!(state.migration_focus, 1);
        assert!(!state.migration_selected[1]);
        let _ = handle_key(&mut state, &key(KeyCode::Char(' ')));
        // Empty categories must stay off — user cannot opt in to
        // copying nothing.
        assert!(!state.migration_selected[1]);
    }

    #[test]
    fn enter_on_skip_advances_without_outcome() {
        let mut state = state_on_migration_step();
        // Walk past all category rows to reach the Skip row.
        for _ in 0..ImportCategory::ALL.len() {
            let _ = handle_key(&mut state, &key(KeyCode::Down));
        }
        assert_eq!(state.migration_focus, ImportCategory::ALL.len());
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(
            outcome,
            OnboardingDialogOutcome::Advanced(OnboardingStepTransition {
                previous_step: Some("import"),
                next_step: Some("security"),
            })
        );
        assert_eq!(state.current_step(), Some(Step::Security));
    }

    #[test]
    fn enter_with_selection_emits_run_migration_with_categories() {
        let mut state = state_on_migration_step();
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        match outcome {
            OnboardingDialogOutcome::RunMigration(cats) => {
                assert!(cats.contains(&ImportCategory::ClaudeSkills));
                assert!(cats.contains(&ImportCategory::CodexPrompts));
                assert!(!cats.contains(&ImportCategory::ClaudeAgents));
                assert!(!cats.contains(&ImportCategory::ClaudeCommands));
                assert!(!cats.contains(&ImportCategory::CodexSkills));
            }
            other => panic!("expected RunMigration, got {other:?}"),
        }
    }

    #[test]
    fn enter_with_no_selection_advances() {
        let mut state = state_on_migration_step();
        state.migration_selected = [false; 5];
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(
            outcome,
            OnboardingDialogOutcome::Advanced(OnboardingStepTransition {
                previous_step: Some("import"),
                next_step: Some("security"),
            })
        );
        assert_eq!(state.current_step(), Some(Step::Security));
    }

    #[test]
    fn report_migration_result_shows_banner_and_blocks_toggle() {
        let mut state = state_on_migration_step();
        let summary = ImportSummary {
            copied: 2,
            skipped_existing: 1,
            failed: 0,
        };
        state.report_migration_result(Ok(summary.clone()));
        assert!(matches!(state.migration_status, Some(Ok(_))));
        // After the banner is up, Space should not toggle anything.
        let before = state.migration_selected;
        let _ = handle_key(&mut state, &key(KeyCode::Char(' ')));
        assert_eq!(state.migration_selected, before);
    }

    #[test]
    fn enter_after_result_advances_past_migration_step() {
        let mut state = state_on_migration_step();
        state.report_migration_result(Ok(ImportSummary::default()));
        assert_eq!(state.current_step(), Some(Step::Migration));
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));
        assert_eq!(
            outcome,
            OnboardingDialogOutcome::Advanced(OnboardingStepTransition {
                previous_step: Some("import"),
                next_step: Some("security"),
            })
        );
        assert_eq!(state.current_step(), Some(Step::Security));
        assert!(state.migration_status.is_some());
    }

    #[test]
    fn retry_after_partial_import_returns_to_checklist() {
        let mut state = state_on_migration_step();
        state.report_migration_result(Ok(ImportSummary {
            copied: 1,
            skipped_existing: 0,
            failed: 1,
        }));

        let outcome = handle_key(&mut state, &key(KeyCode::Char('r')));

        assert_eq!(outcome, OnboardingDialogOutcome::None);
        assert!(state.migration_status.is_none());
        assert_eq!(state.current_step(), Some(Step::Migration));
    }

    #[test]
    fn onboarding_hook_non_theme_advance_outcome_carries_previous_and_next_step() {
        let mut state = OnboardingDialogState {
            steps: vec![Step::Theme, Step::Provider, Step::Security],
            current_step_index: 1,
            done: false,
            dialog_title: "Setup guide",
            show_welcome: true,
            allow_advance_from_login: true,
            is_theme_only: false,
            ui_mode_applies_immediately: false,
            theme_focus: 0,
            theme_options: Vec::new(),
            ui_mode: UiMode::default(),
            setup_pane: SetupPane::LoginMethods,
            provider_setup_status: fresh_provider_setup_status(),
            existing_setup_choice: ExistingSetupChoice::Preserve,
            login_from_existing_setup: false,
            login_options: actionable_login_options(),
            login_focus: 0,
            provider_form: ProviderFormState::default(),
            provider_form_mode: ProviderFormMode::Add,
            provider_field_focus: ProviderField::Preset,
            provider_snapshot: Vec::new(),
            selected_provider: None,
            provider_ready: false,
            add_provider_status: PanelStatus::None,
            add_model_status: PanelStatus::None,
            oauth_view: None,
            migration_snapshot: DiscoverySnapshot::default(),
            migration_selected: [false; 5],
            migration_focus: 0,
            migration_status: None,
        };

        state.provider_ready = true;
        let outcome = handle_key(&mut state, &key(KeyCode::Right));

        assert_eq!(
            outcome,
            OnboardingDialogOutcome::Advanced(OnboardingStepTransition {
                previous_step: Some("setup"),
                next_step: Some("security"),
            })
        );
        assert_eq!(state.current_step_label(), Some("security"));
        assert_eq!(state.onboarding_source(), "slash_onboarding");
    }

    #[test]
    fn onboarding_hook_theme_only_completion_still_uses_theme_selected_without_advanced() {
        let mut state = OnboardingDialogState::open_for_theme_command();
        let outcome = handle_key(&mut state, &key(KeyCode::Enter));

        match outcome {
            OnboardingDialogOutcome::ThemeSelected { transition, .. } => {
                assert_eq!(
                    transition,
                    OnboardingStepTransition {
                        previous_step: Some("theme"),
                        next_step: Some("theme"),
                    }
                );
            }
            other => panic!("expected ThemeSelected, got {other:?}"),
        }
        assert!(state.done);
        assert!(state.is_theme_only());
        assert_eq!(state.onboarding_source(), "slash_theme");
    }

    #[test]
    fn esc_on_migration_step_closes_dialog() {
        let mut state = state_on_migration_step();
        let outcome = handle_key(&mut state, &key(KeyCode::Esc));
        assert_eq!(outcome, OnboardingDialogOutcome::Close);
    }
}
