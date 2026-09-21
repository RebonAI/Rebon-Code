//! Per-session agent loops, on the plugin plane.
//!
//! One loop, one host. The embedded path gave each session its own isolate, and
//! this gives each session its own Node process for the same reason — not by
//! preference, but because the vendored dsh packages say so:
//!
//! * `@deepseek-ai/dsh-agent` declares a Cordis accessor (`agent`) on the
//!   context prototype, which is process-global. Mounting it twice in one host
//!   fails with `property "agent" is already declared as accessor`, whatever
//!   realm each copy is placed in.
//! * `@deepseek-ai/dsh-agent-loop` registers itself as *the* agent factory on
//!   whichever `agents` registry it can see, and a second one is refused.
//!
//! So a realm is not enough to keep two loops apart; a process is. That is the
//! same trade the isolate made, at the same granularity, and it is why rebon's
//! own deployment already works this way: a background worker is one process per
//! session, so the ordinary case is one loop in one host either way.
//!
//! What the loop's host contains is the old `composition_config` said in the
//! plane's vocabulary: the user's `kernelPlugins` entries (their model
//! adapters), plus the loop group — `dsh-session`, `dsh-agent`,
//! `dsh-system-prompt`, rebon's loop assembly and `dsh-agent-loop` — under a
//! group that isolates `systemPrompt`, because the real dsh service and rebon's
//! seat share that name.
//!
//! Nothing it loads is published. What a loop registers belongs to its session:
//! the report still reaches rebon, but the process tables never see it, so
//! disposing a loop cannot remove a route the shared composition still serves.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use rebon_kernel::Context;
use serde_json::{json, Value};

use crate::loop_host::{loop_vendor, LoopAgentSpec, LoopHost, LoopHostSpawner};
use crate::plugin_composition::{plane_composition, CompositionRoots};
use crate::plugin_manifests::PayloadManifests;
use crate::plugin_plane::{ComposeEntry, ComposeNode, PluginPlane, PluginPlaneConfig};

/// Service the loop assembly registers for inbound commands.
const CONTROL_SERVICE: &str = "loop:control";
/// Budget for one control command round-trip (commands are queue writes, not
/// model turns — anything slower means the loop is wedged).
const CONTROL_BUDGET: Duration = Duration::from_secs(10);
/// How long spawn waits for the loop to come up and announce its agent.
const SPAWN_DEADLINE: Duration = Duration::from_secs(30);
/// The one service a loop realm must not share with rebon's own seat.
const LOOP_ISOLATE: &str = "systemPrompt";
/// The loop group, and the package each entry names.
const LOOP_PARTS: &[(&str, &str)] = &[
    ("loop-sessions", "@deepseek-ai/dsh-session"),
    ("loop-agents", "@deepseek-ai/dsh-agent"),
    ("loop-prompt", "@deepseek-ai/dsh-system-prompt"),
    ("loop-assembly", "rebon-loop-assembly"),
    ("agent-loop", "@deepseek-ai/dsh-agent-loop"),
];

/// One live loop, with the host it runs on.
pub struct PlaneLoopHost {
    plane: Arc<PluginPlane>,
    fork: Context,
    scope_id: String,
    agent_id: String,
    events: StdMutex<Option<tokio::sync::mpsc::UnboundedReceiver<Value>>>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
}

impl PlaneLoopHost {
    /// Boots one loop on a host of its own.
    pub async fn spawn(kernel: &Context, spec: LoopAgentSpec) -> Result<Self, String> {
        if let Some(vendor) = loop_vendor(&spec.vendor) {
            if !vendor.bundled {
                return Err(format!(
                    "loop vendor `{}` has no bundled assembly in this build",
                    vendor.id
                ));
            }
        }
        let paths = crate::plugin_boot::plane_paths()?;
        let roots = CompositionRoots {
            payload: crate::plugin_composition::payload_root(&paths.compose_root),
            runtime: paths.compose_root.clone(),
        };
        let manifests = PayloadManifests::load(&roots.runtime)?;

        // The loop's own kernel fork: grants ride it, and disposing it revokes
        // everything the loop's session put on the plane.
        let fork = kernel.fork(&format!("loop-{}", spec.session_id));
        crate::plugin_boot::register_credential_grants(
            &fork,
            crate::plugin_boot::load_credential_grants(&spec.config_dir),
        );

        // The tool seat for this loop: the session's workspace root and the user's grants,
        // rather than the process-wide slot.
        // `kernel` is the process root the spawner was built with, which is
        // the seat this host resolved plugin tools through before it was
        // handed one — the loop's own `fork` is where its *registrations* go,
        // not where it reads rebon's.
        let invoke_host = rebon_kernel_seats::kernel_tool_invoke::EngineToolInvokeHost::new(
            kernel,
            spec.workspace_root.clone(),
            &spec.config_dir,
            rebon_kernel_seats::kernel_tool_invoke::load_tool_grants(&spec.config_dir),
        );
        let exposed_tools: Vec<String> = {
            let engine = invoke_host.engine();
            engine
                .eager_tool_snapshots()
                .into_iter()
                .map(|snapshot| snapshot.name)
                .chain(engine.deferred_tool_names())
                .collect()
        };
        let tool_catalog = invoke_host.describe_catalog();

        // The user's own composition entries: the loop names a model route, and
        // an adapter for it has to be here or every turn ends in NO_ADAPTER.
        let composition =
            plane_composition(&spec.config_dir, &roots, &exposed_tools).unwrap_or_default();
        for skipped in &composition.skipped {
            tracing::warn!(entry = %skipped, "loop composition entry not loaded");
        }

        let mut structure = composition.structure.clone();
        structure.push(loop_group());
        let mut entries = composition.entries.clone();
        entries.extend(loop_entries(
            &manifests,
            &roots,
            &spec,
            &tool_catalog,
            &exposed_tools,
        )?);
        // A loop's registrations are its session's, not the process's.
        for entry in &mut entries {
            entry.publish = false;
        }
        let scope_id = format!("loop:{}", spec.session_id);
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let created: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
        // Observers before anything loads: nothing the loop publishes on its
        // way up is missed.
        {
            let slot = created.clone();
            let mine = scope_id.clone();
            fork.on_json("loop:agent-created", move |payload| {
                if !belongs(payload, &mine) {
                    return;
                }
                if let Some(id) = event_of(payload).get("agentId").and_then(Value::as_str) {
                    *slot.lock().unwrap() = Some(id.to_string());
                }
            });
        }
        for (topic, channel) in [("loop:event", "event"), ("loop:agent-error", "error")] {
            let tx = event_tx.clone();
            let mine = scope_id.clone();
            fork.on_json(topic, move |payload| {
                if !belongs(payload, &mine) {
                    return;
                }
                let mut stamped = event_of(payload).clone();
                if let Some(map) = stamped.as_object_mut() {
                    map.insert("channel".into(), Value::String(channel.into()));
                }
                let _ = tx.send(stamped);
            });
        }

        let registry = rebon_kernel_seats::kernel_compose_tools::ComposeToolRegistry::new(
            exposed_tools.clone(),
        );
        let plane = PluginPlane::start(
            PluginPlaneConfig {
                node: paths.node.clone(),
                host_script: paths.host_script.clone(),
                loader: paths.loader.clone(),
                compose_root: paths.compose_root.clone(),
                // The composition runtime resolves its own payload; rebon only
                // needs a root to name a manifest's module against.
                payload_dir: None,
                structure,
                web: composition.web.clone(),
                modules: composition.modules.clone(),
                exposed_tools,
                exposed_seats: crate::plugin_plane::default_exposed_seats(),
                tool_catalog,
                // The plane exists for this session, so its scope is named
                // after it: what the loop publishes is stamped with a scope
                // that says whose it is, and a subscriber can tell two loops
                // apart on a process-wide event plane.
                scope_id: Some(scope_id.clone()),
                working_directory: spec.workspace_root.clone(),
                unary_call_timeout: None,
            },
            fork.clone(),
            registry,
            invoke_host.clone(),
        )
        .await
        .map_err(|error| {
            fork.dispose();
            format!("loop host: {error}")
        })?;

        let fail = |plane: &Arc<PluginPlane>, fork: &Context, reason: String| {
            let plane = Arc::clone(plane);
            let fork = fork.clone();
            tokio::spawn(async move {
                plane.shutdown().await;
                fork.dispose();
            });
            reason
        };

        for entry in &entries {
            if let Err(error) = plane.load_entry(entry).await {
                return Err(fail(
                    &plane,
                    &fork,
                    format!("loop entry {} failed to load: {error}", entry.id),
                ));
            }
        }
        // No second scope: the plane's own is the session, opened for every
        // entry as it loads. A loop that also opened one of its own would hand
        // its assembly two session handles and publish through whichever
        // attached first.
        let deadline = tokio::time::Instant::now() + SPAWN_DEADLINE;
        let agent_id = loop {
            if let Some(id) = created.lock().unwrap().take() {
                break id;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(fail(
                    &plane,
                    &fork,
                    "the loop agent did not come up before the deadline".to_owned(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        Ok(Self {
            plane,
            fork,
            scope_id,
            agent_id,
            events: StdMutex::new(Some(event_rx)),
            stopped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    async fn control(&self, request: Value) -> Result<Value, String> {
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err("the loop has been shut down".into());
        }
        tokio::time::timeout(
            CONTROL_BUDGET,
            self.plane
                .call_service("loop-assembly", &self.scope_id, CONTROL_SERVICE, request),
        )
        .await
        .map_err(|_| format!("loop:control did not answer within {CONTROL_BUDGET:?}"))?
        .map_err(|error| error.to_string())
    }
}

/// Whether a published event is this loop's.
///
/// By scope rather than by the tag the composition stamps on itself: the scope
/// on the envelope is injected by the host, and every loop has one of its own
/// even though they share plugin ids across hosts. The kernel's event plane is
/// process-wide, so two loops in one rebon process hear each other's events
/// without this.
fn belongs(payload: &Value, scope_id: &str) -> bool {
    payload
        .get("scopeId")
        .and_then(Value::as_str)
        .is_some_and(|scope| scope == scope_id)
}

/// The published envelope wraps the loop's own event; the embedder's stream has
/// always carried the event itself.
fn event_of(payload: &Value) -> &Value {
    payload.get("event").unwrap_or(payload)
}

/// The loop group: one realm for the dsh prompt plane, so it and rebon's seat
/// keep their own `systemPrompt`.
fn loop_group() -> ComposeNode {
    ComposeNode {
        id: "loop".to_owned(),
        isolate: Some(json!({ LOOP_ISOLATE: "loop" })),
        group: Some(
            LOOP_PARTS
                .iter()
                .map(|(id, _)| ComposeNode {
                    id: (*id).to_owned(),
                    isolate: None,
                    group: None,
                })
                .collect(),
        ),
    }
}

/// The five load requests, with the loop's own configuration folded in.
fn loop_entries(
    manifests: &PayloadManifests,
    roots: &CompositionRoots,
    spec: &LoopAgentSpec,
    tool_catalog: &Value,
    exposed_tools: &[String],
) -> Result<Vec<ComposeEntry>, String> {
    let mut assembly_config = json!({
        "tag": spec.session_id,
        "toolCatalog": tool_catalog,
        // Which agents this assembly answers for. One loop, one agent today,
        // and naming it keeps that true rather than assumed.
        "agents": [spec.session_id],
    });
    if let Some(section) = &spec.prompt_section {
        assembly_config["section"] = section.clone();
    }
    let mut out = Vec::new();
    for (id, package) in LOOP_PARTS {
        let manifest = manifests
            .get(package)
            .ok_or_else(|| format!("no manifest ships for {package}"))?;
        let module = manifest
            .entry
            .clone()
            .ok_or_else(|| format!("the manifest for {package} names no module"))?;
        let root = if crate::plugin_manifests::PayloadManifests::root_is_runtime(manifest) {
            roots.runtime.clone()
        } else {
            roots.payload.clone()
        };
        let config = match *id {
            "loop-assembly" => assembly_config.clone(),
            "agent-loop" => json!({ "agents": [{
                "id": spec.session_id,
                "provider": spec.provider,
                "model": spec.model,
            }] }),
            _ => Value::Null,
        };
        // The agent loop's manifest declares `$rebon/tools`, and this is the
        // list that sentinel names. Passing an empty one expanded it to nothing:
        // the loop presented rebon's whole catalog to the model and then had
        // every call it made refused as undeclared.
        out.push(crate::plugin_manifests::entry_for(
            manifest,
            id,
            root,
            module,
            config,
            exposed_tools,
        ));
    }
    Ok(out)
}

#[async_trait]
impl LoopHost for PlaneLoopHost {
    fn agent_id(&self) -> &str {
        &self.agent_id
    }

    fn take_events(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Value>> {
        self.events.lock().unwrap().take()
    }

    async fn followup(&self, text: &str) -> Result<(), String> {
        self.control(json!({ "kind": "followup", "agentId": self.agent_id, "text": text }))
            .await
            .map(|_| ())
    }

    async fn steer(&self, text: &str) -> Result<(), String> {
        self.control(json!({ "kind": "steer", "agentId": self.agent_id, "text": text }))
            .await
            .map(|_| ())
    }

    async fn cancel(&self) -> Result<(), String> {
        self.control(json!({ "kind": "cancel", "agentId": self.agent_id }))
            .await
            .map(|_| ())
    }

    async fn status(&self) -> Result<String, String> {
        let answer = self
            .control(json!({ "kind": "status", "agentId": self.agent_id }))
            .await?;
        Ok(answer
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned())
    }

    fn shutdown(&self) {
        if self.stopped.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        // Winding a host down is a protocol conversation and therefore async,
        // while this is not: the handle's contract is "unusable afterwards",
        // which the flag above already delivers. The host drains on its own and
        // the fork unwinds with it.
        let plane = Arc::clone(&self.plane);
        let fork = self.fork.clone();
        tokio::spawn(async move {
            plane.shutdown().await;
            fork.dispose();
        });
    }

    fn has_exited(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Drop for PlaneLoopHost {
    fn drop(&mut self) {
        // The kill path: a handle dropped without `shutdown` must not leave a
        // Node process holding the session's plugins.
        LoopHost::shutdown(self);
    }
}

/// Boots loop hosts on the plugin plane.
pub struct PlaneLoopSpawner {
    kernel: Context,
}

impl PlaneLoopSpawner {
    pub fn new(kernel: Context) -> Self {
        Self { kernel }
    }
}

#[async_trait]
impl LoopHostSpawner for PlaneLoopSpawner {
    async fn spawn(&self, spec: LoopAgentSpec) -> Result<Arc<dyn LoopHost>, String> {
        Ok(Arc::new(PlaneLoopHost::spawn(&self.kernel, spec).await?))
    }
}
