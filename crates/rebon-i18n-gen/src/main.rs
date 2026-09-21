//! Writes every generated catalogue to the working tree.
//!
//! ```text
//! cargo run -p rebon-i18n-gen
//! ```

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|crates| crates.parent())
        .expect("the crate lives two levels under the repository root")
        .to_path_buf()
}

fn main() {
    let root = repo_root();
    let source = rebon_i18n_gen::Source::read(&root).unwrap_or_else(|error| panic!("{error}"));
    let artefacts =
        rebon_i18n_gen::artefacts(&source).unwrap_or_else(|error| panic!("i18n source: {error}"));
    for (relative, contents) in artefacts {
        let path = root.join(&relative);
        std::fs::write(&path, contents.as_bytes())
            .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        println!("wrote {relative}");
    }
}
