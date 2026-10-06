//! The mods of another process, reached through one JSON call.
//!
//! A session that lives in a worker has its mods there: the worker's plane
//! loaded them, and only there do their hooks hear the session's turns. A
//! surface in another process — the desktop app, a terminal mirroring a
//! worker — draws them all the same, through the session's own wire: it
//! sends one call (`{ "op": ..., ... }`), the worker hands it to
//! [`ModsRegistry::serve_remote`], and the answer comes back as JSON.
//!
//! [`ModsLink`] is the surface's side of that: the same few questions —
//! what changed, draw this pane, the person pressed this — asked of a
//! registry in this process or of one behind a [`RemoteMods`], so a surface
//! is written once and does not know which it has. The in-process link goes
//! through `serve_remote` too, so the two can only differ in transport.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use rebon_types::{ModUiNode, ModUiSurface};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    ModFacts, ModFill, ModFocusRequest, ModPrompt, ModRenderAnswer, ModUiSnapshot, ModsRegistry,
};

/// A mod command as a surface lists it: enough for a `/` menu, and the mod
/// that answers it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModCommandRow {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub argument_hint: Option<String>,
    /// The plugin id of the mod that answers it.
    pub plugin: String,
    /// The mod's own name.
    pub plugin_name: String,
}

/// One answer to `snapshot`: whether the table moved since the version the
/// surface last read, and if so what it holds now.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModsView {
    pub version: u64,
    /// `false` when the table is where `since` left it; nothing else is set.
    pub changed: bool,
    #[serde(default)]
    pub snapshot: Option<ModUiSnapshot>,
    /// Prompts taken off the table for this surface to send (`take`).
    #[serde(default)]
    pub prompts: Vec<ModPrompt>,
    /// Composer fills taken off the table for this surface (`take`).
    #[serde(default)]
    pub fills: Vec<ModFill>,
    /// `$.ui.focus` asks taken off the table for this surface (`take`).
    #[serde(default)]
    pub focus: Vec<ModFocusRequest>,
    /// The loaded mods, as [`ModsRegistry::rows`] lists them.
    #[serde(default)]
    pub mods: Vec<Value>,
    #[serde(default)]
    pub commands: Vec<ModCommandRow>,
}

/// What a mod command answered: text to send as the person's prompt, or
/// nothing when the command did its work itself.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModCommandAnswer {
    pub text: String,
    pub plugin_name: String,
}

/// A registry in another process, as a transport the surface supplies.
pub trait RemoteMods: Send + Sync {
    fn call(&self, call: Value) -> BoxFuture<'static, Result<Value, String>>;
}

impl<F> RemoteMods for F
where
    F: Fn(Value) -> BoxFuture<'static, Result<Value, String>> + Send + Sync,
{
    fn call(&self, call: Value) -> BoxFuture<'static, Result<Value, String>> {
        self(call)
    }
}

/// The mods a surface draws: this process's, or a session worker's.
#[derive(Clone)]
pub enum ModsLink {
    Local(Arc<ModsRegistry>),
    Remote(Arc<dyn RemoteMods>),
}

impl std::fmt::Debug for ModsLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(_) => f.write_str("ModsLink::Local"),
            Self::Remote(_) => f.write_str("ModsLink::Remote"),
        }
    }
}

impl ModsLink {
    async fn call(&self, call: Value) -> Result<Value, String> {
        match self {
            Self::Local(registry) => registry.serve_remote(call).await,
            Self::Remote(remote) => remote.call(call).await,
        }
    }

    /// What changed since `since`; with `take`, the queued prompts and
    /// fills come along and leave the table, so one surface acts on each.
    pub async fn snapshot(
        &self,
        since: Option<u64>,
        take: bool,
        surface: ModUiSurface,
    ) -> Result<ModsView, String> {
        let answer = self
            .call(json!({ "op": "snapshot", "since": since, "take": take, "surface": surface.as_str() }))
            .await?;
        serde_json::from_value(answer)
            .map_err(|error| format!("a mods snapshot did not parse: {error}"))
    }

    /// Asks the mod that owns a pane to draw it.
    pub async fn render(
        &self,
        plugin: &str,
        component: &str,
        surface: ModUiSurface,
        request_id: &str,
        props: Value,
        viewport: Option<(u32, u32)>,
    ) -> Result<ModRenderAnswer, String> {
        let answer = self
            .call(json!({
                "op": "render", "plugin": plugin, "component": component,
                "surface": surface.as_str(), "requestId": request_id, "props": props,
                "viewport": viewport.map(|(columns, rows)| json!({ "columns": columns, "rows": rows })),
            }))
            .await?;
        render_answer_from_json(answer)
    }

    pub async fn press(
        &self,
        plugin: &str,
        component: &str,
        surface: ModUiSurface,
        request_id: &str,
        element: &str,
        href: Option<&str>,
    ) -> Result<(), String> {
        self.call(json!({
            "op": "press", "plugin": plugin, "component": component, "surface": surface.as_str(),
            "requestId": request_id, "element": element, "href": href,
        }))
        .await
        .map(drop)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn input(
        &self,
        plugin: &str,
        component: &str,
        surface: ModUiSurface,
        request_id: &str,
        element: &str,
        kind: &str,
        value: &str,
    ) -> Result<(), String> {
        self.call(json!({
            "op": "input", "plugin": plugin, "component": component, "surface": surface.as_str(),
            "requestId": request_id, "element": element, "kind": kind, "value": value,
        }))
        .await
        .map(drop)
    }

    pub async fn select(
        &self,
        plugin: &str,
        component: &str,
        surface: ModUiSurface,
        request_id: &str,
        element: &str,
        value: Value,
    ) -> Result<(), String> {
        self.call(json!({
            "op": "select", "plugin": plugin, "component": component, "surface": surface.as_str(),
            "requestId": request_id, "element": element, "value": value,
        }))
        .await
        .map(drop)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn client_key(
        &self,
        plugin: &str,
        component: &str,
        surface: ModUiSurface,
        request_id: &str,
        element: &str,
        key: Value,
    ) -> Result<(), String> {
        self.call(json!({
            "op": "clientKey", "plugin": plugin, "component": component, "surface": surface.as_str(),
            "requestId": request_id, "element": element, "key": key,
        }))
        .await
        .map(drop)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn client_pointer(
        &self,
        plugin: &str,
        component: &str,
        surface: ModUiSurface,
        request_id: &str,
        element: &str,
        pointer: Value,
    ) -> Result<(), String> {
        self.call(json!({
            "op": "clientPointer", "plugin": plugin, "component": component, "surface": surface.as_str(),
            "requestId": request_id, "element": element, "pointer": pointer,
        }))
        .await
        .map(drop)
    }

    /// Asks the mod whether the ring may move: the inner `Err` is its
    /// reason for keeping the ring, else where it lands (`None`: off its
    /// elements).
    pub async fn focus(
        &self,
        plugin: &str,
        component: &str,
        request_id: &str,
        element: Option<&str>,
        origin: Value,
    ) -> Result<Result<Option<String>, String>, String> {
        let answer = self
            .call(json!({
                "op": "focus", "plugin": plugin, "component": component,
                "requestId": request_id, "element": element, "origin": origin,
            }))
            .await?;
        Ok(focus_answer(&answer))
    }

    pub async fn close_pane(&self, plugin: &str, id: &str) -> Result<(), String> {
        self.call(json!({ "op": "closePane", "plugin": plugin, "id": id }))
            .await
            .map(drop)
    }

    /// Runs a mod command where its mod lives.
    pub async fn command(
        &self,
        name: &str,
        args: &str,
        surface: ModUiSurface,
    ) -> Result<ModCommandAnswer, String> {
        let answer = self
            .call(
                json!({ "op": "command", "name": name, "args": args, "surface": surface.as_str() }),
            )
            .await?;
        serde_json::from_value(answer)
            .map_err(|error| format!("a mod command answer did not parse: {error}"))
    }

    /// Tells every mod which session, folder and surface it is drawn for.
    pub async fn facts(&self, facts: &ModFacts) -> Result<(), String> {
        self.call(json!({
            "op": "facts", "cwd": facts.cwd, "sessionId": facts.session_id,
            "model": facts.model, "surface": facts.surface,
        }))
        .await
        .map(drop)
    }
}

/// The commands of mods that live in another process, registered in this
/// process's command seat so its `/` menu lists them and typing one runs it
/// there.
///
/// A terminal mirroring a worker loads no mods of its own: the worker's are
/// the ones that hear the session. Their commands are still typed here, so
/// each sync registers the rows the worker last reported, every one answered
/// by [`ModsLink::command`] on that worker.
#[derive(Default)]
pub struct LinkedModCommands {
    rows: Vec<ModCommandRow>,
    scope: Option<rebon_kernel::Context>,
}

impl LinkedModCommands {
    /// How long a linked command waits for its mod's answer.
    pub const BOUND: std::time::Duration = std::time::Duration::from_secs(30);

    /// The rows registered now.
    pub fn rows(&self) -> &[ModCommandRow] {
        &self.rows
    }

    /// Registers `rows` on a fork of `kernel` in place of the last sync's,
    /// and answers whether anything changed. A name the seat already holds
    /// (a built-in, a plugin's) keeps its owner and is left out.
    pub fn sync(
        &mut self,
        kernel: &rebon_kernel::Context,
        rows: &[ModCommandRow],
        link: &ModsLink,
        surface: ModUiSurface,
        runtime: &tokio::runtime::Handle,
    ) -> bool {
        if self.rows == rows {
            return false;
        }
        self.clear();
        let Some(seat) = kernel.get::<rebon_command_seat::CommandSeatService>() else {
            return false;
        };
        let scope = kernel.fork("linked-mod-commands");
        for row in rows {
            let mut spec = rebon_command_seat::CommandSpec::new(
                row.name.clone(),
                row.description.clone().unwrap_or_else(|| {
                    format!("/{}, a command of the mod {}", row.name, row.plugin_name)
                }),
            )
            .kind(rebon_command_seat::CommandKind::Session)
            .surfaces(rebon_command_seat::Surfaces::ALL);
            if let Some(hint) = &row.argument_hint {
                spec = spec.hint(hint.clone());
            }
            let handler = rebon_command_seat::CommandHandler::Prompt(linked_command(
                link.clone(),
                row.name.clone(),
                surface,
                runtime.clone(),
            ));
            if let Err(error) = seat.register(&scope, spec, handler) {
                tracing::warn!(command = %row.name, %error, "mods: a linked command was not registered");
            }
        }
        self.rows = rows.to_vec();
        self.scope = Some(scope);
        true
    }

    /// Unregisters every linked command.
    pub fn clear(&mut self) {
        self.rows.clear();
        if let Some(scope) = self.scope.take() {
            scope.dispose();
        }
    }
}

impl Drop for LinkedModCommands {
    fn drop(&mut self) {
        self.clear();
    }
}

/// A seat expander that runs `/name` where its mod lives. Seat expanders
/// are called off the async runtime (a terminal runs them on the blocking
/// pool), so the call is spawned on `runtime` and waited for here.
fn linked_command(
    link: ModsLink,
    name: String,
    surface: ModUiSurface,
    runtime: tokio::runtime::Handle,
) -> CommandExpander {
    Arc::new(move |args| {
        let link = link.clone();
        let call_name = name.clone();
        let rest = args.rest.clone();
        wait_for_command(&runtime, &name, async move {
            link.command(&call_name, &rest, surface).await
        })
    })
}

/// A seat expander for a mod loaded in this process: `/name` runs through
/// [`ModsRegistry::run_command`], the same way a surface elsewhere runs it,
/// on the surface it was typed on.
pub(crate) fn local_mod_command(
    registry: std::sync::Weak<ModsRegistry>,
    runtime: tokio::runtime::Handle,
    name: String,
) -> CommandExpander {
    Arc::new(move |args| {
        let registry = registry
            .upgrade()
            .ok_or_else(|| format!("/{name}: its mod is no longer loaded"))?;
        let call_name = name.clone();
        let rest = args.rest.clone();
        let surface = match args.surface {
            rebon_command_seat::Surface::Desktop => ModUiSurface::Desktop,
            rebon_command_seat::Surface::Mobile => ModUiSurface::Mobile,
            _ => ModUiSurface::Terminal,
        };
        wait_for_command(&runtime, &name, async move {
            registry.run_command(&call_name, &rest, surface).await
        })
    })
}

/// A prompt command's expander, as the command seat holds one.
type CommandExpander =
    Arc<dyn Fn(&rebon_command_seat::CommandArgs) -> Result<String, String> + Send + Sync>;

/// Runs a command's answer on `runtime` and waits for it here, at most
/// [`LinkedModCommands::BOUND`]: seat expanders are called off the async
/// runtime (a terminal runs them on the blocking pool).
fn wait_for_command(
    runtime: &tokio::runtime::Handle,
    name: &str,
    answer: impl std::future::Future<Output = Result<ModCommandAnswer, String>> + Send + 'static,
) -> Result<String, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    runtime.spawn(async move {
        let _ = tx.send(answer.await);
    });
    match rx.recv_timeout(LinkedModCommands::BOUND) {
        Ok(answer) => answer.map(|answer| answer.text),
        Err(_) => Err(format!(
            "/{name} did not answer within {} seconds",
            LinkedModCommands::BOUND.as_secs()
        )),
    }
}

/// A `ui.focus` answer: `{ deny }` keeps the ring, `{ element }` lands it.
fn focus_answer(answer: &Value) -> Result<Option<String>, String> {
    if let Some(reason) = answer.get("deny").and_then(Value::as_str) {
        return Err(reason.to_owned());
    }
    Ok(answer
        .get("element")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

fn render_answer_from_json(answer: Value) -> Result<ModRenderAnswer, String> {
    if answer.get("engine").and_then(Value::as_bool) == Some(true) {
        return Ok(ModRenderAnswer::Engine);
    }
    if let Some(error) = answer.get("error").and_then(Value::as_str) {
        return Err(error.to_owned());
    }
    let tree = answer
        .get("tree")
        .cloned()
        .ok_or_else(|| "the mod answered no tree".to_owned())?;
    serde_json::from_value::<ModUiNode>(tree)
        .map(ModRenderAnswer::Tree)
        .map_err(|error| format!("the drawn tree did not parse: {error}"))
}

fn str_of<'a>(call: &'a Value, key: &str) -> Result<&'a str, String> {
    call.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("a mods call needs `{key}`"))
}

fn surface_of(call: &Value) -> ModUiSurface {
    call.get("surface")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or(ModUiSurface::Desktop)
}

impl ModsRegistry {
    /// [`Self::serve_remote`] from a thread that is not on a runtime — a
    /// session owner's connection thread — run on the plane's own runtime and
    /// waited for at most `bound`. Safe from any thread: the call is spawned,
    /// never `block_on`'d here.
    pub fn serve_remote_blocking(
        self: &Arc<Self>,
        call: Value,
        bound: std::time::Duration,
    ) -> Result<Value, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let registry = Arc::clone(self);
        self.runtime.spawn(async move {
            let _ = tx.send(registry.serve_remote(call).await);
        });
        rx.recv_timeout(bound).unwrap_or_else(|_| {
            Err(format!(
                "the mods did not answer within {} seconds",
                bound.as_secs()
            ))
        })
    }

    /// The answer of a session with no mods loaded: an empty table at
    /// version 0 for a snapshot, a refusal for anything that needs a mod.
    pub fn answer_without_mods(call: &Value) -> Result<Value, String> {
        match call.get("op").and_then(Value::as_str) {
            Some("snapshot") => {
                let since = call.get("since").and_then(Value::as_u64);
                Ok(json!({ "version": 0, "changed": since != Some(0), "mods": [], "commands": [] }))
            }
            Some("facts") => Ok(json!({})),
            _ => Err("no mods are loaded in this session".to_owned()),
        }
    }

    /// The commands the loaded mods answer, for a surface's `/` menu.
    pub fn command_rows(&self) -> Vec<ModCommandRow> {
        self.mods()
            .iter()
            .flat_map(|record| {
                record.commands().into_iter().map(|command| ModCommandRow {
                    name: command.name,
                    description: command.description,
                    argument_hint: command.argument_hint,
                    plugin: record.id.clone(),
                    plugin_name: record.name.clone(),
                })
            })
            .collect()
    }

    /// Runs a mod command as the person's own act, so a pane it opens is
    /// one they asked for.
    pub async fn run_command(
        &self,
        name: &str,
        args: &str,
        surface: ModUiSurface,
    ) -> Result<ModCommandAnswer, String> {
        let record = self
            .mods()
            .into_iter()
            .find(|record| record.commands().iter().any(|command| command.name == name))
            .ok_or_else(|| format!("no loaded mod answers /{name}"))?;
        let raw = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        let answer = self
            .person_action(
                &record.id,
                json!({ "kind": "command", "command": name, "args": args, "raw": raw, "surface": surface.as_str() }),
            )
            .await?;
        let text = answer
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.record_command_output(name, &text, answer.get("context").and_then(Value::as_str));
        Ok(ModCommandAnswer {
            text,
            plugin_name: record.name.clone(),
        })
    }

    /// One call from a surface, in whatever process it runs.
    pub async fn serve_remote(&self, call: Value) -> Result<Value, String> {
        let op = str_of(&call, "op")?;
        match op {
            "snapshot" => {
                let surface = surface_of(&call);
                self.attach_surface(surface.as_str());
                let since = call.get("since").and_then(Value::as_u64);
                let take = call.get("take").and_then(Value::as_bool).unwrap_or(false);
                let mut snapshot = self.ui.snapshot();
                let (mut prompts, mut fills, mut focus) = (Vec::new(), Vec::new(), Vec::new());
                // Taken only when there is something to take: a take moves
                // the version, and a poll that moved it every time would
                // never see the table at rest.
                if take && !snapshot.prompts.is_empty() {
                    prompts = self.ui.take_prompts();
                }
                if take && !snapshot.fills.is_empty() {
                    fills = self.ui.take_fills();
                }
                if take && !snapshot.focus.is_empty() {
                    focus = self.ui.take_focus();
                }
                if !prompts.is_empty() || !fills.is_empty() || !focus.is_empty() {
                    snapshot = self.ui.snapshot();
                }
                if since == Some(snapshot.version) {
                    return Ok(json!({ "version": snapshot.version, "changed": false }));
                }
                let view = ModsView {
                    version: snapshot.version,
                    changed: true,
                    mods: self.rows(),
                    commands: self.command_rows(),
                    snapshot: Some(snapshot),
                    prompts,
                    fills,
                    focus,
                };
                serde_json::to_value(view).map_err(|error| error.to_string())
            }
            "render" => {
                let viewport = call.get("viewport").and_then(|viewport| {
                    Some((
                        u32::try_from(viewport.get("columns")?.as_u64()?).ok()?,
                        u32::try_from(viewport.get("rows")?.as_u64()?).ok()?,
                    ))
                });
                let answer = self
                    .render(
                        str_of(&call, "plugin")?,
                        str_of(&call, "component")?,
                        surface_of(&call),
                        str_of(&call, "requestId")?,
                        call.get("props").cloned().unwrap_or_else(|| json!({})),
                        viewport,
                    )
                    .await;
                Ok(match answer {
                    Ok(ModRenderAnswer::Engine) => json!({ "engine": true }),
                    Ok(ModRenderAnswer::Tree(tree)) => json!({ "tree": tree }),
                    Err(error) => json!({ "error": error }),
                })
            }
            "press" => {
                self.press(
                    str_of(&call, "plugin")?,
                    str_of(&call, "component")?,
                    surface_of(&call),
                    str_of(&call, "requestId")?,
                    str_of(&call, "element")?,
                    call.get("href").and_then(Value::as_str),
                )
                .await
            }
            "input" => {
                self.input(
                    str_of(&call, "plugin")?,
                    str_of(&call, "component")?,
                    surface_of(&call),
                    str_of(&call, "requestId")?,
                    str_of(&call, "element")?,
                    call.get("kind").and_then(Value::as_str).unwrap_or("submit"),
                    call.get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
                .await
            }
            "select" => {
                self.select(
                    str_of(&call, "plugin")?,
                    str_of(&call, "component")?,
                    surface_of(&call),
                    str_of(&call, "requestId")?,
                    str_of(&call, "element")?,
                    call.get("value").cloned().unwrap_or(Value::Null),
                )
                .await
            }
            "closePane" => {
                let (plugin, id) = (str_of(&call, "plugin")?, str_of(&call, "id")?);
                let closed = self.ui.close_pane(plugin, id);
                // Its Client instances stop with it: their timers would
                // otherwise go on asking for a pane no one draws.
                if let Err(error) = self
                    .call_mod(plugin, json!({ "kind": "forget", "requestId": id }))
                    .await
                {
                    tracing::debug!(%error, "mods: a closed pane's mod did not forget it");
                }
                Ok(json!({ "closed": closed }))
            }
            "clientKey" | "clientPointer" => {
                let plugin = str_of(&call, "plugin")?;
                let component = str_of(&call, "component")?;
                let request_id = str_of(&call, "requestId")?;
                let element = str_of(&call, "element")?;
                let surface = surface_of(&call);
                if op == "clientKey" {
                    self.client_key(
                        plugin,
                        component,
                        surface,
                        request_id,
                        element,
                        call.get("key").cloned().unwrap_or(Value::Null),
                    )
                    .await
                } else {
                    self.client_pointer(
                        plugin,
                        component,
                        surface,
                        request_id,
                        element,
                        call.get("pointer").cloned().unwrap_or(Value::Null),
                    )
                    .await
                }
            }
            "focus" => {
                self.focus(
                    str_of(&call, "plugin")?,
                    str_of(&call, "component")?,
                    str_of(&call, "requestId")?,
                    call.get("element").and_then(Value::as_str),
                    call.get("origin")
                        .cloned()
                        .unwrap_or_else(|| json!({ "kind": "person" })),
                )
                .await
            }
            "command" => {
                let answer = self
                    .run_command(
                        str_of(&call, "name")?,
                        call.get("args").and_then(Value::as_str).unwrap_or_default(),
                        surface_of(&call),
                    )
                    .await?;
                serde_json::to_value(answer).map_err(|error| error.to_string())
            }
            "facts" => {
                let text = |key: &str| call.get(key).and_then(Value::as_str).map(str::to_owned);
                let facts = ModFacts {
                    cwd: text("cwd").unwrap_or_default(),
                    session_id: text("sessionId"),
                    model: text("model"),
                    surface: text("surface"),
                };
                if let Some(surface) = &facts.surface {
                    self.attach_surface(surface);
                }
                for record in self.mods() {
                    self.tell_facts(&record.id, &facts).await;
                }
                Ok(json!({}))
            }
            other => Err(format!("a mods call does not know the op {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_render_answer_reads_back_from_its_json() {
        assert_eq!(
            render_answer_from_json(json!({ "engine": true })).unwrap(),
            ModRenderAnswer::Engine
        );
        assert_eq!(
            render_answer_from_json(json!({ "error": "no" })).unwrap_err(),
            "no"
        );
        let ModRenderAnswer::Tree(tree) = render_answer_from_json(
            json!({ "tree": { "type": "Text", "props": {}, "children": ["hi"] } }),
        )
        .unwrap() else {
            panic!("a tree")
        };
        assert_eq!(tree.ty, "Text");
        assert!(render_answer_from_json(json!({})).is_err());
    }

    #[test]
    fn a_focus_answer_keeps_moves_or_lands_the_ring() {
        assert_eq!(focus_answer(&json!({ "deny": "no" })), Err("no".to_owned()));
        assert_eq!(
            focus_answer(&json!({ "element": "real" })),
            Ok(Some("real".to_owned()))
        );
        assert_eq!(focus_answer(&json!({ "element": null })), Ok(None));
        assert_eq!(focus_answer(&json!({})), Ok(None));
    }

    #[test]
    fn a_session_without_mods_answers_an_empty_table() {
        let view: ModsView = serde_json::from_value(
            ModsRegistry::answer_without_mods(&json!({ "op": "snapshot" })).unwrap(),
        )
        .unwrap();
        assert!(view.changed && view.mods.is_empty() && view.commands.is_empty());
        let rest: ModsView = serde_json::from_value(
            ModsRegistry::answer_without_mods(&json!({ "op": "snapshot", "since": 0 })).unwrap(),
        )
        .unwrap();
        assert!(!rest.changed);
        assert!(ModsRegistry::answer_without_mods(&json!({ "op": "facts" })).is_ok());
        assert!(ModsRegistry::answer_without_mods(&json!({ "op": "press" })).is_err());
    }

    #[test]
    fn a_view_round_trips_and_an_unchanged_one_reads_as_unchanged() {
        let unchanged: ModsView =
            serde_json::from_value(json!({ "version": 7, "changed": false })).unwrap();
        assert_eq!(unchanged.version, 7);
        assert!(!unchanged.changed);
        assert!(unchanged.snapshot.is_none());
        let view = ModsView {
            version: 3,
            changed: true,
            snapshot: Some(ModUiSnapshot::default()),
            commands: vec![ModCommandRow {
                name: "hi".into(),
                plugin: "tally".into(),
                plugin_name: "tally".into(),
                ..ModCommandRow::default()
            }],
            ..ModsView::default()
        };
        let back: ModsView = serde_json::from_value(serde_json::to_value(&view).unwrap()).unwrap();
        assert_eq!(back, view);
    }

    fn row(name: &str) -> ModCommandRow {
        ModCommandRow {
            name: name.into(),
            description: Some(format!("{name} it")),
            plugin: "tally".into(),
            plugin_name: "tally".into(),
            ..ModCommandRow::default()
        }
    }

    /// A remote that answers `command` with the name and args it was sent,
    /// and counts the calls.
    fn echo_link(calls: Arc<std::sync::atomic::AtomicUsize>) -> ModsLink {
        let ask = move |call: Value| -> BoxFuture<'static, Result<Value, String>> {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                assert_eq!(call["op"], "command");
                assert_eq!(call["surface"], "terminal");
                Ok(json!({
                    "text": format!("ran {} with {}", call["name"].as_str().unwrap(), call["args"].as_str().unwrap()),
                    "pluginName": "tally",
                }))
            })
        };
        ModsLink::Remote(Arc::new(ask))
    }

    fn seat_kernel() -> (
        Arc<rebon_kernel::Kernel>,
        Arc<rebon_command_seat::CommandSeat>,
    ) {
        let kernel = rebon_kernel::Kernel::new();
        let seat = rebon_command_seat::CommandSeat::new();
        kernel
            .context()
            .provide::<rebon_command_seat::CommandSeatService>(seat.clone())
            .unwrap();
        (kernel, seat)
    }

    fn expand(seat: &rebon_command_seat::CommandSeat, line: &str) -> Result<String, String> {
        let name = line.trim_start_matches('/').split(' ').next().unwrap();
        let command = seat.find(name).expect("registered");
        let rebon_command_seat::CommandHandler::Prompt(expand) = command.handler else {
            panic!("a prompt handler");
        };
        expand(&rebon_command_seat::CommandArgs {
            raw: line.into(),
            rest: line
                .split_once(' ')
                .map(|(_, rest)| rest.into())
                .unwrap_or_default(),
            surface: rebon_command_seat::Surface::Tui,
        })
    }

    #[test]
    fn linked_commands_run_where_their_mod_lives() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (kernel, seat) = seat_kernel();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let link = echo_link(Arc::clone(&calls));
        let mut linked = LinkedModCommands::default();
        assert!(linked.sync(
            kernel.context(),
            &[row("tally")],
            &link,
            ModUiSurface::Terminal,
            runtime.handle(),
        ));
        let command = seat.find("tally").expect("listed in the seat");
        assert_eq!(command.spec.description.as_ref(), "tally it");
        assert_eq!(expand(&seat, "/tally up 3").unwrap(), "ran tally with up 3");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_sync_replaces_the_last_rows_and_a_clear_or_drop_takes_them_away() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (kernel, seat) = seat_kernel();
        let link = echo_link(Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let mut linked = LinkedModCommands::default();
        let sync = |linked: &mut LinkedModCommands, rows: &[ModCommandRow]| {
            linked.sync(
                kernel.context(),
                rows,
                &link,
                ModUiSurface::Terminal,
                runtime.handle(),
            )
        };
        assert!(sync(&mut linked, &[row("a"), row("b")]));
        assert!(
            !sync(&mut linked, &[row("a"), row("b")]),
            "the same rows are no change"
        );
        assert!(sync(&mut linked, &[row("b")]));
        assert!(seat.find("a").is_none(), "a row the worker dropped goes");
        assert!(seat.find("b").is_some());
        assert_eq!(linked.rows().len(), 1);
        linked.clear();
        assert!(seat.find("b").is_none());
        assert!(sync(&mut linked, &[row("c")]));
        drop(linked);
        assert!(seat.find("c").is_none(), "dropping unregisters too");
    }

    #[test]
    fn a_name_the_seat_already_holds_keeps_its_owner() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (kernel, seat) = seat_kernel();
        let core = kernel.context().fork("core");
        seat.register(
            &core,
            rebon_command_seat::CommandSpec::new("help", "Help"),
            rebon_command_seat::CommandHandler::Prompt(Arc::new(|_| Ok("built-in".into()))),
        )
        .unwrap();
        let link = echo_link(Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let mut linked = LinkedModCommands::default();
        linked.sync(
            kernel.context(),
            &[row("help"), row("tally")],
            &link,
            ModUiSurface::Terminal,
            runtime.handle(),
        );
        assert_eq!(expand(&seat, "/help").unwrap(), "built-in");
        assert!(seat.find("tally").is_some());
        drop(linked);
        assert_eq!(
            expand(&seat, "/help").unwrap(),
            "built-in",
            "clearing the linked rows leaves the built-in"
        );
    }

    #[test]
    fn a_seatless_kernel_registers_nothing() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let kernel = rebon_kernel::Kernel::new();
        let link = echo_link(Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let mut linked = LinkedModCommands::default();
        assert!(!linked.sync(
            kernel.context(),
            &[row("tally")],
            &link,
            ModUiSurface::Terminal,
            runtime.handle(),
        ));
        assert!(linked.rows().is_empty());
    }
}
