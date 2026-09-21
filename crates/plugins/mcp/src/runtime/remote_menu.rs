//! Remote server menu state machine.
//!
//! Remote servers are the most complex menu: the user picks a
//! transport (sse / http / ws), provides a URL, optionally configures
//! headers + OAuth, then validates + saves.
//!
//! The Rust module includes this as:
//! * [`RemoteTransportKind`] — the user's transport choice
//! * [`RemoteAuthKind`] — Bearer / Headers / OAuth / None
//! * [`RemoteMenuStep`] — FSM step
//! * [`RemoteMenuState`] — reducer state
//! * [`RemoteMenuEvent`] — reducer event
//!
//! The `https://` / `wss://` rule in [`validate_remote_url`] is
//! pinned by a test.

use crate::runtime::config::{
    ConfigScope, McpHttpServerConfig, McpOAuthConfig, McpSseServerConfig, McpWsServerConfig,
    ScopedMcpServerConfig, ServerConfigKind,
};
use std::collections::BTreeMap;

/// The transport the user picks for a remote server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoteTransportKind {
    Sse,
    Http,
    Ws,
}

impl RemoteTransportKind {
    pub fn label(&self) -> &'static str {
        match self {
            RemoteTransportKind::Sse => "sse",
            RemoteTransportKind::Http => "http",
            RemoteTransportKind::Ws => "ws",
        }
    }
}

/// The auth strategy for a remote server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoteAuthKind {
    None,
    Bearer,
    Headers,
    OAuth,
}

impl RemoteAuthKind {
    pub fn label(&self) -> &'static str {
        match self {
            RemoteAuthKind::None => "none",
            RemoteAuthKind::Bearer => "bearer",
            RemoteAuthKind::Headers => "headers",
            RemoteAuthKind::OAuth => "oauth",
        }
    }
}

/// The FSM step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoteMenuStep {
    PickingTransport,
    EditingName,
    EditingUrl,
    PickingAuth,
    EditingHeaders,
    EditingOAuth,
    Validating,
    Saving,
    AwaitingCallback, // OAuth callback phase
    Exchanging,       // token exchange phase
    Saved,
    Error,
}

/// The menu state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMenuState {
    pub step: RemoteMenuStep,
    pub transport: Option<RemoteTransportKind>,
    pub auth: RemoteAuthKind,
    pub name: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub oauth: McpOAuthConfig,
    pub error: Option<String>,
}

impl Default for RemoteMenuState {
    fn default() -> Self {
        Self {
            step: RemoteMenuStep::PickingTransport,
            transport: None,
            auth: RemoteAuthKind::None,
            name: String::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            oauth: McpOAuthConfig::default(),
            error: None,
        }
    }
}

/// Events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteMenuEvent {
    SelectTransport(RemoteTransportKind),
    SetName(String),
    SetUrl(String),
    SelectAuth(RemoteAuthKind),
    SetHeaders(BTreeMap<String, String>),
    SetOAuth(McpOAuthConfig),
    Next,
    Back,
    Submit,
    SaveSucceeded,
    SaveFailed(String),
    OAuthCallbackReceived,
    OAuthTokenExchanged,
    Reset,
}

impl RemoteMenuState {
    pub fn apply(mut self, event: RemoteMenuEvent) -> Self {
        match event {
            RemoteMenuEvent::SelectTransport(t) => {
                self.transport = Some(t);
                self.step = RemoteMenuStep::EditingName;
                self.error = None;
            }
            RemoteMenuEvent::SetName(n) => {
                self.name = n;
                self.error = None;
            }
            RemoteMenuEvent::SetUrl(u) => {
                self.url = u;
                self.error = None;
            }
            RemoteMenuEvent::SelectAuth(a) => {
                self.auth = a;
                self.error = None;
            }
            RemoteMenuEvent::SetHeaders(h) => {
                self.headers = h;
                self.error = None;
            }
            RemoteMenuEvent::SetOAuth(o) => {
                self.oauth = o;
                self.error = None;
            }
            RemoteMenuEvent::Next => {
                self.step = self.next_step();
            }
            RemoteMenuEvent::Back => {
                self.step = self.prev_step();
                self.error = None;
            }
            RemoteMenuEvent::Submit => {
                if self.step == RemoteMenuStep::Validating {
                    // OAuth branch goes through the callback dance.
                    if self.auth == RemoteAuthKind::OAuth {
                        self.step = RemoteMenuStep::AwaitingCallback;
                    } else {
                        self.step = RemoteMenuStep::Saving;
                    }
                }
            }
            RemoteMenuEvent::SaveSucceeded => {
                if matches!(
                    self.step,
                    RemoteMenuStep::Saving | RemoteMenuStep::Exchanging
                ) {
                    self.step = RemoteMenuStep::Saved;
                }
            }
            RemoteMenuEvent::SaveFailed(msg) => {
                self.error = Some(msg);
                self.step = RemoteMenuStep::Error;
            }
            RemoteMenuEvent::OAuthCallbackReceived => {
                if self.step == RemoteMenuStep::AwaitingCallback {
                    self.step = RemoteMenuStep::Exchanging;
                }
            }
            RemoteMenuEvent::OAuthTokenExchanged => {
                if self.step == RemoteMenuStep::Exchanging {
                    self.step = RemoteMenuStep::Saving;
                }
            }
            RemoteMenuEvent::Reset => {
                self = Self::default();
            }
        }
        self
    }

    fn next_step(&mut self) -> RemoteMenuStep {
        match self.step {
            RemoteMenuStep::PickingTransport => {
                if self.transport.is_some() {
                    RemoteMenuStep::EditingName
                } else {
                    self.error = Some("Pick a transport first".into());
                    RemoteMenuStep::PickingTransport
                }
            }
            RemoteMenuStep::EditingName => {
                if self.name.trim().is_empty() {
                    self.error = Some("Name cannot be empty".into());
                    RemoteMenuStep::EditingName
                } else {
                    RemoteMenuStep::EditingUrl
                }
            }
            RemoteMenuStep::EditingUrl => match validate_remote_url(&self.url) {
                Ok(()) => RemoteMenuStep::PickingAuth,
                Err(e) => {
                    self.error = Some(e.to_string());
                    RemoteMenuStep::EditingUrl
                }
            },
            RemoteMenuStep::PickingAuth => match self.auth {
                RemoteAuthKind::Headers | RemoteAuthKind::Bearer => RemoteMenuStep::EditingHeaders,
                RemoteAuthKind::OAuth => RemoteMenuStep::EditingOAuth,
                RemoteAuthKind::None => RemoteMenuStep::Validating,
            },
            RemoteMenuStep::EditingHeaders => RemoteMenuStep::Validating,
            RemoteMenuStep::EditingOAuth => {
                if let Err(e) = self.oauth.validate() {
                    self.error = Some(e.to_string());
                    RemoteMenuStep::EditingOAuth
                } else {
                    RemoteMenuStep::Validating
                }
            }
            other => other,
        }
    }

    fn prev_step(&self) -> RemoteMenuStep {
        match self.step {
            RemoteMenuStep::EditingName => RemoteMenuStep::PickingTransport,
            RemoteMenuStep::EditingUrl => RemoteMenuStep::EditingName,
            RemoteMenuStep::PickingAuth => RemoteMenuStep::EditingUrl,
            RemoteMenuStep::EditingHeaders => RemoteMenuStep::PickingAuth,
            RemoteMenuStep::EditingOAuth => RemoteMenuStep::PickingAuth,
            RemoteMenuStep::Validating => match self.auth {
                RemoteAuthKind::Headers | RemoteAuthKind::Bearer => RemoteMenuStep::EditingHeaders,
                RemoteAuthKind::OAuth => RemoteMenuStep::EditingOAuth,
                RemoteAuthKind::None => RemoteMenuStep::PickingAuth,
            },
            RemoteMenuStep::Error => RemoteMenuStep::PickingTransport,
            other => other,
        }
    }

    /// Build the final scoped config from the draft.
    pub fn build_scoped(&self, scope: ConfigScope) -> Result<ScopedMcpServerConfig, String> {
        let transport = self
            .transport
            .ok_or_else(|| "Pick a transport first".to_string())?;

        if self.name.trim().is_empty() {
            return Err("Name cannot be empty".to_string());
        }
        validate_remote_url(&self.url).map_err(|e| e.to_string())?;

        let headers_option = if self.headers.is_empty() {
            None
        } else {
            Some(self.headers.clone())
        };
        let oauth_option = if self.auth == RemoteAuthKind::OAuth {
            self.oauth.validate().map_err(|e| e.to_string())?;
            Some(self.oauth.clone())
        } else {
            None
        };

        let config = match transport {
            RemoteTransportKind::Sse => ServerConfigKind::Sse(McpSseServerConfig {
                url: self.url.clone(),
                headers: headers_option,
                headers_helper: None,
                oauth: oauth_option,
            }),
            RemoteTransportKind::Http => ServerConfigKind::Http(McpHttpServerConfig {
                url: self.url.clone(),
                headers: headers_option,
                headers_helper: None,
                oauth: oauth_option,
            }),
            RemoteTransportKind::Ws => ServerConfigKind::Ws(McpWsServerConfig {
                url: self.url.clone(),
                headers: headers_option,
                headers_helper: None,
            }),
        };

        Ok(ScopedMcpServerConfig {
            config,
            scope,
            plugin_source: None,
        })
    }
}

/// Validate a remote URL. Returns `Ok(())` for `https://…` and
/// `ws(s)://…`; `http://…` is rejected for non-local hosts (consumer
/// may override).
pub fn validate_remote_url(url: &str) -> Result<(), &'static str> {
    if url.is_empty() {
        return Err("URL cannot be empty");
    }
    if url.starts_with("https://") || url.starts_with("wss://") {
        return Ok(());
    }
    if url.starts_with("http://") || url.starts_with("ws://") {
        // Allow localhost / 127.0.0.1 / [::1] for development.
        let rest = &url[url.find("://").unwrap() + 3..];
        if rest.starts_with("localhost")
            || rest.starts_with("127.0.0.1")
            || rest.starts_with("[::1]")
        {
            return Ok(());
        }
        return Err("URL must use https:// (or http:// for localhost)");
    }
    Err("URL must start with https:// or wss://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state() {
        let s = RemoteMenuState::default();
        assert_eq!(s.step, RemoteMenuStep::PickingTransport);
        assert!(s.transport.is_none());
        assert_eq!(s.auth, RemoteAuthKind::None);
    }

    #[test]
    fn transport_labels() {
        assert_eq!(RemoteTransportKind::Sse.label(), "sse");
        assert_eq!(RemoteTransportKind::Http.label(), "http");
        assert_eq!(RemoteTransportKind::Ws.label(), "ws");
    }

    #[test]
    fn auth_labels() {
        assert_eq!(RemoteAuthKind::None.label(), "none");
        assert_eq!(RemoteAuthKind::Bearer.label(), "bearer");
        assert_eq!(RemoteAuthKind::Headers.label(), "headers");
        assert_eq!(RemoteAuthKind::OAuth.label(), "oauth");
    }

    #[test]
    fn select_transport_advances_to_editing_name() {
        let s = RemoteMenuState::default()
            .apply(RemoteMenuEvent::SelectTransport(RemoteTransportKind::Http));
        assert_eq!(s.step, RemoteMenuStep::EditingName);
        assert_eq!(s.transport, Some(RemoteTransportKind::Http));
    }

    #[test]
    fn next_requires_transport_first() {
        let s = RemoteMenuState::default().apply(RemoteMenuEvent::Next);
        assert_eq!(s.step, RemoteMenuStep::PickingTransport);
        assert!(s.error.is_some());
    }

    #[test]
    fn happy_path_no_auth() {
        let s = RemoteMenuState::default()
            .apply(RemoteMenuEvent::SelectTransport(RemoteTransportKind::Http))
            .apply(RemoteMenuEvent::SetName("linear".into()))
            .apply(RemoteMenuEvent::Next);
        assert_eq!(s.step, RemoteMenuStep::EditingUrl);
        let s = s
            .apply(RemoteMenuEvent::SetUrl("https://mcp.linear.app/sse".into()))
            .apply(RemoteMenuEvent::Next);
        assert_eq!(s.step, RemoteMenuStep::PickingAuth);
        let s = s
            .apply(RemoteMenuEvent::SelectAuth(RemoteAuthKind::None))
            .apply(RemoteMenuEvent::Next);
        assert_eq!(s.step, RemoteMenuStep::Validating);
        let s = s.apply(RemoteMenuEvent::Submit);
        assert_eq!(s.step, RemoteMenuStep::Saving);
        let s = s.apply(RemoteMenuEvent::SaveSucceeded);
        assert_eq!(s.step, RemoteMenuStep::Saved);
    }

    #[test]
    fn oauth_path_awaits_callback_then_exchanges() {
        let s = RemoteMenuState {
            step: RemoteMenuStep::Validating,
            transport: Some(RemoteTransportKind::Http),
            auth: RemoteAuthKind::OAuth,
            name: "x".into(),
            url: "https://x.com".into(),
            headers: BTreeMap::new(),
            oauth: McpOAuthConfig::default(),
            error: None,
        };
        let s = s.apply(RemoteMenuEvent::Submit);
        assert_eq!(s.step, RemoteMenuStep::AwaitingCallback);
        let s = s.apply(RemoteMenuEvent::OAuthCallbackReceived);
        assert_eq!(s.step, RemoteMenuStep::Exchanging);
        let s = s.apply(RemoteMenuEvent::OAuthTokenExchanged);
        assert_eq!(s.step, RemoteMenuStep::Saving);
        let s = s.apply(RemoteMenuEvent::SaveSucceeded);
        assert_eq!(s.step, RemoteMenuStep::Saved);
    }

    #[test]
    fn url_validation_https_ok() {
        assert!(validate_remote_url("https://example.com/mcp").is_ok());
    }

    #[test]
    fn url_validation_wss_ok() {
        assert!(validate_remote_url("wss://example.com/mcp").is_ok());
    }

    #[test]
    fn url_validation_http_rejected_for_remote() {
        assert!(validate_remote_url("http://example.com/mcp").is_err());
    }

    #[test]
    fn url_validation_http_localhost_allowed() {
        assert!(validate_remote_url("http://localhost:3000/mcp").is_ok());
        assert!(validate_remote_url("http://127.0.0.1:3000").is_ok());
        assert!(validate_remote_url("http://[::1]:3000").is_ok());
    }

    #[test]
    fn url_validation_ws_localhost_allowed() {
        assert!(validate_remote_url("ws://localhost:3000").is_ok());
    }

    #[test]
    fn url_validation_empty_rejected() {
        assert!(validate_remote_url("").is_err());
    }

    #[test]
    fn url_validation_no_scheme_rejected() {
        assert!(validate_remote_url("example.com").is_err());
    }

    #[test]
    fn url_validation_ftp_rejected() {
        assert!(validate_remote_url("ftp://example.com").is_err());
    }

    #[test]
    fn back_from_editing_url_returns_to_name() {
        let mut s = RemoteMenuState::default();
        s.step = RemoteMenuStep::EditingUrl;
        let s = s.apply(RemoteMenuEvent::Back);
        assert_eq!(s.step, RemoteMenuStep::EditingName);
    }

    #[test]
    fn back_from_validating_with_headers_returns_to_headers() {
        let mut s = RemoteMenuState::default();
        s.step = RemoteMenuStep::Validating;
        s.auth = RemoteAuthKind::Headers;
        let s = s.apply(RemoteMenuEvent::Back);
        assert_eq!(s.step, RemoteMenuStep::EditingHeaders);
    }

    #[test]
    fn back_from_validating_with_oauth_returns_to_oauth() {
        let mut s = RemoteMenuState::default();
        s.step = RemoteMenuStep::Validating;
        s.auth = RemoteAuthKind::OAuth;
        let s = s.apply(RemoteMenuEvent::Back);
        assert_eq!(s.step, RemoteMenuStep::EditingOAuth);
    }

    #[test]
    fn back_from_validating_with_no_auth_returns_to_picking_auth() {
        let mut s = RemoteMenuState::default();
        s.step = RemoteMenuStep::Validating;
        s.auth = RemoteAuthKind::None;
        let s = s.apply(RemoteMenuEvent::Back);
        assert_eq!(s.step, RemoteMenuStep::PickingAuth);
    }

    #[test]
    fn next_from_picking_auth_with_headers_goes_to_headers_step() {
        let mut s = RemoteMenuState::default();
        s.step = RemoteMenuStep::PickingAuth;
        s.auth = RemoteAuthKind::Headers;
        s.transport = Some(RemoteTransportKind::Http);
        let s = s.apply(RemoteMenuEvent::Next);
        assert_eq!(s.step, RemoteMenuStep::EditingHeaders);
    }

    #[test]
    fn next_from_picking_auth_with_oauth_goes_to_oauth_step() {
        let mut s = RemoteMenuState::default();
        s.step = RemoteMenuStep::PickingAuth;
        s.auth = RemoteAuthKind::OAuth;
        let s = s.apply(RemoteMenuEvent::Next);
        assert_eq!(s.step, RemoteMenuStep::EditingOAuth);
    }

    #[test]
    fn next_from_editing_url_rejects_invalid() {
        let mut s = RemoteMenuState::default();
        s.step = RemoteMenuStep::EditingUrl;
        s.url = "bogus".into();
        let s = s.apply(RemoteMenuEvent::Next);
        assert_eq!(s.step, RemoteMenuStep::EditingUrl);
        assert!(s.error.is_some());
    }

    #[test]
    fn next_from_editing_url_accepts_https() {
        let mut s = RemoteMenuState::default();
        s.step = RemoteMenuStep::EditingUrl;
        s.url = "https://example.com/mcp".into();
        let s = s.apply(RemoteMenuEvent::Next);
        assert_eq!(s.step, RemoteMenuStep::PickingAuth);
    }

    #[test]
    fn save_failed_transitions_to_error() {
        let mut s = RemoteMenuState::default();
        s.step = RemoteMenuStep::Saving;
        let s = s.apply(RemoteMenuEvent::SaveFailed("timeout".into()));
        assert_eq!(s.step, RemoteMenuStep::Error);
        assert_eq!(s.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn reset_to_default() {
        let s = RemoteMenuState {
            step: RemoteMenuStep::Saved,
            transport: Some(RemoteTransportKind::Http),
            auth: RemoteAuthKind::OAuth,
            name: "x".into(),
            url: "https://x.com".into(),
            headers: BTreeMap::new(),
            oauth: McpOAuthConfig::default(),
            error: None,
        };
        let s = s.apply(RemoteMenuEvent::Reset);
        assert_eq!(s, RemoteMenuState::default());
    }

    // --- build_scoped ---

    #[test]
    fn build_scoped_http_no_auth() {
        let mut s = RemoteMenuState::default();
        s.transport = Some(RemoteTransportKind::Http);
        s.name = "x".into();
        s.url = "https://x.com/mcp".into();
        let scoped = s.build_scoped(ConfigScope::User).unwrap();
        assert!(matches!(scoped.config, ServerConfigKind::Http(_)));
        assert_eq!(scoped.scope, ConfigScope::User);
    }

    #[test]
    fn build_scoped_sse_with_headers() {
        let mut s = RemoteMenuState::default();
        s.transport = Some(RemoteTransportKind::Sse);
        s.name = "x".into();
        s.url = "https://x.com/sse".into();
        s.auth = RemoteAuthKind::Headers;
        s.headers.insert("Authorization".into(), "Bearer x".into());
        let scoped = s.build_scoped(ConfigScope::User).unwrap();
        if let ServerConfigKind::Sse(sse) = scoped.config {
            assert_eq!(sse.headers.unwrap().len(), 1);
        } else {
            panic!("expected sse");
        }
    }

    #[test]
    fn build_scoped_ws_never_has_oauth() {
        let mut s = RemoteMenuState::default();
        s.transport = Some(RemoteTransportKind::Ws);
        s.name = "x".into();
        s.url = "wss://x.com".into();
        s.auth = RemoteAuthKind::None;
        let scoped = s.build_scoped(ConfigScope::User).unwrap();
        if let ServerConfigKind::Ws(ws) = scoped.config {
            assert!(ws.headers.is_none());
        } else {
            panic!("expected ws");
        }
    }

    #[test]
    fn build_scoped_rejects_empty_name() {
        let mut s = RemoteMenuState::default();
        s.transport = Some(RemoteTransportKind::Http);
        s.url = "https://x.com".into();
        assert!(s.build_scoped(ConfigScope::User).is_err());
    }

    #[test]
    fn build_scoped_rejects_empty_url() {
        let mut s = RemoteMenuState::default();
        s.transport = Some(RemoteTransportKind::Http);
        s.name = "x".into();
        assert!(s.build_scoped(ConfigScope::User).is_err());
    }

    #[test]
    fn build_scoped_rejects_missing_transport() {
        let mut s = RemoteMenuState::default();
        s.name = "x".into();
        s.url = "https://x.com".into();
        assert!(s.build_scoped(ConfigScope::User).is_err());
    }

    #[test]
    fn build_scoped_oauth_validates_https_prefix() {
        let mut s = RemoteMenuState::default();
        s.transport = Some(RemoteTransportKind::Http);
        s.name = "x".into();
        s.url = "https://x.com".into();
        s.auth = RemoteAuthKind::OAuth;
        s.oauth = McpOAuthConfig {
            auth_server_metadata_url: Some("http://bad".into()),
            ..Default::default()
        };
        assert!(s.build_scoped(ConfigScope::User).is_err());
    }
}
