//! Terminal lifecycle: raw mode, alt screen, panic-hook restoration,
//! RAII teardown guard.
//!
//! The enter/exit of the alternate screen + raw mode are wrapped in a
//! single helper
//! so any early return, error, or panic still restores the user's
//! shell to its pre-rebon state.
//!
//! The returned [`TerminalGuard`] owns a ratatui [`Terminal`] backed
//! by crossterm. Dropping the guard (normally or via panic) leaves
//! the alternate screen and disables raw mode unconditionally.
//!
//! ## Mouse tracking
//!
//! The TUI can enable DEC 1000 + 1002 + 1006 for in-app text selection when
//! mouse selection is explicitly enabled:
//!
//!   * **DEC 1000** (normal mouse tracking) — reports button
//!     press/release and scroll-wheel events.
//!   * **DEC 1002** (button-event tracking) — additionally reports
//!     mouse motion while a button is held (drag), enabling
//!     click-drag selection.
//!   * **DEC 1006** (SGR extended coordinates) — encodes coordinates
//!     as decimal CSI sequences instead of legacy X10 bytes, so
//!     columns/rows > 223 are representable.
//!
//! **DEC 1003 (all-motion tracking) is deliberately NOT enabled.**
//! 1003 emits a mouse-move event on every cell of cursor motion —
//! even with no button held — which interleaves move events into
//! the crossterm queue during a paste. That causes the paste-burst
//! detector in `runner/paste_burst.rs` (see `PasteBurst`) to abort
//! after a couple of characters. 1002 only fires during button-hold
//! drags, so pastes (no button held) are unaffected.
//!
//! With mouse tracking enabled, terminal-native text selection is
//! intercepted. The TUI implements its own selection system
//! (`rebon_tui::selection`) that tracks selection in screen
//! coordinates with scroll compensation. Mouse tracking is
//! enabled by the default screen lifecycle and disabled for inline
//! mode so terminal-native drag selection remains available there.

use std::fmt;
use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::Once;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{MoveDown, MoveTo, MoveToColumn, MoveUp};
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::crossterm::Command;
use ratatui::crossterm::{execute, queue};
use ratatui::layout::Rect;
use ratatui::Terminal;
use ratatui::Viewport;

use crate::tui::app::PromptCompletionStatus;

// ── Mouse tracking DEC private modes ────────────────────────────
// Enable DEC 1000 + 1002 + 1006 for in-app text selection with
// scroll-wheel support. See the module docstring for the rationale
// on excluding DEC 1003.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableMouseSelection;

impl Command for EnableMouseSelection {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        // DEC 1000: button press/release + wheel
        // DEC 1002: button-event tracking (drag = motion while held)
        // DEC 1006: SGR extended coordinates (decimal, >223 safe)
        write!(f, "\x1b[?1000h\x1b[?1002h\x1b[?1006h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "tried to execute EnableMouseSelection using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableMouseSelection;

impl Command for DisableMouseSelection {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        // Disable in reverse order of enable.
        write!(f, "\x1b[?1006l\x1b[?1002l\x1b[?1000l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "tried to execute DisableMouseSelection using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

// xterm/rxvt mode 1010 makes the next frame's TTY output reveal the live
// inline viewport without keeping scroll-on-output enabled during streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableScrollToBottomOnOutput;

impl Command for EnableScrollToBottomOnOutput {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1010h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "tried to execute EnableScrollToBottomOnOutput using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableScrollToBottomOnOutput;

impl Command for DisableScrollToBottomOnOutput {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1010l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "tried to execute DisableScrollToBottomOnOutput using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

const MAX_TERMINAL_TITLE_CHARS: usize = 120;
const TERMINAL_TITLE_PROGRESS_BAR_WIDTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalTitleCommand(String);

impl Command for TerminalTitleCommand {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        fmt::Write::write_str(f, &self.0)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "tried to execute TerminalTitleCommand using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub struct TerminalTitleManager {
    disabled: bool,
    last_title: Option<String>,
}

impl TerminalTitleManager {
    pub fn new() -> Self {
        Self {
            disabled: terminal_title_disabled(),
            last_title: None,
        }
    }

    pub fn set_title(&mut self, title: &str) {
        let Some(sequence) = self.sequence_for_title(title) else {
            return;
        };
        if let Err(err) = emit_terminal_title_sequence(sequence) {
            tracing::debug!(%err, "rebon-cli: failed to set terminal title");
        }
    }

    pub fn clear(&mut self) {
        if self.disabled || self.last_title.take().is_none() {
            return;
        }
        if let Err(err) = emit_terminal_title_sequence(clear_terminal_title_sequence().to_string())
        {
            tracing::debug!(%err, "rebon-cli: failed to clear terminal title");
        }
    }

    fn sequence_for_title(&mut self, title: &str) -> Option<String> {
        if self.disabled {
            return None;
        }
        let sanitized = sanitize_terminal_title(title)?;
        if self.last_title.as_deref() == Some(sanitized.as_str()) {
            return None;
        }
        let sequence = terminal_title_sequence_from_sanitized(&sanitized);
        self.last_title = Some(sanitized);
        Some(sequence)
    }

    #[cfg(test)]
    fn new_for_test(disabled: bool) -> Self {
        Self {
            disabled,
            last_title: None,
        }
    }
}

pub(crate) fn terminal_title_for_request_state(
    base_title: &str,
    is_loading: bool,
    elapsed_ms: u64,
    awaiting_permission: bool,
    completion_status: Option<PromptCompletionStatus>,
) -> String {
    let Some(base_title) = sanitize_terminal_title(base_title) else {
        return String::new();
    };
    let completed_body = strip_terminal_title_completion_marker(&base_title);
    if !awaiting_permission && !is_loading && completion_status.is_none() {
        return match completed_body {
            Some(body) => completed_terminal_title(body),
            None => base_title,
        };
    }

    let base_title = completed_body.unwrap_or(&base_title);
    let prefix = if awaiting_permission {
        terminal_title_permission_prefix(elapsed_ms)
    } else if is_loading {
        terminal_title_loading_prefix(elapsed_ms)
    } else {
        match completion_status {
            Some(PromptCompletionStatus::Succeeded) => "✓ ".to_string(),
            Some(PromptCompletionStatus::Failed) => "✗ ".to_string(),
            None => unreachable!("idle request state returned above"),
        }
    };
    let available = MAX_TERMINAL_TITLE_CHARS.saturating_sub(prefix.chars().count());
    let base_title = base_title.chars().take(available).collect::<String>();
    format!("{prefix}{base_title}")
}

fn strip_terminal_title_completion_marker(title: &str) -> Option<&str> {
    let rest = title
        .strip_prefix('✓')
        .or_else(|| title.strip_prefix('☑'))
        .or_else(|| title.strip_prefix('✅'))?;
    Some(rest.strip_prefix('\u{fe0f}').unwrap_or(rest).trim_start())
}

fn completed_terminal_title(body: &str) -> String {
    if body.is_empty() {
        "✓".to_string()
    } else {
        format!("✓ {body}")
    }
}

fn terminal_title_loading_prefix(elapsed_ms: u64) -> String {
    terminal_title_progress_prefix(elapsed_ms, '=')
}

fn terminal_title_permission_prefix(elapsed_ms: u64) -> String {
    terminal_title_progress_prefix(elapsed_ms, '?')
}

fn terminal_title_progress_prefix(elapsed_ms: u64, marker: char) -> String {
    let speed = rebon_spinner::glimmer_speed_for_mode(rebon_spinner::SpinnerMode::Responding);
    let frame = if speed == 0 { 0 } else { elapsed_ms / speed };
    let marker_index = (frame as usize) % TERMINAL_TITLE_PROGRESS_BAR_WIDTH;

    let mut prefix = String::with_capacity(TERMINAL_TITLE_PROGRESS_BAR_WIDTH + 3);
    prefix.push('[');
    for idx in 0..TERMINAL_TITLE_PROGRESS_BAR_WIDTH {
        prefix.push(if idx == marker_index { marker } else { ' ' });
    }
    prefix.push_str("] ");
    prefix
}

fn sanitize_terminal_title(title: &str) -> Option<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut chars = trimmed.chars().peekable();
    let mut sanitized = String::new();
    let mut sanitized_chars = 0usize;

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    skip_csi_sequence(&mut chars);
                }
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    chars.next();
                    skip_string_control_sequence(&mut chars);
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }

        match ch {
            '\u{009b}' => {
                skip_csi_sequence(&mut chars);
                continue;
            }
            '\u{0090}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                skip_string_control_sequence(&mut chars);
                continue;
            }
            _ => {}
        }

        if ch.is_control() {
            continue;
        }

        sanitized.push(ch);
        sanitized_chars += 1;
        if sanitized_chars >= MAX_TERMINAL_TITLE_CHARS {
            break;
        }
    }

    let sanitized = sanitized.trim();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized.to_string())
    }
}

#[cfg(test)]
fn terminal_title_sequence(title: &str) -> Option<String> {
    sanitize_terminal_title(title).map(|title| terminal_title_sequence_from_sanitized(&title))
}

fn terminal_title_sequence_from_sanitized(title: &str) -> String {
    format!("\x1b]0;{title}\x07")
}

fn clear_terminal_title_sequence() -> &'static str {
    "\x1b]0;\x07"
}

fn skip_csi_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for ch in chars.by_ref() {
        let code = ch as u32;
        if (0x40..=0x7e).contains(&code) {
            break;
        }
    }
}

fn skip_string_control_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let mut saw_esc = false;
    for ch in chars.by_ref() {
        if saw_esc {
            if ch == '\\' {
                break;
            }
            saw_esc = ch == '\x1b';
            continue;
        }
        if ch == '\x07' || ch == '\u{009c}' {
            break;
        }
        if ch == '\x1b' {
            saw_esc = true;
        }
    }
}

fn terminal_title_disabled() -> bool {
    terminal_title_disabled_with_env(|name| std::env::var(name).ok())
}

fn terminal_title_disabled_with_env(env: impl Fn(&str) -> Option<String>) -> bool {
    env("REBON_DISABLE_TERMINAL_TITLE")
        .as_deref()
        .is_some_and(rebon_types::env::is_env_truthy)
}

fn emit_terminal_title_sequence(sequence: String) -> io::Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, TerminalTitleCommand(sequence))
}

static PANIC_HOOK: Once = Once::new();
static RAW_MODE_ENABLED: AtomicBool = AtomicBool::new(false);
static ALT_SCREEN_OWNERS: AtomicUsize = AtomicUsize::new(0);
static BRACKETED_PASTE_ENABLED: AtomicBool = AtomicBool::new(false);
static MOUSE_SELECTION_ENABLED: AtomicBool = AtomicBool::new(false);
static SCROLL_TO_BOTTOM_ON_OUTPUT_ENABLED: AtomicBool = AtomicBool::new(false);
static KITTY_KEYBOARD_FLAGS_PUSHED: AtomicBool = AtomicBool::new(false);
/// Height of the inline viewport currently rendered, in rows. Zero
/// means the TUI is either not running or is in alt-screen mode (where
/// `LeaveAlternateScreen` restores the prior buffer). Tracking this in
/// a static lets the panic hook clear the inline viewport on its way
/// out, since panics can't reach the `Drop` impl that would otherwise
/// run the cleanup.
static INLINE_VIEWPORT_HEIGHT: AtomicU16 = AtomicU16::new(0);

fn set_inline_viewport_height(height: u16) {
    INLINE_VIEWPORT_HEIGHT.store(height, Ordering::SeqCst);
}

fn take_inline_viewport_height() -> u16 {
    INLINE_VIEWPORT_HEIGHT.swap(0, Ordering::SeqCst)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AltScreenAcquire {
    should_enter: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AltScreenRelease {
    should_leave: bool,
}

fn acquire_alt_screen_owner() -> AltScreenAcquire {
    AltScreenAcquire {
        should_enter: ALT_SCREEN_OWNERS.fetch_add(1, Ordering::SeqCst) == 0,
    }
}

fn rollback_alt_screen_owner_acquire() {
    let _ = release_alt_screen_owner();
}

fn release_alt_screen_owner() -> AltScreenRelease {
    let mut current = ALT_SCREEN_OWNERS.load(Ordering::SeqCst);
    loop {
        if current == 0 {
            return AltScreenRelease {
                should_leave: false,
            };
        }

        match ALT_SCREEN_OWNERS.compare_exchange(
            current,
            current - 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => {
                return AltScreenRelease {
                    should_leave: current == 1,
                };
            }
            Err(actual) => current = actual,
        }
    }
}

fn clear_alt_screen_owners() -> AltScreenRelease {
    AltScreenRelease {
        should_leave: ALT_SCREEN_OWNERS.swap(0, Ordering::SeqCst) > 0,
    }
}

#[cfg(test)]
fn alt_screen_owner_count_for_test() -> usize {
    ALT_SCREEN_OWNERS.load(Ordering::SeqCst)
}

fn mark_raw_mode_enabled() {
    RAW_MODE_ENABLED.store(true, Ordering::SeqCst);
}

fn mark_bracketed_paste_enabled() {
    BRACKETED_PASTE_ENABLED.store(true, Ordering::SeqCst);
}

fn mark_mouse_selection_enabled() {
    MOUSE_SELECTION_ENABLED.store(true, Ordering::SeqCst);
}

fn mark_scroll_to_bottom_on_output_enabled() {
    SCROLL_TO_BOTTOM_ON_OUTPUT_ENABLED.store(true, Ordering::SeqCst);
}

fn mark_kitty_keyboard_flags_pushed() {
    KITTY_KEYBOARD_FLAGS_PUSHED.store(true, Ordering::SeqCst);
}

/// Install a panic hook that leaves the alternate screen and
/// disables raw mode before delegating to the pre-existing hook.
///
/// Uses [`Once`] so repeated `TerminalGuard::enter()` calls in the
/// same process don't stack hook wrappers.
fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut stdout = io::stdout();
            // Pop kitty flags first so the terminal is back in its
            // baseline input mode before we tear down the alt screen.
            if KITTY_KEYBOARD_FLAGS_PUSHED.swap(false, Ordering::SeqCst) {
                let _ = execute!(stdout, PopKeyboardEnhancementFlags);
            }
            if MOUSE_SELECTION_ENABLED.swap(false, Ordering::SeqCst) {
                let _ = execute!(stdout, DisableMouseSelection);
            }
            if SCROLL_TO_BOTTOM_ON_OUTPUT_ENABLED.swap(false, Ordering::SeqCst) {
                let _ = execute!(stdout, DisableScrollToBottomOnOutput);
            }
            if BRACKETED_PASTE_ENABLED.swap(false, Ordering::SeqCst) {
                let _ = execute!(stdout, DisableBracketedPaste);
            }
            if clear_alt_screen_owners().should_leave {
                let _ = execute!(stdout, LeaveAlternateScreen);
            }
            // Inline mode never entered the alternate screen, so the
            // panicked frame's rendered rows are still visible. Clear
            // them relative to the cursor so the user's shell prompt
            // takes over a clean surface rather than landing on top of
            // the broken viewport.
            let inline_height = take_inline_viewport_height();
            if inline_height > 0 {
                let _ = write_inline_viewport_cleanup(&mut stdout, inline_height);
            }
            if RAW_MODE_ENABLED.swap(false, Ordering::SeqCst) {
                let _ = disable_raw_mode();
            }
            original(info);
        }));
    });
}

/// RAII guard around the ratatui terminal backend.
///
/// [`TerminalGuard::enter`] installs the panic hook, enables raw
/// mode, enters the alternate screen, and constructs a
/// `Terminal<CrosstermBackend<Stdout>>`. Dropping the guard reverses
/// these steps so no early-return or `?`-propagation path can leak
/// raw mode into the user's shell.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    title: TerminalTitleManager,
    lifecycle: TerminalLifecycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalUiSurface {
    Screen,
    Inline { height: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalLifecycle {
    pub raw_mode: bool,
    pub alternate_screen: bool,
    pub bracketed_paste: bool,
    pub mouse_selection: bool,
    pub kitty_keyboard: bool,
    pub clear_on_enter: bool,
}

impl TerminalLifecycle {
    pub fn for_surface(surface: TerminalUiSurface) -> Self {
        match surface {
            TerminalUiSurface::Screen => Self {
                raw_mode: true,
                alternate_screen: true,
                bracketed_paste: true,
                mouse_selection: true,
                kitty_keyboard: true,
                clear_on_enter: true,
            },
            TerminalUiSurface::Inline { .. } => Self {
                raw_mode: true,
                alternate_screen: false,
                bracketed_paste: true,
                mouse_selection: false,
                kitty_keyboard: true,
                clear_on_enter: false,
            },
        }
    }
}

impl TerminalGuard {
    /// Enter raw mode + alt screen and return an owning guard.
    pub fn enter() -> anyhow::Result<Self> {
        Self::enter_surface(TerminalUiSurface::Screen)
    }

    /// Enter the terminal lifecycle for the selected UI surface.
    pub fn enter_surface(surface: TerminalUiSurface) -> anyhow::Result<Self> {
        let entered = std::time::Instant::now();
        install_panic_hook();
        let lifecycle = TerminalLifecycle::for_surface(surface);
        if lifecycle.raw_mode {
            enable_raw_mode()?;
            mark_raw_mode_enabled();
        }
        tracing::info!(
            elapsed_ms = entered.elapsed().as_millis() as u64,
            "rebon startup: raw mode enabled"
        );
        let mut stdout = io::stdout();
        if lifecycle.alternate_screen {
            let acquire = acquire_alt_screen_owner();
            if acquire.should_enter {
                if let Err(err) = execute!(stdout, EnterAlternateScreen) {
                    rollback_alt_screen_owner_acquire();
                    return Err(err.into());
                }
            }
        }
        // How wide this terminal draws an ambiguous character decides
        // how every string after this point is measured, so it is settled
        // before anything is drawn — on the alternate screen where there
        // is one, so the probe's two cells never reach the scrollback.
        crate::tui::ambiguous_width::adopt(&mut stdout);
        if lifecycle.bracketed_paste {
            execute!(stdout, EnableBracketedPaste)?;
            mark_bracketed_paste_enabled();
        }
        if lifecycle.mouse_selection {
            execute!(stdout, EnableMouseSelection)?;
            mark_mouse_selection_enabled();
        }
        // Best-effort: push kitty keyboard protocol flags. On Windows
        // crossterm's `PushKeyboardEnhancementFlags` reports
        // `is_ansi_code_supported() = false` and the `execute!` path
        // returns `Unsupported`, so the push silently no-ops on
        // Windows — exactly what we want here. Forcing the ANSI
        // through anyway (a previous attempt) regressed non-bracketed
        // paste on Windows Terminal: with `REPORT_EVENT_TYPES` /
        // disambiguation live, the key-event shape `detect_paste_batch`
        // relies on shifts subtly, pasted newlines started landing as
        // real Submits, and multi-line pastes collapsed to just the
        // trailing line. Leaving the stock command in place keeps
        // Unix terminals on the kitty protocol while preserving the
        // existing Windows paste-burst detector path.
        if lifecycle.kitty_keyboard {
            if let Err(err) = execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            ) {
                tracing::debug!(%err, "kitty keyboard protocol push failed");
            } else {
                mark_kitty_keyboard_flags_pushed();
            }
        }
        tracing::info!(
            elapsed_ms = entered.elapsed().as_millis() as u64,
            "rebon startup: terminal modes set"
        );
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match surface {
            TerminalUiSurface::Screen => Terminal::new(backend)?,
            TerminalUiSurface::Inline { height } => {
                set_inline_viewport_height(height);
                Terminal::with_options(
                    backend,
                    ratatui::TerminalOptions {
                        viewport: Viewport::Inline(height),
                    },
                )?
            }
        };
        // Force-clear the alt screen before the first diff-based draw.
        // Why: ratatui initializes both buffers with `Cell::EMPTY` (a
        // space). Its first-frame diff skips any cell that is a space
        // in both buffers, on the assumption that the terminal display
        // already matches the "front" buffer. But `EnterAlternateScreen`
        // does NOT guarantee the alt screen is blank — Windows conhost
        // and some terminals retain prior screen contents. Without this
        // explicit clear, blank cells in our render (e.g. the right-edge
        // padding past wrapped text) never get written, leaving pre-rebon
        // shell output visible as character residue at those positions.
        if lifecycle.clear_on_enter {
            terminal.clear()?;
        }
        tracing::info!(
            elapsed_ms = entered.elapsed().as_millis() as u64,
            "rebon startup: terminal ready"
        );
        Ok(Self {
            terminal,
            title: TerminalTitleManager::new(),
            lifecycle,
        })
    }

    /// Mutable access to the underlying terminal for draw calls.
    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }

    /// Push a new terminal-window title. Sanitizes ANSI / control
    /// injection and dedupes against the last-emitted value, so this
    /// is cheap to call every frame.
    pub fn set_title(&mut self, title: &str) {
        self.title.set_title(title);
    }

    pub fn begin_inline_input_scroll(&mut self) -> io::Result<bool> {
        if self.lifecycle.alternate_screen
            || SCROLL_TO_BOTTOM_ON_OUTPUT_ENABLED.load(Ordering::SeqCst)
        {
            return Ok(false);
        }
        execute!(self.terminal.backend_mut(), EnableScrollToBottomOnOutput)?;
        mark_scroll_to_bottom_on_output_enabled();
        Ok(true)
    }

    pub fn end_inline_input_scroll(&mut self, active: bool) -> io::Result<()> {
        if active && SCROLL_TO_BOTTOM_ON_OUTPUT_ENABLED.swap(false, Ordering::SeqCst) {
            execute!(self.terminal.backend_mut(), DisableScrollToBottomOnOutput)?;
        }
        Ok(())
    }

    pub fn suspend_for_terminal_child<F, T>(&mut self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce() -> anyhow::Result<T>,
    {
        self.suspend_lifecycle()?;
        let result = f();
        let restore = self.resume_lifecycle();
        match (result, restore) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(err), Ok(())) => Err(err),
            (Ok(_), Err(err)) => Err(err.into()),
            (Err(err), Err(restore_err)) => {
                tracing::warn!(error = %restore_err, "rebon-cli: failed to restore terminal after child process");
                Err(err)
            }
        }
    }

    fn suspend_lifecycle(&mut self) -> io::Result<()> {
        self.terminal.backend_mut().flush()?;
        if self.lifecycle.kitty_keyboard
            && KITTY_KEYBOARD_FLAGS_PUSHED.swap(false, Ordering::SeqCst)
        {
            execute!(self.terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
        }
        if self.lifecycle.mouse_selection && MOUSE_SELECTION_ENABLED.swap(false, Ordering::SeqCst) {
            execute!(self.terminal.backend_mut(), DisableMouseSelection)?;
        }
        if SCROLL_TO_BOTTOM_ON_OUTPUT_ENABLED.swap(false, Ordering::SeqCst) {
            execute!(self.terminal.backend_mut(), DisableScrollToBottomOnOutput)?;
        }
        if self.lifecycle.bracketed_paste && BRACKETED_PASTE_ENABLED.swap(false, Ordering::SeqCst) {
            execute!(self.terminal.backend_mut(), DisableBracketedPaste)?;
        }
        if self.lifecycle.alternate_screen && release_alt_screen_owner().should_leave {
            execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        }
        if self.lifecycle.raw_mode && RAW_MODE_ENABLED.swap(false, Ordering::SeqCst) {
            disable_raw_mode()?;
        }
        Ok(())
    }

    fn resume_lifecycle(&mut self) -> io::Result<()> {
        if self.lifecycle.raw_mode {
            enable_raw_mode()?;
            mark_raw_mode_enabled();
        }
        if self.lifecycle.alternate_screen {
            let acquire = acquire_alt_screen_owner();
            if acquire.should_enter {
                if let Err(err) = execute!(self.terminal.backend_mut(), EnterAlternateScreen) {
                    rollback_alt_screen_owner_acquire();
                    return Err(err);
                }
            }
        }
        if self.lifecycle.bracketed_paste {
            execute!(self.terminal.backend_mut(), EnableBracketedPaste)?;
            mark_bracketed_paste_enabled();
        }
        if self.lifecycle.mouse_selection {
            execute!(self.terminal.backend_mut(), EnableMouseSelection)?;
            mark_mouse_selection_enabled();
        }
        if self.lifecycle.kitty_keyboard {
            if let Err(err) = execute!(
                self.terminal.backend_mut(),
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            ) {
                tracing::debug!(%err, "kitty keyboard protocol push failed");
            } else {
                mark_kitty_keyboard_flags_pushed();
            }
        }
        if self.lifecycle.clear_on_enter {
            self.terminal.clear()?;
        }
        Ok(())
    }

    /// Best-effort inline-mode cleanup before printing normal stdout
    /// after the TUI exits. This intentionally does not enter/leave the
    /// alternate screen or touch mouse state; it only clears the inline
    /// viewport rows that may contain the prompt box/footer so the resume
    /// hint starts on a clean terminal surface.
    pub fn clear_inline_viewport_for_resume(&mut self) -> io::Result<()> {
        if self.lifecycle.alternate_screen {
            return Ok(());
        }
        let terminal_height = self.terminal.size()?.height;
        let area = self.terminal.current_buffer_mut().area;
        write_inline_area_cleanup(self.terminal.backend_mut(), area, terminal_height)?;
        invalidate_terminal_frame_buffers(&mut self.terminal);
        Ok(())
    }
}

fn invalidate_terminal_frame_buffers<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) {
    for cell in terminal.current_buffer_mut().content.iter_mut() {
        cell.set_symbol("\0");
    }
    terminal.swap_buffers();
}

pub(crate) fn write_inline_area_cleanup<W: Write>(
    writer: &mut W,
    area: Rect,
    terminal_height: u16,
) -> io::Result<()> {
    if area.height == 0 || terminal_height == 0 || area.y >= terminal_height {
        return Ok(());
    }

    let visible_height = area.height.min(terminal_height.saturating_sub(area.y));
    if visible_height == 0 {
        return Ok(());
    }

    for row in 0..visible_height {
        queue!(
            writer,
            MoveTo(0, area.y.saturating_add(row)),
            Clear(ClearType::CurrentLine)
        )?;
    }
    queue!(writer, MoveTo(area.x, area.y))?;
    writer.flush()
}

pub(crate) fn write_inline_viewport_cleanup<W: Write>(
    writer: &mut W,
    height: u16,
) -> io::Result<()> {
    write_inline_rows_cleanup(writer, height, height)
}

pub(crate) fn write_inline_rows_cleanup<W: Write>(
    writer: &mut W,
    visible_height: u16,
    rows_to_clear: u16,
) -> io::Result<()> {
    let rows_to_clear = rows_to_clear.min(visible_height);
    if visible_height == 0 || rows_to_clear == 0 {
        return Ok(());
    }

    queue!(writer, MoveToColumn(0))?;
    let rows_above = visible_height.saturating_sub(1);
    if rows_above > 0 {
        queue!(writer, MoveUp(rows_above))?;
    }
    for row in 0..rows_to_clear {
        queue!(writer, Clear(ClearType::CurrentLine))?;
        if row + 1 < rows_to_clear {
            queue!(writer, MoveDown(1))?;
        }
    }
    queue!(writer, MoveToColumn(0))?;
    writer.flush()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalOverlayLifecycle {
    pub raw_mode: bool,
    pub alternate_screen: bool,
    pub bracketed_paste: bool,
    pub mouse_selection: bool,
    pub kitty_keyboard: bool,
    pub clear_on_enter: bool,
}

impl TerminalOverlayLifecycle {
    pub fn alt_screen_overlay() -> Self {
        Self {
            raw_mode: false,
            alternate_screen: true,
            bracketed_paste: false,
            mouse_selection: false,
            kitty_keyboard: false,
            clear_on_enter: true,
        }
    }
}

/// Lightweight alternate-screen overlay for transient inline-mode surfaces.
///
/// Unlike [`TerminalGuard::enter_surface(TerminalUiSurface::Screen)`], this
/// guard only enters/leaves the alternate screen it owns. It deliberately does
/// not touch raw mode, bracketed paste, mouse tracking, or kitty keyboard flags
/// because those modes belong to the parent inline terminal session.
pub struct AltScreenOverlayGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    lifecycle: TerminalOverlayLifecycle,
}

impl AltScreenOverlayGuard {
    pub fn enter() -> anyhow::Result<Self> {
        let lifecycle = TerminalOverlayLifecycle::alt_screen_overlay();
        let mut stdout = io::stdout();
        if lifecycle.alternate_screen {
            let acquire = acquire_alt_screen_owner();
            if acquire.should_enter {
                if let Err(err) = execute!(stdout, EnterAlternateScreen) {
                    rollback_alt_screen_owner_acquire();
                    return Err(err.into());
                }
            }
        }
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        if lifecycle.clear_on_enter {
            terminal.clear()?;
        }
        Ok(Self {
            terminal,
            lifecycle,
        })
    }

    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for AltScreenOverlayGuard {
    fn drop(&mut self) {
        if self.lifecycle.alternate_screen && release_alt_screen_owner().should_leave {
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort teardown; all errors are swallowed because
        // Drop must not panic and we've already committed to exit.
        // Clear our terminal-window title (only fires if we ever set
        // one) so the user's shell sees its original title back.
        self.title.clear();
        // Pop the kitty flags first so the terminal's input mode is
        // restored before we leave the alt screen.
        if self.lifecycle.kitty_keyboard
            && KITTY_KEYBOARD_FLAGS_PUSHED.swap(false, Ordering::SeqCst)
        {
            let _ = execute!(self.terminal.backend_mut(), PopKeyboardEnhancementFlags);
        }
        if self.lifecycle.mouse_selection && MOUSE_SELECTION_ENABLED.swap(false, Ordering::SeqCst) {
            let _ = execute!(self.terminal.backend_mut(), DisableMouseSelection);
        }
        if SCROLL_TO_BOTTOM_ON_OUTPUT_ENABLED.swap(false, Ordering::SeqCst) {
            let _ = execute!(self.terminal.backend_mut(), DisableScrollToBottomOnOutput);
        }
        if self.lifecycle.bracketed_paste && BRACKETED_PASTE_ENABLED.swap(false, Ordering::SeqCst) {
            let _ = execute!(self.terminal.backend_mut(), DisableBracketedPaste);
        }
        if self.lifecycle.alternate_screen && release_alt_screen_owner().should_leave {
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        }
        // Clear the inline-mode height marker on graceful exit. The
        // explicit `clear_inline_viewport_for_resume` call handles the
        // visual cleanup; here we just zero the panic-hook flag so a
        // subsequent panic in unrelated cleanup doesn't double-clear.
        if !self.lifecycle.alternate_screen {
            take_inline_viewport_height();
        }
        if self.lifecycle.raw_mode && RAW_MODE_ENABLED.swap(false, Ordering::SeqCst) {
            let _ = disable_raw_mode();
        }
    }
}

#[cfg(test)]
mod terminal_title_tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn lifecycle_screen_uses_alt_screen_with_mouse_by_default() {
        let lifecycle = TerminalLifecycle::for_surface(TerminalUiSurface::Screen);
        assert!(lifecycle.raw_mode);
        assert!(lifecycle.alternate_screen);
        assert!(lifecycle.mouse_selection);
        assert!(lifecycle.bracketed_paste);
        assert!(lifecycle.clear_on_enter);
    }

    #[test]
    fn lifecycle_inline_does_not_use_alt_screen_or_mouse() {
        let lifecycle = TerminalLifecycle::for_surface(TerminalUiSurface::Inline { height: 12 });
        assert!(lifecycle.raw_mode);
        assert!(!lifecycle.alternate_screen);
        assert!(!lifecycle.mouse_selection);
        assert!(lifecycle.bracketed_paste);
        assert!(!lifecycle.clear_on_enter);
    }

    #[test]
    fn scroll_to_bottom_on_output_commands_use_xterm_private_mode() {
        let mut enabled = String::new();
        EnableScrollToBottomOnOutput
            .write_ansi(&mut enabled)
            .expect("encode enable sequence");
        let mut disabled = String::new();
        DisableScrollToBottomOnOutput
            .write_ansi(&mut disabled)
            .expect("encode disable sequence");

        assert_eq!(enabled, "\x1b[?1010h");
        assert_eq!(disabled, "\x1b[?1010l");
    }

    #[test]
    fn lifecycle_alt_screen_overlay_only_owns_alt_screen() {
        let lifecycle = TerminalOverlayLifecycle::alt_screen_overlay();
        assert!(!lifecycle.raw_mode);
        assert!(lifecycle.alternate_screen);
        assert!(!lifecycle.bracketed_paste);
        assert!(!lifecycle.mouse_selection);
        assert!(!lifecycle.kitty_keyboard);
        assert!(lifecycle.clear_on_enter);
    }

    #[test]
    fn inline_parent_lifecycle_and_overlay_do_not_overlap_input_modes() {
        let parent = TerminalLifecycle::for_surface(TerminalUiSurface::Inline { height: 12 });
        let overlay = TerminalOverlayLifecycle::alt_screen_overlay();

        assert!(parent.raw_mode);
        assert!(parent.bracketed_paste);
        assert!(parent.kitty_keyboard);
        assert!(!parent.alternate_screen);
        assert!(!overlay.raw_mode);
        assert!(!overlay.bracketed_paste);
        assert!(!overlay.mouse_selection);
        assert!(!overlay.kitty_keyboard);
        assert!(overlay.alternate_screen);
    }

    #[test]
    fn alt_screen_owner_counter_tracks_nested_overlay_and_panic_clear() {
        let _ = clear_alt_screen_owners();
        assert_eq!(alt_screen_owner_count_for_test(), 0);

        let screen_parent = acquire_alt_screen_owner();
        assert!(screen_parent.should_enter);
        assert_eq!(alt_screen_owner_count_for_test(), 1);

        let overlay = acquire_alt_screen_owner();
        assert!(!overlay.should_enter);
        assert_eq!(alt_screen_owner_count_for_test(), 2);

        let overlay_release = release_alt_screen_owner();
        assert!(!overlay_release.should_leave);
        assert_eq!(alt_screen_owner_count_for_test(), 1);

        let panic_clear = clear_alt_screen_owners();
        assert!(panic_clear.should_leave);
        assert_eq!(alt_screen_owner_count_for_test(), 0);

        let parent_unwind_release = release_alt_screen_owner();
        assert!(!parent_unwind_release.should_leave);
        assert_eq!(alt_screen_owner_count_for_test(), 0);
    }

    #[test]
    fn inline_cleanup_clears_viewport_before_resume_output() {
        let mut out = Vec::new();
        write_inline_viewport_cleanup(&mut out, 3).expect("cleanup writes");
        out.extend_from_slice(b"\nTo resume this conversation:\n");

        let output = String::from_utf8(out).expect("ansi utf8");
        let cleanup_idx = output
            .find("\x1b[1G\x1b[2A\x1b[2K\x1b[1B\x1b[2K\x1b[1B\x1b[2K\x1b[1G")
            .expect("cleanup sequence");
        let resume_idx = output
            .find("To resume this conversation:")
            .expect("resume hint");
        assert!(cleanup_idx < resume_idx);
        assert!(!output.contains("\x1b[J"));
    }

    #[test]
    fn inline_area_cleanup_clears_actual_buffer_area_before_resume_output() {
        let mut out = Vec::new();
        write_inline_area_cleanup(&mut out, Rect::new(0, 7, 80, 3), 10).expect("cleanup writes");
        out.extend_from_slice(b"To resume this conversation:\n");

        let output = String::from_utf8(out).expect("ansi utf8");
        assert!(output
            .starts_with("\x1b[8;1H\x1b[2K\x1b[9;1H\x1b[2K\x1b[10;1H\x1b[2K\x1b[8;1HTo resume"));
        assert!(!output.contains("\x1b[J"));
        assert!(!output.contains("\x1b[2A"));
    }

    #[test]
    fn inline_area_cleanup_clamps_to_visible_terminal_height() {
        let mut out = Vec::new();
        write_inline_area_cleanup(&mut out, Rect::new(0, 8, 80, 5), 10).expect("cleanup writes");
        let output = String::from_utf8(out).expect("ansi utf8");

        assert_eq!(output, "\x1b[9;1H\x1b[2K\x1b[10;1H\x1b[2K\x1b[9;1H");
    }

    #[test]
    fn inline_area_cleanup_noops_when_area_starts_below_terminal() {
        let mut out = Vec::new();
        write_inline_area_cleanup(&mut out, Rect::new(0, 10, 80, 3), 10).expect("cleanup writes");

        assert!(out.is_empty());
    }

    #[test]
    fn inline_cleanup_height_one_does_not_move_above_current_row() {
        let mut out = Vec::new();
        write_inline_viewport_cleanup(&mut out, 1).expect("cleanup writes");
        let output = String::from_utf8(out).expect("ansi utf8");

        assert_eq!(output, "\x1b[1G\x1b[2K\x1b[1G");
        assert!(!output.contains('A'));
    }

    #[test]
    fn inline_cleanup_keeps_resume_hint_at_viewport_top_without_absolute_cursor_move() {
        let mut out = Vec::new();
        write_inline_viewport_cleanup(&mut out, 3).expect("cleanup writes");
        out.extend_from_slice(b"To resume this conversation:\n");
        let output = String::from_utf8(out).expect("ansi utf8");

        assert!(
            output.starts_with("\x1b[1G\x1b[2A\x1b[2K\x1b[1B\x1b[2K\x1b[1B\x1b[2K\x1b[1GTo resume")
        );
        assert!(!output.contains("\x1b[H"));
        assert!(!output.contains("\x1b[J"));
    }

    #[test]
    fn invalidating_terminal_buffers_forces_unchanged_cells_to_redraw() {
        use ratatui::backend::TestBackend;
        use ratatui::text::Line;
        use ratatui::widgets::Paragraph;

        let backend = TestBackend::new(8, 2);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new(Line::from("border")), frame.area()))
            .expect("initial draw");
        ratatui::backend::Backend::clear(terminal.backend_mut()).expect("external clear");
        invalidate_terminal_frame_buffers(&mut terminal);

        terminal
            .draw(|frame| frame.render_widget(Paragraph::new(Line::from("border")), frame.area()))
            .expect("redraw after invalidation");
        let rendered = terminal.backend().buffer().cell((0, 0)).unwrap().symbol();

        assert_eq!(rendered, "b");
    }

    #[test]
    fn terminal_title_sequence_uses_osc_zero_with_bel() {
        assert_eq!(
            terminal_title_sequence("Session Title").as_deref(),
            Some("\x1b]0;Session Title\x07")
        );
    }

    #[test]
    fn terminal_title_sequence_ignores_empty_titles() {
        assert_eq!(terminal_title_sequence(""), None);
        assert_eq!(terminal_title_sequence("  \t\r\n  "), None);
    }

    #[test]
    fn sanitize_terminal_title_removes_controls_and_escape_injection() {
        let title = "safe\x1b]0;bad\x07 after\x1b[31m red\x1b[0m\r\n\x07done";
        assert_eq!(
            sanitize_terminal_title(title).as_deref(),
            Some("safe after reddone")
        );
    }

    #[test]
    fn sanitize_terminal_title_drops_unterminated_osc_sequence() {
        assert_eq!(
            sanitize_terminal_title("safe\x1b]0;bad title").as_deref(),
            Some("safe")
        );
    }

    #[test]
    fn sanitize_terminal_title_preserves_unicode() {
        assert_eq!(
            sanitize_terminal_title("  会话标题 Ω  ").as_deref(),
            Some("会话标题 Ω")
        );
    }

    #[test]
    fn sanitize_terminal_title_truncates_by_char() {
        let title = "界".repeat(MAX_TERMINAL_TITLE_CHARS + 8);
        let sanitized = sanitize_terminal_title(&title).unwrap();
        assert_eq!(sanitized.chars().count(), MAX_TERMINAL_TITLE_CHARS);
        assert!(sanitized.chars().all(|ch| ch == '界'));
    }

    #[test]
    fn terminal_title_for_request_state_idle_returns_static_sanitized_title() {
        assert_eq!(
            terminal_title_for_request_state(
                "  \x1b[31mSession Title\x1b[0m  ",
                false,
                1234,
                false,
                None,
            ),
            "Session Title"
        );
    }

    #[test]
    fn terminal_title_for_request_state_loading_includes_base_and_prefix() {
        let title = terminal_title_for_request_state(
            "Session",
            true,
            0,
            false,
            Some(PromptCompletionStatus::Failed),
        );

        assert!(title.contains("Session"));
        assert_ne!(title, "Session");
        assert!(title.starts_with("[=       ] "));
    }

    #[test]
    fn terminal_title_for_request_state_success_uses_check_marker() {
        assert_eq!(
            terminal_title_for_request_state(
                "Session",
                false,
                0,
                false,
                Some(PromptCompletionStatus::Succeeded),
            ),
            "✓ Session"
        );
    }

    #[test]
    fn terminal_title_for_request_state_failure_uses_cross_marker() {
        assert_eq!(
            terminal_title_for_request_state(
                "Session",
                false,
                0,
                false,
                Some(PromptCompletionStatus::Failed),
            ),
            "✗ Session"
        );
    }

    #[test]
    fn terminal_title_for_request_state_active_state_overrides_completed_marker() {
        assert_eq!(
            terminal_title_for_request_state("✓ Session", true, 400, false, None),
            "[  =     ] Session"
        );
        assert_eq!(
            terminal_title_for_request_state("☑️ Session", true, 400, true, None),
            "[  ?     ] Session"
        );
        assert_eq!(
            terminal_title_for_request_state(
                "✅ Session",
                false,
                0,
                false,
                Some(PromptCompletionStatus::Failed),
            ),
            "✗ Session"
        );
    }

    #[test]
    fn terminal_title_for_request_state_normalizes_completed_marker() {
        assert_eq!(
            terminal_title_for_request_state("☑️ Session", false, 0, false, None),
            "✓ Session"
        );
        assert_eq!(
            terminal_title_for_request_state("✅ Session", false, 0, false, None),
            "✓ Session"
        );
    }

    #[test]
    fn terminal_title_for_request_state_uses_two_hundred_ms_buckets() {
        let speed = rebon_spinner::glimmer_speed_for_mode(rebon_spinner::SpinnerMode::Responding);
        assert_eq!(speed, 200);

        let first = terminal_title_for_request_state("Session", true, 0, false, None);
        let same_bucket = terminal_title_for_request_state("Session", true, speed - 1, false, None);
        let next_bucket = terminal_title_for_request_state("Session", true, speed, false, None);

        assert_eq!(first, same_bucket);
        assert_ne!(first, next_bucket);
    }

    #[test]
    fn terminal_title_for_request_state_same_elapsed_reflects_changed_base() {
        let first = terminal_title_for_request_state("First", true, 400, false, None);
        let second = terminal_title_for_request_state("Second", true, 400, false, None);

        assert!(first.contains("First"));
        assert!(second.contains("Second"));
        assert!(!second.contains("First"));
    }

    #[test]
    fn terminal_title_for_request_state_removes_control_injection() {
        let title = terminal_title_for_request_state(
            "safe\x1b]0;bad\x07 after\x1b[31m red\x1b[0m\r\n\x07done",
            true,
            0,
            false,
            None,
        );

        assert!(title.contains("safe after reddone"));
        assert!(!title.contains("bad"));
        assert!(title.chars().all(|ch| !ch.is_control()));
    }

    #[test]
    fn terminal_title_for_request_state_sequence_has_only_wrapper_controls() {
        let title = terminal_title_for_request_state(
            "safe\x1b]0;bad\x07 after\x1b[31m red\x1b[0m",
            true,
            0,
            false,
            None,
        );
        let sequence = terminal_title_sequence(&title).unwrap();
        let payload = sequence
            .strip_prefix("\x1b]0;")
            .and_then(|value| value.strip_suffix('\x07'))
            .unwrap();

        assert!(payload.chars().all(|ch| !ch.is_control()));
        assert_eq!(sequence.matches('\x1b').count(), 1);
        assert_eq!(sequence.matches('\x07').count(), 1);
    }

    #[test]
    fn terminal_title_for_request_state_long_title_with_prefix_is_bounded() {
        let title = terminal_title_for_request_state(
            &"界".repeat(MAX_TERMINAL_TITLE_CHARS + 8),
            true,
            0,
            false,
            None,
        );

        assert!(title.chars().count() <= MAX_TERMINAL_TITLE_CHARS);
        assert!(title.starts_with("[="));
        assert!(title.contains('界'));
    }

    #[test]
    fn terminal_title_for_request_state_long_completed_title_is_bounded() {
        let title = terminal_title_for_request_state(
            &"界".repeat(MAX_TERMINAL_TITLE_CHARS + 8),
            false,
            0,
            false,
            Some(PromptCompletionStatus::Succeeded),
        );

        assert!(title.chars().count() <= MAX_TERMINAL_TITLE_CHARS);
        assert!(title.starts_with("✓ "));
        assert!(title.contains('界'));
    }

    #[test]
    fn terminal_title_for_request_state_permission_wait_uses_question_marker() {
        let speed = rebon_spinner::glimmer_speed_for_mode(rebon_spinner::SpinnerMode::Responding);

        let first = terminal_title_for_request_state(
            "Session",
            true,
            0,
            true,
            Some(PromptCompletionStatus::Succeeded),
        );
        let same_bucket = terminal_title_for_request_state("Session", true, speed - 1, true, None);
        let next_bucket = terminal_title_for_request_state("Session", true, speed, true, None);
        let second_next_bucket =
            terminal_title_for_request_state("Session", true, speed * 2, true, None);

        assert_eq!(first, "[?       ] Session");
        assert_eq!(same_bucket, first);
        assert_eq!(next_bucket, "[ ?      ] Session");
        assert_eq!(second_next_bucket, "[  ?     ] Session");
    }

    #[test]
    fn terminal_title_for_request_state_permission_wait_overrides_idle_title() {
        assert_eq!(
            terminal_title_for_request_state(
                "Session",
                false,
                0,
                true,
                Some(PromptCompletionStatus::Failed),
            ),
            "[?       ] Session"
        );
    }

    #[test]
    fn terminal_title_for_request_state_preserves_unicode_title() {
        let title = terminal_title_for_request_state("会话标题 Ω", true, 400, false, None);

        assert!(title.contains("会话标题 Ω"));
        assert!(title.chars().count() <= MAX_TERMINAL_TITLE_CHARS);
    }

    #[test]
    fn terminal_title_manager_deduplicates_titles() {
        let mut manager = TerminalTitleManager::new_for_test(false);

        assert_eq!(
            manager.sequence_for_title("First").as_deref(),
            Some("\x1b]0;First\x07")
        );
        assert_eq!(manager.sequence_for_title("First"), None);
        assert_eq!(
            manager.sequence_for_title("Second").as_deref(),
            Some("\x1b]0;Second\x07")
        );
    }

    #[test]
    fn terminal_title_env_gate_reads_rebon_var() {
        let rebon_env = env_map(&[("REBON_DISABLE_TERMINAL_TITLE", "true")]);
        assert!(terminal_title_disabled_with_env(|name| rebon_env
            .get(name)
            .cloned()));

        let falsy_env = env_map(&[("REBON_DISABLE_TERMINAL_TITLE", "false")]);
        assert!(!terminal_title_disabled_with_env(|name| falsy_env
            .get(name)
            .cloned()));
    }
}
