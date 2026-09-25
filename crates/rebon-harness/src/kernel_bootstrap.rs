//! Seed of the plugin kernel inside the session assembly path.
//!
//! `build_headless_session` boots (or reuses) one process-wide [`Kernel`]
//! before assembling a session, making the harness the kernel's first real
//! consumer. The assembly is the plugin list in [`builtin_plugin_defs`]: the
//! Core seats the kernel boots with, then the feature plugins, one per
//! capability.
//!
//! The `logger` service registers on both planes: the typed interface for
//! Rust consumers and a JSON facade, which is the ABI the embedded JS
//! plugin plane consumes.

use std::sync::{Arc, OnceLock};

use rebon_kernel::{
    ConfigChanged, ConfigFileKind, Context, DesiredSet, JsonService, Kernel, KernelError, Plugin,
    PluginDef, PluginHost, PluginKind, PluginMeta, PluginRegistry, Service,
};

/// Typed plane of the `logger` service.
pub trait KernelLog: Send + Sync {
    fn log(&self, level: &str, message: &str);
}

/// Service definition for the kernel-wide logger seam.
pub struct LoggerService;

impl Service for LoggerService {
    type Interface = dyn KernelLog;
    const NAME: &'static str = "logger";
}

/// Tracing-backed provider; serves both planes.
struct TracingLogger;

impl KernelLog for TracingLogger {
    fn log(&self, level: &str, message: &str) {
        self.log_from("unknown", level, message);
    }
}

impl TracingLogger {
    /// The same line, attributed.
    ///
    /// `plugin` is a field rather than part of the message so a reader can
    /// filter by it. Which plugin spoke is the first thing anyone wants to know
    /// and the last thing a prefix inside the text is good for.
    ///
    /// `unknown` is used where the caller is genuinely not known, and is
    /// deliberately not left blank: a line whose owner could not be determined
    /// is a different fact from a line nobody bothered to attribute, and only
    /// one of them is worth chasing.
    fn log_from(&self, plugin: &str, level: &str, message: &str) {
        match level {
            "trace" => tracing::trace!(target: "kernel.plugin", plugin, "{message}"),
            "debug" => tracing::debug!(target: "kernel.plugin", plugin, "{message}"),
            "warn" => tracing::warn!(target: "kernel.plugin", plugin, "{message}"),
            "error" => tracing::error!(target: "kernel.plugin", plugin, "{message}"),
            _ => tracing::info!(target: "kernel.plugin", plugin, "{message}"),
        }
    }
}

impl JsonService for TracingLogger {
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, KernelError> {
        let message = params
            .get("message")
            .and_then(|m| m.as_str())
            .or_else(|| params.as_str())
            .unwrap_or_default();
        // The seat dispatcher writes this over whatever the plugin sent, so it
        // is the host's answer to "who is calling" rather than the plugin's
        // claim about itself.
        let plugin = params
            .get(rebon_kernel_seats::kernel_config_seats::CALLER_PLUGIN_ID)
            .and_then(|id| id.as_str())
            .unwrap_or("unknown");
        self.log_from(plugin, method, message);
        Ok(serde_json::Value::Null)
    }
}

/// The kernel's own logger: the first Core plugin, and the only one that
/// exists solely so the JSON plane has somewhere to write.
struct LoggerPlugin;

impl Plugin for LoggerPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("logger").provides(&["logger"])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let logger = Arc::new(TracingLogger);
        ctx.provide_dual::<LoggerService>(logger.clone(), logger)
    }
}

fn make_logger(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(LoggerPlugin))
}

fn make_model_router(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(
        rebon_provider::kernel_model_router::ModelRouterPlugin,
    ))
}

fn make_config_seats(host: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(
        rebon_kernel_seats::kernel_config_seats::ConfigSeatsPlugin::new(host.config_dir.clone()),
    ))
}

fn make_core_commands(host: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(
        rebon_kernel_seats::kernel_core_commands::CoreCommandsPlugin::new(host.kernel.clone()),
    ))
}

fn make_core_config_options(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(
        rebon_kernel_seats::kernel_config_options::CoreConfigOptionsPlugin,
    ))
}

fn make_core_tools(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(
        rebon_kernel_seats::kernel_core_tools::CoreToolsPlugin,
    ))
}

fn make_core_ui(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(rebon_kernel_seats::kernel_core_ui::CoreUiPlugin))
}

/// Every plugin this binary can load, in one place: the Core seats the
/// kernel boots with, then the feature plugins, each behind
/// `plugins.<id>.enabled`.
pub fn builtin_plugin_defs() -> &'static [PluginDef] {
    static DEFS: &[PluginDef] = &[
        PluginDef {
            id: "logger",
            title: "Kernel logger",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_logger,
        },
        PluginDef {
            id: "model-router",
            title: "Model router seat",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_model_router,
        },
        PluginDef {
            id: "config-seats",
            title: "Settings and credentials seats",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_config_seats,
        },
        PluginDef {
            id: rebon_kernel_seats::kernel_core_commands::PLUGIN_ID,
            title: "Built-in slash commands",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_core_commands,
        },
        PluginDef {
            id: rebon_kernel_seats::kernel_core_tools::CORE_TOOLS_PLUGIN_ID,
            title: "Core tools (Read, Write, Edit, Glob, Grep, shell, …)",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_core_tools,
        },
        PluginDef {
            id: rebon_kernel_seats::kernel_core_ui::PLUGIN_ID,
            title: "Dialog registry seat",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_core_ui,
        },
        PluginDef {
            id: rebon_kernel_seats::kernel_config_options::PLUGIN_ID,
            title: "Settings option seat",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_core_config_options,
        },
        rebon_kernel_seats::kernel_code_mode::PLUGIN,
        rebon_plugin_cron::PLUGIN,
        rebon_plugin_web::PLUGIN,
        rebon_plugin_monitor::PLUGIN,
        rebon_plugin_memory::PLUGIN,
        rebon_plugin_notebook::PLUGIN,
        rebon_plugin_structured_output::PLUGIN,
        rebon_plugin_profile::PLUGIN,
        rebon_plugin_computer_use::PLUGIN,
        rebon_plugin_image_gen::PLUGIN,
        rebon_plugin_skill::PLUGIN,
        rebon_plugin_escalation::PLUGIN,
        rebon_plugin_mcp::PLUGIN,
        rebon_plugin_tasks::PLUGIN,
        rebon_plugin_plan_mode::PLUGIN,
        rebon_plugin_agents::PLUGIN,
        rebon_plugin_workflow::PLUGIN,
        rebon_plugin_model_prompt::PLUGIN,
        rebon_plugin_model_routing::PLUGIN,
        rebon_plugin_updater::PLUGIN,
        rebon_plugin_remote::PLUGIN,
        rebon_plugin_onboarding::PLUGIN,
        rebon_plugin_sandbox::PLUGIN,
        rebon_plugin_host::kernel_node_host::PLUGIN,
    ];
    DEFS
}

/// The switches the settings chain (user, project, local — the same files
/// every other setting is read from) carries under `plugins.<id>.enabled`, as
/// a desired set. Unlisted plugins keep their definition's default.
pub fn desired_from_settings() -> DesiredSet {
    let mut desired = desired_from_switches(rebon_config::saved_plugin_switches());
    apply_legacy_switches(&mut desired, &|name| std::env::var(name).ok());
    desired
}

fn desired_from_switches(switches: std::collections::BTreeMap<String, bool>) -> DesiredSet {
    let mut desired = DesiredSet::new();
    for (id, enabled) in switches {
        desired.set(&id, Some(enabled));
    }
    desired
}

/// The switches that predate `plugins.<id>.enabled`, mapped onto the plugin
/// they now name. Read-only and one-way: an old kill switch that is set
/// turns its plugin off, and one that is absent changes nothing, so a
/// `plugins.<id>.enabled` written by the settings UI still wins by default.
///
/// | old switch | plugin |
/// |---|---|
/// | `REBON_DISABLE_CRON` | `cron` |
///
/// Delete this table once the settings UI is the only writer.
fn apply_legacy_switches(desired: &mut DesiredSet, env: &dyn Fn(&str) -> Option<String>) {
    const KILL_SWITCHES: &[(&str, &str)] = &[("REBON_DISABLE_CRON", rebon_plugin_cron::PLUGIN_ID)];
    for (variable, plugin) in KILL_SWITCHES {
        let Some(value) = env(variable) else {
            continue;
        };
        if matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES") {
            desired.set(plugin, Some(false));
        }
    }
}

/// Which of the plugins the settings switch on an entry point runs with.
///
/// `plugins.<id>.enabled` lives in one file that every surface reads, so it
/// cannot say "on in my terminal, but not for the script that calls me". This
/// is where an entry point says that for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PluginSurface {
    /// Whatever the settings say. The terminal, `serve`, the background
    /// host the desktop app runs its chats on.
    #[default]
    Configured,
    /// An entry point driven by a caller outside Rebon — `rebon exec` (evals,
    /// scripts, the desktop app's inline rewrite) and the `--acp` server an
    /// editor drives. The caller named a model and expects that model and the
    /// ordinary tool set; plugins that re-pick the model or reshape the tool
    /// set from the user's own settings would change the run under it.
    Plain,
}

impl PluginSurface {
    /// The plugins this surface keeps unloaded, whatever the switches say.
    ///
    /// `model-routing` re-picks the provider, model and effort from the first
    /// prompt, for the session and for every sub-agent. `code-mode` puts
    /// `run_code` in front of the model (with `defaultOn`, before anyone asked
    /// for it). Unloading them is the whole switch: the executor, the
    /// sub-agent spawner and `run_code` each look for their plugin on the
    /// kernel and do nothing without it.
    pub fn withheld(self) -> &'static [&'static str] {
        match self {
            Self::Configured => &[],
            Self::Plain => &[
                rebon_plugin_model_routing::PLUGIN_ID,
                rebon_kernel_seats::kernel_code_mode::PLUGIN_ID,
            ],
        }
    }
}

static DECLARED_PLUGIN_SURFACE: OnceLock<PluginSurface> = OnceLock::new();

/// Declare which plugins this process runs with. Once per process, by the
/// entry point, before anything boots the kernel.
///
/// Declared late, it still holds: whatever it withholds is unloaded from the
/// live registry at once, and sessions already built lose it with the
/// plugin. A second, different declaration is ignored with a warning —
/// withholding is not undone, and a process that serves one surface serves
/// no other.
pub fn declare_plugin_surface(surface: PluginSurface) {
    if DECLARED_PLUGIN_SURFACE.set(surface).is_err() {
        let declared = declared_plugin_surface();
        if declared != surface {
            tracing::warn!(
                ?declared,
                ?surface,
                "kernel: plugin surface already declared"
            );
        }
        return;
    }
    if let Some(registry) = rebon_kernel::process_registry() {
        withhold_for(&registry, surface);
    }
}

/// The surface this process declared, [`PluginSurface::Configured`] if none.
pub fn declared_plugin_surface() -> PluginSurface {
    DECLARED_PLUGIN_SURFACE.get().copied().unwrap_or_default()
}

fn withhold_for(registry: &PluginRegistry, surface: PluginSurface) {
    let withheld = surface.withheld();
    if withheld.is_empty() {
        return;
    }
    let report = registry
        .withhold(withheld)
        .expect("a surface withholds only built-in feature plugins");
    tracing::info!(?surface, ?withheld, unloaded = ?report.unloaded, "kernel: plugins withheld");
}

/// The process-wide registry over [`builtin_plugin_defs`]. Booting it is
/// what [`process_kernel`] does; `/plugins`, the settings switches and
/// `/kernel reload` all talk to this one instance.
///
/// **The only thing in this build that boots a kernel.** Booting needs
/// [`builtin_plugin_defs`], which names every plugin crate, so it can only
/// live here; the *slot* the result goes into lives under everything, in
/// [`rebon_kernel::process`]. Everything below the assembly layer reads
/// `rebon_kernel::process_registry()` or is handed a kernel by its caller —
/// see the invariant beside `pub use rebon_kernel` in `lib.rs`.
pub fn process_plugin_registry() -> Arc<PluginRegistry> {
    static REGISTRY: OnceLock<Arc<PluginRegistry>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let kernel = Kernel::new();
            let config_dir = rebon_config::config_home_dir();
            let host = PluginHost {
                kernel: kernel.clone(),
                config_dir,
            };
            let registry = PluginRegistry::new(kernel.clone(), builtin_plugin_defs(), host);
            // Before the first reconcile, so a withheld plugin never loads
            // even for a moment.
            withhold_for(&registry, declared_plugin_surface());
            let report = registry.reconcile(&desired_from_settings());
            let core_failures: Vec<&(String, String)> = report
                .failed
                .iter()
                .filter(|(id, _)| {
                    registry
                        .def(id)
                        .map(|def| def.kind == PluginKind::Core)
                        .unwrap_or(false)
                })
                .collect();
            assert!(
                core_failures.is_empty(),
                "kernel: the builtin Core plugins must always load: {core_failures:?}"
            );
            if !report.failed.is_empty() {
                tracing::warn!(failed = ?report.failed, "kernel: some feature plugins did not load");
            }
            // A settings write reconciles the switches at once (RFC
            // kernel-plugins §7, path A). Weak: the registry owns the kernel
            // that owns the root context this listener lives in.
            let switches = Arc::downgrade(&registry);
            kernel.context().on::<ConfigChanged>(move |event| {
                if event.kind != ConfigFileKind::Settings {
                    return;
                }
                // A write through the settings seat names the one namespace it
                // touched, and the seat refuses `enabled` — so a named
                // namespace is a write that cannot have moved a switch. The
                // reconcile is for the writes that say nothing.
                if event.namespace.is_some() {
                    return;
                }
                let Some(registry) = switches.upgrade() else {
                    return;
                };
                let report = registry.reconcile(&desired_from_settings());
                if !report.loaded.is_empty()
                    || !report.unloaded.is_empty()
                    || !report.failed.is_empty()
                {
                    tracing::info!(
                        loaded = ?report.loaded,
                        unloaded = ?report.unloaded,
                        failed = ?report.failed,
                        "kernel: plugin switches reconciled from settings"
                    );
                }
                // `node-host` may be what just moved, and the external rows
                // exist only while it is loaded. Re-deriving them here is what
                // makes switching the host off and on again put the packages
                // back rather than leaving an empty list behind.
                rebon_plugin_host::kernel_node_host::mark_external_defs_stale();
                rebon_plugin_host::kernel_node_host::refresh_external_defs(&registry);
            });
            // Config writes become kernel events. One observer per process;
            // a second caller (tests booting their own kernel) just loses.
            let events = kernel.clone();
            rebon_config::install_config_change_observer(move |kind, path, namespace| {
                let kind = match kind {
                    rebon_config::ConfigFileKind::Config => ConfigFileKind::Config,
                    rebon_config::ConfigFileKind::Settings => ConfigFileKind::Settings,
                    rebon_config::ConfigFileKind::Credentials => ConfigFileKind::Credentials,
                };
                events.context().emit(&ConfigChanged {
                    kind,
                    path: path.to_path_buf(),
                    namespace: namespace.map(str::to_string),
                });
            });
            // Last, and only once everything above stood up: the slot's `Some`
            // is what every crate under this one reads as "the process has
            // booted", so it must not be visible while the boot is still
            // deciding what loaded. Failure means a second boot raced this
            // one, which the `OnceLock` above already ruled out.
            if rebon_kernel::install_process_registry(registry.clone()).is_err() {
                tracing::warn!("kernel: the process registry slot was already taken");
            }
            tracing::info!(plugins = ?kernel.plugin_names(), "kernel booted");
            registry
        })
        .clone()
}

/// The one kernel this process runs. Booted on first use through the
/// plugin registry; see [`process_plugin_registry`].
pub fn process_kernel() -> Arc<Kernel> {
    process_plugin_registry().kernel().clone()
}

/// The skills this process's compiled plugins ship, for a front end building
/// a skill index (`SkillLoaderConfig::skill_bundles`). Boots the kernel, the
/// same as [`process_kernel`].
pub fn skill_bundles() -> Vec<rebon_core::skill_seat::SkillBundle> {
    rebon_kernel_seats::kernel_core_tools::process_skill_bundles(&process_kernel())
}

/// What one loaded plugin registered at run time: its services and its slash
/// commands, in that order.
///
/// A different question from [`rebon_kernel::PluginStatus::provides`], which
/// is the kernel services a plugin's `meta()` declares before it ever runs. An
/// external plugin declares none of those and cannot: `meta()` is asked before
/// `plugin/load` has told anyone what the plugin registers. Without this, a
/// `node:*` row reads as contributing nothing at all.
pub type RuntimeContributions<'a> = &'a dyn Fn(&str) -> (Vec<String>, Vec<String>);

/// The answer when there is no plane to ask.
pub fn no_runtime_contributions(_plugin_id: &str) -> (Vec<String>, Vec<String>) {
    (Vec::new(), Vec::new())
}

/// One line per plugin — what `/kernel plugins` and `/plugin list` print.
pub fn render_plugin_registry_snapshot(
    snapshot: &[rebon_kernel::PluginStatus],
    runtime: RuntimeContributions<'_>,
) -> String {
    use rebon_kernel::{PluginKind, PluginState};
    let mut out = String::from("Kernel plugins:\n");
    for status in snapshot {
        let kind = match status.kind {
            PluginKind::Core => "core",
            PluginKind::Feature => "feature",
            PluginKind::External => "external",
        };
        let state = match &status.state {
            PluginState::Loaded => "loaded".to_string(),
            PluginState::Disabled => "disabled".to_string(),
            PluginState::Failed(message) => format!("FAILED: {message}"),
        };
        out.push_str(&format!("  {:<16} {:<8} {}", status.id, kind, state));
        if !status.provides.is_empty() {
            out.push_str(&format!("  provides {}", status.provides.join(", ")));
        }
        if !status.dependents.is_empty() {
            out.push_str(&format!("  needed by {}", status.dependents.join(", ")));
        }
        let (services, commands) = runtime(&status.id);
        if !services.is_empty() {
            out.push_str(&format!("  serves {}", services.join(", ")));
        }
        if !commands.is_empty() {
            out.push_str(&format!("  commands {}", commands.join(", ")));
        }
        out.push('\n');
    }
    out.push_str(
        "Use /kernel enable|disable <id> to flip a feature switch, /kernel reload <id> to re-apply one; core plugins cannot be disabled.",
    );
    out
}

/// What one `enable` / `disable` / `reload` did, for a terminal or a log line.
pub fn render_reconcile_report(
    verb: &str,
    id: &str,
    report: &rebon_kernel::ReconcileReport,
) -> String {
    let mut lines = vec![format!("{verb} {id} — generation {}.", report.generation)];
    for loaded in &report.loaded {
        lines.push(format!("  + {loaded}  loaded"));
    }
    for unloaded in &report.unloaded {
        lines.push(format!("  - {unloaded}  unloaded"));
    }
    for (disabled, dependents) in &report.cascaded {
        lines.push(format!(
            "  ~ {disabled} took {} with it",
            dependents.join(", ")
        ));
    }
    for (failed, message) in &report.failed {
        lines.push(format!("  ! {failed}  {message}"));
    }
    if lines.len() == 1 {
        lines.push("  nothing changed".to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The half of a plugin's log line that lives on this side, pinned.
    ///
    /// The out-of-call log route rests on "route what a plugin says on its own
    /// schedule into the path a plugin's in-call line already takes". That
    /// premise was read out of the code and never measured, and three rounds
    /// of work were lost to assuming something about this plumbing, so it is
    /// measured here before anything is built on it.
    ///
    /// What this pins is the Rust half: a `logger` seat call becomes a
    /// `tracing` event on the `kernel.plugin` target, at the level the caller
    /// asked for. The Node half — that `bridge.mjs` reaches this seat when a
    /// call is in flight — is pinned in `runtimes/node/compose-runtime`.
    #[test]
    fn a_logger_seat_call_becomes_a_tracing_event_on_the_plugin_target() {
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};
        use tracing::span::{Attributes, Record};
        use tracing::{Event, Id, Metadata, Subscriber};

        #[derive(Default)]
        struct Seen {
            lines: Vec<(String, String, String, String)>, // target, level, message, plugin
        }

        struct Capture(Arc<Mutex<Seen>>);

        #[derive(Default)]
        struct Fields {
            message: String,
            plugin: String,
        }
        impl Visit for Fields {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                match field.name() {
                    "message" => self.message = format!("{value:?}"),
                    "plugin" => self.plugin = format!("{value:?}"),
                    _ => {}
                }
            }
            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "plugin" {
                    self.plugin = value.to_string();
                }
            }
        }

        impl Subscriber for Capture {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &Attributes<'_>) -> Id {
                Id::from_u64(1)
            }
            fn record(&self, _: &Id, _: &Record<'_>) {}
            fn record_follows_from(&self, _: &Id, _: &Id) {}
            fn event(&self, event: &Event<'_>) {
                let mut fields = Fields::default();
                event.record(&mut fields);
                let meta = event.metadata();
                self.0.lock().expect("capture").lines.push((
                    meta.target().to_string(),
                    meta.level().to_string(),
                    fields.message,
                    fields.plugin,
                ));
            }
            fn enter(&self, _: &Id) {}
            fn exit(&self, _: &Id) {}
        }

        let seen = Arc::new(Mutex::new(Seen::default()));
        let logger = TracingLogger;
        tracing::subscriber::with_default(Capture(seen.clone()), || {
            for level in ["trace", "debug", "info", "warn", "error"] {
                JsonService::call(
                    &logger,
                    level,
                    serde_json::json!({
                        "message": format!("said at {level}"),
                        rebon_kernel_seats::kernel_config_seats::CALLER_PLUGIN_ID: "ask-probe",
                    }),
                )
                .expect("the logger seat answers");
            }
            // A bare string, which is the other shape the seat accepts.
            JsonService::call(&logger, "info", serde_json::json!("bare"))
                .expect("the logger seat answers");
        });

        let lines = &seen.lock().expect("capture").lines;
        assert_eq!(lines.len(), 6, "every seat call emits exactly one event");
        for (target, _, _, _) in lines {
            assert_eq!(
                target, "kernel.plugin",
                "a plugin's line has to be filterable as one"
            );
        }
        let levels: Vec<&str> = lines.iter().map(|(_, level, ..)| level.as_str()).collect();
        assert_eq!(
            levels,
            vec!["TRACE", "DEBUG", "INFO", "WARN", "ERROR", "INFO"],
            "the level the plugin asked for is the level that is emitted; an \
             unknown one would fall to INFO, which the last line also checks"
        );
        assert!(
            lines[4].2.contains("said at error"),
            "the plugin's own words survive: {:?}",
            lines[4].2
        );
        assert!(
            lines[5].2.contains("bare"),
            "a bare string is a message too: {:?}",
            lines[5].2
        );
        for line in &lines[..5] {
            assert_eq!(
                line.3, "ask-probe",
                "the line says which plugin spoke, as a field"
            );
        }
        assert_eq!(
            lines[5].3, "unknown",
            "a call with no caller id is attributed to nobody, on purpose"
        );
    }

    #[test]
    fn settings_switches_become_desired_overrides_and_never_touch_core() {
        let switches = std::collections::BTreeMap::from([
            ("cron".to_string(), false),
            ("memory".to_string(), true),
            ("core-tools".to_string(), false),
        ]);
        let desired = desired_from_switches(switches);
        assert_eq!(desired.override_for("cron"), Some(false));
        assert_eq!(desired.override_for("memory"), Some(true));
        assert_eq!(desired.override_for("nope"), None);
        // A Core definition ignores its switch: the registry, not the file,
        // decides what cannot be turned off.
        let core = builtin_plugin_defs()
            .iter()
            .find(|def| def.id == "core-tools")
            .expect("core-tools is built in");
        assert!(desired.wants(core), "core stays wanted despite the switch");
    }

    /// The `turn-hooks` seat has to be on the kernel, not built per
    /// executor, or a plugin's subscriber is registered somewhere no turn
    /// looks. `skill` is the one that registers there today, so booting the
    /// real plugin table is what says the wiring holds end to end.
    #[test]
    fn the_skill_plugin_reaches_the_turn_hook_seat_the_engine_will_read() {
        let kernel = process_kernel();
        let seat = kernel
            .context()
            .get::<rebon_core::turn_hook::TurnHookSeatService>()
            .expect("core-tools provides the turn-hooks seat on the root");
        assert!(
            seat.subscriber_ids()
                .iter()
                .any(|id| id == rebon_plugin_skill::PROGRESSIVE_SKILL_DISCOVERY_HOOK_ID),
            "skill's discovery subscriber is on the seat: {:?}",
            seat.subscriber_ids()
        );
    }

    /// The two things every front end asks the real plugin table about
    /// updates: the seat it polls for the startup check, and `/update` on the
    /// command seat. Both are the `updater` plugin's, and both are resolved
    /// from the kernel root — the terminal's drain and `/status` look them up
    /// exactly this way, so a registration that only worked on a hand-built
    /// kernel would show up here and nowhere else.
    #[test]
    fn the_updater_plugin_reaches_the_seats_the_front_ends_read() {
        let kernel = process_kernel();
        assert!(
            rebon_plugin_updater::update_check_seat(kernel.context()).is_some(),
            "the loaded updater plugin provides its update-check seat"
        );
        let commands = rebon_kernel_seats::kernel_core_commands::command_seat()
            .expect("core-commands is a Core plugin and always loads");
        let update = commands
            .find("update")
            .expect("/update is registered by the updater plugin");
        assert_eq!(update.owner, rebon_plugin_updater::PLUGIN_ID);
    }

    /// The same two questions for `/sandbox`: the command on the command
    /// seat and the panel on `ui-registry`, both registered by the sandbox
    /// plugin. The panel spent its whole life as a view model nobody called,
    /// so what this pins is that the front end can now reach it by id.
    ///
    /// The disabled half is the `sandbox_seat_disabled` integration test's: the
    /// process kernel boots once and reads the switches while it does, so a
    /// test that wants the plugin off cannot share this one's kernel.
    #[test]
    fn the_sandbox_plugin_reaches_the_seats_the_front_ends_read() {
        let kernel = process_kernel();
        let commands = rebon_kernel_seats::kernel_core_commands::command_seat()
            .expect("core-commands is a Core plugin and always loads");
        let sandbox = commands
            .find("sandbox")
            .expect("/sandbox is registered by the sandbox plugin");
        assert_eq!(sandbox.owner, rebon_plugin_sandbox::PLUGIN_ID);
        assert_eq!(sandbox.handler.native_id(), Some("sandbox"));

        let ui = kernel
            .context()
            .get::<rebon_kernel_seats::kernel_core_ui::UiSeatService>()
            .expect("core-ui provides the `ui-registry` seat on the root");
        assert!(
            ui.has(rebon_plugin_sandbox::panel::DIALOG_ID),
            "the panel is on the seat: {:?}",
            ui.ids()
        );
    }

    #[test]
    fn process_kernel_boots_once_and_serves_both_planes() {
        let kernel = process_kernel();
        let again = process_kernel();
        assert!(Arc::ptr_eq(&kernel, &again), "one kernel per process");
        let plugins = kernel.plugin_names();
        for core in [
            "config-seats",
            "logger",
            "model-router",
            "core-commands",
            "core-tools",
        ] {
            assert!(
                plugins.iter().any(|name| name == core),
                "{core} missing from {plugins:?}"
            );
        }

        let typed = kernel
            .context()
            .get::<LoggerService>()
            .expect("typed logger resolves");
        typed.log("info", "typed plane ok");

        let out = kernel
            .context()
            .call_json(
                "logger",
                "info",
                serde_json::json!({"message": "json plane ok"}),
            )
            .expect("json logger resolves");
        assert!(out.is_null());
    }

    /// The snapshot says what a plugin declared; the plane says what it
    /// registered. A `node:*` row has only the second kind, so the renderer
    /// has to show it or that plugin reads as contributing nothing.
    #[test]
    fn a_row_shows_what_the_plane_says_it_registered() {
        use rebon_kernel::{PluginKind, PluginState, PluginStatus};

        let snapshot = vec![
            PluginStatus {
                id: "core-commands".to_string(),
                title: "Built-in slash commands".to_string(),
                kind: PluginKind::Core,
                state: PluginState::Loaded,
                provides: vec!["command-registry".to_string()],
                dependents: Vec::new(),
            },
            PluginStatus {
                id: "node:deploy".to_string(),
                title: "deploy".to_string(),
                kind: PluginKind::External,
                state: PluginState::Loaded,
                provides: Vec::new(),
                dependents: Vec::new(),
            },
        ];
        let runtime = |id: &str| match id {
            "node:deploy" => (
                vec!["deploy-report".to_string()],
                vec!["deploy".to_string(), "rollback".to_string()],
            ),
            _ => (Vec::new(), Vec::new()),
        };

        let text = render_plugin_registry_snapshot(&snapshot, &runtime);
        assert!(text.contains("provides command-registry"), "{text}");
        assert!(text.contains("serves deploy-report"), "{text}");
        assert!(text.contains("commands deploy, rollback"), "{text}");
        // A plugin the plane knows nothing about gains no empty columns.
        assert!(
            !text.contains("core-commands  core     loaded  serves"),
            "{text}"
        );
    }

    /// With no plane there is nothing to add, and the rows read as they did.
    #[test]
    fn no_plane_leaves_the_rows_alone() {
        use rebon_kernel::{PluginKind, PluginState, PluginStatus};

        let snapshot = vec![PluginStatus {
            id: "node:deploy".to_string(),
            title: "deploy".to_string(),
            kind: PluginKind::External,
            state: PluginState::Loaded,
            provides: Vec::new(),
            dependents: Vec::new(),
        }];
        let text = render_plugin_registry_snapshot(&snapshot, &no_runtime_contributions);
        assert!(text.contains("node:deploy"), "{text}");
        assert!(!text.contains("serves"), "{text}");
        assert!(!text.contains("commands"), "{text}");
    }
}
