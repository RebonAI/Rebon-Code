//! Runtimes Rebon put on the machine itself.
//!
//! Two constraints shape this module.
//!
//! **It lands in the user's data directory, never beside the binary.** An
//! installed Rebon may sit under `Program Files` or `/usr/local`; a per-user
//! install has to work without an administrator, and one user's runtime must
//! not be another's.
//!
//! **A half-written runtime must never be visible.** Extraction happens in a
//! staging directory under the same root — same filesystem, so the publish step
//! is a directory rename — and the receipt is written *inside* staging before
//! that rename. A version directory therefore either has a receipt and an
//! executable, or it was never published. Anything else on disk is debris from
//! an interrupted install and is ignored on read.
//!
//! The digest is checked before a single byte is extracted, so a corrupted or
//! substituted archive never reaches the filesystem at all.

use std::{
    fmt, fs, io,
    path::{Component, Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    integrity::{ArchiveExpectation, DigestError, Sha256Digest},
    version::{NodeVersion, NodeVersionRange},
};

/// Written last inside staging, so its presence means the install completed.
pub const RECEIPT_FILE_NAME: &str = "rebon-node-receipt.json";

/// Managed runtimes live under `<config home>/runtime/node/<version>/`.
pub fn default_store_root(config_home: &Path) -> PathBuf {
    config_home.join("runtime").join("node")
}

/// What a published version directory says about itself.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InstallReceipt {
    /// Bumped when the fields below change meaning. A receipt from a future
    /// schema is ignored rather than guessed at, which downgrades to "not
    /// installed" instead of pointing a plugin host at an unknown layout.
    pub schema: u32,
    pub version: String,
    /// Path to the executable, relative to the version directory, always with
    /// `/` separators so a store copied between platforms still reads.
    pub executable: String,
    pub archive_file_name: String,
    pub archive_sha256: String,
    /// `pinned` or `declared` — see [`ArchiveExpectation`].
    pub provenance: String,
}

pub const RECEIPT_SCHEMA: u32 = 1;

/// A published managed runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledRuntime {
    pub version: NodeVersion,
    /// Absolute path to the Node executable.
    pub executable: PathBuf,
    pub receipt: InstallReceipt,
}

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("{action}: {source}")]
    Io {
        action: String,
        #[source]
        source: io::Error,
    },
    #[error(
        "archive {archive} has SHA-256 {actual}, expected the {provenance} digest {expected} — \
         refusing to install"
    )]
    Integrity {
        archive: String,
        provenance: &'static str,
        expected: String,
        actual: String,
    },
    #[error("cannot hash the archive: {0}")]
    Digest(#[from] DigestError),
    #[error("unpacking {archive} failed: {message}")]
    Extract { archive: String, message: String },
    #[error("{archive} unpacked without a Node executable (looked for bin/node and node.exe)")]
    NoExecutable { archive: String },
}

fn io_error(action: impl Into<String>) -> impl FnOnce(io::Error) -> InstallError {
    let action = action.into();
    move |source| InstallError::Io { action, source }
}

/// Unpacking is injected so the install state machine — verify, stage, publish,
/// roll back — is testable without a real archive or a `tar` on PATH.
pub trait ArchiveExtractor {
    fn extract(&self, archive: &Path, destination: &Path) -> Result<(), String>;
}

/// `tar -xf`, which reads both the `.tar.gz` and the `.zip` Node publishes:
/// bsdtar ships with Windows 10+, GNU/BSD tar with macOS and Linux. Shelling
/// out keeps two decompressors and a zip implementation out of the dependency
/// graph.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemTarExtractor;

impl ArchiveExtractor for SystemTarExtractor {
    fn extract(&self, archive: &Path, destination: &Path) -> Result<(), String> {
        let mut command = Command::new("tar");
        command.arg("-xf").arg(archive).arg("-C").arg(destination);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let output = command
            .output()
            .map_err(|error| format!("running tar failed ({error}); is tar on PATH?"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "tar exited with {}: {}",
            output.status,
            stderr.trim()
        ))
    }
}

/// The set of managed runtimes under one root.
#[derive(Clone, Debug)]
pub struct ManagedRuntimeStore {
    root: PathBuf,
}

impl ManagedRuntimeStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The store Rebon uses in production, under the config home.
    pub fn under_config_home(config_home: &Path) -> Self {
        Self::new(default_store_root(config_home))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn version_dir(&self, version: &NodeVersion) -> PathBuf {
        self.root.join(version.to_string())
    }

    /// Reads one published version, or `None` if it was never published, was
    /// left half-installed, or no longer holds the executable it recorded.
    pub fn read(&self, version: &NodeVersion) -> Option<InstalledRuntime> {
        let dir = self.version_dir(version);
        let receipt: InstallReceipt =
            serde_json::from_slice(&fs::read(dir.join(RECEIPT_FILE_NAME)).ok()?).ok()?;
        if receipt.schema != RECEIPT_SCHEMA {
            return None;
        }
        // The directory name is the identity a caller asked for; a receipt that
        // names something else means the tree was moved or hand-edited.
        if NodeVersion::parse(&receipt.version).ok()? != *version {
            return None;
        }
        let executable = join_relative(&dir, &receipt.executable)?;
        if !executable.is_file() {
            return None;
        }
        Some(InstalledRuntime {
            version: *version,
            executable,
            receipt,
        })
    }

    /// Every published runtime, newest first. Debris and foreign directories are
    /// skipped silently: this is a read of a cache, not a validation pass.
    pub fn installed(&self) -> Vec<InstalledRuntime> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut runtimes: Vec<InstalledRuntime> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let version = NodeVersion::parse(name.to_str()?).ok()?;
                self.read(&version)
            })
            .collect();
        runtimes.sort_by_key(|runtime| std::cmp::Reverse(runtime.version));
        runtimes
    }

    /// The newest published runtime inside `range`.
    pub fn best_in_range(&self, range: NodeVersionRange) -> Option<InstalledRuntime> {
        self.installed()
            .into_iter()
            .find(|runtime| range.contains(&runtime.version))
    }

    /// Verifies, stages, and publishes `archive`.
    ///
    /// Re-running with an already published version is a no-op that returns the
    /// existing install: an interrupted download that is retried must not throw
    /// away a working runtime. `force` replaces it instead.
    pub fn install(
        &self,
        archive: &Path,
        expectation: &ArchiveExpectation,
        extractor: &dyn ArchiveExtractor,
        force: bool,
    ) -> Result<InstalledRuntime, InstallError> {
        let version = expectation.version();
        if let Some(existing) = self.read(&version) {
            if !force {
                return Ok(existing);
            }
        }

        let actual = Sha256Digest::of_file(archive)?;
        let expected = expectation.digest();
        if actual != expected {
            return Err(InstallError::Integrity {
                archive: archive.display().to_string(),
                provenance: expectation.provenance(),
                expected: expected.to_hex(),
                actual: actual.to_hex(),
            });
        }

        fs::create_dir_all(&self.root)
            .map_err(io_error(format!("creating {}", self.root.display())))?;
        let staging = self.staging_dir();
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)
            .map_err(io_error(format!("creating {}", staging.display())))?;

        let published = self.stage_and_publish(
            archive,
            expectation,
            extractor,
            force,
            &staging,
            &version,
            actual,
        );
        // Staging is gone on the happy path (it was renamed); this clears it
        // after any failure so a retry does not trip over the debris.
        let _ = fs::remove_dir_all(&staging);
        published
    }

    #[allow(clippy::too_many_arguments)]
    fn stage_and_publish(
        &self,
        archive: &Path,
        expectation: &ArchiveExpectation,
        extractor: &dyn ArchiveExtractor,
        force: bool,
        staging: &Path,
        version: &NodeVersion,
        digest: Sha256Digest,
    ) -> Result<InstalledRuntime, InstallError> {
        extractor
            .extract(archive, staging)
            .map_err(|message| InstallError::Extract {
                archive: archive.display().to_string(),
                message,
            })?;

        // Node's archives unpack into a single `node-vX.Y.Z-<platform>/`
        // directory. Keeping that layout — rather than flattening it — is what
        // lets `lib/` and `include/` stay where the runtime expects them.
        let (payload_root, relative) =
            locate_executable(staging).ok_or_else(|| InstallError::NoExecutable {
                archive: archive.display().to_string(),
            })?;

        let receipt = InstallReceipt {
            schema: RECEIPT_SCHEMA,
            version: version.to_string(),
            executable: to_receipt_path(&payload_root.join(&relative), staging),
            archive_file_name: archive
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            archive_sha256: digest.to_hex(),
            provenance: expectation.provenance().to_string(),
        };
        let encoded =
            serde_json::to_vec_pretty(&receipt).expect("receipt is plain strings and one integer");
        fs::write(staging.join(RECEIPT_FILE_NAME), encoded)
            .map_err(io_error("writing the install receipt"))?;

        let destination = self.version_dir(version);
        if destination.exists() {
            if !force {
                // Another process published while this one was unpacking.
                if let Some(existing) = self.read(version) {
                    return Ok(existing);
                }
            }
            // Either a forced replacement or unpublishable debris. Move it aside
            // first so the publish stays a single rename.
            let discarded = self.staging_dir();
            fs::rename(&destination, &discarded)
                .map_err(io_error(format!("replacing {}", destination.display())))?;
            let _ = fs::remove_dir_all(&discarded);
        }

        fs::rename(staging, &destination)
            .map_err(io_error(format!("publishing {}", destination.display())))?;

        self.read(version)
            .ok_or_else(|| InstallError::NoExecutable {
                archive: archive.display().to_string(),
            })
    }

    pub fn remove(&self, version: &NodeVersion) -> Result<bool, InstallError> {
        let dir = self.version_dir(version);
        if !dir.exists() {
            return Ok(false);
        }
        fs::remove_dir_all(&dir).map_err(io_error(format!("removing {}", dir.display())))?;
        Ok(true)
    }

    /// A name no published version can collide with: [`NodeVersion::parse`]
    /// rejects a leading `.`, so `installed()` skips these even mid-install.
    fn staging_dir(&self) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ticket = COUNTER.fetch_add(1, Ordering::Relaxed);
        self.root
            .join(format!(".staging-{}-{ticket}", std::process::id()))
    }
}

impl fmt::Display for ManagedRuntimeStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.root.display())
    }
}

/// Finds the Node executable in a freshly unpacked tree.
///
/// Returns the directory the payload actually lives in together with the
/// executable's path relative to it. One level of descent covers the wrapper
/// directory Node's own archives carry; deeper searching is deliberately not
/// done, so an archive with an unexpected shape fails loudly instead of
/// installing whatever binary happens to be named `node`.
fn locate_executable(root: &Path) -> Option<(PathBuf, PathBuf)> {
    const CANDIDATES: [&str; 4] = ["bin/node", "bin/node.exe", "node.exe", "node"];
    let direct = |base: &Path| -> Option<PathBuf> {
        CANDIDATES.iter().find_map(|candidate| {
            let relative: PathBuf = candidate.split('/').collect();
            base.join(&relative).is_file().then_some(relative)
        })
    };
    if let Some(relative) = direct(root) {
        return Some((root.to_path_buf(), relative));
    }
    let mut children = fs::read_dir(root).ok()?.flatten();
    let only = children.next()?;
    if children.next().is_some() || !only.file_type().ok()?.is_dir() {
        return None;
    }
    let nested = only.path();
    direct(&nested).map(|relative| (nested, relative))
}

/// Renders an absolute path as a `/`-separated path relative to `base`.
fn to_receipt_path(path: &Path, base: &Path) -> String {
    let relative = path.strip_prefix(base).unwrap_or(path);
    relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Resolves a receipt's relative path under `base`, refusing anything that
/// could climb out of the version directory.
fn join_relative(base: &Path, relative: &str) -> Option<PathBuf> {
    if relative.is_empty() {
        return None;
    }
    let mut resolved = base.to_path_buf();
    for part in relative.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        let candidate = PathBuf::from(part);
        if candidate.components().count() != 1
            || !matches!(candidate.components().next(), Some(Component::Normal(_)))
        {
            return None;
        }
        resolved.push(part);
    }
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_paths_are_slash_separated_and_relative() {
        let base = Path::new("/store/24.19.0");
        assert_eq!(
            to_receipt_path(
                &base
                    .join("node-v24.19.0-linux-x64")
                    .join("bin")
                    .join("node"),
                base
            ),
            "node-v24.19.0-linux-x64/bin/node"
        );
    }

    #[test]
    fn join_relative_refuses_to_escape_the_version_directory() {
        let base = Path::new("/store/24.19.0");
        assert_eq!(
            join_relative(base, "bin/node"),
            Some(base.join("bin").join("node"))
        );
        for hostile in [
            "",
            "..",
            "../node",
            "bin/../../node",
            "/etc/passwd",
            "bin//node",
        ] {
            assert!(
                join_relative(base, hostile).is_none(),
                "expected `{hostile}` to be refused"
            );
        }
    }

    #[test]
    fn default_root_is_under_the_config_home() {
        let root = default_store_root(Path::new("/home/u/.rebon"));
        assert!(root.ends_with(Path::new("runtime").join("node")));
        assert!(root.starts_with("/home/u/.rebon"));
    }

    #[test]
    fn staging_names_are_unique_and_unparseable_as_versions() {
        let store = ManagedRuntimeStore::new("/store");
        let first = store.staging_dir();
        let second = store.staging_dir();
        assert_ne!(first, second);
        for staging in [first, second] {
            let name = staging.file_name().unwrap().to_string_lossy().into_owned();
            assert!(name.starts_with(".staging-"));
            assert!(NodeVersion::parse(&name).is_err());
        }
    }
}
