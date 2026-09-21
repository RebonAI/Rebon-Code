//! Bridge configuration and the protocol data types it carries.
//!
//! This is everything the [`crate::api_client::BridgeApiClient`] trait passes
//! around, plus the richer [`BridgeConfig`] a concrete runtime needs to bring
//! an environment up.
//!
//! Pure data: no I/O, no async, and no path to another `rebon-*` crate. The
//! `dependency_contract` test in `lib.rs` holds that last part in place.

use serde::{Deserialize, Serialize};

use crate::projects::ProjectInfo;

/// How a bridge environment chooses session working directories.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpawnMode {
    /// One session in cwd, bridge tears down when it ends.
    #[default]
    SingleSession,
    /// Persistent server, every session gets an isolated git worktree.
    Worktree,
    /// Persistent server, every session shares cwd (can stomp each other).
    SameDir,
}

/// Well-known `worker_type` values a Rebon worker sends.
///
/// Sent as `BridgeConfig.worker_type` at environment registration so a
/// control surface can tell what kind of worker an environment runs.
/// RC stores and echoes it but never interprets it: it is an opaque
/// label, so [`BridgeConfig::worker_type`] stays a plain `String` and a
/// fork or a third-party worker can send whatever it likes. This enum is
/// only for ergonomic construction of the values *this* codebase sends.
///
/// These strings are a published wire contract: changing one is a
/// protocol change, not a rename.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WellKnownWorkerType {
    /// A Rebon session worker, i.e. `rebon rc` (wire value `rebon`).
    /// The default.
    #[default]
    Rebon,
    /// A Rebon worker in assistant mode (wire value `rebon_assistant`).
    RebonAssistant,
}

impl WellKnownWorkerType {
    /// Wire string sent as `BridgeConfig.worker_type`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rebon => "rebon",
            Self::RebonAssistant => "rebon_assistant",
        }
    }
}

/// Full runtime bridge configuration.
///
/// Shape a concrete bridge runtime needs to bring up an environment.
/// It is a superset of what status reporting alone needs, so a consumer
/// that only reports diagnostics should borrow this rather than keep a
/// narrower mirror of the same data that can drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeConfig {
    /// Working directory the bridge is serving.
    pub dir: String,
    /// Human-visible machine name (used in environment metadata).
    pub machine_name: String,
    /// Current git branch, or empty string when not in a git repo.
    pub branch: String,
    /// Git remote URL, or `None` when not in a git repo.
    pub git_repo_url: Option<String>,
    /// Maximum concurrent sessions the bridge will run.
    pub max_sessions: u32,
    /// Session spawn mode.
    pub spawn_mode: SpawnMode,
    /// Verbose logging flag.
    pub verbose: bool,
    /// Sandbox flag (enables sandboxed session execution).
    pub sandbox: bool,
    /// Client-generated UUID identifying this bridge instance.
    pub bridge_id: String,
    /// Opaque worker label (see [`WellKnownWorkerType`]); RC stores and
    /// lists it so a control surface can filter by origin.
    pub worker_type: String,
    /// Client-generated UUID for idempotent environment registration.
    pub environment_id: String,
    /// Backend-issued environment id to reuse on re-register. `None`
    /// when starting fresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reuse_environment_id: Option<String>,
    /// API base URL the bridge is polling.
    pub api_base_url: String,
    /// Session ingress base URL for WebSocket / SSE connections (may
    /// differ from `api_base_url` locally).
    pub session_ingress_url: String,
    /// Debug file path passed via `--debug-file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_file: Option<String>,
    /// Per-session timeout in milliseconds, as the service registered it.
    ///
    /// The runtime's poll loop stops a session that reaches this bound and
    /// reports [`crate::RuntimeStatus::TimedOut`]. Absent, the product
    /// default applies ([`crate::constants::DEFAULT_SESSION_TIMEOUT_MS`]);
    /// an explicit `0` turns the bound off rather than expiring the session
    /// on arrival.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_timeout_ms: Option<u64>,
    /// Projects this environment serves. One environment is one
    /// machine, so this is normally several checkouts. Optional on the
    /// wire: a registration without it (or with it empty) serves `dir`
    /// alone — see [`BridgeConfig::effective_projects`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<ProjectInfo>,
}

impl BridgeConfig {
    /// Minimal constructor useful for tests. Required fields (dir,
    /// branch, bridge_id, …) get sensible defaults; the caller
    /// overrides anything it cares about via field updates.
    pub fn minimal(
        bridge_id: impl Into<String>,
        environment_id: impl Into<String>,
        api_base_url: impl Into<String>,
        session_ingress_url: impl Into<String>,
    ) -> Self {
        Self {
            dir: String::new(),
            machine_name: String::new(),
            branch: String::new(),
            git_repo_url: None,
            max_sessions: 1,
            spawn_mode: SpawnMode::SingleSession,
            verbose: false,
            sandbox: false,
            bridge_id: bridge_id.into(),
            worker_type: WellKnownWorkerType::default().as_str().to_string(),
            environment_id: environment_id.into(),
            reuse_environment_id: None,
            api_base_url: api_base_url.into(),
            session_ingress_url: session_ingress_url.into(),
            debug_file: None,
            session_timeout_ms: None,
            projects: Vec::new(),
        }
    }

    /// The projects this registration advertises.
    ///
    /// `projects` when it is non-empty. Otherwise `dir` as the single
    /// default project — labelled after its last path component and
    /// carrying `git_repo_url` and `branch` — so a registration from
    /// before projects existed still serves the directory it names. An
    /// empty `dir` as well means no projects at all.
    pub fn effective_projects(&self) -> Vec<ProjectInfo> {
        if !self.projects.is_empty() {
            return self.projects.clone();
        }
        if self.dir.is_empty() {
            return Vec::new();
        }
        let mut project = ProjectInfo::from_path(self.dir.clone());
        project.remote = self.git_repo_url.clone();
        project.branch = Some(self.branch.clone()).filter(|branch| !branch.is_empty());
        vec![project]
    }
}

/// Per-work-item type discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkDataType {
    /// Regular session work.
    Session,
    /// Health check probe issued by the server.
    Healthcheck,
}

/// Inner payload of a [`WorkResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkData {
    /// Discriminant — session vs healthcheck.
    #[serde(rename = "type")]
    pub data_type: WorkDataType,
    /// Opaque id the server uses to correlate this work item.
    pub id: String,
}

/// Response from `poll_for_work`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkResponse {
    /// Server-issued work item id.
    pub id: String,
    /// Always the literal string `"work"` — present so the wire shape
    /// stays a discriminated union.
    #[serde(rename = "type")]
    pub response_type: String,
    /// Environment id the work item was queued on.
    pub environment_id: String,
    /// Coarse state string (e.g. `"ready"`, `"running"`).
    pub state: String,
    /// Inner payload.
    pub data: WorkData,
    /// Base64url-encoded JSON that decodes into a work secret.
    /// Kept as a string in the api layer; the decode lives in a
    /// higher layer that does JSON parsing.
    pub secret: String,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
}

/// What a session work item asks the runner to do: which project to run
/// in, what to say first, and which existing Rebon session to continue.
///
/// Carried as the `session` key of a polled work item ([`WorkItem`]),
/// **next to** `data` rather than inside it. `WorkData` and
/// `WorkResponse` are built with struct literals by crates outside the
/// protocol, so growing either would break them; a sibling key is the
/// same additive change on the wire (a decoder that predates it ignores
/// it) without that cost. It is one optional object rather than three
/// optional fields because the three travel together: a healthcheck has
/// none of them, and session work always names its project.
///
/// None of this is credential material, so none of it is in the work
/// secret: the prompt in particular stays out of
/// [`crate::work_secret::WorkSecret`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWork {
    /// The project to run in: a `path` the environment advertised
    /// ([`crate::projects::ProjectInfo::path`]), byte for byte. RC refuses
    /// to queue work for anything else, but the list can change after
    /// the work was queued, so a runner still checks it against its own.
    pub project: String,
    /// The first prompt. Absent when the work only (re)opens a session —
    /// a resume with nothing new to say, or a reconnect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// An existing Rebon session (the machine's own transcript id, not an
    /// RC session id) to continue instead of starting a new one. Always
    /// satisfies [`valid_rebon_session_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_rebon_session_id: Option<String>,
}

/// Longest Rebon session id a work item may name.
pub const MAX_REBON_SESSION_ID: usize = 128;

/// Whether `id` is acceptable as [`SessionWork::resume_rebon_session_id`]:
/// 1 to [`MAX_REBON_SESSION_ID`] characters of `[A-Za-z0-9._-]`, not
/// starting with `.`.
///
/// The id comes from a controller and ends up naming something on the
/// machine's disk, so both ends check it: RC refuses to queue anything
/// else, and a runner must refuse it too rather than trust the server.
/// The rule excludes separators, `..` as a whole component and hidden
/// names; it does not promise the session exists.
pub fn valid_rebon_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REBON_SESSION_ID
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// A polled work item: the [`WorkResponse`] envelope plus, for session
/// work, the [`SessionWork`] that says what to run.
///
/// ```json
/// {
///   "id": "wrk_…", "type": "work", "environment_id": "env_…",
///   "state": "leased", "secret": "…", "created_at": "…",
///   "data": {"type": "session", "id": "sess_…"},
///   "session": {
///     "project": "/home/me/src/app",
///     "prompt": "fix the flaky test",
///     "resume_rebon_session_id": "7f3c…"
///   }
/// }
/// ```
///
/// `session` is absent for a healthcheck, and from a server that
/// predates it; a runner treats session work without it as work it
/// cannot place and stops it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    /// The envelope every work item has.
    #[serde(flatten)]
    pub response: WorkResponse,
    /// What to run, for session work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionWork>,
}

impl From<WorkResponse> for WorkItem {
    fn from(response: WorkResponse) -> Self {
        Self {
            response,
            session: None,
        }
    }
}

/// Body of `POST /v1/environments/{env}/work`: a controller queues work.
///
/// Unknown keys are refused, so a controller that misspells `project`
/// gets a 400 rather than work in some other directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnqueueWorkRequest {
    /// Session work or a healthcheck probe.
    #[serde(rename = "type")]
    pub work_type: WorkDataType,
    /// The first prompt. Session work needs a prompt, a resume target,
    /// or both; a healthcheck takes neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// An existing RC session on this environment to queue more work
    /// for; a new session is created when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Where to run: a project `path` the environment advertises. May be
    /// omitted when the target session already has one (it must then
    /// match if given) or when the environment advertises exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// An existing Rebon session to continue; see
    /// [`SessionWork::resume_rebon_session_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_rebon_session_id: Option<String>,
}

impl EnqueueWorkRequest {
    /// Session work that starts with `prompt`.
    pub fn session(prompt: impl Into<String>) -> Self {
        Self {
            work_type: WorkDataType::Session,
            prompt: Some(prompt.into()),
            session_id: None,
            project: None,
            resume_rebon_session_id: None,
        }
    }

    /// A healthcheck probe.
    pub fn healthcheck() -> Self {
        Self {
            work_type: WorkDataType::Healthcheck,
            prompt: None,
            session_id: None,
            project: None,
            resume_rebon_session_id: None,
        }
    }

    /// Run in `project`.
    pub fn in_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Queue for an existing RC session.
    pub fn for_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Continue an existing Rebon session.
    pub fn resuming(mut self, rebon_session_id: impl Into<String>) -> Self {
        self.resume_rebon_session_id = Some(rebon_session_id.into());
        self
    }
}

/// Response to `POST /v1/environments/{env}/work` (201).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnqueuedWork {
    /// The queued item.
    pub work_id: String,
    /// The session it belongs to; `null` for a healthcheck.
    pub session_id: Option<String>,
}

/// Result of a successful environment registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RegisteredEnvironment {
    /// Backend-issued environment id.
    pub environment_id: String,
    /// Shared secret used to authenticate subsequent polls.
    pub environment_secret: String,
}

/// Lease-heartbeat response shape returned by
/// [`crate::api_client::BridgeApiClient::heartbeat_work`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HeartbeatOutcome {
    /// Whether the backend extended the lease (`false` typically means
    /// the work item has already been reclaimed by another worker).
    pub lease_extended: bool,
    /// Coarse state returned by the backend.
    pub state: String,
}

/// A `control_response` event sent back to a session, e.g. a permission
/// decision answering a pending prompt.
///
/// The inner `response.response` blob stays opaque JSON; a layer that has
/// SDK message types can type it more tightly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PermissionResponseEvent {
    /// Always the literal string `"control_response"`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// The nested control response body.
    pub response: PermissionResponseBody,
}

impl PermissionResponseEvent {
    /// Convenience constructor that stamps the `type` field and wraps
    /// `inner`.
    pub fn new(inner: PermissionResponseBody) -> Self {
        Self {
            event_type: "control_response".to_string(),
            response: inner,
        }
    }
}

/// Inner `response` body of a [`PermissionResponseEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PermissionResponseBody {
    /// Always the literal string `"success"`.
    pub subtype: String,
    /// Matches the `requestId` of the pending permission prompt.
    pub request_id: String,
    /// Opaque decision payload —
    /// `{ behavior: "allow" | "deny", ... }` on the wire. Kept as raw
    /// JSON here to avoid pulling in the SDK message types.
    pub response: serde_json::Value,
}

impl PermissionResponseBody {
    /// Convenience constructor that stamps `subtype = "success"`.
    pub fn success(request_id: impl Into<String>, response: serde_json::Value) -> Self {
        Self {
            subtype: "success".to_string(),
            request_id: request_id.into(),
            response,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_config_round_trips_through_json() {
        let mut cfg = BridgeConfig::minimal(
            "bridge-uuid-1",
            "env-uuid-1",
            "https://api.example.com",
            "wss://session.example.com",
        );
        cfg.dir = "/home/user/repo".to_string();
        cfg.branch = "main".to_string();
        cfg.git_repo_url = Some("git@github.com:user/repo.git".to_string());
        cfg.max_sessions = 4;
        cfg.spawn_mode = SpawnMode::Worktree;
        cfg.verbose = true;
        cfg.session_timeout_ms = Some(3_600_000);

        let json = serde_json::to_string(&cfg).unwrap();
        // camelCase wire fields
        assert!(json.contains("\"bridgeId\":\"bridge-uuid-1\""));
        assert!(json.contains("\"environmentId\":\"env-uuid-1\""));
        assert!(json.contains("\"gitRepoUrl\":\"git@github.com:user/repo.git\""));
        assert!(json.contains("\"spawnMode\":\"worktree\""));
        assert!(json.contains("\"sessionTimeoutMs\":3600000"));

        let decoded: BridgeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, cfg);
    }

    #[test]
    fn bridge_config_omits_none_fields() {
        let cfg = BridgeConfig::minimal("b", "e", "https://a", "wss://s");
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("reuseEnvironmentId"));
        assert!(!json.contains("debugFile"));
        assert!(!json.contains("sessionTimeoutMs"));
    }

    #[test]
    fn a_registration_without_projects_still_parses_and_serves_dir() {
        // A registration body from before projects existed.
        let raw = r#"{
            "dir": "/home/user/repo",
            "machineName": "workshop",
            "branch": "main",
            "gitRepoUrl": "git@github.com:user/repo.git",
            "maxSessions": 1,
            "spawnMode": "single-session",
            "verbose": false,
            "sandbox": false,
            "bridgeId": "b",
            "workerType": "rebon",
            "environmentId": "e",
            "apiBaseUrl": "https://a",
            "sessionIngressUrl": "wss://s"
        }"#;
        let cfg: BridgeConfig = serde_json::from_str(raw).expect("old shape parses");
        assert!(cfg.projects.is_empty());
        assert_eq!(
            cfg.effective_projects(),
            vec![ProjectInfo::new("/home/user/repo", "repo")
                .with_remote("git@github.com:user/repo.git")
                .with_branch("main")]
        );
        // And an empty list is not written back out.
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("projects"), "{json}");
    }

    #[test]
    fn explicit_projects_win_over_dir() {
        let mut cfg = BridgeConfig::minimal("b", "e", "https://a", "wss://s");
        cfg.dir = "/ignored".into();
        cfg.projects = vec![
            ProjectInfo::new("/srv/one", "One"),
            ProjectInfo::from_path("/srv/two").with_branch("dev"),
        ];
        assert_eq!(cfg.effective_projects(), cfg.projects);

        let value = serde_json::to_value(&cfg).unwrap();
        assert_eq!(
            value["projects"],
            serde_json::json!([
                {"path": "/srv/one", "label": "One"},
                {"path": "/srv/two", "label": "two", "branch": "dev"}
            ])
        );
        let back: BridgeConfig = serde_json::from_value(value).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn no_dir_and_no_projects_serves_nothing() {
        let cfg = BridgeConfig::minimal("b", "e", "https://a", "wss://s");
        assert!(cfg.effective_projects().is_empty());

        // A branch is only reported when there is one.
        let mut bare = cfg.clone();
        bare.dir = "/tmp/scratch".into();
        assert_eq!(
            bare.effective_projects(),
            vec![ProjectInfo::new("/tmp/scratch", "scratch")]
        );
    }

    #[test]
    fn spawn_mode_serializes_as_kebab_case() {
        assert_eq!(
            serde_json::to_string(&SpawnMode::SingleSession).unwrap(),
            "\"single-session\""
        );
        assert_eq!(
            serde_json::to_string(&SpawnMode::Worktree).unwrap(),
            "\"worktree\""
        );
        assert_eq!(
            serde_json::to_string(&SpawnMode::SameDir).unwrap(),
            "\"same-dir\""
        );
    }

    #[test]
    fn well_known_worker_type_wire_strings_are_stable() {
        assert_eq!(WellKnownWorkerType::Rebon.as_str(), "rebon");
        assert_eq!(
            WellKnownWorkerType::RebonAssistant.as_str(),
            "rebon_assistant"
        );
        assert_eq!(WellKnownWorkerType::default(), WellKnownWorkerType::Rebon);
        // `minimal` registers as a Rebon worker.
        let cfg = BridgeConfig::minimal("b", "e", "https://a", "wss://s");
        assert_eq!(cfg.worker_type, "rebon");
    }

    #[test]
    fn work_response_snake_case_round_trip() {
        let raw = r#"{
            "id": "work-1",
            "type": "work",
            "environment_id": "env-1",
            "state": "ready",
            "data": {"type": "session", "id": "sess-1"},
            "secret": "base64url-payload",
            "created_at": "2026-04-09T00:00:00.000Z"
        }"#;
        let decoded: WorkResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(decoded.id, "work-1");
        assert_eq!(decoded.response_type, "work");
        assert_eq!(decoded.environment_id, "env-1");
        assert_eq!(decoded.data.data_type, WorkDataType::Session);
        assert_eq!(decoded.data.id, "sess-1");
        assert_eq!(decoded.secret, "base64url-payload");
    }

    fn session_work_item() -> WorkItem {
        WorkItem {
            response: WorkResponse {
                id: "wrk_1".into(),
                response_type: "work".into(),
                environment_id: "env_1".into(),
                state: "leased".into(),
                data: WorkData {
                    data_type: WorkDataType::Session,
                    id: "sess_1".into(),
                },
                secret: "opaque".into(),
                created_at: "2026-09-16T00:00:00.000Z".into(),
            },
            session: Some(SessionWork {
                project: "/srv/app".into(),
                prompt: Some("fix it".into()),
                resume_rebon_session_id: Some("0192-abc".into()),
            }),
        }
    }

    #[test]
    fn a_work_item_puts_the_session_next_to_data() {
        let item = session_work_item();
        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "id": "wrk_1",
                "type": "work",
                "environment_id": "env_1",
                "state": "leased",
                "data": {"type": "session", "id": "sess_1"},
                "secret": "opaque",
                "created_at": "2026-09-16T00:00:00.000Z",
                "session": {
                    "project": "/srv/app",
                    "prompt": "fix it",
                    "resume_rebon_session_id": "0192-abc"
                }
            })
        );
        let back: WorkItem = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, item);
        // A decoder that only knows the envelope still reads the item.
        let envelope: WorkResponse = serde_json::from_value(value).unwrap();
        assert_eq!(envelope, item.response);
    }

    #[test]
    fn a_work_item_without_a_session_is_just_the_envelope() {
        let mut item = session_work_item();
        item.session = None;
        let value = serde_json::to_value(&item).unwrap();
        assert!(value.get("session").is_none(), "{value}");
        assert_eq!(value, serde_json::to_value(&item.response).unwrap());
        // What an older server answered parses, with no session.
        let old: WorkItem = serde_json::from_value(value).unwrap();
        assert_eq!(old, WorkItem::from(item.response));

        let minimal = SessionWork {
            project: "/srv/app".into(),
            prompt: None,
            resume_rebon_session_id: None,
        };
        assert_eq!(
            serde_json::to_value(&minimal).unwrap(),
            serde_json::json!({"project": "/srv/app"})
        );
        assert!(serde_json::from_str::<SessionWork>("{}").is_err());
    }

    #[test]
    fn an_enqueue_request_serializes_only_what_it_sets() {
        assert_eq!(
            serde_json::to_value(EnqueueWorkRequest::session("hi")).unwrap(),
            serde_json::json!({"type": "session", "prompt": "hi"})
        );
        assert_eq!(
            serde_json::to_value(EnqueueWorkRequest::healthcheck()).unwrap(),
            serde_json::json!({"type": "healthcheck"})
        );
        let full = EnqueueWorkRequest::session("hi")
            .in_project("/srv/app")
            .for_session("sess_1")
            .resuming("abc");
        let value = serde_json::to_value(&full).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "session",
                "prompt": "hi",
                "session_id": "sess_1",
                "project": "/srv/app",
                "resume_rebon_session_id": "abc"
            })
        );
        assert_eq!(
            serde_json::from_value::<EnqueueWorkRequest>(value).unwrap(),
            full
        );
        // The pre-project body still parses.
        let old: EnqueueWorkRequest =
            serde_json::from_str(r#"{"type":"session","prompt":"hi","session_id":"sess_1"}"#)
                .unwrap();
        assert_eq!(old, EnqueueWorkRequest::session("hi").for_session("sess_1"));
        // A misspelt key is refused rather than ignored.
        assert!(serde_json::from_str::<EnqueueWorkRequest>(
            r#"{"type":"session","prompt":"hi","projcet":"/x"}"#
        )
        .is_err());
    }

    #[test]
    fn enqueued_work_keeps_a_null_session_for_a_healthcheck() {
        let value = serde_json::to_value(EnqueuedWork {
            work_id: "wrk_1".into(),
            session_id: None,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"work_id": "wrk_1", "session_id": null})
        );
    }

    #[test]
    fn rebon_session_ids_are_plain_names() {
        for good in [
            "a",
            "0192f3c4-5e6f-7a8b-9c0d-1e2f3a4b5c6d",
            "session_1.2",
            "x..y",
            &"a".repeat(MAX_REBON_SESSION_ID),
        ] {
            assert!(valid_rebon_session_id(good), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "a/b",
            "a\\b",
            "../etc",
            "a b",
            "ü",
            "a\0",
            &"a".repeat(MAX_REBON_SESSION_ID + 1),
        ] {
            assert!(!valid_rebon_session_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn work_data_type_accepts_healthcheck() {
        let raw = r#"{"type":"healthcheck","id":"hc-1"}"#;
        let decoded: WorkData = serde_json::from_str(raw).unwrap();
        assert_eq!(decoded.data_type, WorkDataType::Healthcheck);
        assert_eq!(decoded.id, "hc-1");
    }

    #[test]
    fn permission_response_event_has_fixed_type_string() {
        let body =
            PermissionResponseBody::success("req-1", serde_json::json!({"behavior": "allow"}));
        let event = PermissionResponseEvent::new(body);
        assert_eq!(event.event_type, "control_response");
        assert_eq!(event.response.subtype, "success");
        assert_eq!(event.response.request_id, "req-1");
        assert_eq!(
            event.response.response,
            serde_json::json!({"behavior": "allow"})
        );

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"control_response\""));
        assert!(json.contains("\"subtype\":\"success\""));
    }

    #[test]
    fn registered_environment_snake_case_round_trip() {
        let raw = r#"{"environment_id":"env-abc","environment_secret":"shhh"}"#;
        let decoded: RegisteredEnvironment = serde_json::from_str(raw).unwrap();
        assert_eq!(decoded.environment_id, "env-abc");
        assert_eq!(decoded.environment_secret, "shhh");
    }

    #[test]
    fn heartbeat_outcome_snake_case_round_trip() {
        let raw = r#"{"lease_extended":true,"state":"running"}"#;
        let decoded: HeartbeatOutcome = serde_json::from_str(raw).unwrap();
        assert!(decoded.lease_extended);
        assert_eq!(decoded.state, "running");
    }
}
