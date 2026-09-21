//! `rebon rc serve`: one machine, one environment, many sessions.
//!
//! On start: take the per-machine lock, load (or mint) the environment
//! identity, resolve the projects, register — which rotates the
//! environment secret — and hand the secret to the client. Then:
//!
//! * a **poller** long-polls for work while there is a free session slot
//!   and hands each item over with the slot it took;
//! * the **loop** plans each item, supersedes a local item already
//!   serving the same RC session, and runs it
//!   ([`crate::session::run_work_item`]);
//! * every [`ServeTiming::project_refresh`] the project list is read again
//!   and sent when it changed;
//! * a poll refused as unauthorized means the secret was rotated (or the
//!   device revoked): register again, and give up only if that is refused
//!   too.
//!
//! On shutdown every item stands down — streams closed, session leases
//! released — and the loop waits for them briefly. Work leases are left to
//! lapse: RC hands those items out again, and a runner started later picks
//! them up and resumes the same sessions without repeating their prompts.
//! The environment is not deregistered; the machine keeps its identity.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rebon_bridge::api_client::{BridgeApiError, PollOptions};
use rebon_bridge::config::{BridgeConfig, SpawnMode, WellKnownWorkerType, WorkItem};
use rebon_bridge::projects::ProjectInfo;
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

use crate::core::work::{plan_work, Backoff, WorkPlan};
use crate::files::{EnvironmentIdentity, RcDir};
use crate::projects::{project_paths, resolve_projects, ProjectSource};
use crate::session::{run_work_item, Interrupt, WorkContext, WorkOutcome};

/// The loop's own knobs.
#[derive(Debug, Clone, Copy)]
pub struct ServeTiming {
    pub project_refresh: Duration,
    pub retry_base: Duration,
    pub retry_max: Duration,
    /// How long shutdown waits for items to stand down.
    pub shutdown_grace: Duration,
}

impl Default for ServeTiming {
    fn default() -> Self {
        Self {
            project_refresh: Duration::from_secs(30),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(15),
        }
    }
}

/// What `serve` needs besides the work context.
pub struct ServeConfig {
    pub server: String,
    pub machine_name: String,
    pub max_sessions: usize,
    pub configured: ProjectSource,
    pub project_flags: Vec<PathBuf>,
    pub start_dir: PathBuf,
    pub timing: ServeTiming,
}

/// The environment as registered.
#[derive(Clone)]
struct Registered {
    environment_id: String,
    secret: String,
}

impl std::fmt::Debug for Registered {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Registered")
            .field("environment_id", &self.environment_id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

enum Polled {
    Item(Box<WorkItem>, OwnedSemaphorePermit),
    Unauthorized,
}

struct Active {
    work_id: String,
    interrupt: watch::Sender<Interrupt>,
    finished: watch::Receiver<bool>,
}

/// Serve until `shutdown` resolves.
///
/// `context` is a template: its `environment_id` is filled in by the
/// registration, and replaced if a re-registration changes it.
pub async fn serve(
    dir: &RcDir,
    template: WorkContext,
    config: ServeConfig,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let _lock = dir.lock_serve()?;
    let mut identity = dir
        .load_environment()?
        .filter(|identity| identity.server == config.server)
        .unwrap_or_else(|| EnvironmentIdentity::fresh(&config.server));
    let mut projects = current_projects(&config)?;
    let api = Arc::clone(&template.api);
    let registered = register(&*api, dir, &mut identity, &config, &projects).await?;
    tracing::info!(
        environment_id = %registered.environment_id,
        projects = projects.len(),
        "rebon rc: registered"
    );
    eprintln!(
        "rebon rc: serving {} project(s) as environment {}",
        projects.len(),
        registered.environment_id
    );
    for project in &projects {
        eprintln!("  {}  ({})", project.path, project.label);
    }

    let mut ctx = Arc::new(WorkContext {
        environment_id: registered.environment_id.clone(),
        ..template
    });
    let (credentials_tx, credentials_rx) = watch::channel(registered);
    let slots = Arc::new(Semaphore::new(config.max_sessions.max(1)));
    let (polled_tx, mut polled_rx) = mpsc::channel(1);
    let poller = tokio::spawn(poll_for_work(
        Arc::clone(&api),
        credentials_rx,
        Arc::clone(&slots),
        polled_tx,
        config.timing,
    ));

    let mut active: HashMap<String, Active> = HashMap::new();
    let mut tasks: JoinSet<WorkOutcome> = JoinSet::new();
    let mut refresh = tokio::time::interval(config.timing.project_refresh);
    refresh.tick().await;
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            _ = &mut shutdown => break Ok(()),
            Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                match joined {
                    Ok(outcome) => forget(&mut active, &outcome),
                    Err(error) => tracing::error!(%error, "rebon rc: a work item task failed"),
                }
            }
            polled = polled_rx.recv() => match polled {
                Some(Polled::Item(item, permit)) => {
                    let plan = plan_work(&item, &project_paths(&projects), |rc| {
                        ctx.ledger.remembered(rc)
                    });
                    dispatch(&ctx, plan, permit, &mut active, &mut tasks);
                }
                Some(Polled::Unauthorized) => {
                    match register(&*api, dir, &mut identity, &config, &projects).await {
                        Ok(registered) => {
                            if registered.environment_id != ctx.environment_id {
                                ctx = Arc::new(WorkContext {
                                    environment_id: registered.environment_id.clone(),
                                    ..clone_context(&ctx)
                                });
                            }
                            let _ = credentials_tx.send(registered);
                        }
                        Err(error) => break Err(error),
                    }
                }
                None => break Err(anyhow::anyhow!("the work poller stopped")),
            },
            _ = refresh.tick() => {
                match current_projects(&config) {
                    Ok(next) if next != projects => {
                        match api.update_projects(&ctx.environment_id, &next).await {
                            Ok(()) => {
                                tracing::info!(projects = next.len(), "rebon rc: project list updated");
                                projects = next;
                            }
                            Err(error) => tracing::warn!(%error, "rebon rc: could not update the project list"),
                        }
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "rebon rc: the configured projects cannot be read; keeping the current list"),
                }
            }
        }
    };
    poller.abort();
    for entry in active.values() {
        let _ = entry.interrupt.send(Interrupt::Shutdown);
    }
    let drained = tokio::time::timeout(config.timing.shutdown_grace, async {
        while let Some(joined) = tasks.join_next().await {
            if let Ok(outcome) = joined {
                forget(&mut active, &outcome);
            }
        }
    })
    .await;
    if drained.is_err() {
        tracing::warn!("rebon rc: some sessions did not stand down in time");
        tasks.abort_all();
    }
    result
}

fn current_projects(config: &ServeConfig) -> anyhow::Result<Vec<ProjectInfo>> {
    resolve_projects(
        (config.configured)()?,
        &config.project_flags,
        &config.start_dir,
    )
}

/// `WorkContext` holds trait objects and is not `Clone`; this is the one
/// place a copy is made, for a changed environment id.
fn clone_context(ctx: &WorkContext) -> WorkContext {
    WorkContext {
        api: Arc::clone(&ctx.api),
        streams: Arc::clone(&ctx.streams),
        host: Arc::clone(&ctx.host),
        ledger: ctx.ledger.clone(),
        projection: Arc::clone(&ctx.projection),
        environment_id: ctx.environment_id.clone(),
        timing: ctx.timing,
        policy: ctx.policy,
        pid: ctx.pid,
    }
}

/// The registration body for this machine.
pub fn bridge_config(
    identity: &EnvironmentIdentity,
    config: &ServeConfig,
    projects: &[ProjectInfo],
) -> BridgeConfig {
    let mut bridge = BridgeConfig::minimal(
        identity.bridge_id.clone(),
        identity.client_environment_id.clone(),
        config.server.clone(),
        config.server.clone(),
    );
    bridge.worker_type = WellKnownWorkerType::Rebon.as_str().to_string();
    bridge.machine_name = config.machine_name.clone();
    bridge.dir = projects
        .first()
        .map(|project| project.path.clone())
        .unwrap_or_default();
    bridge.max_sessions = u32::try_from(config.max_sessions).unwrap_or(u32::MAX);
    // Every session runs in its own project directory, in a worker of its
    // own; two sessions of one project share the directory.
    bridge.spawn_mode = SpawnMode::SameDir;
    bridge.reuse_environment_id = identity.environment_id.clone();
    bridge.projects = projects.to_vec();
    bridge
}

/// Register (again), with backoff on transient failures. The secret RC
/// answers with replaces every copy the runner holds.
async fn register(
    api: &dyn crate::ports::EnvironmentApi,
    dir: &RcDir,
    identity: &mut EnvironmentIdentity,
    config: &ServeConfig,
    projects: &[ProjectInfo],
) -> anyhow::Result<Registered> {
    let mut backoff = Backoff::new(config.timing.retry_base, config.timing.retry_max);
    loop {
        match api
            .register_bridge_environment(&bridge_config(identity, config, projects))
            .await
        {
            Ok(registered) => {
                api.set_environment_secret(&registered.environment_secret);
                identity.environment_id = Some(registered.environment_id.clone());
                dir.save_environment(identity)?;
                return Ok(Registered {
                    environment_id: registered.environment_id,
                    secret: registered.environment_secret,
                });
            }
            Err(error) if error.is_transient() => {
                let delay = backoff.next_delay();
                tracing::warn!(%error, delay_ms = delay.as_millis() as u64, "rebon rc: registration failed; retrying");
                tokio::time::sleep(delay).await;
            }
            Err(BridgeApiError::Unauthorized(detail)) => anyhow::bail!(
                "the RC server refused this device ({detail}); run `rebon rc login` again"
            ),
            Err(error) => anyhow::bail!("the RC server refused the registration: {error}"),
        }
    }
}

async fn poll_for_work(
    api: Arc<dyn crate::ports::EnvironmentApi>,
    mut credentials: watch::Receiver<Registered>,
    slots: Arc<Semaphore>,
    polled: mpsc::Sender<Polled>,
    timing: ServeTiming,
) {
    let mut backoff = Backoff::new(timing.retry_base, timing.retry_max);
    loop {
        let Ok(permit) = Arc::clone(&slots).acquire_owned().await else {
            return;
        };
        let current = credentials.borrow_and_update().clone();
        match api
            .poll_for_work_item(
                &current.environment_id,
                &current.secret,
                PollOptions::default(),
            )
            .await
        {
            Ok(None) => backoff.reset(),
            Ok(Some(item)) => {
                backoff.reset();
                if polled
                    .send(Polled::Item(Box::new(item), permit))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(BridgeApiError::Unauthorized(_)) => {
                drop(permit);
                if polled.send(Polled::Unauthorized).await.is_err() {
                    return;
                }
                // Wait for the loop to register again before polling.
                if credentials.changed().await.is_err() {
                    return;
                }
            }
            Err(error) => {
                drop(permit);
                let delay = backoff.next_delay();
                tracing::warn!(%error, delay_ms = delay.as_millis() as u64, "rebon rc: polling for work failed");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

fn dispatch(
    ctx: &Arc<WorkContext>,
    plan: WorkPlan,
    permit: OwnedSemaphorePermit,
    active: &mut HashMap<String, Active>,
    tasks: &mut JoinSet<WorkOutcome>,
) {
    let (interrupt_tx, interrupt_rx) = watch::channel(Interrupt::Running);
    let (finished_tx, finished_rx) = watch::channel(false);
    let mut previous = None;
    if let WorkPlan::Session(session) = &plan {
        if let Some(old) = active.remove(&session.rc_session_id) {
            tracing::info!(
                rc_session = %session.rc_session_id,
                old_work = %old.work_id,
                new_work = %session.work_id,
                "rebon rc: a newer work item takes over a session"
            );
            let _ = old.interrupt.send(Interrupt::Superseded);
            previous = Some(old.finished);
        }
        active.insert(
            session.rc_session_id.clone(),
            Active {
                work_id: session.work_id.clone(),
                interrupt: interrupt_tx,
                finished: finished_rx,
            },
        );
    }
    let ctx = Arc::clone(ctx);
    tasks.spawn(async move {
        // The item it replaces stands down first: two followers on one
        // session would each think the other's frames were theirs.
        if let Some(mut previous) = previous {
            while !*previous.borrow_and_update() {
                if previous.changed().await.is_err() {
                    break;
                }
            }
        }
        let outcome = run_work_item(ctx, plan, interrupt_rx).await;
        let _ = finished_tx.send(true);
        drop(permit);
        outcome
    });
}

fn forget(active: &mut HashMap<String, Active>, outcome: &WorkOutcome) {
    let Some(rc_session_id) = &outcome.rc_session_id else {
        return;
    };
    if active
        .get(rc_session_id)
        .is_some_and(|entry| entry.work_id == outcome.work_id)
    {
        active.remove(rc_session_id);
    }
}
