//! A [`ProfileSession`] with no session behind it.
//!
//! Its existence is half the point of the trait: every rule in this crate can
//! be pinned without booting a terminal, which is also what a second front end
//! will need in order to implement it.

use std::sync::Mutex;

use rebon_permissions::PermissionMode;
use rebon_tool::ToolFilter;

use crate::apply::ProfileSession;

pub struct FakeSession {
    pub model: String,
    pub agent: String,
    pub known_agents: Vec<String>,
    pub mode: PermissionMode,
    pub filter: Mutex<ToolFilter>,
    pub default_filter: ToolFilter,
    /// Agents `switch_agent` refuses, and the reason.
    pub refuse_switch: Option<String>,
}

impl Default for FakeSession {
    fn default() -> Self {
        Self {
            model: "vendor-pro".into(),
            agent: "local".into(),
            known_agents: vec!["local".into()],
            mode: PermissionMode::Default,
            filter: Mutex::new(ToolFilter::unrestricted()),
            default_filter: ToolFilter::unrestricted(),
            refuse_switch: None,
        }
    }
}

impl FakeSession {
    pub fn with_filter(self, filter: ToolFilter) -> Self {
        *self.filter.lock().unwrap() = filter;
        self
    }

    pub fn with_model(mut self, model: &str) -> Self {
        self.model = model.into();
        self
    }
}

impl ProfileSession for FakeSession {
    fn model_name(&self) -> String {
        self.model.clone()
    }

    fn agent_id(&self) -> String {
        self.agent.clone()
    }

    fn known_agent_ids(&self) -> Vec<String> {
        self.known_agents.clone()
    }

    fn switch_agent(&self, id: &str) -> Result<String, String> {
        match &self.refuse_switch {
            Some(reason) => Err(reason.clone()),
            None => Ok(id.to_string()),
        }
    }

    fn permission_mode(&self) -> PermissionMode {
        self.mode
    }

    fn set_permission_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    fn tool_filter(&self) -> ToolFilter {
        self.filter.lock().unwrap().clone()
    }

    fn set_tool_filter(&self, filter: ToolFilter) {
        *self.filter.lock().unwrap() = filter;
    }

    fn default_tool_filter(&self) -> ToolFilter {
        self.default_filter.clone()
    }
}
