//! Which shell tool the agent is allowed to reach for.
//!
//! Rebon ships two peer shell tools — `Bash` and `PowerShell`. They are not
//! variants of one
//! another: each has its own description, spawn arguments, and permission
//! surface, so the choice of which one(s) to advertise is a real setting
//! rather than a platform detail.
//!
//! The choice lives in three places, in priority order:
//!
//! 1. `REBON_SHELL_TOOL` — process-lifetime env override (`auto`, `bash`,
//!    `powershell`, `both`), the escape hatch for tests and one-off shells.
//! 2. The process-global atomic below, seeded at startup from
//!    `~/.rebon/config.json`'s `shellTool` key (see
//!    the saved config) and re-set live when the desktop
//!    Settings window or the TUI `/config` dialog changes it.
//! 3. [`ShellToolPreference::Auto`] — the default, which resolves per
//!    platform (see [`bash_tool_enabled`] / [`powershell_tool_enabled`]).
//!
//! Both `BashTool::is_enabled` and `PowerShellTool::is_enabled` read this,
//! so a disabled shell disappears from the API tool list *and* from the
//! system prompt's tool section on the very next turn — the same mechanism
//! the sub-agent toggle uses.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// Which shell tool(s) to advertise to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellToolPreference {
    /// Resolve per platform: PowerShell where a PowerShell runtime exists,
    /// Bash where a POSIX shell does, and PowerShell alone on a Windows box
    /// with no Git Bash.
    #[default]
    Auto,
    /// Advertise `Bash` only.
    Bash,
    /// Advertise `PowerShell` only.
    PowerShell,
    /// Advertise both and let the model pick per command.
    Both,
}

impl ShellToolPreference {
    /// Wire value stored in `config.json` and shown in the settings UIs.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Bash => "bash",
            Self::PowerShell => "powershell",
            Self::Both => "both",
        }
    }

    /// Every value in display order — the option list both settings surfaces
    /// render, so they cannot drift apart.
    pub const ALL: [Self; 4] = [Self::Auto, Self::Bash, Self::PowerShell, Self::Both];

    /// Parse a persisted or user-typed value.
    ///
    /// Unknown values return `None` so a caller can tell "not configured"
    /// from "configured to something we don't understand" — readers that
    /// just want a value use [`ShellToolPreference::from_wire_or_default`].
    pub fn parse(value: &str) -> Option<Self> {
        match value
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_'], "")
            .as_str()
        {
            "auto" | "default" => Some(Self::Auto),
            "bash" | "sh" | "posix" => Some(Self::Bash),
            "powershell" | "pwsh" | "ps" => Some(Self::PowerShell),
            "both" | "all" => Some(Self::Both),
            _ => None,
        }
    }

    /// Same as [`ShellToolPreference::parse`] but falls back to
    /// [`ShellToolPreference::Auto`] instead of reporting the miss.
    pub fn from_wire_or_default(value: &str) -> Self {
        Self::parse(value).unwrap_or_default()
    }

    const fn as_u8(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::Bash => 1,
            Self::PowerShell => 2,
            Self::Both => 3,
        }
    }

    const fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Bash,
            2 => Self::PowerShell,
            3 => Self::Both,
            _ => Self::Auto,
        }
    }
}

impl std::fmt::Display for ShellToolPreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

static SHELL_TOOL_PREFERENCE: AtomicU8 = AtomicU8::new(0);

/// Env override checked ahead of the persisted setting.
pub const SHELL_TOOL_ENV_VAR: &str = "REBON_SHELL_TOOL";

/// The preference in force for this process.
///
/// Called from `is_enabled`, which the engine runs for every tool on every
/// snapshot, so the env override is read once rather than per call — reading
/// it takes a process-wide lock and allocates, and it cannot change under a
/// running process anyway.
pub fn shell_tool_preference() -> ShellToolPreference {
    static ENV_OVERRIDE: OnceLock<Option<ShellToolPreference>> = OnceLock::new();
    let override_value = ENV_OVERRIDE.get_or_init(|| {
        std::env::var(SHELL_TOOL_ENV_VAR)
            .ok()
            .as_deref()
            .and_then(ShellToolPreference::parse)
    });
    override_value.unwrap_or_else(|| {
        ShellToolPreference::from_u8(SHELL_TOOL_PREFERENCE.load(Ordering::Relaxed))
    })
}

/// Seed or change the process-wide preference.
///
/// Called at startup by the front end (TUI and ACP wiring) with the value
/// from `~/.rebon/config.json`, and at runtime by the desktop Settings window
/// and the TUI `/config` dialog so the change lands without a restart.
pub fn set_shell_tool_preference(preference: ShellToolPreference) {
    SHELL_TOOL_PREFERENCE.store(preference.as_u8(), Ordering::Relaxed);
}

/// Whether the `Bash` tool should be advertised.
///
/// Under [`ShellToolPreference::Auto`] this is "yes, unless the platform
/// cannot run it": a Windows box with no Git Bash would otherwise get a Bash
/// tool that silently executes through `powershell.exe`, which is worse than
/// the real PowerShell tool for both the model (wrong syntax in the prompt)
/// and the user (permission rules written in the wrong shell's grammar).
///
/// Under [`ShellToolPreference::PowerShell`] Bash stays enabled when no
/// PowerShell runtime exists, because handing the session *no* shell at all
/// is a worse failure than honouring the preference imperfectly.
pub fn bash_tool_enabled() -> bool {
    match shell_tool_preference() {
        ShellToolPreference::Bash | ShellToolPreference::Both => true,
        ShellToolPreference::PowerShell => !crate::powershell::is_available(),
        ShellToolPreference::Auto => posix_shell_available(),
    }
}

/// Whether the `PowerShell` tool should be advertised.
///
/// Always gated on an actual runtime being installed — advertising a tool
/// whose every call would answer "PowerShell is not available" wastes a
/// schema in every request and invites the model to keep retrying.
pub fn powershell_tool_enabled() -> bool {
    if !crate::powershell::is_available() {
        return false;
    }
    match shell_tool_preference() {
        ShellToolPreference::PowerShell | ShellToolPreference::Both => true,
        ShellToolPreference::Bash => false,
        // On Windows PowerShell is the native shell and is
        // advertised alongside Bash; elsewhere it stays registered but
        // deferred (see `BuiltinToolExposurePolicy`), reachable via ToolSearch.
        ShellToolPreference::Auto => true,
    }
}

/// Whether this platform can actually run the `Bash` tool.
#[cfg(windows)]
fn posix_shell_available() -> bool {
    crate::bash::git_bash_available()
}

#[cfg(not(windows))]
fn posix_shell_available() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The atomic is process-global, so preference tests serialize on this.
    pub(crate) static PREFERENCE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Restore(ShellToolPreference);

    impl Drop for Restore {
        fn drop(&mut self) {
            set_shell_tool_preference(self.0);
        }
    }

    #[test]
    fn wire_values_round_trip() {
        for preference in ShellToolPreference::ALL {
            assert_eq!(
                ShellToolPreference::parse(preference.as_wire()),
                Some(preference)
            );
        }
    }

    #[test]
    fn parse_accepts_aliases_and_rejects_the_rest() {
        assert_eq!(
            ShellToolPreference::parse("PowerShell"),
            Some(ShellToolPreference::PowerShell)
        );
        assert_eq!(
            ShellToolPreference::parse("pwsh"),
            Some(ShellToolPreference::PowerShell)
        );
        assert_eq!(
            ShellToolPreference::parse(" Both "),
            Some(ShellToolPreference::Both)
        );
        assert_eq!(ShellToolPreference::parse("fish"), None);
        assert_eq!(
            ShellToolPreference::from_wire_or_default("fish"),
            ShellToolPreference::Auto
        );
    }

    #[test]
    fn explicit_bash_preference_hides_powershell() {
        let _guard = PREFERENCE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = Restore(shell_tool_preference());
        set_shell_tool_preference(ShellToolPreference::Bash);
        assert!(bash_tool_enabled());
        assert!(!powershell_tool_enabled());
    }

    #[test]
    fn both_preference_enables_bash_and_powershell_when_installed() {
        let _guard = PREFERENCE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = Restore(shell_tool_preference());
        set_shell_tool_preference(ShellToolPreference::Both);
        assert!(bash_tool_enabled());
        assert_eq!(powershell_tool_enabled(), crate::powershell::is_available());
    }

    /// Choosing PowerShell on a box with no PowerShell must not leave the
    /// session with no shell at all.
    #[test]
    fn powershell_preference_keeps_bash_when_no_runtime_exists() {
        let _guard = PREFERENCE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = Restore(shell_tool_preference());
        set_shell_tool_preference(ShellToolPreference::PowerShell);
        assert_eq!(bash_tool_enabled(), !crate::powershell::is_available());
    }
}
