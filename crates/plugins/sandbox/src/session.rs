//! Settings on disk → the sandbox a session's commands run under.
//!
//! Shared by every entrypoint that builds an executor — the TUI, the ACP
//! server, and the headless harness — because an editor-driven session runs
//! the same tools against the same workspace, and a `sandbox` block that only
//! bound in one of them would be a setting the user could not tell was off.
//! None of them call in here directly: they resolve the `session-sandbox`
//! seat, which is how the plugin switch gets to mean something.

use std::path::Path;
use std::sync::Arc;

use crate::exec::SandboxPolicy;
use crate::runtime::{SandboxSettings, SessionSandbox};

/// Resolve the session's sandbox from `settings.json`.
///
/// Returns `None` when nothing should be attached: either the sandbox
/// is off, or its configuration is broken. A broken configuration is
/// logged at error level and the session continues **unsandboxed**
/// rather than failing to start — this runs inside the constructor
/// for the whole session, where there is nowhere to surface a fatal
/// error to the user, and a Rebon that refuses to open is a worse
/// outcome than one that says loudly it is not confining anything.
pub fn resolve_sandbox_policy(cwd: &Path) -> Option<SandboxPolicy> {
    match setup_sandbox_session(cwd) {
        Ok(session) => {
            for note in &session.notes {
                tracing::warn!("{note}");
            }
            for error in &session.support.errors {
                tracing::warn!(sandbox_dependency = %error, "sandbox dependency missing");
            }
            if !session.enabled {
                return None;
            }
            tracing::info!(
                sandbox_platform = ?session.platform,
                sandbox_override = ?session.override_mode,
                "rebon: sandbox policy attached"
            );
            Some(SandboxPolicy::from_session(session))
        }
        Err(err) => {
            tracing::error!(
                "sandbox configuration is invalid, continuing without a sandbox: {err}"
            );
            None
        }
    }
}

/// Read the sandbox settings, start the proxy the network rules need, and
/// compile the session.
///
/// The proxy has to exist **before** the session is built, because its ports
/// go into the session config that every wrapped command's environment is
/// derived from. It is handed to the session as a resource so it stops when
/// the session does — a listener outliving its session would keep answering
/// with a policy nobody is enforcing any more.
fn setup_sandbox_session(
    cwd: &Path,
) -> Result<SessionSandbox, crate::runtime::session::SandboxSetupError> {
    let (mut settings, mut notes) =
        crate::runtime::session::load_settings(&rebon_config::config_home_dir(), cwd)?;

    // Only when domain rules were actually written. A session with no
    // network rules must not pay for a listener, and pointing the
    // environment at a proxy nobody needs would route every request through
    // an extra hop for nothing.
    let policy = crate::proxy::DomainPolicy::new(
        settings.session.network.allowed_domains.iter(),
        settings.session.network.denied_domains.iter(),
    );
    let resources: Option<Arc<dyn std::any::Any + Send + Sync>> =
        if settings.enabled && !policy.is_empty() {
            start_sandbox_proxy(policy, &mut settings, &mut notes)
        } else {
            None
        };

    Ok(crate::runtime::session::build_with(
        settings,
        cwd,
        notes,
        crate::runtime::session::SessionOptions {
            resources,
            // The process-wide store the `/sandbox` violations tab reads.
            // It keeps the `tracing::warn!` the default sink emitted, so the
            // audit trail is unchanged and the panel gains a source — see
            // [`crate::violations`].
            violation_sink: Some(crate::violations::sink()),
        },
    ))
}

fn start_sandbox_proxy(
    policy: crate::proxy::DomainPolicy,
    settings: &mut SandboxSettings,
    notes: &mut Vec<String>,
) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
    // Bound synchronously on the current runtime; the accept loops go on it.
    // Without a runtime there is nothing to spawn onto, which happens in
    // tests and in tools that build a session outside an async context.
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        notes.push(
            "sandbox: domain rules are configured but there is no async runtime to start the \
             proxy on, so the network will be blocked entirely rather than filtered"
                .to_string(),
        );
        return None;
    };

    // Only Linux needs the socket files — see `crate::proxy::server`.
    // Per-process so two Rebon sessions on one machine do not collide.
    let socket_dir = if cfg!(target_os = "linux") {
        Some(std::env::temp_dir().join(format!("rebon-sandbox-{}", std::process::id())))
    } else {
        None
    };

    match crate::proxy::start_blocking(policy, socket_dir.as_deref(), &handle) {
        Ok(proxy) => {
            let endpoints = proxy.endpoints().clone();
            tracing::info!(
                http_port = endpoints.http_port,
                socks_port = endpoints.socks_port,
                "rebon: sandbox proxy listening"
            );
            settings.session.runtime.http_proxy_port = Some(endpoints.http_port);
            settings.session.runtime.socks_proxy_port = Some(endpoints.socks_port);
            settings.session.runtime.http_proxy_socket = endpoints.http_socket;
            settings.session.runtime.socks_proxy_socket = endpoints.socks_socket;
            Some(Arc::new(proxy))
        }
        Err(error) => {
            // Left unset on purpose. The wrap then reports
            // `domain_rules_without_proxy`, which says the command got no
            // network at all — the truth — rather than pointing it at a port
            // nobody is listening on, where every request would fail with a
            // connection refused that names the wrong thing.
            notes.push(format!(
                "sandbox: the proxy could not be started ({error}), so domain rules cannot be \
                 applied and network-restricted commands will have no network at all"
            ));
            None
        }
    }
}
