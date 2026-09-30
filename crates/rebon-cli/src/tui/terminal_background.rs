//! The terminal's own background colour, which the panel tints (user
//! prompt card, code block panel, inline code chip, selection band) are
//! blended from — see `rebon_design_system::theme::set_terminal_background`.
//!
//! Sources, first hit wins:
//!
//! 1. `REBON_TERMINAL_BG=#rrggbb`, an explicit override.
//! 2. Windows Terminal: the settings of the running `WindowsTerminal.exe`,
//!    read for the profile `WT_PROFILE_ID` names.
//!
//! Nothing found leaves the palette's fixed panel colours in place.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Override variable: `#rrggbb` (or `rrggbb`).
const OVERRIDE_VAR: &str = "REBON_TERMINAL_BG";

/// Detect the background once per process and hand it to the theme.
pub fn apply() {
    static APPLIED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    APPLIED.get_or_init(|| {
        let background = detect();
        tracing::debug!(?background, "terminal background");
        if background.is_some() {
            rebon_design_system::theme::set_terminal_background(background);
        }
    });
}

fn detect() -> Option<(u8, u8, u8)> {
    if let Ok(value) = std::env::var(OVERRIDE_VAR) {
        return parse_hex(&value);
    }
    std::env::var_os("WT_SESSION")?;
    let settings = windows_terminal::settings_path()?;
    let text = std::fs::read_to_string(settings).ok()?;
    let profile = std::env::var("WT_PROFILE_ID").ok();
    windows_terminal_background(&text, profile.as_deref())
}

/// `#rrggbb` / `rrggbb` → RGB.
fn parse_hex(value: &str) -> Option<(u8, u8, u8)> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// Backgrounds of the colour schemes Windows Terminal ships; a settings file
/// only lists the schemes the user added or edited.
const BUILTIN_SCHEMES: &[(&str, &str)] = &[
    ("Campbell", "#0C0C0C"),
    ("Campbell Powershell", "#012456"),
    ("Vintage", "#000000"),
    ("One Half Dark", "#282C34"),
    ("One Half Light", "#FAFAFA"),
    ("Solarized Dark", "#002B36"),
    ("Solarized Light", "#FDF6E3"),
    ("Tango Dark", "#000000"),
    ("Tango Light", "#FFFFFF"),
    ("Dark+", "#1E1E1E"),
    ("CGA", "#000000"),
    ("IBM 5153", "#000000"),
    ("Ottosson", "#000000"),
];

/// The scheme a profile falls back to when nothing names one.
const DEFAULT_SCHEME: &str = "Campbell";

/// Background of the profile `profile_id` (else the default profile) in a
/// Windows Terminal `settings.json`: the profile's own `background`, else
/// its colour scheme's, each falling back to `profiles.defaults`.
fn windows_terminal_background(settings: &str, profile_id: Option<&str>) -> Option<(u8, u8, u8)> {
    let settings: Value = serde_json::from_str(&strip_jsonc(settings)).ok()?;
    let profiles = settings.get("profiles")?;
    let defaults = profiles.get("defaults");
    let list = profiles
        .get("list")
        .or(Some(profiles))
        .and_then(Value::as_array)?;
    let wanted = profile_id
        .map(str::to_owned)
        .or_else(|| settings.get("defaultProfile")?.as_str().map(str::to_owned))?;
    let profile = list.iter().find(|profile| {
        profile
            .get("guid")
            .and_then(Value::as_str)
            .is_some_and(|guid| guid.eq_ignore_ascii_case(&wanted))
    });
    let setting = |key: &str| {
        profile
            .and_then(|profile| profile.get(key))
            .or_else(|| defaults.and_then(|defaults| defaults.get(key)))
    };
    if let Some(background) = setting("background")
        .and_then(Value::as_str)
        .and_then(parse_hex)
    {
        return Some(background);
    }
    let light_app = settings.get("theme").and_then(Value::as_str) == Some("light");
    let scheme = match setting("colorScheme") {
        Some(Value::String(name)) => name.clone(),
        // `{ "light": …, "dark": … }` follows the app theme.
        Some(Value::Object(pair)) => pair
            .get(if light_app { "light" } else { "dark" })
            .and_then(Value::as_str)?
            .to_string(),
        _ => DEFAULT_SCHEME.to_string(),
    };
    let custom = settings
        .get("schemes")
        .and_then(Value::as_array)
        .and_then(|schemes| {
            schemes
                .iter()
                .find(|entry| entry.get("name").and_then(Value::as_str) == Some(scheme.as_str()))
        })
        .and_then(|entry| entry.get("background")?.as_str().and_then(parse_hex));
    custom.or_else(|| {
        BUILTIN_SCHEMES
            .iter()
            .find(|(name, _)| *name == scheme)
            .and_then(|(_, background)| parse_hex(background))
    })
}

/// Windows Terminal's settings are JSON with comments and trailing commas.
/// Drop both, leaving string contents alone.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            match ch {
                '\\' => {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '/' if chars.peek() == Some(&'/') => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
            }
            ',' => {
                // A trailing comma: the next significant character closes.
                let rest = chars.clone().find(|c| !c.is_whitespace());
                if !matches!(rest, Some('}' | ']')) {
                    out.push(ch);
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Where the running Windows Terminal keeps its `settings.json`, by how it
/// was installed: portable (a `.portable` file beside the exe, as scoop
/// does), packaged (under `WindowsApps`), or unpackaged.
fn settings_path_for_exe(exe: &Path, local_app_data: &Path) -> PathBuf {
    let dir = exe.parent().unwrap_or(Path::new(""));
    if dir.join(".portable").exists() {
        return dir.join("settings").join("settings.json");
    }
    // `…\WindowsApps\Microsoft.WindowsTerminal_1.21.…_x64__8wekyb3d8bbwe\`:
    // the package family is the name before the version plus the publisher.
    let package = dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|_| dir.components().any(|c| c.as_os_str() == "WindowsApps"));
    if let Some(package) = package {
        if let (Some(name), Some(publisher)) =
            (package.split('_').next(), package.rsplit('_').next())
        {
            return local_app_data
                .join("Packages")
                .join(format!("{name}_{publisher}"))
                .join("LocalState")
                .join("settings.json");
        }
    }
    local_app_data
        .join("Microsoft")
        .join("Windows Terminal")
        .join("settings.json")
}

#[cfg(windows)]
mod windows_terminal {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// Ancestors walked looking for the terminal; shells nest a few deep.
    const MAX_DEPTH: usize = 16;

    pub(super) fn settings_path() -> Option<PathBuf> {
        let exe = terminal_exe()?;
        let local_app_data = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
        Some(super::settings_path_for_exe(&exe, &local_app_data))
    }

    /// Path of the `WindowsTerminal.exe` this process runs under.
    fn terminal_exe() -> Option<PathBuf> {
        let processes = snapshot()?;
        let mut pid = std::process::id();
        for _ in 0..MAX_DEPTH {
            let (parent, _) = processes.get(&pid)?;
            let (_, name) = processes.get(parent)?;
            if name.eq_ignore_ascii_case("WindowsTerminal.exe") {
                return image_path(*parent);
            }
            pid = *parent;
        }
        None
    }

    /// pid → (parent pid, image name) for every process.
    fn snapshot() -> Option<HashMap<u32, (u32, String)>> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: PROCESSENTRY32W is plain data; dwSize tells the API the
        // struct size it may write, and the handle stays open for the walk.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut processes = HashMap::new();
        let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while has_entry {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            processes.insert(
                entry.th32ProcessID,
                (
                    entry.th32ParentProcessID,
                    String::from_utf16_lossy(&entry.szExeFile[..len]),
                ),
            );
            has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        unsafe {
            CloseHandle(snapshot);
        }
        Some(processes)
    }

    fn image_path(pid: u32) -> Option<PathBuf> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle == 0 {
            return None;
        }
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        // SAFETY: `len` holds the buffer's capacity in u16s; on success the
        // API writes at most that many and stores the length it wrote.
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut len) };
        unsafe {
            CloseHandle(handle);
        }
        (ok != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize])))
    }
}

#[cfg(not(windows))]
mod windows_terminal {
    pub(super) fn settings_path() -> Option<std::path::PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTINGS: &str = r##"{
        // comment
        "defaultProfile": "{61c54bbd-c2c6-5271-96e7-009a87ff44bf}",
        "theme": "light",
        "profiles": {
            "defaults": { "colorScheme": "Solarized Dark", },
            "list": [
                { "guid": "{61c54bbd-c2c6-5271-96e7-009a87ff44bf}", "colorScheme": "Solarized Light" },
                { "guid": "{0caa0dad-35be-5f56-a8ff-afceeeaa6101}" },
                { "guid": "{11111111-0000-0000-0000-000000000000}", "background": "#123456" },
                { "guid": "{22222222-0000-0000-0000-000000000000}", "colorScheme": "Mine" },
                { "guid": "{33333333-0000-0000-0000-000000000000}",
                  "colorScheme": { "light": "Tango Light", "dark": "Tango Dark" } },
            ],
        },
        /* block */
        "schemes": [ { "name": "Mine", "background": "#0A0B0C" } ],
    }"##;

    #[test]
    fn the_named_profile_uses_its_scheme() {
        assert_eq!(
            windows_terminal_background(SETTINGS, Some("{61C54BBD-C2C6-5271-96E7-009A87FF44BF}")),
            Some((0xFD, 0xF6, 0xE3))
        );
    }

    #[test]
    fn a_profile_without_a_scheme_inherits_the_defaults() {
        assert_eq!(
            windows_terminal_background(SETTINGS, Some("{0caa0dad-35be-5f56-a8ff-afceeeaa6101}")),
            Some((0x00, 0x2B, 0x36))
        );
    }

    #[test]
    fn an_explicit_background_and_a_custom_scheme_win() {
        assert_eq!(
            windows_terminal_background(SETTINGS, Some("{11111111-0000-0000-0000-000000000000}")),
            Some((0x12, 0x34, 0x56))
        );
        assert_eq!(
            windows_terminal_background(SETTINGS, Some("{22222222-0000-0000-0000-000000000000}")),
            Some((0x0A, 0x0B, 0x0C))
        );
    }

    #[test]
    fn a_light_dark_scheme_pair_follows_the_app_theme() {
        assert_eq!(
            windows_terminal_background(SETTINGS, Some("{33333333-0000-0000-0000-000000000000}")),
            Some((0xFF, 0xFF, 0xFF))
        );
    }

    #[test]
    fn no_profile_id_falls_back_to_the_default_profile() {
        assert_eq!(
            windows_terminal_background(SETTINGS, None),
            Some((0xFD, 0xF6, 0xE3))
        );
    }

    #[test]
    fn jsonc_stripping_keeps_strings_intact() {
        let stripped = strip_jsonc(r#"{"a": "x // y, ]", /* c */ "b": [1, 2,],}"#);
        let value: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(value["a"], "x // y, ]");
        assert_eq!(value["b"], serde_json::json!([1, 2]));
    }

    // Windows Terminal and its paths exist only on Windows; elsewhere `Path`
    // reads `C:\…` as one relative component and nothing here applies.
    #[cfg(windows)]
    #[test]
    fn install_kind_picks_the_settings_file() {
        let local = Path::new(r"C:\Users\u\AppData\Local");
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".portable"), "").unwrap();
        assert_eq!(
            settings_path_for_exe(&tmp.path().join("WindowsTerminal.exe"), local),
            tmp.path().join("settings").join("settings.json")
        );
        let packaged = Path::new(
            r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal_1.21.2361.0_x64__8wekyb3d8bbwe\WindowsTerminal.exe",
        );
        assert_eq!(
            settings_path_for_exe(packaged, local),
            local
                .join(r"Packages\Microsoft.WindowsTerminal_8wekyb3d8bbwe\LocalState\settings.json")
        );
        assert_eq!(
            settings_path_for_exe(Path::new(r"C:\tools\wt\WindowsTerminal.exe"), local),
            local.join(r"Microsoft\Windows Terminal\settings.json")
        );
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("#fdf6e3"), Some((0xFD, 0xF6, 0xE3)));
        assert_eq!(parse_hex("FDF6E3"), Some((0xFD, 0xF6, 0xE3)));
        assert_eq!(parse_hex("#fff"), None);
    }
}
