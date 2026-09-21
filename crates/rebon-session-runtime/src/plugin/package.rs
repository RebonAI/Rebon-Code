//! Reading a self-contained Rebon plugin package.
//!
//! The canonical install unit is a directory (or a tarball of one) with
//! `rebon-plugin.json` at its root. Installing one **never** runs anything from
//! inside it: no `npm install`, no `prepare`/`postinstall`, no lifecycle hook of
//! any kind. Publishing a plugin means shipping its runtime dependencies inside
//! the package; the host-provided Cordis/dsh is host ABI, not a per-plugin
//! install.
//!
//! # Why the tar reader is hand-written
//!
//! A plugin tarball is untrusted input, and the interesting attacks are all in
//! the *format*, not the payload: `..` members, absolute members, symlinks that
//! redirect a later write outside the tree, hardlinks to `/etc/…`, GNU long-name
//! and pax extension records that carry a second name past the one that was
//! validated, sparse files that expand, and members that collide only after the
//! filesystem case-folds them.
//!
//! Handing that to a general-purpose extractor means trusting it to have every
//! one of those switched off, on three platforms. This reader instead accepts a
//! deliberately tiny subset — ustar regular files and directories, nothing else
//! — and refuses everything it does not recognise, before a single byte is
//! written. What it cannot express, it will not unpack.
//!
//! `tar` is also not shelled out to: the system tools differ across platforms in
//! exactly the areas above, so the security boundary would be whatever bsdtar
//! happens to do this year.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context};

use super::manifest::PLUGIN_MANIFEST_FILE;

/// Ceilings on what one package may unpack to. They exist so a tarball that
/// compresses a petabyte of zeroes fails fast instead of filling the disk.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PackageLimits {
    pub max_entries: usize,
    pub max_total_bytes: u64,
    pub max_entry_bytes: u64,
    pub max_path_bytes: usize,
    pub max_path_depth: usize,
}

impl Default for PackageLimits {
    fn default() -> Self {
        Self {
            max_entries: 8192,
            max_total_bytes: 128 * 1024 * 1024,
            max_entry_bytes: 64 * 1024 * 1024,
            // ustar itself caps a name at 255 bytes across `prefix` + `name`.
            max_path_bytes: 255,
            max_path_depth: 32,
        }
    }
}

/// What an accepted package unpacked to.
#[derive(Debug, Clone)]
pub(crate) struct UnpackedPackage {
    /// Directory holding `rebon-plugin.json`. Either the unpack destination or
    /// the single wrapper directory inside it.
    pub root: PathBuf,
    pub entries: usize,
    pub bytes: u64,
}

/// Whether a path names an archive this module can read.
pub(crate) fn is_package_archive(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    name.ends_with(".tgz") || name.ends_with(".tar.gz") || name.ends_with(".tar")
}

/// Verifies, unpacks, and locates the package root.
///
/// `destination` must not exist yet — it is created here so that a failure
/// leaves nothing for a caller to mistake for a package.
pub(crate) fn unpack_package(
    archive: &Path,
    destination: &Path,
    limits: PackageLimits,
) -> anyhow::Result<UnpackedPackage> {
    let file = fs::File::open(archive)
        .with_context(|| format!("failed to open plugin package {}", archive.display()))?;
    let reader = std::io::BufReader::new(file);
    let name = archive
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let unpacked = if name.ends_with(".tar") {
        unpack_tar(reader, destination, limits)
    } else {
        unpack_tar(
            std::io::BufReader::new(flate2::read::GzDecoder::new(reader)),
            destination,
            limits,
        )
    };
    let unpacked = match unpacked {
        Ok(unpacked) => unpacked,
        Err(error) => {
            let _ = fs::remove_dir_all(destination);
            return Err(error.context(format!("plugin package {}", archive.display())));
        }
    };
    match locate_package_root(destination) {
        Some(root) => Ok(UnpackedPackage { root, ..unpacked }),
        None => {
            let _ = fs::remove_dir_all(destination);
            bail!(
                "plugin package {} has no {PLUGIN_MANIFEST_FILE} at its root (or in a single \
                 top-level directory)",
                archive.display()
            )
        }
    }
}

/// A package is either flat or wrapped in exactly one directory — the shape npm
/// tarballs use. More than one candidate is ambiguous, and guessing which
/// directory is the plugin is how the wrong code gets installed.
fn locate_package_root(destination: &Path) -> Option<PathBuf> {
    if destination.join(PLUGIN_MANIFEST_FILE).is_file() {
        return Some(destination.to_path_buf());
    }
    let mut children = fs::read_dir(destination).ok()?.flatten();
    let only = children.next()?;
    if children.next().is_some() || !only.file_type().ok()?.is_dir() {
        return None;
    }
    let nested = only.path();
    nested
        .join(PLUGIN_MANIFEST_FILE)
        .is_file()
        .then_some(nested)
}

const BLOCK: usize = 512;

fn unpack_tar<R: Read>(
    mut reader: R,
    destination: &Path,
    limits: PackageLimits,
) -> anyhow::Result<UnpackedPackage> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;

    // Keyed by the case-folded path: two members differing only in case unpack
    // to one file on Windows and macOS, so the second silently rewrites the
    // first. A package that means different things per platform is refused.
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut entries = 0usize;
    let mut total_bytes = 0u64;
    let mut block = [0u8; BLOCK];

    loop {
        match read_exact_block(&mut reader, &mut block)? {
            BlockRead::Eof => break,
            BlockRead::Zero => {
                // The entry stream ends at the first zero block. Everything
                // after it must be padding — appended data would be content the
                // validation above never saw.
                let mut rest = Vec::new();
                reader
                    .read_to_end(&mut rest)
                    .context("failed to read the archive trailer")?;
                if rest.iter().any(|byte| *byte != 0) {
                    bail!("archive has data after its end-of-archive marker");
                }
                break;
            }
            BlockRead::Header => {}
        }

        entries += 1;
        if entries > limits.max_entries {
            bail!("archive has more than {} entries", limits.max_entries);
        }

        let header = Header::parse(&block)?;
        let path = header.validate_path(&limits)?;
        if let Some(previous) = seen.insert(fold_case(&path), path.clone()) {
            if previous == path {
                bail!("archive contains `{path}` twice");
            }
            bail!(
                "archive contains `{previous}` and `{path}`, which are the same file on a \
                 case-insensitive filesystem"
            );
        }

        match header.kind {
            EntryKind::Directory => {
                if header.size != 0 {
                    bail!("directory entry `{path}` declares a nonzero size");
                }
                fs::create_dir_all(destination.join(&path))
                    .with_context(|| format!("failed to create `{path}`"))?;
            }
            EntryKind::File => {
                if header.size > limits.max_entry_bytes {
                    bail!(
                        "`{path}` is {} bytes, over the {} byte per-file limit",
                        header.size,
                        limits.max_entry_bytes
                    );
                }
                total_bytes = total_bytes.saturating_add(header.size);
                if total_bytes > limits.max_total_bytes {
                    bail!(
                        "archive unpacks to more than {} bytes",
                        limits.max_total_bytes
                    );
                }
                let target = destination.join(&path);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create the parent of `{path}`"))?;
                }
                write_entry(&mut reader, &target, header.size)
                    .with_context(|| format!("failed to unpack `{path}`"))?;
            }
        }
        skip_padding(&mut reader, header.size)?;
    }

    Ok(UnpackedPackage {
        root: destination.to_path_buf(),
        entries,
        bytes: total_bytes,
    })
}

enum BlockRead {
    Header,
    Zero,
    Eof,
}

fn read_exact_block<R: Read>(reader: &mut R, block: &mut [u8; BLOCK]) -> anyhow::Result<BlockRead> {
    let mut filled = 0;
    while filled < BLOCK {
        let read = reader
            .read(&mut block[filled..])
            .context("failed to read an archive block")?;
        if read == 0 {
            if filled == 0 {
                return Ok(BlockRead::Eof);
            }
            bail!("archive ends mid-block");
        }
        filled += read;
    }
    Ok(if block.iter().all(|byte| *byte == 0) {
        BlockRead::Zero
    } else {
        BlockRead::Header
    })
}

fn write_entry<R: Read>(reader: &mut R, target: &Path, size: u64) -> anyhow::Result<()> {
    let mut file = fs::File::create(target)?;
    let mut remaining = size;
    let mut buffer = [0u8; 8192];
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        let read = reader.read(&mut buffer[..want])?;
        if read == 0 {
            bail!("archive ends inside a file entry");
        }
        file.write_all(&buffer[..read])?;
        remaining -= read as u64;
    }
    file.flush()?;
    Ok(())
}

/// tar pads every entry to a block boundary.
fn skip_padding<R: Read>(reader: &mut R, size: u64) -> anyhow::Result<()> {
    let padding = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
    let mut remaining = padding;
    let mut buffer = [0u8; BLOCK];
    while remaining > 0 {
        let want = remaining.min(BLOCK as u64) as usize;
        let read = reader.read(&mut buffer[..want])?;
        if read == 0 {
            bail!("archive ends inside an entry's padding");
        }
        remaining -= read as u64;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
}

struct Header {
    name: String,
    prefix: String,
    size: u64,
    kind: EntryKind,
}

impl Header {
    fn parse(block: &[u8; BLOCK]) -> anyhow::Result<Self> {
        verify_checksum(block)?;

        // Every type flag other than these two carries something this format
        // deliberately cannot express. Naming them makes the refusal actionable
        // instead of "unsupported entry".
        let kind = match block[156] {
            b'0' | 0 => EntryKind::File,
            b'5' => EntryKind::Directory,
            b'1' => bail!("archive contains a hard link, which plugin packages may not use"),
            b'2' => bail!("archive contains a symlink, which plugin packages may not use"),
            b'3' | b'4' => bail!("archive contains a device node"),
            b'6' => bail!("archive contains a FIFO"),
            b'7' => bail!("archive contains a contiguous-file entry"),
            b'x' | b'g' | b'X' => {
                bail!("archive uses pax extended headers, which may restate a validated name")
            }
            b'L' | b'K' => {
                bail!("archive uses GNU long-name records, which may restate a validated name")
            }
            b'S' => bail!("archive contains a sparse file"),
            other => bail!(
                "archive contains an unsupported entry type `{}`",
                other as char
            ),
        };

        let size = parse_octal(&block[124..136], "size")?;
        Ok(Self {
            name: parse_string(&block[0..100], "name")?,
            prefix: parse_string(&block[345..500], "prefix")?,
            size,
            kind,
        })
    }

    /// The member's path, relative and normalised, or an error naming what made
    /// it unacceptable.
    fn validate_path(&self, limits: &PackageLimits) -> anyhow::Result<String> {
        let joined = if self.prefix.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", self.prefix, self.name)
        };
        let trimmed = joined.trim_end_matches('/');
        if trimmed.is_empty() {
            bail!("archive contains an entry with an empty name");
        }
        if trimmed.len() > limits.max_path_bytes {
            bail!(
                "archive entry `{trimmed}` has a path longer than {} bytes",
                limits.max_path_bytes
            );
        }
        if trimmed.starts_with('/') {
            bail!("archive entry `{trimmed}` is an absolute path");
        }
        if trimmed.contains('\\') {
            bail!("archive entry `{trimmed}` contains a backslash, which is a path separator on Windows");
        }
        if trimmed.chars().any(|c| c.is_control()) {
            bail!("archive entry name contains a control character");
        }
        // `C:` and `\\?\` style prefixes only become path components on Windows,
        // so ask the platform-independent parser rather than pattern-matching.
        let parsed = PathBuf::from(trimmed);
        let mut depth = 0usize;
        for component in parsed.components() {
            match component {
                Component::Normal(_) => depth += 1,
                Component::CurDir => {}
                Component::ParentDir => {
                    bail!("archive entry `{trimmed}` climbs out of the package with `..`")
                }
                Component::RootDir | Component::Prefix(_) => {
                    bail!("archive entry `{trimmed}` is an absolute path")
                }
            }
        }
        if depth == 0 {
            bail!("archive contains an entry with an empty name");
        }
        if depth > limits.max_path_depth {
            bail!(
                "archive entry `{trimmed}` nests deeper than {} directories",
                limits.max_path_depth
            );
        }
        Ok(parsed
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/"))
    }
}

fn verify_checksum(block: &[u8; BLOCK]) -> anyhow::Result<()> {
    let declared = parse_octal(&block[148..156], "checksum")?;
    let mut sum: u64 = 0;
    for (index, byte) in block.iter().enumerate() {
        // The checksum field itself is summed as spaces.
        sum += if (148..156).contains(&index) {
            u64::from(b' ')
        } else {
            u64::from(*byte)
        };
    }
    if sum != declared {
        bail!("archive header checksum does not match its contents");
    }
    Ok(())
}

fn parse_octal(field: &[u8], what: &str) -> anyhow::Result<u64> {
    // The high bit marks GNU base-256 encoding, used for values too large for
    // the octal field. Nothing a plugin package may contain needs it.
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        bail!("archive uses base-256 numeric fields");
    }
    let text: String = field
        .iter()
        .take_while(|byte| **byte != 0 && **byte != b' ')
        .map(|byte| *byte as char)
        .collect();
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    if !text.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        bail!("archive has a malformed {what} field");
    }
    u64::from_str_radix(text, 8).map_err(|_| anyhow!("archive has an out-of-range {what} field"))
}

fn parse_string(field: &[u8], what: &str) -> anyhow::Result<String> {
    let bytes: Vec<u8> = field
        .iter()
        .copied()
        .take_while(|byte| *byte != 0)
        .collect();
    String::from_utf8(bytes).map_err(|_| anyhow!("archive has a non-UTF-8 {what} field"))
}

/// ASCII case folding: the collision this guards against is what Windows and
/// macOS do, and both fold ASCII.
fn fold_case(path: &str) -> String {
    path.to_ascii_lowercase()
}

/// Builds a valid ustar archive of regular files, for tests elsewhere in the
/// crate that need a real package to install rather than a header to reject.
#[cfg(test)]
pub(crate) fn tar_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, contents) in entries {
        let mut block = [0u8; BLOCK];
        let name_bytes = name.as_bytes();
        block[..name_bytes.len()].copy_from_slice(name_bytes);
        let octal = format!("{:011o}\0", contents.len());
        block[124..124 + octal.len()].copy_from_slice(octal.as_bytes());
        block[156] = b'0';
        block[257..262].copy_from_slice(b"ustar");
        let checksum: u64 = block
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                if (148..156).contains(&index) {
                    u64::from(b' ')
                } else {
                    u64::from(*byte)
                }
            })
            .sum();
        let text = format!("{checksum:06o}\0 ");
        block[148..148 + text.len()].copy_from_slice(text.as_bytes());
        out.extend_from_slice(&block);
        out.extend_from_slice(contents);
        out.extend(std::iter::repeat_n(
            0u8,
            (BLOCK - contents.len() % BLOCK) % BLOCK,
        ));
    }
    out.extend(std::iter::repeat_n(0u8, BLOCK * 2));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds tar blocks directly so a test can express headers no writer would
    /// produce — which is the entire point of the reader.
    struct TarBuilder {
        blocks: Vec<u8>,
    }

    impl TarBuilder {
        fn new() -> Self {
            Self { blocks: Vec::new() }
        }

        fn header(&mut self, name: &str, kind: u8, size: u64) -> &mut Self {
            self.raw_header(name, "", kind, size)
        }

        fn raw_header(&mut self, name: &str, prefix: &str, kind: u8, size: u64) -> &mut Self {
            self.tweaked_header(name, prefix, kind, size, |_| {})
        }

        /// Lets a test corrupt a field *before* the checksum is computed, so the
        /// header stays internally consistent and the reader's other rejections
        /// are what gets exercised.
        fn tweaked_header(
            &mut self,
            name: &str,
            prefix: &str,
            kind: u8,
            size: u64,
            tweak: impl FnOnce(&mut [u8; BLOCK]),
        ) -> &mut Self {
            let mut block = [0u8; BLOCK];
            block[..name.len().min(100)].copy_from_slice(&name.as_bytes()[..name.len().min(100)]);
            let octal = format!("{size:011o}\0");
            block[124..124 + octal.len()].copy_from_slice(octal.as_bytes());
            block[156] = kind;
            block[257..262].copy_from_slice(b"ustar");
            if !prefix.is_empty() {
                block[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
            }
            tweak(&mut block);
            let checksum: u64 = block
                .iter()
                .enumerate()
                .map(|(index, byte)| {
                    if (148..156).contains(&index) {
                        u64::from(b' ')
                    } else {
                        u64::from(*byte)
                    }
                })
                .sum();
            let text = format!("{checksum:06o}\0 ");
            block[148..148 + text.len()].copy_from_slice(text.as_bytes());
            self.blocks.extend_from_slice(&block);
            self
        }

        fn file(&mut self, name: &str, contents: &[u8]) -> &mut Self {
            self.header(name, b'0', contents.len() as u64);
            self.blocks.extend_from_slice(contents);
            let padding = (BLOCK - contents.len() % BLOCK) % BLOCK;
            self.blocks.extend(std::iter::repeat_n(0u8, padding));
            self
        }

        fn dir(&mut self, name: &str) -> &mut Self {
            self.header(name, b'5', 0)
        }

        fn finish(&mut self) -> Vec<u8> {
            let mut out = self.blocks.clone();
            out.extend(std::iter::repeat_n(0u8, BLOCK * 2));
            out
        }
    }

    fn manifest() -> Vec<u8> {
        br#"{"name":"demo","version":"1.0.0"}"#.to_vec()
    }

    /// Renders the whole `anyhow` chain: the refusal that matters is the inner
    /// one, and `Display` alone shows only the outermost context.
    fn refusal<T>(result: anyhow::Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected the package to be refused"),
            Err(error) => format!("{error:#}"),
        }
    }

    fn unpack(bytes: &[u8]) -> anyhow::Result<(tempfile::TempDir, UnpackedPackage)> {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("plugin.tar");
        fs::write(&archive, bytes).unwrap();
        let destination = temp.path().join("out");
        let unpacked = unpack_package(&archive, &destination, PackageLimits::default())?;
        Ok((temp, unpacked))
    }

    #[test]
    fn a_flat_package_unpacks_and_finds_its_manifest() {
        let bytes = TarBuilder::new()
            .file(PLUGIN_MANIFEST_FILE, &manifest())
            .file("skills/demo.md", b"# demo")
            .finish();
        let (_temp, unpacked) = unpack(&bytes).unwrap();
        assert!(unpacked.root.join(PLUGIN_MANIFEST_FILE).is_file());
        assert!(unpacked.root.join("skills").join("demo.md").is_file());
        assert_eq!(unpacked.entries, 2);
        assert_eq!(unpacked.bytes, manifest().len() as u64 + 6);
    }

    /// npm tarballs wrap everything in `package/`.
    #[test]
    fn a_single_wrapper_directory_is_stripped() {
        let bytes = TarBuilder::new()
            .dir("package/")
            .file("package/rebon-plugin.json", &manifest())
            .finish();
        let (_temp, unpacked) = unpack(&bytes).unwrap();
        assert!(unpacked.root.ends_with("package"));
        assert!(unpacked.root.join(PLUGIN_MANIFEST_FILE).is_file());
    }

    #[test]
    fn ustar_prefix_and_name_are_joined() {
        let bytes = TarBuilder::new()
            .file(PLUGIN_MANIFEST_FILE, &manifest())
            .raw_header("deep.txt", "a/b/c", b'0', 0)
            .finish();
        let (_temp, unpacked) = unpack(&bytes).unwrap();
        assert!(unpacked
            .root
            .join("a")
            .join("b")
            .join("c")
            .join("deep.txt")
            .is_file());
    }

    #[test]
    fn a_package_without_a_manifest_is_refused_and_leaves_nothing() {
        let bytes = TarBuilder::new().file("readme.md", b"hi").finish();
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("plugin.tar");
        fs::write(&archive, &bytes).unwrap();
        let destination = temp.path().join("out");

        let error = refusal(unpack_package(
            &archive,
            &destination,
            PackageLimits::default(),
        ));
        assert!(error.to_string().contains(PLUGIN_MANIFEST_FILE), "{error}");
        assert!(!destination.exists(), "a refused package leaves no tree");
    }

    #[test]
    fn two_wrapper_directories_are_ambiguous() {
        let bytes = TarBuilder::new()
            .file("one/rebon-plugin.json", &manifest())
            .file("two/rebon-plugin.json", &manifest())
            .finish();
        assert!(unpack(&bytes).is_err());
    }

    #[test]
    fn traversal_and_absolute_members_are_refused_before_any_write() {
        for name in [
            "../escape.txt",
            "a/../../escape.txt",
            "/etc/passwd",
            "//etc/passwd",
        ] {
            let bytes = TarBuilder::new().file(name, b"x").finish();
            let error = refusal(unpack(&bytes));
            let rendered = error.to_string();
            assert!(
                rendered.contains("climbs out") || rendered.contains("absolute"),
                "`{name}` produced: {rendered}"
            );
        }
    }

    /// A backslash is a separator on Windows, so `a\..\b` would traverse there
    /// while reading as one harmless component elsewhere.
    #[test]
    fn backslashes_are_refused_so_a_package_cannot_mean_two_things() {
        let bytes = TarBuilder::new().file("a\\..\\escape.txt", b"x").finish();
        let error = refusal(unpack(&bytes));
        assert!(error.to_string().contains("backslash"), "{error}");
    }

    #[test]
    fn every_entry_type_but_files_and_directories_is_named_and_refused() {
        for (flag, needle) in [
            (b'1', "hard link"),
            (b'2', "symlink"),
            (b'3', "device node"),
            (b'4', "device node"),
            (b'6', "FIFO"),
            (b'x', "pax extended headers"),
            (b'L', "GNU long-name"),
            (b'S', "sparse file"),
        ] {
            let bytes = TarBuilder::new().header("thing", flag, 0).finish();
            let error = refusal(unpack(&bytes));
            assert!(
                error.to_string().contains(needle),
                "type `{}` produced: {error}",
                flag as char
            );
        }
    }

    #[test]
    fn a_tampered_header_fails_its_checksum() {
        let mut bytes = TarBuilder::new()
            .file(PLUGIN_MANIFEST_FILE, &manifest())
            .finish();
        bytes[0] = b'X';
        let error = refusal(unpack(&bytes));
        assert!(error.to_string().contains("checksum"), "{error}");
    }

    #[test]
    fn duplicate_members_are_refused() {
        let bytes = TarBuilder::new()
            .file(PLUGIN_MANIFEST_FILE, &manifest())
            .file("dup.txt", b"first")
            .file("dup.txt", b"second")
            .finish();
        let error = refusal(unpack(&bytes));
        assert!(error.to_string().contains("twice"), "{error}");
    }

    /// On Windows and macOS the second member overwrites the first, so the
    /// package would install different bytes depending on the machine.
    #[test]
    fn members_colliding_only_by_case_are_refused() {
        let bytes = TarBuilder::new()
            .file(PLUGIN_MANIFEST_FILE, &manifest())
            .file("Tool.js", b"first")
            .file("tool.js", b"second")
            .finish();
        let error = refusal(unpack(&bytes));
        assert!(error.to_string().contains("case-insensitive"), "{error}");
    }

    #[test]
    fn data_appended_after_the_end_marker_is_refused() {
        let mut bytes = TarBuilder::new()
            .file(PLUGIN_MANIFEST_FILE, &manifest())
            .finish();
        bytes.extend_from_slice(b"smuggled");
        let error = refusal(unpack(&bytes));
        assert!(
            error
                .to_string()
                .contains("after its end-of-archive marker"),
            "{error}"
        );
    }

    #[test]
    fn base_256_numeric_fields_are_refused() {
        let bytes = TarBuilder::new()
            .tweaked_header("big.bin", "", b'0', 0, |block| block[124] = 0x80)
            .finish();
        let error = refusal(unpack(&bytes));
        assert!(error.contains("base-256"), "{error}");
    }

    #[test]
    fn limits_bound_entry_count_size_and_depth() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("plugin.tar");

        let mut builder = TarBuilder::new();
        builder.file(PLUGIN_MANIFEST_FILE, &manifest());
        builder.file("a.txt", b"aaaa");
        builder.file("b.txt", b"bbbb");
        let bytes = builder.finish();
        fs::write(&archive, &bytes).unwrap();

        let tight = PackageLimits {
            max_entries: 2,
            ..PackageLimits::default()
        };
        let error = refusal(unpack_package(&archive, &temp.path().join("o1"), tight));
        assert!(error.to_string().contains("more than 2 entries"), "{error}");

        let small = PackageLimits {
            max_total_bytes: 8,
            ..PackageLimits::default()
        };
        let error = refusal(unpack_package(&archive, &temp.path().join("o2"), small));
        assert!(error.to_string().contains("more than 8 bytes"), "{error}");

        let per_file = PackageLimits {
            max_entry_bytes: 3,
            ..PackageLimits::default()
        };
        let error = refusal(unpack_package(&archive, &temp.path().join("o3"), per_file));
        assert!(error.to_string().contains("per-file limit"), "{error}");

        let deep = TarBuilder::new().file("a/b/c/d.txt", b"x").finish();
        fs::write(&archive, &deep).unwrap();
        let shallow = PackageLimits {
            max_path_depth: 2,
            ..PackageLimits::default()
        };
        let error = refusal(unpack_package(&archive, &temp.path().join("o4"), shallow));
        assert!(error.to_string().contains("nests deeper"), "{error}");
    }

    #[test]
    fn a_truncated_archive_is_refused() {
        let mut bytes = TarBuilder::new()
            .file(PLUGIN_MANIFEST_FILE, &manifest())
            .finish();
        bytes.truncate(BLOCK + 4);
        let error = refusal(unpack(&bytes));
        assert!(error.to_string().contains("ends"), "{error}");
    }

    #[test]
    fn a_gzipped_package_reads_the_same_way() {
        use std::io::Write as _;
        let bytes = TarBuilder::new()
            .file(PLUGIN_MANIFEST_FILE, &manifest())
            .finish();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&bytes).unwrap();
        let gz = encoder.finish().unwrap();

        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("plugin.tgz");
        fs::write(&archive, gz).unwrap();
        let unpacked =
            unpack_package(&archive, &temp.path().join("out"), PackageLimits::default()).unwrap();
        assert!(unpacked.root.join(PLUGIN_MANIFEST_FILE).is_file());
    }

    #[test]
    fn archive_extensions_are_recognised_case_insensitively() {
        for name in ["a.tgz", "a.tar.gz", "a.TAR.GZ", "a.tar", "b.TGZ"] {
            assert!(is_package_archive(Path::new(name)), "{name}");
        }
        for name in ["a.zip", "a.tar.bz2", "plugin", "a.json"] {
            assert!(!is_package_archive(Path::new(name)), "{name}");
        }
    }
}
