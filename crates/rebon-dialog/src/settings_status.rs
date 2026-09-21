//! Status tab — pure projection of `Property` rows.
//!
//! ## Fixed rules
//!
//! 1. **Primary section order: Version, Session name, Session ID, cwd,
//!    then the account rows, then the api-provider rows.**
//! 2. **Secondary section order: Model, then the IDE, MCP, sandbox and
//!    setting-source rows.**
//! 3. **`Session name` falls back to a `/rename to add a name`
//!    placeholder when the session has no custom title.**
//! 4. **`PropertyValue` rendering rules:**
//!    - `List` → its items joined with `, ` (comma-space), with
//!      no trailing comma, on one line.
//!    - `Text` → as-is.
//!    - `Widget` → the stored string is passed through untouched.

/// A property value — string, list of strings, or pre-formatted
/// "widget" placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropertyValue {
    /// A single line.
    Text(String),
    /// Several values shown on one line, joined with `, `.
    List(Vec<String>),
    /// Pre-rendered widget bytes. Stored as a String so rendering remains the
    /// consumer's responsibility.
    Widget(String),
}

/// The `Property` row shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    /// Optional label. Missing labels are allowed and [`format_property_value`]
    /// skips the label prefix when None.
    pub label: Option<String>,
    /// What the row shows.
    pub value: PropertyValue,
}

impl Property {
    /// A labelled row.
    pub fn new(label: impl Into<String>, value: PropertyValue) -> Self {
        Self {
            label: Some(label.into()),
            value,
        }
    }

    /// A row that shows only its value.
    pub fn unlabeled(value: PropertyValue) -> Self {
        Self { label: None, value }
    }
}

/// Inputs for [`build_primary_section`] and [`build_secondary_section`].
///
/// Every field is filled in by the caller before either builder runs;
/// nothing here reads live session state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusInputs {
    /// The running build's version string.
    pub version: String,
    /// The session's id, shown verbatim.
    pub session_id: String,
    /// The name `/rename` gave the session, if any.
    pub session_custom_title: Option<String>,
    /// The session's working directory.
    pub cwd: String,
    /// Pre-built account properties. This module does not compute them:
    /// they arrive from a separate helper.
    pub account_properties: Vec<Property>,
    /// Pre-built provider rows, same arrangement as the account rows.
    pub api_provider_properties: Vec<Property>,
    /// The model as the user chose it, already resolved to a label.
    pub model_label: String,
    /// Pre-built IDE-connection rows.
    pub ide_properties: Vec<Property>,
    /// Pre-built MCP-server rows.
    pub mcp_properties: Vec<Property>,
    /// Pre-built sandbox rows.
    pub sandbox_properties: Vec<Property>,
    /// Pre-built rows naming the settings files in effect.
    pub setting_sources_properties: Vec<Property>,
}

/// Placeholder text used when the session has no custom title.
pub const SESSION_NAME_PLACEHOLDER: &str = "/rename to add a name";

/// Build the primary section.
pub fn build_primary_section(inputs: &StatusInputs) -> Vec<Property> {
    let session_name_value = match &inputs.session_custom_title {
        Some(title) => PropertyValue::Text(title.clone()),
        None => PropertyValue::Widget(SESSION_NAME_PLACEHOLDER.to_string()),
    };
    let mut out: Vec<Property> = vec![
        Property::new("Version", PropertyValue::Text(inputs.version.clone())),
        Property::new("Session name", session_name_value),
        Property::new("Session ID", PropertyValue::Text(inputs.session_id.clone())),
        Property::new("cwd", PropertyValue::Text(inputs.cwd.clone())),
    ];
    out.extend(inputs.account_properties.clone());
    out.extend(inputs.api_provider_properties.clone());
    out
}

/// Build the secondary section.
pub fn build_secondary_section(inputs: &StatusInputs) -> Vec<Property> {
    let mut out = vec![Property::new(
        "Model",
        PropertyValue::Text(inputs.model_label.clone()),
    )];
    out.extend(inputs.ide_properties.clone());
    out.extend(inputs.mcp_properties.clone());
    out.extend(inputs.sandbox_properties.clone());
    out.extend(inputs.setting_sources_properties.clone());
    out
}

/// Render a [`PropertyValue`] as one display string.
///
/// `Text` and `Widget` are returned unchanged. `List` is joined with
/// `, ` — a comma immediately followed by a space — so the result is a
/// single line with no trailing comma.
pub fn format_property_value(value: &PropertyValue) -> String {
    match value {
        PropertyValue::Text(s) => s.clone(),
        PropertyValue::List(items) => items.join(", "),
        PropertyValue::Widget(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_inputs() -> StatusInputs {
        StatusInputs {
            version: "1.0.0".into(),
            session_id: "abc123".into(),
            session_custom_title: None,
            cwd: "/tmp/project".into(),
            account_properties: vec![],
            api_provider_properties: vec![],
            model_label: "claude-3.5-sonnet".into(),
            ide_properties: vec![],
            mcp_properties: vec![],
            sandbox_properties: vec![],
            setting_sources_properties: vec![],
        }
    }

    // ---- build_primary_section ----

    #[test]
    fn primary_section_order_pinned() {
        let inputs = empty_inputs();
        let p = build_primary_section(&inputs);
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].label.as_deref(), Some("Version"));
        assert_eq!(p[1].label.as_deref(), Some("Session name"));
        assert_eq!(p[2].label.as_deref(), Some("Session ID"));
        assert_eq!(p[3].label.as_deref(), Some("cwd"));
    }

    #[test]
    fn primary_section_session_name_placeholder() {
        let inputs = empty_inputs();
        let p = build_primary_section(&inputs);
        // Session name with no custom title → placeholder Widget
        assert_eq!(
            p[1].value,
            PropertyValue::Widget(SESSION_NAME_PLACEHOLDER.to_string())
        );
    }

    #[test]
    fn primary_section_session_name_custom_title() {
        let mut inputs = empty_inputs();
        inputs.session_custom_title = Some("My session".into());
        let p = build_primary_section(&inputs);
        assert_eq!(p[1].value, PropertyValue::Text("My session".into()));
    }

    #[test]
    fn primary_section_includes_account_properties_after_cwd() {
        let mut inputs = empty_inputs();
        inputs.account_properties = vec![Property::new(
            "Account",
            PropertyValue::Text("alice".into()),
        )];
        let p = build_primary_section(&inputs);
        assert_eq!(p.len(), 5);
        assert_eq!(p[4].label.as_deref(), Some("Account"));
    }

    #[test]
    fn primary_section_account_then_api_provider() {
        let mut inputs = empty_inputs();
        inputs.account_properties = vec![Property::new(
            "Account",
            PropertyValue::Text("alice".into()),
        )];
        inputs.api_provider_properties = vec![Property::new(
            "API Provider",
            PropertyValue::Text("anthropic".into()),
        )];
        let p = build_primary_section(&inputs);
        assert_eq!(p.len(), 6);
        assert_eq!(p[4].label.as_deref(), Some("Account"));
        assert_eq!(p[5].label.as_deref(), Some("API Provider"));
    }

    // ---- build_secondary_section ----

    #[test]
    fn secondary_section_starts_with_model() {
        let inputs = empty_inputs();
        let p = build_secondary_section(&inputs);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].label.as_deref(), Some("Model"));
        assert_eq!(p[0].value, PropertyValue::Text("claude-3.5-sonnet".into()));
    }

    #[test]
    fn secondary_section_full_order() {
        let mut inputs = empty_inputs();
        inputs.ide_properties = vec![Property::new("IDE", PropertyValue::Text("vscode".into()))];
        inputs.mcp_properties = vec![Property::new("MCP", PropertyValue::Text("server-1".into()))];
        inputs.sandbox_properties = vec![Property::new(
            "Sandbox",
            PropertyValue::Text("enabled".into()),
        )];
        inputs.setting_sources_properties = vec![Property::new(
            "Settings",
            PropertyValue::Text("user".into()),
        )];

        let p = build_secondary_section(&inputs);
        assert_eq!(p.len(), 5);
        assert_eq!(p[0].label.as_deref(), Some("Model"));
        assert_eq!(p[1].label.as_deref(), Some("IDE"));
        assert_eq!(p[2].label.as_deref(), Some("MCP"));
        assert_eq!(p[3].label.as_deref(), Some("Sandbox"));
        assert_eq!(p[4].label.as_deref(), Some("Settings"));
    }

    // ---- format_property_value ----

    #[test]
    fn format_property_value_text() {
        let v = PropertyValue::Text("hello".into());
        assert_eq!(format_property_value(&v), "hello");
    }

    #[test]
    fn format_property_value_list_joined_with_comma_space() {
        let v = PropertyValue::List(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(format_property_value(&v), "a, b, c");
    }

    #[test]
    fn format_property_value_empty_list() {
        let v = PropertyValue::List(vec![]);
        assert_eq!(format_property_value(&v), "");
    }

    #[test]
    fn format_property_value_single_list() {
        let v = PropertyValue::List(vec!["only".into()]);
        assert_eq!(format_property_value(&v), "only");
    }

    #[test]
    fn format_property_value_widget_passthrough() {
        let v = PropertyValue::Widget("/rename to add a name".into());
        assert_eq!(format_property_value(&v), "/rename to add a name");
    }

    // ---- diagnostics view ----

    // ---- session name placeholder ----

    #[test]
    fn session_name_placeholder_pinned() {
        assert_eq!(SESSION_NAME_PLACEHOLDER, "/rename to add a name");
    }
}
