//! The Node runtime that hosts the plugin plane: which builds are allowed, how
//! one is found, and how Rebon installs one when the machine has none.
//!
//! Rebon does not embed a JavaScript engine for plugins and does not bundle
//! Node. `npm install -g @rebon/cli` implies a Node on the machine; `cargo
//! install` and a downloaded binary do not. So the runtime has to be *resolved*,
//! and the answer has to be reproducible enough to put in an error message.
//!
//! # Resolution is a ladder, not a search
//!
//! 1. [`NODE_EXECUTABLE_ENV`] — an absolute path someone chose deliberately.
//!    This is also the channel a parent process uses to hand a worker the
//!    runtime it resolved, because a worker runs in a separate `rebon` process
//!    that may not share the parent's `PATH`.
//! 2. A managed install under the config home ([`ManagedRuntimeStore`]).
//! 3. `node` on `PATH` — the npm-distributed case, where npm's own installer
//!    already put a runtime there.
//!
//! Every rung is version-gated against [`SUPPORTED_NODE_VERSIONS`], and a
//! failure at rung 1 is fatal rather than a fall-through: a demand that quietly
//! degrades to a different runtime would run plugins on something nobody vetted.
//!
//! Rung 3 is a deliberate policy choice, not an oversight. Plugins are trusted
//! local code, so the user's own Node is the right runtime for them and
//! refusing `PATH` would only make the common install fail. Components that
//! execute untrusted, model-authored code read this ladder differently and take
//! only a managed install — which is vetted by construction, because nothing is
//! unpacked until its bytes match a digest compiled into this binary.
//!
//! # Installing is fail-closed
//!
//! A managed install accepts an archive only when its SHA-256 matches a digest
//! compiled into this binary ([`PINNED_ARCHIVES`]) or one an operator declared
//! explicitly. Nothing is extracted before that check passes, the unpack happens
//! in a staging directory, and publication is a single rename. See [`managed`]
//! for the guarantees that buys.
//!
//! Downloading is the caller's job — this crate never opens a socket — but the
//! policy that says whether downloading is allowed at all lives here, so every
//! caller answers that question the same way.

pub mod integrity;
pub mod managed;
pub mod probe;
pub mod version;

use std::{
    fmt,
    path::{Path, PathBuf},
};

use thiserror::Error;

pub use integrity::{
    host_platform, pinned_archive, pinned_archive_for_host, ArchiveExpectation, DigestError,
    NodePlatform, PinnedArchive, Sha256Digest, DEFAULT_NODE_DIST_BASE, PINNED_ARCHIVES,
};
pub use managed::{
    default_store_root, ArchiveExtractor, InstallError, InstallReceipt, InstalledRuntime,
    ManagedRuntimeStore, SystemTarExtractor, RECEIPT_FILE_NAME, RECEIPT_SCHEMA,
};
pub use probe::{ExecutingProbe, NodeProbe, ProbeError, RejectedCandidate, RejectionReason};
pub use version::{
    NodeVersion, NodeVersionRange, VersionError, PINNED_NODE_VERSION, SUPPORTED_NODE_VERSIONS,
};

/// Absolute path to the Node executable the plugin plane must use.
///
/// Set by whoever already resolved a runtime — the desktop app for its workers,
/// an administrator for a locked-down deployment, a developer for a checkout.
pub const NODE_EXECUTABLE_ENV: &str = "REBON_PLUGIN_NODE";

/// Set to `off` (or `0`, `no`, `false`, `never`) to forbid fetching a runtime.
pub const DOWNLOAD_POLICY_ENV: &str = "REBON_PLUGIN_NODE_DOWNLOAD";

/// Whether this machine may fetch a Node runtime over the network.
///
/// Offline and air-gapped deployments still install: [`ManagedRuntimeStore`]
/// takes a local archive, and the digest check is identical either way. The
/// policy only governs whether Rebon may go and get one by itself.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DownloadPolicy {
    #[default]
    Allowed,
    Disabled,
}

impl DownloadPolicy {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "no" | "false" | "never" | "disabled" => Self::Disabled,
            _ => Self::Allowed,
        }
    }

    pub fn from_env() -> Self {
        std::env::var(DOWNLOAD_POLICY_ENV)
            .ok()
            .map_or(Self::Allowed, |raw| Self::parse(&raw))
    }

    pub fn is_disabled(self) -> bool {
        self == Self::Disabled
    }

    /// Why an install stopped, phrased for someone who did not set the variable.
    pub fn refusal(self) -> String {
        format!(
            "downloading a Node runtime is disabled by {DOWNLOAD_POLICY_ENV}; \
             install one from a local archive instead"
        )
    }
}

/// Which rung of the ladder answered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeOrigin {
    /// Named by [`NODE_EXECUTABLE_ENV`].
    Demanded,
    /// A managed install under the config home.
    Managed,
    /// Found as `node` on `PATH`.
    SearchPath,
}

impl fmt::Display for RuntimeOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Demanded => NODE_EXECUTABLE_ENV,
            Self::Managed => "managed install",
            Self::SearchPath => "PATH",
        })
    }
}

/// A Node executable that exists, runs, and reports a supported version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedNodeRuntime {
    pub executable: PathBuf,
    pub version: NodeVersion,
    pub origin: RuntimeOrigin,
}

impl ResolvedNodeRuntime {
    /// The environment entry that hands this exact runtime to a child `rebon`
    /// process. A worker inherits the parent's environment, so applying this to
    /// the spawn is what stops the child from re-running the ladder and landing
    /// somewhere else — which is the whole point when the parent is a GUI app
    /// whose `PATH` the worker does not share.
    pub fn worker_env(&self) -> (&'static str, &Path) {
        (NODE_EXECUTABLE_ENV, self.executable.as_path())
    }
}

impl fmt::Display for ResolvedNodeRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Node {} at {} (via {})",
            self.version,
            self.executable.display(),
            self.origin
        )
    }
}

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("{NODE_EXECUTABLE_ENV} must be an absolute path, got `{0}`")]
    DemandNotAbsolute(PathBuf),
    #[error("{NODE_EXECUTABLE_ENV} points at {0}, which does not exist")]
    DemandMissing(PathBuf),
    #[error("{NODE_EXECUTABLE_ENV} points at a runtime that cannot be used: {0}")]
    DemandUnusable(#[source] ProbeError),
    #[error(
        "{NODE_EXECUTABLE_ENV} points at Node {version} ({path}), but the plugin host needs {supported}"
    )]
    DemandUnsupported {
        path: PathBuf,
        version: NodeVersion,
        supported: NodeVersionRange,
    },
    #[error("{}", unavailable_message(.supported, .rejected))]
    Unavailable {
        supported: NodeVersionRange,
        rejected: Vec<RejectedCandidate>,
    },
}

fn unavailable_message(supported: &NodeVersionRange, rejected: &[RejectedCandidate]) -> String {
    let mut message = format!("no Node runtime matching {supported} is available");
    if !rejected.is_empty() {
        let seen: Vec<String> = rejected.iter().map(RejectedCandidate::to_string).collect();
        message.push_str(" (");
        message.push_str(&seen.join("; "));
        message.push(')');
    }
    message.push_str("; install one with `rebon node install`, or point ");
    message.push_str(NODE_EXECUTABLE_ENV);
    message.push_str(" at an existing runtime");
    message
}

/// Walks the resolution ladder.
///
/// Every input is injected so the order — and each rung's failure mode — is
/// testable without installing six Node versions.
pub struct NodeRuntimeResolver<'a> {
    probe: &'a dyn NodeProbe,
    supported: NodeVersionRange,
    demanded: Option<PathBuf>,
    store: Option<&'a ManagedRuntimeStore>,
    search_candidates: Vec<PathBuf>,
}

impl<'a> NodeRuntimeResolver<'a> {
    pub fn new(probe: &'a dyn NodeProbe) -> Self {
        Self {
            probe,
            supported: SUPPORTED_NODE_VERSIONS,
            demanded: None,
            store: None,
            search_candidates: Vec::new(),
        }
    }

    /// The production configuration: the demand from the environment, the
    /// managed store, and `PATH`.
    pub fn from_env(probe: &'a dyn NodeProbe, store: &'a ManagedRuntimeStore) -> Self {
        Self::new(probe)
            .with_demand(std::env::var_os(NODE_EXECUTABLE_ENV).map(PathBuf::from))
            .with_store(store)
            .with_search_candidates(search_path_candidates())
    }

    pub fn with_supported_range(mut self, supported: NodeVersionRange) -> Self {
        self.supported = supported;
        self
    }

    pub fn with_demand(mut self, demanded: Option<PathBuf>) -> Self {
        // An empty value is how a launcher spells "I have nothing to hand you";
        // treating it as a demand would fail every session on that machine.
        self.demanded = demanded.filter(|path| !path.as_os_str().is_empty());
        self
    }

    pub fn with_store(mut self, store: &'a ManagedRuntimeStore) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_search_candidates(mut self, candidates: Vec<PathBuf>) -> Self {
        self.search_candidates = candidates;
        self
    }

    pub fn resolve(&self) -> Result<ResolvedNodeRuntime, ResolveError> {
        if let Some(demanded) = &self.demanded {
            return self.resolve_demand(demanded);
        }

        let mut rejected = Vec::new();
        if let Some(store) = self.store {
            // The store already knows which of its installs are in range, so a
            // managed runtime that is present but too old is not a "rejected
            // candidate" — it is simply not the one being asked for.
            if let Some(installed) = store.best_in_range(self.supported) {
                match self.accept(&installed.executable, RuntimeOrigin::Managed) {
                    Ok(resolved) => return Ok(resolved),
                    Err(rejection) => rejected.push(rejection),
                }
            }
        }

        for candidate in &self.search_candidates {
            match self.accept(candidate, RuntimeOrigin::SearchPath) {
                Ok(resolved) => return Ok(resolved),
                Err(rejection) => rejected.push(rejection),
            }
        }

        Err(ResolveError::Unavailable {
            supported: self.supported,
            rejected,
        })
    }

    fn resolve_demand(&self, demanded: &Path) -> Result<ResolvedNodeRuntime, ResolveError> {
        if !demanded.is_absolute() {
            return Err(ResolveError::DemandNotAbsolute(demanded.to_path_buf()));
        }
        if !demanded.is_file() {
            return Err(ResolveError::DemandMissing(demanded.to_path_buf()));
        }
        let version = self
            .probe
            .probe(demanded)
            .map_err(ResolveError::DemandUnusable)?;
        if !self.supported.contains(&version) {
            return Err(ResolveError::DemandUnsupported {
                path: demanded.to_path_buf(),
                version,
                supported: self.supported,
            });
        }
        Ok(ResolvedNodeRuntime {
            executable: demanded.to_path_buf(),
            version,
            origin: RuntimeOrigin::Demanded,
        })
    }

    fn accept(
        &self,
        candidate: &Path,
        origin: RuntimeOrigin,
    ) -> Result<ResolvedNodeRuntime, RejectedCandidate> {
        match self.probe.probe(candidate) {
            Ok(version) if self.supported.contains(&version) => Ok(ResolvedNodeRuntime {
                executable: candidate.to_path_buf(),
                version,
                origin,
            }),
            Ok(version) => Err(RejectedCandidate {
                executable: candidate.to_path_buf(),
                reason: RejectionReason::Unsupported(version),
            }),
            Err(error) => Err(RejectedCandidate {
                executable: candidate.to_path_buf(),
                reason: RejectionReason::Unusable(error),
            }),
        }
    }
}

/// The `node` executables on `PATH`, in `PATH` order, without repeats.
///
/// Only the platform's real executable name is considered. A `node.cmd` shim
/// would be spawnable, but it is a batch file whose behaviour under argument
/// quoting differs from the binary the plugin host will actually be launched
/// with, and every Node version manager also puts the real executable on `PATH`.
///
/// Real `PATH`s repeat entries. Each repeat would otherwise cost another
/// process spawn and appear again in the rejection list of a failure message.
pub fn search_path_candidates() -> Vec<PathBuf> {
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    candidate_paths(&path, name)
        .into_iter()
        .filter(|candidate| candidate.is_file())
        .collect()
}

/// The `PATH` arithmetic, without touching the filesystem.
fn candidate_paths(path_var: &std::ffi::OsStr, name: &str) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    std::env::split_paths(path_var)
        .filter(|entry| !entry.as_os_str().is_empty())
        .map(|entry| entry.join(name))
        .filter(|candidate| seen.insert(candidate.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    /// A probe backed by a fixed table, plus a log of what was asked.
    #[derive(Default)]
    struct FakeProbe {
        answers: HashMap<PathBuf, Result<NodeVersion, &'static str>>,
        asked: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl FakeProbe {
        fn with(mut self, path: &str, version: NodeVersion) -> Self {
            self.answers.insert(PathBuf::from(path), Ok(version));
            self
        }

        fn broken(mut self, path: &str) -> Self {
            self.answers.insert(PathBuf::from(path), Err("boom"));
            self
        }
    }

    impl NodeProbe for FakeProbe {
        fn probe(&self, executable: &Path) -> Result<NodeVersion, ProbeError> {
            self.asked.lock().unwrap().push(executable.to_path_buf());
            match self.answers.get(executable) {
                Some(Ok(version)) => Ok(*version),
                Some(Err(message)) => Err(ProbeError::Failed {
                    path: executable.to_path_buf(),
                    status: "exit status: 1".into(),
                    stderr: (*message).into(),
                }),
                None => Err(ProbeError::Spawn {
                    path: executable.to_path_buf(),
                    message: "not in the fake table".into(),
                }),
            }
        }
    }

    const SUPPORTED: NodeVersion = NodeVersion::new(24, 19, 0);
    const TOO_OLD: NodeVersion = NodeVersion::new(22, 21, 1);

    #[test]
    fn path_is_used_when_nothing_else_answers() {
        let probe = FakeProbe::default().with("/usr/bin/node", SUPPORTED);
        let resolved = NodeRuntimeResolver::new(&probe)
            .with_search_candidates(vec![PathBuf::from("/usr/bin/node")])
            .resolve()
            .unwrap();
        assert_eq!(resolved.origin, RuntimeOrigin::SearchPath);
        assert_eq!(resolved.version, SUPPORTED);
    }

    #[test]
    fn path_entries_are_tried_in_order_and_the_first_supported_one_wins() {
        let probe = FakeProbe::default()
            .with("/a/node", TOO_OLD)
            .with("/b/node", SUPPORTED)
            .with("/c/node", SUPPORTED);
        let asked = Arc::clone(&probe.asked);
        let resolved = NodeRuntimeResolver::new(&probe)
            .with_search_candidates(
                ["/a/node", "/b/node", "/c/node"]
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
            )
            .resolve()
            .unwrap();
        assert_eq!(resolved.executable, PathBuf::from("/b/node"));
        assert_eq!(
            *asked.lock().unwrap(),
            vec![PathBuf::from("/a/node"), PathBuf::from("/b/node")],
            "resolution stops at the first acceptable runtime"
        );
    }

    #[test]
    fn an_unsupported_path_runtime_is_named_in_the_failure() {
        let probe = FakeProbe::default().with("/usr/bin/node", TOO_OLD);
        let error = NodeRuntimeResolver::new(&probe)
            .with_search_candidates(vec![PathBuf::from("/usr/bin/node")])
            .resolve()
            .unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("is Node 22.21.1"), "{rendered}");
        assert!(rendered.contains(">=24.19.0 <25.0.0"), "{rendered}");
        assert!(rendered.contains("rebon node install"), "{rendered}");
    }

    #[test]
    fn a_broken_path_runtime_is_reported_rather_than_swallowed() {
        let probe = FakeProbe::default().broken("/usr/bin/node");
        let error = NodeRuntimeResolver::new(&probe)
            .with_search_candidates(vec![PathBuf::from("/usr/bin/node")])
            .resolve()
            .unwrap_err();
        assert!(error.to_string().contains("is unusable"), "{error}");
    }

    #[test]
    fn nothing_anywhere_still_says_what_was_needed() {
        let probe = FakeProbe::default();
        let error = NodeRuntimeResolver::new(&probe).resolve().unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.starts_with("no Node runtime matching >=24.19.0 <25.0.0"));
        assert!(rendered.contains(NODE_EXECUTABLE_ENV));
    }

    #[test]
    fn a_relative_demand_is_refused_before_anything_is_spawned() {
        let probe = FakeProbe::default().with("node", SUPPORTED);
        let asked = Arc::clone(&probe.asked);
        let error = NodeRuntimeResolver::new(&probe)
            .with_demand(Some(PathBuf::from("node")))
            .resolve()
            .unwrap_err();
        assert!(matches!(error, ResolveError::DemandNotAbsolute(_)));
        assert!(asked.lock().unwrap().is_empty());
    }

    #[test]
    fn a_missing_demand_never_falls_through_to_path() {
        let missing = std::env::temp_dir().join("rebon-node-runtime-absent-node");
        let _ = std::fs::remove_file(&missing);
        let probe = FakeProbe::default().with("/usr/bin/node", SUPPORTED);
        let error = NodeRuntimeResolver::new(&probe)
            .with_demand(Some(missing.clone()))
            .with_search_candidates(vec![PathBuf::from("/usr/bin/node")])
            .resolve()
            .unwrap_err();
        match error {
            ResolveError::DemandMissing(path) => assert_eq!(path, missing),
            other => panic!("expected DemandMissing, got {other:?}"),
        }
    }

    /// The demand is a statement about which runtime plugins run on. Degrading
    /// to a different one because this one is old would be the failure the
    /// variable exists to prevent.
    #[test]
    fn an_out_of_range_demand_fails_instead_of_degrading() {
        let dir = tempfile::tempdir().unwrap();
        let demanded = dir.path().join("node");
        std::fs::write(&demanded, b"#!/bin/sh\n").unwrap();
        let probe = FakeProbe::default()
            .with(demanded.to_str().unwrap(), TOO_OLD)
            .with("/usr/bin/node", SUPPORTED);
        let error = NodeRuntimeResolver::new(&probe)
            .with_demand(Some(demanded.clone()))
            .with_search_candidates(vec![PathBuf::from("/usr/bin/node")])
            .resolve()
            .unwrap_err();
        match error {
            ResolveError::DemandUnsupported { path, version, .. } => {
                assert_eq!(path, demanded);
                assert_eq!(version, TOO_OLD);
            }
            other => panic!("expected DemandUnsupported, got {other:?}"),
        }
    }

    #[test]
    fn a_demand_that_cannot_run_is_its_own_error() {
        let dir = tempfile::tempdir().unwrap();
        let demanded = dir.path().join("node");
        std::fs::write(&demanded, b"not an executable").unwrap();
        let probe = FakeProbe::default().broken(demanded.to_str().unwrap());
        let error = NodeRuntimeResolver::new(&probe)
            .with_demand(Some(demanded))
            .resolve()
            .unwrap_err();
        assert!(matches!(error, ResolveError::DemandUnusable(_)));
    }

    #[test]
    fn an_empty_demand_is_not_a_demand() {
        let probe = FakeProbe::default().with("/usr/bin/node", SUPPORTED);
        let resolved = NodeRuntimeResolver::new(&probe)
            .with_demand(Some(PathBuf::new()))
            .with_search_candidates(vec![PathBuf::from("/usr/bin/node")])
            .resolve()
            .unwrap();
        assert_eq!(resolved.origin, RuntimeOrigin::SearchPath);
    }

    #[test]
    fn worker_env_hands_the_child_the_exact_executable() {
        let resolved = ResolvedNodeRuntime {
            executable: PathBuf::from("/opt/node/bin/node"),
            version: SUPPORTED,
            origin: RuntimeOrigin::Managed,
        };
        let (key, value) = resolved.worker_env();
        assert_eq!(key, NODE_EXECUTABLE_ENV);
        assert_eq!(value, Path::new("/opt/node/bin/node"));
    }

    #[test]
    fn resolution_renders_for_a_status_line() {
        let resolved = ResolvedNodeRuntime {
            executable: PathBuf::from("/opt/node/bin/node"),
            version: SUPPORTED,
            origin: RuntimeOrigin::SearchPath,
        };
        assert_eq!(
            resolved.to_string(),
            "Node 24.19.0 at /opt/node/bin/node (via PATH)"
        );
    }

    #[test]
    fn download_policy_reads_the_off_spellings() {
        for raw in ["off", "OFF", " no ", "0", "false", "never", "disabled"] {
            assert_eq!(
                DownloadPolicy::parse(raw),
                DownloadPolicy::Disabled,
                "`{raw}` should disable downloads"
            );
        }
        for raw in ["", "on", "1", "yes", "anything else"] {
            assert_eq!(
                DownloadPolicy::parse(raw),
                DownloadPolicy::Allowed,
                "`{raw}` should leave downloads allowed"
            );
        }
        assert!(DownloadPolicy::Disabled
            .refusal()
            .contains(DOWNLOAD_POLICY_ENV));
    }

    /// A real `PATH` repeats directories. Each repeat would otherwise cost a
    /// process spawn and show up twice in the failure message.
    #[test]
    fn path_candidates_keep_order_and_drop_repeats() {
        let separator = if cfg!(windows) { ";" } else { ":" };
        let raw = ["/a", "/b", "/a", "", "/c", "/a"].join(separator);
        let candidates = candidate_paths(std::ffi::OsStr::new(&raw), "node");
        assert_eq!(
            candidates,
            ["/a", "/b", "/c"]
                .into_iter()
                .map(|entry| PathBuf::from(entry).join("node"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_trailing_separator_does_not_make_a_second_candidate() {
        let separator = if cfg!(windows) { ";" } else { ":" };
        let raw = ["/a", "/a/"].join(separator);
        assert_eq!(candidate_paths(std::ffi::OsStr::new(&raw), "node").len(), 1);
    }

    #[test]
    fn search_path_candidates_only_returns_existing_files() {
        for candidate in search_path_candidates() {
            assert!(candidate.is_file());
            let name = candidate
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            assert_eq!(name, if cfg!(windows) { "node.exe" } else { "node" });
        }
    }
}
