//! The page `rebon serve` serves: a build of the web UI, published
//! separately as the `@rebon/rebon-web` npm package.
//!
//! The build (`assets/web-ui/dist`, committed) is compiled into the binary
//! by `build.rs`, so `rebon serve` needs nothing beside the executable.
//! `--web-ui <dir>` swaps in a directory on disk instead — a `pnpm dev`
//! build, a customised page — read on every request so an edit shows on
//! reload. Either way the page is a single-page app: a path that names no
//! file and has no extension is the app's own route and gets `index.html`.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

use rebon_proto::web_api::WebUiSource;

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

/// One file of the page, ready to send.
pub struct Asset {
    pub body: Cow<'static, [u8]>,
    pub content_type: &'static str,
    /// Whether this is the app shell (gets the CSP header).
    pub is_document: bool,
}

#[derive(Debug, Clone)]
pub enum WebAssets {
    /// The build compiled into the binary.
    Embedded,
    /// A directory on disk, resolved to an absolute path.
    Directory(PathBuf),
}

impl WebAssets {
    /// Serve from `dir`, which must exist and contain an `index.html`.
    pub fn directory(dir: &Path) -> anyhow::Result<Self> {
        let root = dir
            .canonicalize()
            .map_err(|err| anyhow::anyhow!("--web-ui {}: {err}", dir.display()))?;
        if !root.join("index.html").is_file() {
            anyhow::bail!(
                "--web-ui {}: no index.html there; point at a built web UI (the @rebon/rebon-web package, or the copy this binary embeds from assets/web-ui/dist)",
                dir.display()
            );
        }
        Ok(Self::Directory(root))
    }

    /// What `/api/info` reports about the page's origin.
    ///
    /// The two cases stay two shapes: an embedded build reports what it was
    /// built from, a directory reports no `built` key at all, and that
    /// asymmetry is what the wire has always carried.
    pub fn describe(&self) -> WebUiSource {
        match self {
            Self::Embedded => WebUiSource {
                source: "embedded".into(),
                path: None,
                built: Some(WEB_ASSETS_SOURCE.to_string()),
            },
            Self::Directory(root) => WebUiSource {
                source: "directory".into(),
                path: Some(root.to_string_lossy().into_owned()),
                built: None,
            },
        }
    }

    /// Whether the embedded table is a real build rather than the
    /// placeholder `build.rs` writes when `assets/web-ui/dist` is absent.
    pub fn embedded_is_placeholder() -> bool {
        WEB_ASSETS_PLACEHOLDER
    }

    /// The file for a request path, or `None` for a 404.
    pub fn get(&self, request_path: &str) -> Option<Asset> {
        let relative = normalize(request_path)?;
        let candidate = if relative.is_empty() {
            "index.html"
        } else {
            relative.as_str()
        };
        if let Some(asset) = self.lookup(candidate) {
            return Some(asset);
        }
        // An app route (`/sessions/abc`) rather than a missing file.
        let last = candidate.rsplit('/').next().unwrap_or(candidate);
        if !last.contains('.') {
            return self.lookup("index.html");
        }
        None
    }

    fn lookup(&self, relative: &str) -> Option<Asset> {
        let content_type = content_type_for(relative);
        let is_document = relative == "index.html";
        match self {
            Self::Embedded => {
                WEB_ASSETS
                    .iter()
                    .find(|(name, _)| *name == relative)
                    .map(|(_, body)| Asset {
                        body: Cow::Borrowed(body),
                        content_type,
                        is_document,
                    })
            }
            Self::Directory(root) => {
                let path = root.join(relative);
                let resolved = path.canonicalize().ok()?;
                if !resolved.starts_with(root) || !resolved.is_file() {
                    return None;
                }
                std::fs::read(&resolved).ok().map(|body| Asset {
                    body: Cow::Owned(body),
                    content_type,
                    is_document,
                })
            }
        }
    }
}

/// `/assets/app.js` → `assets/app.js`; anything that escapes the root
/// (`..`, an absolute component, a drive) is refused.
fn normalize(request_path: &str) -> Option<String> {
    let trimmed = request_path.trim_start_matches('/');
    if trimmed.contains('\\') || trimmed.contains('\0') {
        return None;
    }
    let mut parts = Vec::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

fn content_type_for(path: &str) -> &'static str {
    let extension = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match extension.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_build_has_a_document_and_its_script() {
        let assets = WebAssets::Embedded;
        let index = assets.get("/").expect("index.html");
        assert!(index.is_document);
        assert_eq!(index.content_type, "text/html; charset=utf-8");
        let html = String::from_utf8_lossy(&index.body);
        if WebAssets::embedded_is_placeholder() {
            assert!(
                html.contains("assets/web-ui/dist"),
                "placeholder names the directory the build is expected in"
            );
            return;
        }
        // The real build references its bundle, which must be in the table.
        let script = html
            .split("src=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("index.html references a script");
        let bundle = assets
            .get(script)
            .expect("the referenced bundle is embedded");
        assert_eq!(bundle.content_type, "text/javascript; charset=utf-8");
        assert!(!bundle.is_document);
    }

    #[test]
    fn app_routes_fall_back_to_the_document_and_files_do_not() {
        let assets = WebAssets::Embedded;
        assert!(assets.get("/sessions/abc").is_some_and(|a| a.is_document));
        assert!(assets.get("/assets/missing.js").is_none());
        assert!(assets.get("/../Cargo.toml").is_none());
        assert!(assets.get("/..\\Cargo.toml").is_none());
    }

    #[test]
    fn a_directory_is_confined_to_its_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<html>x</html>").unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/app.js"), "1").unwrap();
        let assets = WebAssets::directory(dir.path()).unwrap();
        assert!(assets.get("/assets/app.js").is_some());
        assert!(assets.get("/route/here").is_some_and(|a| a.is_document));
        assert!(assets.get("/../").is_none());
        assert!(assets.get("/assets/../../etc/passwd").is_none());
        assert!(WebAssets::directory(&dir.path().join("nope")).is_err());
    }

    #[test]
    fn content_types_follow_the_extension() {
        assert_eq!(
            content_type_for("assets/app.css"),
            "text/css; charset=utf-8"
        );
        assert_eq!(content_type_for("a.woff2"), "font/woff2");
        assert_eq!(content_type_for("noext"), "application/octet-stream");
    }
}
