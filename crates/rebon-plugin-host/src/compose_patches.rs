//! Layered composition patches — a PURE function from
//! `(base entries, patch layers)` to the final effective composition,
//! with per-entry provenance and collected warnings.
//!
//! Layers are applied in the order they are handed in, and every caller in
//! the tree passes one — `user-patches` (the config's own `patches`). Each
//! layer's patch is a JSON array of items:
//!
//! - `{ "id": "x", "config": {...} }` — locate the entry by id and
//!   REPLACE its config wholesale (no deep merge — dsh's cordis.yml
//!   entries are replaced, not merged, so partial-merge surprises cannot
//!   exist). A missing id warns and skips: a typo never breaks the
//!   running composition.
//! - `{ "insert": { "id": "y", "name": "module", "config": {...} } }` —
//!   append a new entry; a duplicate id warns and skips.
//! - `{ "remove": "x" }` — drop the entry; a missing id warns and skips.
//!
//! A malformed layer (not an array, malformed item) contributes warnings
//! and changes NOTHING — the last good composition always survives a bad
//! patch (the transaction half at the patch plane; boot-time
//! staging of a LIVE reload is the remaining half and lands with the
//! reconciler).

use serde_json::Value;

/// The final effective composition plus how it came to be.
#[derive(Debug, Clone)]
pub struct ResolvedComposition {
    pub entries: Vec<Value>,
    /// One row per surviving entry: which layer last touched it.
    pub provenance: Vec<EntryProvenance>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EntryProvenance {
    pub id: String,
    pub layer: String,
}

fn entry_id(entry: &Value) -> Option<&str> {
    entry
        .get("id")
        .and_then(|id| id.as_str())
        .or_else(|| entry.get("name").and_then(|name| name.as_str()))
}

/// Apply one layer's patch to `entries` in place. Pure over its inputs:
/// all effects are the returned mutations of the owned vec + warnings.
fn apply_layer(
    entries: &mut Vec<Value>,
    provenance: &mut Vec<EntryProvenance>,
    layer: &str,
    patch: &Value,
    warnings: &mut Vec<String>,
) {
    let Some(items) = patch.as_array() else {
        if !patch.is_null() {
            warnings.push(format!(
                "layer `{layer}`: patch must be an array — layer ignored"
            ));
        }
        return;
    };
    for item in items {
        if let Some(insert) = item.get("insert") {
            let Some(id) = entry_id(insert) else {
                warnings.push(format!(
                    "layer `{layer}`: insert without an id/name — skipped"
                ));
                continue;
            };
            if entries.iter().any(|entry| entry_id(entry) == Some(id)) {
                warnings.push(format!(
                    "layer `{layer}`: insert `{id}` already exists — skipped"
                ));
                continue;
            }
            entries.push(insert.clone());
            provenance.push(EntryProvenance {
                id: id.to_string(),
                layer: layer.to_string(),
            });
            continue;
        }
        if let Some(remove) = item.get("remove").and_then(|r| r.as_str()) {
            let before = entries.len();
            entries.retain(|entry| entry_id(entry) != Some(remove));
            if entries.len() == before {
                warnings.push(format!(
                    "layer `{layer}`: remove `{remove}` matches no entry — skipped"
                ));
            } else {
                provenance.retain(|row| row.id != remove);
            }
            continue;
        }
        let Some(id) = item.get("id").and_then(|id| id.as_str()) else {
            warnings.push(format!(
                "layer `{layer}`: patch item without `id`/`insert`/`remove` — skipped"
            ));
            continue;
        };
        let Some(target) = entries.iter_mut().find(|entry| entry_id(entry) == Some(id)) else {
            warnings.push(format!(
                "layer `{layer}`: patch target `{id}` matches no entry — skipped"
            ));
            continue;
        };
        // Wholesale config replacement — the documented contract.
        if let Some(object) = target.as_object_mut() {
            object.insert(
                "config".into(),
                item.get("config").cloned().unwrap_or(Value::Null),
            );
        }
        if let Some(row) = provenance.iter_mut().find(|row| row.id == id) {
            row.layer = layer.to_string();
        }
    }
}

/// Resolve the effective composition from the base entry list plus ordered
/// patch layers (`(layer name, patch value)`). Pure: same inputs, same
/// output; a broken layer contributes warnings only.
pub fn resolve_composition(base: &[Value], layers: &[(&str, &Value)]) -> ResolvedComposition {
    let mut entries: Vec<Value> = base.to_vec();
    let mut warnings = Vec::new();
    let mut provenance: Vec<EntryProvenance> = entries
        .iter()
        .filter_map(|entry| {
            entry_id(entry).map(|id| EntryProvenance {
                id: id.to_string(),
                layer: "base".to_string(),
            })
        })
        .collect();
    for (layer, patch) in layers {
        apply_layer(&mut entries, &mut provenance, layer, patch, &mut warnings);
    }
    ResolvedComposition {
        entries,
        provenance,
        warnings,
    }
}

/// The explainability face: the final effective composition for `config_dir`,
/// with provenance and warnings.
pub fn dump_composition(config_dir: &std::path::Path) -> Value {
    let raw = std::fs::read(config_dir.join("config.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    let section = raw.get("kernelPlugins").cloned().unwrap_or(Value::Null);
    let base: Vec<Value> = section
        .get("plugins")
        .and_then(|plugins| plugins.as_array())
        .cloned()
        .unwrap_or_default();
    let user_patches = section.get("patches").cloned().unwrap_or(Value::Null);
    let resolved = resolve_composition(&base, &[("user-patches", &user_patches)]);
    serde_json::json!({
        "entries": resolved.entries,
        "provenance": resolved
            .provenance
            .iter()
            .map(|row| serde_json::json!({ "id": row.id, "layer": row.layer }))
            .collect::<Vec<_>>(),
        "warnings": resolved.warnings,
        "web": raw.get("web").cloned().unwrap_or(Value::Null),
        "credentialGrants": section.get("credentialGrants").cloned().unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Vec<Value> {
        vec![
            json!({ "id": "llm", "name": "@deepseek-ai/dsh-llm-deepseek", "config": { "a": 1 } }),
            json!({ "id": "todo", "name": "@deepseek-ai/dsh-tool-todo",
                    "config": { "allowParallelInProgress": false } }),
        ]
    }

    #[test]
    fn replace_is_wholesale_insert_appends_and_misses_warn_and_skip() {
        let patch = json!([
            { "id": "todo", "config": { "allowParallelInProgress": true } },
            { "insert": { "id": "web", "name": "@deepseek-ai/dsh-tool-web", "config": {} } },
            { "id": "nope", "config": {} },
            { "remove": "ghost" },
        ]);
        let resolved = resolve_composition(&base(), &[("user", &patch)]);
        assert_eq!(resolved.entries.len(), 3);
        assert_eq!(
            resolved.entries[1]["config"],
            json!({ "allowParallelInProgress": true }),
            "config replaced wholesale"
        );
        assert_eq!(resolved.entries[2]["id"], "web");
        assert_eq!(resolved.warnings.len(), 2, "{:?}", resolved.warnings);
        assert!(resolved.warnings[0].contains("`nope`"));
        assert!(resolved.warnings[1].contains("`ghost`"));

        // Provenance names the last-touching layer.
        let layer_of = |id: &str| {
            resolved
                .provenance
                .iter()
                .find(|row| row.id == id)
                .map(|row| row.layer.clone())
        };
        assert_eq!(layer_of("llm").as_deref(), Some("base"));
        assert_eq!(layer_of("todo").as_deref(), Some("user"));
        assert_eq!(layer_of("web").as_deref(), Some("user"));
    }

    #[test]
    fn layers_apply_in_order_and_later_layers_win() {
        let user = json!([{ "id": "llm", "config": { "a": 2 } }]);
        let overlay = json!([
            { "id": "llm", "config": { "a": 3 } },
            { "remove": "todo" },
        ]);
        let resolved =
            resolve_composition(&base(), &[("user", &user), ("session-overlay", &overlay)]);
        assert_eq!(resolved.entries.len(), 1);
        assert_eq!(resolved.entries[0]["config"], json!({ "a": 3 }));
        assert_eq!(resolved.provenance[0].layer, "session-overlay");
        assert!(resolved.warnings.is_empty());
    }

    /// The transaction half at the patch plane: a malformed layer changes
    /// NOTHING — the last good composition survives.
    #[test]
    fn a_bad_layer_warns_and_leaves_the_composition_untouched() {
        let bad = json!({ "not": "an array" });
        let resolved = resolve_composition(&base(), &[("session-overlay", &bad)]);
        assert_eq!(resolved.entries, base());
        assert_eq!(resolved.warnings.len(), 1);
        assert!(resolved.warnings[0].contains("must be an array"));

        // Determinism: pure function, same inputs, same output.
        let again = resolve_composition(&base(), &[("session-overlay", &bad)]);
        assert_eq!(resolved.entries, again.entries);
    }

    #[test]
    fn dump_composition_reads_config_and_applies_user_patches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.json"),
            serde_json::to_string(&json!({
                "kernelPlugins": {
                    "plugins": [
                        { "id": "todo", "name": "@deepseek-ai/dsh-tool-todo", "config": {} },
                    ],
                    "patches": [
                        { "id": "todo", "config": { "allowParallelInProgress": true } },
                        { "id": "typo", "config": {} },
                    ],
                },
                "web": { "searchProvider": "exa" },
            }))
            .unwrap(),
        )
        .unwrap();
        let dump = dump_composition(tmp.path());
        assert_eq!(
            dump["entries"][0]["config"]["allowParallelInProgress"], true,
            "{dump}"
        );
        assert_eq!(dump["provenance"][0]["layer"], "user-patches");
        assert_eq!(dump["web"]["searchProvider"], "exa");
        assert_eq!(dump["warnings"].as_array().unwrap().len(), 1);
    }
}
