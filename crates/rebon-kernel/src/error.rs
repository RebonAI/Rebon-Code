use thiserror::Error;

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("service `{service}` already has a provider (while loading `{plugin}`)")]
    DuplicateProvider { plugin: String, service: String },

    #[error("plugin `{plugin}` requires service `{service}` but no plugin provides it")]
    MissingProvider { plugin: String, service: String },

    #[error("dependency cycle among plugins: {0:?}")]
    DependencyCycle(Vec<String>),

    #[error("service `{0}` is not registered")]
    ServiceNotFound(String),

    #[error("service `{service}` exists but not on the requested plane/type")]
    ServicePlaneMismatch { service: String },

    #[error("plugin `{plugin}` failed to apply: {message}")]
    PluginFailed { plugin: String, message: String },

    #[error("plugin `{0}` is already loaded")]
    AlreadyLoaded(String),

    /// A held reference whose registration is draining or already unloaded.
    ///
    /// The `[STALE_PROVIDER]` token is part of the wire contract: hosts and
    /// dynamically-typed plugins match on it, so it must not be reworded.
    #[error("[STALE_PROVIDER] service `{service}` is draining or unloaded")]
    ServiceClosed { service: String },

    /// Ambient lookup refused: the plugin resolved a service its manifest
    /// never declared.
    ///
    /// The `[UNAUTHORIZED_RESOLVE]` token is part of the wire contract. This
    /// blocks ambient lookup only — same-process plugins remain
    /// trusted-but-buggy, since a handle already handed out cannot be clawed
    /// back; real boundaries are isolates and processes.
    #[error(
        "[UNAUTHORIZED_RESOLVE] plugin `{plugin}` did not declare service `{service}` \
         (add it to inject/optional_inject/provides)"
    )]
    UnauthorizedResolve { plugin: String, service: String },

    #[error("{0}")]
    Other(String),
}
