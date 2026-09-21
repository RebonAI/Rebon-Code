//! The process-level registration face for composition-contributed
//! system-prompt sections.
//!
//! The composition's `ctx.systemPrompt` seat mirrors every
//! `section({name, order, text})` here over `register`/`unregister` calls —
//! the effect ledger covers the kill path, the token guards stale
//! sweeps. The table is one provider on the engine's `prompt-sections`
//! seat ([`rebon_core::prompt_seat`]), registered by the plane boot under
//! [`NODE_COMPOSITION_PROVIDER_ID`]; every section it answers with sits on
//! [`Rung::Context`], the stable runtime-context rung composition sections
//! have always rendered on. An absent composition (or empty table) leaves
//! the prompt byte-identical.

use std::collections::HashMap;
use std::sync::RwLock;

use rebon_core::prompt_seat::{PluginPromptSection, PromptSectionProvider, PromptSubject, Rung};
use rebon_kernel::{JsonService, KernelError};
use serde_json::Value;

/// JSON-plane name of the section registry service.
pub const SYSTEM_PROMPT_SERVICE: &str = "system-prompt";

/// The table's provider id on the engine's `prompt-sections` seat — the
/// prefix of it: the plane boot appends its attempt number, so a plane
/// that is still being torn down never blocks its successor's registration.
pub const NODE_COMPOSITION_PROVIDER_ID: &str = "node-composition";

struct SectionEntry {
    token: u64,
    order: f64,
    text: String,
}

/// Process table of composition prompt sections. One per composition boot.
#[derive(Default)]
pub struct ComposePromptSections {
    entries: RwLock<HashMap<String, SectionEntry>>,
    next_token: std::sync::atomic::AtomicU64,
}

impl ComposePromptSections {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// Snapshot for the engine assembly, sorted by (order, name) — the dsh
    /// ascending-order contract with a deterministic tie-break. Every
    /// section is on [`Rung::Context`]; the composition's `order` is the
    /// position inside that rung.
    pub fn snapshot(&self) -> Vec<PluginPromptSection> {
        let entries = self.entries.read().expect("prompt section table poisoned");
        let mut sections: Vec<PluginPromptSection> = entries
            .iter()
            .map(|(name, entry)| PluginPromptSection {
                name: name.clone(),
                rung: Rung::Context,
                order: entry.order,
                text: entry.text.clone(),
            })
            .collect();
        sections.sort_by(|a, b| {
            a.order
                .partial_cmp(&b.order)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.cmp(&b.name))
        });
        sections
    }

    pub fn section_count(&self) -> usize {
        self.entries
            .read()
            .expect("prompt section table poisoned")
            .len()
    }

    fn register(&self, params: &Value) -> Result<Value, KernelError> {
        let name = params
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| {
                KernelError::Other("system-prompt register requires a non-empty `name`".into())
            })?;
        let order = params
            .get("order")
            .and_then(|o| o.as_f64())
            .filter(|o| o.is_finite())
            .ok_or_else(|| {
                KernelError::Other(format!(
                    "prompt section `{name}` order must be a finite number"
                ))
            })?;
        let text = params
            .get("text")
            .and_then(|t| t.as_str())
            .ok_or_else(|| {
                KernelError::Other(format!("prompt section `{name}` text must be a string"))
            })?
            .to_string();
        let mut entries = self.entries.write().expect("prompt section table poisoned");
        // The JS seat already rejects duplicates plugin-side; a duplicate
        // here is an event replay or a second isolate — refuse, dsh rule.
        if entries.contains_key(name) {
            return Err(KernelError::Other(format!(
                "prompt section \"{name}\" is already registered in this scope"
            )));
        }
        let token = self
            .next_token
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        entries.insert(name.to_string(), SectionEntry { token, order, text });
        Ok(serde_json::json!({ "name": name, "token": token }))
    }

    fn unregister(&self, params: &Value) -> Value {
        let token_guard = params.get("token").and_then(|t| t.as_u64());
        let removed = params
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .is_some_and(|name| {
                let mut entries = self.entries.write().expect("prompt section table poisoned");
                match entries.get(name) {
                    Some(entry) if token_guard.is_none_or(|t| entry.token == t) => {
                        entries.remove(name).is_some()
                    }
                    _ => false,
                }
            });
        serde_json::json!({ "removed": removed })
    }
}

/// The table answers every subject alike: a composition section is static
/// text, and which model or session reads it is not its concern.
impl PromptSectionProvider for ComposePromptSections {
    fn sections_for(&self, _subject: &PromptSubject) -> Vec<PluginPromptSection> {
        self.snapshot()
    }
}

impl JsonService for ComposePromptSections {
    fn call(&self, method: &str, params: Value) -> Result<Value, KernelError> {
        match method {
            "register" => self.register(&params),
            "unregister" => Ok(self.unregister(&params)),
            "list" => Ok(serde_json::json!({
                "sections": self
                    .snapshot()
                    .into_iter()
                    .map(|s| serde_json::json!({ "name": s.name, "order": s.order, "text": s.text }))
                    .collect::<Vec<_>>(),
            })),
            other => Err(KernelError::Other(format!(
                "system-prompt has no method `{other}`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_snapshot_and_token_guard() {
        let sections = ComposePromptSections::new();
        let a = sections
            .call(
                "register",
                serde_json::json!({ "name": "tool:web_fetch", "order": 111, "text": "fetch" }),
            )
            .unwrap();
        sections
            .call(
                "register",
                serde_json::json!({ "name": "tool:web_search", "order": 110, "text": "search" }),
            )
            .unwrap();

        // Ascending (order, name) snapshot, every section on the Context rung.
        let snapshot = sections.snapshot();
        assert_eq!(snapshot[0].name, "tool:web_search");
        assert_eq!(snapshot[1].name, "tool:web_fetch");
        assert!(snapshot.iter().all(|s| s.rung == Rung::Context));

        // Duplicate names refuse with the dsh message.
        let err = sections
            .call(
                "register",
                serde_json::json!({ "name": "tool:web_search", "order": 1, "text": "x" }),
            )
            .expect_err("duplicate refused");
        assert!(err.to_string().contains("already registered"), "{err}");

        // Malformed orders refuse.
        let err = sections
            .call(
                "register",
                serde_json::json!({ "name": "bad", "text": "x" }),
            )
            .expect_err("missing order refused");
        assert!(err.to_string().contains("finite number"), "{err}");

        // Stale token spares nothing it should not.
        let stale = sections
            .call(
                "unregister",
                serde_json::json!({ "name": "tool:web_fetch", "token": 999 }),
            )
            .unwrap();
        assert_eq!(stale["removed"], false);
        let live = sections
            .call(
                "unregister",
                serde_json::json!({ "name": "tool:web_fetch", "token": a["token"] }),
            )
            .unwrap();
        assert_eq!(live["removed"], true);
        assert_eq!(sections.section_count(), 1);
    }

    /// On the engine's seat the table is one provider: what the JSON face
    /// registers is what a turn's lookup sees, on the Context rung, and a
    /// JSON unregister takes it back out.
    #[test]
    fn the_table_is_a_provider_on_the_engine_seat() {
        use rebon_core::prompt_seat::{sections_for, PromptSeat, PromptSeatService};
        use rebon_kernel::Kernel;

        let kernel = Kernel::new();
        let seat = PromptSeat::new();
        kernel
            .context()
            .provide::<PromptSeatService>(seat.clone())
            .unwrap();
        let plane = kernel.context().fork_scoped("plugin-plane");
        let table = ComposePromptSections::new();
        seat.register(&plane, NODE_COMPOSITION_PROVIDER_ID, table.clone())
            .unwrap();
        let subject = PromptSubject::new("any-model");
        let session = kernel.context().fork_scoped("session/abc");
        assert!(sections_for(&session, &subject).is_empty());

        let registered = table
            .call(
                "register",
                serde_json::json!({ "name": "tool:probe", "order": 1, "text": "Use probe." }),
            )
            .unwrap();
        let live = sections_for(&session, &subject);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].rung, Rung::Context);
        assert_eq!(live[0].text, "Use probe.");

        table
            .call(
                "unregister",
                serde_json::json!({ "name": "tool:probe", "token": registered["token"] }),
            )
            .unwrap();
        assert!(sections_for(&session, &subject).is_empty());

        // The plane's teardown takes the provider off the seat.
        table
            .call(
                "register",
                serde_json::json!({ "name": "tool:again", "order": 1, "text": "x" }),
            )
            .unwrap();
        plane.dispose();
        assert!(seat.provider_ids().is_empty());
        assert!(sections_for(&session, &subject).is_empty());
    }
}
