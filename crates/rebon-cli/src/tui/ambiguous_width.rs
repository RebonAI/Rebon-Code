//! Which way this terminal paints East Asian ambiguous characters.
//!
//! `—` (U+2014) and `·` (U+00B7) — the two characters the Agent View
//! builds its session rows out of — are *ambiguous width*: one cell in a
//! Western context, two in a CJK one. The renderer has to know which,
//! because a string measured one way and painted the other leaves the
//! cursor a column off, and the next partial redraw writes into the wrong
//! cell and cannot erase what it left behind (`· lingers 9 min` over
//! `stopped` came out as `· ingers 9 min      ed`).
//!
//! There is no portable way to ask a terminal what it will do, so this
//! settles the question the only way that cannot be wrong on the machine
//! it runs on: print one such character and see how far the cursor moved.
//! On Windows that reading is a console API call either side of a
//! two-cell write — microseconds, no round trip, nothing to time out —
//! which is why the probe is Windows-only. Elsewhere the locale is the
//! best guess available, and [`OVERRIDE_ENV`] overrides both.
//!
//! Resolved once, before the first frame, into [`rebon_width`]; every
//! measurement in the render stack reads it from there.

use std::io::{IsTerminal, Stdout};

/// `wide` / `narrow` (also `double` / `single`, `cjk`, `2` / `1`) forces the
/// policy, for a terminal neither the probe nor the locale reads right.
pub const OVERRIDE_ENV: &str = "REBON_AMBIGUOUS_WIDTH";

/// The character the probe measures. Ambiguous, and one of the two the
/// bug was found with.
#[cfg(windows)]
const PROBE_CHAR: &str = "·";

/// Resolve this terminal's policy and install it process-wide.
///
/// Call once, from the terminal lifecycle, before anything measures a
/// string for this terminal.
pub(crate) fn adopt(stdout: &mut Stdout) {
    let (wide, source) = resolve(stdout);
    rebon_width::set_ambiguous_wide(wide);
    tracing::info!(
        ambiguous_wide = wide,
        source,
        "rebon startup: east asian ambiguous width policy"
    );
}

fn resolve(stdout: &mut Stdout) -> (bool, &'static str) {
    if let Some(forced) = from_override() {
        return (forced, "override");
    }
    if let Some(measured) = probe(stdout) {
        return (measured, "probe");
    }
    (from_locale(&read_locale()), "locale")
}

fn from_override() -> Option<bool> {
    let raw = std::env::var(OVERRIDE_ENV).ok()?;
    match parse_override(&raw) {
        Some(wide) => Some(wide),
        None => {
            tracing::warn!(
                value = %raw,
                "rebon: {OVERRIDE_ENV} is neither wide nor narrow; ignoring it"
            );
            None
        }
    }
}

fn parse_override(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "wide" | "double" | "2" | "cjk" => Some(true),
        "narrow" | "single" | "1" => Some(false),
        _ => None,
    }
}

/// Print an ambiguous character and read how far the cursor moved.
///
/// Runs on the row the first frame is about to own — inside the alternate
/// screen where there is one, and inside the inline viewport where there
/// is not — so the two cells it borrows are painted over either way. It
/// still puts them back, in case the reading fails and no frame follows.
#[cfg(windows)]
fn probe(stdout: &mut Stdout) -> Option<bool> {
    use ratatui::crossterm::cursor::{MoveTo, MoveToColumn};
    use ratatui::crossterm::execute;
    use ratatui::crossterm::style::Print;

    if !stdout.is_terminal() {
        return None;
    }
    let (start_column, row) = ratatui::crossterm::cursor::position().ok()?;
    if execute!(stdout, MoveTo(0, row), Print(PROBE_CHAR)).is_err() {
        return None;
    }
    let advance = ratatui::crossterm::cursor::position()
        .ok()
        .map(|(column, _)| column);
    // Two spaces rather than an erase to the line end: whatever else is
    // on this row is none of the probe's business.
    let _ = execute!(
        stdout,
        MoveTo(0, row),
        Print("  "),
        MoveToColumn(start_column)
    );
    match advance? {
        1 => Some(false),
        2 => Some(true),
        other => {
            tracing::debug!(
                advance = other,
                "rebon: ambiguous width probe read an implausible cursor advance; falling back"
            );
            None
        }
    }
}

/// Off Windows, reading the cursor back means an escape sequence and a
/// reply from the terminal — a round trip that can go unanswered, on the
/// startup path that must not stall. The locale answers instead.
#[cfg(not(windows))]
fn probe(_stdout: &mut Stdout) -> Option<bool> {
    None
}

fn read_locale() -> String {
    for name in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return value;
            }
        }
    }
    String::new()
}

/// A CJK locale is the context the wide reading belongs to, and the
/// terminals set up for one draw these characters wide.
fn from_locale(locale: &str) -> bool {
    let language = locale
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let language = language.split(['_', '-']).next().unwrap_or_default();
    matches!(language, "zh" | "ja" | "ko")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_reads_both_ways_and_ignores_nonsense() {
        assert_eq!(parse_override("wide"), Some(true));
        assert_eq!(parse_override(" Double "), Some(true));
        assert_eq!(parse_override("2"), Some(true));
        assert_eq!(parse_override("narrow"), Some(false));
        assert_eq!(parse_override("1"), Some(false));
        assert_eq!(parse_override("maybe"), None);
        assert_eq!(parse_override(""), None);
    }

    #[test]
    fn cjk_locales_read_wide_and_others_narrow() {
        assert!(from_locale("zh_CN.UTF-8"));
        assert!(from_locale("ja_JP.eucJP"));
        assert!(from_locale("ko_KR"));
        assert!(from_locale("zh-Hans"));
        assert!(!from_locale("en_US.UTF-8"));
        assert!(!from_locale("C"));
        assert!(!from_locale(""));
        // A language that merely starts with the same letters is not one
        // of them.
        assert!(!from_locale("zu_ZA.UTF-8"));
    }
}
