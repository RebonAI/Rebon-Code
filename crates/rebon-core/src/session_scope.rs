//! The session-scope seat: which session a kernel context belongs to.
//!
//! A host forks one kernel scope per session and provides this on it, so
//! anything running under that scope can ask which session it is in without
//! being handed the id through every call.
//!
//! **Why it is the host's and not a plugin's.** Four feature plugins used to
//! each fork under `SessionOpened` and provide a private `<id>/session`
//! marker answering the same one question, plus a `HashMap<session_id,
//! Context>` and a `SessionClosed` handler and an unload disposer to hold the
//! other end of that lifetime. Four byte-identical copies of the same idea,
//! none with a production reader. The scope a session runs in is the host's
//! to own — it already forks it, already disposes it — so the seat lives here
//! and the host provides it once.
//!
//! **What that changed.** Unloading a feature plugin no longer tears down
//! anything session-scoped, because a feature plugin no longer owns anything
//! session-scoped. Its tools and producers still leave their seats the moment
//! it unloads, which is what the model sees. A plugin that later needs its
//! own per-session state forks under the session then, and owns that fork's
//! lifetime the way those four used to.
//!
//! Both planes are served under one name: the typed [`SessionScopeService`]
//! for Rust callers and a JSON facade answering `session_id` for
//! dynamically-typed hosts, which is what the markers offered.

use std::sync::Arc;

use rebon_kernel::{Context, JsonService, KernelError, Service};

/// Service name for the session scope, on both planes.
pub const SESSION_SCOPE_SERVICE: &str = "session/scope";

/// The session a kernel scope belongs to.
pub struct SessionScope {
    session_id: String,
}

impl SessionScope {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
        }
    }

    /// The session this scope was forked for.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl JsonService for SessionScope {
    fn call(
        &self,
        method: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, KernelError> {
        match method {
            "session_id" => Ok(serde_json::Value::String(self.session_id.clone())),
            other => Err(KernelError::Other(format!(
                "{SESSION_SCOPE_SERVICE} has no method `{other}`"
            ))),
        }
    }
}

/// Typed definition for the kernel's `session/scope` service.
pub struct SessionScopeService;

impl Service for SessionScopeService {
    type Interface = SessionScope;
    const NAME: &'static str = SESSION_SCOPE_SERVICE;
}

/// Put the scope on `ctx`, on both planes.
///
/// The host calls this on the scope it forked for the session, before it
/// announces the session, so a `SessionOpened` handler that asks already
/// gets an answer.
pub fn provide(ctx: &Context, session_id: &str) -> Result<(), KernelError> {
    let scope = Arc::new(SessionScope::new(session_id));
    ctx.provide_dual::<SessionScopeService>(scope.clone(), scope)
}

/// The session scope `ctx` sits under, if it sits under one.
pub fn session_scope(ctx: &Context) -> Option<Arc<SessionScope>> {
    ctx.get::<SessionScopeService>()
}

/// Whether `ctx` — the session scope, or anything forked under it — belongs
/// to `session_id`.
///
/// This is the question the four plugin markers existed to answer.
pub fn session_scope_is(ctx: &Context, session_id: &str) -> bool {
    session_scope(ctx).is_some_and(|scope| scope.session_id() == session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_kernel::Kernel;

    fn session(kernel: &Kernel, id: &str) -> Context {
        let ctx = kernel.context().fork_scoped(&format!("session/{id}"));
        provide(&ctx, id).unwrap();
        ctx
    }

    #[test]
    fn a_scope_answers_the_session_it_was_forked_for() {
        let kernel = Kernel::new();
        let ctx = session(&kernel, "sess-a");

        assert_eq!(
            session_scope(&ctx).map(|scope| scope.session_id().to_string()),
            Some("sess-a".to_string())
        );
        assert!(session_scope_is(&ctx, "sess-a"));
        assert!(!session_scope_is(&ctx, "sess-b"));
    }

    /// Anything forked under the session inherits the answer, which is what
    /// makes this usable from inside a tool call rather than only at the
    /// scope's top edge.
    #[test]
    fn a_fork_under_the_session_inherits_it() {
        let kernel = Kernel::new();
        let ctx = session(&kernel, "sess-a");

        let nested = ctx.fork("some-plugin").fork("deeper");

        assert!(session_scope_is(&nested, "sess-a"));
    }

    /// Two live sessions do not cross: each scope has its own service layer,
    /// so one session's lookup can never land on the other's.
    #[test]
    fn two_sessions_do_not_cross() {
        let kernel = Kernel::new();
        let first = session(&kernel, "sess-a");
        let second = session(&kernel, "sess-b");

        assert!(session_scope_is(&first, "sess-a"));
        assert!(session_scope_is(&second, "sess-b"));
        assert!(!session_scope_is(&first, "sess-b"));
        assert!(!session_scope_is(&second, "sess-a"));
    }

    /// Disposing one session takes its scope out of the tree and leaves the
    /// other standing. The host disposes a generation this way.
    #[test]
    fn disposing_one_session_leaves_the_other() {
        let kernel = Kernel::new();
        let first = session(&kernel, "sess-a");
        let second = session(&kernel, "sess-b");

        first.dispose();

        assert!(
            !first.has_service(SESSION_SCOPE_SERVICE),
            "the disposed session's scope is gone from the namespace"
        );
        assert!(!session_scope_is(&first, "sess-a"));
        assert!(
            session_scope_is(&second, "sess-b"),
            "the other session is untouched"
        );
    }

    /// Rebinding the same id — a resume or a hand-over — is a fresh scope,
    /// and the old one going away does not take the new one with it.
    #[test]
    fn rebinding_a_session_id_is_a_fresh_scope() {
        let kernel = Kernel::new();
        let old = session(&kernel, "sess-a");
        let new = session(&kernel, "sess-a");

        old.dispose();

        assert!(session_scope_is(&new, "sess-a"));
    }

    /// The JSON plane answers the same question, because a dynamically-typed
    /// host has no way to name the typed interface.
    #[test]
    fn the_json_plane_answers_session_id() {
        let kernel = Kernel::new();
        let ctx = session(&kernel, "sess-a");

        assert_eq!(
            ctx.call_json(SESSION_SCOPE_SERVICE, "session_id", serde_json::Value::Null)
                .unwrap(),
            serde_json::Value::String("sess-a".to_string())
        );
        assert!(ctx
            .call_json(SESSION_SCOPE_SERVICE, "nope", serde_json::Value::Null)
            .is_err());
    }

    /// A context outside any session has no scope, rather than a wrong one.
    #[test]
    fn the_root_has_no_session_scope() {
        let kernel = Kernel::new();

        assert!(session_scope(kernel.context()).is_none());
        assert!(!session_scope_is(kernel.context(), "sess-a"));
    }
}
