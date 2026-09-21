use rebon_types::env::is_env_truthy as is_truthy_env;

pub(super) fn terminal_supports_hyperlinks() -> bool {
    // Explicit overrides take precedence over terminal auto-detection, with the
    // disable flag winning when both overrides are enabled.
    if std::env::var("REBON_DISABLE_HYPERLINKS")
        .ok()
        .as_deref()
        .is_some_and(is_truthy_env)
    {
        return false;
    }
    if std::env::var("REBON_FORCE_HYPERLINKS")
        .ok()
        .as_deref()
        .is_some_and(is_truthy_env)
    {
        return true;
    }

    // Hyperlink passthrough depends on tmux version/configuration, so require
    // the explicit force override rather than risking leaked OSC state.
    if std::env::var_os("TMUX").is_some() {
        return false;
    }
    if std::env::var_os("WT_SESSION").is_some_and(|value| !value.is_empty()) {
        return true;
    }
    if let Ok(term_program) = std::env::var("TERM_PROGRAM") {
        if matches!(
            term_program.to_ascii_lowercase().as_str(),
            "iterm.app" | "wezterm" | "warpterminal" | "ghostty" | "vscode"
        ) {
            return true;
        }
    }
    if std::env::var_os("KITTY_WINDOW_ID").is_some_and(|value| !value.is_empty()) {
        return true;
    }
    if let Ok(term) = std::env::var("TERM") {
        let term = term.to_ascii_lowercase();
        if term.contains("kitty") || term.starts_with("foot") {
            return true;
        }
    }
    if let Ok(vte) = std::env::var("VTE_VERSION") {
        if vte
            .trim()
            .parse::<u32>()
            .is_ok_and(|version| version >= 5000)
        {
            return true;
        }
    }
    std::env::var("KONSOLE_VERSION")
        .ok()
        .and_then(|version| version.trim().parse::<u32>().ok())
        .is_some_and(|version| version >= 220400)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MathGraphicsCapability {
    pub(super) protocol: rebon_tui::MathGraphicsProtocol,
    pub(super) cell_size: (u16, u16),
}

pub(super) fn terminal_math_graphics_capability() -> Option<MathGraphicsCapability> {
    if std::env::var_os("TMUX").is_some()
        || std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_CLIENT").is_some()
        || std::env::var_os("SSH_TTY").is_some()
    {
        return None;
    }

    let protocol = math_graphics_protocol_from_env()?;
    let cell_size = ratatui::crossterm::terminal::window_size()
        .ok()
        .and_then(|size| {
            let width = size.width.checked_div(size.columns)?;
            let height = size.height.checked_div(size.rows)?;
            (width > 0 && height > 0).then_some((width, height))
        })
        .unwrap_or((8, 16));

    Some(MathGraphicsCapability {
        protocol,
        cell_size,
    })
}

pub(super) fn terminal_math_display_mode(
    mode: crate::rebon_config::MathRenderingMode,
    ui_mode: crate::ui_config::UiMode,
) -> rebon_tui::MathDisplayMode {
    math_display_mode_with_capability(mode, ui_mode, terminal_math_graphics_capability())
}

fn math_display_mode_with_capability(
    mode: crate::rebon_config::MathRenderingMode,
    ui_mode: crate::ui_config::UiMode,
    capability: Option<MathGraphicsCapability>,
) -> rebon_tui::MathDisplayMode {
    use crate::rebon_config::MathRenderingMode;
    use crate::ui_config::UiMode;

    match mode {
        MathRenderingMode::Off => rebon_tui::MathDisplayMode::Off,
        MathRenderingMode::Unicode => rebon_tui::MathDisplayMode::Unicode,
        MathRenderingMode::GraphicsAuto => match (ui_mode, capability) {
            (UiMode::Screen, Some(capability)) => rebon_tui::MathDisplayMode::Graphics {
                protocol: capability.protocol,
                cell_size: capability.cell_size,
            },
            _ => rebon_tui::MathDisplayMode::Unicode,
        },
    }
}

fn math_graphics_protocol_from_env() -> Option<rebon_tui::MathGraphicsProtocol> {
    use rebon_tui::MathGraphicsProtocol;

    if std::env::var_os("KITTY_WINDOW_ID").is_some_and(|value| !value.is_empty())
        || std::env::var("TERM")
            .ok()
            .is_some_and(|term| term.to_ascii_lowercase().contains("kitty"))
    {
        return Some(MathGraphicsProtocol::Kitty);
    }
    if std::env::var("TERM")
        .ok()
        .is_some_and(|term| term.to_ascii_lowercase().contains("sixel"))
    {
        return Some(MathGraphicsProtocol::Sixel);
    }
    if std::env::var("TERM_PROGRAM")
        .ok()
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "iterm.app" | "wezterm"))
        || std::env::var("LC_TERMINAL")
            .ok()
            .is_some_and(|value| value.to_ascii_lowercase().contains("iterm"))
    {
        return Some(MathGraphicsProtocol::Iterm2);
    }
    None
}

/// Whether the host terminal honors DEC private mode 2026 (synchronized
/// output). Between `BeginSynchronizedUpdate` (`CSI ?2026h`) and
/// `EndSynchronizedUpdate` (`CSI ?2026l`) the emulator buffers every write
/// and repaints them as ONE atomic frame, which is what lets the inline
/// composer's per-frame `resize → insert_before → draw` land without the
/// half-drawn intermediate frames that read as flicker.
///
/// Allow-list of terminals known to support synchronized output. Terminals
/// that ignore unknown private modes treat the sequences as harmless no-ops, so a
/// false negative just forgoes the optimization — but tmux can split a
/// BSU/ESU pair across its own output chunks and break atomicity, so it is
/// skipped outright.
pub(super) fn terminal_supports_synchronized_output() -> bool {
    if std::env::var("REBON_DISABLE_SYNC_OUTPUT")
        .ok()
        .as_deref()
        .is_some_and(is_truthy_env)
    {
        return false;
    }
    if std::env::var("REBON_FORCE_SYNC_OUTPUT")
        .ok()
        .as_deref()
        .is_some_and(is_truthy_env)
    {
        return true;
    }
    // tmux multiplexes child output in chunks that can fall between BSU and
    // ESU, defeating the atomicity guarantee even where newer tmux advertises
    // 2026, so it is skipped outright.
    if std::env::var_os("TMUX").is_some() {
        return false;
    }
    // Windows Terminal sets WT_SESSION and honors DEC 2026.
    if std::env::var_os("WT_SESSION").is_some() {
        return true;
    }
    if let Ok(term_program) = std::env::var("TERM_PROGRAM") {
        if matches!(
            term_program.as_str(),
            "iTerm.app"
                | "WezTerm"
                | "WarpTerminal"
                | "ghostty"
                | "contour"
                | "vscode"
                | "alacritty"
        ) {
            return true;
        }
    }
    // kitty (KITTY_WINDOW_ID or TERM=xterm-kitty) and foot (TERM=foot*).
    if std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return true;
    }
    if let Ok(term) = std::env::var("TERM") {
        if term.contains("kitty") || term.contains("foot") {
            return true;
        }
    }
    // Zed's built-in terminal.
    if std::env::var("ZED_TERM").is_ok_and(|value| value == "true") {
        return true;
    }
    // VTE-based terminals (GNOME Terminal, etc.) gained synchronized output in
    // VTE 0.68 (VTE_VERSION 6800).
    if let Ok(vte) = std::env::var("VTE_VERSION") {
        if let Ok(version) = vte.trim().parse::<u32>() {
            return version >= 6800;
        }
    }
    false
}

/// RAII bracket for a DEC 2026 synchronized frame.
///
/// Construction emits `BeginSynchronizedUpdate` (only when the terminal
/// supports it); the matching `EndSynchronizedUpdate` fires on drop, so the
/// frame is always closed even when rendering bails out early via `?`. On
/// terminals without 2026 support this is an inert pair of no-ops.
pub(super) struct SynchronizedFrame {
    active: bool,
}

impl Drop for SynchronizedFrame {
    fn drop(&mut self) {
        if self.active {
            use ratatui::crossterm::execute;
            use ratatui::crossterm::terminal::EndSynchronizedUpdate;
            let _ = execute!(std::io::stdout(), EndSynchronizedUpdate);
        }
    }
}

/// Open a synchronized frame, returning a guard that closes it on drop.
///
/// BSU/ESU are written to `std::io::stdout()` — the same global handle the
/// ratatui `CrosstermBackend` and the out-of-band cursor `MoveTo`/`Show`
/// write through — so all three share one ordered byte stream and the
/// terminal sees `BSU … draw … cursor … ESU` in sequence.
pub(super) fn begin_synchronized_frame() -> SynchronizedFrame {
    let active = terminal_supports_synchronized_output();
    if active {
        use ratatui::crossterm::execute;
        use ratatui::crossterm::terminal::BeginSynchronizedUpdate;
        let _ = execute!(std::io::stdout(), BeginSynchronizedUpdate);
    }
    SynchronizedFrame { active }
}

#[cfg(test)]
mod tests {
    use super::{
        is_truthy_env, math_display_mode_with_capability, terminal_math_graphics_capability,
        terminal_supports_hyperlinks, terminal_supports_synchronized_output,
        MathGraphicsCapability,
    };
    use ratatui::{
        backend::{Backend, CrosstermBackend},
        buffer::Buffer,
        layout::Rect,
    };

    /// Snapshots and clears every env var the terminal capability tests read.
    /// shared lock serializes these process-global mutations with other TUI env
    /// tests, and drop restores the caller's environment.
    struct HyperlinkEnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HyperlinkEnvGuard {
        const VARS: [&'static str; 12] = [
            "REBON_FORCE_HYPERLINKS",
            "REBON_DISABLE_HYPERLINKS",
            "TMUX",
            "WT_SESSION",
            "TERM_PROGRAM",
            "KITTY_WINDOW_ID",
            "TERM",
            "VTE_VERSION",
            "KONSOLE_VERSION",
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
        ];

        fn clear() -> Self {
            let _lock = crate::test_env::lock_env();
            let saved = Self::VARS
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect();
            unsafe {
                for name in Self::VARS {
                    std::env::remove_var(name);
                }
            }
            Self { saved, _lock }
        }
    }

    impl Drop for HyperlinkEnvGuard {
        fn drop(&mut self) {
            unsafe {
                for (name, value) in self.saved.drain(..) {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    #[test]
    fn truthy_env_accepts_common_enabled_values() {
        for value in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(is_truthy_env(value), "{value}");
        }
        for value in ["", "0", "false", "no", "off", "enabled"] {
            assert!(!is_truthy_env(value), "{value}");
        }
    }

    #[test]
    fn math_display_mode_defaults_off_and_keeps_unicode_portable() {
        use crate::rebon_config::MathRenderingMode;
        use crate::ui_config::UiMode;

        assert_eq!(
            math_display_mode_with_capability(MathRenderingMode::Off, UiMode::Screen, None),
            rebon_tui::MathDisplayMode::Off
        );
        assert_eq!(
            math_display_mode_with_capability(
                MathRenderingMode::Unicode,
                UiMode::Screen,
                Some(MathGraphicsCapability {
                    protocol: rebon_tui::MathGraphicsProtocol::Kitty,
                    cell_size: (9, 18),
                }),
            ),
            rebon_tui::MathDisplayMode::Unicode
        );
    }

    #[test]
    fn graphics_auto_uses_protocol_only_on_screen_surface() {
        use crate::rebon_config::MathRenderingMode;
        use crate::ui_config::UiMode;

        let capability = Some(MathGraphicsCapability {
            protocol: rebon_tui::MathGraphicsProtocol::Sixel,
            cell_size: (8, 16),
        });
        assert_eq!(
            math_display_mode_with_capability(
                MathRenderingMode::GraphicsAuto,
                UiMode::Screen,
                capability,
            ),
            rebon_tui::MathDisplayMode::Graphics {
                protocol: rebon_tui::MathGraphicsProtocol::Sixel,
                cell_size: (8, 16),
            }
        );
        assert_eq!(
            math_display_mode_with_capability(
                MathRenderingMode::GraphicsAuto,
                UiMode::Inline,
                capability,
            ),
            rebon_tui::MathDisplayMode::Unicode
        );
        assert_eq!(
            math_display_mode_with_capability(
                MathRenderingMode::GraphicsAuto,
                UiMode::Screen,
                None,
            ),
            rebon_tui::MathDisplayMode::Unicode
        );
    }

    #[test]
    fn ssh_and_tmux_force_native_math_graphics_fallback() {
        for variable in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "TMUX"] {
            let _guard = HyperlinkEnvGuard::clear();
            unsafe {
                std::env::set_var("KITTY_WINDOW_ID", "1");
                std::env::set_var(variable, "active");
            }
            assert_eq!(terminal_math_graphics_capability(), None, "{variable}");
        }
    }

    #[test]
    fn terminal_hyperlink_backend_preserves_width_and_incremental_updates() {
        fn draw_diff(previous: &Buffer, current: &Buffer) -> String {
            let mut bytes = Vec::new();
            {
                let mut backend = CrosstermBackend::new(&mut bytes);
                backend.draw(previous.diff(current).into_iter()).unwrap();
            }
            String::from_utf8(bytes).unwrap()
        }

        let area = Rect::new(0, 0, 6, 1);
        let previous = Buffer::empty(area);
        let mut first = Buffer::empty(area);
        for (x, symbol) in [(0, "G"), (1, "o"), (2, "中"), (4, "🙂")] {
            first[(x, 0)]
                .set_symbol(symbol)
                .set_hyperlink("https://one.test");
        }
        first[(3, 0)].set_hyperlink("https://one.test");
        first[(5, 0)].set_hyperlink("https://one.test");

        let first_output = draw_diff(&previous, &first);
        for symbol in ["G", "o", "中", "🙂"] {
            assert!(
                first_output.contains(&format!("\x1b]8;;https://one.test\x07{symbol}\x1b]8;;\x07"))
            );
        }
        assert_eq!(
            first_output.matches("\x1b]8;;https://one.test\x07").count(),
            first_output.matches("\x1b]8;;\x07").count()
        );

        let mut second = first.clone();
        for cell in &mut second.content {
            if cell.hyperlink().is_some() {
                cell.set_hyperlink("https://two.test");
            }
        }
        let second_output = draw_diff(&first, &second);
        assert!(!second_output.contains("https://one.test"));
        for symbol in ["G", "o", "中", "🙂"] {
            assert!(second_output
                .contains(&format!("\x1b]8;;https://two.test\x07{symbol}\x1b]8;;\x07")));
        }
        assert_eq!(
            second_output
                .matches("\x1b]8;;https://two.test\x07")
                .count(),
            second_output.matches("\x1b]8;;\x07").count()
        );
    }

    #[test]
    fn terminal_hyperlinks_disabled_without_a_known_terminal() {
        let _env = HyperlinkEnvGuard::clear();
        assert!(!terminal_supports_hyperlinks());
    }

    #[test]
    fn terminal_hyperlink_force_flag_enables() {
        let _env = HyperlinkEnvGuard::clear();
        unsafe {
            std::env::set_var("REBON_FORCE_HYPERLINKS", "1");
        }
        assert!(terminal_supports_hyperlinks());
    }

    #[test]
    fn terminal_hyperlink_disable_flag_overrides_force_and_terminal() {
        let _env = HyperlinkEnvGuard::clear();
        unsafe {
            std::env::set_var("WT_SESSION", "cafe-1234");
            std::env::set_var("REBON_FORCE_HYPERLINKS", "1");
            std::env::set_var("REBON_DISABLE_HYPERLINKS", "true");
        }
        assert!(!terminal_supports_hyperlinks());
    }

    #[test]
    fn terminal_hyperlinks_support_windows_terminal() {
        let _env = HyperlinkEnvGuard::clear();
        unsafe {
            std::env::set_var("WT_SESSION", "cafe-1234");
        }
        assert!(terminal_supports_hyperlinks());
    }

    #[test]
    fn terminal_hyperlinks_detect_known_terminal_families() {
        for (name, value) in [
            ("TERM_PROGRAM", "WezTerm"),
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM_PROGRAM", "ghostty"),
            ("TERM_PROGRAM", "vscode"),
            ("KITTY_WINDOW_ID", "1"),
            ("TERM", "foot-extra"),
            ("VTE_VERSION", "5000"),
            ("KONSOLE_VERSION", "220400"),
        ] {
            let _env = HyperlinkEnvGuard::clear();
            unsafe {
                std::env::set_var(name, value);
            }
            assert!(terminal_supports_hyperlinks(), "{name}={value}");
        }
    }

    #[test]
    fn terminal_hyperlinks_disable_tmux_without_force_override() {
        let _env = HyperlinkEnvGuard::clear();
        unsafe {
            std::env::set_var("WT_SESSION", "cafe-1234");
            std::env::set_var("TMUX", "/tmp/tmux-1000/default,1,0");
        }
        assert!(!terminal_supports_hyperlinks());

        unsafe {
            std::env::set_var("REBON_FORCE_HYPERLINKS", "1");
        }
        assert!(terminal_supports_hyperlinks());
    }

    #[test]
    fn terminal_hyperlinks_require_non_empty_windows_terminal_session() {
        let _env = HyperlinkEnvGuard::clear();
        unsafe {
            std::env::set_var("WT_SESSION", "");
        }
        assert!(!terminal_supports_hyperlinks());
    }

    /// Snapshots and clears every env var the synchronized-output detector
    /// reads, so each case starts from a known-empty environment regardless of
    /// what the host CI terminal exports. Restores on drop.
    struct SyncEnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl SyncEnvGuard {
        const VARS: [&'static str; 9] = [
            "REBON_FORCE_SYNC_OUTPUT",
            "REBON_DISABLE_SYNC_OUTPUT",
            "TMUX",
            "WT_SESSION",
            "TERM_PROGRAM",
            "KITTY_WINDOW_ID",
            "TERM",
            "ZED_TERM",
            "VTE_VERSION",
        ];

        fn clear() -> Self {
            let _lock = crate::test_env::lock_env();
            let saved = Self::VARS
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect();
            unsafe {
                for name in Self::VARS {
                    std::env::remove_var(name);
                }
            }
            Self { saved, _lock }
        }
    }

    impl Drop for SyncEnvGuard {
        fn drop(&mut self) {
            unsafe {
                for (name, value) in self.saved.drain(..) {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    #[test]
    fn sync_output_disabled_without_a_known_terminal() {
        let _env = SyncEnvGuard::clear();
        assert!(!terminal_supports_synchronized_output());
    }

    #[test]
    fn sync_output_force_flag_enables() {
        let _env = SyncEnvGuard::clear();
        unsafe {
            std::env::set_var("REBON_FORCE_SYNC_OUTPUT", "1");
        }
        assert!(terminal_supports_synchronized_output());
    }

    #[test]
    fn sync_output_disable_flag_overrides_force_and_terminal() {
        let _env = SyncEnvGuard::clear();
        unsafe {
            std::env::set_var("WT_SESSION", "1");
            std::env::set_var("REBON_FORCE_SYNC_OUTPUT", "1");
            std::env::set_var("REBON_DISABLE_SYNC_OUTPUT", "true");
        }
        assert!(!terminal_supports_synchronized_output());
    }

    #[test]
    fn sync_output_windows_terminal_supported() {
        let _env = SyncEnvGuard::clear();
        unsafe {
            std::env::set_var("WT_SESSION", "cafe-1234");
        }
        assert!(terminal_supports_synchronized_output());
    }

    #[test]
    fn sync_output_tmux_skipped_even_inside_supported_terminal() {
        let _env = SyncEnvGuard::clear();
        unsafe {
            std::env::set_var("WT_SESSION", "1");
            std::env::set_var("TMUX", "/tmp/tmux-1000/default,1,0");
        }
        assert!(!terminal_supports_synchronized_output());
    }

    #[test]
    fn sync_output_term_program_allow_list() {
        let _env = SyncEnvGuard::clear();
        unsafe {
            std::env::set_var("TERM_PROGRAM", "WezTerm");
        }
        assert!(terminal_supports_synchronized_output());
    }

    #[test]
    fn sync_output_kitty_term_supported() {
        let _env = SyncEnvGuard::clear();
        unsafe {
            std::env::set_var("TERM", "xterm-kitty");
        }
        assert!(terminal_supports_synchronized_output());
    }

    #[test]
    fn sync_output_vte_requires_minimum_version() {
        let _env = SyncEnvGuard::clear();
        unsafe {
            std::env::set_var("VTE_VERSION", "6003");
        }
        assert!(!terminal_supports_synchronized_output());
        unsafe {
            std::env::set_var("VTE_VERSION", "6800");
        }
        assert!(terminal_supports_synchronized_output());
    }
}
