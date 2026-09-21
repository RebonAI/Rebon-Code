//! Writes the generated artefacts to the working tree.
//!
//! ```text
//! cargo run -p rebon-schema-gen
//! ```

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `<root>/crates/rebon-schema-gen`.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|crates| crates.parent())
        .expect("the crate lives two levels under the repository root")
        .to_path_buf()
}

fn main() {
    let root = repo_root();
    for (relative, contents) in rebon_schema_gen::artefacts() {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("create {}: {error}", parent.display()));
        }
        std::fs::write(&path, contents.as_bytes())
            .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        println!("wrote {relative}");
    }
}
