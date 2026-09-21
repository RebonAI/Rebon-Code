//! The supported Node range is one fact stored in six places, five of which
//! are not compiled: the host package's `engines`, the composition runtime
//! package's, the release workflow's gate, and the two READMEs. Only one of
//! them can be wrong quietly — these tests read the others back so a range
//! change that misses one fails here instead of at a user's install.

use std::path::{Path, PathBuf};

use rebon_node_runtime::{
    NodeVersion, NodeVersionRange, PINNED_NODE_VERSION, SUPPORTED_NODE_VERSIONS,
};

fn repo_file(relative: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> sits two levels under the repository root");
    root.join(relative)
}

fn read(relative: &str) -> String {
    let path = repo_file(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("cannot read {}: {error}", path.display());
    })
}

/// Parses the npm `engines` spelling this repository uses: `>=A.B.C <D[.E[.F]]`.
/// The upper bound is allowed to be a bare major because that is how the
/// package file writes it; missing components are zero, which is what npm means.
fn parse_engines(expression: &str) -> NodeVersionRange {
    let mut parts = expression.split_whitespace();
    let lower = parts
        .next()
        .and_then(|part| part.strip_prefix(">="))
        .unwrap_or_else(|| panic!("`{expression}` does not start with >="));
    let upper = parts
        .next()
        .and_then(|part| part.strip_prefix('<'))
        .unwrap_or_else(|| panic!("`{expression}` has no < upper bound"));
    assert!(
        parts.next().is_none(),
        "`{expression}` has more clauses than this parser understands"
    );
    NodeVersionRange::new(
        NodeVersion::parse(lower).expect("lower bound is a full version"),
        parse_padded(upper),
    )
}

fn parse_padded(text: &str) -> NodeVersion {
    let mut components = text.split('.').map(|part| {
        part.parse::<u32>()
            .unwrap_or_else(|_| panic!("`{text}` is not a version"))
    });
    NodeVersion::new(
        components.next().expect("at least a major"),
        components.next().unwrap_or(0),
        components.next().unwrap_or(0),
    )
}

#[test]
fn the_host_packages_engines_field_matches_the_compiled_range() {
    let package: serde_json::Value =
        serde_json::from_str(&read("runtimes/node/plugin-host/package.json"))
            .expect("valid package.json");
    let engines = package["engines"]["node"]
        .as_str()
        .expect("engines.node is a string");
    assert_eq!(
        parse_engines(engines),
        SUPPORTED_NODE_VERSIONS,
        "runtimes/node/plugin-host/package.json declares `{engines}`"
    );
}

/// The composition runtime is a second Node package, and it runs the vendored
/// dsh payload on the same runtime the host does. Two packages declaring two
/// ranges would mean a Node that satisfies one and not the other — installable,
/// launchable, and broken only once a composition is loaded.
#[test]
fn the_compose_runtime_package_declares_the_same_range() {
    let package: serde_json::Value =
        serde_json::from_str(&read("runtimes/node/compose-runtime/package.json"))
            .expect("valid package.json");
    let engines = package["engines"]["node"]
        .as_str()
        .expect("engines.node is a string");
    assert_eq!(
        parse_engines(engines),
        SUPPORTED_NODE_VERSIONS,
        "runtimes/node/compose-runtime/package.json declares `{engines}`"
    );
}

/// The workflow gate pins one exact build. That build has to be the one a
/// managed install produces, otherwise CI proves the host works on a runtime
/// Rebon never installs.
#[test]
fn the_release_workflow_gate_pins_the_version_managed_installs_produce() {
    let workflow = read(".github/workflows/release.yml");
    let needle = format!("if ($version -ne '{PINNED_NODE_VERSION}')");
    assert!(
        workflow.contains(&needle),
        "expected the Node gate in .github/workflows/release.yml to pin {PINNED_NODE_VERSION}; \
         looked for `{needle}`"
    );
    assert!(
        SUPPORTED_NODE_VERSIONS.contains(&PINNED_NODE_VERSION),
        "the pinned build must itself be supported"
    );
}

#[test]
fn the_node_package_readmes_quote_the_same_range() {
    let quoted = format!(
        "`>={} <{}`",
        SUPPORTED_NODE_VERSIONS.min_inclusive, SUPPORTED_NODE_VERSIONS.max_exclusive.major
    );
    for readme in [
        "runtimes/node/plugin-host/README.md",
        "runtimes/node/compose-runtime/README.md",
    ] {
        assert!(
            read(readme).contains(&quoted),
            "expected {readme} to state the supported runtime as {quoted}"
        );
    }
}

/// A dependency Rebon installs must be one the host package would accept, and
/// a lockfile that has grown real dependencies changes what "install the host"
/// means — the lock ships with the binary, so its shape is pinned here.
#[test]
fn the_host_lockfile_agrees_with_the_package_it_locks() {
    let package: serde_json::Value =
        serde_json::from_str(&read("runtimes/node/plugin-host/package.json"))
            .expect("valid package.json");
    let lock: serde_json::Value =
        serde_json::from_str(&read("runtimes/node/plugin-host/package-lock.json"))
            .expect("valid package-lock.json");

    assert_eq!(
        lock["name"], package["name"],
        "lockfile names another package"
    );
    assert_eq!(lock["version"], package["version"], "lockfile is stale");
    assert_eq!(
        lock["packages"][""]["engines"]["node"], package["engines"]["node"],
        "the lockfile's root entry declares a different engines range"
    );

    let packages = lock["packages"].as_object().expect("packages is an object");
    let dependencies: Vec<&String> = packages.keys().filter(|key| !key.is_empty()).collect();
    assert!(
        dependencies.is_empty(),
        "the plugin host is a zero-dependency package; the lockfile now carries {dependencies:?}, \
         which the installer would have to vendor and verify"
    );
}
