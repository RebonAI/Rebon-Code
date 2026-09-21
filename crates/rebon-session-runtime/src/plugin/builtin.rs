use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuiltinPluginAlias {
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
}

pub(crate) const RUST_LSP_ALIAS: BuiltinPluginAlias = BuiltinPluginAlias {
    name: "rust-lsp",
    version: "1",
    description:
        "Expose rust-analyzer diagnostics and hover through Rebon's built-in rust_lsp MCP bridge.",
};

pub(crate) fn builtin_alias(name: &str) -> Option<BuiltinPluginAlias> {
    match name.trim() {
        "rust-lsp" => Some(RUST_LSP_ALIAS),
        _ => None,
    }
}

/// The MCP server entry the `rust-lsp` builtin alias installs.
///
/// Points at `rebon-lsp-mcp`, the binary half of this alias, resolved beside
/// `rebon_exe`. Fails when the
/// binary is not installed rather than writing a command that cannot start.
pub(crate) fn rust_lsp_mcp_config_payload(
    cwd: &Path,
    rebon_exe: Option<&Path>,
) -> anyhow::Result<serde_json::Value> {
    let bridge = match rebon_exe {
        Some(exe) => {
            rebon_types::sibling_binary::resolve_from_executable(crate::LSP_MCP_SIBLING, exe)?
        }
        None => rebon_types::sibling_binary::resolve(crate::LSP_MCP_SIBLING)?,
    };
    Ok(serde_json::json!({
        "mcpServers": {
            crate::RUST_LSP_SERVER_NAME: {
                "command": bridge.to_string_lossy(),
                "args": ["rust"],
                "cwd": cwd.to_string_lossy(),
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_only_rust_lsp_alias() {
        assert_eq!(builtin_alias("rust-lsp").unwrap().name, "rust-lsp");
        assert!(builtin_alias("unknown").is_none());
    }

    #[test]
    fn rust_lsp_alias_points_at_the_installed_bridge_binary() {
        let directory = tempfile::tempdir().expect("temp dir");
        let rebon = directory
            .path()
            .join(rebon_types::sibling_binary::file_name("rebon"));
        let bridge = directory
            .path()
            .join(rebon_types::sibling_binary::file_name(
                crate::LSP_MCP_SIBLING,
            ));
        std::fs::write(&bridge, b"binary").expect("write bridge");
        let payload = rust_lsp_mcp_config_payload(Path::new("/tmp/project"), Some(&rebon))
            .expect("the bridge is installed");
        let server = &payload["mcpServers"][crate::RUST_LSP_SERVER_NAME];
        assert_eq!(server["args"], serde_json::json!(["rust"]));
        assert_eq!(
            server["command"].as_str().expect("command is a string"),
            bridge.to_string_lossy()
        );
    }

    #[test]
    fn rust_lsp_alias_fails_when_the_bridge_binary_is_missing() {
        let directory = tempfile::tempdir().expect("temp dir");
        let rebon = directory
            .path()
            .join(rebon_types::sibling_binary::file_name("rebon"));
        let error = rust_lsp_mcp_config_payload(Path::new("/tmp/project"), Some(&rebon))
            .expect_err("nothing was installed beside the executable")
            .to_string();
        assert!(error.contains(crate::LSP_MCP_SIBLING), "{error}");
    }
}
