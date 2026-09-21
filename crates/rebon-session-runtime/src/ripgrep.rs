//! Ripgrep executable resolution, shared by the file scanner, global search and `/doctor`.
//!
//! Prefer a bundled sidecar `rg[.exe]` next to the running binary,
//! allow users to request system ripgrep with `USE_BUILTIN_RIPGREP=0/false`,
//! and fall back to `PATH` when no bundled binary exists.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RipgrepMode {
    Bundled,
    System,
    Override,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RipgrepCommand {
    pub program: PathBuf,
    pub mode: RipgrepMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RipgrepNotFound {
    pub checked_bundled_paths: Vec<PathBuf>,
}

impl std::fmt::Display for RipgrepNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let checked_bundled = if self.checked_bundled_paths.is_empty() {
            "no bundled sidecar/npm payload paths were available to check".to_string()
        } else {
            format!(
                "bundled sidecar/npm payload paths: {}",
                self.checked_bundled_paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        write!(
            f,
            "rg not found (checked REBON_RIPGREP_PATH override, {checked_bundled}, and PATH). File scanning can fall back to Rebon's native scanner when available; cargo-install users who want faster scans or rg-backed global text search should install ripgrep or set REBON_RIPGREP_PATH to an rg executable."
        )
    }
}

impl std::error::Error for RipgrepNotFound {}

pub const RIPGREP_ACTIONABLE_GUIDANCE: &str =
    "Install ripgrep or set REBON_RIPGREP_PATH to an rg executable; @ file scanning can use Rebon's native fallback, but global text search requires rg.";

pub fn resolve_ripgrep_command() -> Result<RipgrepCommand, RipgrepNotFound> {
    let current_exe = env::current_exe().ok();
    resolve_ripgrep_command_with(current_exe.as_deref())
}

fn resolve_ripgrep_command_with(
    current_exe: Option<&Path>,
) -> Result<RipgrepCommand, RipgrepNotFound> {
    resolve_ripgrep_command_inner(
        current_exe,
        env::var_os("PATH"),
        env::var_os("PATHEXT"),
        env::var_os("REBON_RIPGREP_PATH").map(PathBuf::from),
        env_defined_falsy("USE_BUILTIN_RIPGREP"),
    )
}

fn resolve_ripgrep_command_inner(
    current_exe: Option<&Path>,
    path_env: Option<OsString>,
    pathext_env: Option<OsString>,
    override_path: Option<PathBuf>,
    wants_system: bool,
) -> Result<RipgrepCommand, RipgrepNotFound> {
    if let Some(override_path) = override_path {
        if override_path.is_file() {
            return Ok(RipgrepCommand {
                program: override_path,
                mode: RipgrepMode::Override,
            });
        }
    }

    if wants_system {
        if let Some(program) = find_on_path_in("rg", path_env.as_ref(), pathext_env.as_ref()) {
            return Ok(RipgrepCommand {
                program,
                mode: RipgrepMode::System,
            });
        }
    }

    let bundled_paths = current_exe.map(candidate_bundled_paths).unwrap_or_default();
    for path in &bundled_paths {
        if path.is_file() {
            return Ok(RipgrepCommand {
                program: path.clone(),
                mode: RipgrepMode::Bundled,
            });
        }
    }

    if !wants_system {
        if let Some(program) = find_on_path_in("rg", path_env.as_ref(), pathext_env.as_ref()) {
            return Ok(RipgrepCommand {
                program,
                mode: RipgrepMode::System,
            });
        }
    }

    Err(RipgrepNotFound {
        checked_bundled_paths: bundled_paths,
    })
}

fn env_defined_falsy(name: &str) -> bool {
    let Some(value) = env::var_os(name) else {
        return false;
    };
    let value = value.to_string_lossy().trim().to_ascii_lowercase();
    value.is_empty() || matches!(value.as_str(), "0" | "false" | "no" | "off")
}

fn candidate_bundled_paths(current_exe: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let Some(exe_dir) = current_exe.parent() else {
        return candidates;
    };
    let rg_name = ripgrep_binary_name();
    let platform_key = platform_key();

    // Sidecar beside the executable (native build or npm platform package):
    //   dist/rg[.exe]
    //   package/bin/rg        (Unix)
    //   package/payload/rg.exe or managed-bin/rg.exe (Windows)
    candidates.push(exe_dir.join(rg_name));

    // non-bundled npm layout:
    //   vendor/ripgrep/<arch-platform>/rg[.exe]
    candidates.push(
        exe_dir
            .join("vendor")
            .join("ripgrep")
            .join(&platform_key)
            .join(rg_name),
    );
    if let Some(parent) = exe_dir.parent() {
        candidates.push(
            parent
                .join("vendor")
                .join("ripgrep")
                .join(&platform_key)
                .join(rg_name),
        );
        candidates.push(parent.join("payload").join(rg_name));
    }

    candidates
}

fn ripgrep_binary_name() -> &'static str {
    if cfg!(windows) {
        "rg.exe"
    } else {
        "rg"
    }
}

fn platform_key() -> String {
    let arch = match env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    let os = match env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    format!("{arch}-{os}")
}

fn find_on_path_in(
    program: &str,
    path_env: Option<&OsString>,
    pathext_env: Option<&OsString>,
) -> Option<PathBuf> {
    let paths = path_env?;
    for dir in env::split_paths(paths) {
        for candidate in path_candidates(&dir, program, pathext_env) {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn path_candidates(dir: &Path, program: &str, pathext_env: Option<&OsString>) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut candidates = Vec::new();
        let program_path = Path::new(program);
        if program_path.extension().is_some() {
            candidates.push(dir.join(program));
            return candidates;
        }
        let pathext = pathext_env
            .cloned()
            .unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
        for ext in pathext.to_string_lossy().split(';') {
            if ext.is_empty() {
                continue;
            }
            candidates.push(dir.join(format!("{program}{}", ext.to_ascii_lowercase())));
            candidates.push(dir.join(format!("{program}{ext}")));
        }
        candidates
    }

    #[cfg(not(windows))]
    {
        // PATHEXT resolution is Windows-only; suffix-less lookup elsewhere.
        let _ = pathext_env;
        vec![dir.join(program)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_bundled_sidecar_over_path() {
        let temp = tempfile::tempdir().unwrap();
        let exe_dir = temp.path().join("app");
        let path_dir = temp.path().join("path");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&path_dir).unwrap();
        let exe = exe_dir.join(if cfg!(windows) { "rebon.exe" } else { "rebon" });
        let bundled = exe_dir.join(ripgrep_binary_name());
        let system = path_dir.join(ripgrep_binary_name());
        std::fs::write(&exe, "").unwrap();
        std::fs::write(&bundled, "").unwrap();
        std::fs::write(&system, "").unwrap();

        let resolved = resolve_ripgrep_command_inner(
            Some(&exe),
            Some(path_dir.as_os_str().to_os_string()),
            Some(OsString::from(".EXE;.CMD")),
            None,
            false,
        )
        .unwrap();

        assert_eq!(resolved.program, bundled);
        assert_eq!(resolved.mode, RipgrepMode::Bundled);
    }

    #[test]
    fn resolve_falls_back_to_path_when_bundled_missing() {
        let temp = tempfile::tempdir().unwrap();
        let exe_dir = temp.path().join("app");
        let path_dir = temp.path().join("path");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&path_dir).unwrap();
        let exe = exe_dir.join(if cfg!(windows) { "rebon.exe" } else { "rebon" });
        let system = path_dir.join(ripgrep_binary_name());
        std::fs::write(&exe, "").unwrap();
        std::fs::write(&system, "").unwrap();

        let resolved = resolve_ripgrep_command_inner(
            Some(&exe),
            Some(path_dir.as_os_str().to_os_string()),
            Some(OsString::from(".EXE;.CMD")),
            None,
            false,
        )
        .unwrap();

        assert_eq!(resolved.program, system);
        assert_eq!(resolved.mode, RipgrepMode::System);
    }

    #[test]
    fn resolve_reports_not_found_with_checked_bundled_paths() {
        let temp = tempfile::tempdir().unwrap();
        let exe_dir = temp.path().join("app");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let exe = exe_dir.join(if cfg!(windows) { "rebon.exe" } else { "rebon" });
        std::fs::write(&exe, "").unwrap();

        let err =
            resolve_ripgrep_command_inner(Some(&exe), Some(OsString::new()), None, None, false)
                .unwrap_err();

        assert!(!err.checked_bundled_paths.is_empty());
        let message = err.to_string();
        assert!(message.contains("rg not found"));
        assert!(message.contains("REBON_RIPGREP_PATH"));
        assert!(message.contains("PATH"));
        assert!(message.contains("bundled sidecar/npm payload paths"));
        assert!(message.contains("native scanner"));
        assert!(message.contains("cargo-install users"));
    }
}
