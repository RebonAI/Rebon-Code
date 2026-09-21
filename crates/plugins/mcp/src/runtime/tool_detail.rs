//! Tool detail view — projection from a single tool into a detailed
//! display block.
//!
//! Implements a fixed display layout.
//!
//! The block shows:
//! * Header: `{server} :: {tool}`
//! * Description paragraph (from the tool's description)
//! * Annotations list (read-only / destructive / open-world)
//! * Input schema preview (properties table or "(none)")
//!
//! The Rust module builds a pure data shape — the consumer renders.

use crate::runtime::tool_list::{DescriptionColor, ToolAnnotationKind};

/// A single annotation with its label and color. Small wrapper to
/// keep the tool_detail API self-contained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolAnnotation {
    pub kind: ToolAnnotationKind,
    pub label: &'static str,
    pub color: Option<DescriptionColor>,
}

/// A single input-schema property row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSchemaProperty {
    pub name: String,
    pub type_label: String,
    pub description: Option<String>,
    pub required: bool,
}

/// The full tool detail block. Pure data — consumer renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDetail {
    /// Header line: `{server} :: {tool}`.
    pub header: String,
    /// Display name (post-prefix-strip).
    pub display_name: String,
    /// The server the tool belongs to.
    pub server_name: String,
    /// The tool's description from the MCP metadata. Empty when the
    /// tool didn't advertise one.
    pub description: String,
    /// The annotation list in the same order as [`ToolAnnotationKind`]'s
    /// push order.
    pub annotations: Vec<ToolAnnotation>,
    /// The input-schema preview rows. Empty means the tool has no
    /// declared inputs.
    pub schema_properties: Vec<ToolSchemaProperty>,
}

/// The shape of the tool info the detail view needs. The consumer
/// (which has the full `Tool` value) pre-builds this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDetailInput {
    pub display_name: String,
    pub server_name: String,
    pub description: String,
    pub is_read_only: bool,
    pub is_destructive: bool,
    pub is_open_world: bool,
    pub schema_properties: Vec<ToolSchemaProperty>,
}

/// Build a [`ToolAnnotation`] from a [`ToolAnnotationKind`]. The
/// color rule pins destructive → Error, read-only → Success, other → None.
pub fn build_annotation(kind: ToolAnnotationKind) -> ToolAnnotation {
    let color = match kind {
        ToolAnnotationKind::Destructive => Some(DescriptionColor::Error),
        ToolAnnotationKind::ReadOnly => Some(DescriptionColor::Success),
        ToolAnnotationKind::OpenWorld => None,
    };
    ToolAnnotation {
        kind,
        label: kind.label(),
        color,
    }
}

/// Build the full tool detail block from the input.
pub fn build_tool_detail(input: &ToolDetailInput) -> ToolDetail {
    let mut annotations = Vec::new();
    if input.is_read_only {
        annotations.push(build_annotation(ToolAnnotationKind::ReadOnly));
    }
    if input.is_destructive {
        annotations.push(build_annotation(ToolAnnotationKind::Destructive));
    }
    if input.is_open_world {
        annotations.push(build_annotation(ToolAnnotationKind::OpenWorld));
    }

    ToolDetail {
        header: format!("{} :: {}", input.server_name, input.display_name),
        display_name: input.display_name.clone(),
        server_name: input.server_name.clone(),
        description: input.description.clone(),
        annotations,
        schema_properties: input.schema_properties.clone(),
    }
}

/// Format a property row in the pinned form:
/// `{name}{*?} : {type}`.
///
/// The `*` suffix is appended when the property is required. Pinned
/// so a refactor can't silently switch it to a prefix or a different
/// sigil.
pub fn format_property_line(prop: &ToolSchemaProperty) -> String {
    let required_marker = if prop.required { "*" } else { "" };
    format!(
        "{name}{marker} : {ty}",
        name = prop.name,
        marker = required_marker,
        ty = prop.type_label
    )
}

/// Format the full schema block. Returns `"(none)"` for an empty
/// schema.
pub fn format_schema_block(properties: &[ToolSchemaProperty]) -> String {
    if properties.is_empty() {
        return "(none)".to_string();
    }
    properties
        .iter()
        .map(format_property_line)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_input(ro: bool, dest: bool, open: bool) -> ToolDetailInput {
        ToolDetailInput {
            display_name: "search".to_string(),
            server_name: "linear".to_string(),
            description: "Search issues".to_string(),
            is_read_only: ro,
            is_destructive: dest,
            is_open_world: open,
            schema_properties: vec![],
        }
    }

    #[test]
    fn header_uses_double_colon_separator() {
        let detail = build_tool_detail(&mk_input(false, false, false));
        assert_eq!(detail.header, "linear :: search");
    }

    #[test]
    fn empty_annotations_list() {
        let detail = build_tool_detail(&mk_input(false, false, false));
        assert!(detail.annotations.is_empty());
    }

    #[test]
    fn read_only_annotation_is_success_color() {
        let detail = build_tool_detail(&mk_input(true, false, false));
        assert_eq!(detail.annotations.len(), 1);
        assert_eq!(detail.annotations[0].kind, ToolAnnotationKind::ReadOnly);
        assert_eq!(detail.annotations[0].color, Some(DescriptionColor::Success));
    }

    #[test]
    fn destructive_annotation_is_error_color() {
        let detail = build_tool_detail(&mk_input(false, true, false));
        assert_eq!(detail.annotations.len(), 1);
        assert_eq!(detail.annotations[0].color, Some(DescriptionColor::Error));
    }

    #[test]
    fn open_world_has_no_color() {
        let detail = build_tool_detail(&mk_input(false, false, true));
        assert_eq!(detail.annotations[0].color, None);
    }

    #[test]
    fn annotation_order_readonly_destructive_openworld() {
        let detail = build_tool_detail(&mk_input(true, true, true));
        assert_eq!(detail.annotations.len(), 3);
        assert_eq!(detail.annotations[0].kind, ToolAnnotationKind::ReadOnly);
        assert_eq!(detail.annotations[1].kind, ToolAnnotationKind::Destructive);
        assert_eq!(detail.annotations[2].kind, ToolAnnotationKind::OpenWorld);
    }

    #[test]
    fn description_passes_through() {
        let mut input = mk_input(false, false, false);
        input.description = "Line1\nLine2".to_string();
        let detail = build_tool_detail(&input);
        assert_eq!(detail.description, "Line1\nLine2");
    }

    #[test]
    fn schema_properties_pass_through() {
        let mut input = mk_input(false, false, false);
        input.schema_properties = vec![
            ToolSchemaProperty {
                name: "query".into(),
                type_label: "string".into(),
                description: Some("The search query".into()),
                required: true,
            },
            ToolSchemaProperty {
                name: "limit".into(),
                type_label: "number".into(),
                description: None,
                required: false,
            },
        ];
        let detail = build_tool_detail(&input);
        assert_eq!(detail.schema_properties.len(), 2);
    }

    #[test]
    fn format_property_line_required() {
        let p = ToolSchemaProperty {
            name: "query".into(),
            type_label: "string".into(),
            description: None,
            required: true,
        };
        assert_eq!(format_property_line(&p), "query* : string");
    }

    #[test]
    fn format_property_line_optional() {
        let p = ToolSchemaProperty {
            name: "limit".into(),
            type_label: "number".into(),
            description: None,
            required: false,
        };
        assert_eq!(format_property_line(&p), "limit : number");
    }

    #[test]
    fn format_schema_block_empty_is_none_placeholder() {
        assert_eq!(format_schema_block(&[]), "(none)");
    }

    #[test]
    fn format_schema_block_joins_with_newline() {
        let props = vec![
            ToolSchemaProperty {
                name: "a".into(),
                type_label: "string".into(),
                description: None,
                required: true,
            },
            ToolSchemaProperty {
                name: "b".into(),
                type_label: "number".into(),
                description: None,
                required: false,
            },
        ];
        assert_eq!(format_schema_block(&props), "a* : string\nb : number");
    }

    #[test]
    fn build_annotation_for_each_kind() {
        for kind in [
            ToolAnnotationKind::ReadOnly,
            ToolAnnotationKind::Destructive,
            ToolAnnotationKind::OpenWorld,
        ] {
            let a = build_annotation(kind);
            assert_eq!(a.kind, kind);
            assert_eq!(a.label, kind.label());
        }
    }

    #[test]
    fn server_name_preserved_in_detail() {
        let detail = build_tool_detail(&mk_input(false, false, false));
        assert_eq!(detail.server_name, "linear");
    }

    #[test]
    fn display_name_preserved_in_detail() {
        let detail = build_tool_detail(&mk_input(false, false, false));
        assert_eq!(detail.display_name, "search");
    }
}
