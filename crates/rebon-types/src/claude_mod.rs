//! What a Claude Code mod declares, and what it draws.
//!
//! A *mod* is a Claude Code plugin of function hooks: a folder holding
//! `.claude-plugin/plugin.json` and a `hooks/hooks.json` that names one hooks
//! module, which exports `register(on, options)`. Rebon runs one on its Node
//! plugin plane through the `mods-runtime` loader. These are the values both
//! sides of that read: the manifest as the folder writes it, the scan of the
//! hooks module that stands in for a `rebon-plugin.json` ceiling, and the
//! tree a `ui.render` hook answers, which every surface validates before it
//! draws.
//!
//! They live here rather than beside one reader because there are several:
//! `rebon-plugin-package` scans a folder, `rebon-plugin-host` loads it and
//! answers its `$`, and the terminal and the desktop app draw its trees.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The manifest inside a mod's folder, relative to it.
pub const CLAUDE_PLUGIN_MANIFEST: &str = ".claude-plugin/plugin.json";
/// The hooks file naming the hooks module, relative to the folder.
pub const CLAUDE_HOOKS_FILE: &str = "hooks/hooks.json";
/// The kernel seat a mod's `$` is answered by.
pub const MODS_SEAT: &str = "mods";
/// The one service every mod registers on the plane.
pub const MOD_SERVICE: &str = "mod";
/// The key rebon puts on a mod's load-request `config` so the loader knows
/// the entry is a mod without reading its module.
pub const MOD_MARKER: &str = "$claudeMod";
/// The folder under the config home whose children load as mods.
pub const MODS_DIR: &str = "mods";
/// The environment variable listing more mod folders, path-separator joined.
pub const MOD_DIRS_ENV: &str = "REBON_MOD_DIRS";
/// Claude Code's own spelling of the same list, honoured as well.
pub const CLAUDE_PLUGIN_DIRS_ENV: &str = "CLAUDE_CODE_PLUGIN_DIRS";

/// `.claude-plugin/plugin.json`, the fields rebon reads.
///
/// Lenient on purpose: a plugin folder may also carry commands, agents,
/// skills and MCP servers the mods layer does not run; those keys are kept
/// in `rest` so a listing can say they were there.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeModManifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// The options `register(on, options)` receives, by field name.
    #[serde(default)]
    pub user_config: BTreeMap<String, ClaudeModUserConfigField>,
    /// The contract a mod ships for the noun it adds to `$`, if any.
    #[serde(default)]
    pub types: Option<String>,
    /// Plugins whose contracts this one's module is typed against.
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(flatten)]
    pub rest: BTreeMap<String, Value>,
}

/// One `userConfig` field: what the config menu shows and `options` carries.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeModUserConfigField {
    /// `string`, `boolean` or `number`.
    #[serde(default, rename = "type")]
    pub ty: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default: Option<Value>,
    /// For a `string` field, the values a picker offers; a stored value
    /// outside them counts as unset.
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub required: bool,
}

/// `hooks/hooks.json`: the one hooks module, beside any command hooks the
/// file also declares (which rebon's own hook loader reads separately).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ClaudeModHooksFile {
    #[serde(default)]
    pub modules: Vec<String>,
    #[serde(flatten)]
    pub rest: BTreeMap<String, Value>,
}

/// What a hooks module registers and calls, as read off its source.
///
/// The same lists `claude plugin validate` prints. Rebon reads them for the
/// ceiling a mod loads against: a command or tool name has to be spelled as
/// a literal in a `$.command.register({ name: "..." })` or
/// `$.tool.register({ name: "..." })` call to be registered at all.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModScan {
    /// The patterns its `on(...)` registrations name, in order, each once.
    pub events: Vec<String>,
    /// What it calls on `$`, spelled `noun.method`, sorted, each once.
    pub calls: Vec<String>,
    pub commands: Vec<ModCommandDecl>,
    pub tools: Vec<ModToolDecl>,
    pub env_reads: Vec<String>,
    pub env_writes: Vec<String>,
    /// What the scanner could not read as it should be written.
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModCommandDecl {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub argument_hint: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModToolDecl {
    /// The name as the model sees it: `mcp__<plugin>__<name>`.
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

impl ModScan {
    /// Whether the module hooks `event` by name, by glob or by `*`.
    pub fn hooks_event(&self, event: &str) -> bool {
        self.events
            .iter()
            .any(|pattern| pattern_selects(pattern, event))
    }

    /// Whether the module calls `noun.method` on `$`.
    pub fn calls(&self, call: &str) -> bool {
        self.calls.iter().any(|c| c == call)
    }
}

/// The selection rule the Node chain applies, written once more here so a
/// subscriber can tell whether a mod wants an event before paying a call.
pub fn pattern_selects(pattern: &str, event: &str) -> bool {
    if pattern == "*" {
        return !event.starts_with("telemetry.");
    }
    if let Some(excluded) = pattern.strip_prefix('!') {
        if event.starts_with("telemetry.") {
            return false;
        }
        return !pattern_selects(excluded, event);
    }
    if let Some(namespace) = pattern.strip_suffix(".*") {
        return event
            .strip_prefix(namespace)
            .is_some_and(|rest| rest.starts_with('.'));
    }
    pattern == event
}

/// The surfaces a render hook is asked for, and the elements each draws.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ModUiSurface {
    Terminal,
    Desktop,
    Mobile,
    Vscode,
}

impl ModUiSurface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Desktop => "desktop",
            Self::Mobile => "mobile",
            Self::Vscode => "vscode",
        }
    }

    /// The element types this surface draws; the `Elements` table of the
    /// Claude Code declarations, minus the ones no rebon surface paints
    /// (`Raster`, `Image`). A `Client` arrives drawn: the mod's host runs
    /// its surface module and hands the tree it drew as the Client's child.
    pub fn elements(self) -> &'static [&'static str] {
        match self {
            Self::Terminal => &[
                "Box", "Text", "Button", "Input", "Select", "Link", "Code", "Markdown", "Client",
            ],
            Self::Desktop => &[
                "Box", "Text", "Button", "Input", "Select", "Svg", "Link", "Code", "Markdown",
                "Client",
            ],
            Self::Vscode => &[
                "Box", "Text", "Button", "Input", "Select", "Svg", "Link", "Code", "Markdown",
            ],
            Self::Mobile => &["Box", "Text", "Button", "Svg", "Link", "Code", "Markdown"],
        }
    }
}

/// One drawn element: its type, its plain props, its children.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ModUiNode {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub props: serde_json::Map<String, Value>,
    #[serde(default)]
    pub children: Vec<ModUiChild>,
}

/// A child: text, or another element.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ModUiChild {
    Text(String),
    Node(ModUiNode),
}

impl ModUiNode {
    /// The `key` prop, which a press, an input and a select name.
    pub fn key(&self) -> Option<&str> {
        self.props.get("key").and_then(Value::as_str)
    }

    pub fn prop_str(&self, name: &str) -> Option<&str> {
        self.props.get(name).and_then(Value::as_str)
    }

    pub fn prop_bool(&self, name: &str) -> bool {
        self.props
            .get(name)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Every text child joined, for an element whose children are its label.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for child in &self.children {
            match child {
                ModUiChild::Text(text) => out.push_str(text),
                ModUiChild::Node(node) => out.push_str(&node.text()),
            }
        }
        out
    }

    /// The elements of `ty` anywhere beneath this one, this one included.
    pub fn find_all<'a>(&'a self, ty: &str, out: &mut Vec<&'a ModUiNode>) {
        if self.ty == ty {
            out.push(self);
        }
        for child in &self.children {
            if let ModUiChild::Node(node) = child {
                node.find_all(ty, out);
            }
        }
    }
}

/// How deep a tree may nest and how many elements it may hold. A hook that
/// answers more than this is drawing something no surface can lay out.
pub const MAX_UI_DEPTH: usize = 64;
pub const MAX_UI_NODES: usize = 4_096;

/// Why a tree was refused, with the path to the element at fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModUiRefusal {
    pub path: String,
    pub reason: String,
}

impl std::fmt::Display for ModUiRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

/// Reads a tree as a surface accepts it, or says what it refuses.
///
/// The rules are the ones the Claude Code engine states for an invalid
/// tree: an element the surface lacks, a prop of the wrong kind, a child
/// where none goes. A refused tree is not drawn; the surface draws its own.
pub fn validate_ui_tree(value: &Value, surface: ModUiSurface) -> Result<ModUiNode, ModUiRefusal> {
    let mut count = 0usize;
    validate_node(value, surface, "root", 0, &mut count, false)
}

fn validate_node(
    value: &Value,
    surface: ModUiSurface,
    path: &str,
    depth: usize,
    count: &mut usize,
    in_client: bool,
) -> Result<ModUiNode, ModUiRefusal> {
    let refuse = |reason: String| ModUiRefusal {
        path: path.to_owned(),
        reason,
    };
    if depth > MAX_UI_DEPTH {
        return Err(refuse(format!("the tree nests deeper than {MAX_UI_DEPTH}")));
    }
    *count += 1;
    if *count > MAX_UI_NODES {
        return Err(refuse(format!(
            "the tree holds more than {MAX_UI_NODES} elements"
        )));
    }
    let Some(object) = value.as_object() else {
        return Err(refuse("an element is an object with a type".to_owned()));
    };
    let Some(ty) = object.get("type").and_then(Value::as_str) else {
        return Err(refuse("an element names its type".to_owned()));
    };
    if in_client && ty == "Client" {
        return Err(refuse(
            "a Client's surface module draws no Client of its own".to_owned(),
        ));
    }
    if !surface.elements().contains(&ty) {
        return Err(refuse(format!(
            "{ty} is not an element the {} surface draws (its elements are {})",
            surface.as_str(),
            surface.elements().join(", ")
        )));
    }
    let props = match object.get("props") {
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(Value::Object(props)) => props.clone(),
        Some(_) => return Err(refuse(format!("{ty}'s props are not an object"))),
    };
    if let Some(key) = props.get("key") {
        if !key.is_string() && !key.is_number() {
            return Err(refuse(format!(
                "{ty}'s key is neither a string nor a number"
            )));
        }
    }
    validate_props(ty, &props).map_err(refuse)?;
    let mut children = Vec::new();
    let raw_children = match object.get("children") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(list)) => list.as_slice(),
        Some(_) => return Err(refuse(format!("{ty}'s children are not a list"))),
    };
    for (index, child) in raw_children.iter().enumerate() {
        let child_path = format!("{path}/{ty}[{index}]");
        match child {
            Value::String(text) => {
                if ty == "Box" {
                    return Err(ModUiRefusal {
                        path: child_path,
                        reason: "text goes inside a Text, not directly in a Box".to_owned(),
                    });
                }
                children.push(ModUiChild::Text(text.clone()));
            }
            Value::Number(number) => children.push(ModUiChild::Text(number.to_string())),
            Value::Object(_) => {
                let node = validate_node(
                    child,
                    surface,
                    &child_path,
                    depth + 1,
                    count,
                    in_client || ty == "Client",
                )?;
                if matches!(ty, "Text" | "Button" | "Link") && node.ty != "Text" {
                    return Err(ModUiRefusal {
                        path: child_path,
                        reason: format!("a {ty} holds text and Text elements, not a {}", node.ty),
                    });
                }
                children.push(ModUiChild::Node(node));
            }
            Value::Bool(_) | Value::Null => {}
            Value::Array(_) => {
                return Err(ModUiRefusal {
                    path: child_path,
                    reason: "a nested list is not a child".to_owned(),
                })
            }
        }
    }
    Ok(ModUiNode {
        ty: ty.to_owned(),
        props,
        children,
    })
}

/// The props each element needs, and the kinds the ones it takes must be.
fn validate_props(ty: &str, props: &serde_json::Map<String, Value>) -> Result<(), String> {
    let string_prop = |name: &str| -> Result<(), String> {
        match props.get(name) {
            None | Some(Value::Null) | Some(Value::String(_)) => Ok(()),
            Some(_) => Err(format!("{ty}'s {name} is not a string")),
        }
    };
    let required_string = |name: &str| -> Result<(), String> {
        match props.get(name) {
            Some(Value::String(_)) => Ok(()),
            _ => Err(format!("{ty} needs a {name} string")),
        }
    };
    match ty {
        "Box" => {
            string_prop("flexDirection")?;
            string_prop("borderStyle")?;
            string_prop("justifyContent")?;
            string_prop("alignItems")?;
            for numeric in [
                "gap", "padding", "paddingX", "paddingY", "margin", "width", "height", "flexGrow",
            ] {
                if let Some(value) = props.get(numeric) {
                    if !value.is_number() && !value.is_string() && !value.is_null() {
                        return Err(format!("Box's {numeric} is neither a number nor a string"));
                    }
                }
            }
            Ok(())
        }
        "Text" => {
            string_prop("color")?;
            string_prop("backgroundColor")?;
            Ok(())
        }
        "Button" => {
            string_prop("variant")?;
            string_prop("hotkey")?;
            string_prop("role")?;
            string_prop("action")?;
            Ok(())
        }
        "Input" => {
            required_string("key")?;
            string_prop("value")?;
            string_prop("placeholder")?;
            Ok(())
        }
        "Select" => {
            required_string("key")?;
            match props.get("options") {
                Some(Value::Array(_)) => Ok(()),
                _ => Err("Select needs an options list".to_owned()),
            }
        }
        "Link" => {
            if props.get("href").and_then(Value::as_str).is_none()
                && props.get("url").and_then(Value::as_str).is_none()
            {
                return Err("Link needs an href".to_owned());
            }
            Ok(())
        }
        "Code" => {
            required_string("source")?;
            string_prop("language")?;
            string_prop("format")?;
            string_prop("path")?;
            Ok(())
        }
        "Markdown" => {
            if props.get("source").and_then(Value::as_str).is_none()
                && props.get("text").and_then(Value::as_str).is_none()
            {
                return Err("Markdown needs a source string".to_owned());
            }
            Ok(())
        }
        "Svg" => required_string("source"),
        "Client" => {
            required_string("key")?;
            required_string("module")
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_manifest_reads_its_known_fields_and_keeps_the_rest() {
        let manifest: ClaudeModManifest = serde_json::from_value(json!({
            "name": "tally",
            "version": "0.1.0",
            "description": "counts",
            "userConfig": { "prefix": { "type": "string", "default": "n=", "options": ["n=", "#"] } },
            "types": "./types/index.d.ts",
            "commands": "./commands",
        }))
        .unwrap();
        assert_eq!(manifest.name, "tally");
        assert_eq!(manifest.user_config["prefix"].default, Some(json!("n=")));
        assert_eq!(manifest.user_config["prefix"].options, vec!["n=", "#"]);
        assert_eq!(manifest.rest["commands"], json!("./commands"));
    }

    #[test]
    fn a_hooks_file_names_modules_beside_command_hooks() {
        let hooks: ClaudeModHooksFile = serde_json::from_value(json!({
            "modules": ["./register.tsx"],
            "PreToolUse": [{ "hooks": [{ "type": "command", "command": "echo" }] }],
        }))
        .unwrap();
        assert_eq!(hooks.modules, vec!["./register.tsx"]);
        assert!(hooks.rest.contains_key("PreToolUse"));
    }

    #[test]
    fn patterns_select_as_the_chain_does() {
        assert!(pattern_selects("tool.call", "tool.call"));
        assert!(!pattern_selects("tool.call", "tool.check"));
        assert!(pattern_selects("tool.*", "tool.call"));
        assert!(!pattern_selects("tool.*", "tools.call"));
        assert!(pattern_selects("*", "prompt.submit"));
        assert!(!pattern_selects("*", "telemetry.log"));
        assert!(pattern_selects("!tool.describe", "tool.call"));
        assert!(!pattern_selects("!tool.describe", "tool.describe"));
        let scan = ModScan {
            events: vec!["classic.*".into(), "tool.call".into()],
            calls: vec!["ui.status".into()],
            ..Default::default()
        };
        assert!(scan.hooks_event("classic.Stop"));
        assert!(scan.hooks_event("tool.call"));
        assert!(!scan.hooks_event("prompt.submit"));
        assert!(scan.calls("ui.status"));
    }

    fn tree() -> Value {
        json!({
            "type": "Box",
            "props": { "flexDirection": "column", "borderStyle": "round" },
            "children": [
                { "type": "Text", "props": { "bold": true }, "children": ["3 clicks", { "type": "Text", "props": { "dimColor": true }, "children": [" so far"] }] },
                { "type": "Button", "props": { "key": "more", "variant": "primary" }, "children": ["more"] },
                { "type": "Code", "props": { "source": "let x = 1", "language": "js" } },
                { "type": "Markdown", "props": { "source": "# hi" } },
                { "type": "Link", "props": { "href": "https://example.com" }, "children": ["docs"] }
            ]
        })
    }

    #[test]
    fn a_drawn_client_validates_where_clients_are_drawn_and_holds_no_client() {
        let client = json!({ "type": "Client", "props": { "key": "game", "module": "./game.tsx" }, "children": [
            { "type": "Text", "props": {}, "children": ["score 3"] },
        ] });
        let node =
            validate_ui_tree(&client, ModUiSurface::Terminal).expect("drawn on the terminal");
        assert_eq!(node.ty, "Client");
        assert!(validate_ui_tree(&client, ModUiSurface::Desktop).is_ok());
        assert!(validate_ui_tree(&client, ModUiSurface::Mobile).is_err());
        assert!(validate_ui_tree(&client, ModUiSurface::Vscode).is_err());
        let nested = json!({ "type": "Client", "props": { "key": "a", "module": "./a.tsx" }, "children": [
            { "type": "Box", "props": {}, "children": [
                { "type": "Client", "props": { "key": "b", "module": "./b.tsx" }, "children": [] },
            ] },
        ] });
        let refused = validate_ui_tree(&nested, ModUiSurface::Terminal).unwrap_err();
        assert!(
            refused.reason.contains("no Client of its own"),
            "{refused:?}"
        );
        let keyless = json!({ "type": "Client", "props": { "module": "./a.tsx" }, "children": [] });
        assert!(validate_ui_tree(&keyless, ModUiSurface::Terminal).is_err());
    }

    #[test]
    fn a_valid_tree_reads_on_every_surface_that_draws_its_elements() {
        for surface in [
            ModUiSurface::Terminal,
            ModUiSurface::Desktop,
            ModUiSurface::Vscode,
            ModUiSurface::Mobile,
        ] {
            let node = validate_ui_tree(&tree(), surface)
                .unwrap_or_else(|refusal| panic!("{surface:?}: {refusal}"));
            assert_eq!(node.ty, "Box");
            assert_eq!(node.children.len(), 5);
            let mut buttons = Vec::new();
            node.find_all("Button", &mut buttons);
            assert_eq!(buttons[0].key(), Some("more"));
            assert_eq!(buttons[0].text(), "more");
        }
    }

    #[test]
    fn an_element_the_surface_lacks_is_refused_by_name() {
        let svg = json!({ "type": "Svg", "props": { "source": "<svg/>" } });
        assert!(validate_ui_tree(&svg, ModUiSurface::Desktop).is_ok());
        let refusal = validate_ui_tree(&svg, ModUiSurface::Terminal).unwrap_err();
        assert!(
            refusal
                .reason
                .contains("Svg is not an element the terminal surface draws"),
            "{refusal}"
        );
        let input = json!({ "type": "Input", "props": { "key": "q" } });
        assert!(validate_ui_tree(&input, ModUiSurface::Mobile).is_err());
        assert!(validate_ui_tree(&input, ModUiSurface::Terminal).is_ok());
    }

    #[test]
    fn a_child_where_none_goes_and_a_missing_prop_are_refused_with_a_path() {
        let text_in_box = json!({ "type": "Box", "children": ["loose"] });
        let refusal = validate_ui_tree(&text_in_box, ModUiSurface::Terminal).unwrap_err();
        assert_eq!(refusal.path, "root/Box[0]");
        let select = json!({ "type": "Select", "props": { "key": "k" } });
        assert!(validate_ui_tree(&select, ModUiSurface::Terminal)
            .unwrap_err()
            .reason
            .contains("options"));
        let code = json!({ "type": "Code", "props": {} });
        assert!(validate_ui_tree(&code, ModUiSurface::Terminal)
            .unwrap_err()
            .reason
            .contains("source"));
        let box_in_text = json!({ "type": "Text", "children": [{ "type": "Box" }] });
        assert!(validate_ui_tree(&box_in_text, ModUiSurface::Terminal)
            .unwrap_err()
            .reason
            .contains("not a Box"));
    }

    #[test]
    fn a_tree_too_deep_is_refused() {
        let mut value = json!({ "type": "Text", "children": ["leaf"] });
        for _ in 0..(MAX_UI_DEPTH + 2) {
            value = json!({ "type": "Box", "children": [value] });
        }
        let refusal = validate_ui_tree(&value, ModUiSurface::Terminal).unwrap_err();
        assert!(refusal.reason.contains("nests deeper"), "{refusal}");
    }

    #[test]
    fn a_tree_round_trips_through_serde() {
        let node = validate_ui_tree(&tree(), ModUiSurface::Desktop).unwrap();
        let again: ModUiNode =
            serde_json::from_value(serde_json::to_value(&node).unwrap()).unwrap();
        assert_eq!(again, node);
    }
}
