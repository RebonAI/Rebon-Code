//! The terminal's half of the account logins.
//!
//! The order of the phases, what each result means, and what the wizard is
//! told after each one live in [`rebon_plugin_onboarding::drive`]. What is
//! left here is the only part of it that needs a terminal: reading crossterm
//! events while a phase blocks, and turning them into the three answers the
//! driver asks for — nothing happened, the user gave up, the user submitted a
//! pasted callback — plus putting a device login's code on the clipboard.
//!
//! That split is not cosmetic. The key handling reads modifier bits and event
//! kinds (Esc versus anything else, a bracketed paste versus typed
//! characters) that the plugin has no vocabulary for, while the phase order is
//! the same on any front end — the desktop app drives the same phases from a
//! GPUI event loop.
//!
//! Two callers own a terminal and use this: `run_startup_onboarding` (the
//! blocking pre-session loop) and the in-session runner's `/login` dispatch.
//! Both already run on a thread that may block, which is what lets the driver
//! be synchronous on the outside; the HTTP is async, and the driver bridges
//! it with the runtime handle passed in.

use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use tokio::runtime::Handle;

use rebon_plugin_onboarding::{
    drive::{self, DriveInput, OAuthHost},
    OnboardingDialogOutcome, OnboardingDialogState,
};

pub use drive::Outcome;

/// How often we poll for key events while the driver waits on the background
/// listener thread or on a pasted callback.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Drive the login `login_id` names (an id from
/// `rebon_config::account_login`'s table), painting through `redraw` and
/// reading this process's terminal for the user's answers.
pub fn drive_account_login_blocking(
    state: &mut OnboardingDialogState,
    runtime: &Handle,
    login_id: &str,
    redraw: impl FnMut(&OnboardingDialogState) -> std::io::Result<()>,
) -> anyhow::Result<Outcome> {
    let mut host = TerminalOAuthHost { redraw };
    drive::drive_account_login_blocking(state, runtime, &mut host, login_id)
}

/// The login a start outcome names: the ChatGPT login for
/// [`OnboardingDialogOutcome::StartOpenAIOAuth`], the named one for
/// [`OnboardingDialogOutcome::StartAccountLogin`], `None` for anything else.
pub fn login_to_start(outcome: &OnboardingDialogOutcome) -> Option<&'static str> {
    match outcome {
        OnboardingDialogOutcome::StartOpenAIOAuth => Some(rebon_config::CODEX_LOGIN_ID),
        OnboardingDialogOutcome::StartAccountLogin(id) => Some(id),
        _ => None,
    }
}

struct TerminalOAuthHost<R: FnMut(&OnboardingDialogState) -> std::io::Result<()>> {
    redraw: R,
}

impl<R> OAuthHost for TerminalOAuthHost<R>
where
    R: FnMut(&OnboardingDialogState) -> std::io::Result<()>,
{
    fn redraw(&mut self, state: &OnboardingDialogState) -> std::io::Result<()> {
        (self.redraw)(state)
    }

    fn next_input(&mut self, state: &mut OnboardingDialogState) -> std::io::Result<DriveInput> {
        if !event::poll(TICK_INTERVAL)? {
            return Ok(DriveInput::Idle);
        }
        let key = match event::read()? {
            Event::Paste(text) => {
                // A bracketed paste lands in the wizard's text field; nothing
                // has been submitted yet, so the phase keeps waiting.
                state.handle_paste(&text);
                (self.redraw)(state)?;
                return Ok(DriveInput::Idle);
            }
            Event::Key(key) => key,
            _ => return Ok(DriveInput::Idle),
        };
        if !is_press(&key) {
            return Ok(DriveInput::Idle);
        }
        // Esc is answered here rather than through the wizard because it means
        // the same thing in every sub-view, including the one with no key
        // handling of its own (waiting on the loopback listener).
        if key.code == KeyCode::Esc {
            return Ok(DriveInput::Cancelled);
        }

        let outcome = crate::tui::onboarding_dialog::handle_key(state, &key);
        (self.redraw)(state)?;
        Ok(match outcome {
            OnboardingDialogOutcome::SubmitOpenAIOAuthPaste(text) => DriveInput::Submitted(text),
            OnboardingDialogOutcome::CancelOpenAIOAuth => DriveInput::Cancelled,
            _ => DriveInput::Idle,
        })
    }

    /// The system clipboard, not OSC 52: the code is typed into a browser
    /// on this machine, and a terminal that ignores OSC 52 would leave the
    /// view claiming a copy that never happened.
    fn offer_user_code(&mut self, user_code: &str) -> bool {
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(user_code)) {
            Ok(()) => true,
            Err(err) => {
                tracing::debug!(error = %err, "device login: clipboard unavailable");
                false
            }
        }
    }
}

fn is_press(key: &KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    #[test]
    fn is_press_filters_release_events() {
        let press = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(is_press(&press));
    }

    /// Both start outcomes name their login; nothing else starts one.
    #[test]
    fn each_start_outcome_names_its_login() {
        assert_eq!(
            login_to_start(&OnboardingDialogOutcome::StartOpenAIOAuth),
            Some("openai")
        );
        assert_eq!(
            login_to_start(&OnboardingDialogOutcome::StartAccountLogin("copilot")),
            Some("copilot")
        );
        assert_eq!(
            login_to_start(&OnboardingDialogOutcome::CancelOpenAIOAuth),
            None
        );
        assert_eq!(login_to_start(&OnboardingDialogOutcome::None), None);
    }
}
