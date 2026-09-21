//! `OPEN_FOLDER_PREFIX` constant + encode/decode helpers.
//!
//! ## Behavior notes
//!
//! ```text
//! OPEN_FOLDER_PREFIX = "__open_folder__"
//!
//! // Three option rows, each value carrying the prefix:
//! "Open auto-memory folder"        -> encode_open_folder(auto_mem_path)
//! "Open team memory folder"        -> encode_open_folder(team_mem_path)
//! "Open {agent_type} agent memory" -> encode_open_folder(agent_dir)
//!
//! // What happens to a row value the user picks:
//! if value.starts_with(OPEN_FOLDER_PREFIX):
//!     decode_open_folder(value) -> Decoded::Folder(rest)  // prefix stripped
//!     // the consumer then mkdir(rest, recursive) and open_path(rest)
//! else:
//!     decode_open_folder(value) -> Decoded::File(value)   // returned as-is
//!     // the consumer stores it as the last selected path
//! ```
//!
//! ## What this module implements
//!
//! * The [`OPEN_FOLDER_PREFIX`] constant — `__open_folder__`.
//! Pinned at compile time.
//! * [`encode_open_folder`] — wraps a folder path with the prefix.
//! * [`decode_open_folder`] — strips the prefix if present and
//! returns either the inner folder path (`Decoded::Folder`) or
//! the exact value (`Decoded::File`). The selector pattern-
//! matches on this when a row is chosen.

/// Sentinel prefix for "open folder" rows. Pinned exactly.
pub const OPEN_FOLDER_PREFIX: &str = "__open_folder__";

/// Wrap a folder path with the `__open_folder__` prefix. Plain
/// concatenation — no escaping.
pub fn encode_open_folder(folder_path: &str) -> String {
    let mut s = String::with_capacity(OPEN_FOLDER_PREFIX.len() + folder_path.len());
    s.push_str(OPEN_FOLDER_PREFIX);
    s.push_str(folder_path);
    s
}

/// The result of decoding a row value from the selector.
///
/// The decoding branch:
///
/// * `Folder(path)` — value started with `OPEN_FOLDER_PREFIX`. The
/// selector creates the folder (recursively) and opens it. The
/// crate does **not** execute either side effect; the consumer
/// pattern-matches on this variant and runs whatever I/O it wants.
/// * `File(path)` — value did NOT start with the prefix. The
/// selector stores the value as the last selected path and hands
/// it on as the chosen file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Decoded<'a> {
    /// "Open folder" row. Inner string is the folder path with the
    /// prefix stripped.
    Folder(&'a str),
    /// "Open file" row. Inner string is the value exact.
    File(&'a str),
}

impl<'a> Decoded<'a> {
    /// `true` for the `Folder(_)` variant.
    pub const fn is_folder(&self) -> bool {
        matches!(self, Self::Folder(_))
    }

    /// `true` for the `File(_)` variant.
    pub const fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /// Inner string regardless of variant.
    pub const fn inner(&self) -> &'a str {
        match self {
            Self::Folder(s) | Self::File(s) => s,
        }
    }
}

/// Decode a row value from the selector into either a [`Decoded::Folder`]
/// (with the prefix stripped) or a [`Decoded::File`] (exact).
pub fn decode_open_folder(value: &str) -> Decoded<'_> {
    if let Some(rest) = value.strip_prefix(OPEN_FOLDER_PREFIX) {
        Decoded::Folder(rest)
    } else {
        Decoded::File(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_folder_prefix_is_pinned() {
        // The literal is pinned.
        assert_eq!(OPEN_FOLDER_PREFIX, "__open_folder__");
    }

    #[test]
    fn open_folder_prefix_length_is_15() {
        assert_eq!(OPEN_FOLDER_PREFIX.len(), 15);
    }

    #[test]
    fn encode_concatenates_prefix() {
        assert_eq!(
            encode_open_folder("/home/u/.rebon/auto-mem"),
            "__open_folder__/home/u/.rebon/auto-mem"
        );
    }

    #[test]
    fn encode_with_empty_path_yields_just_the_prefix() {
        assert_eq!(encode_open_folder(""), "__open_folder__");
    }

    #[test]
    fn decode_recognises_prefixed_value_as_folder() {
        let v = "__open_folder__/home/u/auto";
        let d = decode_open_folder(v);
        assert!(d.is_folder());
        assert_eq!(d.inner(), "/home/u/auto");
    }

    #[test]
    fn decode_falls_through_to_file_when_not_prefixed() {
        let v = "/home/u/.rebon/REBON.md";
        let d = decode_open_folder(v);
        assert!(d.is_file());
        assert!(!d.is_folder());
        assert_eq!(d.inner(), v);
    }

    #[test]
    fn decode_empty_string_is_file_variant() {
        let d = decode_open_folder("");
        assert_eq!(d, Decoded::File(""));
    }

    #[test]
    fn decode_just_prefix_is_folder_with_empty_inner() {
        let d = decode_open_folder("__open_folder__");
        assert_eq!(d, Decoded::Folder(""));
    }

    #[test]
    fn round_trip_encode_decode_preserves_path() {
        let original = "/some/folder/path";
        let encoded = encode_open_folder(original);
        let decoded = decode_open_folder(&encoded);
        assert_eq!(decoded, Decoded::Folder(original));
    }

    #[test]
    fn decode_partial_prefix_is_file() {
        // Defensive: a value that contains, but doesn't start with,
        // the prefix should NOT decode as a folder.
        let v = "/__open_folder__/oops";
        assert_eq!(decode_open_folder(v), Decoded::File(v));
    }

    #[test]
    fn decode_prefix_appears_twice_only_strips_first() {
        let v = "__open_folder____open_folder__/x";
        // After stripping the first prefix, the inner is still
        // `__open_folder__/x` — this is NOT re-decoded, since the
        // selector calls `decode_open_folder` exactly once.
        assert_eq!(decode_open_folder(v), Decoded::Folder("__open_folder__/x"));
    }

    #[test]
    fn decode_table() {
        let cases: &[(&str, Decoded)] = &[
            ("__open_folder__/a", Decoded::Folder("/a")),
            ("__open_folder__", Decoded::Folder("")),
            ("/a/b", Decoded::File("/a/b")),
            ("", Decoded::File("")),
            ("__open_folder", Decoded::File("__open_folder")),
        ];
        for (input, expected) in cases {
            assert_eq!(&decode_open_folder(input), expected, "decode {input:?}");
        }
    }
}
