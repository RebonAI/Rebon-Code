//! The public ACP agent registry
//! (<https://agentclientprotocol.com/get-started/registry>): the agent CLIs
//! it lists, how each one starts on this machine, and installing one as an
//! `acpAgents` entry in `config.json`, where `/backend` and the desktop app
//! find it.
//!
//! - [`Registry`] — the file, as `FORMAT.md` in agentclientprotocol/registry
//!   describes it.
//! - [`fetch`] — reading it, with a copy kept under the config home for when
//!   the network is not there.
//! - [`choose`] — how an agent would start here: a binary to download, or an
//!   npx or uvx package; or why it cannot.
//! - [`install`] — doing it, and taking it out again.
//! - [`cli`] — `rebon agent`.
//!
//! Some agents are never run this way. Their providers allow only their own
//! client, so Rebon offers them in a terminal, in their own CLI
//! ([`terminal_only`]); the registry listing them changes nothing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub mod cli;
pub mod fetch;
pub mod install;

/// The registry file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Registry {
    pub version: String,
    pub agents: Vec<RegistryAgent>,
    /// Entries left out because they did not parse, or named themselves
    /// with characters an id here may not have.
    #[serde(skip)]
    pub skipped: usize,
}

impl Registry {
    /// Parses the registry, keeping every agent that reads and dropping the
    /// rest: one malformed entry upstream must not hide the other forty.
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        #[derive(Deserialize)]
        struct Raw {
            version: String,
            agents: Vec<serde_json::Value>,
        }
        let raw: Raw = serde_json::from_slice(bytes)
            .map_err(|error| anyhow::anyhow!("the ACP registry did not parse: {error}"))?;
        let total = raw.agents.len();
        let agents: Vec<RegistryAgent> = raw
            .agents
            .into_iter()
            .filter_map(|agent| serde_json::from_value::<RegistryAgent>(agent).ok())
            .filter(|agent| valid_id(&agent.id))
            .collect();
        Ok(Self {
            version: raw.version,
            skipped: total - agents.len(),
            agents,
        })
    }

    pub fn agent(&self, id: &str) -> Option<&RegistryAgent> {
        self.agents
            .iter()
            .find(|agent| agent.id.eq_ignore_ascii_case(id.trim()))
    }
}

/// One agent the registry lists.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RegistryAgent {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    pub distribution: Distribution,
}

/// The ways an agent can be had. At least one is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Distribution {
    /// Per platform ([`current_platform`]'s names).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub binary: BTreeMap<String, BinaryTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npx: Option<Package>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uvx: Option<Package>,
}

/// An npm or PyPI package, run through npx or uvx.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Package {
    /// With its version pinned (`pi-acp@0.0.35`, `fast-agent-acp==0.10.66`).
    pub package: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// A download for one platform.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BinaryTarget {
    /// A `.zip`, `.tar.gz`/`.tgz`, `.tar.bz2`/`.tbz2`, or the executable
    /// itself.
    pub archive: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The executable, relative to where the archive unpacks (`./goose`).
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// Registry ids become directory names under the config home, so only
/// these characters are let through.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// An agent Rebon runs only in a terminal, in its own CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TerminalOnly {
    pub name: &'static str,
}

/// Agents whose providers allow only their own client and treat any other
/// one — an ACP adapter included — as a breach of their terms. Matched on the
/// registry id without its `-acp` suffix, so a renamed entry still counts.
const TERMINAL_ONLY: &[(&str, &str)] = &[("antigravity", "Google Antigravity"), ("zcode", "ZCode")];

/// Whether `registry_id` is one of the agents [`TERMINAL_ONLY`] names.
pub fn terminal_only(registry_id: &str) -> Option<TerminalOnly> {
    let id = registry_id.trim().to_ascii_lowercase();
    let base = id.strip_suffix("-acp").unwrap_or(&id);
    TERMINAL_ONLY
        .iter()
        .find(|(name, _)| base == *name || base.starts_with(&format!("{name}-")))
        .map(|(_, display)| TerminalOnly { name: display })
}

/// This machine's name in the registry's binary table, or `None` on a
/// platform it has no name for.
pub fn current_platform() -> Option<&'static str> {
    platform_name(
        std::env::consts::OS,
        std::env::consts::ARCH,
        host_uses_musl(),
    )
}

fn platform_name(os: &str, arch: &str, musl: bool) -> Option<&'static str> {
    Some(match (os, arch, musl) {
        ("macos", "aarch64", _) => "darwin-aarch64",
        ("macos", "x86_64", _) => "darwin-x86_64",
        ("windows", "aarch64", _) => "windows-aarch64",
        ("windows", "x86_64", _) => "windows-x86_64",
        ("linux", "aarch64", false) => "linux-aarch64",
        ("linux", "x86_64", false) => "linux-x86_64",
        ("linux", "aarch64", true) => "linux-aarch64-musl",
        ("linux", "x86_64", true) => "linux-x86_64-musl",
        _ => return None,
    })
}

/// The host's libc, not the one this binary was built for: a musl build of
/// rebon on a glibc system still wants the glibc agents. The registry asks
/// for this, and the musl loader's presence is how to tell.
fn host_uses_musl() -> bool {
    cfg!(target_os = "linux")
        && std::fs::read_dir("/lib").is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("ld-musl-"))
        })
}

/// How an agent starts here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// Download this and run what it unpacks.
    Binary(BinaryTarget),
    Npx(Package),
    Uvx(Package),
}

impl Method {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Binary(_) => "binary",
            Self::Npx(_) => "npx",
            Self::Uvx(_) => "uvx",
        }
    }
}

/// Why an agent cannot be installed here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "camelCase")]
pub enum Unavailable {
    /// Its provider allows only its own CLI.
    TerminalOnly { name: &'static str },
    /// Nothing for this platform.
    NotForThisPlatform,
    /// Only as a package, and the program that runs it is missing.
    NeedsProgram { program: &'static str },
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TerminalOnly { name } => write!(
                f,
                "{name} runs only in its own CLI, in a terminal: its provider does not allow other clients"
            ),
            Self::NotForThisPlatform => write!(f, "the registry has no build of it for this machine"),
            Self::NeedsProgram { program: "npx" } => {
                write!(f, "it is an npm package, and npx is not on PATH (install Node.js)")
            }
            Self::NeedsProgram { program } => {
                write!(f, "it is a Python package, and {program} is not on PATH (install uv)")
            }
        }
    }
}

/// How `agent` would start on `platform`: a binary when there is one for
/// it, since that needs nothing else installed, else an npx package, else a
/// uvx one, each only when `has_program` finds the program that runs it.
pub fn choose(
    agent: &RegistryAgent,
    platform: Option<&str>,
    has_program: impl Fn(&str) -> bool,
) -> Result<Method, Unavailable> {
    if let Some(terminal) = terminal_only(&agent.id) {
        return Err(Unavailable::TerminalOnly {
            name: terminal.name,
        });
    }
    let distribution = &agent.distribution;
    if let Some(target) = platform.and_then(|platform| distribution.binary.get(platform)) {
        return Ok(Method::Binary(target.clone()));
    }
    if let Some(package) = &distribution.npx {
        if has_program("npx") {
            return Ok(Method::Npx(package.clone()));
        }
    }
    if let Some(package) = &distribution.uvx {
        if has_program("uvx") {
            return Ok(Method::Uvx(package.clone()));
        }
    }
    Err(match (&distribution.npx, &distribution.uvx) {
        (Some(_), _) => Unavailable::NeedsProgram { program: "npx" },
        (None, Some(_)) => Unavailable::NeedsProgram { program: "uvx" },
        (None, None) => Unavailable::NotForThisPlatform,
    })
}

/// The id an agent gets in `acpAgents` unless told otherwise: the
/// registry's, lowercased, without the `-acp` most adapters carry
/// (`pi-acp` → `pi`), so `/backend pi` reads the way the user says it.
pub fn default_id(registry_id: &str) -> String {
    let id = registry_id.trim().to_ascii_lowercase();
    match id.strip_suffix("-acp") {
        Some(base) if !base.is_empty() && base != rebon_config::LOCAL_AGENT_ID => base.to_string(),
        _ => id,
    }
}

/// Whether `program` is on PATH, as the spawn of an `acpAgents` command
/// will look for it: with `PATHEXT`'s extensions on Windows.
pub fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&path).any(|dir| {
        extensions
            .iter()
            .any(|extension| dir.join(format!("{program}{extension}")).is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": "1.0.0",
            "agents": [
                {
                    "id": "pi-acp", "name": "pi ACP", "version": "0.0.35",
                    "description": "ACP adapter for pi coding agent",
                    "distribution": { "npx": { "package": "pi-acp@0.0.35" } },
                    "aNewField": true
                },
                {
                    "id": "goose", "name": "goose", "version": "1.61.1",
                    "distribution": {
                        "binary": {
                            "darwin-aarch64": {
                                "archive": "https://example.invalid/goose.tar.bz2",
                                "sha256": "00", "cmd": "./goose", "args": ["acp"]
                            }
                        }
                    }
                },
                {
                    "id": "antigravity-acp", "name": "Google Antigravity", "version": "1.5.2",
                    "distribution": { "npx": { "package": "antigravity@1.5.2" } }
                },
                {
                    "id": "fast-agent", "name": "fast-agent", "version": "0.10.66",
                    "distribution": { "uvx": { "package": "fast-agent-acp==0.10.66", "args": ["-x"] } }
                },
                { "id": "broken", "name": "missing its version and distribution" },
                {
                    "id": "../escape", "name": "x", "version": "1",
                    "distribution": { "npx": { "package": "x" } }
                }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn the_registry_keeps_every_entry_that_reads_and_counts_the_rest() {
        let registry = Registry::parse(&sample()).unwrap();
        let ids: Vec<&str> = registry.agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["pi-acp", "goose", "antigravity-acp", "fast-agent"]);
        assert_eq!(registry.skipped, 2);
        assert_eq!(registry.agent("PI-ACP").unwrap().version, "0.0.35");
        assert!(Registry::parse(b"[]").is_err());
    }

    #[test]
    fn antigravity_and_zcode_are_terminal_only_under_any_spelling() {
        for id in [
            "antigravity-acp",
            "antigravity",
            "Antigravity-CLI",
            "zcode",
            "zcode-acp",
        ] {
            assert!(terminal_only(id).is_some(), "{id}");
        }
        for id in ["pi-acp", "goose", "antigravityx", "z-code"] {
            assert!(terminal_only(id).is_none(), "{id}");
        }
    }

    #[test]
    fn a_binary_wins_then_npx_then_uvx_and_a_missing_runtime_is_named() {
        let registry = Registry::parse(&sample()).unwrap();
        let all = |_: &str| true;
        let none = |_: &str| false;

        let goose = registry.agent("goose").unwrap();
        assert_eq!(
            choose(goose, Some("darwin-aarch64"), none).unwrap().kind(),
            "binary"
        );
        assert_eq!(
            choose(goose, Some("windows-x86_64"), all),
            Err(Unavailable::NotForThisPlatform)
        );

        let pi = registry.agent("pi-acp").unwrap();
        assert_eq!(
            choose(pi, Some("windows-x86_64"), all).unwrap().kind(),
            "npx"
        );
        assert_eq!(
            choose(pi, Some("windows-x86_64"), none),
            Err(Unavailable::NeedsProgram { program: "npx" })
        );

        let fast = registry.agent("fast-agent").unwrap();
        assert_eq!(choose(fast, None, all).unwrap().kind(), "uvx");
        assert_eq!(
            choose(fast, None, none),
            Err(Unavailable::NeedsProgram { program: "uvx" })
        );

        let agy = registry.agent("antigravity-acp").unwrap();
        assert!(matches!(
            choose(agy, Some("windows-x86_64"), all),
            Err(Unavailable::TerminalOnly { .. })
        ));
    }

    #[test]
    fn platforms_follow_the_registry_names_and_musl_never_falls_back_to_glibc() {
        assert_eq!(
            platform_name("macos", "aarch64", false),
            Some("darwin-aarch64")
        );
        assert_eq!(
            platform_name("windows", "x86_64", false),
            Some("windows-x86_64")
        );
        assert_eq!(
            platform_name("linux", "x86_64", false),
            Some("linux-x86_64")
        );
        assert_eq!(
            platform_name("linux", "x86_64", true),
            Some("linux-x86_64-musl")
        );
        assert_eq!(platform_name("freebsd", "x86_64", false), None);
    }

    #[test]
    fn the_default_id_drops_the_adapter_suffix_but_never_becomes_local() {
        assert_eq!(default_id("pi-acp"), "pi");
        assert_eq!(default_id("Claude-ACP"), "claude");
        assert_eq!(default_id("goose"), "goose");
        assert_eq!(default_id("local-acp"), "local-acp");
        assert_eq!(default_id("-acp"), "-acp");
    }
}
