use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub fn package_files(root: &Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files, false)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

pub fn compute_dir_digest(root: &Path) -> anyhow::Result<String> {
    digest_files(package_files(root)?)
}

pub fn compute_legacy_dir_digest(root: &Path) -> anyhow::Result<String> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files, true)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    digest_files(files)
}

fn digest_files(files: Vec<(String, PathBuf)>) -> anyhow::Result<String> {
    // Detect post-install drift, not malicious concurrent replacement between checking and opening.
    let mut hasher = Sha256::new();
    for (relative, path) in files {
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        let mut file = fs::File::open(&path)?;
        let mut buf = [0u8; 8192];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        hasher.update([0]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn require_regular_entry(path: &Path, metadata: &fs::Metadata) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            anyhow::bail!(
                "unverifiable entry {}: Windows reparse point",
                path.display()
            );
        }
    }
    if !metadata.is_file() && !metadata.is_dir() {
        anyhow::bail!(
            "unverifiable entry {}: not a regular file or directory",
            path.display()
        );
    }
    Ok(())
}

fn collect_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf)>,
    legacy: bool,
) -> anyhow::Result<()> {
    require_regular_entry(dir, &fs::symlink_metadata(dir)?)?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        // Old npm installs on Unix created internal .bin links that the old digest omitted.
        if legacy
            && cfg!(unix)
            && metadata.file_type().is_symlink()
            && path
                .parent()
                .is_some_and(|parent| parent.ends_with("node_modules/.bin"))
            && path.canonicalize()?.starts_with(root.canonicalize()?)
        {
            continue;
        }
        require_regular_entry(&path, &metadata)?;
        if metadata.is_dir() {
            collect_files(root, &path, out, legacy)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("walked files descend from the supplied root")
                .to_string_lossy()
                .replace('\\', "/");
            out.push((relative, path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn legacy_npm_bin_links_inside_the_package_keep_the_original_digest() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("node_modules/.bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(root.path().join("node_modules/cli.js"), "cli").unwrap();
        let original = compute_dir_digest(root.path()).unwrap();
        std::os::unix::fs::symlink("../cli.js", bin.join("cli")).unwrap();
        assert_eq!(compute_legacy_dir_digest(root.path()).unwrap(), original);
        assert!(compute_dir_digest(root.path())
            .unwrap_err()
            .to_string()
            .contains("unverifiable entry"));
        fs::write(root.path().join("node_modules/cli.js"), "changed").unwrap();
        assert_ne!(compute_legacy_dir_digest(root.path()).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_npm_bin_links_outside_the_package_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("plugin");
        let bin = root.join("node_modules/.bin");
        fs::create_dir_all(&bin).unwrap();
        let outside = dir.path().join("outside.js");
        fs::write(&outside, "outside").unwrap();
        std::os::unix::fs::symlink(&outside, bin.join("cli")).unwrap();
        assert!(compute_legacy_dir_digest(&root)
            .unwrap_err()
            .to_string()
            .contains("unverifiable entry"));
    }

    #[test]
    fn records_names_and_bytes_in_the_installer_digest_format() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), b"hello").unwrap();
        let expected = format!("sha256:{:x}", Sha256::digest(b"a\0hello\0"));
        assert_eq!(compute_dir_digest(root.path()).unwrap(), expected);
    }

    #[test]
    fn root_and_creation_order_do_not_change_the_digest() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for (root, names) in [(first.path(), ["a", "b"]), (second.path(), ["b", "a"])] {
            fs::create_dir(root.join("nested")).unwrap();
            for name in names {
                fs::write(root.join("nested").join(name), name.as_bytes()).unwrap();
            }
        }
        assert_eq!(
            compute_dir_digest(first.path()).unwrap(),
            compute_dir_digest(second.path()).unwrap()
        );
    }

    #[test]
    fn content_and_filename_changes_are_detected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a");
        fs::write(&path, "before").unwrap();
        let before = compute_dir_digest(root.path()).unwrap();
        fs::write(&path, "after").unwrap();
        let after = compute_dir_digest(root.path()).unwrap();
        assert_ne!(before, after);
        fs::rename(path, root.path().join("b")).unwrap();
        assert_ne!(after, compute_dir_digest(root.path()).unwrap());
    }
}
