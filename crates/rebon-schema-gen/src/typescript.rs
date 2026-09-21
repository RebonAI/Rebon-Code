//! JSON Schema → TypeScript.
//!
//! Only the shapes `schemars` produces for rebon's wire types are handled, and
//! anything else is a panic rather than a guess: a silently wrong type is the
//! failure mode this whole crate exists to remove. Growing the wire with a
//! shape that lands here means teaching this file about it.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::REGENERATE_COMMAND;

/// Every definition, rendered as one module.
pub fn render(defs: &BTreeMap<String, Value>) -> String {
    let defs = fold_tags_into_variants(defs);
    let mut out = String::new();
    out.push_str(&header());
    for (name, schema) in &defs {
        out.push('\n');
        out.push_str(&render_definition(name, schema));
    }
    // One pass at the end rather than a rule in every branch: a union that
    // breaks onto its own lines leaves `= ` at the end of the first one, and
    // trailing whitespace is noise in a file that is diffed byte for byte.
    let mut trimmed = String::with_capacity(out.len());
    for line in out.lines() {
        trimmed.push_str(line.trim_end());
        trimmed.push('\n');
    }
    trimmed
}

/// Move an internally tagged enum's discriminator onto the variant type
/// itself, so `DiffContent` carries `type: "diff"` and the union is a plain
/// `DiffContent | TerminalContent | RegularContent`.
///
/// JSON Schema puts the tag on the *branch* — a `$ref` with a sibling `const`
/// property — which renders as `(DiffContent & { type: "diff" }) | …`. That is
/// the same type, but the page reads a variant on its own (`content.oldText`
/// after checking `content.type === "diff"`), and a `DiffContent` without the
/// tag makes that a type error. Folding costs nothing while a variant struct
/// belongs to one union with one tag value, which this checks.
fn fold_tags_into_variants(defs: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    let mut tags: BTreeMap<String, (String, Value)> = BTreeMap::new();
    for (union_name, schema) in defs {
        for branch in tagged_branches(schema) {
            let (variant, field, constant) = branch;
            if let Some((existing, previous)) = tags.get(&variant) {
                assert!(
                    existing == &field && previous == &constant,
                    "{variant} is a variant of more than one tagged union \
                     (in {union_name}), with different tags — fold it by hand"
                );
                continue;
            }
            tags.insert(variant, (field, constant));
        }
    }

    let mut folded = BTreeMap::new();
    for (name, schema) in defs {
        let mut schema = schema.clone();
        if let Some((field, constant)) = tags.get(name) {
            let object = schema
                .as_object_mut()
                .expect("a tagged variant is an object schema");
            object
                .entry("properties")
                .or_insert_with(|| Value::Object(Default::default()))
                .as_object_mut()
                .expect("properties is an object")
                .insert(field.clone(), serde_json::json!({ "const": constant }));
            let required = object
                .entry("required")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .expect("required is an array");
            required.insert(0, Value::String(field.clone()));
        }
        // The union itself keeps only the bare refs; the tag now lives on the
        // variant each one points at.
        if !tagged_branches(&schema).is_empty() {
            let branches: Vec<Value> = schema["oneOf"]
                .as_array()
                .expect("checked by tagged_branches")
                .iter()
                .map(|branch| serde_json::json!({ "$ref": branch["$ref"].clone() }))
                .collect();
            schema["oneOf"] = Value::Array(branches);
        }
        folded.insert(name.clone(), schema);
    }
    folded
}

/// `(variant type, tag field, tag value)` for each branch, when the schema is
/// a `oneOf` whose every branch is a `$ref` carrying exactly one `const`
/// property. Anything else yields nothing and is rendered as it stands.
fn tagged_branches(schema: &Value) -> Vec<(String, String, Value)> {
    let Some(branches) = schema.get("oneOf").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for branch in branches {
        let (Some(reference), Some(properties)) = (
            branch.get("$ref").and_then(Value::as_str),
            branch.get("properties").and_then(Value::as_object),
        ) else {
            return Vec::new();
        };
        let Some(name) = reference.strip_prefix("#/$defs/") else {
            return Vec::new();
        };
        if properties.len() != 1 {
            return Vec::new();
        }
        let (field, constraint) = properties.iter().next().expect("one property");
        let Some(constant) = constraint.get("const") else {
            return Vec::new();
        };
        found.push((name.to_string(), field.clone(), constant.clone()));
    }
    found
}

fn header() -> String {
    format!(
        "\
/* Generated by `{REGENERATE_COMMAND}` from the Rust definitions in
 * `crates/rebon-proto` and `crates/rebon-types`. Do not edit by hand — a
 * change made here is overwritten by the next run, and the staleness test
 * `generated_files_are_current` fails until the two agree.
 *
 * An optional field reads `field?: T` with no `| null`, because the `Option`
 * behind it carries `skip_serializing_if = \"Option::is_none\"` and the server
 * leaves the key out rather than sending null. A field that really does send
 * null says so — `oldText: string | null`. The test
 * `optional_fields_are_omitted_not_nulled` keeps the two apart. */

/** Any JSON, as `serde_json::Value` reaches the wire. */
export type JsonValue = null | boolean | number | string | JsonValue[] | {{ [key: string]: JsonValue }};
"
    )
}

fn render_definition(name: &str, schema: &Value) -> String {
    let description = doc_comment(schema, "");
    if let Some(fields) = object_fields(schema) {
        let mut out = description;
        out.push_str(&format!("export interface {name} {{\n"));
        out.push_str(&fields);
        out.push_str("}\n");
        return out;
    }
    format!("{description}export type {name} = {};\n", type_expr(schema))
}

/// The body of an `interface`, or `None` when the schema is not a plain object
/// with named properties (a union, a string enum, a map).
fn object_fields(schema: &Value) -> Option<String> {
    let object = schema.as_object()?;
    if object.get("type")? != "object" {
        return None;
    }
    let properties = object.get("properties")?.as_object()?;
    let required: Vec<&str> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut out = String::new();
    for (field, field_schema) in properties {
        out.push_str(&doc_comment(field_schema, "  "));
        let optional = if required.contains(&field.as_str()) {
            ""
        } else {
            "?"
        };
        let rendered = if optional.is_empty() {
            type_expr(field_schema)
        } else {
            // See the header: an absent key is `undefined`, not `null`.
            strip_null(&type_expr(field_schema))
        };
        out.push_str(&format!("  {}{optional}: {rendered};\n", quote_key(field)));
    }
    // A `#[serde(flatten)]`ed map, or `HashMap<String, Value>` as the whole
    // body: both reach here as `additionalProperties` beside the named ones.
    if let Some(extra) = object.get("additionalProperties") {
        if extra != &Value::Bool(false) {
            // `| undefined` because TypeScript requires every named property
            // to be assignable to the index type, and the optional ones are
            // `T | undefined`.
            out.push_str(&format!(
                "  [key: string]: {} | undefined;\n",
                type_expr(extra)
            ));
        }
    }
    Some(out)
}

/// A TypeScript type expression for one schema node.
fn type_expr(schema: &Value) -> String {
    match schema {
        // `serde_json::Value` and `HashMap<String, Value>`'s values.
        Value::Bool(true) => return "JsonValue".to_string(),
        Value::Bool(false) => return "never".to_string(),
        _ => {}
    }
    let object = schema
        .as_object()
        .unwrap_or_else(|| panic!("schema node is neither a boolean nor an object: {schema}"));

    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let name = reference
            .strip_prefix("#/$defs/")
            .unwrap_or_else(|| panic!("unsupported $ref target {reference}"))
            .to_string();
        // A `$ref` with sibling constraints is how JSON Schema 2020-12 — and
        // so `schemars` — writes an internally tagged enum variant: the
        // referenced struct *and* the `type` discriminator the tag adds. Drop
        // the siblings and the union stops discriminating, which is exactly
        // the narrowing the page relies on.
        if object.contains_key("properties") {
            let mut siblings = object.clone();
            siblings.remove("$ref");
            siblings.remove("description");
            let fields = object_fields(&Value::Object(siblings))
                .expect("a $ref sibling carrying `properties` is an object schema");
            let body = fields.lines().map(str::trim).collect::<Vec<_>>().join(" ");
            // Parenthesised so the shape survives being read: `&` does bind
            // tighter than `|`, but a union of bare intersections is a line
            // nobody should have to re-derive precedence for.
            return format!("({name} & {{ {body} }})");
        }
        return name;
    }
    if let Some(constant) = object.get("const") {
        return literal(constant);
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return union(values.iter().map(literal));
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(branches) = object.get(key).and_then(Value::as_array) {
            // `allOf` with one branch is how schemars carries a `$ref` that
            // also has a description; anything longer would need an
            // intersection, which these types never produce.
            if key == "allOf" && branches.len() != 1 {
                panic!("allOf with {} branches is not supported", branches.len());
            }
            return union(branches.iter().map(type_expr));
        }
    }
    match object.get("type") {
        Some(Value::Array(names)) => union(names.iter().map(|name| {
            primitive(
                name.as_str()
                    .unwrap_or_else(|| panic!("type entry is not a string: {name}")),
                object,
            )
        })),
        Some(Value::String(name)) => primitive(name, object),
        // No `type` and no combinator: an unconstrained schema, i.e. any JSON.
        None => "JsonValue".to_string(),
        Some(other) => panic!("unsupported `type` value {other}"),
    }
}

fn primitive(name: &str, object: &serde_json::Map<String, Value>) -> String {
    match name {
        "null" => "null".to_string(),
        "boolean" => "boolean".to_string(),
        "integer" | "number" => "number".to_string(),
        "string" => "string".to_string(),
        "array" => {
            let items = object
                .get("items")
                .map(type_expr)
                .unwrap_or_else(|| "JsonValue".to_string());
            if items.contains(' ') {
                format!("Array<{items}>")
            } else {
                format!("{items}[]")
            }
        }
        "object" => {
            if let Some(fields) = object_fields(&Value::Object(object.clone())) {
                // An inline object — a struct variant of an enum, say.
                let body = fields
                    .lines()
                    .map(|line| format!("  {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                return format!("{{\n{body}\n  }}");
            }
            let values = object
                .get("additionalProperties")
                .map(type_expr)
                .unwrap_or_else(|| "JsonValue".to_string());
            format!("Record<string, {values}>")
        }
        other => panic!("unsupported primitive type {other}"),
    }
}

fn literal(value: &Value) -> String {
    match value {
        Value::String(text) => format!("{:?}", text),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn union(parts: impl Iterator<Item = String>) -> String {
    let mut seen: Vec<String> = Vec::new();
    for part in parts {
        if !seen.contains(&part) {
            seen.push(part);
        }
    }
    let inline = seen.join(" | ");
    // One branch per line once the union stops fitting on one, so a wire type
    // that grows a variant shows up as one added line in the diff.
    if seen.len() > 1 && (inline.len() > 96 || inline.contains('\n')) {
        return format!("\n  | {}", seen.join("\n  | "));
    }
    inline
}

/// Drop a `| null` branch from an optional field's type — see the header.
fn strip_null(expr: &str) -> String {
    let parts: Vec<&str> = expr.split(" | ").filter(|part| *part != "null").collect();
    if parts.is_empty() {
        return "null".to_string();
    }
    parts.join(" | ")
}

/// A property name, quoted only when it is not a plain identifier — `_meta`
/// is fine bare, `content-type` would not be.
fn quote_key(key: &str) -> String {
    let plain = !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if plain {
        key.to_string()
    } else {
        format!("{key:?}")
    }
}

/// The Rust doc comment, carried through as a JSDoc block so the page shows
/// the same explanation on hover that the Rust reader sees.
fn doc_comment(schema: &Value, indent: &str) -> String {
    let Some(description) = schema.get("description").and_then(Value::as_str) else {
        return String::new();
    };
    let lines: Vec<&str> = description.lines().collect();
    if lines.len() == 1 {
        return format!("{indent}/** {} */\n", lines[0]);
    }
    let mut out = format!("{indent}/**\n");
    for line in lines {
        if line.is_empty() {
            out.push_str(&format!("{indent} *\n"));
        } else {
            out.push_str(&format!("{indent} * {line}\n"));
        }
    }
    out.push_str(&format!("{indent} */\n"));
    out
}
