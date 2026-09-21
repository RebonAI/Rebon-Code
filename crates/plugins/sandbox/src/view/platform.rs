//! `SandboxPlatform` enum and the platform predicates the sandbox UI
//! uses to decide what it offers and how it words its empty states.
//!
//! ## Behaviour notes
//!
//! The enum is a wire round-trip over four literal strings — `"macos"`,
//! `"linux"`, `"windows"` and `"unknown"` — with three predicates
//! derived from it:
//!
//! ```text
//! is_supported() -> macos || linux
//! is_mac()       -> macos
//! is_linux()     -> linux
//! ```
//!
//! ## Pinned rules
//!
//! 1. **The wire strings are `"macos"`, `"linux"`, `"windows"`,
//! `"unknown"` and nothing else.** [`SandboxPlatform::from_wire`]
//! accepts the first three literally and maps everything else —
//! `""`, `"freebsd"`, `"MacOS"` — to [`SandboxPlatform::Unknown`].
//! 2. **`is_supported` is `macos || linux`.** This is the set of
//! platforms the `/sandbox` UI offers; Windows and unknown are NOT in
//! it. Whether a command can actually be wrapped here is a separate
//! question, answered by `crate::runtime::has_backend`, which does
//! include Windows.
//! 3. **`is_mac` picks the install hint.** Blocked-operation reporting
//! and the `ripgrep` install hint differ by platform: macOS gets
//! `"brew install ripgrep"`, everything else `"apt install ripgrep"`.
//! 4. **`is_linux` words the empty violations tab.** Blocked-operation
//! reporting is macOS-only, so on Linux the tab says bubblewrap fails
//! the command instead, and on any other non-macOS platform it says
//! the reporting is macOS-only.

/// Platform the sandbox runtime is running on. The wire strings are
/// user-observable — they appear in error messages and in the doctor
/// output — so they are pinned literally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxPlatform {
    /// `"macos"` — uses Apple seatbelt as the sandbox primitive.
    Macos,
    /// `"linux"` — uses bubblewrap + seccomp as the sandbox primitive.
    Linux,
    /// `"windows"` — wrapped by the `sandbox-win` helper backend, but
    /// not offered by the `/sandbox` UI.
    Windows,
    /// `"unknown"` — fallback for any platform string that is not one
    /// of the above. Treated as unsupported.
    Unknown,
}

impl SandboxPlatform {
    /// Wire string. Pinned literally; do NOT change without bumping
    /// the consumer.
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Windows => "windows",
            Self::Unknown => "unknown",
        }
    }

    /// Parse the wire string. Returns `Unknown` for any unrecognised
    /// input — the fallback arm of the match.
    pub fn from_wire(s: &str) -> Self {
        match s {
            "macos" => Self::Macos,
            "linux" => Self::Linux,
            "windows" => Self::Windows,
            _ => Self::Unknown,
        }
    }

    /// `true` for macOS and Linux only — the platforms the `/sandbox`
    /// UI offers.
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Macos | Self::Linux)
    }

    /// `true` for macOS only. Used to choose the install hint
    /// (`brew install ripgrep` vs `apt install ripgrep`).
    pub fn is_mac(&self) -> bool {
        matches!(self, Self::Macos)
    }

    /// `true` for Linux only. Used to word the empty violations tab:
    /// bubblewrap fails the command instead of reporting it.
    pub fn is_linux(&self) -> bool {
        matches!(self, Self::Linux)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_wire_round_trip() {
        assert_eq!(SandboxPlatform::Macos.as_wire(), "macos");
        assert_eq!(SandboxPlatform::from_wire("macos"), SandboxPlatform::Macos);
    }

    #[test]
    fn linux_wire_round_trip() {
        assert_eq!(SandboxPlatform::Linux.as_wire(), "linux");
        assert_eq!(SandboxPlatform::from_wire("linux"), SandboxPlatform::Linux);
    }

    #[test]
    fn windows_wire_round_trip() {
        assert_eq!(SandboxPlatform::Windows.as_wire(), "windows");
        assert_eq!(
            SandboxPlatform::from_wire("windows"),
            SandboxPlatform::Windows
        );
    }

    #[test]
    fn unknown_wire_round_trip() {
        assert_eq!(SandboxPlatform::Unknown.as_wire(), "unknown");
        assert_eq!(
            SandboxPlatform::from_wire("unknown"),
            SandboxPlatform::Unknown
        );
    }

    #[test]
    fn empty_wire_is_unknown() {
        assert_eq!(SandboxPlatform::from_wire(""), SandboxPlatform::Unknown);
    }

    #[test]
    fn garbage_wire_is_unknown() {
        assert_eq!(
            SandboxPlatform::from_wire("freebsd"),
            SandboxPlatform::Unknown
        );
        assert_eq!(
            SandboxPlatform::from_wire("MacOS"),
            SandboxPlatform::Unknown
        ); // case-sensitive
    }

    #[test]
    fn supported_includes_mac_and_linux() {
        assert!(SandboxPlatform::Macos.is_supported());
        assert!(SandboxPlatform::Linux.is_supported());
    }

    #[test]
    fn supported_excludes_windows_and_unknown() {
        assert!(!SandboxPlatform::Windows.is_supported());
        assert!(!SandboxPlatform::Unknown.is_supported());
    }

    #[test]
    fn is_mac_only_macos() {
        assert!(SandboxPlatform::Macos.is_mac());
        assert!(!SandboxPlatform::Linux.is_mac());
        assert!(!SandboxPlatform::Windows.is_mac());
        assert!(!SandboxPlatform::Unknown.is_mac());
    }

    #[test]
    fn is_linux_only_linux() {
        assert!(!SandboxPlatform::Macos.is_linux());
        assert!(SandboxPlatform::Linux.is_linux());
        assert!(!SandboxPlatform::Windows.is_linux());
        assert!(!SandboxPlatform::Unknown.is_linux());
    }

    #[test]
    fn platform_predicates_table() {
        // Each row pins all three predicates for one platform.
        //
        // (platform, supported, is_mac, is_linux)
        let table = [
            (SandboxPlatform::Macos, true, true, false),
            (SandboxPlatform::Linux, true, false, true),
            (SandboxPlatform::Windows, false, false, false),
            (SandboxPlatform::Unknown, false, false, false),
        ];
        for (p, sup, mac, linux) in table {
            assert_eq!(p.is_supported(), sup, "is_supported({:?})", p);
            assert_eq!(p.is_mac(), mac, "is_mac({:?})", p);
            assert_eq!(p.is_linux(), linux, "is_linux({:?})", p);
        }
    }
}
