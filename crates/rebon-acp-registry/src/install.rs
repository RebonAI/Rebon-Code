//! Installing a registry agent as an `acpAgents` entry, and removing it.
//!
//! A package agent (npx, uvx) is an entry and nothing else: its runner
//! fetches the package the first time the agent starts. A binary agent is
//! downloaded, checked against the registry's SHA-256 when it gives one,
//! and unpacked under `<config home>/acp-agents/<registry id>/<version>/`;
//! its entry points at the executable there. Removing an agent installed
//! this way removes that directory as well.
//!
//! An entry the user declared by hand is never replaced, and one registry
//! agent is installed under one id: a second id would share its download,
//! and removing either would take the other's executable away.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use rebon_config::{AcpAgentConfig, AcpRegistrySource};
use sha2::{Digest, Sha256};

use crate::{BinaryTarget, Method, RegistryAgent};

/// What [`install`] did.
#[derive(Debug, Clone)]
pub struct Installed {
    pub agent: AcpAgentConfig,
    /// `binary`, `npx` or `uvx`.
    pub method: &'static str,
    /// The entry it replaced: an older install of the same agent.
    pub replaced: Option<AcpAgentConfig>,
    /// Where a binary agent was unpacked.
    pub unpacked_in: Option<PathBuf>,
}

/// What [`remove`] did.
#[derive(Debug, Clone)]
pub struct Removed {
    pub agent: AcpAgentConfig,
    /// The download directory it took with it.
    pub deleted: Option<PathBuf>,
}

/// Downloads are refused past this size; the largest agent the registry
/// ships is a fraction of it.
const MAX_DOWNLOAD: u64 = 1 << 30;

/// Where the binary agents' downloads live.
pub fn agents_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("acp-agents")
}

/// Installs `agent` as the entry `id` (by default [`crate::default_id`]).
pub async fn install(
    client: &reqwest::Client,
    config_dir: &Path,
    agent: &RegistryAgent,
    id: Option<&str>,
) -> Result<Installed> {
    let method = crate::choose(agent, crate::current_platform(), crate::on_path)
        .map_err(|why| anyhow::anyhow!("{} cannot be installed: {why}", agent.name))?;
    let id = id
        .map(|id| id.trim().to_string())
        .unwrap_or_else(|| crate::default_id(&agent.id));
    check_free(config_dir, &id, agent)?;
    let unpacked_in = match &method {
        Method::Binary(target) => Some(fetch_binary(client, config_dir, agent, target).await?),
        Method::Npx(_) | Method::Uvx(_) => None,
    };
    let entry = entry(agent, &id, &method, unpacked_in.as_deref(), cfg!(windows))?;
    let replaced = commit(config_dir, &entry)?;
    Ok(Installed {
        agent: entry,
        method: method.kind(),
        replaced,
        unpacked_in,
    })
}

/// Takes the entry `id` out of `acpAgents`, and with it the download
/// directory of a registry agent. `None` when there was no such entry.
pub fn remove(config_dir: &Path, id: &str) -> Result<Option<Removed>> {
    let Some(agent) = rebon_config::remove_acp_agent_in(config_dir, id)? else {
        return Ok(None);
    };
    let deleted = match &agent.registry {
        Some(source) if crate::valid_id(&source.id) => {
            let dir = agents_dir(config_dir).join(&source.id);
            if dir.exists() {
                std::fs::remove_dir_all(&dir)
                    .with_context(|| format!("failed to remove {}", dir.display()))?;
                Some(dir)
            } else {
                None
            }
        }
        _ => None,
    };
    Ok(Some(Removed { agent, deleted }))
}

/// Refuses an `id` that names a hand-written entry or another registry
/// agent, and an agent already installed under a different id.
fn check_free(config_dir: &Path, id: &str, agent: &RegistryAgent) -> Result<()> {
    if id.is_empty() || id.eq_ignore_ascii_case(rebon_config::LOCAL_AGENT_ID) {
        bail!("`{id}` cannot be an agent's id");
    }
    for existing in rebon_config::load_acp_agents_in(config_dir)? {
        let same_id = existing.id.eq_ignore_ascii_case(id);
        let same_agent = existing
            .registry
            .as_ref()
            .is_some_and(|source| source.id.eq_ignore_ascii_case(&agent.id));
        match (same_id, same_agent) {
            (true, true) | (false, false) => {}
            (true, false) if existing.registry.is_none() => bail!(
                "an agent `{id}` is already declared in config.json by hand; it was left alone. Pick another id (--as) or remove that entry first"
            ),
            (true, false) => bail!(
                "`{id}` is already {}, installed from the registry; pick another id (--as) or remove it first",
                existing.label()
            ),
            (false, true) => bail!(
                "{} is already installed as `{}`; remove that first",
                agent.name,
                existing.id
            ),
        }
    }
    Ok(())
}

/// The `acpAgents` entry for `agent` started by `method`, with a binary
/// agent unpacked in `unpacked_in`.
fn entry(
    agent: &RegistryAgent,
    id: &str,
    method: &Method,
    unpacked_in: Option<&Path>,
    windows: bool,
) -> Result<AcpAgentConfig> {
    let (command, args, env) = match method {
        // `-y`: npx would otherwise ask before installing, on the very
        // stdin that carries the ACP connection.
        Method::Npx(package) => (
            if windows { "npx.cmd" } else { "npx" }.to_string(),
            ["-y".to_string(), package.package.clone()]
                .into_iter()
                .chain(package.args.iter().cloned())
                .collect(),
            package.env.clone(),
        ),
        Method::Uvx(package) => (
            "uvx".to_string(),
            std::iter::once(package.package.clone())
                .chain(package.args.iter().cloned())
                .collect(),
            package.env.clone(),
        ),
        Method::Binary(target) => {
            let dir = unpacked_in.context("a binary agent needs the directory it unpacked in")?;
            (
                command_path(dir, &target.cmd)?
                    .to_string_lossy()
                    .into_owned(),
                target.args.clone(),
                target.env.clone(),
            )
        }
    };
    Ok(AcpAgentConfig {
        id: id.to_string(),
        display_name: Some(agent.name.clone()),
        command,
        args,
        env,
        cwd: None,
        inject_fs_tools: None,
        session_meta: None,
        registry: Some(AcpRegistrySource {
            id: agent.id.clone(),
            version: agent.version.clone(),
        }),
    })
}

/// Writes `entry`, then clears away the download of the version it
/// replaced, if that was a different one.
fn commit(config_dir: &Path, entry: &AcpAgentConfig) -> Result<Option<AcpAgentConfig>> {
    let replaced = rebon_config::upsert_acp_agent_in(config_dir, entry)?;
    let old_version = replaced
        .as_ref()
        .and_then(|old| old.registry.as_ref())
        .filter(|old| Some(&old.version) != entry.registry.as_ref().map(|new| &new.version));
    if let Some(old) = old_version {
        if crate::valid_id(&old.id) {
            let dir = agents_dir(config_dir)
                .join(&old.id)
                .join(path_safe(&old.version));
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    Ok(replaced)
}

/// Downloads and unpacks `target` into its version directory, unless an
/// earlier install already did: the directory only appears once it is
/// complete. Answers the directory.
async fn fetch_binary(
    client: &reqwest::Client,
    config_dir: &Path,
    agent: &RegistryAgent,
    target: &BinaryTarget,
) -> Result<PathBuf> {
    let root = agents_dir(config_dir).join(&agent.id);
    let dir = root.join(path_safe(&agent.version));
    if command_path(&dir, &target.cmd)?.is_file() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&root)
        .with_context(|| format!("failed to create {}", root.display()))?;
    let archive = root.join(format!(".download-{}", std::process::id()));
    let outcome = match download(client, &target.archive, &archive).await {
        Ok(()) => verify(&archive, target.sha256.as_deref())
            .and_then(|()| place(&archive, &dir, &target.cmd)),
        Err(error) => Err(error),
    };
    let _ = std::fs::remove_file(&archive);
    outcome?;
    Ok(dir)
}

async fn download(client: &reqwest::Client, url: &str, to: &Path) -> Result<()> {
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to reach {url}"))?
        .error_for_status()
        .with_context(|| format!("{url} refused"))?;
    let mut file =
        std::fs::File::create(to).with_context(|| format!("failed to create {}", to.display()))?;
    let mut written = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("the download of {url} broke off"))?
    {
        written += chunk.len() as u64;
        if written > MAX_DOWNLOAD {
            bail!("{url} is larger than any agent should be; the download was stopped");
        }
        file.write_all(&chunk)
            .with_context(|| format!("failed to write {}", to.display()))?;
    }
    file.flush()?;
    Ok(())
}

/// Checks the file against the registry's digest, when it gives one.
fn verify(file: &Path, sha256: Option<&str>) -> Result<()> {
    let Some(expected) = sha256.map(str::trim).filter(|sum| !sum.is_empty()) else {
        return Ok(());
    };
    let mut hasher = Sha256::new();
    let mut reader =
        std::fs::File::open(file).with_context(|| format!("failed to read {}", file.display()))?;
    std::io::copy(&mut reader, &mut hasher)?;
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("the download's SHA-256 is {actual}, not the {expected} the registry gives; it was discarded");
    }
    Ok(())
}

/// Unpacks `archive` (or, when it is no archive, takes it as the
/// executable itself) into `dir`, by way of a directory beside it that is
/// renamed into place once everything is there.
fn place(archive: &Path, dir: &Path, cmd: &str) -> Result<()> {
    let staging = dir.with_file_name(format!(
        ".{}.partial-{}",
        dir.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("failed to create {}", staging.display()))?;
    let outcome = unpack(archive, &staging, cmd).and_then(|()| {
        let command = command_path(&staging, cmd)?;
        if !command.is_file() {
            bail!("the download has no `{cmd}` in it");
        }
        make_executable(&command)?;
        let _ = std::fs::remove_dir_all(dir);
        std::fs::rename(&staging, dir)
            .with_context(|| format!("failed to move the agent into {}", dir.display()))
    });
    if outcome.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    outcome
}

/// What an archive is, from its first bytes: the registry's URLs do not
/// always end in an extension (some carry the file name in a query).
#[derive(Debug, PartialEq, Eq)]
enum Format {
    Zip,
    TarGz,
    TarBz2,
    Executable,
}

fn sniff(archive: &Path) -> Result<Format> {
    let mut head = [0u8; 4];
    let read = std::fs::File::open(archive)?.read(&mut head)?;
    Ok(match &head[..read] {
        [b'P', b'K', 3, 4] => Format::Zip,
        [0x1f, 0x8b, ..] => Format::TarGz,
        [b'B', b'Z', b'h', ..] => Format::TarBz2,
        _ => Format::Executable,
    })
}

fn unpack(archive: &Path, into: &Path, cmd: &str) -> Result<()> {
    let open = || std::fs::File::open(archive).context("failed to reopen the download");
    match sniff(archive)? {
        // `unpack` keeps every entry inside `into`; one that would land
        // outside it is an error, not a file somewhere else.
        Format::TarGz => tar::Archive::new(flate2::read::GzDecoder::new(open()?))
            .unpack(into)
            .context("the .tar.gz did not unpack"),
        Format::TarBz2 => tar::Archive::new(bzip2::read::BzDecoder::new(open()?))
            .unpack(into)
            .context("the .tar.bz2 did not unpack"),
        // `extract` skips any name that is not enclosed in `into`.
        Format::Zip => zip::ZipArchive::new(open()?)
            .and_then(|mut zip| zip.extract(into))
            .context("the .zip did not unpack"),
        Format::Executable => {
            let to = command_path(into, cmd)?;
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(archive, &to)
                .map(drop)
                .with_context(|| format!("failed to write {}", to.display()))
        }
    }
}

/// `cmd` (`./goose`, `bin/agent`) under `dir`, refusing one that would
/// leave it.
fn command_path(dir: &Path, cmd: &str) -> Result<PathBuf> {
    let relative = Path::new(cmd.trim());
    let mut path = dir.to_path_buf();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => path.push(part),
            _ => bail!("the registry's command `{cmd}` points outside the agent's directory"),
        }
    }
    if path == dir {
        bail!("the registry gives no command for this agent");
    }
    Ok(path)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to make {} executable", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// A registry version as one path component.
fn path_safe(version: &str) -> String {
    let safe: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect();
    match safe.trim_matches('.') {
        "" => "_".to_string(),
        trimmed => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Registry;

    fn agents() -> Registry {
        Registry::parse(&crate::tests::sample()).unwrap()
    }

    fn npx_pi() -> (RegistryAgent, Method) {
        let pi = agents().agent("pi-acp").unwrap().clone();
        let method = Method::Npx(pi.distribution.npx.clone().unwrap());
        (pi, method)
    }

    fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn tar_bz2(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(bzip2::write::BzEncoder::new(
            Vec::new(),
            bzip2::Compression::fast(),
        ));
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, data) in files {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn placed(bytes: &[u8], cmd: &str) -> (tempfile::TempDir, Result<PathBuf>) {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("download");
        std::fs::write(&archive, bytes).unwrap();
        let dir = root.path().join("agent").join("1.0.0");
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        let outcome = place(&archive, &dir, cmd).map(|()| dir);
        (root, outcome)
    }

    #[test]
    fn every_archive_the_registry_ships_unpacks_to_its_command() {
        for (label, bytes) in [
            ("tar.gz", tar_gz(&[("goose", b"#!agent"), ("lib/x", b"x")])),
            ("tar.bz2", tar_bz2(&[("goose", b"#!agent")])),
            ("zip", zip_of(&[("goose", b"#!agent"), ("README", b"r")])),
            ("raw", b"\x7fELF not an archive".to_vec()),
        ] {
            let (_root, dir) = placed(&bytes, "./goose");
            let dir = dir.unwrap_or_else(|error| panic!("{label}: {error:#}"));
            assert!(dir.join("goose").is_file(), "{label}");
            assert!(
                !dir.parent().unwrap().read_dir().unwrap().any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("partial")),
                "{label}: the staging directory is gone"
            );
        }
    }

    #[test]
    fn an_archive_without_the_command_leaves_nothing_behind() {
        let (root, dir) = placed(&tar_gz(&[("other", b"x")]), "./goose");
        let error = dir.unwrap_err().to_string();
        assert!(error.contains("no `./goose`"), "{error}");
        let parent = root.path().join("agent");
        assert_eq!(parent.read_dir().unwrap().count(), 0);
    }

    #[test]
    fn a_command_outside_the_agent_directory_is_refused() {
        let dir = Path::new("/agents/goose/1.0.0");
        assert_eq!(
            command_path(dir, "./bin/goose").unwrap(),
            dir.join("bin").join("goose")
        );
        assert!(command_path(dir, "../goose").is_err());
        assert!(command_path(dir, "/usr/bin/goose").is_err());
        assert!(command_path(dir, "./").is_err());
    }

    #[test]
    fn a_download_that_does_not_match_the_registry_digest_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("download");
        std::fs::write(&file, b"agent").unwrap();
        let right = format!("{:x}", Sha256::digest(b"agent"));
        assert!(verify(&file, Some(&right)).is_ok());
        assert!(verify(&file, Some(&right.to_uppercase())).is_ok());
        assert!(verify(&file, None).is_ok());
        assert!(verify(&file, Some(&"0".repeat(64)))
            .unwrap_err()
            .to_string()
            .contains("discarded"));
    }

    #[test]
    fn package_entries_say_yes_to_npx_and_find_the_windows_shim() {
        let (pi, method) = npx_pi();
        let unix = entry(&pi, "pi", &method, None, false).unwrap();
        assert_eq!(unix.command, "npx");
        assert_eq!(unix.args, ["-y", "pi-acp@0.0.35"]);
        assert_eq!(unix.display_name.as_deref(), Some("pi ACP"));
        assert_eq!(unix.registry.as_ref().unwrap().id, "pi-acp");
        assert_eq!(
            entry(&pi, "pi", &method, None, true).unwrap().command,
            "npx.cmd"
        );

        let fast = agents().agent("fast-agent").unwrap().clone();
        let uvx = Method::Uvx(fast.distribution.uvx.clone().unwrap());
        let entry = entry(&fast, "fast-agent", &uvx, None, false).unwrap();
        assert_eq!(entry.command, "uvx");
        assert_eq!(entry.args, ["fast-agent-acp==0.10.66", "-x"]);
    }

    #[test]
    fn a_binary_entry_points_at_the_unpacked_command() {
        let goose = agents().agent("goose").unwrap().clone();
        let target = goose.distribution.binary["darwin-aarch64"].clone();
        let dir = Path::new("/home/me/.rebon/acp-agents/goose/1.61.1");
        let entry = entry(&goose, "goose", &Method::Binary(target), Some(dir), false).unwrap();
        assert_eq!(Path::new(&entry.command), dir.join("goose"));
        assert_eq!(entry.args, ["acp"]);
    }

    #[test]
    fn a_hand_written_entry_or_a_second_id_for_one_agent_is_refused() {
        let config = tempfile::tempdir().unwrap();
        let (pi, method) = npx_pi();
        rebon_config::upsert_acp_agent_in(
            config.path(),
            &AcpAgentConfig {
                registry: None,
                ..entry(&pi, "mine", &method, None, false).unwrap()
            },
        )
        .unwrap();
        let error = check_free(config.path(), "mine", &pi)
            .unwrap_err()
            .to_string();
        assert!(error.contains("by hand"), "{error}");

        commit(
            config.path(),
            &entry(&pi, "pi", &method, None, false).unwrap(),
        )
        .unwrap();
        assert!(
            check_free(config.path(), "pi", &pi).is_ok(),
            "reinstall in place"
        );
        let error = check_free(config.path(), "pi2", &pi)
            .unwrap_err()
            .to_string();
        assert!(error.contains("already installed as `pi`"), "{error}");
        assert!(check_free(config.path(), "local", &pi).is_err());
    }

    #[test]
    fn an_update_drops_the_old_version_and_removal_drops_the_rest() {
        let config = tempfile::tempdir().unwrap();
        let goose = agents().agent("goose").unwrap().clone();
        let target = goose.distribution.binary["darwin-aarch64"].clone();
        let root = agents_dir(config.path()).join("goose");
        let old_dir = root.join("1.61.1");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("goose"), b"old").unwrap();
        let method = Method::Binary(target);
        commit(
            config.path(),
            &entry(&goose, "goose", &method, Some(&old_dir), false).unwrap(),
        )
        .unwrap();

        let mut newer = goose.clone();
        newer.version = "1.62.0".into();
        let new_dir = root.join("1.62.0");
        std::fs::create_dir_all(&new_dir).unwrap();
        let replaced = commit(
            config.path(),
            &entry(&newer, "goose", &method, Some(&new_dir), false).unwrap(),
        )
        .unwrap();
        assert_eq!(replaced.unwrap().registry.unwrap().version, "1.61.1");
        assert!(!old_dir.exists());
        assert!(new_dir.exists());

        let removed = remove(config.path(), "goose").unwrap().unwrap();
        assert_eq!(removed.deleted.as_deref(), Some(root.as_path()));
        assert!(!root.exists());
        assert!(remove(config.path(), "goose").unwrap().is_none());
    }

    #[test]
    fn removing_a_package_agent_touches_no_directory() {
        let config = tempfile::tempdir().unwrap();
        let (pi, method) = npx_pi();
        commit(
            config.path(),
            &entry(&pi, "pi", &method, None, false).unwrap(),
        )
        .unwrap();
        let removed = remove(config.path(), "pi").unwrap().unwrap();
        assert!(removed.deleted.is_none());
        assert_eq!(removed.agent.args, ["-y", "pi-acp@0.0.35"]);
    }

    #[test]
    fn versions_become_one_safe_path_component() {
        assert_eq!(path_safe("1.61.1"), "1.61.1");
        assert_eq!(path_safe("2026.11.22"), "2026.11.22");
        assert_eq!(path_safe("../../etc"), "_.._etc");
        assert_eq!(path_safe(".."), "_");
        assert_eq!(path_safe("1.0/beta"), "1.0_beta");
    }
}
