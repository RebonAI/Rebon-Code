//! What the mods have put on screen, for every surface to read.
//!
//! A mod's `$.ui.status`, `$.ui.toast`, `$.ui.log`, `$.ui.open` and
//! `$.prompt.submit` all land here, in one table with one version counter.
//! A surface — the terminal, the desktop app, a serve page — takes a
//! [`ModUiSnapshot`] whenever the version moved, draws the status texts and
//! the open panes, shows the toasts it has not shown, and submits the
//! prompts it has not submitted. Nothing here draws: the table is the
//! authoritative state, and each surface projects it in its own way.
//!
//! A pane's tree is not kept here. The surface asks the mod to draw it
//! (`ModsRegistry::render`) with its own viewport, every time the pane's
//! version moves — which a `$.ui.invalidate`, a `$.state.set` the drawing
//! read, or a press does.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How many toasts and log lines the table keeps for a late surface.
const KEPT_TOASTS: usize = 64;
const KEPT_LOG_LINES: usize = 256;
const KEPT_PROMPTS: usize = 32;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModToast {
    pub id: u64,
    pub plugin: String,
    pub text: String,
    /// `info`, `warning`, `error`, or whatever the mod said.
    #[serde(default)]
    pub kind: Option<String>,
    pub at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModLogLine {
    pub id: u64,
    pub plugin: String,
    pub text: String,
    /// `transcript` or `debug`.
    pub to: String,
    pub at_ms: u64,
}

/// A pane a mod opened: the surface seats it and asks for its drawing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModPane {
    /// The plugin id (the composition entry) that owns it.
    pub plugin: String,
    /// The mod's own name, which its render hook's `e.plugin` carries.
    pub plugin_name: String,
    /// The pane's id: the `requestId` its render hook is asked with.
    pub id: String,
    pub title: String,
    /// Bumped by every change that should redraw it.
    pub version: u64,
    /// Opened asking for the keyboard (`focus: true`).
    #[serde(default)]
    pub focus: bool,
    /// Opened as a dialog that closes on Esc.
    #[serde(default)]
    pub close_on_escape: bool,
    /// Rows asked for, where the surface honours it.
    #[serde(default)]
    pub rows: Option<u32>,
    /// Whether the person asked for it (a command, a press) or the mod
    /// opened it unasked; an unasked pane seats only where there is room.
    #[serde(default)]
    pub unasked: bool,
}

/// A prompt a mod queued with `$.prompt.submit`, for the surface to send.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModPrompt {
    pub id: u64,
    pub plugin: String,
    pub text: String,
    pub at_ms: u64,
}

/// Text a mod asked to put in the prompt box (`$.prompt.fill`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModFill {
    pub id: u64,
    pub plugin: String,
    pub text: String,
    /// `replace`, `append` or `insert`.
    pub mode: String,
    pub at_ms: u64,
}

/// A mod asking a site's focus ring onto one of its elements
/// (`$.ui.focus({ requestId, key })`), for the surface holding that site's
/// keyboard to move it there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModFocusRequest {
    pub id: u64,
    pub plugin: String,
    #[serde(rename = "requestId")]
    pub request_id: String,
    pub key: String,
}

/// A notice a mod put under a tool's open dialog.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModNotice {
    pub plugin: String,
    pub tool_use_id: String,
    pub text: String,
}

#[derive(Default)]
struct Table {
    status: BTreeMap<String, String>,
    toasts: VecDeque<ModToast>,
    log: VecDeque<ModLogLine>,
    panes: BTreeMap<String, ModPane>,
    prompts: VecDeque<ModPrompt>,
    fills: VecDeque<ModFill>,
    focus: VecDeque<ModFocusRequest>,
    notices: Vec<ModNotice>,
    next_id: u64,
}

/// The table, with a version a surface compares to its last read.
#[derive(Default)]
pub struct ModUiState {
    table: Mutex<Table>,
    version: AtomicU64,
    changed: tokio::sync::Notify,
}

/// One read of the table.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModUiSnapshot {
    pub version: u64,
    /// Status texts by plugin id, in plugin order.
    pub status: Vec<(String, String)>,
    pub toasts: Vec<ModToast>,
    pub log: Vec<ModLogLine>,
    pub panes: Vec<ModPane>,
    pub prompts: Vec<ModPrompt>,
    pub fills: Vec<ModFill>,
    pub notices: Vec<ModNotice>,
    /// `$.ui.focus` asks not yet taken; absent from an older owner.
    #[serde(default)]
    pub focus: Vec<ModFocusRequest>,
}

impl ModUiState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current version; a surface redraws when it moved.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Resolves once the version moves past `seen`.
    pub async fn changed_since(&self, seen: u64) {
        loop {
            if self.version() != seen {
                return;
            }
            self.changed.notified().await;
        }
    }

    fn bump(&self) -> u64 {
        let version = self.version.fetch_add(1, Ordering::AcqRel) + 1;
        self.changed.notify_waiters();
        version
    }

    fn with<T>(&self, f: impl FnOnce(&mut Table) -> T) -> T {
        let mut table = self.table.lock().expect("mod ui table poisoned");
        let out = f(&mut table);
        drop(table);
        self.bump();
        out
    }

    pub fn snapshot(&self) -> ModUiSnapshot {
        let table = self.table.lock().expect("mod ui table poisoned");
        ModUiSnapshot {
            version: self.version(),
            status: table
                .status
                .iter()
                .map(|(plugin, text)| (plugin.clone(), text.clone()))
                .collect(),
            toasts: table.toasts.iter().cloned().collect(),
            log: table.log.iter().cloned().collect(),
            panes: table.panes.values().cloned().collect(),
            prompts: table.prompts.iter().cloned().collect(),
            fills: table.fills.iter().cloned().collect(),
            notices: table.notices.clone(),
            focus: table.focus.iter().cloned().collect(),
        }
    }

    pub fn set_status(&self, plugin: &str, text: Option<String>) {
        self.with(|table| match text {
            Some(text) if !text.trim().is_empty() => {
                table.status.insert(plugin.to_owned(), text);
            }
            _ => {
                table.status.remove(plugin);
            }
        });
    }

    pub fn push_toast(&self, plugin: &str, text: String, kind: Option<String>) -> u64 {
        self.with(|table| {
            table.next_id += 1;
            let id = table.next_id;
            table.toasts.push_back(ModToast {
                id,
                plugin: plugin.to_owned(),
                text,
                kind,
                at_ms: rebon_types::wall_clock_ms(),
            });
            while table.toasts.len() > KEPT_TOASTS {
                table.toasts.pop_front();
            }
            id
        })
    }

    pub fn push_log(&self, plugin: &str, text: String, to: &str) -> u64 {
        self.with(|table| {
            table.next_id += 1;
            let id = table.next_id;
            table.log.push_back(ModLogLine {
                id,
                plugin: plugin.to_owned(),
                text,
                to: to.to_owned(),
                at_ms: rebon_types::wall_clock_ms(),
            });
            while table.log.len() > KEPT_LOG_LINES {
                table.log.pop_front();
            }
            id
        })
    }

    pub fn push_prompt(&self, plugin: &str, text: String) -> u64 {
        self.with(|table| {
            table.next_id += 1;
            let id = table.next_id;
            table.prompts.push_back(ModPrompt {
                id,
                plugin: plugin.to_owned(),
                text,
                at_ms: rebon_types::wall_clock_ms(),
            });
            while table.prompts.len() > KEPT_PROMPTS {
                table.prompts.pop_front();
            }
            id
        })
    }

    /// Takes the prompts queued so far: the surface that takes them sends
    /// them, and a second surface finds none.
    pub fn take_prompts(&self) -> Vec<ModPrompt> {
        let taken: Vec<ModPrompt> = self.with(|table| table.prompts.drain(..).collect());
        taken
    }

    pub fn push_fill(&self, plugin: &str, text: String, mode: String) -> u64 {
        self.with(|table| {
            table.next_id += 1;
            let id = table.next_id;
            table.fills.push_back(ModFill {
                id,
                plugin: plugin.to_owned(),
                text,
                mode,
                at_ms: rebon_types::wall_clock_ms(),
            });
            while table.fills.len() > KEPT_PROMPTS {
                table.fills.pop_front();
            }
            id
        })
    }

    pub fn take_fills(&self) -> Vec<ModFill> {
        self.with(|table| table.fills.drain(..).collect())
    }

    /// Queues a `$.ui.focus` ask; a later one for the same site replaces it.
    pub fn push_focus(&self, plugin: &str, request_id: String, key: String) -> u64 {
        self.with(|table| {
            table.next_id += 1;
            let id = table.next_id;
            table
                .focus
                .retain(|held| !(held.plugin == plugin && held.request_id == request_id));
            table.focus.push_back(ModFocusRequest {
                id,
                plugin: plugin.to_owned(),
                request_id,
                key,
            });
            while table.focus.len() > KEPT_PROMPTS {
                table.focus.pop_front();
            }
            id
        })
    }

    /// The focus asks, taken: one surface acts on each.
    pub fn take_focus(&self) -> Vec<ModFocusRequest> {
        self.with(|table| table.focus.drain(..).collect())
    }

    pub fn set_notice(&self, plugin: &str, tool_use_id: &str, text: Option<String>) {
        self.with(|table| {
            table
                .notices
                .retain(|notice| !(notice.plugin == plugin && notice.tool_use_id == tool_use_id));
            if let Some(text) = text {
                table.notices.push(ModNotice {
                    plugin: plugin.to_owned(),
                    tool_use_id: tool_use_id.to_owned(),
                    text,
                });
            }
        });
    }

    /// Opens a pane, or re-titles and redraws an open one with the same id.
    pub fn open_pane(&self, pane: ModPane) {
        self.with(|table| {
            let version = table
                .panes
                .get(&pane.id)
                .map(|open| open.version + 1)
                .unwrap_or(1);
            table
                .panes
                .insert(pane.id.clone(), ModPane { version, ..pane });
        });
    }

    /// Closes a pane; `true` when one was open.
    pub fn close_pane(&self, plugin: &str, id: &str) -> bool {
        self.with(|table| match table.panes.get(id) {
            Some(open) if open.plugin == plugin => {
                table.panes.remove(id);
                true
            }
            _ => false,
        })
    }

    /// Asks for every pane of `plugin` (or one, by id) to be drawn again.
    pub fn invalidate(&self, plugin: &str, id: Option<&str>) {
        self.with(|table| {
            for pane in table.panes.values_mut() {
                if pane.plugin == plugin && id.map_or(true, |wanted| wanted == pane.id) {
                    pane.version += 1;
                }
            }
        });
    }

    pub fn pane(&self, id: &str) -> Option<ModPane> {
        self.table
            .lock()
            .expect("mod ui table poisoned")
            .panes
            .get(id)
            .cloned()
    }

    pub fn panes_of(&self, plugin: &str) -> Vec<ModPane> {
        self.table
            .lock()
            .expect("mod ui table poisoned")
            .panes
            .values()
            .filter(|pane| pane.plugin == plugin)
            .cloned()
            .collect()
    }

    /// Everything a plugin put here, taken out when it unloads.
    pub fn forget_plugin(&self, plugin: &str) {
        self.with(|table| {
            table.status.remove(plugin);
            table.panes.retain(|_, pane| pane.plugin != plugin);
            table.notices.retain(|notice| notice.plugin != plugin);
        });
    }

    /// The pane list as JSON, for `$.ui.panes()`.
    pub fn panes_json(&self, plugin: &str) -> Value {
        Value::Array(
            self.panes_of(plugin)
                .into_iter()
                .map(|pane| serde_json::to_value(pane).expect("a pane is plain data"))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(plugin: &str, id: &str) -> ModPane {
        ModPane {
            plugin: plugin.to_owned(),
            plugin_name: plugin.to_owned(),
            id: id.to_owned(),
            title: id.to_owned(),
            version: 0,
            focus: false,
            close_on_escape: false,
            rows: None,
            unasked: false,
        }
    }

    #[test]
    fn every_write_moves_the_version_and_a_snapshot_reads_it_back() {
        let ui = ModUiState::new();
        assert_eq!(ui.version(), 0);
        ui.set_status("a", Some("busy".into()));
        ui.push_toast("a", "hello".into(), None);
        ui.push_log("a", "line".into(), "debug");
        ui.open_pane(pane("a", "p"));
        let snapshot = ui.snapshot();
        assert_eq!(snapshot.version, 4);
        assert_eq!(snapshot.status, vec![("a".to_owned(), "busy".to_owned())]);
        assert_eq!(snapshot.toasts[0].text, "hello");
        assert_eq!(snapshot.log[0].to, "debug");
        assert_eq!(snapshot.panes[0].version, 1);
        ui.set_status("a", None);
        assert!(ui.snapshot().status.is_empty());
    }

    #[test]
    fn panes_reopen_in_place_invalidate_and_close_by_owner() {
        let ui = ModUiState::new();
        ui.open_pane(pane("a", "p"));
        ui.open_pane(ModPane {
            title: "again".into(),
            ..pane("a", "p")
        });
        assert_eq!(ui.pane("p").unwrap().version, 2);
        assert_eq!(ui.pane("p").unwrap().title, "again");
        ui.invalidate("a", None);
        assert_eq!(ui.pane("p").unwrap().version, 3);
        ui.invalidate("b", None);
        assert_eq!(ui.pane("p").unwrap().version, 3);
        assert!(!ui.close_pane("b", "p"));
        assert!(ui.close_pane("a", "p"));
        assert!(ui.pane("p").is_none());
    }

    #[test]
    fn a_focus_ask_is_taken_once_and_a_later_one_for_the_site_replaces_it() {
        let ui = ModUiState::new();
        ui.push_focus("a", "pane".into(), "first".into());
        ui.push_focus("a", "pane".into(), "second".into());
        ui.push_focus("a", "other".into(), "x".into());
        assert_eq!(ui.snapshot().focus.len(), 2);
        let taken = ui.take_focus();
        assert_eq!(
            taken.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(),
            vec!["second", "x"]
        );
        assert!(ui.take_focus().is_empty());
    }

    #[test]
    fn prompts_and_fills_are_taken_once() {
        let ui = ModUiState::new();
        ui.push_prompt("a", "go".into());
        ui.push_fill("a", "draft".into(), "insert".into());
        assert_eq!(ui.take_prompts().len(), 1);
        assert!(ui.take_prompts().is_empty());
        assert_eq!(ui.take_fills()[0].mode, "insert");
        assert!(ui.take_fills().is_empty());
    }

    #[test]
    fn forgetting_a_plugin_takes_its_status_panes_and_notices() {
        let ui = ModUiState::new();
        ui.set_status("a", Some("x".into()));
        ui.set_status("b", Some("y".into()));
        ui.open_pane(pane("a", "p"));
        ui.set_notice("a", "t1", Some("checked".into()));
        ui.forget_plugin("a");
        let snapshot = ui.snapshot();
        assert_eq!(snapshot.status, vec![("b".to_owned(), "y".to_owned())]);
        assert!(snapshot.panes.is_empty());
        assert!(snapshot.notices.is_empty());
    }

    #[tokio::test]
    async fn changed_since_wakes_on_a_write() {
        let ui = std::sync::Arc::new(ModUiState::new());
        let waiter = {
            let ui = ui.clone();
            tokio::spawn(async move {
                ui.changed_since(0).await;
                ui.version()
            })
        };
        tokio::task::yield_now().await;
        ui.push_toast("a", "x".into(), None);
        assert_eq!(waiter.await.unwrap(), 1);
    }
}
