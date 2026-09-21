//! The managed-install state machine: verify, stage, publish, or leave the
//! store exactly as it was.
//!
//! Every failure case asserts on what is *left behind*, because the whole point
//! of staging is that a half-installed runtime is never visible to the resolver.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use rebon_node_runtime::{
    ArchiveExpectation, ArchiveExtractor, InstallError, ManagedRuntimeStore, NodeVersion,
    NodeVersionRange, Sha256Digest, SystemTarExtractor, RECEIPT_FILE_NAME, RECEIPT_SCHEMA,
};

const V24: NodeVersion = NodeVersion::new(24, 19, 0);
const V25: NodeVersion = NodeVersion::new(25, 1, 0);

/// Writes a chosen tree instead of unpacking, and counts how often it ran.
struct FakeExtractor {
    layout: Vec<&'static str>,
    runs: AtomicUsize,
}

impl FakeExtractor {
    fn new(layout: &[&'static str]) -> Self {
        Self {
            layout: layout.to_vec(),
            runs: AtomicUsize::new(0),
        }
    }

    fn unix_payload() -> Self {
        Self::new(&[
            "node-v24.19.0-linux-x64/bin/node",
            "node-v24.19.0-linux-x64/README.md",
        ])
    }

    fn runs(&self) -> usize {
        self.runs.load(Ordering::Relaxed)
    }
}

impl ArchiveExtractor for FakeExtractor {
    fn extract(&self, _archive: &Path, destination: &Path) -> Result<(), String> {
        self.runs.fetch_add(1, Ordering::Relaxed);
        for entry in &self.layout {
            let path = destination.join(entry.replace('/', std::path::MAIN_SEPARATOR_STR));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"payload").unwrap();
        }
        Ok(())
    }
}

struct FailingExtractor;

impl ArchiveExtractor for FailingExtractor {
    fn extract(&self, _archive: &Path, destination: &Path) -> Result<(), String> {
        // Leave debris behind: a real tar failure can too.
        fs::write(destination.join("half-written"), b"x").unwrap();
        Err("tar exited with 2".into())
    }
}

fn archive(dir: &Path, bytes: &[u8]) -> PathBuf {
    let path = dir.join("node.tar.gz");
    fs::write(&path, bytes).unwrap();
    path
}

fn expect(archive: &Path, version: NodeVersion) -> ArchiveExpectation {
    ArchiveExpectation::Declared {
        version,
        digest: Sha256Digest::of_file(archive).unwrap(),
    }
}

/// Entries in the store root other than published version directories.
fn debris(store: &ManagedRuntimeStore) -> Vec<String> {
    fs::read_dir(store.root())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| NodeVersion::parse(name).is_err())
        .collect()
}

#[test]
fn a_verified_archive_publishes_a_readable_runtime() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");
    let extractor = FakeExtractor::unix_payload();

    let installed = store
        .install(&archive, &expect(&archive, V24), &extractor, false)
        .unwrap();

    assert_eq!(installed.version, V24);
    assert!(installed.executable.is_file());
    assert!(installed.executable.starts_with(store.version_dir(&V24)));
    assert_eq!(installed.receipt.schema, RECEIPT_SCHEMA);
    assert_eq!(installed.receipt.version, "24.19.0");
    assert_eq!(
        installed.receipt.executable, "node-v24.19.0-linux-x64/bin/node",
        "the receipt keeps Node's own layout, so lib/ and include/ stay put"
    );
    assert_eq!(installed.receipt.provenance, "declared");
    assert_eq!(installed.receipt.archive_file_name, "node.tar.gz");

    assert_eq!(store.read(&V24), Some(installed));
    assert!(
        debris(&store).is_empty(),
        "staging must not survive a success"
    );
}

#[test]
fn a_digest_mismatch_extracts_nothing_and_leaves_no_trace() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");
    let extractor = FakeExtractor::unix_payload();

    let error = store
        .install(
            &archive,
            &ArchiveExpectation::Declared {
                version: V24,
                digest: Sha256Digest::from_bytes([0u8; 32]),
            },
            &extractor,
            false,
        )
        .unwrap_err();

    match error {
        InstallError::Integrity {
            provenance,
            expected,
            actual,
            ..
        } => {
            assert_eq!(provenance, "declared");
            assert_ne!(expected, actual);
            assert_eq!(expected, "0".repeat(64));
        }
        other => panic!("expected an integrity failure, got {other:?}"),
    }
    assert_eq!(
        extractor.runs(),
        0,
        "bytes are checked before they are unpacked"
    );
    assert_eq!(store.read(&V24), None);
    assert!(store.installed().is_empty());
}

#[test]
fn reinstalling_a_published_version_reuses_it_instead_of_unpacking_again() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");
    let extractor = FakeExtractor::unix_payload();
    let expectation = expect(&archive, V24);

    let first = store
        .install(&archive, &expectation, &extractor, false)
        .unwrap();
    let second = store
        .install(&archive, &expectation, &extractor, false)
        .unwrap();

    assert_eq!(first, second);
    assert_eq!(
        extractor.runs(),
        1,
        "a retried install keeps the working runtime"
    );
}

#[test]
fn force_replaces_the_published_tree() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");
    let expectation = expect(&archive, V24);

    store
        .install(
            &archive,
            &expectation,
            &FakeExtractor::new(&[
                "node-v24.19.0-linux-x64/bin/node",
                "node-v24.19.0-linux-x64/stale-marker",
            ]),
            false,
        )
        .unwrap();
    let stale = store
        .version_dir(&V24)
        .join("node-v24.19.0-linux-x64")
        .join("stale-marker");
    assert!(stale.is_file());

    let replaced = store
        .install(&archive, &expectation, &FakeExtractor::unix_payload(), true)
        .unwrap();

    assert!(replaced.executable.is_file());
    assert!(
        !stale.exists(),
        "a forced install replaces the tree rather than merging into it"
    );
    assert!(debris(&store).is_empty());
}

/// A directory left by an interrupted install has no receipt. It must read as
/// "not installed" — anything else points the plugin host at a partial tree.
#[test]
fn a_tree_without_a_receipt_is_not_an_install() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let orphan = store.version_dir(&V24);
    fs::create_dir_all(orphan.join("bin")).unwrap();
    fs::write(orphan.join("bin").join("node"), b"payload").unwrap();

    assert_eq!(store.read(&V24), None);
    assert!(store.installed().is_empty());

    // …and a later install takes the directory over.
    let archive = archive(temp.path(), b"pretend tarball");
    let installed = store
        .install(
            &archive,
            &expect(&archive, V24),
            &FakeExtractor::unix_payload(),
            false,
        )
        .unwrap();
    assert!(installed.executable.is_file());
}

#[test]
fn a_receipt_that_names_another_version_is_ignored() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");
    store
        .install(
            &archive,
            &expect(&archive, V24),
            &FakeExtractor::unix_payload(),
            false,
        )
        .unwrap();

    let receipt_path = store.version_dir(&V24).join(RECEIPT_FILE_NAME);
    let mut receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    receipt["version"] = serde_json::json!("25.1.0");
    fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();

    assert_eq!(
        store.read(&V24),
        None,
        "a moved or hand-edited tree is not trusted"
    );
    assert_eq!(
        store.read(&V25),
        None,
        "and it does not answer for the version it claims"
    );
}

#[test]
fn a_receipt_from_a_newer_schema_is_ignored() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");
    store
        .install(
            &archive,
            &expect(&archive, V24),
            &FakeExtractor::unix_payload(),
            false,
        )
        .unwrap();

    let receipt_path = store.version_dir(&V24).join(RECEIPT_FILE_NAME);
    let mut receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    receipt["schema"] = serde_json::json!(RECEIPT_SCHEMA + 1);
    fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();

    assert_eq!(store.read(&V24), None);
}

#[test]
fn a_receipt_pointing_outside_the_version_directory_is_ignored() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");
    store
        .install(
            &archive,
            &expect(&archive, V24),
            &FakeExtractor::unix_payload(),
            false,
        )
        .unwrap();

    let receipt_path = store.version_dir(&V24).join(RECEIPT_FILE_NAME);
    for hostile in ["../../node", "/usr/bin/node", ""] {
        let mut receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
        receipt["executable"] = serde_json::json!(hostile);
        fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert_eq!(store.read(&V24), None, "`{hostile}` must not resolve");
    }
}

#[test]
fn an_archive_without_a_node_executable_is_refused_and_publishes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");

    let error = store
        .install(
            &archive,
            &expect(&archive, V24),
            &FakeExtractor::new(&["node-v24.19.0-linux-x64/bin/npm"]),
            false,
        )
        .unwrap_err();

    assert!(
        matches!(error, InstallError::NoExecutable { .. }),
        "{error:?}"
    );
    assert!(!store.version_dir(&V24).exists());
    assert!(
        debris(&store).is_empty(),
        "staging is cleaned up after a refusal"
    );
}

#[test]
fn an_extractor_failure_cleans_up_after_itself() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");

    let error = store
        .install(&archive, &expect(&archive, V24), &FailingExtractor, false)
        .unwrap_err();

    assert!(matches!(error, InstallError::Extract { .. }), "{error:?}");
    assert!(error.to_string().contains("tar exited with 2"));
    assert!(!store.version_dir(&V24).exists());
    assert!(debris(&store).is_empty());
}

/// Windows archives put `node.exe` at the payload root instead of under `bin/`.
#[test]
fn a_flat_windows_payload_resolves_too() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend zip");

    let installed = store
        .install(
            &archive,
            &expect(&archive, V24),
            &FakeExtractor::new(&["node-v24.19.0-win-x64/node.exe"]),
            false,
        )
        .unwrap();

    assert_eq!(
        installed.receipt.executable,
        "node-v24.19.0-win-x64/node.exe"
    );
    assert!(installed.executable.is_file());
}

/// An archive that unpacks straight into the destination, with no wrapper
/// directory, is just as valid — an operator repacking a runtime may produce it.
#[test]
fn a_payload_without_a_wrapper_directory_resolves() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");

    let installed = store
        .install(
            &archive,
            &expect(&archive, V24),
            &FakeExtractor::new(&["bin/node"]),
            false,
        )
        .unwrap();

    assert_eq!(installed.receipt.executable, "bin/node");
}

/// Two wrapper directories mean the archive is not shaped like a Node
/// distribution, and guessing which one holds the runtime is how the wrong
/// binary gets installed.
#[test]
fn an_ambiguous_payload_is_refused_rather_than_guessed_at() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");

    let error = store
        .install(
            &archive,
            &expect(&archive, V24),
            &FakeExtractor::new(&["one/bin/node", "two/bin/node"]),
            false,
        )
        .unwrap_err();

    assert!(
        matches!(error, InstallError::NoExecutable { .. }),
        "{error:?}"
    );
}

#[test]
fn listing_is_newest_first_and_range_selection_follows_it() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");
    let digest = Sha256Digest::of_file(&archive).unwrap();

    for version in [V24, NodeVersion::new(24, 20, 3), V25] {
        store
            .install(
                &archive,
                &ArchiveExpectation::Declared { version, digest },
                &FakeExtractor::new(&["bin/node"]),
                false,
            )
            .unwrap();
    }
    // Debris and foreign names in the store root must not disturb the listing.
    fs::create_dir_all(store.root().join("not-a-version")).unwrap();
    fs::write(store.root().join("stray-file"), b"x").unwrap();

    let versions: Vec<NodeVersion> = store.installed().iter().map(|r| r.version).collect();
    assert_eq!(versions, vec![V25, NodeVersion::new(24, 20, 3), V24]);

    let supported = NodeVersionRange::new(V24, NodeVersion::new(25, 0, 0));
    assert_eq!(
        store.best_in_range(supported).unwrap().version,
        NodeVersion::new(24, 20, 3),
        "the newest install inside the range wins"
    );
    assert_eq!(
        store.best_in_range(NodeVersionRange::new(
            NodeVersion::new(26, 0, 0),
            NodeVersion::new(27, 0, 0)
        )),
        None
    );
}

#[test]
fn removing_reports_whether_anything_was_there() {
    let temp = tempfile::tempdir().unwrap();
    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let archive = archive(temp.path(), b"pretend tarball");
    store
        .install(
            &archive,
            &expect(&archive, V24),
            &FakeExtractor::new(&["bin/node"]),
            false,
        )
        .unwrap();

    assert!(store.remove(&V24).unwrap());
    assert!(!store.remove(&V24).unwrap());
    assert_eq!(store.read(&V24), None);
}

/// The production extractor, against an archive `tar` itself produced. Skipped
/// where `tar` is absent rather than failing: the release gate runs on machines
/// that have it, and every other test here covers the state machine.
#[test]
fn the_system_extractor_round_trips_a_real_tarball() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("node-v24.19.0-linux-x64");
    fs::create_dir_all(source.join("bin")).unwrap();
    fs::write(
        source.join("bin").join("node"),
        b"#!/bin/sh\necho 24.19.0\n",
    )
    .unwrap();

    let tarball = temp.path().join("node.tar.gz");
    let mut pack = std::process::Command::new("tar");
    pack.arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(temp.path())
        .arg("node-v24.19.0-linux-x64");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        pack.creation_flags(0x0800_0000);
    }
    let Ok(status) = pack.status() else {
        eprintln!("skipping: tar is not on PATH");
        return;
    };
    assert!(status.success(), "packing the fixture failed with {status}");

    let store = ManagedRuntimeStore::new(temp.path().join("store"));
    let installed = store
        .install(&tarball, &expect(&tarball, V24), &SystemTarExtractor, false)
        .unwrap();

    assert_eq!(
        installed.receipt.executable,
        "node-v24.19.0-linux-x64/bin/node"
    );
    assert_eq!(
        fs::read(&installed.executable).unwrap(),
        b"#!/bin/sh\necho 24.19.0\n"
    );
}
