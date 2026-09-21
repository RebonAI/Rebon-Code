// Build-time work for the `rebon` binary:
//
// * `REBON_BETA_TAG` — optional version-tag override for local/dev builds.
//   When set, the compile-time `CARGO_PKG_VERSION` is replaced with
//   `<package-version>-<tag>` so every `env!("CARGO_PKG_VERSION")` call
//   site (header, footer, ACP `initialize` reply, etc.) renders the
//   suffixed version. No-op when unset or empty.
// * The Windows icon resource.
// * The `rebon serve` page: `assets/web-ui/dist` (a committed Vite build) is
//   turned into a static table of `include_bytes!` so the binary carries
//   the page and `cargo build` never needs Node. When the directory is
//   missing — a checkout that deleted it — a placeholder page that says how
//   to build it is embedded instead, so the crate still compiles and the
//   server still answers.

use std::path::{Path, PathBuf};

const WINDOWS_ICON: &str = "../../assets/app-icon/icon.ico";
const WEB_UI_DIST: &str = "../../assets/web-ui/dist";

fn main() {
    println!("cargo:rerun-if-env-changed=REBON_BETA_TAG");
    println!("cargo:rerun-if-changed={WINDOWS_ICON}");

    #[cfg(windows)]
    {
        // `cfg(windows)` in a build script asks where cargo is *running*,
        // not what it is building for — it stays true while cross-compiling
        // to Linux from a Windows box. The resource compiler accepts only
        // `gnu` / `msvc` target envs, so it has to be asked separately what
        // the target is; without this, `cargo zigbuild --target
        // x86_64-unknown-linux-musl` on a Windows host dies here before a
        // line of rebon is compiled. CI never saw it because CI cross-builds
        // from Linux, where the whole block is absent.
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            let mut resource = winresource::WindowsResource::new();
            resource.set_icon(WINDOWS_ICON);
            resource
                .compile()
                .expect("failed to embed the Rebon icon into the Windows CLI executable");
        }
    }

    embed_web_ui();

    let Ok(tag) = std::env::var("REBON_BETA_TAG") else {
        return;
    };
    let tag = tag.trim();
    if tag.is_empty() {
        return;
    }

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    println!("cargo:rustc-env=CARGO_PKG_VERSION={version}-{tag}");
}

fn embed_web_ui() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let dist = manifest_dir.join(WEB_UI_DIST);
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    if dist.join("index.html").is_file() {
        collect(&dist, &dist, &mut files);
    }
    files.sort();

    let mut source = String::new();
    source.push_str("/// The web UI build: `(path relative to the build root, bytes)`.\n");
    source.push_str("pub static WEB_ASSETS: &[(&str, &[u8])] = &[\n");
    let placeholder = files.is_empty();
    if placeholder {
        let path = out_dir.join("web_ui_placeholder.html");
        std::fs::write(&path, PLACEHOLDER_HTML).expect("write the placeholder page");
        source.push_str(&format!(
            "    (\"index.html\", include_bytes!({})),\n",
            rust_string(&path.to_string_lossy())
        ));
    } else {
        for relative in &files {
            let absolute = dist.join(relative);
            println!("cargo:rerun-if-changed={}", absolute.display());
            source.push_str(&format!(
                "    ({}, include_bytes!({})),\n",
                rust_string(relative),
                rust_string(&absolute.to_string_lossy())
            ));
        }
    }
    source.push_str("];\n");
    source.push_str(&format!(
        "/// Whether the table is the placeholder rather than a build.\npub const WEB_ASSETS_PLACEHOLDER: bool = {placeholder};\n"
    ));
    source.push_str(&format!(
        "/// Where the build was read from at compile time.\npub const WEB_ASSETS_SOURCE: &str = {};\n",
        rust_string(&if placeholder {
            "placeholder (assets/web-ui/dist was absent at build time)".to_string()
        } else {
            format!("assets/web-ui/dist ({} files)", files.len())
        })
    ));
    std::fs::write(out_dir.join("web_assets.rs"), source).expect("write web_assets.rs");
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("under the root")
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push(relative);
        }
    }
}

/// A Rust string literal for `text` (raw, so Windows paths survive).
fn rust_string(text: &str) -> String {
    let mut hashes = String::new();
    while text.contains(&format!("\"{hashes}")) {
        hashes.push('#');
    }
    format!("r{hashes}\"{text}\"{hashes}")
}

const PLACEHOLDER_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Rebon</title>
<style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:4rem auto;padding:0 1rem;color:#21272e;background:#f9fbfe}code{background:#ecf0f3;padding:.1rem .3rem;border-radius:4px}</style>
</head><body>
<h1>The web UI is not built into this binary</h1>
<p>This <code>rebon</code> was compiled while <code>assets/web-ui/dist</code> was absent. The page ships separately as the <code>@rebon/rebon-web</code> npm package. Restore that directory and rebuild, or point the server at a build you already have:</p>
<pre><code>cargo build -p rebon-cli

# or, without rebuilding:
rebon serve --web-ui &lt;path to a built web UI&gt;</code></pre>
<p>The ACP socket at <code>/ws</code> and the <code>/api/*</code> reads work regardless.</p>
</body></html>
"#;
