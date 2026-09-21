//! The terminal's half of the OpenAI OAuth flow.
//!
//! The order of the phases, what each result means, and what the wizard is
//! told after each one live in [`rebon_plugin_onboarding::drive`]. What is
//! left here is the only part of it that needs a terminal: reading crossterm
//! events while a phase blocks, and turning them into the three answers the
//! driver asks for — nothing happened, the user gave up, the user submitted a
//! pasted callback.
//!
//! That split is not cosmetic. The key handling reads modifier bits and event
//! kinds (Esc versus anything else, a bracketed paste versus typed
//! characters) that the plugin has no vocabulary for, while the phase order is
//! the same on any front end — the desktop app drives the same three phases
//! from a GPUI event loop.
//!
//! Two callers own a terminal and use this: `run_startup_onboarding` (the
//! blocking pre-session loop) and the in-session runner's `/login` dispatch.
//! Both already run on a thread that may block, which is what lets the driver
//! be synchronous on the outside; only the token-exchange POST is async, and
//! the driver bridges that with the runtime handle passed in.

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

/// Drive the full flow, painting through `redraw` and reading this process's
/// terminal for the user's answers.
pub fn drive_oauth_flow_blocking(
    state: &mut OnboardingDialogState,
    runtime: &Handle,
    redraw: impl FnMut(&OnboardingDialogState) -> std::io::Result<()>,
) -> anyhow::Result<Outcome> {
    let mut host = TerminalOAuthHost { redraw };
    drive::drive_oauth_flow_blocking(state, runtime, &mut host)
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
}
