//! `rebon node …` — the Node runtime the plugin plane runs on.
//!
//! Rebon does not bundle Node. `npm install -g @rebon/cli` implies one is
//! already on the machine; `cargo install` and a downloaded binary do not. This
//! module is the surface for the gap: it reports which runtime would be used,
//! and installs one under the config home when nothing suitable is present.
//!
//! ```text
//! ~/.rebon/runtime/node/24.19.0/…      (an installed runtime + its receipt)
//! ```
//!
//! Downloading is the only thing this module adds to `rebon-node-runtime`;
//! verification, staging, and publication all live in the crate so the desktop
//! app installs exactly the same way. Rebon only ever downloads a build whose
//! SHA-256 is compiled into this binary — a mirror can serve the bytes, but it
//! cannot change which bytes are accepted. An archive Rebon does not pin can
//! still be installed from disk, by naming its version and digest.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use rebon_node_runtime::{
    pinned_archive_for_host, ArchiveExpectation, DownloadPolicy, ExecutingProbe,
    ManagedRuntimeStore, NodeRuntimeResolver, NodeVersion, Sha256Digest, SystemTarExtractor,
    DEFAULT_NODE_DIST_BASE, DOWNLOAD_POLICY_ENV, NODE_EXECUTABLE_ENV, SUPPORTED_NODE_VERSIONS,
};

use crate::NodeCommand;

pub(crate) async fn run(command: NodeCommand) -> anyhow::Result<()> {
    let store = ManagedRuntimeStore::under_config_home(&::rebon_config::config_home_dir());
    match command {
        NodeCommand::Status => status(&store),
        NodeCommand::Install {
            from_path,
            sha256,
            version,
            dist_base,
            force,
        } => install(&store, from_path, sha256, version, dist_base, force).await,
        NodeCommand::Uninstall { version, all } => uninstall(&store, version, all),
    }
}

fn status(store: &ManagedRuntimeStore) -> anyhow::Result<()> {
    println!("Supported Node: {SUPPORTED_NODE_VERSIONS}");
    let probe = ExecutingProbe;
    match NodeRuntimeResolver::from_env(&probe, store).resolve() {
        Ok(resolved) => println!("Resolved: {resolved}"),
        Err(error) => println!("Resolved: none — {error}"),
    }

    println!("\nManaged runtimes under {store}:");
    let installed = store.installed();
    if installed.is_empty() {
        println!("  (none installed — `rebon node install` puts one here)");
    }
    for runtime in installed {
        println!(
            "  {}  {}  [{} {}]",
            runtime.version,
            runtime.executable.display(),
            runtime.receipt.provenance,
            runtime.receipt.archive_file_name
        );
    }

    println!();
    match DownloadPolicy::from_env() {
        DownloadPolicy::Allowed => println!("Downloads: allowed"),
        DownloadPolicy::Disabled => {
            println!("Downloads: disabled by {DOWNLOAD_POLICY_ENV}");
        }
    }
    match pinned_archive_for_host() {
        Some(archive) => println!(
            "Pinned build: {} ({})",
            archive.file_name,
            archive.url(DEFAULT_NODE_DIST_BASE)
        ),
        None => println!(
            "Pinned build: none for this platform — install from a local archive \
             with --from-path, --version and --sha256"
        ),
    }
    println!("Override: set {NODE_EXECUTABLE_ENV} to an absolute Node executable");
    Ok(())
}

/// Which digest an install must match, given the flags.
///
/// Split out from [`install`] because it is the part that can be wrong in a way
/// that matters: every combination here decides what bytes get trusted.
fn expectation_for(
    from_path: Option<&Path>,
    sha256: Option<&str>,
    version: Option<&str>,
) -> anyhow::Result<ArchiveExpectation> {
    match (sha256, version) {
        (Some(sha256), Some(version)) => {
            if from_path.is_none() {
                bail!(
                    "--sha256 and --version describe a local archive; pass --from-path too \
                     (Rebon only downloads the builds it pins)"
                );
            }
            let digest =
                Sha256Digest::parse_hex(sha256).with_context(|| format!("--sha256 {sha256}"))?;
            let version =
                NodeVersion::parse(version).with_context(|| format!("--version {version}"))?;
            if !SUPPORTED_NODE_VERSIONS.contains(&version) {
                bail!(
                    "Node {version} is outside the range the plugin host supports \
                     ({SUPPORTED_NODE_VERSIONS})"
                );
            }
            Ok(ArchiveExpectation::Declared { version, digest })
        }
        (Some(_), None) | (None, Some(_)) => bail!(
            "--sha256 and --version go together: an archive Rebon does not pin has to name \
             both what it is and what it hashes to"
        ),
        (None, None) => pinned_archive_for_host()
            .map(ArchiveExpectation::Pinned)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Rebon pins no Node build for this platform; install one from a local archive \
                 with --from-path, --version and --sha256"
                )
            }),
    }
}

/// Install the pinned build with no arguments, for a caller that is not a
/// command line.
///
/// The startup gate offers this, so it goes through exactly the same path
/// `rebon node install` does — same digest, same staging, same publication. An
/// install offered in a dialog must not be a second, laxer installer.
pub(crate) async fn install_pinned_runtime() -> anyhow::Result<()> {
    let store = ManagedRuntimeStore::under_config_home(&::rebon_config::config_home_dir());
    install(&store, None, None, None, None, false).await
}

async fn install(
    store: &ManagedRuntimeStore,
    from_path: Option<PathBuf>,
    sha256: Option<String>,
    version: Option<String>,
    dist_base: Option<String>,
    force: bool,
) -> anyhow::Result<()> {
    let expectation = expectation_for(from_path.as_deref(), sha256.as_deref(), version.as_deref())?;

    // The store treats a re-install as a no-op, but it only learns that after
    // it has been handed an archive — which on the download path is tens of
    // megabytes already fetched. Ask first.
    if let Some(existing) = already_installed(store, &expectation, force) {
        println!(
            "Node {} is already installed at {} — pass --force to reinstall",
            existing.version,
            existing.executable.display()
        );
        report_resolution(store);
        return Ok(());
    }

    let installed = match from_path {
        Some(archive) => {
            if !archive.is_file() {
                bail!("{} is not a file", archive.display());
            }
            store.install(&archive, &expectation, &SystemTarExtractor, force)?
        }
        None => {
            if DownloadPolicy::from_env().is_disabled() {
                bail!("{}", DownloadPolicy::Disabled.refusal());
            }
            let ArchiveExpectation::Pinned(pinned) = expectation else {
                unreachable!("expectation_for rejects a declared archive without --from-path");
            };
            let base = dist_base.unwrap_or_else(|| DEFAULT_NODE_DIST_BASE.to_string());
            let url = pinned.url(&base);
            // Land the download beside the store so publishing stays a rename
            // on one filesystem, and so a large temp file lands wherever the
            // user already agreed Rebon may write.
            let scratch = store
                .root()
                .join(format!(".download-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch)
                .with_context(|| format!("creating {}", scratch.display()))?;
            let result =
                download_and_install(store, &url, pinned.file_name, &scratch, &expectation, force)
                    .await;
            let _ = std::fs::remove_dir_all(&scratch);
            result?
        }
    };

    println!(
        "Installed Node {} at {}",
        installed.version,
        installed.executable.display()
    );
    println!(
        "  {} archive {} ({})",
        installed.receipt.provenance,
        installed.receipt.archive_file_name,
        installed.receipt.archive_sha256
    );
    report_resolution(store);
    Ok(())
}

/// The published runtime this install would produce, when there is nothing to do.
fn already_installed(
    store: &ManagedRuntimeStore,
    expectation: &ArchiveExpectation,
    force: bool,
) -> Option<rebon_node_runtime::InstalledRuntime> {
    (!force)
        .then(|| store.read(&expectation.version()))
        .flatten()
}

/// What a session started right now would run on. Printed after an install so
/// the answer is the resolver's, not an assumption that the newest install wins.
fn report_resolution(store: &ManagedRuntimeStore) {
    let probe = ExecutingProbe;
    match NodeRuntimeResolver::from_env(&probe, store).resolve() {
        Ok(resolved) => println!("  sessions will use {resolved}"),
        Err(error) => println!("  warning: {error}"),
    }
}

async fn download_and_install(
    store: &ManagedRuntimeStore,
    url: &str,
    file_name: &str,
    scratch: &Path,
    expectation: &ArchiveExpectation,
    force: bool,
) -> anyhow::Result<rebon_node_runtime::InstalledRuntime> {
    println!("Downloading {url} …");
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("fetching {url}"))?
        .error_for_status()
        .with_context(|| format!("{url} did not serve a Node archive"))?;
    let bytes = response.bytes().await.context("reading the archive")?;
    let archive = scratch.join(file_name);
    std::fs::write(&archive, &bytes).with_context(|| format!("writing {}", archive.display()))?;
    Ok(store.install(&archive, expectation, &SystemTarExtractor, force)?)
}

fn uninstall(
    store: &ManagedRuntimeStore,
    version: Option<String>,
    all: bool,
) -> anyhow::Result<()> {
    let targets: Vec<NodeVersion> = match (version, all) {
        (Some(_), true) => bail!("pass either --version or --all, not both"),
        (Some(raw), false) => {
            vec![NodeVersion::parse(&raw).with_context(|| format!("--version {raw}"))?]
        }
        (None, true) => store.installed().iter().map(|r| r.version).collect(),
        (None, false) => bail!("name a version with --version, or pass --all"),
    };
    if targets.is_empty() {
        println!("Nothing installed under {store}");
        return Ok(());
    }
    for version in targets {
        if store.remove(&version)? {
            println!("Removed Node {version} from {store}");
        } else {
            println!("Node {version} was not installed under {store}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINNED_HEX: &str = "57f71ab3652e797d84acddc79c81cc9ff1c6ddb2a1974cdb83f00fee9bff4c73";

    #[test]
    fn bare_install_uses_the_platforms_pinned_build() {
        let expectation = expectation_for(None, None, None).unwrap();
        assert!(matches!(expectation, ArchiveExpectation::Pinned(_)));
        assert_eq!(expectation.provenance(), "pinned");
    }

    #[test]
    fn a_local_archive_may_declare_its_own_identity() {
        let expectation = expectation_for(
            Some(Path::new("node.tar.gz")),
            Some(PINNED_HEX),
            Some("24.20.1"),
        )
        .unwrap();
        assert_eq!(expectation.version(), NodeVersion::new(24, 20, 1));
        assert_eq!(expectation.digest().to_hex(), PINNED_HEX);
    }

    /// Half a declaration is how an archive gets installed under a version it is
    /// not, or accepted without being hashed at all.
    #[test]
    fn version_and_digest_must_arrive_together() {
        let only_digest = expectation_for(Some(Path::new("a.tgz")), Some(PINNED_HEX), None)
            .unwrap_err()
            .to_string();
        assert!(only_digest.contains("go together"), "{only_digest}");
        let only_version = expectation_for(Some(Path::new("a.tgz")), None, Some("24.19.0"))
            .unwrap_err()
            .to_string();
        assert!(only_version.contains("go together"), "{only_version}");
    }

    /// Rebon downloads only what it pins: a declared digest with no local file
    /// would mean fetching arbitrary bytes and trusting a flag for what they are.
    #[test]
    fn a_declared_archive_without_a_local_file_is_refused() {
        let error = expectation_for(None, Some(PINNED_HEX), Some("24.19.0"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("--from-path"), "{error}");
    }

    #[test]
    fn a_declared_version_outside_the_supported_range_is_refused() {
        let error = expectation_for(Some(Path::new("a.tgz")), Some(PINNED_HEX), Some("22.21.1"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(">=24.19.0 <25.0.0"), "{error}");
    }

    #[test]
    fn malformed_declarations_name_the_flag_that_is_wrong() {
        let bad_digest =
            expectation_for(Some(Path::new("a.tgz")), Some("not-hex"), Some("24.19.0"))
                .unwrap_err()
                .to_string();
        assert!(bad_digest.contains("--sha256"), "{bad_digest}");
        let bad_version =
            expectation_for(Some(Path::new("a.tgz")), Some(PINNED_HEX), Some("24.19"))
                .unwrap_err()
                .to_string();
        assert!(bad_version.contains("--version"), "{bad_version}");
    }

    /// Writes a minimal payload so a store can be populated without an archive.
    struct StubExtractor;

    impl rebon_node_runtime::ArchiveExtractor for StubExtractor {
        fn extract(&self, _archive: &Path, destination: &Path) -> Result<(), String> {
            std::fs::create_dir_all(destination.join("bin")).unwrap();
            std::fs::write(destination.join("bin").join("node"), b"payload").unwrap();
            Ok(())
        }
    }

    fn populated_store(dir: &Path) -> (ManagedRuntimeStore, ArchiveExpectation) {
        let archive = dir.join("node.tar.gz");
        std::fs::write(&archive, b"bytes").unwrap();
        let expectation = ArchiveExpectation::Declared {
            version: NodeVersion::new(24, 19, 0),
            digest: Sha256Digest::of_file(&archive).unwrap(),
        };
        let store = ManagedRuntimeStore::new(dir.join("store"));
        store
            .install(&archive, &expectation, &StubExtractor, false)
            .unwrap();
        (store, expectation)
    }

    /// The store's own idempotence only kicks in once it has the archive, which
    /// on the download path means the bytes were already fetched.
    #[test]
    fn a_published_version_short_circuits_before_anything_is_fetched() {
        let temp = tempfile::tempdir().unwrap();
        let (store, expectation) = populated_store(temp.path());

        let existing = already_installed(&store, &expectation, false).expect("already installed");
        assert_eq!(existing.version, NodeVersion::new(24, 19, 0));
        assert!(
            already_installed(&store, &expectation, true).is_none(),
            "--force must still do the work"
        );
    }

    #[test]
    fn an_empty_store_does_not_short_circuit() {
        let temp = tempfile::tempdir().unwrap();
        let store = ManagedRuntimeStore::new(temp.path().join("store"));
        let expectation = ArchiveExpectation::Declared {
            version: NodeVersion::new(24, 19, 0),
            digest: Sha256Digest::from_bytes([0u8; 32]),
        };
        assert!(already_installed(&store, &expectation, false).is_none());
    }

    #[test]
    fn uninstall_needs_a_target() {
        let store = ManagedRuntimeStore::new(std::env::temp_dir().join("rebon-node-cmd-empty"));
        let error = uninstall(&store, None, false).unwrap_err().to_string();
        assert!(error.contains("--version"), "{error}");
        let both = uninstall(&store, Some("24.19.0".into()), true)
            .unwrap_err()
            .to_string();
        assert!(both.contains("not both"), "{both}");
    }

    #[test]
    fn uninstall_all_on_an_empty_store_is_not_an_error() {
        let store = ManagedRuntimeStore::new(std::env::temp_dir().join("rebon-node-cmd-empty"));
        uninstall(&store, None, true).unwrap();
    }
}
