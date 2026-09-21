//! # rebon-bridge — remote-control bridge layer
//!
//! The crate is split in two halves: a foundation layer of pure protocol
//! types and decision logic with no transport, and a runtime layer that
//! drives an async bridge over those types.
//!
//! ## Foundation layer
//!
//! * [`session_id_compat`] — `cse_*` ↔ `session_*` re-tag helpers plus an
//!   injected `Fn() -> bool` kill switch.
//! * [`permission_callbacks`] — the typed permission response shape
//!   (`behavior`, `updated_input`, `updated_permissions`, `message`), its
//!   wire-string parser, and an in-process handler registry with a
//!   request-scoped response handler and unsubscribe protocol.
//! * [`remote_permission`] — maps a controller's permission decision onto
//!   one of Rebon's option ids, one-shot unless the runner opts into
//!   remote "always" rules or edited input.
//! * [`active_handle`] — generic `ActiveHandleSlot<H: BridgeHandle>` with
//!   a set/get/compat-id surface; publishing the new compat id is returned
//!   as data instead of fired as a hidden I/O.
//! * [`uuid_set`] — `BoundedUuidSet`, a ring-buffer FIFO set used for
//!   echo/re-delivery dedup on the ingress pipeline.
//! * [`session_stream`] — the session stream's frame envelope
//!   ([`session_stream::SessionFrame`]) and its close codes. This is the
//!   single definition of what travels over `GET /v1/sessions/{id}/stream`,
//!   so both ends of the socket deserialize the same type.
//! * [`control_request`] — decision tree for server-initiated control
//!   requests. Every request that changes something is answered from a
//!   verdict the caller reached by trying it, and a success says whether
//!   it applies now or from the next turn; transport I/O stays
//!   caller-owned.
//! * [`constants`] — bridge string / timeout constants and the
//!   outbound-only error.
//!
//! ## Runtime layer (async, tokio-backed)
//!
//! * [`config`] — bridge configuration and protocol data types:
//!   [`config::BridgeConfig`], [`config::WorkResponse`],
//!   [`config::PermissionResponseEvent`], and the rest. Pure data, no I/O.
//! * [`work_secret`] — [`work_secret::WorkSecret`], the decoded form of
//!   the otherwise opaque [`config::WorkResponse::secret`] blob. Shape and
//!   codec only; minting one is the server's business.
//! * [`projects`] — what an environment serves: the projects one machine
//!   advertises ([`projects::ProjectInfo`]) and the controller's view of an
//!   environment. Pure data.
//! * [`devices`] — binding a device to an account: the body and answer of
//!   `POST /v1/devices` ([`devices::IssuedDevice`]). Pure data.
//! * [`history`] — the controller's read side: a session's persisted events
//!   and the account's session list, both cursor-paged
//!   ([`history::SessionEventPage`], [`history::SessionPage`]). Pure data,
//!   shared by the server and the HTTP client.
//! * [`api_client`] — the async [`api_client::BridgeApiClient`] trait, plus
//!   an [`api_client::InMemoryBridgeApiClient`] test double that records
//!   calls and replays scripted poll responses.
//! * `http_client` — behind the **`http`** cargo feature, a concrete
//!   `HttpBridgeApiClient` over the HTTP routes, including the two
//!   controller reads of [`history`] as inherent methods (`reqwest`,
//!   rustls). Off by default so crates that only need the trait and the
//!   protocol types never link an HTTP stack.
//! * `stream_client` — behind the **`ws`** cargo feature, the session
//!   stream's WebSocket client (`tokio-tungstenite`, rustls): connect with a
//!   work secret's `ingress_url`, a shareable object-safe sink, a reader,
//!   and a close reason precise enough to tell "superseded" from "lease
//!   gone" from "network". Off by default for the same reason as `http`.
//! * [`runtime`] — high-level [`runtime::start_bridge_runtime`] entry point
//!   that registers an environment, spawns a poll task, and returns an
//!   `Arc<`[`runtime::ReplBridgeHandle`]`>` which implements
//!   [`active_handle::BridgeHandle`], so it can be handed straight to
//!   whatever owns the session.
//!
//! ## Dependency contract
//!
//! This crate **must not** depend on any other `rebon-*` crate: it is a
//! leaf. The `dependency_contract` test at the bottom of this file enforces
//! that. External crates (tokio, async-trait, serde, reqwest) are fine;
//! `rebon-*` crates are not.
//!
//! The `http` and `ws` features are off by default, so a plain workspace
//! build does not cover them. Build and test this crate with
//! `--all-features` to exercise both transports.
//!
//! ## Out of scope
//!
//! * The session runner (child process spawning, NDJSON parsing,
//!   stdin/stdout piping) — it sits on top of the runtime layer and is not
//!   implemented here.
//! * The REPL-specific bring-up glue that reads session storage, git, and
//!   OAuth to produce a [`config::BridgeConfig`]. That belongs in an upper
//!   crate.
//! * Full SDK message ingress routing, eligibility predicates and title
//!   extraction — those need SDK message types, which this crate
//!   deliberately does not depend on.

#![deny(missing_docs)]

pub mod active_handle;
pub mod api_client;
pub mod config;
pub mod constants;
pub mod control_request;
pub mod devices;
pub mod history;
#[cfg(feature = "http")]
pub mod http_client;
pub mod permission_callbacks;
pub mod projects;
pub mod remote_permission;
pub mod runtime;
pub mod session_id_compat;
pub mod session_stream;
#[cfg(feature = "ws")]
pub mod stream_client;
pub mod uuid_set;
pub mod work_secret;

pub use active_handle::{ActiveHandleChange, ActiveHandleSlot, BridgeHandle};
pub use api_client::{
    BridgeApiClient, BridgeApiError, BridgeApiResult, InMemoryBridgeApiClient, PollOptions,
    RecordedCall, RecordedMethod,
};
pub use config::{
    valid_rebon_session_id, BridgeConfig, EnqueueWorkRequest, EnqueuedWork, HeartbeatOutcome,
    PermissionResponseBody, PermissionResponseEvent, RegisteredEnvironment, SessionWork, SpawnMode,
    WellKnownWorkerType, WorkData, WorkDataType, WorkItem, WorkResponse,
};
pub use constants::{
    DEFAULT_SESSION_TIMEOUT_MS, OUTBOUND_ONLY_ERROR, REMOTE_CONTROL_DISCONNECTED_MSG,
};
pub use control_request::{
    plan_server_control_response, unsupported_control_error, ControlApplied, ControlEffect,
    ControlVerdict, InitializeResponseBody, ServerControlRequestPlanInput,
    ServerControlRequestSubtype, ServerControlResponsePlan,
};
pub use devices::{IssueDeviceRequest, IssuedDevice};
pub use history::{
    PageRequest, ReportedSessionState, SessionEventPage, SessionEventRecord, SessionPage,
    SessionSummary,
};
#[cfg(feature = "http")]
pub use http_client::{DeviceCredentials, HttpBridgeApiClient, HttpClientConfig};
pub use permission_callbacks::{
    parse_behavior, BridgePermissionBehavior, BridgePermissionResponse, DeliveryOutcome,
    OpaqueJson, PermissionResponseHandler, PermissionResponseRegistry, RegistrationGuard,
};
pub use projects::{EnvironmentList, EnvironmentSummary, ProjectInfo, ProjectList, MAX_PROJECTS};
pub use remote_permission::{
    map_remote_decision, parse_remote_decision, RebonPermissionOption, RemoteDecisionParseError,
    RemotePermissionAnswer, RemotePermissionPolicy, RemotePermissionRefusal,
};
pub use runtime::{start_bridge_runtime, ReplBridgeHandle, RuntimeOptions, RuntimeStatus};
pub use session_id_compat::{
    clear_cse_shim_gate, set_cse_shim_gate, to_compat_session_id, to_infra_session_id, CseShimGate,
};
pub use session_stream::{
    stamp_event_id, ControlResponseBody, DeliveredFrame, FrameOrigin, SessionFrame,
    SessionRunState, UnknownFrame,
};
#[cfg(feature = "ws")]
pub use stream_client::{
    CloseReason, SessionFrameSink, SessionStream, SessionStreamError, SessionStreamOptions,
    SessionStreamRx, SessionStreamTx,
};
pub use uuid_set::BoundedUuidSet;
pub use work_secret::WorkSecret;

#[cfg(test)]
mod dependency_contract {
    #[test]
    fn no_rebon_deps_in_cargo_toml() {
        let cargo = include_str!("../Cargo.toml");
        for line in cargo.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            assert!(
                !trimmed.starts_with("rebon-"),
                "rebon-bridge must stay dep-free of other rebon crates; found: {line}"
            );
        }
    }
}
