//! MCP server config types — the discriminated-union input to the
//! rest of the crate.
//!
//! Covers the eight variants of `ServerConfigKind` plus the
//! `ConfigScope` enum and the `ScopedMcpServerConfig` wrapper.
//!
//! Schema validation is out of scope here; this crate takes
//! already-parsed structs as input (validation is the consumer's
//! responsibility). The Rust types are pure data — no Cargo
//! deps (no serde, no chrono) — so this module stays dep-free.
//!
//! The variant names are pinned to the wire `type` literals used by
//! serialized config files:
//!
//! | Wire `type` value | Rust [`TransportKind`] | Rust [`ServerConfigKind`] |
//! |---|---|---|
//! | `'stdio'` (or absent) | `TransportKind::Stdio` | `ServerConfigKind::Stdio` |
//! | `'sse'` | `TransportKind::Sse` | `ServerConfigKind::Sse` |
//! | `'sse-ide'` | `TransportKind::SseIde` | `ServerConfigKind::SseIde` |
//! | `'ws-ide'` | `TransportKind::WsIde` | `ServerConfigKind::WsIde` |
//! | `'http'` | `TransportKind::Http` | `ServerConfigKind::Http` |
//! | `'ws'` | `TransportKind::Ws` | `ServerConfigKind::Ws` |
//! | `'sdk'` | `TransportKind::Sdk` | `ServerConfigKind::Sdk` |
//! | `'claudeai-proxy'` | `TransportKind::ClaudeAiProxy` | `ServerConfigKind::ClaudeAiProxy` |
//!
//! The stdio variant has an optional `type` field (`'stdio'` or absent)
//! for backwards compatibility. All other variants require the
//! discriminator.

use std::collections::BTreeMap;

/// The scope a server config is sourced from.
///
/// The order is pinned — any reorder would be a compat break against
/// serialized config files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigScope {
    Local,
    User,
    Project,
    Dynamic,
    Enterprise,
    ClaudeAi,
    Managed,
}

impl ConfigScope {
    /// The wire string for this scope.
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfigScope::Local => "local",
            ConfigScope::User => "user",
            ConfigScope::Project => "project",
            ConfigScope::Dynamic => "dynamic",
            ConfigScope::Enterprise => "enterprise",
            ConfigScope::ClaudeAi => "claudeai",
            ConfigScope::Managed => "managed",
        }
    }

    /// Parse a wire string back into the enum.
    pub fn from_str(s: &str) -> Option<ConfigScope> {
        match s {
            "local" => Some(ConfigScope::Local),
            "user" => Some(ConfigScope::User),
            "project" => Some(ConfigScope::Project),
            "dynamic" => Some(ConfigScope::Dynamic),
            "enterprise" => Some(ConfigScope::Enterprise),
            "claudeai" => Some(ConfigScope::ClaudeAi),
            "managed" => Some(ConfigScope::Managed),
            _ => None,
        }
    }

    /// The full list of scopes in declaration order.
    pub const ALL: [ConfigScope; 7] = [
        ConfigScope::Local,
        ConfigScope::User,
        ConfigScope::Project,
        ConfigScope::Dynamic,
        ConfigScope::Enterprise,
        ConfigScope::ClaudeAi,
        ConfigScope::Managed,
    ];
}

/// The transport kind of a server config: one variant per
/// [`ServerConfigKind`] variant, `sdk` and `claudeai-proxy` included.
/// Those two are server configs rather than transports in the MCP sense,
/// but the list still needs a tag to render for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    Stdio,
    Sse,
    SseIde,
    WsIde,
    Http,
    Ws,
    Sdk,
    ClaudeAiProxy,
}

impl TransportKind {
    /// The wire string — the discriminator used by the `type:` field.
    pub fn as_str(&self) -> &'static str {
        match self {
            TransportKind::Stdio => "stdio",
            TransportKind::Sse => "sse",
            TransportKind::SseIde => "sse-ide",
            TransportKind::WsIde => "ws-ide",
            TransportKind::Http => "http",
            TransportKind::Ws => "ws",
            TransportKind::Sdk => "sdk",
            TransportKind::ClaudeAiProxy => "claudeai-proxy",
        }
    }
}

/// The stdio server variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStdioServerConfig {
    /// The executable to run.
    pub command: String,
    /// Args passed to the executable. Defaults to empty.
    pub args: Vec<String>,
    /// Environment variables. None means "inherit the parent's env".
    pub env: Option<BTreeMap<String, String>>,
}

/// OAuth config for remote server variants.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct McpOAuthConfig {
    pub client_id: Option<String>,
    pub callback_port: Option<u16>,
    /// Must start with `https://`; validated in [`McpOAuthConfig::validate`].
    pub auth_server_metadata_url: Option<String>,
    pub xaa: Option<bool>,
}

impl McpOAuthConfig {
    /// Validate the `authServerMetadataUrl` https:// prefix rule.
    /// Returns `Ok(())` on pass and a descriptive message on fail.
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some(url) = &self.auth_server_metadata_url {
            if !url.starts_with("https://") {
                return Err("authServerMetadataUrl must use https://");
            }
        }
        Ok(())
    }
}

/// The SSE server variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSseServerConfig {
    pub url: String,
    pub headers: Option<BTreeMap<String, String>>,
    pub headers_helper: Option<String>,
    pub oauth: Option<McpOAuthConfig>,
}

/// The SSE-IDE internal variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSseIdeServerConfig {
    pub url: String,
    pub ide_name: String,
    pub ide_running_in_windows: Option<bool>,
}

/// The WebSocket-IDE internal variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpWsIdeServerConfig {
    pub url: String,
    pub ide_name: String,
    pub auth_token: Option<String>,
    pub ide_running_in_windows: Option<bool>,
}

/// The HTTP server variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHttpServerConfig {
    pub url: String,
    pub headers: Option<BTreeMap<String, String>>,
    pub headers_helper: Option<String>,
    pub oauth: Option<McpOAuthConfig>,
}

/// The WebSocket server variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpWsServerConfig {
    pub url: String,
    pub headers: Option<BTreeMap<String, String>>,
    pub headers_helper: Option<String>,
}

/// The SDK server variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSdkServerConfig {
    pub name: String,
}

/// The `claudeai-proxy` server variant. Parsed so config files that
/// declare it keep loading; Rebon does not connect it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpClaudeAIProxyServerConfig {
    pub url: String,
    pub id: String,
}

/// Discriminated union of the eight MCP server config variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerConfigKind {
    Stdio(McpStdioServerConfig),
    Sse(McpSseServerConfig),
    SseIde(McpSseIdeServerConfig),
    WsIde(McpWsIdeServerConfig),
    Http(McpHttpServerConfig),
    Ws(McpWsServerConfig),
    Sdk(McpSdkServerConfig),
    ClaudeAiProxy(McpClaudeAIProxyServerConfig),
}

impl ServerConfigKind {
    /// The transport kind. Handy for rendering.
    pub fn transport(&self) -> TransportKind {
        match self {
            ServerConfigKind::Stdio(_) => TransportKind::Stdio,
            ServerConfigKind::Sse(_) => TransportKind::Sse,
            ServerConfigKind::SseIde(_) => TransportKind::SseIde,
            ServerConfigKind::WsIde(_) => TransportKind::WsIde,
            ServerConfigKind::Http(_) => TransportKind::Http,
            ServerConfigKind::Ws(_) => TransportKind::Ws,
            ServerConfigKind::Sdk(_) => TransportKind::Sdk,
            ServerConfigKind::ClaudeAiProxy(_) => TransportKind::ClaudeAiProxy,
        }
    }

    /// Whether this variant is a remote (networked) config. Stdio,
    /// sdk, and IDE variants are local; sse / http / ws /
    /// claudeai-proxy are remote.
    pub fn is_remote(&self) -> bool {
        matches!(
            self,
            ServerConfigKind::Sse(_)
                | ServerConfigKind::Http(_)
                | ServerConfigKind::Ws(_)
                | ServerConfigKind::ClaudeAiProxy(_)
        )
    }

    /// Whether this variant is IDE-internal. Pinned because the UI
    /// hides IDE variants from the list panel.
    pub fn is_ide_internal(&self) -> bool {
        matches!(
            self,
            ServerConfigKind::SseIde(_) | ServerConfigKind::WsIde(_)
        )
    }

    /// The oauth block, if present. Only sse / http variants have
    /// one.
    pub fn oauth(&self) -> Option<&McpOAuthConfig> {
        match self {
            ServerConfigKind::Sse(c) => c.oauth.as_ref(),
            ServerConfigKind::Http(c) => c.oauth.as_ref(),
            _ => None,
        }
    }
}

/// A server config plus the scope it was loaded from plus the
/// optional plugin source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedMcpServerConfig {
    pub config: ServerConfigKind,
    pub scope: ConfigScope,
    /// For plugin-provided servers: the providing plugin's
    /// source identifier (e.g. `"slack@official"`). Stashed at
    /// config-build time so the channel gate has it without waiting
    /// on later session state.
    pub plugin_source: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ConfigScope ---

    #[test]
    fn config_scope_round_trip_all() {
        for scope in ConfigScope::ALL {
            assert_eq!(ConfigScope::from_str(scope.as_str()), Some(scope));
        }
    }

    #[test]
    fn config_scope_unknown_returns_none() {
        assert_eq!(ConfigScope::from_str("global"), None);
        assert_eq!(ConfigScope::from_str(""), None);
        assert_eq!(ConfigScope::from_str("Local"), None); // case-sensitive
    }

    #[test]
    fn config_scope_wire_strings_are_pinned() {
        // These wire strings are pinned; changing them is a compat break.
        assert_eq!(ConfigScope::Local.as_str(), "local");
        assert_eq!(ConfigScope::User.as_str(), "user");
        assert_eq!(ConfigScope::Project.as_str(), "project");
        assert_eq!(ConfigScope::Dynamic.as_str(), "dynamic");
        assert_eq!(ConfigScope::Enterprise.as_str(), "enterprise");
        assert_eq!(ConfigScope::ClaudeAi.as_str(), "claudeai");
        assert_eq!(ConfigScope::Managed.as_str(), "managed");
    }

    #[test]
    fn config_scope_all_has_seven_entries() {
        // Guardrail: adding a new scope without updating the
        // declaration list here should break the test.
        assert_eq!(ConfigScope::ALL.len(), 7);
    }

    // --- TransportKind ---

    #[test]
    fn transport_kind_wire_strings_are_pinned() {
        assert_eq!(TransportKind::Stdio.as_str(), "stdio");
        assert_eq!(TransportKind::Sse.as_str(), "sse");
        assert_eq!(TransportKind::SseIde.as_str(), "sse-ide");
        assert_eq!(TransportKind::WsIde.as_str(), "ws-ide");
        assert_eq!(TransportKind::Http.as_str(), "http");
        assert_eq!(TransportKind::Ws.as_str(), "ws");
        assert_eq!(TransportKind::Sdk.as_str(), "sdk");
        assert_eq!(TransportKind::ClaudeAiProxy.as_str(), "claudeai-proxy");
    }

    // --- OAuth validation (the https:// rule) ---

    #[test]
    fn oauth_valid_with_https_url() {
        let cfg = McpOAuthConfig {
            auth_server_metadata_url: Some("https://auth.example.com/.well-known".into()),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn oauth_rejects_http_url() {
        let cfg = McpOAuthConfig {
            auth_server_metadata_url: Some("http://auth.example.com/.well-known".into()),
            ..Default::default()
        };
        assert_eq!(
            cfg.validate().unwrap_err(),
            "authServerMetadataUrl must use https://"
        );
    }

    #[test]
    fn oauth_empty_url_is_rejected() {
        let cfg = McpOAuthConfig {
            auth_server_metadata_url: Some("".into()),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn oauth_valid_when_url_absent() {
        let cfg = McpOAuthConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn oauth_validation_error_message_is_expected() {
        // This is the exact message users see in error banners. A
        // drift here would change user-visible copy.
        let cfg = McpOAuthConfig {
            auth_server_metadata_url: Some("ftp://a".into()),
            ..Default::default()
        };
        assert_eq!(
            cfg.validate().unwrap_err(),
            "authServerMetadataUrl must use https://"
        );
    }

    // --- ServerConfigKind discriminator ---

    #[test]
    fn stdio_transport_is_stdio() {
        let k = ServerConfigKind::Stdio(McpStdioServerConfig {
            command: "node".into(),
            args: vec!["server.js".into()],
            env: None,
        });
        assert_eq!(k.transport(), TransportKind::Stdio);
        assert!(!k.is_remote());
        assert!(!k.is_ide_internal());
    }

    #[test]
    fn sse_is_remote() {
        let k = ServerConfigKind::Sse(McpSseServerConfig {
            url: "https://example.com/sse".into(),
            headers: None,
            headers_helper: None,
            oauth: None,
        });
        assert!(k.is_remote());
        assert_eq!(k.transport(), TransportKind::Sse);
    }

    #[test]
    fn http_is_remote() {
        let k = ServerConfigKind::Http(McpHttpServerConfig {
            url: "https://example.com/mcp".into(),
            headers: None,
            headers_helper: None,
            oauth: None,
        });
        assert!(k.is_remote());
    }

    #[test]
    fn ws_is_remote() {
        let k = ServerConfigKind::Ws(McpWsServerConfig {
            url: "wss://example.com/mcp".into(),
            headers: None,
            headers_helper: None,
        });
        assert!(k.is_remote());
    }

    #[test]
    fn claudeai_proxy_is_remote() {
        let k = ServerConfigKind::ClaudeAiProxy(McpClaudeAIProxyServerConfig {
            url: "https://proxy.example/mcp".into(),
            id: "proxy-1".into(),
        });
        assert!(k.is_remote());
    }

    #[test]
    fn sse_ide_is_ide_internal_and_not_remote() {
        let k = ServerConfigKind::SseIde(McpSseIdeServerConfig {
            url: "http://localhost:5000".into(),
            ide_name: "VSCode".into(),
            ide_running_in_windows: None,
        });
        assert!(k.is_ide_internal());
        assert!(!k.is_remote());
    }

    #[test]
    fn ws_ide_is_ide_internal() {
        let k = ServerConfigKind::WsIde(McpWsIdeServerConfig {
            url: "ws://localhost:5000".into(),
            ide_name: "VSCode".into(),
            auth_token: None,
            ide_running_in_windows: Some(true),
        });
        assert!(k.is_ide_internal());
    }

    #[test]
    fn sdk_transport_is_sdk() {
        let k = ServerConfigKind::Sdk(McpSdkServerConfig {
            name: "custom".into(),
        });
        assert_eq!(k.transport(), TransportKind::Sdk);
        assert!(!k.is_remote());
        assert!(!k.is_ide_internal());
    }

    #[test]
    fn oauth_accessor_returns_some_only_for_sse_and_http() {
        let sse = ServerConfigKind::Sse(McpSseServerConfig {
            url: "x".into(),
            headers: None,
            headers_helper: None,
            oauth: Some(McpOAuthConfig::default()),
        });
        assert!(sse.oauth().is_some());

        let http = ServerConfigKind::Http(McpHttpServerConfig {
            url: "x".into(),
            headers: None,
            headers_helper: None,
            oauth: Some(McpOAuthConfig::default()),
        });
        assert!(http.oauth().is_some());

        let stdio = ServerConfigKind::Stdio(McpStdioServerConfig {
            command: "x".into(),
            args: vec![],
            env: None,
        });
        assert!(stdio.oauth().is_none());

        let ws = ServerConfigKind::Ws(McpWsServerConfig {
            url: "x".into(),
            headers: None,
            headers_helper: None,
        });
        // ws has headers but NO oauth block.
        assert!(ws.oauth().is_none());
    }

    #[test]
    fn scoped_config_round_trip() {
        let scoped = ScopedMcpServerConfig {
            config: ServerConfigKind::Stdio(McpStdioServerConfig {
                command: "python".into(),
                args: vec!["-m".into(), "server".into()],
                env: Some({
                    let mut m = BTreeMap::new();
                    m.insert("API_KEY".into(), "secret".into());
                    m
                }),
            }),
            scope: ConfigScope::User,
            plugin_source: Some("slack@official".into()),
        };
        assert_eq!(scoped.scope, ConfigScope::User);
        assert_eq!(scoped.plugin_source.as_deref(), Some("slack@official"));
        assert_eq!(scoped.config.transport(), TransportKind::Stdio);
    }

    #[test]
    fn stdio_config_default_args_is_empty() {
        // `args` defaults to empty rather than `None`.
        let cfg = McpStdioServerConfig {
            command: "echo".into(),
            args: vec![],
            env: None,
        };
        assert!(cfg.args.is_empty());
    }

    #[test]
    fn stdio_config_preserves_arg_order() {
        let cfg = McpStdioServerConfig {
            command: "node".into(),
            args: vec![
                "--experimental".into(),
                "server.js".into(),
                "--port=3000".into(),
            ],
            env: None,
        };
        assert_eq!(cfg.args.len(), 3);
        assert_eq!(cfg.args[0], "--experimental");
        assert_eq!(cfg.args[2], "--port=3000");
    }

    #[test]
    fn all_scope_round_trip_unique() {
        let mut seen = std::collections::HashSet::new();
        for scope in ConfigScope::ALL {
            assert!(seen.insert(scope.as_str()));
        }
        assert_eq!(seen.len(), 7);
    }
}
