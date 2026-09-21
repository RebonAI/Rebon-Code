//! The shape of "a plugin that ships a binary".
//!
//! Three rules
//! hold for every `[[bin]]` under `crates/plugins/`, and each of them is a way
//! the packaging or the product boundary breaks quietly if it stops holding:
//!
//! - the name starts with `rebon-`, because the binary lands in the same
//!   directory as `rebon` in an npm install and in the desktop bundle —
//!   except for the names in [`FROZEN_NAMES`], which are install contracts
//!   older than the rule;
//! - the source is `src/bin/<name>.rs`, so the manifest can be derived by
//!   globbing that directory as well as by reading `cargo metadata` — the
//!   packaging script uses one and the desktop app's build script (a
//!   separate product, https://reboncode.ai) uses the other, and the two
//!   readings have to agree;
//! - the crate does not depend on `rebon-cli`, which is the dependency
//!   direction this whole arrangement exists to reverse.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The names `rebon`'s deprecated shells forward to, from `main.rs`'s
/// `BROWSER_MCP_SIBLING` / `LSP_MCP_SIBLING` / `COMPUTER_USE_SIBLING`.
///
/// Repeated here rather than imported because `rebon-cli` is a binary crate
/// with nothing to import from. A shell pointing at a name no plugin declares
/// is a `rebon` that says "command not found" for a feature that shipped.
const FORWARDED_SIBLINGS: [&str; 3] = ["rebon-browser-mcp", "rebon-lsp-mcp", "rebon-computer-use"];

/// Plugin binaries whose names are frozen by a contract older than the
/// `rebon-` rule, and so are exempt from it.
///
/// `sandbox-win` is the only one: its installation contract
/// §2.4 freezes the file name `sandbox-win.exe`, because the caller finds the
/// helper by that name in three places it does not control — beside the
/// running executable, `%ProgramFiles%\Rebon\`, and `%LOCALAPPDATA%\Rebon\` —
/// and the npm payload ships it under that name too. Renaming it would leave
/// every already-installed helper unfindable. The other two rules below (the
/// `src/bin/<name>.rs` path and the ban on depending on `rebon-cli`) apply to
/// it unchanged.
const FROZEN_NAMES: [&str; 1] = ["sandbox-win"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the harness crate sits two levels under the workspace root")
        .to_path_buf()
}

fn plugins_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the harness crate sits two levels under the workspace root")
        .join("crates")
        .join("plugins")
}

/// One `[[bin]]` declaration: the crate it belongs to, its name and its path.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BinTarget {
    crate_dir: String,
    name: String,
    path: String,
}

/// Reads the `[[bin]]` sections out of a `Cargo.toml`.
///
/// A hand-rolled scan rather than a TOML dependency: the manifests here
/// declare `name` and `path` as bare single-line strings, and a test that
/// pins the repository's layout should not need a parser to do it.
fn bin_targets(crate_dir: &str, manifest: &str) -> Vec<BinTarget> {
    let mut targets = Vec::new();
    let mut in_bin = false;
    let mut name = None;
    let mut path = None;
    let flush = |name: &mut Option<String>, path: &mut Option<String>, out: &mut Vec<_>| {
        if let Some(name) = name.take() {
            out.push(BinTarget {
                crate_dir: crate_dir.to_string(),
                name,
                path: path.take().unwrap_or_default(),
            });
        } else {
            *path = None;
        }
    };
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if in_bin {
                flush(&mut name, &mut path, &mut targets);
            }
            in_bin = trimmed == "[[bin]]";
            continue;
        }
        if !in_bin {
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("name") {
            name = unquote(value);
        } else if let Some(value) = trimmed.strip_prefix("path") {
            path = unquote(value);
        }
    }
    if in_bin {
        flush(&mut name, &mut path, &mut targets);
    }
    targets
}

fn unquote(value: &str) -> Option<String> {
    let value = value.trim_start().strip_prefix('=')?.trim();
    let inner = value.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

fn declared_targets() -> Vec<BinTarget> {
    let mut targets = Vec::new();
    for entry in std::fs::read_dir(plugins_root()).expect("crates/plugins is readable") {
        let entry = entry.expect("directory entry");
        if !entry.file_type().expect("file type").is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("Cargo.toml");
        let Ok(manifest) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let crate_dir = entry.file_name().to_string_lossy().into_owned();
        targets.extend(bin_targets(&crate_dir, &manifest));
    }
    targets.sort();
    targets
}

#[test]
fn every_plugin_binary_is_named_and_placed_the_same_way() {
    let targets = declared_targets();
    assert!(
        !targets.is_empty(),
        "no [[bin]] under crates/plugins; the packaging manifest would be empty"
    );
    for target in &targets {
        assert!(
            target.name.starts_with("rebon-") || FROZEN_NAMES.contains(&target.name.as_str()),
            "{}: `{}` ships beside `rebon` and must be named `rebon-<feature>` \
             unless it is one of the frozen names {FROZEN_NAMES:?}",
            target.crate_dir,
            target.name
        );
        let expected = format!("src/bin/{}.rs", target.name);
        assert_eq!(
            target.path, expected,
            "{}: `{}` must be declared at {expected}",
            target.crate_dir, target.name
        );
        let source = plugins_root().join(&target.crate_dir).join(&target.path);
        assert!(source.is_file(), "{} does not exist", source.display());
    }
}

#[test]
fn globbing_src_bin_finds_exactly_the_declared_binaries() {
    // The packaging script derives its manifest from `cargo metadata`; the
    // desktop app's build script derives it by globbing. This is the
    // assertion that lets them be two readings of one fact rather than
    // two lists.
    let declared: BTreeSet<(String, String)> = declared_targets()
        .into_iter()
        .map(|target| (target.crate_dir, target.name))
        .collect();
    let mut found = BTreeSet::new();
    for entry in std::fs::read_dir(plugins_root()).expect("crates/plugins is readable") {
        let entry = entry.expect("directory entry");
        let bin_dir = entry.path().join("src").join("bin");
        let Ok(sources) = std::fs::read_dir(&bin_dir) else {
            continue;
        };
        for source in sources {
            let source = source.expect("directory entry").path();
            if source.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let stem = source
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("a .rs file has a stem")
                .to_string();
            found.insert((entry.file_name().to_string_lossy().into_owned(), stem));
        }
    }
    assert_eq!(declared, found);
}

#[test]
fn no_plugin_that_ships_a_binary_depends_on_the_cli() {
    for target in declared_targets() {
        let manifest =
            std::fs::read_to_string(plugins_root().join(&target.crate_dir).join("Cargo.toml"))
                .expect("manifest is readable");
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            assert!(
                !trimmed.starts_with("rebon-cli"),
                "{} depends on rebon-cli; dependencies only point at the kernel",
                target.crate_dir
            );
        }
    }
}

#[test]
fn the_forwarding_shells_write_nothing_to_stdout() {
    // Two of the three shells sit in front of an MCP server whose stdout *is*
    // the protocol channel, so one stray byte breaks the client that spawned
    // `rebon`. The deprecation notice and every error go to stderr.
    //
    // Read from disk rather than asserted by spawning: the shell replaces this
    // process on Unix and the only way to observe it is a real install, which
    // a unit test does not have. What can be checked cheaply is that the file
    // contains no way to reach stdout at all.
    let source = std::fs::read_to_string(
        workspace_root()
            .join("crates")
            .join("rebon-cli")
            .join("src")
            .join("sibling_command.rs"),
    )
    .expect("the forwarding shells are readable");
    for (line_number, line) in source.lines().enumerate() {
        // `eprintln!` ends in `println!`, and it is the whole point of this
        // file, so the stderr macros come out before the stdout ones go in.
        let code = line
            .split("//")
            .next()
            .unwrap_or_default()
            .replace("eprintln!", "")
            .replace("eprint!", "");
        for forbidden in ["println!", "print!", "stdout"] {
            assert!(
                !code.contains(forbidden),
                "sibling_command.rs:{}: `{forbidden}` writes to stdout, which is \
                 the MCP protocol channel for two of the three shells",
                line_number + 1
            );
        }
    }
}

#[test]
fn every_forwarded_subcommand_names_a_declared_binary() {
    let declared: BTreeSet<String> = declared_targets()
        .into_iter()
        .map(|target| target.name)
        .collect();
    for sibling in FORWARDED_SIBLINGS {
        assert!(
            declared.contains(sibling),
            "`rebon` forwards to `{sibling}`, which no plugin declares: {declared:?}"
        );
    }
}
