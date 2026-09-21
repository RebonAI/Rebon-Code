//! Filesystem path to `file://` URL, for the OSC-8 hyperlinks the tool
//! cards paint.
//!
//! Arrived here when the misc helper crate was taken
//! apart. Only this one conversion survived the move: the module
//! also carried `build_clickable_image_ref`, `build_file_path_link`,
//! `image_ref_label` and their two plan structs, none of which had a
//! caller anywhere in the tree.
//!
//! Its pair is `rebon_shell::create_hyperlink`, which wraps the URL this
//! returns in the OSC-8 escape. Note the second, laxer answer to the
//! same question in `rebon_render::common::simple_file_url`: that one
//! string-rewrites the separators and percent-encodes nothing, so a path
//! with a space comes out unusable. The two have not been merged because
//! the semantics differ; whoever merges them should keep this one.

use std::path::Path;

use url::Url;

/// Converts a filesystem path into a `file://` URL.
pub(super) fn file_path_url(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(|url| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_percent_encodes_a_space() {
        let path = std::env::current_dir()
            .expect("cwd")
            .join("fixtures")
            .join("image 1.png");
        let url = file_path_url(&path).expect("absolute path converts");
        assert!(url.starts_with("file:"));
        assert!(url.contains("%20"));
    }

    #[test]
    fn relative_path_has_no_file_url() {
        assert_eq!(file_path_url(Path::new("relative/notes.txt")), None);
    }
}
