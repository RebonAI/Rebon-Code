//! The web contract that outlives the `web` plugin.
//!
//! `WebFetch` and `WebSearch` themselves live in the web plugin, which can
//! be switched off. What stays here is what the rest of the tree names them
//! by and talks to them through: the two canonical tool names, and the
//! delegate a provider installs on a [`ToolContext`](crate::ToolContext) to
//! preempt the local search path. The run loop builds a delegate for the
//! Codex OAuth route and never sees the tool, so the trait cannot live with
//! the tool without pointing the run loop at a plugin.
//!
//! The arbitrated route seat, [`WebProviderRouter`](crate::WebProviderRouter),
//! is next door in `lib.rs` for the same reason.

use async_trait::async_trait;
use rebon_tools_core::ToolResult;
use serde_json::Value;
use std::sync::Arc;

pub const WEB_SEARCH_TOOL_NAME: &str = "WebSearch";
pub const WEB_FETCH_TOOL_NAME: &str = "WebFetch";

/// A provider-side web search that runs instead of the built-in one.
///
/// Best-effort by contract: a delegate that fails lets `WebSearch` fall
/// back to the local engines, unlike a
/// [`WebProviderRouter`](crate::WebProviderRouter) route, which must fail
/// loudly.
#[async_trait]
pub trait WebSearchDelegate: Send + Sync {
    async fn web_search(&self, input: Value) -> ToolResult<Value>;
}

/// Web routing state that [`ToolContext`](crate::ToolContext) carries in its
/// extension bag.
///
/// Storage only — read and written through the unchanged
/// `web_search_delegate()` / `web_provider_router()` accessors.
#[derive(Clone, Default)]
pub struct WebContext {
    pub search_delegate: Option<Arc<dyn WebSearchDelegate>>,
    pub provider_router: Option<Arc<dyn crate::WebProviderRouter>>,
}
