//! Putting the skills compiled plugins ship on disk.
//!
//! A plugin registers a [`SkillBundle`] on the `skill-bundles` seat — the
//! files of one skill directory, embedded in the binary. The loader reads
//! skills from directories, and a skill's references and scripts have to be
//! somewhere the model's `Read` and `Bash` can reach, so each bundle is
//! written under the config home before the session index loads.
//!
//! Each bundle gets a skills root of its own —
//! `<config_home>/cache/skill-bundles/<name>/<name>/SKILL.md` — and only the
//! roots of bundles registered *now* are loaded. A shared root would load
//! every directory ever written there, so turning a plugin off would leave
//! its skill in every later session; deleting the stale ones instead would
//! race another process whose plugin set differs.

use std::path::{Path, PathBuf};

use rebon_core::skill_seat::SkillBundle;

/// Where bundles land under a config home.
pub fn skill_bundles_root(config_home: &Path) -> PathBuf {
    config_home.join("cache").join("skill-bundles")
}

/// Write `bundles` under `config_home` and return the skills root to load for
/// each, in bundle order.
///
/// A file whose bytes already match is left alone, so an unchanged bundle
/// costs reads rather than writes and a running session never sees its skill
/// files rewritten under it. A bundle that fails to write is logged and left
/// out: one plugin's unwritable directory must not keep the session's other
/// skills from loading.
pub fn materialize_skill_bundles(config_home: &Path, bundles: &[SkillBundle]) -> Vec<PathBuf> {
    let root = skill_bundles_root(config_home);
    bundles
        .iter()
        .filter_map(|bundle| {
            let bundle_root = root.join(bundle.name);
            match write_bundle(&bundle_root.join(bundle.name), bundle) {
                Ok(()) => Some(bundle_root),
                Err(err) => {
                    tracing::warn!(
                        skill = bundle.name,
                        dir = %bundle_root.display(),
                        "skill bundle not written: {err}"
                    );
                    None
                }
            }
        })
        .collect()
}

fn write_bundle(skill_dir: &Path, bundle: &SkillBundle) -> std::io::Result<()> {
    for (relative, contents) in bundle.files {
        let path = relative
            .split('/')
            .fold(skill_dir.to_path_buf(), |path, segment| path.join(segment));
        if std::fs::read(&path).is_ok_and(|existing| existing == contents.as_bytes()) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        rebon_session::write_file_atomically(&path, contents.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUNDLE: SkillBundle = SkillBundle {
        name: "imagegen",
        files: &[
            ("SKILL.md", "---\nname: imagegen\n---\nbody"),
            ("references/prompting.md", "prompting"),
        ],
    };

    #[test]
    fn a_bundle_lands_in_a_skills_root_of_its_own() {
        let home = tempfile::TempDir::new().unwrap();
        let roots = materialize_skill_bundles(home.path(), &[BUNDLE]);
        let expected = skill_bundles_root(home.path()).join("imagegen");
        assert_eq!(roots, vec![expected.clone()]);
        assert_eq!(
            std::fs::read_to_string(expected.join("imagegen").join("SKILL.md")).unwrap(),
            "---\nname: imagegen\n---\nbody"
        );
        assert_eq!(
            std::fs::read_to_string(
                expected
                    .join("imagegen")
                    .join("references")
                    .join("prompting.md")
            )
            .unwrap(),
            "prompting"
        );
    }

    #[test]
    fn a_changed_file_is_rewritten_and_an_unchanged_one_is_left_alone() {
        let home = tempfile::TempDir::new().unwrap();
        materialize_skill_bundles(home.path(), &[BUNDLE]);
        let skill_dir = skill_bundles_root(home.path())
            .join("imagegen")
            .join("imagegen");
        std::fs::write(skill_dir.join("SKILL.md"), "stale").unwrap();
        let reference = skill_dir.join("references").join("prompting.md");
        let untouched = std::fs::metadata(&reference).unwrap().modified().unwrap();

        materialize_skill_bundles(home.path(), &[BUNDLE]);
        assert_eq!(
            std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap(),
            "---\nname: imagegen\n---\nbody"
        );
        assert_eq!(
            std::fs::metadata(&reference).unwrap().modified().unwrap(),
            untouched
        );
    }

    /// Only bundles registered now are loaded: a directory a disabled
    /// plugin wrote earlier is still on disk but not among the roots.
    #[test]
    fn only_current_bundles_are_returned() {
        let home = tempfile::TempDir::new().unwrap();
        materialize_skill_bundles(home.path(), &[BUNDLE]);
        assert!(materialize_skill_bundles(home.path(), &[]).is_empty());
    }

    #[test]
    fn an_unwritable_bundle_is_left_out() {
        let home = tempfile::TempDir::new().unwrap();
        // A file where the bundles root should be makes every write fail.
        std::fs::create_dir_all(home.path().join("cache")).unwrap();
        std::fs::write(skill_bundles_root(home.path()), "not a directory").unwrap();
        assert!(materialize_skill_bundles(home.path(), &[BUNDLE]).is_empty());
    }
}
