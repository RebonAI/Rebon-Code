//! The terminal as a surface for Claude Code mods.
//!
//! Once a pass, after the inbound channels have drained, the loop reads what
//! the mods have put on screen and projects it the way the terminal shows
//! such things: a mod's `$.ui.status` goes into the footer beside the working
//! directory, a `$.ui.toast` and a transcript `$.ui.log` line become system
//! notices, a `$.prompt.submit` is submitted through the same path a typed
//! line takes, and a `$.prompt.fill` lands in the input.
//!
//! Whose mods those are depends on where the session runs. A session in this
//! process (the default, and `--local`) has them in this process's plane,
//! read directly. A session hosted by a worker (`--hosted`, an attach) has
//! them in the worker, because only there do their hooks hear its turns; the
//! terminal then asks the worker over the session's wire (`_session/mods`),
//! one snapshot in flight at a time, and registers the commands the worker's
//! mods answer in this process's command seat, each run on the worker
//! ([`LinkedModCommands`]). Such a terminal loads no mods of its own
//! (`decide_session_host`), so a mod's `session.start` runs once.
//!
//! What a mod draws is asked for here too ([`draw_tick`]): the band above
//! the prompt (`AbovePrompt`) whenever the table moves, and each open pane
//! whenever its version does. The answers land in `AppState` for the frame to
//! paint (`render::mods`), and what the person does to what the frame painted
//! — a click, or the keyboard once a site holds it (`mods_keys`) — comes back
//! through `AppState::mods_acts` as a press, an input or a pick for the mod.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio::runtime::Handle;

use rebon_plugin_host::mods::{
    process_mods, LinkedModCommands, ModFacts, ModPane, ModRenderAnswer, ModUiSnapshot, ModsLink,
    ModsView,
};
use rebon_types::ModUiSurface;

use crate::tui::app::{AppState, ModAct, ModFocusOutcome, ModSite, TerminalModPane};
use crate::tui::permission_modal::PendingPermission;
use crate::tui::wiring::TuiEngineSession;
use crate::ui_config::UiMode;

use super::submit::submit_or_queue;
use super::transcript_messages::inject_mod_notice;
use super::ActivePrompt;

/// How often a hosted session's worker is asked what its mods changed.
const REMOTE_POLL: Duration = Duration::from_millis(300);
/// And how often, while it reported no mods at all.
const REMOTE_POLL_IDLE: Duration = Duration::from_secs(2);

/// What this surface has already shown, so a pass shows each thing once.
#[derive(Default)]
pub(super) struct ModsSurfaceState {
    seen_version: u64,
    last_toast: u64,
    last_log: u64,
    attached: bool,
    /// The session the mods were last told about.
    facts_told_for: Option<String>,
    /// The worker's mods, while the session is hosted by one.
    remote: Option<RemoteMods>,
    /// The drawings asked for and not yet painted.
    draw: DrawState,
}

/// The `requestId` the band above the prompt is asked under.
const BAND: &str = "AbovePrompt";

/// Drawings in flight, and the answers that came back since the last pass.
#[derive(Default)]
struct DrawState {
    answers: Arc<Mutex<Vec<Drawn>>>,
    in_flight: std::collections::HashSet<String>,
    /// What the band was last asked for: table version, whether a turn was
    /// running, terminal width.
    band_for: Option<(u64, bool, u16)>,
    /// The person's last acts still being handed to the mods.
    acts_tail: Option<tokio::task::JoinHandle<()>>,
    /// What the mods' `ui.focus` hooks answered since the last pass.
    focus_outcomes: Arc<Mutex<Vec<ModFocusOutcome>>>,
}

/// One thing the person did, handed to the mod that drew it. A move of the
/// focus ring leaves the mod's answer in `focus_outcomes`.
async fn hand_over(link: &ModsLink, act: ModAct, focus_outcomes: &Mutex<Vec<ModFocusOutcome>>) {
    let surface = ModUiSurface::Terminal;
    let (what, outcome) = match &act {
        ModAct::Focus {
            site,
            element,
            previous,
            by_plugin,
        } => {
            let origin = if *by_plugin {
                json!({ "kind": "plugin" })
            } else {
                json!({ "kind": "person" })
            };
            let answer = link
                .focus(
                    &site.plugin,
                    &site.component,
                    &site.request_id,
                    element.as_deref(),
                    origin,
                )
                .await;
            let outcome = answer.map(|answer| {
                focus_outcomes
                    .lock()
                    .expect("mods focus outcomes poisoned")
                    .push(ModFocusOutcome {
                        site: site.clone(),
                        asked: element.clone(),
                        previous: previous.clone(),
                        answer,
                    });
            });
            (element.clone().unwrap_or_default(), outcome)
        }
        ModAct::ClientKey { site, element, key } => (
            element.clone(),
            link.client_key(
                &site.plugin,
                &site.component,
                surface,
                &site.request_id,
                element,
                key.clone(),
            )
            .await,
        ),
        ModAct::ClientPointer {
            site,
            element,
            pointer,
        } => (
            element.clone(),
            link.client_pointer(
                &site.plugin,
                &site.component,
                surface,
                &site.request_id,
                element,
                pointer.clone(),
            )
            .await,
        ),
        ModAct::Press {
            site,
            element,
            href,
        } => (
            element.clone(),
            link.press(
                &site.plugin,
                &site.component,
                surface,
                &site.request_id,
                element,
                href.as_deref(),
            )
            .await,
        ),
        ModAct::Input {
            site,
            element,
            kind,
            value,
        } => (
            element.clone(),
            link.input(
                &site.plugin,
                &site.component,
                surface,
                &site.request_id,
                element,
                kind,
                value,
            )
            .await,
        ),
        ModAct::Select {
            site,
            element,
            value,
        } => (
            element.clone(),
            link.select(
                &site.plugin,
                &site.component,
                surface,
                &site.request_id,
                element,
                Value::String(value.clone()),
            )
            .await,
        ),
        ModAct::ClosePane { plugin, id } => (id.clone(), link.close_pane(plugin, id).await),
    };
    if let Err(error) = outcome {
        tracing::warn!(%error, element = %what, "mods: an act was not taken");
    }
}

/// One drawing that came back.
struct Drawn {
    request_id: String,
    version: u64,
    /// The mod whose hook drew it, for the band, which any mod may draw.
    plugin: Option<String>,
    answer: Result<ModRenderAnswer, String>,
}

/// Whether a mod's hooks include one a `ui.render` ask reaches.
fn renders(events: &[String]) -> bool {
    events
        .iter()
        .any(|pattern| rebon_types::pattern_selects(pattern, "ui.render"))
}

/// A hosted session's mods, as this terminal follows them.
struct RemoteMods {
    session_id: String,
    link: ModsLink,
    seen_version: Option<u64>,
    last_toast: u64,
    last_log: u64,
    has_mods: bool,
    /// The mods there that draw, by plugin id, in their order.
    renderers: Vec<String>,
    last_poll: Option<Instant>,
    /// The snapshot in flight, and where its answer lands.
    pending: Option<SnapshotSlot>,
    /// The worker's mod commands, registered here while it hosts the session.
    commands: LinkedModCommands,
}

/// Where a snapshot in flight leaves its answer for the next pass.
type SnapshotSlot = Arc<Mutex<Option<Result<ModsView, String>>>>;

/// A worker's owner as a [`ModsLink`]. The owner client blocks, so each
/// question runs on the blocking pool.
fn owner_link(owner: rebon_session_host::OwnerHandle) -> ModsLink {
    let ask = move |call: Value| -> BoxFuture<'static, Result<Value, String>> {
        let owner = owner.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                owner.mods(call).map_err(|error| format!("{error:#}"))
            })
            .await
            .map_err(|error| error.to_string())?
        })
    };
    ModsLink::Remote(Arc::new(ask))
}

/// The worker hosting this session, when one is.
fn hosted_owner(session: &TuiEngineSession) -> Option<rebon_session_host::OwnerHandle> {
    let attachment = session.remote_background_attachment.as_ref()?;
    Some(attachment.worker.as_ref()?.connection.owner().clone())
}

/// Registers the worker's mod commands here, and brings the `/` menu in line
/// with the seat: rows of commands the worker dropped go, new ones come in.
fn sync_linked_commands(
    app: &mut AppState,
    linked: &mut LinkedModCommands,
    rows: &[rebon_plugin_host::mods::ModCommandRow],
    link: &ModsLink,
    handle: &Handle,
) {
    let before: Vec<String> = linked.rows().iter().map(|row| row.name.clone()).collect();
    let kernel = rebon_harness::kernel_bootstrap::process_kernel();
    if linked.sync(kernel.context(), rows, link, ModUiSurface::Terminal, handle) {
        refresh_picker(app, &before);
    }
}

/// A worker this terminal no longer follows: its commands leave the seat and
/// the `/` menu.
fn forget_remote(app: &mut AppState, remote: RemoteMods) {
    let gone: Vec<String> = remote
        .commands
        .rows()
        .iter()
        .map(|row| row.name.clone())
        .collect();
    drop(remote);
    if !gone.is_empty() {
        refresh_picker(app, &gone);
    }
}

/// The `/` menu after linked commands came or went: the old rows out, then
/// whatever the seat lists now back in.
fn refresh_picker(app: &mut AppState, gone: &[String]) {
    app.slash_commands
        .retain(|command| !gone.contains(&command.name));
    super::commands::register_local_slash_commands(&mut app.slash_commands);
}

/// One pass: project the mods' table onto the terminal.
#[allow(clippy::too_many_arguments)]
pub(super) fn mods_tick(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    surface: &mut ModsSurfaceState,
    active_prompt: &mut Option<ActivePrompt>,
    pending_permission: &mut Option<PendingPermission>,
    ui_mode: UiMode,
) {
    if let Some(owner) = hosted_owner(session) {
        remote_tick(
            app,
            session,
            handle,
            surface,
            owner,
            active_prompt,
            pending_permission,
            ui_mode,
        );
        return;
    }
    if let Some(remote) = surface.remote.take() {
        // Back in this process (a worker that went away): the footer stops
        // showing the worker's status until this process's mods say something,
        // and the worker's commands leave the `/` menu.
        app.mods_status = None;
        surface.seen_version = 0;
        forget_remote(app, remote);
    }
    let Some(mods) = process_mods() else {
        return;
    };
    if !surface.attached {
        mods.attach_surface("terminal");
        surface.attached = true;
    }
    if surface.facts_told_for.as_deref() != Some(session.session_id.as_str()) {
        surface.facts_told_for = Some(session.session_id.clone());
        let facts = ModFacts {
            cwd: std::env::current_dir()
                .map(|cwd| cwd.to_string_lossy().into_owned())
                .unwrap_or_default(),
            session_id: Some(session.session_id.clone()),
            model: None,
            surface: Some("terminal".to_owned()),
        };
        for record in mods.mods() {
            let mods = mods.clone();
            let facts = facts.clone();
            handle.spawn(async move { mods.tell_facts(&record.id, &facts).await });
        }
    }
    let version = mods.ui.version();
    let link = ModsLink::Local(Arc::clone(&mods));
    let renderers: Vec<String> = mods
        .mods()
        .iter()
        .filter(|record| renders(&record.events))
        .map(|record| record.id.clone())
        .collect();
    let working = active_prompt.is_some();
    if version == surface.seen_version {
        draw_tick(
            app,
            handle,
            &mut surface.draw,
            &link,
            version,
            None,
            &renderers,
            working,
        );
        return;
    }
    let snapshot = mods.ui.snapshot();
    let names: std::collections::BTreeMap<String, String> = mods
        .mods()
        .iter()
        .map(|record| (record.id.clone(), record.name.clone()))
        .collect();
    // Taken only when queued: a take moves the version, and taking nothing
    // would make every pass look like a change.
    let fills = if snapshot.fills.is_empty() {
        Vec::new()
    } else {
        mods.ui.take_fills()
    };
    let focus_asks = if snapshot.focus.is_empty() {
        Vec::new()
    } else {
        mods.ui.take_focus()
    };
    let prompts = if snapshot.prompts.is_empty() {
        Vec::new()
    } else {
        mods.ui.take_prompts()
    };
    surface.seen_version = mods.ui.version();
    project(
        app,
        &snapshot,
        &names,
        (&mut surface.last_toast, &mut surface.last_log),
        false,
    );
    draw_tick(
        app,
        handle,
        &mut surface.draw,
        &link,
        surface.seen_version,
        Some(&snapshot.panes),
        &renderers,
        working,
    );
    for fill in fills {
        apply_fill(app, &fill.text, &fill.mode);
    }
    for ask in focus_asks {
        super::mods_keys::apply_focus_request(app, &ask.plugin, &ask.request_id, &ask.key);
    }
    for prompt in prompts {
        submit_mod_prompt(
            app,
            session,
            handle,
            name_of(&names, &prompt.plugin),
            prompt.text,
            active_prompt,
            pending_permission,
            ui_mode,
        );
    }
}

/// One pass for a hosted session: take the answer of the snapshot in
/// flight, if it came, and ask again when one is due.
#[allow(clippy::too_many_arguments)]
fn remote_tick(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    surface: &mut ModsSurfaceState,
    owner: rebon_session_host::OwnerHandle,
    active_prompt: &mut Option<ActivePrompt>,
    pending_permission: &mut Option<PendingPermission>,
    ui_mode: UiMode,
) {
    let session_id = owner.session_id.clone();
    if surface
        .remote
        .as_ref()
        .map_or(true, |remote| remote.session_id != session_id)
    {
        app.mods_status = None;
        let link = owner_link(owner);
        let facts = ModFacts {
            cwd: std::env::current_dir()
                .map(|cwd| cwd.to_string_lossy().into_owned())
                .unwrap_or_default(),
            session_id: Some(session_id.clone()),
            model: None,
            surface: Some(ModUiSurface::Terminal.as_str().to_owned()),
        };
        {
            let link = link.clone();
            handle.spawn(async move {
                if let Err(error) = link.facts(&facts).await {
                    tracing::debug!(%error, "mods: the worker did not take the session's facts");
                }
            });
        }
        if let Some(previous) = surface.remote.take() {
            forget_remote(app, previous);
        }
        surface.remote = Some(RemoteMods {
            session_id,
            link,
            seen_version: None,
            last_toast: 0,
            last_log: 0,
            has_mods: true,
            renderers: Vec::new(),
            last_poll: None,
            pending: None,
            commands: LinkedModCommands::default(),
        });
        surface.draw = DrawState::default();
        app.mods_band = None;
        app.mods_panes.clear();
        app.mods_focus = None;
    }
    let Some(remote) = surface.remote.as_mut() else {
        return;
    };
    let answered = remote
        .pending
        .as_ref()
        .and_then(|slot| slot.lock().ok()?.take());
    let mut changed_panes: Option<Vec<ModPane>> = None;
    if let Some(answer) = answered {
        remote.pending = None;
        match answer {
            Ok(view) => {
                let first_read = remote.seen_version.is_none();
                remote.seen_version = Some(view.version);
                if view.changed {
                    remote.has_mods = !view.mods.is_empty();
                    remote.renderers = view
                        .mods
                        .iter()
                        .filter(|row| {
                            let events: Vec<String> = row
                                .get("events")
                                .and_then(Value::as_array)
                                .map(|list| {
                                    list.iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_owned)
                                        .collect()
                                })
                                .unwrap_or_default();
                            renders(&events)
                        })
                        .filter_map(|row| Some(row.get("id")?.as_str()?.to_owned()))
                        .collect();
                    let names: std::collections::BTreeMap<String, String> = view
                        .mods
                        .iter()
                        .filter_map(|row| {
                            Some((
                                row.get("id")?.as_str()?.to_owned(),
                                row.get("name")?.as_str()?.to_owned(),
                            ))
                        })
                        .collect();
                    sync_linked_commands(
                        app,
                        &mut remote.commands,
                        &view.commands,
                        &remote.link,
                        handle,
                    );
                    let snapshot = view.snapshot.unwrap_or_default();
                    changed_panes = Some(snapshot.panes.clone());
                    project(
                        app,
                        &snapshot,
                        &names,
                        (&mut remote.last_toast, &mut remote.last_log),
                        first_read,
                    );
                    for fill in view.fills {
                        apply_fill(app, &fill.text, &fill.mode);
                    }
                    for ask in view.focus {
                        super::mods_keys::apply_focus_request(
                            app,
                            &ask.plugin,
                            &ask.request_id,
                            &ask.key,
                        );
                    }
                    for prompt in view.prompts {
                        let plugin = name_of(&names, &prompt.plugin);
                        submit_mod_prompt(
                            app,
                            session,
                            handle,
                            plugin,
                            prompt.text,
                            active_prompt,
                            pending_permission,
                            ui_mode,
                        );
                    }
                }
            }
            Err(error) => {
                tracing::debug!(%error, "mods: the worker did not answer a snapshot");
                // Read from scratch next time: the worker may be a new one.
                remote.seen_version = None;
            }
        }
    }
    let Some(remote) = surface.remote.as_mut() else {
        return;
    };
    let link = remote.link.clone();
    let renderers = remote.renderers.clone();
    let version = remote.seen_version.unwrap_or(0);
    draw_tick(
        app,
        handle,
        &mut surface.draw,
        &link,
        version,
        changed_panes.as_deref(),
        &renderers,
        active_prompt.is_some(),
    );
    let Some(remote) = surface.remote.as_mut() else {
        return;
    };
    let every = if remote.has_mods {
        REMOTE_POLL
    } else {
        REMOTE_POLL_IDLE
    };
    let due = remote
        .last_poll
        .map_or(true, |last| last.elapsed() >= every);
    if remote.pending.is_none() && due {
        remote.last_poll = Some(Instant::now());
        let slot = Arc::new(Mutex::new(None));
        remote.pending = Some(Arc::clone(&slot));
        let link = remote.link.clone();
        let since = remote.seen_version;
        handle.spawn(async move {
            let answer = link.snapshot(since, true, ModUiSurface::Terminal).await;
            if let Ok(mut slot) = slot.lock() {
                *slot = Some(answer);
            }
        });
    }
}

/// The footer, the new toasts and the new transcript log lines.
///
/// On a first read of a table that was already running — a terminal
/// attaching to a worker — the toasts and lines it kept from before are
/// counted as seen rather than replayed.
fn project(
    app: &mut AppState,
    snapshot: &ModUiSnapshot,
    names: &std::collections::BTreeMap<String, String>,
    (last_toast, last_log): (&mut u64, &mut u64),
    first_read: bool,
) {
    app.mods_status = (!snapshot.status.is_empty()).then(|| {
        snapshot
            .status
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join(" · ")
    });
    for toast in &snapshot.toasts {
        if toast.id <= *last_toast {
            continue;
        }
        *last_toast = toast.id;
        if first_read {
            continue;
        }
        let level = match toast.kind.as_deref() {
            Some("error") => rebon_tui::SystemLevel::Error,
            Some("warning") => rebon_tui::SystemLevel::Warning,
            _ => rebon_tui::SystemLevel::Info,
        };
        inject_mod_notice(
            app,
            level,
            &format!("toast-{}", toast.id),
            &format!("{}: {}", name_of(names, &toast.plugin), toast.text),
        );
    }
    for line in &snapshot.log {
        if line.id <= *last_log {
            continue;
        }
        *last_log = line.id;
        if !first_read && line.to == "transcript" {
            inject_mod_notice(
                app,
                rebon_tui::SystemLevel::Info,
                &format!("log-{}", line.id),
                &line.text,
            );
        }
    }
}

fn name_of(names: &std::collections::BTreeMap<String, String>, plugin: &str) -> String {
    names
        .get(plugin)
        .cloned()
        .unwrap_or_else(|| plugin.to_owned())
}

/// `$.prompt.fill`: into the input, by mode.
fn apply_fill(app: &mut AppState, text: &str, mode: &str) {
    match mode {
        "replace" => app.input = text.to_owned(),
        _ => app.input.push_str(text),
    }
}

/// `$.prompt.submit`: said, then submitted the way a typed line is.
#[allow(clippy::too_many_arguments)]
fn submit_mod_prompt(
    app: &mut AppState,
    session: &mut TuiEngineSession,
    handle: &Handle,
    plugin: String,
    text: String,
    active_prompt: &mut Option<ActivePrompt>,
    pending_permission: &mut Option<PendingPermission>,
    ui_mode: UiMode,
) {
    inject_mod_notice(
        app,
        rebon_tui::SystemLevel::Info,
        "prompt",
        &format!("{plugin} submitted a prompt"),
    );
    submit_or_queue(
        app,
        text,
        session,
        handle,
        active_prompt,
        pending_permission,
        ui_mode,
    );
}

/// The drawings: take the answers that came back, hand the person's presses
/// to the mods, and ask for what moved — the band when the table, the turn
/// or the width did, a pane when its version did.
#[allow(clippy::too_many_arguments)]
fn draw_tick(
    app: &mut AppState,
    handle: &Handle,
    draw: &mut DrawState,
    link: &ModsLink,
    version: u64,
    panes: Option<&[ModPane]>,
    renderers: &[String],
    working: bool,
) {
    super::mods_keys::settle_focus(app);
    if let Some(panes) = panes {
        // A pane first seen here that was opened with `focus` takes the
        // keyboard once the frame has drawn it.
        if let Some(opened) = panes
            .iter()
            .find(|pane| pane.focus && !app.mods_panes.iter().any(|held| held.id == pane.id))
        {
            app.mods_focus_wanted = Some(ModSite {
                plugin: opened.plugin.clone(),
                component: "Pane".to_owned(),
                request_id: opened.id.clone(),
            });
        }
        app.mods_panes = panes
            .iter()
            .map(|pane| {
                let held = app.mods_panes.iter().find(|held| held.id == pane.id);
                TerminalModPane {
                    id: pane.id.clone(),
                    plugin: pane.plugin.clone(),
                    plugin_name: pane.plugin_name.clone(),
                    title: pane.title.clone(),
                    unasked: pane.unasked,
                    close_on_escape: pane.close_on_escape,
                    version: pane.version,
                    drawn_version: held.map_or(0, |held| held.drawn_version),
                    tree: held.and_then(|held| held.tree.clone()),
                    error: held.and_then(|held| held.error.clone()),
                }
            })
            .collect();
    }

    let answers = draw
        .answers
        .lock()
        .map(|mut answers| std::mem::take(&mut *answers))
        .unwrap_or_default();
    for drawn in answers {
        draw.in_flight.remove(&drawn.request_id);
        if drawn.request_id == BAND {
            app.mods_band = match (drawn.plugin, drawn.answer) {
                (Some(plugin), Ok(ModRenderAnswer::Tree(tree))) => Some((plugin, tree)),
                _ => None,
            };
            continue;
        }
        if let Some(pane) = app
            .mods_panes
            .iter_mut()
            .find(|pane| pane.id == drawn.request_id)
        {
            pane.drawn_version = drawn.version;
            match drawn.answer {
                Ok(ModRenderAnswer::Tree(tree)) => {
                    pane.tree = Some(tree);
                    pane.error = None;
                }
                Ok(ModRenderAnswer::Engine) => {
                    pane.tree = None;
                    pane.error = None;
                }
                Err(error) => pane.error = Some(error),
            }
        }
    }

    let outcomes = std::mem::take(
        &mut *draw
            .focus_outcomes
            .lock()
            .expect("mods focus outcomes poisoned"),
    );
    for outcome in outcomes {
        super::mods_keys::apply_focus_outcome(app, outcome);
    }

    // In the order the person made them, and each batch behind the last: a
    // field's last `change` reaches the mod before its `submit`.
    let acts = std::mem::take(&mut app.mods_acts);
    if !acts.is_empty() {
        let link = link.clone();
        let before = draw.acts_tail.take();
        let focus_outcomes = Arc::clone(&draw.focus_outcomes);
        draw.acts_tail = Some(handle.spawn(async move {
            if let Some(before) = before {
                let _ = before.await;
            }
            for act in acts {
                hand_over(&link, act, &focus_outcomes).await;
            }
        }));
    }

    let columns = ratatui::crossterm::terminal::size()
        .map(|(columns, _)| columns)
        .unwrap_or(80);
    if renderers.is_empty() {
        app.mods_band = None;
        draw.band_for = None;
    } else {
        let wanted = (version, working, columns);
        if draw.band_for != Some(wanted) && !draw.in_flight.contains(BAND) {
            draw.band_for = Some(wanted);
            draw.in_flight.insert(BAND.to_owned());
            let link = link.clone();
            let renderers = renderers.to_vec();
            let answers = Arc::clone(&draw.answers);
            let rows = ratatui::crossterm::terminal::size()
                .map(|(_, rows)| rows)
                .unwrap_or(24);
            let props = json!({
                "hasSurvey": false,
                "isWorking": working,
                "maxRows": (rows / 4).max(1),
                "bodyColumns": columns.saturating_sub(5),
                "scroll": { "offset": 0, "bodyRows": (rows / 4).max(1) },
            });
            handle.spawn(async move {
                // The band is one instance every mod may draw: the first
                // that answers with a tree has it, in the mods' order.
                let mut drawn = Drawn {
                    request_id: BAND.to_owned(),
                    version,
                    plugin: None,
                    answer: Ok(ModRenderAnswer::Engine),
                };
                for plugin in renderers {
                    let answer = link
                        .render(
                            &plugin,
                            "AbovePrompt",
                            ModUiSurface::Terminal,
                            BAND,
                            props.clone(),
                            Some((columns.into(), rows.into())),
                        )
                        .await;
                    if matches!(answer, Ok(ModRenderAnswer::Tree(_))) {
                        drawn.plugin = Some(plugin);
                        drawn.answer = answer;
                        break;
                    }
                }
                if let Ok(mut answers) = answers.lock() {
                    answers.push(drawn);
                }
            });
        }
    }

    let body_rows = ratatui::crossterm::terminal::size()
        .map(|(_, rows)| rows.saturating_sub(8))
        .unwrap_or(16);
    let wanted: Vec<(String, String, u64)> = app
        .mods_panes
        .iter()
        .filter(|pane| pane.version > pane.drawn_version && !draw.in_flight.contains(&pane.id))
        .map(|pane| (pane.plugin.clone(), pane.id.clone(), pane.version))
        .collect();
    for (plugin, id, pane_version) in wanted {
        draw.in_flight.insert(id.clone());
        let link = link.clone();
        let answers = Arc::clone(&draw.answers);
        let body_columns = super::render::PANE_BODY_COLUMNS;
        handle.spawn(async move {
            let answer = link
                .render(
                    &plugin,
                    "Pane",
                    ModUiSurface::Terminal,
                    &id,
                    json!({
                        "bodyColumns": body_columns,
                        "isFullscreen": false,
                        "scroll": { "offset": 0, "bodyRows": body_rows },
                    }),
                    Some((body_columns.into(), body_rows.into())),
                )
                .await;
            if let Ok(mut answers) = answers.lock() {
                answers.push(Drawn {
                    request_id: id,
                    version: pane_version,
                    plugin: Some(plugin),
                    answer,
                });
            }
        });
    }
}
