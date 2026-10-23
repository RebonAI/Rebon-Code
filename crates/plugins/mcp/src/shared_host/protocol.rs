//! What the host and the sessions say to each other, and where the host is.
//!
//! A connection opens with one [`Hello`] line and one [`Reply`] line. An
//! `attach` that succeeds then carries the server's own MCP NDJSON in both
//! directions; every other request ends the connection after its reply.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

/// Bumped whenever a message below changes shape. A host on another schema
/// is not used, and its lock is a different file, so two can run side by side
/// while an old one drains.
pub(crate) const HOST_SCHEMA: u32 = 1;

pub(crate) const INFO_FILE: &str = "host-v1.json";
pub(crate) const LOCK_FILE: &str = "host-v1.lock";
pub(crate) const LOG_FILE: &str = "host.log";

/// Sent by the host to every session on a server it stopped for good, just
/// before the connection closes. A session that sees it drops the server
/// instead of bringing it back.
pub(crate) const STOPPED_METHOD: &str = "notifications/rebon/stopped";

/// Where the host's lock, address and log live: `$REBON_MCP_HOST_DIR`, else
/// `mcp-host` under the config home, so a relocated config home has its own.
pub(crate) fn host_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("REBON_MCP_HOST_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = rebon_session::config_home_with_env(|name| std::env::var_os(name))
        .ok_or_else(|| anyhow!("cannot locate the rebon config home"))?;
    Ok(home.join("mcp-host"))
}

/// The running host's address, as it published it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct HostInfo {
    pub schema: u32,
    pub pid: u32,
    pub port: u16,
    pub token: String,
}

pub(crate) fn load_host_info(dir: &Path) -> Option<HostInfo> {
    let text = std::fs::read_to_string(dir.join(INFO_FILE)).ok()?;
    let info: HostInfo = serde_json::from_str(&text).ok()?;
    (info.schema == HOST_SCHEMA).then_some(info)
}

pub(crate) fn store_host_info(dir: &Path, info: &HostInfo) -> anyhow::Result<()> {
    let text = serde_json::to_vec_pretty(info)?;
    rebon_session::write_private_file_atomically(&dir.join(INFO_FILE), &text)
        .with_context(|| format!("failed to publish {INFO_FILE} in {}", dir.display()))
}

/// One server exactly as the host should start it.
///
/// Everything the process sees is here, environment included, because the
/// host was started by some other session and its own environment is not the
/// one this session would have given the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// The whole environment the server starts with.
    pub env: Vec<(String, String)>,
    /// Absolute: a server with no `cwd` of its own runs where the session
    /// does, so two projects never share one.
    pub cwd: String,
    /// Part of the key without being the whole environment: the variables
    /// that decide which program `command` is and what it reads, plus the
    /// server's own `env`. Two sessions that differ only in, say, the
    /// terminal they run in still share.
    pub key_env: Vec<(String, String)>,
    pub init_timeout_ms: u64,
}

impl ServerSpec {
    /// Two attaches with the same key get the same process.
    pub fn key(&self) -> String {
        let mut env = self.key_env.clone();
        env.sort();
        let identity = serde_json::json!({
            "name": self.name,
            "command": self.command,
            "args": self.args,
            "cwd": self.cwd,
            "env": env,
        });
        format!("{:016x}", fnv1a64(identity.to_string().as_bytes()))
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Why a server is being stopped, which decides what its sessions do next.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Turned off: sessions drop the server and its tools.
    Disabled,
    /// Restarted: sessions keep the server and attach to a fresh process the
    /// next time they use it.
    Restart,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Request {
    Attach { spec: ServerSpec },
    List,
    Stop { name: String, reason: StopReason },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Hello {
    pub schema: u32,
    pub token: String,
    pub request: Request,
}

/// A server the host is running, as `list` reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SharedServerInfo {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub pid: Option<u32>,
    /// Sessions attached right now. Zero while the host keeps an idle server
    /// warm for the next one.
    pub sessions: usize,
    pub started_at_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub servers: Option<Vec<SharedServerInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl Reply {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServerSpec {
        ServerSpec {
            name: "fs".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "server".into()],
            env: vec![("TERM".into(), "xterm".into())],
            cwd: "/work".into(),
            key_env: vec![("PATH".into(), "/bin".into()), ("A".into(), "1".into())],
            init_timeout_ms: 1000,
        }
    }

    #[test]
    fn the_key_ignores_what_does_not_change_the_server() {
        let base = spec();
        let mut other_terminal = base.clone();
        other_terminal.env = vec![("TERM".into(), "dumb".into())];
        other_terminal.init_timeout_ms = 5;
        other_terminal.key_env.reverse();
        assert_eq!(base.key(), other_terminal.key());
    }

    #[test]
    fn the_key_tells_apart_what_does() {
        let base = spec();
        let changes: [fn(&mut ServerSpec); 5] = [
            |s| s.name = "other".into(),
            |s| s.command = "node".into(),
            |s| s.args.push("--flag".into()),
            |s| s.cwd = "/elsewhere".into(),
            |s| s.key_env[0].1 = "/usr/bin".into(),
        ];
        for change in changes {
            let mut changed = base.clone();
            change(&mut changed);
            assert_ne!(base.key(), changed.key());
        }
    }

    #[test]
    fn host_info_from_another_schema_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let info = HostInfo {
            schema: HOST_SCHEMA,
            pid: 1,
            port: 2,
            token: "t".into(),
        };
        store_host_info(dir.path(), &info).unwrap();
        assert_eq!(load_host_info(dir.path()), Some(info.clone()));
        let old = HostInfo {
            schema: HOST_SCHEMA + 1,
            ..info
        };
        std::fs::write(
            dir.path().join(INFO_FILE),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        assert_eq!(load_host_info(dir.path()), None);
    }

    #[test]
    fn requests_round_trip() {
        let hello = Hello {
            schema: HOST_SCHEMA,
            token: "t".into(),
            request: Request::Stop {
                name: "fs".into(),
                reason: StopReason::Restart,
            },
        };
        let text = serde_json::to_string(&hello).unwrap();
        assert!(text.contains("\"op\":\"stop\""), "{text}");
        let back: Hello = serde_json::from_str(&text).unwrap();
        assert_eq!(back.request, hello.request);
    }
}
