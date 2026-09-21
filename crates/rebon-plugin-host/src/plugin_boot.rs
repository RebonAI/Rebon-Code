//! Booting the configured composition, on the plugin plane.
//!
//! `kernelPlugins` describes one composition; the plane hosts every one of
//! them. Everything here is about that — resolving the plane's own pieces,
//! holding the process-wide instance, and handing the credential grants to the
//! seat that answers for them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rebon_kernel::{Context, Kernel, PluginRegistry};
use rebon_kernel_seats::kernel_tool_asks::{
    clear_process_tool_asks, set_process_tool_asks, ToolAskService, DEFAULT_ASK_TIMEOUT,
    TOOL_ASKS_SERVICE,
};
use rebon_kernel_seats::kernel_tool_invoke::{load_tool_grants, EngineToolInvokeHost};

/// Parse the `kernelPlugins.credentialGrants` list: environment-variable
/// names the user has explicitly allowed composed plugins to resolve.
pub fn load_credential_grants(config_dir: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read(config_dir.join("config.json")) else {
        return Vec::new();
    };
    let Ok(config) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let mut grants: Vec<String> = config
        .get("kernelPlugins")
        .and_then(|section| section.get("credentialGrants"))
        .and_then(|list| list.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    grants.sort();
    grants.dedup();
    grants
}

/// Register the host-side authorizer for the config-granted credential
/// references: only `env`-shaped requests (`{env: true}`) for exactly the
/// listed names are allowed; every other request — other refs, provider-keyed
/// `get`s — falls through the waterfall unanswered and stays fail-closed.
pub fn register_credential_grants(ctx: &Context, grants: Vec<String>) {
    if grants.is_empty() {
        return;
    }
    let granted: std::collections::HashSet<String> = grants.into_iter().collect();
    tracing::info!(
        refs = ?granted,
        "kernel credential grants active (config kernelPlugins.credentialGrants)"
    );
    ctx.wrap_json(
        rebon_kernel_seats::kernel_config_seats::CREDENTIALS_AUTHORIZE_EVENT,
        move |payload, next| {
            let env_shaped = payload.get("env").and_then(|e| e.as_bool()) == Some(true);
            let allowed = payload
                .get("ref")
                .and_then(|r| r.as_str())
                .is_some_and(|name| granted.contains(name));
            if env_shaped && allowed {
                serde_json::json!({ "allow": true })
            } else {
                next.call(payload)
            }
        },
    );
}

/// Boots the configured composition on whichever runtime this build is set to.
///
/// One entry point rather than two call sites choosing for themselves: a
/// process that booted the embedded composition on one path and the plane on
/// another would have two compositions registering into the same seats, and
/// whichever lost the race would look like a plugin that silently does nothing.
///
/// `plugins` is the process registry, which is both the kernel the plane forks
/// its scope off and the row table the `node:*` switches live in. It is a
/// parameter rather than a lookup because a caller can reach this before the
/// assembly layer has booted — the desktop starts its plane and its kernel as
/// two tasks on one runtime — and a lookup that resolved the process registry
/// at the call site would boot one if that race went the other
/// way, which is what the lookup this replaced used to do.
pub async fn ensure_process_composition(
    plugins: &Arc<PluginRegistry>,
) -> Option<CompositionRefusal> {
    ensure_process_plugin_plane(plugins).await;
    composition_refusal()
}

/// Where the plugin plane's own pieces live.
///
/// The ladder [`rebon_plugin_supervisor::locate_host_script`] uses: a deployed
/// rebon carries these beside its executable, and a build tree resolves them
/// from where they were built.
pub struct PlanePaths {
    pub node: PathBuf,
    pub host_script: PathBuf,
    pub loader: PathBuf,
    pub compose_root: PathBuf,
}

/// The plane's own files, which ship with rebon and are always present.
#[derive(Clone, Debug)]
pub struct PlaneScripts {
    pub host_script: PathBuf,
    pub loader: PathBuf,
    pub compose_root: PathBuf,
}

/// Why a *configured* composition did not boot.
///
/// Only ever produced when kernel plugins were actually asked for. A machine
/// that configured none has nothing to be told, and telling it anyway would
/// train people to ignore the message that matters.
#[derive(Clone, Debug)]
pub enum CompositionRefusal {
    /// Kernel plugins are configured and there is no Node runtime to run them.
    ///
    /// The one failure with a one-command fix, so it carries its own
    /// instructions rather than making a caller invent them.
    NoRuntime { entries: usize, reason: String },
    /// The composition was reached but something else stopped it.
    Failed { reason: String },
}

impl CompositionRefusal {
    /// What a person should see. Ends in what to do about it.
    pub fn message(&self) -> String {
        match self {
            Self::NoRuntime { entries, reason } => format!(
                "{entries} kernel plugin{} configured, but there is no Node runtime to run \
                 {}: {reason}.\n\n  rebon node install\n\ninstalls the pinned build \
                 ({}) under ~/.rebon and needs no arguments. Rebon only accepts bytes \
                 matching a SHA-256 compiled into this binary. Already have a Node? Point \
                 {} at it instead. Offline? `rebon node install --from-path <archive> \
                 --version <x.y.z> --sha256 <digest>`.",
                if *entries == 1 { " is" } else { "s are" },
                if *entries == 1 { "it" } else { "them" },
                rebon_node_runtime::PINNED_NODE_VERSION,
                rebon_node_runtime::NODE_EXECUTABLE_ENV,
            ),
            Self::Failed { reason } => {
                format!("the kernel plugin composition did not start: {reason}")
            }
        }
    }
}

fn repo_relative(relative: &str) -> Option<PathBuf> {
    let candidate = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    candidate.exists().then(|| candidate)
}

/// Resolves everything the plane needs, or says which piece is missing.
/// The Node rebon runs on, by the resolution ladder `rebon-node-runtime`
/// defines: an explicit `REBON_PLUGIN_NODE`, then a managed install, then PATH.
///
/// One answer for everything that needs Node, so a plugin provider and the
/// plugin host cannot end up on two different ones — and so "the user machine
/// needs no Node of its own" keeps meaning something once the embedded runtime
/// that used to make it true is gone.
pub fn resolve_node() -> Result<PathBuf, String> {
    let store = rebon_node_runtime::ManagedRuntimeStore::under_config_home(
        &rebon_config::config_home_dir(),
    );
    let probe = rebon_node_runtime::ExecutingProbe;
    rebon_node_runtime::NodeRuntimeResolver::from_env(&probe, &store)
        .resolve()
        .map(|resolved| resolved.executable.clone())
        .map_err(|error| format!("no usable Node runtime: {error}"))
}

/// Everything the plane needs except the runtime.
///
/// Split from [`plane_paths`] so a boot can find out *whether there is anything
/// to run* before it demands a Node. Rebon must not tell someone with no kernel
/// plugins to go and install a runtime for them.
pub fn plane_scripts() -> Result<PlaneScripts, String> {
    let host_script = rebon_plugin_supervisor::locate_host_script()
        .ok()
        .or_else(|| repo_relative("runtimes/node/plugin-host/src/cli.mjs"))
        .ok_or_else(|| {
            "no plugin host script; set REBON_PLUGIN_HOST_JS to an absolute cli.mjs".to_owned()
        })?;
    let loader = rebon_plugin_supervisor::locate_compose_loader()
        .ok()
        .or_else(|| repo_relative("runtimes/node/compose-runtime/src/index.mjs"))
        .ok_or_else(|| {
            "no composition loader; set REBON_COMPOSE_LOADER_JS to an absolute index.mjs".to_owned()
        })?;
    let compose_root = loader
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("{} has no package root", loader.display()))?
        .to_path_buf();

    Ok(PlaneScripts {
        host_script,
        loader,
        compose_root,
    })
}

pub fn plane_paths() -> Result<PlanePaths, String> {
    let node = resolve_node()?;
    let scripts = plane_scripts()?;
    Ok(PlanePaths {
        node,
        host_script: scripts.host_script,
        loader: scripts.loader,
        compose_root: scripts.compose_root,
    })
}

/// Kernel plugins are configured and there is nothing to run them on.
#[derive(Clone, Debug)]
pub struct MissingRuntime {
    /// How many entries the configuration asks for. Counted from the written
    /// list, not from a resolved composition — see
    /// [`missing_runtime_for_configured_plugins`].
    pub entries: usize,
    /// The resolver's own words for why nothing was found.
    pub reason: String,
}

/// A cheap pre-flight: does this machine want kernel plugins it cannot run?
///
/// Reads the configured entry list rather than resolving the composition,
/// because it runs before a session exists and its only job is to decide
/// whether to offer an install. The boot's own [`CompositionRefusal`] stays
/// authoritative about what actually happened.
///
/// `kernelPlugins` is opt-in and absent from a default install, so this asks
/// nothing of the many and asks exactly once of the few who wrote the section.
pub fn missing_runtime_for_configured_plugins() -> Option<MissingRuntime> {
    let config_dir = rebon_config::config_home_dir();
    let raw = std::fs::read(config_dir.join("config.json")).ok()?;
    let config: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let entries = config
        .get("kernelPlugins")?
        .get("plugins")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if entries == 0 {
        return None;
    }
    let reason = resolve_node().err()?;
    Some(MissingRuntime { entries, reason })
}

/// The plugin runtime, as a surface shows it to a person.
///
/// [`missing_runtime_for_configured_plugins`] answers one yes/no question at
/// startup, which is the right shape for a gate and the wrong shape for a panel:
/// a settings page has to say what *is* the case as well as what is wrong, and
/// has to say it whether or not anything is configured yet.
#[derive(Clone, Debug)]
pub struct PlaneRuntimeStatus {
    /// Kernel plugin entries the configuration asks for. Zero is a normal
    /// state, not a problem — `kernelPlugins` is opt-in.
    pub configured_plugins: usize,
    /// The runtime a composition would run on, if one resolves.
    pub runtime: Option<ResolvedRuntime>,
    /// The resolver's own words for why nothing resolved.
    pub problem: Option<String>,
    /// A Node that was found and turned down for its version.
    ///
    /// Separate from `problem` because it is a different situation with a
    /// different fix, and because "no usable Node runtime" reads as "none
    /// installed" to someone who has one — the search already looked at `PATH`
    /// and this is what it found there.
    pub out_of_range: Option<OutOfRangeRuntime>,
    /// The versions a runtime may report, as a range someone can read.
    pub supported: String,
    /// Whether `rebon node install` could fix this here: rebon pins a build for
    /// this platform and nothing forbids fetching it. False on a platform with
    /// no pinned build, or where an operator disabled downloads — telling
    /// someone to run a command that will refuse is worse than saying nothing.
    pub installable: bool,
}

/// A Node that exists and answered, but not with a version this build accepts.
#[derive(Clone, Debug)]
pub struct OutOfRangeRuntime {
    pub version: String,
    pub executable: PathBuf,
}

/// One resolved Node, flattened so a UI needs no dependency on the resolver.
#[derive(Clone, Debug)]
pub struct ResolvedRuntime {
    pub version: String,
    pub executable: PathBuf,
    /// Which rung of the ladder answered: an explicit demand, a managed
    /// install, or `PATH`.
    pub origin: String,
}

impl PlaneRuntimeStatus {
    /// Whether something is configured that cannot run.
    pub fn is_blocked(&self) -> bool {
        self.configured_plugins > 0 && self.runtime.is_none()
    }
}

/// Reads the current state. Cheap enough for a panel to call on open: one
/// config read plus, at most, one short-lived `node --version`.
pub fn plane_runtime_status() -> PlaneRuntimeStatus {
    let config_dir = rebon_config::config_home_dir();
    let configured_plugins = std::fs::read(config_dir.join("config.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .and_then(|config| {
            config
                .get("kernelPlugins")?
                .get("plugins")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
        })
        .unwrap_or(0);

    let store = rebon_node_runtime::ManagedRuntimeStore::under_config_home(
        &rebon_config::config_home_dir(),
    );
    let probe = rebon_node_runtime::ExecutingProbe;
    let mut out_of_range = None;
    let (runtime, problem) =
        match rebon_node_runtime::NodeRuntimeResolver::from_env(&probe, &store).resolve() {
            Ok(resolved) => (
                Some(ResolvedRuntime {
                    version: resolved.version.to_string(),
                    executable: resolved.executable.clone(),
                    origin: resolved.origin.to_string(),
                }),
                None,
            ),
            Err(error) => {
                // The newest one that was turned down for its version. A
                // machine usually has one Node; when it has several, the newest
                // is the one whose upgrade is shortest.
                if let rebon_node_runtime::ResolveError::Unavailable { rejected, .. } = &error {
                    out_of_range = rejected
                        .iter()
                        .filter_map(|candidate| match &candidate.reason {
                            rebon_node_runtime::RejectionReason::Unsupported(version) => {
                                Some((*version, candidate.executable.clone()))
                            }
                            rebon_node_runtime::RejectionReason::Unusable(_) => None,
                        })
                        .max_by_key(|(version, _)| *version)
                        .map(|(version, executable)| OutOfRangeRuntime {
                            version: version.to_string(),
                            executable,
                        });
                }
                (None, Some(error.to_string()))
            }
        };

    PlaneRuntimeStatus {
        configured_plugins,
        runtime,
        problem,
        out_of_range,
        supported: rebon_node_runtime::SUPPORTED_NODE_VERSIONS.to_string(),
        installable: rebon_node_runtime::pinned_archive_for_host().is_some()
            && !rebon_node_runtime::DownloadPolicy::from_env().is_disabled(),
    }
}

/// The version `rebon node install` would put on this machine.
pub fn pinned_node_version() -> String {
    rebon_node_runtime::PINNED_NODE_VERSION.to_string()
}

/// Records a refusal and returns the `None` the boot owes its caller.
///
/// One place, so a boot cannot grow a path that fails without saying so — which
/// is exactly what the old `let _ =` at the call site allowed: kernel plugins
/// configured, no runtime present, and nothing anywhere that said either.
fn refuse<T>(refusal: CompositionRefusal) -> Option<T> {
    tracing::warn!(refusal = %refusal.message(), "kernel plugin composition unavailable");
    *refusal_slot().lock().expect("composition refusal poisoned") = Some(refusal);
    None
}

/// Why this process has no composition *right now*.
///
/// A cell rather than the `OnceLock` this was, for the reason the plane's own
/// slot stopped being one: a refusal is a fact about one attempt to start, and
/// the plane can now be stopped and started again. A message from the attempt
/// before last, shown next to a plane that is running, is worse than no
/// message — it says a composition failed when one is serving turns.
fn refusal_slot() -> &'static std::sync::Mutex<Option<CompositionRefusal>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<CompositionRefusal>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Forgets the last refusal. Called where a new attempt begins and where a
/// plane goes away, so the answer is always about the current generation.
pub(crate) fn clear_composition_refusal() {
    *refusal_slot().lock().expect("composition refusal poisoned") = None;
}

/// Why this process has no kernel plugin composition, when it should have had
/// one. `None` means either that it has one or that none was configured.
///
/// Read after [`ensure_process_composition`]; a surface that can show a person
/// something renders [`CompositionRefusal::message`].
pub fn composition_refusal() -> Option<CompositionRefusal> {
    refusal_slot()
        .lock()
        .expect("composition refusal poisoned")
        .clone()
}

/// The running plane, if there is one.
///
/// A cell rather than a `OnceCell`, because the plane is now something that
/// can be turned off: unloading the `node-host` plugin winds it down, and
/// loading it again boots a new one. A `OnceCell` remembers the first answer
/// forever, which was right while nothing could stop a plane and wrong the
/// moment a switch could.
#[derive(Clone, Default)]
pub(crate) struct RunningPlane {
    pub(crate) plane: Option<Arc<crate::plugin_plane::PluginPlane>>,
    /// The kernel fork everything the plane published lives on. Disposing it
    /// is what takes the tools, the prompt sections and the seats back out.
    pub(crate) ctx: Option<rebon_kernel::Context>,
    /// The runtime the plane was booted on, so a switch flipped from a
    /// thread with no runtime of its own still has somewhere to await.
    pub(crate) runtime: Option<tokio::runtime::Handle>,
}

/// Never held across an `await`: it holds three handles and nothing else, and
/// the one long operation — booting — serialises on [`boot_lock`] instead.
fn plane_slot() -> &'static std::sync::Mutex<RunningPlane> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<RunningPlane>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(RunningPlane::default()))
}

fn boot_lock() -> &'static tokio::sync::Mutex<()> {
    static BOOT: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    BOOT.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) fn running_plane() -> RunningPlane {
    plane_slot().lock().expect("plane slot poisoned").clone()
}

/// Takes the plane out of the slot, for a caller that is about to stop it.
pub(crate) fn take_running_plane() -> RunningPlane {
    std::mem::take(&mut *plane_slot().lock().expect("plane slot poisoned"))
}

/// Take this process's plane out of its slot and wind it down.
///
/// Idempotent: whichever exit path arrives first empties the slot and the
/// others find nothing. The fork is disposed after the host is down, not
/// before — what the fork holds is what the plane registered, and taking that
/// away first would leave calls in flight resolving to nothing.
///
/// The slot itself stays private. A caller that has more to wind down
/// (the process front end that also retires the native MCP
/// runtimes) wraps this rather than reaching into it.
pub async fn shutdown_process_plane() {
    let running = take_running_plane();
    if let Some(plane) = running.plane {
        plane.shutdown().await;
    }
    if let Some(ctx) = running.ctx {
        ctx.dispose();
    }
}

/// The plane this process runs, booting it if the `node-host` plugin says it
/// may run and it has not started yet.
///
/// The plugin decides, not the caller. Before this, four hosts each called a
/// boot function and the first one to arrive settled it; now they all ask the
/// same question — is `node-host` loaded — and a `None` means exactly what it
/// has always meant: no composition, and nothing that depends on one.
pub async fn ensure_process_plugin_plane(
    plugins: &Arc<PluginRegistry>,
) -> Option<Arc<crate::plugin_plane::PluginPlane>> {
    ensure_plane(plugins, false).await
}

/// The plane, started even when the composition is empty.
///
/// "Nothing configured" is a reason not to start a host *for a composition*.
/// It is not a reason to refuse one to a caller that has its own plugin to
/// load — a package's model provider, selected by the user — and treating it
/// as one would mean a provider package could only work on a machine that also
/// configured `kernelPlugins`.
pub async fn ensure_plane_for_provider(
    plugins: &Arc<PluginRegistry>,
) -> Option<Arc<crate::plugin_plane::PluginPlane>> {
    ensure_plane(plugins, true).await
}

async fn ensure_plane(
    plugins: &Arc<PluginRegistry>,
    even_with_no_entries: bool,
) -> Option<Arc<crate::plugin_plane::PluginPlane>> {
    // Here rather than only in `ensure_process_composition`, because the wanted
    // set is what the `node:*` rows put there and a caller that reached this
    // first — `rebon serve`'s status endpoint does — would otherwise boot a
    // plane with nothing in it and cache that answer. Cheap when nothing on
    // disk moved.
    crate::kernel_node_host::refresh_external_defs(plugins);
    let host = node_host(plugins.kernel())?;
    if let Some(plane) = running_plane().plane {
        return Some(plane);
    }
    // A boot that already failed stands until something says the answer
    // changed. Without this the verdict was cleared and retried by *every*
    // caller: one unusable machine turned a single failed resolution into a
    // string of full boot attempts, each paying the startup cost again and
    // each leaving another Node behind. `clear_composition_refusal` — which
    // `node-host` calls when it is (re)loaded — is what makes a retry mean
    // something again.
    if composition_refusal().is_some() {
        return None;
    }
    let _one_boot_at_a_time = boot_lock().lock().await;
    // Checked again under the lock: two hosts assembling their first session
    // at once must not each start a Node process.
    if let Some(plane) = running_plane().plane {
        return Some(plane);
    }
    if composition_refusal().is_some() {
        return None;
    }
    let wanted = host.wanted();
    boot_attempts().fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Bounded, and the bound is the caller's: a host that never finishes
    // starting used to park the caller forever, on the main thread of a
    // process whose only remaining job was to say what went wrong. A boot is
    // a local process start; when it has not finished in this long it is not
    // going to.
    let booted = match tokio::time::timeout(
        PLANE_BOOT_TIMEOUT,
        boot_process_plugin_plane(plugins.kernel(), &wanted, even_with_no_entries),
    )
    .await
    {
        Ok(booted) => booted,
        Err(_) => refuse(CompositionRefusal::Failed {
            reason: format!(
                "the plugin host did not finish starting within {} seconds",
                PLANE_BOOT_TIMEOUT.as_secs()
            ),
        }),
    };
    let (plane, ctx) = booted?;
    *plane_slot().lock().expect("plane slot poisoned") = RunningPlane {
        plane: Some(Arc::clone(&plane)),
        ctx: Some(ctx),
        runtime: Some(tokio::runtime::Handle::current()),
    };
    Some(plane)
}

/// The plane this process is running, without starting one.
///
/// The read-only half of [`ensure_process_plugin_plane`]: a caller that wants
/// to *describe* what is running must not bring a Node host up by asking.
pub fn running_plugin_plane() -> Option<Arc<crate::plugin_plane::PluginPlane>> {
    running_plane().plane
}

/// How many times this process has actually tried to start a host.
///
/// Not a statistic: it is the only externally visible difference between "the
/// verdict stood" and "we started another Node and it failed the same way", and
/// the second of those is what this counter exists to keep from coming back.
pub fn plane_boot_attempts() -> u64 {
    boot_attempts().load(std::sync::atomic::Ordering::Relaxed)
}

fn boot_attempts() -> &'static std::sync::atomic::AtomicU64 {
    static ATTEMPTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    &ATTEMPTS
}

/// How long a caller waits for the plane to come up before being told it did
/// not. Generous for a local process start, and finite, which is the point.
const PLANE_BOOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The `node-host` plugin's service, when it is loaded.
fn node_host(kernel: &Kernel) -> Option<Arc<rebon_plugin_node_host::NodeHost>> {
    kernel
        .context()
        .get::<rebon_plugin_node_host::NodeHostService>()
}

/// Re-reads `kernelPlugins` and brings the running composition in line with it.
///
/// The reconciler the embedded composition used to have, on the plane. What it
/// restarts is exactly what changed: an entry whose configuration is identical
/// keeps running, because a restart costs a plugin whatever state it was
/// holding, and a reload that restarted everything would be a slower way of
/// closing the session.
///
/// Only reconciles a plane that is *already up*. Booting one here would turn a
/// reload into a first boot, which is a different decision with different
/// failure modes — and the caller asking to reload has a composition in mind
/// that it believes is running.
pub async fn reload_process_composition(
    plugins: &Arc<PluginRegistry>,
) -> Result<crate::plugin_plane::ReloadOutcome, String> {
    let Some(plane) = ensure_process_plugin_plane(plugins).await else {
        return Err(match composition_refusal() {
            Some(refusal) => format!("no composition is running: {}", refusal.message()),
            None => "no composition is running — `kernelPlugins` configures none".to_string(),
        });
    };

    let kernel = plugins.kernel();
    let wanted = node_host(kernel)
        .map(|host| host.wanted())
        .unwrap_or_else(|| crate::kernel_node_host::configured_entry_ids());
    let entries = crate::kernel_node_host::entries_for(kernel.context(), &wanted)?;

    plane
        .reload(&entries)
        .await
        .map_err(|error| format!("{error}"))
}

/// Every tool this build exposes to the plane, which is what the
/// `$rebon/tools` sentinel in a manifest expands to.
///
/// Rebuilt rather than remembered from the boot: a reload is the moment to
/// re-read what this build offers, and the list is cheap to produce.
pub(crate) fn exposed_tool_names(upstream: &Context) -> Vec<String> {
    let engine = EngineToolInvokeHost::new(
        upstream,
        // Runs on every plane reload, mid-session: a cwd that has since
        // vanished must not fail the reload, so fall back to the relative root.
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        &rebon_config::config_home_dir(),
        Vec::new(),
    );
    let engine = engine.engine();
    engine
        .eager_tool_snapshots()
        .into_iter()
        .map(|snapshot| snapshot.name)
        .chain(engine.deferred_tool_names())
        .collect()
}

async fn boot_process_plugin_plane(
    kernel: &Arc<Kernel>,
    wanted: &std::collections::BTreeSet<String>,
    even_with_no_entries: bool,
) -> Option<(Arc<crate::plugin_plane::PluginPlane>, rebon_kernel::Context)> {
    use crate::plugin_composition::{plane_composition, CompositionRoots};

    let config_dir = rebon_config::config_home_dir();
    let scripts = match plane_scripts() {
        Ok(scripts) => scripts,
        Err(reason) => return refuse(CompositionRefusal::Failed { reason }),
    };

    // Scoped, not plain: a plain `fork` shares the parent's service layer, so
    // the `provide_dual` below lands in the same layer `core-tools` already
    // provides `tool-registry` in and is refused as a duplicate. The whole
    // publish then silently does not happen -- which is what the
    // "tool-registry failed to publish" warning was, on every boot. A scoped
    // fork gets a layer of its own, where the pair may shadow the inherited
    // name for this subtree and vanish with it.
    let ctx = kernel.context().fork_scoped("plugin-plane");
    // Grants ride the plane's fork: teardown revokes them with everything else.
    register_credential_grants(&ctx, load_credential_grants(&config_dir));

    // The suspend-answer ask surface, published before anything can ask.
    let asks = ToolAskService::new(ctx.fork("tool-asks"), DEFAULT_ASK_TIMEOUT);
    if let Err(err) = ctx.provide_json(TOOL_ASKS_SERVICE, asks.clone()) {
        tracing::warn!(%err, "tool-asks surface failed to publish");
    }
    // …and into the process slot, which is how a front end reaches it. The
    // `provide_json` above lands on this *scoped* fork, so the root context a
    // TUI or a worker holds cannot resolve it; the slot is the seam that
    // crosses that layer. Cleared by an identity-guarded effect on the same
    // fork, so every teardown path — unload, shutdown, a failed boot — takes
    // it back out.
    set_process_tool_asks(asks.clone());
    {
        let asks = asks.clone();
        ctx.effect_labeled("plane tool-asks slot", move || {
            rebon_kernel::Disposer::new(move || {
                clear_process_tool_asks(&asks);
            })
        });
    }
    let workspace_root = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return refuse(CompositionRefusal::Failed {
                reason: format!("the current working directory is unreadable: {error}"),
            })
        }
    };
    let invoke_host = EngineToolInvokeHost::with_ask_surface(
        kernel.context(),
        workspace_root.clone(),
        &config_dir,
        load_tool_grants(&config_dir),
        Some(asks),
    );
    let builtin_names: Vec<String> = {
        let engine = invoke_host.engine();
        engine
            .eager_tool_snapshots()
            .into_iter()
            .map(|snapshot| snapshot.name)
            .chain(engine.deferred_tool_names())
            .collect()
    };

    let mut composition = match plane_composition(
        &config_dir,
        &CompositionRoots {
            payload: crate::plugin_composition::payload_root(&scripts.compose_root),
            runtime: scripts.compose_root.clone(),
        },
        &builtin_names,
    ) {
        Ok(composition) => composition,
        Err(reason) => {
            ctx.dispose();
            return refuse(CompositionRefusal::Failed { reason });
        }
    };
    for skipped in &composition.skipped {
        tracing::warn!(entry = %skipped, "composition entry not loaded");
    }
    // The registry's switches, applied before anything starts. `wanted` is
    // what the `node:*` rows asked for, and their defaults are the entries
    // the configuration already named — so a machine nobody has switched
    // anything on boots exactly the composition it booted before.
    composition
        .entries
        .retain(|entry| wanted.contains(&entry.id));
    composition
        .entries
        .extend(crate::kernel_node_host::unconfigured_entries(
            &config_dir,
            wanted,
            &composition.entries,
            &builtin_names,
        ));
    // Nothing configured is not a problem, and must not sound like one —
    // unless the caller has a plugin of its own to load, in which case an
    // empty composition is simply an empty composition.
    if composition.entries.is_empty() && !even_with_no_entries {
        ctx.dispose();
        return None;
    }

    // Only now is a runtime owed. Asked for after the composition rather than
    // before it, so the demand is only ever made of someone who needs it met.
    let node = match resolve_node() {
        Ok(node) => node,
        Err(reason) => {
            ctx.dispose();
            return refuse(CompositionRefusal::NoRuntime {
                entries: composition.entries.len(),
                reason,
            });
        }
    };

    // The three registration faces, published exactly as the embedded path
    // publishes them — what a composition provides is the same fact either way.
    let compose_tools =
        rebon_kernel_seats::kernel_compose_tools::ComposeToolRegistry::new(builtin_names.clone());
    // Both planes under one name, the way a session scope does it. The
    // registry keys typed and JSON by the same string, so a JSON-only
    // `tool-registry` here would hide the root's typed seat from every lookup
    // made under this fork — including the plane's own, which is where a
    // composition's tools now get registered.
    let registered = match kernel
        .context()
        .get::<rebon_core::tool_seat::ToolSeatService>()
    {
        Some(seat) => {
            ctx.provide_dual::<rebon_core::tool_seat::ToolSeatService>(seat, compose_tools.clone())
        }
        None => ctx.provide_json("tool-registry", compose_tools.clone()),
    };
    if let Err(err) = registered {
        tracing::warn!(%err, "composition tool-registry failed to publish");
    }
    // Still needed, and more so now. The pair above is published on the
    // plane's *own* layer, so it answers for the plane fork and its
    // descendants -- which is exactly what a scoped fork means, and exactly
    // what the publish is for. A session context is not one of those
    // descendants; it is a sibling. `SessionPluginTools::list` reads this slot
    // to put a composition's tools in front of a model, and there is no path
    // from a sibling fork to a scoped provider. The slot is the seam between
    // two subtrees, not a second copy of one.
    rebon_kernel_seats::kernel_compose_tools::set_process_compose_tools(compose_tools.clone());
    {
        let registry = compose_tools.clone();
        ctx.effect_labeled("plane tool-registry slot", move || {
            rebon_kernel::Disposer::new(move || {
                rebon_kernel_seats::kernel_compose_tools::clear_process_compose_tools(&registry);
            })
        });
    }

    let prompt_sections = rebon_kernel_seats::kernel_prompt_sections::ComposePromptSections::new();
    if let Err(err) = ctx.provide_json(
        rebon_kernel_seats::kernel_prompt_sections::SYSTEM_PROMPT_SERVICE,
        prompt_sections.clone(),
    ) {
        tracing::warn!(%err, "composition system-prompt registry failed to publish");
    }
    // The composition's sections reach the prompt through the process
    // `prompt-sections` seat like a Rust plugin's: one provider, registered
    // as an effect of the plane's fork so a plane teardown takes it off the
    // seat. The seat is looked up on the root, where `core-tools` provides
    // it (loaded by `process_plugin_registry` before `node_host()` can
    // answer, so it is there by the time any plane boots); a lookup that
    // starts at the root cannot be stopped by a layer in between.
    //
    // The id carries the boot attempt: a plane being stopped leaves the
    // slot (`take_running_plane`) before its fork is disposed, so a
    // successor booting in that window would otherwise collide with the
    // predecessor's registration and lose its sections until the next boot.
    let provider_id = format!(
        "{}#{}",
        rebon_kernel_seats::kernel_prompt_sections::NODE_COMPOSITION_PROVIDER_ID,
        boot_attempts().load(std::sync::atomic::Ordering::Relaxed)
    );
    match kernel
        .context()
        .get::<rebon_core::prompt_seat::PromptSeatService>()
    {
        Some(seat) => {
            if let Err(err) = seat.register(&ctx, &provider_id, prompt_sections.clone()) {
                tracing::warn!(%err, "composition prompt sections failed to join the prompt seat");
            }
        }
        None => tracing::warn!(
            "no prompt-sections seat on the process kernel; composition prompt sections will not reach the model"
        ),
    }

    let web_seat = rebon_kernel_seats::kernel_web_seat::WebSeat::new(
        rebon_kernel_seats::kernel_web_seat::WebSeatConfig::from_section(&composition.web),
    );
    rebon_kernel_seats::kernel_web_seat::set_process_web_seat(web_seat.clone());
    {
        let seat = web_seat.clone();
        ctx.effect_labeled("plane web-seat slot", move || {
            rebon_kernel::Disposer::new(move || {
                rebon_kernel_seats::kernel_web_seat::clear_process_web_seat(&seat);
            })
        });
    }

    let plane = match crate::plugin_plane::PluginPlane::start(
        crate::plugin_plane::PluginPlaneConfig {
            node,
            host_script: scripts.host_script,
            loader: scripts.loader,
            compose_root: scripts.compose_root,
            // Not said: the composition runtime resolves its own payload, the
            // same way whether rebon speaks or not. Saying nothing is what
            // keeps this side from knowing where a tree it does not own lives.
            payload_dir: None,
            structure: composition.structure.clone(),
            web: composition.web.clone(),
            modules: composition.modules.clone(),
            exposed_tools: builtin_names,
            exposed_seats: crate::plugin_plane::default_exposed_seats(),
            // Read once, before anything can consume it: what rebon offers a
            // model is settled when the composition is built, and a loop's
            // prompt assembly is handed the same list rebon dispatches from.
            tool_catalog: invoke_host.describe_catalog(),
            scope_id: None,
            working_directory: workspace_root,
            unary_call_timeout: None,
        },
        ctx.clone(),
        compose_tools.clone(),
        invoke_host.clone(),
    )
    .await
    {
        Ok(plane) => plane,
        Err(error) => {
            ctx.dispose();
            return refuse(CompositionRefusal::Failed {
                reason: error.to_string(),
            });
        }
    };
    compose_tools.bind_plane(plane.tool_dispatch());
    web_seat.bind_plane(plane.tool_dispatch());

    // One load per entry, in config order. A single bad entry is reported and
    // the rest of the composition still runs — the same rule the embedded
    // host's per-entry failure list expressed.
    for entry in &composition.entries {
        match plane.load_entry(entry).await {
            Ok(report) => {
                for provider in report
                    .extras
                    .get("webProviders")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                {
                    let (Some(kind), Some(id)) = (
                        provider.get("kind").and_then(serde_json::Value::as_str),
                        provider.get("id").and_then(serde_json::Value::as_str),
                    ) else {
                        continue;
                    };
                    let capability = match kind {
                        "search" => rebon_kernel_seats::kernel_web_seat::WebCapability::Search,
                        "fetch" => rebon_kernel_seats::kernel_web_seat::WebCapability::Fetch,
                        _ => continue,
                    };
                    let available = provider
                        .get("available")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    web_seat.mirror_registration(capability, id, available);
                }
            }
            Err(error) => {
                tracing::warn!(entry = %entry.id, %error, "composition entry failed to load");
            }
        }
    }
    tracing::info!(
        entries = composition.entries.len(),
        "kernel plugin composition booted on the plugin plane"
    );
    Some((plane, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal has to be actionable, because it replaces a silence.
    ///
    /// Before this, kernel plugins configured on a machine with no runtime
    /// produced nothing at all — not an error, not a log line, just a session
    /// missing a feature. Whatever else changes here, the message must still
    /// name the command that fixes it.
    #[test]
    fn a_missing_runtime_is_told_how_to_stop_being_missing() {
        let refusal = super::CompositionRefusal::NoRuntime {
            entries: 3,
            reason: "no usable Node runtime: none found".to_owned(),
        };
        let message = refusal.message();
        assert!(message.contains("rebon node install"), "{message}");
        assert!(
            message.contains("3 kernel plugins are configured"),
            "{message}"
        );
        assert!(message.contains("no usable Node runtime"), "{message}");
        // The two ways out that are not a download.
        assert!(
            message.contains(rebon_node_runtime::NODE_EXECUTABLE_ENV),
            "{message}"
        );
        assert!(message.contains("--from-path"), "{message}");

        let one = super::CompositionRefusal::NoRuntime {
            entries: 1,
            reason: "nope".to_owned(),
        };
        assert!(
            one.message().contains("1 kernel plugin is configured"),
            "{}",
            one.message()
        );
    }

    /// Blocked means "something is configured that cannot run", and nothing
    /// else. A machine with no runtime and no kernel plugins is in a normal
    /// state, and a panel that warned about it would be warning everyone —
    /// which is how a warning stops being read.
    #[test]
    fn only_a_configured_composition_with_no_runtime_counts_as_blocked() {
        let ready = ResolvedRuntime {
            version: "24.19.0".into(),
            executable: PathBuf::from("/usr/bin/node"),
            origin: "managed install".into(),
        };
        let case = |configured: usize, runtime: Option<ResolvedRuntime>| PlaneRuntimeStatus {
            configured_plugins: configured,
            runtime,
            problem: None,
            out_of_range: None,
            supported: ">=24.19.0 <25.0.0".to_owned(),
            installable: true,
        };

        assert!(
            case(2, None).is_blocked(),
            "configured, nothing to run them"
        );
        assert!(!case(0, None).is_blocked(), "nothing configured");
        assert!(
            !case(2, Some(ready.clone())).is_blocked(),
            "configured and ready"
        );
        assert!(
            !case(0, Some(ready)).is_blocked(),
            "ready, nothing asked for"
        );
    }

    #[test]
    fn credential_grants_parse_trimmed_and_deduped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.json"),
            r#"{
                "kernelPlugins": {
                    "plugins": [{ "id": "x", "name": "x" }],
                    "credentialGrants": ["REBON_TEST_GRANT_A", "  ", "REBON_TEST_GRANT_A", "REBON_TEST_GRANT_B"]
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            load_credential_grants(tmp.path()),
            vec!["REBON_TEST_GRANT_A", "REBON_TEST_GRANT_B"]
        );

        std::fs::write(tmp.path().join("config.json"), r#"{}"#).unwrap();
        assert!(load_credential_grants(tmp.path()).is_empty());
    }

    #[test]
    fn credential_grants_authorize_granted_env_refs_only() {
        use rebon_kernel_seats::kernel_config_seats::{ConfigSeatsPlugin, CREDENTIALS_SERVICE};

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.json"),
            r#"{ "customProviders": [{ "name": "cfg-ds", "apiKey": "sk-literal" }] }"#,
        )
        .unwrap();
        let kernel = rebon_kernel::Kernel::new();
        kernel
            .load(vec![Box::new(ConfigSeatsPlugin::new(
                tmp.path().to_path_buf(),
            ))])
            .expect("seats load");
        let grants_ctx = kernel.context().fork("grants");
        register_credential_grants(&grants_ctx, vec!["REBON_TEST_GRANT_OK".into()]);

        // Granted ref resolves from the environment.
        std::env::set_var("REBON_TEST_GRANT_OK", "sk-granted");
        let hit = kernel
            .context()
            .call_json(
                CREDENTIALS_SERVICE,
                "resolveEnv",
                serde_json::json!({ "ref": "REBON_TEST_GRANT_OK" }),
            )
            .expect("granted ref resolves");
        assert_eq!(hit["value"], "sk-granted");
        std::env::remove_var("REBON_TEST_GRANT_OK");

        // Any other ref stays fail-closed.
        let err = kernel
            .context()
            .call_json(
                CREDENTIALS_SERVICE,
                "resolveEnv",
                serde_json::json!({ "ref": "REBON_TEST_GRANT_OTHER" }),
            )
            .expect_err("ungranted ref denied");
        assert!(err.to_string().contains("not granted"), "{err}");

        // An env grant never leaks into provider-keyed credential requests.
        let err = kernel
            .context()
            .call_json(
                CREDENTIALS_SERVICE,
                "get",
                serde_json::json!({ "provider": "cfg-ds" }),
            )
            .expect_err("provider get stays denied");
        assert!(err.to_string().contains("not granted"), "{err}");
    }
}
