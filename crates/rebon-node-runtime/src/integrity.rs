//! What a managed install is allowed to accept.
//!
//! Rebon does not bundle a Node runtime, so a managed install has to take bytes
//! from somewhere the user's machine reached on its own. The digest that decides
//! whether those bytes are used is compiled into this binary, not fetched
//! alongside them: a checksum file served by the same host as the archive
//! proves the download was not corrupted in transit and nothing more.
//!
//! Two provenances exist, and both are fail-closed. An [`ArchiveExpectation::Pinned`]
//! archive must hash to the entry below for the running platform. An
//! [`ArchiveExpectation::Declared`] archive must hash to a digest the operator
//! typed in — the escape hatch for a platform with no pinned build or a runtime
//! built in-house, where naming the digest is the act of vouching for it.

use std::{fmt, fs::File, io, path::Path};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::version::{NodeVersion, PINNED_NODE_VERSION};

/// A SHA-256 digest of an archive's exact bytes.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct Sha256Digest([u8; 32]);

#[derive(Debug, Error)]
pub enum DigestError {
    #[error("`{0}` is not a 64-character hex SHA-256 digest")]
    Malformed(String),
    #[error("cannot read {path}: {source}")]
    Unreadable {
        path: String,
        #[source]
        source: io::Error,
    },
}

impl Sha256Digest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn parse_hex(text: &str) -> Result<Self, DigestError> {
        let malformed = || DigestError::Malformed(text.to_string());
        if text.len() != 64 {
            return Err(malformed());
        }
        let mut bytes = [0u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let pair = text.get(index * 2..index * 2 + 2).ok_or_else(malformed)?;
            // Uppercase hex is accepted on input; `to_hex` always emits
            // lowercase so a receipt has one spelling.
            if !pair.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(malformed());
            }
            *byte = u8::from_str_radix(pair, 16).map_err(|_| malformed())?;
        }
        Ok(Self(bytes))
    }

    /// Streams the file rather than reading it whole: a Node archive is tens of
    /// megabytes and this runs on the install path of a desktop app.
    pub fn of_file(path: &Path) -> Result<Self, DigestError> {
        let mut file = File::open(path).map_err(|source| DigestError::Unreadable {
            path: path.display().to_string(),
            source,
        })?;
        let mut hasher = Sha256::new();
        io::copy(&mut file, &mut hasher).map_err(|source| DigestError::Unreadable {
            path: path.display().to_string(),
            source,
        })?;
        Ok(Self(hasher.finalize().into()))
    }

    pub fn to_hex(self) -> String {
        use std::fmt::Write as _;
        let mut text = String::with_capacity(64);
        for byte in self.0 {
            let _ = write!(text, "{byte:02x}");
        }
        text
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256Digest({})", self.to_hex())
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A platform in nodejs.org's own naming, which is what appears in the archive
/// file name and therefore in the pinned table.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NodePlatform {
    pub os: &'static str,
    pub cpu: &'static str,
}

impl fmt::Display for NodePlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.os, self.cpu)
    }
}

/// The platform this process is running on, or `None` where Rebon has no
/// pinned Node build — the caller must then fall back to an operator-declared
/// archive rather than inventing a file name.
pub fn host_platform() -> Option<NodePlatform> {
    let os = match std::env::consts::OS {
        "windows" => "win",
        "macos" => "darwin",
        "linux" => "linux",
        _ => return None,
    };
    let cpu = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some(NodePlatform { os, cpu })
}

/// One official build of [`PINNED_NODE_VERSION`], bound to its exact bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinnedArchive {
    pub platform: NodePlatform,
    pub file_name: &'static str,
    pub sha256_hex: &'static str,
}

impl PinnedArchive {
    pub fn version(&self) -> NodeVersion {
        PINNED_NODE_VERSION
    }

    pub fn digest(&self) -> Sha256Digest {
        // Pins are compiled-in constants covered by `pinned_digests_are_wellformed_and_distinct`;
        // a malformed one is a build-time authoring error, not a runtime state.
        Sha256Digest::parse_hex(self.sha256_hex).expect("pinned digest is valid hex")
    }

    /// The download location on the official distribution host. The base is a
    /// parameter so an operator can point at an internal mirror: the digest is
    /// what decides acceptance, so the transport does not have to be trusted.
    pub fn url(&self, dist_base: &str) -> String {
        format!(
            "{}/{}/{}",
            dist_base.trim_end_matches('/'),
            PINNED_NODE_VERSION.tag(),
            self.file_name
        )
    }
}

/// nodejs.org's release directory. Not a configured default anywhere else —
/// callers that offer a mirror pass their own base to [`PinnedArchive::url`].
pub const DEFAULT_NODE_DIST_BASE: &str = "https://nodejs.org/dist";

/// SHA-256 digests published for `node-v24.19.0-*` on nodejs.org.
///
/// Bumping [`PINNED_NODE_VERSION`] means replacing every row: a mixed table
/// would install a version the receipt does not name.
pub const PINNED_ARCHIVES: &[PinnedArchive] = &[
    PinnedArchive {
        platform: NodePlatform {
            os: "win",
            cpu: "x64",
        },
        file_name: "node-v24.19.0-win-x64.zip",
        sha256_hex: "57f71ab3652e797d84acddc79c81cc9ff1c6ddb2a1974cdb83f00fee9bff4c73",
    },
    PinnedArchive {
        platform: NodePlatform {
            os: "win",
            cpu: "arm64",
        },
        file_name: "node-v24.19.0-win-arm64.zip",
        sha256_hex: "8502f4a50b458d4cc38ed8f2001556c2cd239d464920f74017926ccb1e1c157f",
    },
    PinnedArchive {
        platform: NodePlatform {
            os: "darwin",
            cpu: "x64",
        },
        file_name: "node-v24.19.0-darwin-x64.tar.gz",
        sha256_hex: "d1b5e999db158c62fe8f7267a4476b035d8bd93b1a605bac24a3f0dd166e3316",
    },
    PinnedArchive {
        platform: NodePlatform {
            os: "darwin",
            cpu: "arm64",
        },
        file_name: "node-v24.19.0-darwin-arm64.tar.gz",
        sha256_hex: "8294b7aa9b03997481c06babf1e8b270c859358f27da57a11509afe537ac381d",
    },
    PinnedArchive {
        platform: NodePlatform {
            os: "linux",
            cpu: "x64",
        },
        file_name: "node-v24.19.0-linux-x64.tar.gz",
        sha256_hex: "f625d97cd707df4ff96254916fbc5ff014f09c09effe5a1e0ca8f6d41a8789d4",
    },
    PinnedArchive {
        platform: NodePlatform {
            os: "linux",
            cpu: "arm64",
        },
        file_name: "node-v24.19.0-linux-arm64.tar.gz",
        sha256_hex: "d28c8a5bf0a808f0ed434a1dce8c54ae98f0371c0bd86ac58abc613f73e6643f",
    },
];

pub fn pinned_archive(platform: NodePlatform) -> Option<&'static PinnedArchive> {
    PINNED_ARCHIVES
        .iter()
        .find(|archive| archive.platform == platform)
}

/// The pinned build for the running platform, if Rebon ships one.
pub fn pinned_archive_for_host() -> Option<&'static PinnedArchive> {
    host_platform().and_then(pinned_archive)
}

/// What a managed install checks the bytes against, and who vouched for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArchiveExpectation {
    /// Rebon's own compiled-in digest for an official build.
    Pinned(&'static PinnedArchive),
    /// An operator naming both the version and the digest, for a platform or a
    /// build Rebon does not pin.
    Declared {
        version: NodeVersion,
        digest: Sha256Digest,
    },
}

impl ArchiveExpectation {
    pub fn version(&self) -> NodeVersion {
        match self {
            Self::Pinned(archive) => archive.version(),
            Self::Declared { version, .. } => *version,
        }
    }

    pub fn digest(&self) -> Sha256Digest {
        match self {
            Self::Pinned(archive) => archive.digest(),
            Self::Declared { digest, .. } => *digest,
        }
    }

    pub fn provenance(&self) -> &'static str {
        match self {
            Self::Pinned(_) => "pinned",
            Self::Declared { .. } => "declared",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_digests_are_wellformed_and_distinct() {
        let mut seen = std::collections::HashSet::new();
        for archive in PINNED_ARCHIVES {
            let digest = Sha256Digest::parse_hex(archive.sha256_hex).expect("valid hex");
            assert_eq!(digest.to_hex(), archive.sha256_hex, "digests are lowercase");
            assert!(
                seen.insert(digest),
                "two platforms share a digest: {}",
                archive.file_name
            );
        }
    }

    /// The file name is what a download URL is built from, so it has to name
    /// the version this table claims and carry the extension that platform
    /// actually publishes.
    #[test]
    fn pinned_file_names_match_the_version_platform_and_format() {
        for archive in PINNED_ARCHIVES {
            let expected_stem = format!("node-{}-{}", PINNED_NODE_VERSION.tag(), archive.platform);
            let extension = if archive.platform.os == "win" {
                ".zip"
            } else {
                ".tar.gz"
            };
            assert_eq!(
                archive.file_name,
                format!("{expected_stem}{extension}"),
                "{} is misnamed",
                archive.platform
            );
        }
    }

    #[test]
    fn every_platform_appears_once() {
        let mut seen = std::collections::HashSet::new();
        for archive in PINNED_ARCHIVES {
            assert!(
                seen.insert(archive.platform),
                "{} is pinned twice",
                archive.platform
            );
        }
        assert_eq!(seen.len(), 6);
    }

    #[test]
    fn url_joins_without_doubling_the_separator() {
        let archive = pinned_archive(NodePlatform {
            os: "linux",
            cpu: "x64",
        })
        .unwrap();
        assert_eq!(
            archive.url(DEFAULT_NODE_DIST_BASE),
            "https://nodejs.org/dist/v24.19.0/node-v24.19.0-linux-x64.tar.gz"
        );
        assert_eq!(
            archive.url("https://mirror.internal/node/"),
            "https://mirror.internal/node/v24.19.0/node-v24.19.0-linux-x64.tar.gz"
        );
    }

    #[test]
    fn host_platform_resolves_to_a_pinned_build_on_supported_targets() {
        // Every target this workspace builds for is a platform Node publishes.
        let platform = host_platform().expect("supported host platform");
        assert!(pinned_archive(platform).is_some());
    }

    #[test]
    fn digest_hex_round_trips_and_rejects_junk() {
        let hex = "57f71ab3652e797d84acddc79c81cc9ff1c6ddb2a1974cdb83f00fee9bff4c73";
        assert_eq!(Sha256Digest::parse_hex(hex).unwrap().to_hex(), hex);
        assert_eq!(
            Sha256Digest::parse_hex(&hex.to_uppercase())
                .unwrap()
                .to_hex(),
            hex,
            "uppercase input normalises to one spelling"
        );
        let too_long = format!("{hex}0");
        let bad_digit = hex.replace('5', "g");
        let multibyte = "é".repeat(32);
        for bad in ["", "abc", &hex[..63], &too_long, &bad_digit, &multibyte] {
            assert!(
                Sha256Digest::parse_hex(bad).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
    }

    #[test]
    fn file_digest_matches_a_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.bin");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            Sha256Digest::of_file(&path).unwrap().to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn missing_file_is_a_read_error_not_a_digest() {
        let error = Sha256Digest::of_file(Path::new("Z:/definitely/not/here.tgz")).unwrap_err();
        assert!(matches!(error, DigestError::Unreadable { .. }));
    }

    #[test]
    fn expectation_reports_its_provenance() {
        let pinned = ArchiveExpectation::Pinned(&PINNED_ARCHIVES[0]);
        assert_eq!(pinned.provenance(), "pinned");
        assert_eq!(pinned.version(), PINNED_NODE_VERSION);

        let declared = ArchiveExpectation::Declared {
            version: NodeVersion::new(24, 20, 1),
            digest: Sha256Digest::from_bytes([7u8; 32]),
        };
        assert_eq!(declared.provenance(), "declared");
        assert_eq!(declared.version(), NodeVersion::new(24, 20, 1));
    }
}
