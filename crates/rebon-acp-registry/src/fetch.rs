//! Reading the registry, and the copy of it kept for later.
//!
//! The copy lives at `<config home>/cache/acp-registry.json`. A copy younger
//! than the caller's limit is used without asking the network; an older one
//! still answers when the network does not, flagged as such, so listing
//! agents works on a plane.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::Registry;

/// The registry every ACP client reads; dated snapshots sit beside it.
pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// How long a kept copy is used without asking again.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// A registry, and where it came from.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub registry: Registry,
    /// When it was read from the network.
    pub fetched_at_ms: u64,
    /// Read from the kept copy rather than the network.
    pub from_cache: bool,
    /// The network failed and this is an older copy; why it failed.
    pub stale_because: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Kept {
    fetched_at_ms: u64,
    registry: serde_json::Value,
}

pub fn cache_path(config_dir: &Path) -> PathBuf {
    config_dir.join("cache").join("acp-registry.json")
}

/// The HTTP client the registry and downloads go through: the system's
/// proxy settings, a timeout so a dead network fails rather than hangs.
pub fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("rebon/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("failed to build the HTTP client")
}

/// The registry: the kept copy when it is younger than `max_age` and
/// `refresh` is off, else the network's (kept for next time), else — the
/// network failing — whatever copy there is.
pub async fn load(
    client: &reqwest::Client,
    config_dir: &Path,
    max_age: Duration,
    refresh: bool,
) -> anyhow::Result<Fetched> {
    let kept = read_kept(config_dir);
    let now = now_ms();
    if let Some((fetched_at_ms, registry)) = &kept {
        if !refresh && now.saturating_sub(*fetched_at_ms) < max_age.as_millis() as u64 {
            return Ok(Fetched {
                registry: registry.clone(),
                fetched_at_ms: *fetched_at_ms,
                from_cache: true,
                stale_because: None,
            });
        }
    }
    match download(client).await {
        Ok(bytes) => {
            let registry = Registry::parse(&bytes)?;
            // A failure to keep the copy costs the next call a download,
            // nothing more; it is not this call's failure.
            let _ = keep(config_dir, now, &bytes);
            Ok(Fetched {
                registry,
                fetched_at_ms: now,
                from_cache: false,
                stale_because: None,
            })
        }
        Err(error) => match kept {
            Some((fetched_at_ms, registry)) => Ok(Fetched {
                registry,
                fetched_at_ms,
                from_cache: true,
                stale_because: Some(format!("{error:#}")),
            }),
            None => Err(error),
        },
    }
}

async fn download(client: &reqwest::Client) -> anyhow::Result<Vec<u8>> {
    let response = client
        .get(REGISTRY_URL)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("failed to reach {REGISTRY_URL}"))?
        .error_for_status()
        .with_context(|| format!("{REGISTRY_URL} refused"))?;
    Ok(response
        .bytes()
        .await
        .context("the ACP registry download broke off")?
        .to_vec())
}

fn read_kept(config_dir: &Path) -> Option<(u64, Registry)> {
    let bytes = std::fs::read(cache_path(config_dir)).ok()?;
    let kept: Kept = serde_json::from_slice(&bytes).ok()?;
    let registry = Registry::parse(&serde_json::to_vec(&kept.registry).ok()?).ok()?;
    Some((kept.fetched_at_ms, registry))
}

fn keep(config_dir: &Path, fetched_at_ms: u64, bytes: &[u8]) -> anyhow::Result<()> {
    let path = cache_path(config_dir);
    let kept = Kept {
        fetched_at_ms,
        registry: serde_json::from_slice(bytes)?,
    };
    std::fs::create_dir_all(path.parent().expect("the cache path has a directory"))?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(&kept)?)?;
    std::fs::rename(&temporary, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_copy_reads_back_as_the_registry_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = crate::tests::sample();
        keep(dir.path(), 42, &bytes).unwrap();
        let (at, registry) = read_kept(dir.path()).unwrap();
        assert_eq!(at, 42);
        assert_eq!(registry, Registry::parse(&bytes).unwrap());
    }

    #[test]
    fn a_damaged_copy_is_no_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert!(read_kept(dir.path()).is_none());
    }
}
