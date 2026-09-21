//! `workflow`: the feature plugin that owns workflow orchestration whole —
//! the `Workflow` tool on the process tool seat, and the runtime behind it on
//! the `workflow-launcher` seat.
//!
//! One tool, `Workflow` (alias `RunWorkflow`): the model hands it a
//! deterministic JavaScript script and the script orchestrates a fleet of
//! local sub-agents — fan out, verify, synthesize — under one permission
//! prompt that shows the reviewed plan before anything runs. [`runtime`] is
//! what actually runs it: the Boa interpreter, the `agent()` bridge onto the
//! sub-agent spawner, the resume cache, and the pre-run static review.
//!
//! **What lives here and what does not.** Tool and runtime are both here;
//! the launch contract they speak lives in `rebon-tool`, because this
//! plugin is not its only side:
//!
//! | lives in `rebon-tool` | why |
//! |---|---|
//! | [`WorkflowLauncher`](rebon_tool::WorkflowLauncher) | `ToolContext` hands it out, and the engine's executor holds one |
//! | `WorkflowLaunchSpec` / `WorkflowLaunchStatus` | the launcher's request and reply; `rebon-render` renders the status |
//! | `WorkflowPermissionPreview` and its review rows | the preview crosses into the front end's permission prompt |
//! | `WORKFLOW_TOOL_NAME` / `RUN_WORKFLOW_ALIAS` | `rebon-core` matches both names when it projects the tool list, and it may not depend on a plugin |
//! | [`WorkflowContext`](rebon_tool::WorkflowContext) | `ToolContext` carries the launcher and the nesting depth, and the depth is stamped by the sub-agent path too |
//! | [`WorkflowNesting`](rebon_tool::WorkflowNesting) | the `Agent` tool and the sub-agent spawner decide by the same depth |
//! | `WORKFLOW_STRUCTURED_OUTPUT_SCHEMA_KEY` | written here, read back by the sub-agent spawner in the `agents` plugin |
//!
//! Turning the plugin off takes `Workflow` off the tool seat *and* leaves the
//! `workflow-launcher` seat empty, so a session is built with no launcher at
//! all: the model can no longer launch or inspect a workflow, and nothing is
//! standing by to run one. Runs already on disk are untouched, and
//! `/workflows` still lists them — that view reads the task registry, not
//! this plugin.
//!
//! **Session scopes.** The tool reads the launcher and the nesting depth off
//! `ToolContext` at call time, so it is a process-wide registration and goes
//! on the process seat. Per-session workflow state, when it moves, lands on
//! the host's session scope
//! ([`rebon_core::session_scope`](rebon_core::session_scope)) — the session
//! scope is one seat the host provides for every session.

use std::sync::Arc;

use rebon_core::tool_seat::{Priority, ToolSeatService, TOOL_SEAT_SERVICE};
use rebon_kernel::{Context, KernelError, Plugin, PluginDef, PluginHost, PluginKind, PluginMeta};
use rebon_tool::{
    WorkflowLauncher, WorkflowLauncherRequest, WorkflowLauncherService, WorkflowLauncherSource,
};

pub mod runtime;
pub mod workflow_tool;

pub use runtime::WorkflowRegistryLauncher;
pub use workflow_tool::WorkflowTool;

pub use rebon_tool::workflow::{RUN_WORKFLOW_ALIAS, WORKFLOW_TOOL_NAME};

/// Stable id: the config key `plugins.workflow.enabled`.
pub const PLUGIN_ID: &str = "workflow";

const PROVIDER_ID: &str = "workflow";

/// The one tool.
///
/// Public so a test that needs the whole builtin catalogue on a bare
/// [`rebon_core::Engine`] can register it without standing up a kernel.
pub fn tools() -> Vec<Arc<dyn rebon_tool::Tool>> {
    vec![Arc::new(WorkflowTool::new()) as Arc<dyn rebon_tool::Tool>]
}

/// Every tool name this plugin puts on the seat, canonical spelling.
pub const TOOL_NAMES: &[&str] = &[WORKFLOW_TOOL_NAME];

/// The provider on the `workflow-launcher` seat.
///
/// Stateless: everything a run needs is in the request, because the front end
/// is the side that knows which tree, which config home, and which provider's
/// profiles this session has. The one thing it cannot name is the task
/// registry resolver — `rebon-tool` sits below the crate that owns it — so it
/// crosses as a [`rebon_tool::WorkflowTaskRuntimeHandle`] and is unwrapped
/// here, where the concrete type is in scope again.
struct RegistryLauncherSource;

impl WorkflowLauncherSource for RegistryLauncherSource {
    fn for_session(
        &self,
        request: WorkflowLauncherRequest,
    ) -> Result<Arc<dyn WorkflowLauncher>, String> {
        let resolver = request
            .task_runtime
            .get::<rebon_plugin_tasks::TaskRegistryResolver>()
            .ok_or_else(|| {
                "the workflow launcher needs a `TaskRegistryResolver` on its task-runtime handle"
                    .to_string()
            })?
            .clone();
        let mut launcher = WorkflowRegistryLauncher::new_resolving(
            resolver,
            request.cwd,
            request.config_home_dir,
            request.session_root,
        )
        .with_model_profiles(request.model_profiles);
        if let Some(provider) = request.active_provider {
            launcher = launcher.with_active_provider(provider);
        }
        Ok(Arc::new(launcher) as Arc<dyn WorkflowLauncher>)
    }
}

#[derive(Default)]
pub struct WorkflowPlugin;

impl Plugin for WorkflowPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(PLUGIN_ID)
            .inject(&[TOOL_SEAT_SERVICE])
            .provides(&[rebon_tool::WORKFLOW_LAUNCHER_SERVICE])
    }

    fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
        let seat = ctx.require::<ToolSeatService>()?;
        seat.register_tools(ctx, PROVIDER_ID, Priority::Feature, tools())?;
        ctx.provide::<WorkflowLauncherService>(Arc::new(RegistryLauncherSource))?;

        Ok(())
    }
}

fn make(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
    Ok(Box::new(WorkflowPlugin::default()))
}

/// This crate's one export to the binary's plugin table.
pub static PLUGIN: PluginDef = PluginDef {
    id: PLUGIN_ID,
    title: "Workflow orchestration (Workflow / RunWorkflow)",
    kind: PluginKind::Feature,
    default_enabled: true,
    factory: make,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_core::tool_seat::ToolSeat;
    use rebon_kernel::{DesiredSet, Kernel, PluginRegistry};
    use rebon_tool::{Tool, ToolResolver};

    /// Stands in for `core-tools`, which lives in `rebon-harness` and cannot
    /// be depended on from here. All this plugin needs is the root seat.
    struct SeatPlugin;

    impl Plugin for SeatPlugin {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("test-seat").provides(&[TOOL_SEAT_SERVICE])
        }

        fn apply(&self, ctx: &Context) -> Result<(), KernelError> {
            ctx.provide::<ToolSeatService>(ToolSeat::new())
        }
    }

    fn make_seat(_: &PluginHost) -> Result<Box<dyn Plugin>, KernelError> {
        Ok(Box::new(SeatPlugin))
    }

    static DEFS: &[PluginDef] = &[
        PluginDef {
            id: "test-seat",
            title: "Test seat",
            kind: PluginKind::Core,
            default_enabled: true,
            factory: make_seat,
        },
        PLUGIN,
    ];

    fn boot() -> (Arc<Kernel>, Arc<PluginRegistry>) {
        let kernel = Kernel::new();
        let host = PluginHost {
            kernel: kernel.clone(),
            config_dir: std::env::temp_dir(),
        };
        let registry = PluginRegistry::new(kernel.clone(), DEFS, host);
        let report = registry.reconcile(&DesiredSet::new());
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        (kernel, registry)
    }

    fn seat(kernel: &Kernel) -> Arc<ToolSeat> {
        kernel
            .context()
            .get::<ToolSeatService>()
            .expect("the seat is on the root")
    }

    #[test]
    fn the_switch_takes_the_tool_off_the_seat_and_puts_it_back() {
        let (kernel, registry) = boot();
        let seat = seat(&kernel);

        assert_eq!(TOOL_NAMES.len(), 1);
        for name in TOOL_NAMES {
            assert!(
                seat.resolve(name, None).unwrap().is_some(),
                "{name} resolves while workflow is loaded"
            );
        }

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("workflow is a feature plugin");
        for name in TOOL_NAMES {
            assert!(
                seat.resolve(name, None).unwrap().is_none(),
                "disabling the plugin takes {name} off the seat"
            );
        }

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        for name in TOOL_NAMES {
            assert!(
                seat.resolve(name, None).unwrap().is_some(),
                "{name} is back on the seat"
            );
        }
    }

    /// The alias is the half `rebon-core`'s tool projection re-advertises,
    /// so the switch has to reach it too.
    #[test]
    fn the_switch_reaches_the_run_workflow_alias() {
        let (kernel, registry) = boot();
        let seat = seat(&kernel);

        assert!(seat
            .resolve(RUN_WORKFLOW_ALIAS, None)
            .unwrap()
            .is_some_and(|tool| tool.id().as_str() == WORKFLOW_TOOL_NAME));

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("workflow is a feature plugin");
        assert!(seat.resolve(RUN_WORKFLOW_ALIAS, None).unwrap().is_none());
    }

    /// The switch reaches the runtime, not only the tool. With the plugin
    /// off the seat is empty, a session is built with no launcher, and there
    /// is nothing standing by to run a workflow — which is the same answer
    /// the model gets from the missing tool.
    #[test]
    fn the_switch_takes_the_launcher_off_its_seat_and_puts_it_back() {
        let (kernel, registry) = boot();

        assert!(kernel.context().get::<WorkflowLauncherService>().is_some());

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("workflow is a feature plugin");
        assert!(
            kernel.context().get::<WorkflowLauncherService>().is_none(),
            "no provider on the seat means the front end attaches no launcher"
        );

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        assert!(kernel.context().get::<WorkflowLauncherService>().is_some());
    }

    /// The task runtime crosses the seat untyped, so a handle of the wrong
    /// shape has to come back as an error at the one call site that builds a
    /// session — not as a panic partway through someone's run.
    #[test]
    fn a_task_runtime_handle_of_the_wrong_shape_is_refused() {
        let refused = RegistryLauncherSource.for_session(WorkflowLauncherRequest {
            task_runtime: rebon_tool::WorkflowTaskRuntimeHandle::new(7u32),
            cwd: std::path::PathBuf::from("/work"),
            config_home_dir: std::path::PathBuf::from("/home/.rebon"),
            session_root: std::path::PathBuf::from("/home/.rebon/projects"),
            model_profiles: rebon_types::ModelProfileMap::default(),
            active_provider: None,
        });

        let Err(error) = refused else {
            panic!("a u32 is not a task registry resolver");
        };
        assert!(error.contains("TaskRegistryResolver"), "{error}");
    }

    /// The names the rest of the tree spells this tool by live in
    /// `rebon-tool`; the tool itself must keep answering to them.
    #[test]
    fn the_tool_answers_to_the_shared_names() {
        let tool = WorkflowTool::new();
        assert_eq!(tool.id().as_str(), WORKFLOW_TOOL_NAME);
        assert_eq!(tool.aliases(), &[RUN_WORKFLOW_ALIAS]);
        assert_eq!(
            (WORKFLOW_TOOL_NAME, RUN_WORKFLOW_ALIAS),
            ("Workflow", "RunWorkflow"),
            "the wire names are a compatibility surface"
        );
    }

    /// The session scope is the host's, not a plugin's: turning a feature
    /// plugin off does not disturb anything session-scoped, because a feature
    /// plugin owns nothing session-scoped. Its tools still leave the seat,
    /// which is what the model sees.
    #[test]
    fn the_hosts_session_scope_outlives_this_plugin_being_turned_off() {
        use rebon_core::session_scope::session_scope_is;

        let (kernel, registry) = boot();
        // Standing in for `bind_session_scope`, which lives in
        // `rebon-harness` and cannot be depended on from here.
        let session = kernel.context().fork_scoped("session/sess-a");
        rebon_core::session_scope::provide(&session, "sess-a").unwrap();
        assert!(session_scope_is(&session, "sess-a"));

        registry
            .set_enabled(PLUGIN_ID, false)
            .expect("workflow is a feature plugin");

        assert!(
            session_scope_is(&session, "sess-a"),
            "the scope belongs to the host, so unloading a plugin leaves it standing"
        );
        assert!(
            seat(&kernel)
                .resolve(WORKFLOW_TOOL_NAME, None)
                .unwrap()
                .is_none(),
            "the tool, which is the plugin's, does leave"
        );

        registry.set_enabled(PLUGIN_ID, true).expect("and back");
        session.dispose();
    }
}
