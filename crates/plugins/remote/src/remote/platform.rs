//! Which Rebon build the far end needs.
//!
//! Named after npm's `os`/`cpu` vocabulary rather than Rust target
//! triples on purpose: the artifacts that actually exist are the
//! scoped platform packages `@rebon/cli-<os>-<cpu>` that
//! `scripts/build_npm_package.py` publishes, and inventing a second
//! naming scheme here would mean maintaining a mapping between them
//! forever.

use std::fmt;

/// The five platforms Rebon publishes binaries for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemotePlatform {
    LinuxX64,
    LinuxArm64,
    DarwinX64,
    DarwinArm64,
    Win32X64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlatformError {
    #[error("unsupported remote operating system `{0}` — Rebon publishes linux, macOS, and Windows builds")]
    UnsupportedOs(String),
    #[error(
        "unsupported remote architecture `{arch}` on {os} — Rebon publishes x64 and arm64 builds"
    )]
    UnsupportedArch { os: String, arch: String },
    #[error(
        "could not read the remote platform from `{0}` — expected `uname -s` and `uname -m` output"
    )]
    Unreadable(String),
}

impl RemotePlatform {
    /// Map `uname -s` / `uname -m` onto a published platform.
    ///
    /// Both arguments are lowercased and trimmed here rather than at
    /// the call site, because they arrive straight off a remote
    /// command's stdout with whatever line endings that shell used.
    pub fn from_uname(sysname: &str, machine: &str) -> Result<Self, PlatformError> {
        let os = sysname.trim().to_ascii_lowercase();
        let arch = machine.trim().to_ascii_lowercase();
        if os.is_empty() || arch.is_empty() {
            return Err(PlatformError::Unreadable(format!("{sysname} {machine}")));
        }

        // MSYS/Cygwin/Git-Bash on a Windows host report things like
        // `MINGW64_NT-10.0-19044`. They are still Windows, and the
        // Windows build is what will run.
        let is_windows = os.contains("mingw")
            || os.contains("msys")
            || os.contains("cygwin")
            || os.contains("windows")
            || os.contains("_nt-");

        match (os.as_str(), is_windows) {
            (_, true) => match arch.as_str() {
                "x86_64" | "amd64" | "x64" => Ok(Self::Win32X64),
                other => Err(PlatformError::UnsupportedArch {
                    os: "windows".to_string(),
                    arch: other.to_string(),
                }),
            },
            ("linux", _) => match arch.as_str() {
                "x86_64" | "amd64" | "x64" => Ok(Self::LinuxX64),
                "aarch64" | "arm64" => Ok(Self::LinuxArm64),
                other => Err(PlatformError::UnsupportedArch {
                    os: "linux".to_string(),
                    arch: other.to_string(),
                }),
            },
            ("darwin", _) => match arch.as_str() {
                "x86_64" | "amd64" | "x64" => Ok(Self::DarwinX64),
                "aarch64" | "arm64" => Ok(Self::DarwinArm64),
                other => Err(PlatformError::UnsupportedArch {
                    os: "darwin".to_string(),
                    arch: other.to_string(),
                }),
            },
            (other, _) => Err(PlatformError::UnsupportedOs(other.to_string())),
        }
    }

    /// Parse the two-line output of the probe script (`uname -s` then
    /// `uname -m`), tolerating CRLF and blank padding.
    pub fn from_probe_output(stdout: &str) -> Result<Self, PlatformError> {
        let mut lines = stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty());
        let sysname = lines
            .next()
            .ok_or_else(|| PlatformError::Unreadable(stdout.to_string()))?;
        let machine = lines
            .next()
            .ok_or_else(|| PlatformError::Unreadable(stdout.to_string()))?;
        Self::from_uname(sysname, machine)
    }

    pub fn npm_os(self) -> &'static str {
        match self {
            Self::LinuxX64 | Self::LinuxArm64 => "linux",
            Self::DarwinX64 | Self::DarwinArm64 => "darwin",
            Self::Win32X64 => "win32",
        }
    }

    pub fn npm_cpu(self) -> &'static str {
        match self {
            Self::LinuxX64 | Self::DarwinX64 | Self::Win32X64 => "x64",
            Self::LinuxArm64 | Self::DarwinArm64 => "arm64",
        }
    }

    /// The scoped platform package that carries this build.
    pub fn package_name(self) -> String {
        format!("@rebon/cli-{}-{}", self.npm_os(), self.npm_cpu())
    }

    /// Registry tarball URL for a version.
    ///
    /// npm's layout drops the scope from the filename, so
    /// `@rebon/cli-linux-x64@0.15.0` lives at
    /// `…/@rebon/cli-linux-x64/-/cli-linux-x64-0.15.0.tgz`. Getting
    /// this wrong is a 404 at install time, which is why the shape is
    /// pinned by a test.
    pub fn tarball_url(self, registry: &str, version: &str) -> String {
        let registry = registry.trim_end_matches('/');
        let unscoped = format!("cli-{}-{}", self.npm_os(), self.npm_cpu());
        format!("{registry}/@rebon/{unscoped}/-/{unscoped}-{version}.tgz")
    }

    /// Name of the executable inside the package's `bin/`.
    pub fn binary_name(self) -> &'static str {
        match self {
            Self::Win32X64 => "rebon.exe",
            _ => "rebon",
        }
    }

    /// Whether the far end runs a POSIX shell, which is what every
    /// remote script this crate generates assumes.
    ///
    /// A Windows server reached over OpenSSH answers `cmd.exe` by
    /// default, so the scripts would not run. Detected rather than
    /// attempted: a half-executed install is worse than a refusal
    /// that names the reason.
    pub fn has_posix_shell(self) -> bool {
        !matches!(self, Self::Win32X64)
    }

    /// The platform this Rebon is running on, when it is one Rebon
    /// publishes. Used to decide whether the local binary can be
    /// pushed to the far end as-is.
    pub fn local() -> Option<Self> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Some(Self::LinuxX64),
            ("linux", "aarch64") => Some(Self::LinuxArm64),
            ("macos", "x86_64") => Some(Self::DarwinX64),
            ("macos", "aarch64") => Some(Self::DarwinArm64),
            ("windows", "x86_64") => Some(Self::Win32X64),
            _ => None,
        }
    }
}

impl fmt::Display for RemotePlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.npm_os(), self.npm_cpu())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_and_macos_map_both_architectures() {
        assert_eq!(
            RemotePlatform::from_uname("Linux", "x86_64").unwrap(),
            RemotePlatform::LinuxX64
        );
        assert_eq!(
            RemotePlatform::from_uname("Linux", "aarch64").unwrap(),
            RemotePlatform::LinuxArm64
        );
        assert_eq!(
            RemotePlatform::from_uname("Darwin", "arm64").unwrap(),
            RemotePlatform::DarwinArm64
        );
        assert_eq!(
            RemotePlatform::from_uname("Darwin", "x86_64").unwrap(),
            RemotePlatform::DarwinX64
        );
    }

    #[test]
    fn arch_aliases_collapse() {
        // FreeBSD-style `amd64` and npm-style `x64` mean the same
        // machine as `x86_64`; a miss here sends the wrong binary.
        for machine in ["x86_64", "amd64", "x64", "X86_64"] {
            assert_eq!(
                RemotePlatform::from_uname("Linux", machine).unwrap(),
                RemotePlatform::LinuxX64,
                "machine {machine}"
            );
        }
        for machine in ["aarch64", "arm64", "ARM64"] {
            assert_eq!(
                RemotePlatform::from_uname("linux", machine).unwrap(),
                RemotePlatform::LinuxArm64,
                "machine {machine}"
            );
        }
    }

    #[test]
    fn git_bash_on_windows_is_recognised_as_windows() {
        assert_eq!(
            RemotePlatform::from_uname("MINGW64_NT-10.0-19044", "x86_64").unwrap(),
            RemotePlatform::Win32X64
        );
        assert_eq!(
            RemotePlatform::from_uname("CYGWIN_NT-10.0", "x86_64").unwrap(),
            RemotePlatform::Win32X64
        );
    }

    #[test]
    fn unsupported_os_and_arch_are_distinct_errors() {
        assert!(matches!(
            RemotePlatform::from_uname("FreeBSD", "x86_64").unwrap_err(),
            PlatformError::UnsupportedOs(_)
        ));
        assert!(matches!(
            RemotePlatform::from_uname("Linux", "riscv64").unwrap_err(),
            PlatformError::UnsupportedArch { .. }
        ));
    }

    #[test]
    fn probe_output_tolerates_crlf_and_blank_lines() {
        let platform = RemotePlatform::from_probe_output("Linux\r\nx86_64\r\n").unwrap();
        assert_eq!(platform, RemotePlatform::LinuxX64);
        let padded = RemotePlatform::from_probe_output("\n Darwin \n\n arm64 \n").unwrap();
        assert_eq!(padded, RemotePlatform::DarwinArm64);
    }

    #[test]
    fn truncated_probe_output_is_an_error_not_a_guess() {
        assert!(matches!(
            RemotePlatform::from_probe_output("Linux\n").unwrap_err(),
            PlatformError::Unreadable(_)
        ));
        assert!(matches!(
            RemotePlatform::from_probe_output("").unwrap_err(),
            PlatformError::Unreadable(_)
        ));
    }

    #[test]
    fn package_name_matches_the_published_scope() {
        // Mirrors scripts/build_npm_package.py's
        // `scoped_platform_package_name`.
        assert_eq!(
            RemotePlatform::LinuxX64.package_name(),
            "@rebon/cli-linux-x64"
        );
        assert_eq!(
            RemotePlatform::DarwinArm64.package_name(),
            "@rebon/cli-darwin-arm64"
        );
    }

    #[test]
    fn tarball_url_drops_the_scope_from_the_filename() {
        assert_eq!(
            RemotePlatform::LinuxArm64.tarball_url("https://registry.npmjs.org", "0.15.0"),
            "https://registry.npmjs.org/@rebon/cli-linux-arm64/-/cli-linux-arm64-0.15.0.tgz"
        );
    }

    #[test]
    fn a_trailing_slash_on_the_registry_does_not_double_up() {
        assert_eq!(
            RemotePlatform::LinuxX64.tarball_url("https://registry.npmjs.org/", "1.0.0"),
            "https://registry.npmjs.org/@rebon/cli-linux-x64/-/cli-linux-x64-1.0.0.tgz"
        );
    }

    #[test]
    fn only_windows_lacks_a_posix_shell() {
        assert!(RemotePlatform::LinuxX64.has_posix_shell());
        assert!(RemotePlatform::DarwinArm64.has_posix_shell());
        assert!(!RemotePlatform::Win32X64.has_posix_shell());
        assert_eq!(RemotePlatform::Win32X64.binary_name(), "rebon.exe");
        assert_eq!(RemotePlatform::LinuxX64.binary_name(), "rebon");
    }
}
