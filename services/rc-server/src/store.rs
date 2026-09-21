//! SQLite persistence.
//!
//! One connection behind a `Mutex`, opened WAL + `synchronous = FULL`,
//! with an additive `CREATE TABLE IF NOT EXISTS` schema applied in a
//! single `execute_batch` at open time. Every caller reaches this
//! module from a `spawn_blocking` task, so the methods are plain
//! blocking functions.
//!
//! Credentials are never stored in the clear: devices, environments
//! and per-work session tokens are all kept as HMAC digests (see
//! [`crate::auth`] for the domain separators).

use std::{collections::HashMap, path::Path as StdPath, sync::Mutex, time::Duration};

use rebon_bridge::config::{BridgeConfig, WorkDataType};
use rebon_bridge::projects::ProjectInfo;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::ids::{generate_id, TokenDigest};

/// Lifecycle states a work item moves through.
///
/// `ready → leased → acked → done`, with `stopped` reachable from any
/// non-terminal state and `leased | acked → ready` on lease expiry.
pub mod work_state {
    /// Queued, waiting for a poller.
    pub const READY: &str = "ready";
    /// Handed to a poller; the lease clock is running.
    pub const LEASED: &str = "leased";
    /// The worker confirmed receipt; heartbeats extend the lease.
    pub const ACKED: &str = "acked";
    /// Finished normally.
    pub const DONE: &str = "done";
    /// Superseded or explicitly stopped.
    pub const STOPPED: &str = "stopped";
}

/// Lifecycle states a session row moves through.
pub mod session_state {
    /// Work is queued for the session but no worker has taken it.
    pub const QUEUED: &str = "queued";
    /// A worker holds the session.
    pub const RUNNING: &str = "running";
    /// Closed; it no longer shows as active to controllers.
    pub const ARCHIVED: &str = "archived";
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS accounts (
    account_id        TEXT PRIMARY KEY NOT NULL,
    issuer            TEXT,
    subject           TEXT,
    created_at_unix   INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS accounts_identity
    ON accounts(issuer, subject) WHERE issuer IS NOT NULL AND subject IS NOT NULL;

CREATE TABLE IF NOT EXISTS devices (
    device_id              TEXT PRIMARY KEY NOT NULL,
    account_id             TEXT NOT NULL REFERENCES accounts(account_id),
    label                  TEXT NOT NULL,
    refresh_hmac           BLOB NOT NULL CHECK(length(refresh_hmac) = 32),
    access_hmac            BLOB CHECK(access_hmac IS NULL OR length(access_hmac) = 32),
    access_expires_at_unix INTEGER,
    created_at_unix        INTEGER NOT NULL,
    last_seen_at_unix      INTEGER,
    revoked_at_unix        INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS devices_refresh ON devices(refresh_hmac);
CREATE UNIQUE INDEX IF NOT EXISTS devices_access ON devices(access_hmac)
    WHERE access_hmac IS NOT NULL;
CREATE INDEX IF NOT EXISTS devices_account ON devices(account_id);

CREATE TABLE IF NOT EXISTS environments (
    environment_id         TEXT PRIMARY KEY NOT NULL,
    account_id             TEXT NOT NULL REFERENCES accounts(account_id),
    device_id              TEXT NOT NULL REFERENCES devices(device_id),
    client_environment_id  TEXT NOT NULL,
    secret_hmac            BLOB NOT NULL CHECK(length(secret_hmac) = 32),
    bridge_id              TEXT NOT NULL,
    machine_name           TEXT NOT NULL,
    dir                    TEXT NOT NULL,
    branch                 TEXT NOT NULL,
    git_repo_url           TEXT,
    worker_type            TEXT NOT NULL,
    max_sessions           INTEGER NOT NULL,
    spawn_mode             TEXT NOT NULL,
    config_json            TEXT NOT NULL,
    created_at_unix        INTEGER NOT NULL,
    last_seen_at_unix      INTEGER NOT NULL,
    deregistered_at_unix   INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS environments_client_key
    ON environments(account_id, client_environment_id);
CREATE UNIQUE INDEX IF NOT EXISTS environments_secret ON environments(secret_hmac);
CREATE INDEX IF NOT EXISTS environments_account ON environments(account_id, created_at_unix);
CREATE INDEX IF NOT EXISTS environments_device ON environments(device_id);

CREATE TABLE IF NOT EXISTS environment_projects (
    environment_id  TEXT NOT NULL REFERENCES environments(environment_id),
    position        INTEGER NOT NULL,
    path            TEXT NOT NULL,
    label           TEXT NOT NULL,
    remote          TEXT,
    branch          TEXT,
    PRIMARY KEY (environment_id, path)
);
CREATE INDEX IF NOT EXISTS environment_projects_order
    ON environment_projects(environment_id, position);

CREATE TABLE IF NOT EXISTS work (
    work_id             TEXT PRIMARY KEY NOT NULL,
    environment_id      TEXT NOT NULL REFERENCES environments(environment_id),
    work_type           TEXT NOT NULL,
    state               TEXT NOT NULL,
    session_id          TEXT,
    prompt              TEXT,
    session_token_hmac  BLOB CHECK(session_token_hmac IS NULL OR length(session_token_hmac) = 32),
    created_at_unix     INTEGER NOT NULL,
    updated_at_unix     INTEGER NOT NULL,
    leased_at_unix      INTEGER,
    lease_deadline_unix INTEGER,
    force               INTEGER NOT NULL DEFAULT 0,
    project_path        TEXT,
    resume_rebon_session_id TEXT
);
CREATE INDEX IF NOT EXISTS work_queue ON work(environment_id, state, created_at_unix);
CREATE INDEX IF NOT EXISTS work_lease ON work(state, lease_deadline_unix);
CREATE INDEX IF NOT EXISTS work_session ON work(session_id, state);
CREATE UNIQUE INDEX IF NOT EXISTS work_session_token ON work(session_token_hmac)
    WHERE session_token_hmac IS NOT NULL;

CREATE TABLE IF NOT EXISTS sessions (
    session_id       TEXT PRIMARY KEY NOT NULL,
    environment_id   TEXT NOT NULL REFERENCES environments(environment_id),
    account_id       TEXT NOT NULL REFERENCES accounts(account_id),
    state            TEXT NOT NULL,
    created_at_unix  INTEGER NOT NULL,
    updated_at_unix  INTEGER NOT NULL,
    project_path     TEXT,
    rebon_session_id TEXT
);
CREATE INDEX IF NOT EXISTS sessions_environment ON sessions(environment_id, state);
CREATE INDEX IF NOT EXISTS sessions_account ON sessions(account_id, updated_at_unix);
CREATE INDEX IF NOT EXISTS sessions_activity
    ON sessions(account_id, updated_at_unix, session_id);

CREATE TABLE IF NOT EXISTS session_events (
    event_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL REFERENCES sessions(session_id),
    kind            TEXT NOT NULL,
    payload_json    TEXT NOT NULL,
    created_at_unix INTEGER NOT NULL,
    dedupe_key      TEXT
);
CREATE INDEX IF NOT EXISTS session_events_session ON session_events(session_id, event_id);
CREATE INDEX IF NOT EXISTS session_events_kind ON session_events(session_id, kind, event_id);

-- The answer that currently holds each prompt. A row
-- exists only while a claim is held: the runner refusing the answer
-- deletes it. `connection_id` and `worker_connection_id` are the
-- in-memory socket ids of the answering controller and of the worker the
-- answer was routed to; they are only compared, never trusted across a
-- restart (see `Store::record_answer`).
CREATE TABLE IF NOT EXISTS session_answers (
    session_id           TEXT NOT NULL REFERENCES sessions(session_id),
    request_id           TEXT NOT NULL,
    event_id             INTEGER NOT NULL,
    device_id            TEXT NOT NULL,
    connection_id        INTEGER NOT NULL,
    worker_connection_id INTEGER NOT NULL,
    answered_at_unix     INTEGER NOT NULL,
    PRIMARY KEY (session_id, request_id)
);

CREATE TABLE IF NOT EXISTS audit (
    audit_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    actor           TEXT NOT NULL,
    action          TEXT NOT NULL,
    target          TEXT NOT NULL,
    created_at_unix INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS audit_created ON audit(created_at_unix);
";

/// Columns added after a table first shipped. `CREATE TABLE IF NOT
/// EXISTS` leaves an existing table alone, so a database created by an
/// older build gets these with `ALTER TABLE … ADD COLUMN` at open time.
/// Additive only, and every one is nullable: an old row reads as "not
/// known".
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("work", "project_path", "TEXT"),
    ("work", "resume_rebon_session_id", "TEXT"),
    ("sessions", "project_path", "TEXT"),
    ("sessions", "rebon_session_id", "TEXT"),
    ("session_events", "dedupe_key", "TEXT"),
];

/// Schema that refers to [`ADDED_COLUMNS`], so it can only run once
/// [`migrate`] has added them.
const POST_MIGRATION_SCHEMA: &str = "
CREATE UNIQUE INDEX IF NOT EXISTS session_events_dedupe
    ON session_events(session_id, dedupe_key) WHERE dedupe_key IS NOT NULL;
";

/// Bring a database created by an older build up to [`SCHEMA`].
fn migrate(connection: &Connection) -> rusqlite::Result<()> {
    for (table, column, declaration) in ADDED_COLUMNS {
        let present: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
            params![table, column],
            |row| row.get(0),
        )?;
        if !present {
            // Identifiers cannot be bound; these come from the constant
            // above, never from input.
            connection.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {declaration};"
            ))?;
        }
    }
    connection.execute_batch(POST_MIGRATION_SCHEMA)
}

/// Owns the single SQLite connection and every statement run against it.
pub struct Store {
    connection: Mutex<Connection>,
}

/// A device as resolved from one of its credentials.
#[derive(Debug, Clone)]
pub struct DeviceAuth {
    /// Device this credential belongs to.
    pub device_id: String,
    /// Account that owns the device.
    pub account_id: String,
    /// Whether the device has been revoked (the caller maps this to 404).
    pub revoked: bool,
}

/// One row of `GET /v1/devices`.
#[derive(Debug, Clone)]
pub struct DeviceSummary {
    /// Device id.
    pub device_id: String,
    /// Human label supplied at issuance.
    pub label: String,
    /// Issuance time.
    pub created_at_unix: i64,
    /// Last time a credential for this device authenticated a request.
    pub last_seen_at_unix: Option<i64>,
    /// Revocation time, when revoked.
    pub revoked_at_unix: Option<i64>,
}

/// An environment as resolved from its secret, before scope checks.
///
/// Deliberately returned even when dead so the route layer can tell
/// "wrong credential" (401) from "right credential, dead resource"
/// (404) and from "right credential, wrong environment" (403).
#[derive(Debug, Clone)]
pub struct EnvironmentAuth {
    /// Environment the secret belongs to.
    pub environment_id: String,
    /// Owning account.
    pub account_id: String,
    /// Device that registered the environment.
    pub device_id: String,
    /// Whether the environment has been deregistered.
    pub deregistered: bool,
    /// Whether the registering device has been revoked (secrets cascade dead).
    pub device_revoked: bool,
}

impl EnvironmentAuth {
    /// Whether the environment is still usable.
    pub fn alive(&self) -> bool {
        !self.deregistered && !self.device_revoked
    }
}

/// One row of `GET /v1/environments`. Never carries the secret.
#[derive(Debug, Clone)]
pub struct EnvironmentSummary {
    /// Backend-issued environment id.
    pub environment_id: String,
    /// Client-generated idempotency key from `BridgeConfig.environment_id`.
    pub client_environment_id: String,
    /// Device that registered it.
    pub device_id: String,
    /// `BridgeConfig.bridge_id`.
    pub bridge_id: String,
    /// `BridgeConfig.machine_name`.
    pub machine_name: String,
    /// `BridgeConfig.dir`.
    pub dir: String,
    /// `BridgeConfig.branch`.
    pub branch: String,
    /// `BridgeConfig.git_repo_url`.
    pub git_repo_url: Option<String>,
    /// `BridgeConfig.worker_type`.
    pub worker_type: String,
    /// `BridgeConfig.max_sessions`.
    pub max_sessions: i64,
    /// `BridgeConfig.spawn_mode` in its wire (kebab-case) form.
    pub spawn_mode: String,
    /// The projects the environment advertises, in its order.
    pub projects: Vec<ProjectInfo>,
    /// First registration time.
    pub created_at_unix: i64,
    /// Last authenticated request from the environment.
    pub last_seen_at_unix: i64,
    /// Deregistration time, when deregistered.
    pub deregistered_at_unix: Option<i64>,
}

/// Outcome of a registration upsert.
#[derive(Debug, Clone)]
pub struct Registration {
    /// Backend-issued environment id (stable across re-registration).
    pub environment_id: String,
    /// Whether a new row was inserted rather than an existing one reused.
    pub created: bool,
}

/// A work item successfully leased to a poller.
#[derive(Debug, Clone)]
pub struct ClaimedWork {
    /// Work item id.
    pub work_id: String,
    /// Session vs healthcheck.
    pub work_type: WorkDataType,
    /// Session the item belongs to; `None` for a healthcheck.
    pub session_id: Option<String>,
    /// Creation time, echoed to the client as `created_at`.
    pub created_at_unix: i64,
    /// Project path to run in; `None` for a healthcheck, and for session
    /// work queued by a build that predates projects.
    pub project_path: Option<String>,
    /// First prompt, when the item has one.
    pub prompt: Option<String>,
    /// Rebon session to continue, when the item names one.
    pub resume_rebon_session_id: Option<String>,
}

/// Work a controller asks to queue, already shape-checked by the route.
#[derive(Debug, Clone, Copy)]
pub struct NewWork<'a> {
    /// Session vs healthcheck.
    pub work_type: WorkDataType,
    /// Existing session to queue for; a new one is minted when absent.
    pub session_id: Option<&'a str>,
    /// First prompt.
    pub prompt: Option<&'a str>,
    /// Requested project path.
    pub project: Option<&'a str>,
    /// Rebon session to continue.
    pub resume_rebon_session_id: Option<&'a str>,
}

/// What [`Store::enqueue_work`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enqueued {
    /// Queued.
    Queued {
        /// The new work item.
        work_id: String,
        /// Its session; `None` for a healthcheck.
        session_id: Option<String>,
    },
    /// The named session exists but not on this environment and account.
    /// Reported like any foreign resource: not found.
    ForeignSession,
    /// No project the environment advertises could be chosen: the one
    /// named is not advertised, differs from the session's, or none was
    /// named and the environment does not have exactly one.
    ProjectRefused,
}

/// A work item resolved from its session token.
#[derive(Debug, Clone)]
pub struct WorkByToken {
    /// Work item id.
    pub work_id: String,
    /// Environment the item is queued on.
    pub environment_id: String,
    /// Session the item belongs to; `None` for a healthcheck.
    pub session_id: Option<String>,
    /// Current state.
    pub state: String,
}

/// The subset of a work row the state-transition routes need.
#[derive(Debug, Clone)]
pub struct WorkRow {
    /// Work item id.
    pub work_id: String,
    /// Current state.
    pub state: String,
    /// Session the item belongs to; `None` for a healthcheck.
    pub session_id: Option<String>,
    /// Whether the session token in the request matched the stored digest.
    pub token_matches: bool,
}

/// A persisted session event.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    /// Monotonic insertion id; what the session stream replays and
    /// deduplicates on.
    pub event_id: i64,
    /// Event discriminator, taken from the event's `type` field.
    pub kind: String,
    /// Verbatim event JSON.
    pub payload_json: String,
    /// When it was persisted.
    pub created_at_unix: i64,
}

/// What [`Store::record_keyed_session_event`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// Stored under this new event id.
    New(i64),
    /// A frame with the same key is already stored, under this id;
    /// nothing was written.
    Duplicate(i64),
}

/// A controller's answer to a prompt, as [`Store::record_answer`] needs
/// it.
#[derive(Debug, Clone, Copy)]
pub struct AnswerAttempt<'a> {
    /// The prompt answered (`SessionFrame::answered_request_id`).
    pub request_id: &'a str,
    /// Device the answering connection authenticated as.
    pub device_id: &'a str,
    /// The answering socket.
    pub connection_id: u64,
    /// The worker socket the answer is about to be routed to.
    pub worker_connection_id: u64,
}

/// Whoever holds a prompt: the answer [`Store::record_answer`] refused a
/// later one in favour of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerHolder {
    /// Event the winning answer was stored under.
    pub event_id: i64,
    /// Device that answered.
    pub device_id: String,
    /// That device's label, if the device row is still there.
    pub label: Option<String>,
    /// The socket that answered.
    pub connection_id: u64,
}

/// What [`Store::record_answer`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerClaim {
    /// Stored under this event id; the answer now holds the prompt.
    Recorded(i64),
    /// Another connection's answer holds the prompt; nothing was written.
    Taken(AnswerHolder),
}

/// What storing an event also does to the session, in the same
/// transaction.
#[derive(Debug, Clone, Copy)]
enum EventEffect<'a> {
    /// Nothing beyond the activity time.
    None,
    /// A `session_bound`: record the Rebon session on the row.
    Bind(&'a str),
    /// A worker's refusal of an answer: the prompt is open again.
    Release(&'a str),
}

/// One row of the session list, with what a list needs from the
/// session's history already joined in.
#[derive(Debug, Clone)]
pub struct SessionListing {
    /// Session id.
    pub session_id: String,
    /// Environment it runs on.
    pub environment_id: String,
    /// Lifecycle state (see [`session_state`]).
    pub state: String,
    /// Creation time.
    pub created_at_unix: i64,
    /// Latest activity; the list's sort key.
    pub updated_at_unix: i64,
    /// Newest event id and its time, when there is one.
    pub last_event: Option<(i64, i64)>,
    /// Newest `session_state` event id and its verbatim JSON.
    pub reported_state: Option<(i64, String)>,
}

/// One audit row.
#[derive(Debug, Clone)]
pub struct AuditRow {
    /// Who performed the action (device id, or `bootstrap`).
    pub actor: String,
    /// What was done, e.g. `device.issue`.
    pub action: String,
    /// What it was done to.
    pub target: String,
}

fn work_type_str(value: WorkDataType) -> &'static str {
    match value {
        WorkDataType::Session => "session",
        WorkDataType::Healthcheck => "healthcheck",
    }
}

fn parse_work_type(value: &str) -> rusqlite::Result<WorkDataType> {
    match value {
        "session" => Ok(WorkDataType::Session),
        "healthcheck" => Ok(WorkDataType::Healthcheck),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

/// Wire form of `BridgeConfig::spawn_mode` (`single-session`, …).
///
/// Derived from the serde representation rather than a hand-written
/// match so a new variant cannot silently map to the wrong string.
fn spawn_mode_wire(config: &BridgeConfig) -> String {
    serde_json::to_value(config.spawn_mode)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Replace an environment's project list inside `transaction`.
fn write_projects(
    transaction: &rusqlite::Transaction<'_>,
    environment_id: &str,
    projects: &[ProjectInfo],
) -> rusqlite::Result<()> {
    transaction.execute(
        "DELETE FROM environment_projects WHERE environment_id = ?1",
        params![environment_id],
    )?;
    let mut insert = transaction.prepare_cached(
        "INSERT INTO environment_projects (environment_id, position, path, label, remote, branch)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (position, project) in projects.iter().enumerate() {
        insert.execute(params![
            environment_id,
            i64::try_from(position).unwrap_or(i64::MAX),
            project.path,
            project.label,
            project.remote,
            project.branch
        ])?;
    }
    Ok(())
}

fn project_from_row(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<ProjectInfo> {
    Ok(ProjectInfo {
        path: row.get(offset)?,
        label: row.get(offset + 1)?,
        remote: row.get(offset + 2)?,
        branch: row.get(offset + 3)?,
    })
}

impl Store {
    /// Open (or create) the database at `path`.
    pub fn open(path: impl AsRef<StdPath>) -> rusqlite::Result<Self> {
        Self::from_connection(Connection::open(path.as_ref())?)
    }

    /// Open a private in-memory database. Used by tests.
    pub fn in_memory() -> rusqlite::Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> rusqlite::Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(SCHEMA)?;
        migrate(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().expect("database lock")
    }

    // ─── Accounts and bootstrap ────────────────────────────────────────────

    /// Whether any account exists. Gates the bootstrap token.
    pub fn has_account(&self) -> rusqlite::Result<bool> {
        self.lock()
            .query_row("SELECT EXISTS(SELECT 1 FROM accounts)", [], |row| {
                row.get(0)
            })
    }

    /// Consume the bootstrap token: mint the first account plus its
    /// first device, but only if no account exists yet.
    ///
    /// The check and the insert share one immediate transaction, so two
    /// concurrent bootstrap requests cannot both mint an account — the
    /// loser sees `Ok(None)` and is rejected as an ordinary bad
    /// credential. "Consumed on first use" is therefore a property of
    /// the data, not of a separate flag that could drift from it.
    pub fn bootstrap_account(
        &self,
        label: &str,
        refresh: &TokenDigest,
        access: &TokenDigest,
        access_expires_at_unix: i64,
        now_unix: i64,
    ) -> rusqlite::Result<Option<(String, String)>> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let taken: bool =
            transaction.query_row("SELECT EXISTS(SELECT 1 FROM accounts)", [], |row| {
                row.get(0)
            })?;
        if taken {
            transaction.rollback()?;
            return Ok(None);
        }
        let account_id = generate_id("acc");
        let device_id = generate_id("dev");
        transaction.execute(
            "INSERT INTO accounts (account_id, issuer, subject, created_at_unix)
             VALUES (?1, NULL, NULL, ?2)",
            params![account_id, now_unix],
        )?;
        transaction.execute(
            "INSERT INTO devices (
                 device_id, account_id, label, refresh_hmac, access_hmac,
                 access_expires_at_unix, created_at_unix, last_seen_at_unix, revoked_at_unix
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL)",
            params![
                device_id,
                account_id,
                label,
                refresh.as_slice(),
                access.as_slice(),
                access_expires_at_unix,
                now_unix
            ],
        )?;
        transaction.execute(
            "INSERT INTO audit (actor, action, target, created_at_unix) VALUES (?1, ?2, ?3, ?4)",
            params!["bootstrap", "account.bootstrap", account_id, now_unix],
        )?;
        transaction.execute(
            "INSERT INTO audit (actor, action, target, created_at_unix) VALUES (?1, ?2, ?3, ?4)",
            params!["bootstrap", "device.issue", device_id, now_unix],
        )?;
        transaction.commit()?;
        Ok(Some((account_id, device_id)))
    }

    // ─── Devices ───────────────────────────────────────────────────────────

    /// Issue an additional device on an existing account.
    #[allow(clippy::too_many_arguments)]
    pub fn issue_device(
        &self,
        account_id: &str,
        actor: &str,
        label: &str,
        refresh: &TokenDigest,
        access: &TokenDigest,
        access_expires_at_unix: i64,
        now_unix: i64,
    ) -> rusqlite::Result<String> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let device_id = generate_id("dev");
        transaction.execute(
            "INSERT INTO devices (
                 device_id, account_id, label, refresh_hmac, access_hmac,
                 access_expires_at_unix, created_at_unix, last_seen_at_unix, revoked_at_unix
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL)",
            params![
                device_id,
                account_id,
                label,
                refresh.as_slice(),
                access.as_slice(),
                access_expires_at_unix,
                now_unix
            ],
        )?;
        transaction.execute(
            "INSERT INTO audit (actor, action, target, created_at_unix) VALUES (?1, ?2, ?3, ?4)",
            params![actor, "device.issue", device_id, now_unix],
        )?;
        transaction.commit()?;
        Ok(device_id)
    }

    /// Resolve a device from its access token, stamping `last_seen`.
    ///
    /// An expired access token resolves to `None` — the sweeper clears
    /// the column eventually, but the deadline is authoritative here so
    /// expiry does not depend on sweep timing.
    pub fn device_by_access(
        &self,
        digest: &TokenDigest,
        now_unix: i64,
    ) -> rusqlite::Result<Option<DeviceAuth>> {
        let connection = self.lock();
        let found = connection
            .query_row(
                "SELECT device_id, account_id, revoked_at_unix
                 FROM devices
                 WHERE access_hmac = ?1
                   AND access_expires_at_unix IS NOT NULL
                   AND access_expires_at_unix > ?2",
                params![digest.as_slice(), now_unix],
                |row| {
                    Ok(DeviceAuth {
                        device_id: row.get(0)?,
                        account_id: row.get(1)?,
                        revoked: row.get::<_, Option<i64>>(2)?.is_some(),
                    })
                },
            )
            .optional()?;
        if let Some(device) = &found {
            connection.execute(
                "UPDATE devices SET last_seen_at_unix = ?1 WHERE device_id = ?2",
                params![now_unix, device.device_id],
            )?;
        }
        Ok(found)
    }

    /// Resolve a device from its long-lived refresh token.
    pub fn device_by_refresh(&self, digest: &TokenDigest) -> rusqlite::Result<Option<DeviceAuth>> {
        self.lock()
            .query_row(
                "SELECT device_id, account_id, revoked_at_unix FROM devices WHERE refresh_hmac = ?1",
                params![digest.as_slice()],
                |row| {
                    Ok(DeviceAuth {
                        device_id: row.get(0)?,
                        account_id: row.get(1)?,
                        revoked: row.get::<_, Option<i64>>(2)?.is_some(),
                    })
                },
            )
            .optional()
    }

    /// Replace a device's access token with a freshly minted one.
    pub fn set_access_token(
        &self,
        device_id: &str,
        access: &TokenDigest,
        expires_at_unix: i64,
        now_unix: i64,
    ) -> rusqlite::Result<()> {
        self.lock().execute(
            "UPDATE devices
             SET access_hmac = ?1, access_expires_at_unix = ?2, last_seen_at_unix = ?3
             WHERE device_id = ?4 AND revoked_at_unix IS NULL",
            params![access.as_slice(), expires_at_unix, now_unix, device_id],
        )?;
        Ok(())
    }

    /// List every device on an account, revoked ones included.
    pub fn list_devices(&self, account_id: &str) -> rusqlite::Result<Vec<DeviceSummary>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT device_id, label, created_at_unix, last_seen_at_unix, revoked_at_unix
             FROM devices WHERE account_id = ?1 ORDER BY created_at_unix, device_id",
        )?;
        let rows = statement.query_map(params![account_id], |row| {
            Ok(DeviceSummary {
                device_id: row.get(0)?,
                label: row.get(1)?,
                created_at_unix: row.get(2)?,
                last_seen_at_unix: row.get(3)?,
                revoked_at_unix: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// Revoke a device, dropping its access token.
    ///
    /// Returns `false` when the target is not a device of `account_id`,
    /// which the route reports as 404 rather than confirming that a
    /// device id exists on some other account.
    pub fn revoke_device(
        &self,
        account_id: &str,
        device_id: &str,
        actor: &str,
        now_unix: i64,
    ) -> rusqlite::Result<bool> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM devices WHERE device_id = ?1 AND account_id = ?2)",
            params![device_id, account_id],
            |row| row.get(0),
        )?;
        if !exists {
            transaction.rollback()?;
            return Ok(false);
        }
        transaction.execute(
            "UPDATE devices
             SET revoked_at_unix = COALESCE(revoked_at_unix, ?1),
                 access_hmac = NULL,
                 access_expires_at_unix = NULL
             WHERE device_id = ?2",
            params![now_unix, device_id],
        )?;
        transaction.execute(
            "INSERT INTO audit (actor, action, target, created_at_unix) VALUES (?1, ?2, ?3, ?4)",
            params![actor, "device.revoke", device_id, now_unix],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    // ─── Environments ──────────────────────────────────────────────────────

    /// Register or re-register an environment, rotating its secret.
    ///
    /// The environment's project list is replaced with
    /// [`BridgeConfig::effective_projects`] in the same transaction, so a
    /// re-registration is also a full project update.
    ///
    /// Idempotency key order: an explicit `reuse_environment_id` that
    /// belongs to this account wins, then the client-generated
    /// `environment_id`, then a fresh row. Re-registering a
    /// deregistered environment revives it — a bridge coming back up
    /// after a clean shutdown should reclaim its identity rather than
    /// litter the account with dead rows.
    pub fn register_environment(
        &self,
        account_id: &str,
        device_id: &str,
        config: &BridgeConfig,
        secret: &TokenDigest,
        now_unix: i64,
    ) -> rusqlite::Result<Registration> {
        let config_json = serde_json::to_string(config)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let spawn_mode = spawn_mode_wire(config);
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let mut existing: Option<String> = None;
        if let Some(reuse) = config.reuse_environment_id.as_deref() {
            existing = transaction
                .query_row(
                    "SELECT environment_id FROM environments
                     WHERE environment_id = ?1 AND account_id = ?2",
                    params![reuse, account_id],
                    |row| row.get(0),
                )
                .optional()?;
        }
        if existing.is_none() {
            existing = transaction
                .query_row(
                    "SELECT environment_id FROM environments
                     WHERE account_id = ?1 AND client_environment_id = ?2",
                    params![account_id, config.environment_id],
                    |row| row.get(0),
                )
                .optional()?;
        }

        let registration = match existing {
            Some(environment_id) => {
                transaction.execute(
                    "UPDATE environments SET
                         device_id = ?1, client_environment_id = ?2, secret_hmac = ?3,
                         bridge_id = ?4, machine_name = ?5, dir = ?6, branch = ?7,
                         git_repo_url = ?8, worker_type = ?9, max_sessions = ?10,
                         spawn_mode = ?11, config_json = ?12, last_seen_at_unix = ?13,
                         deregistered_at_unix = NULL
                     WHERE environment_id = ?14",
                    params![
                        device_id,
                        config.environment_id,
                        secret.as_slice(),
                        config.bridge_id,
                        config.machine_name,
                        config.dir,
                        config.branch,
                        config.git_repo_url,
                        config.worker_type,
                        config.max_sessions,
                        spawn_mode,
                        config_json,
                        now_unix,
                        environment_id
                    ],
                )?;
                Registration {
                    environment_id,
                    created: false,
                }
            }
            None => {
                let environment_id = generate_id("env");
                transaction.execute(
                    "INSERT INTO environments (
                         environment_id, account_id, device_id, client_environment_id, secret_hmac,
                         bridge_id, machine_name, dir, branch, git_repo_url, worker_type,
                         max_sessions, spawn_mode, config_json, created_at_unix,
                         last_seen_at_unix, deregistered_at_unix
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15, NULL)",
                    params![
                        environment_id,
                        account_id,
                        device_id,
                        config.environment_id,
                        secret.as_slice(),
                        config.bridge_id,
                        config.machine_name,
                        config.dir,
                        config.branch,
                        config.git_repo_url,
                        config.worker_type,
                        config.max_sessions,
                        spawn_mode,
                        config_json,
                        now_unix
                    ],
                )?;
                Registration {
                    environment_id,
                    created: true,
                }
            }
        };
        write_projects(
            &transaction,
            &registration.environment_id,
            &config.effective_projects(),
        )?;
        transaction.commit()?;
        Ok(registration)
    }

    /// Resolve an environment from its secret, stamping `last_seen`.
    ///
    /// Dead environments still resolve: the route layer needs to tell a
    /// stale-but-genuine secret (404) from a bogus one (401).
    pub fn environment_by_secret(
        &self,
        digest: &TokenDigest,
        now_unix: i64,
    ) -> rusqlite::Result<Option<EnvironmentAuth>> {
        let connection = self.lock();
        let found = connection
            .query_row(
                "SELECT e.environment_id, e.account_id, e.device_id,
                        e.deregistered_at_unix, d.revoked_at_unix
                 FROM environments e JOIN devices d ON d.device_id = e.device_id
                 WHERE e.secret_hmac = ?1",
                params![digest.as_slice()],
                |row| {
                    Ok(EnvironmentAuth {
                        environment_id: row.get(0)?,
                        account_id: row.get(1)?,
                        device_id: row.get(2)?,
                        deregistered: row.get::<_, Option<i64>>(3)?.is_some(),
                        device_revoked: row.get::<_, Option<i64>>(4)?.is_some(),
                    })
                },
            )
            .optional()?;
        if let Some(environment) = &found {
            if environment.alive() {
                connection.execute(
                    "UPDATE environments SET last_seen_at_unix = ?1 WHERE environment_id = ?2",
                    params![now_unix, environment.environment_id],
                )?;
            }
        }
        Ok(found)
    }

    /// Look an environment up by id, for controller-authenticated routes.
    pub fn environment_by_id(
        &self,
        environment_id: &str,
    ) -> rusqlite::Result<Option<EnvironmentAuth>> {
        self.lock()
            .query_row(
                "SELECT e.environment_id, e.account_id, e.device_id,
                        e.deregistered_at_unix, d.revoked_at_unix
                 FROM environments e JOIN devices d ON d.device_id = e.device_id
                 WHERE e.environment_id = ?1",
                params![environment_id],
                |row| {
                    Ok(EnvironmentAuth {
                        environment_id: row.get(0)?,
                        account_id: row.get(1)?,
                        device_id: row.get(2)?,
                        deregistered: row.get::<_, Option<i64>>(3)?.is_some(),
                        device_revoked: row.get::<_, Option<i64>>(4)?.is_some(),
                    })
                },
            )
            .optional()
    }

    /// List an account's environments, newest registration last, each
    /// with its projects.
    pub fn list_environments(&self, account_id: &str) -> rusqlite::Result<Vec<EnvironmentSummary>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT environment_id, client_environment_id, device_id, bridge_id, machine_name,
                    dir, branch, git_repo_url, worker_type, max_sessions, spawn_mode,
                    created_at_unix, last_seen_at_unix, deregistered_at_unix
             FROM environments WHERE account_id = ?1 ORDER BY created_at_unix, environment_id",
        )?;
        let rows = statement.query_map(params![account_id], |row| {
            Ok(EnvironmentSummary {
                environment_id: row.get(0)?,
                client_environment_id: row.get(1)?,
                device_id: row.get(2)?,
                bridge_id: row.get(3)?,
                machine_name: row.get(4)?,
                dir: row.get(5)?,
                branch: row.get(6)?,
                git_repo_url: row.get(7)?,
                worker_type: row.get(8)?,
                max_sessions: row.get(9)?,
                spawn_mode: row.get(10)?,
                projects: Vec::new(),
                created_at_unix: row.get(11)?,
                last_seen_at_unix: row.get(12)?,
                deregistered_at_unix: row.get(13)?,
            })
        })?;
        let mut environments = rows.collect::<rusqlite::Result<Vec<_>>>()?;

        let mut projects = connection.prepare(
            "SELECT p.environment_id, p.path, p.label, p.remote, p.branch
             FROM environment_projects p
             JOIN environments e ON e.environment_id = p.environment_id
             WHERE e.account_id = ?1
             ORDER BY p.environment_id, p.position",
        )?;
        let rows = projects.query_map(params![account_id], |row| {
            Ok((row.get::<_, String>(0)?, project_from_row(row, 1)?))
        })?;
        let mut by_environment: HashMap<String, Vec<ProjectInfo>> = HashMap::new();
        for row in rows {
            let (environment_id, project) = row?;
            by_environment
                .entry(environment_id)
                .or_default()
                .push(project);
        }
        for environment in &mut environments {
            if let Some(projects) = by_environment.remove(&environment.environment_id) {
                environment.projects = projects;
            }
        }
        Ok(environments)
    }

    /// The projects an environment advertises, in its order.
    pub fn environment_projects(&self, environment_id: &str) -> rusqlite::Result<Vec<ProjectInfo>> {
        let connection = self.lock();
        let mut statement = connection.prepare_cached(
            "SELECT path, label, remote, branch FROM environment_projects
             WHERE environment_id = ?1 ORDER BY position",
        )?;
        let rows = statement.query_map(params![environment_id], |row| project_from_row(row, 0))?;
        rows.collect()
    }

    /// Replace an environment's project list and stamp `last_seen`.
    pub fn replace_projects(
        &self,
        environment_id: &str,
        projects: &[ProjectInfo],
        now_unix: i64,
    ) -> rusqlite::Result<()> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        write_projects(&transaction, environment_id, projects)?;
        transaction.execute(
            "UPDATE environments SET last_seen_at_unix = ?1 WHERE environment_id = ?2",
            params![now_unix, environment_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Deregister an environment: invalidate its secret and stop the queue.
    pub fn deregister_environment(
        &self,
        environment_id: &str,
        now_unix: i64,
    ) -> rusqlite::Result<()> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // A fresh random digest retires the old secret without leaving a
        // NULL that could collide with another retired environment under
        // the unique index.
        let mut retired = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut retired);
        transaction.execute(
            "UPDATE environments
             SET deregistered_at_unix = COALESCE(deregistered_at_unix, ?1), secret_hmac = ?2
             WHERE environment_id = ?3",
            params![now_unix, retired.as_slice(), environment_id],
        )?;
        transaction.execute(
            "UPDATE work
             SET state = ?1, lease_deadline_unix = NULL,
                 updated_at_unix = ?2
             WHERE environment_id = ?3 AND state IN ('ready', 'leased', 'acked')",
            params![work_state::STOPPED, now_unix, environment_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    // ─── Work queue ────────────────────────────────────────────────────────

    /// Enqueue a work item, creating the session row when needed.
    ///
    /// For session work this is also where the project is decided, in
    /// the same transaction as the insert so a concurrent project update
    /// cannot slip between the check and the write:
    ///
    /// 1. A named session must belong to this environment and account.
    ///    It may not exist yet — a controller may choose the id — but if
    ///    it does and is someone else's, the work is refused. (Queueing it
    ///    anyway would hand this environment's worker a session token for
    ///    that session.)
    /// 2. A session that already has a project keeps it: `project` may be
    ///    omitted, and must match if given.
    /// 3. Otherwise `project` is used, or — when omitted — the
    ///    environment's only project.
    /// 4. Whatever was chosen must be a project the environment currently
    ///    advertises.
    pub fn enqueue_work(
        &self,
        environment_id: &str,
        account_id: &str,
        work: &NewWork<'_>,
        now_unix: i64,
    ) -> rusqlite::Result<Enqueued> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (session_id, project) = match work.work_type {
            WorkDataType::Healthcheck => (None, None),
            WorkDataType::Session => {
                let existing: Option<(String, String, Option<String>)> = match work.session_id {
                    None => None,
                    Some(id) => transaction
                        .query_row(
                            "SELECT environment_id, account_id, project_path
                             FROM sessions WHERE session_id = ?1",
                            params![id],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )
                        .optional()?,
                };
                let session_project = match &existing {
                    Some((session_environment, session_account, project))
                        if session_environment == environment_id
                            && session_account == account_id =>
                    {
                        project.clone()
                    }
                    Some(_) => {
                        transaction.rollback()?;
                        return Ok(Enqueued::ForeignSession);
                    }
                    None => None,
                };
                let chosen = match (session_project, work.project) {
                    (Some(fixed), Some(requested)) if fixed != requested => None,
                    (Some(fixed), _) => Some(fixed),
                    (None, Some(requested)) => Some(requested.to_string()),
                    (None, None) => {
                        let mut statement = transaction.prepare_cached(
                            "SELECT path FROM environment_projects
                             WHERE environment_id = ?1 LIMIT 2",
                        )?;
                        let paths = statement
                            .query_map(params![environment_id], |row| row.get::<_, String>(0))?
                            .collect::<rusqlite::Result<Vec<String>>>()?;
                        match <[String; 1]>::try_from(paths) {
                            Ok([only]) => Some(only),
                            Err(_) => None,
                        }
                    }
                };
                let advertised = match &chosen {
                    None => false,
                    Some(path) => transaction.query_row(
                        "SELECT EXISTS(SELECT 1 FROM environment_projects
                                       WHERE environment_id = ?1 AND path = ?2)",
                        params![environment_id, path],
                        |row| row.get(0),
                    )?,
                };
                if !advertised {
                    transaction.rollback()?;
                    return Ok(Enqueued::ProjectRefused);
                }
                let id = work
                    .session_id
                    .map(str::to_string)
                    .unwrap_or_else(|| generate_id("sess"));
                transaction.execute(
                    "INSERT INTO sessions (
                         session_id, environment_id, account_id, state,
                         created_at_unix, updated_at_unix, project_path
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?7)
                     ON CONFLICT(session_id) DO UPDATE SET
                         state = CASE WHEN sessions.state = ?6 THEN ?4 ELSE sessions.state END,
                         updated_at_unix = ?5,
                         project_path = COALESCE(sessions.project_path, ?7)",
                    params![
                        id,
                        environment_id,
                        account_id,
                        session_state::QUEUED,
                        now_unix,
                        session_state::ARCHIVED,
                        chosen
                    ],
                )?;
                (Some(id), chosen)
            }
        };
        let work_id = generate_id("wrk");
        transaction.execute(
            "INSERT INTO work (
                 work_id, environment_id, work_type, state, session_id, prompt,
                 session_token_hmac, created_at_unix, updated_at_unix,
                 leased_at_unix, lease_deadline_unix, force,
                 project_path, resume_rebon_session_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?7, NULL, NULL, 0, ?8, ?9)",
            params![
                work_id,
                environment_id,
                work_type_str(work.work_type),
                work_state::READY,
                session_id,
                work.prompt,
                now_unix,
                project,
                work.resume_rebon_session_id
            ],
        )?;
        transaction.commit()?;
        Ok(Enqueued::Queued {
            work_id,
            session_id,
        })
    }

    /// Claim at most one item for a poller, under one immediate
    /// transaction so concurrent pollers cannot both win the same item.
    ///
    /// Preference order is oldest `ready` first, with `rowid` — not the
    /// random work id — breaking ties, so items enqueued within the same
    /// second still come out in the order they went in. Only when the
    /// queue holds no ready item does the poller look at reclaimable
    /// leased ones. An
    /// item is reclaimable when its lease deadline has passed, or —
    /// when the client sends the `reclaimOlderThanMs` hint — when it has
    /// been leased for longer than the client is willing to wait.
    pub fn claim_work(
        &self,
        environment_id: &str,
        session_token: &TokenDigest,
        reclaim_older_than_ms: Option<u64>,
        lease_seconds: i64,
        now_unix: i64,
    ) -> rusqlite::Result<Option<ClaimedWork>> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let mut candidate = transaction
            .query_row(
                "SELECT work_id FROM work WHERE environment_id = ?1 AND state = ?2
                 ORDER BY created_at_unix, rowid LIMIT 1",
                params![environment_id, work_state::READY],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        if candidate.is_none() {
            let reclaim_before = reclaim_older_than_ms
                .map(|ms| now_unix.saturating_sub((ms / 1000) as i64))
                .unwrap_or(i64::MIN);
            candidate = transaction
                .query_row(
                    "SELECT work_id
                     FROM work
                     WHERE environment_id = ?1 AND state IN ('leased', 'acked')
                       AND (lease_deadline_unix IS NULL
                            OR lease_deadline_unix <= ?2
                            OR leased_at_unix <= ?3)
                     ORDER BY created_at_unix, rowid LIMIT 1",
                    params![environment_id, now_unix, reclaim_before],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
        }

        let Some(work_id) = candidate else {
            transaction.rollback()?;
            return Ok(None);
        };
        let (work_type, session_id, created_at_unix, project_path, prompt, resume_rebon_session_id) =
            transaction.query_row(
                // An item that names no resume target continues the Rebon
                // session its RC session was last bound to, so a prompt
                // queued while no worker was attached, or an item requeued
                // after a runner went away, lands in the same local session.
                "SELECT work.work_type, work.session_id, work.created_at_unix,
                        work.project_path, work.prompt,
                        COALESCE(work.resume_rebon_session_id, sessions.rebon_session_id)
                 FROM work LEFT JOIN sessions ON sessions.session_id = work.session_id
                 WHERE work.work_id = ?1",
                params![work_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )?;

        transaction.execute(
            "UPDATE work SET state = ?1, session_token_hmac = ?2, leased_at_unix = ?3,
                             lease_deadline_unix = ?4, updated_at_unix = ?3
             WHERE work_id = ?5",
            params![
                work_state::LEASED,
                session_token.as_slice(),
                now_unix,
                now_unix + lease_seconds,
                work_id
            ],
        )?;
        if let Some(session_id) = &session_id {
            transaction.execute(
                "UPDATE sessions SET state = ?1, updated_at_unix = ?2
                 WHERE session_id = ?3 AND state <> ?4",
                params![
                    session_state::RUNNING,
                    now_unix,
                    session_id,
                    session_state::ARCHIVED
                ],
            )?;
        }
        transaction.commit()?;
        Ok(Some(ClaimedWork {
            work_id,
            work_type: parse_work_type(&work_type)?,
            session_id,
            created_at_unix,
            project_path,
            prompt,
            resume_rebon_session_id,
        }))
    }

    /// Fetch a work item scoped to an environment, checking the session
    /// token the caller presented against the stored digest.
    pub fn work_row(
        &self,
        environment_id: &str,
        work_id: &str,
        presented: Option<&TokenDigest>,
    ) -> rusqlite::Result<Option<WorkRow>> {
        self.lock()
            .query_row(
                "SELECT work_id, state, session_id, session_token_hmac
                 FROM work WHERE work_id = ?1 AND environment_id = ?2",
                params![work_id, environment_id],
                |row| {
                    let stored: Option<Vec<u8>> = row.get(3)?;
                    let token_matches = match (presented, stored) {
                        (Some(presented), Some(stored)) => {
                            use subtle::ConstantTimeEq;
                            stored.len() == 32 && bool::from(stored.ct_eq(presented.as_slice()))
                        }
                        _ => false,
                    };
                    Ok(WorkRow {
                        work_id: row.get(0)?,
                        state: row.get(1)?,
                        session_id: row.get(2)?,
                        token_matches,
                    })
                },
            )
            .optional()
    }

    /// Move a leased item to `acked` and extend its lease.
    pub fn acknowledge_work(
        &self,
        work_id: &str,
        lease_seconds: i64,
        now_unix: i64,
    ) -> rusqlite::Result<bool> {
        let changed = self.lock().execute(
            "UPDATE work SET state = ?1, lease_deadline_unix = ?2, updated_at_unix = ?3
             WHERE work_id = ?4 AND state IN ('leased', 'acked')",
            params![
                work_state::ACKED,
                now_unix + lease_seconds,
                now_unix,
                work_id
            ],
        )?;
        Ok(changed == 1)
    }

    /// Extend the lease of an item the caller still owns.
    ///
    /// Returns the state to report back: `true` plus `running` when the
    /// lease was extended, `false` plus the item's real state when it
    /// has been reclaimed, stopped or finished underneath the worker.
    pub fn heartbeat_work(
        &self,
        work_id: &str,
        lease_seconds: i64,
        now_unix: i64,
    ) -> rusqlite::Result<(bool, String)> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: Option<String> = transaction
            .query_row(
                "SELECT state FROM work WHERE work_id = ?1",
                params![work_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            transaction.rollback()?;
            return Err(rusqlite::Error::QueryReturnedNoRows);
        };
        let extendable = state == work_state::LEASED || state == work_state::ACKED;
        if extendable {
            transaction.execute(
                "UPDATE work SET lease_deadline_unix = ?1, updated_at_unix = ?2 WHERE work_id = ?3",
                params![now_unix + lease_seconds, now_unix, work_id],
            )?;
        }
        transaction.commit()?;
        Ok(if extendable {
            (true, state)
        } else {
            (false, state)
        })
    }

    /// Stop a work item, recording whether the stop was forced.
    pub fn stop_work(&self, work_id: &str, force: bool, now_unix: i64) -> rusqlite::Result<bool> {
        let changed = self.lock().execute(
            "UPDATE work
             SET state = ?1, force = ?2, lease_deadline_unix = NULL,
                 updated_at_unix = ?3
             WHERE work_id = ?4 AND state IN ('ready', 'leased', 'acked')",
            params![work_state::STOPPED, i64::from(force), now_unix, work_id],
        )?;
        Ok(changed == 1)
    }

    /// Return every item whose lease has run out to the `ready` queue.
    ///
    /// Returns the environments that gained work so the caller can wake
    /// their pollers.
    pub fn expire_leases(&self, now_unix: i64) -> rusqlite::Result<Vec<String>> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let environments: Vec<String> = {
            let mut statement = transaction.prepare(
                "SELECT DISTINCT environment_id FROM work
                 WHERE state IN ('leased', 'acked') AND lease_deadline_unix IS NOT NULL
                   AND lease_deadline_unix <= ?1",
            )?;
            let rows = statement.query_map(params![now_unix], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()?
        };
        if !environments.is_empty() {
            transaction.execute(
                "UPDATE work
                 SET state = ?1, leased_at_unix = NULL,
                     lease_deadline_unix = NULL, updated_at_unix = ?2
                 WHERE state IN ('leased', 'acked') AND lease_deadline_unix IS NOT NULL
                   AND lease_deadline_unix <= ?2",
                params![work_state::READY, now_unix],
            )?;
        }
        transaction.commit()?;
        Ok(environments)
    }

    /// Clear access tokens whose deadline has passed.
    pub fn expire_access_tokens(&self, now_unix: i64) -> rusqlite::Result<usize> {
        self.lock().execute(
            "UPDATE devices SET access_hmac = NULL, access_expires_at_unix = NULL
             WHERE access_expires_at_unix IS NOT NULL AND access_expires_at_unix <= ?1",
            params![now_unix],
        )
    }

    // ─── Sessions ──────────────────────────────────────────────────────────

    /// Resolve the work item a session token belongs to.
    pub fn work_by_session_token(
        &self,
        digest: &TokenDigest,
    ) -> rusqlite::Result<Option<WorkByToken>> {
        self.lock()
            .query_row(
                "SELECT work_id, environment_id, session_id, state
                 FROM work WHERE session_token_hmac = ?1",
                params![digest.as_slice()],
                |row| {
                    Ok(WorkByToken {
                        work_id: row.get(0)?,
                        environment_id: row.get(1)?,
                        session_id: row.get(2)?,
                        state: row.get(3)?,
                    })
                },
            )
            .optional()
    }

    /// Look a session up, returning its environment, account and state.
    pub fn session(&self, session_id: &str) -> rusqlite::Result<Option<(String, String, String)>> {
        self.lock()
            .query_row(
                "SELECT environment_id, account_id, state FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
    }

    /// Persist one session event, returning the id it was written under.
    ///
    /// The id is what the session stream deduplicates on: a frame is
    /// persisted before it is fanned out, and a controller that is still
    /// replaying the backlog drops anything at or below the id it
    /// replayed through.
    pub fn record_session_event(
        &self,
        session_id: &str,
        kind: &str,
        payload_json: &str,
        now_unix: i64,
    ) -> rusqlite::Result<i64> {
        match self.record_keyed_session_event(session_id, kind, payload_json, None, now_unix)? {
            Recorded::New(event_id) | Recorded::Duplicate(event_id) => Ok(event_id),
        }
    }

    /// Persist one session event unless one with the same `dedupe_key`
    /// is already stored for the session.
    ///
    /// The key is `SessionFrame::idempotency_key`: a worker that resends
    /// after a reconnect gets [`Recorded::Duplicate`] and the history
    /// keeps one copy. The lookup and the insert share one immediate
    /// transaction, and a partial unique index backs it up. A duplicate
    /// does not count as activity.
    pub fn record_keyed_session_event(
        &self,
        session_id: &str,
        kind: &str,
        payload_json: &str,
        dedupe_key: Option<&str>,
        now_unix: i64,
    ) -> rusqlite::Result<Recorded> {
        self.record_event(
            session_id,
            kind,
            payload_json,
            dedupe_key,
            EventEffect::None,
            now_unix,
        )
    }

    /// Persist a worker's refusal of an answer to `request_id` (a
    /// `control_response` error) and, in the same transaction, drop
    /// whatever claim holds that prompt, so the next answer is taken.
    ///
    /// The refusal names a request, not an answer, so the claim goes
    /// whoever holds it: only one answer is ever routed while a claim is
    /// held, and a refusal that crosses a newer claim in flight costs no
    /// more than the runner judging one more answer itself.
    pub fn record_answer_refusal(
        &self,
        session_id: &str,
        payload_json: &str,
        request_id: &str,
        now_unix: i64,
    ) -> rusqlite::Result<i64> {
        match self.record_event(
            session_id,
            "control_response",
            payload_json,
            None,
            EventEffect::Release(request_id),
            now_unix,
        )? {
            Recorded::New(event_id) | Recorded::Duplicate(event_id) => Ok(event_id),
        }
    }

    /// Persist a controller's answer to a prompt, unless another
    /// connection's answer already holds it.
    ///
    /// The lookup, the event and the claim share one immediate
    /// transaction, so of two answers racing for one prompt exactly one
    /// is stored. An existing claim is **not** in the way when:
    ///
    /// * it is the same connection's — a controller retrying is not told
    ///   it lost to itself;
    /// * it was routed to a different worker socket than the one this
    ///   answer goes to. A worker gets no replay, so an answer queued for
    ///   a socket that has since gone may never have arrived; holding the
    ///   prompt for it would hold it forever. The runner still checks the
    ///   prompt is pending before it applies anything, so the worst this
    ///   costs is a `control_response` error instead of a refusal here.
    ///   Socket ids are in-memory, so after a restart every claim is in
    ///   this state.
    ///
    /// Either way the new answer takes the claim over.
    pub fn record_answer(
        &self,
        session_id: &str,
        kind: &str,
        payload_json: &str,
        attempt: AnswerAttempt<'_>,
        now_unix: i64,
    ) -> rusqlite::Result<AnswerClaim> {
        // SQLite integers are signed; the ids are only ever compared, so
        // the bit pattern is what is stored.
        let connection_id = attempt.connection_id as i64;
        let worker_connection_id = attempt.worker_connection_id as i64;
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let holder = transaction
            .query_row(
                "SELECT a.event_id, a.device_id, d.label, a.connection_id, a.worker_connection_id
                 FROM session_answers a LEFT JOIN devices d ON d.device_id = a.device_id
                 WHERE a.session_id = ?1 AND a.request_id = ?2",
                params![session_id, attempt.request_id],
                |row| {
                    Ok((
                        AnswerHolder {
                            event_id: row.get(0)?,
                            device_id: row.get(1)?,
                            label: row.get(2)?,
                            connection_id: row.get::<_, i64>(3)? as u64,
                        },
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        if let Some((holder, routed_to)) = holder {
            if holder.connection_id != attempt.connection_id && routed_to == worker_connection_id {
                transaction.rollback()?;
                return Ok(AnswerClaim::Taken(holder));
            }
        }
        transaction.execute(
            "INSERT INTO session_events (session_id, kind, payload_json, created_at_unix)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, kind, payload_json, now_unix],
        )?;
        let event_id = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO session_answers (session_id, request_id, event_id, device_id,
                                          connection_id, worker_connection_id, answered_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (session_id, request_id) DO UPDATE SET
                 event_id = excluded.event_id,
                 device_id = excluded.device_id,
                 connection_id = excluded.connection_id,
                 worker_connection_id = excluded.worker_connection_id,
                 answered_at_unix = excluded.answered_at_unix",
            params![
                session_id,
                attempt.request_id,
                event_id,
                attempt.device_id,
                connection_id,
                worker_connection_id,
                now_unix
            ],
        )?;
        transaction.execute(
            "UPDATE sessions SET updated_at_unix = ?1 WHERE session_id = ?2",
            params![now_unix, session_id],
        )?;
        transaction.commit()?;
        Ok(AnswerClaim::Recorded(event_id))
    }

    /// Persist a worker's `session_bound` frame and record the Rebon
    /// session it names on the session row, in one transaction.
    ///
    /// The row's `rebon_session_id` is what later work for the session
    /// resumes: [`Self::claim_work`] falls back to it for an item that
    /// names no resume target, and [`Self::reconnect_session`] prefers it.
    /// A resend of a binding already stored is a [`Recorded::Duplicate`]
    /// and changes nothing; a different id is a new binding and replaces
    /// the old one.
    pub fn record_session_binding(
        &self,
        session_id: &str,
        payload_json: &str,
        dedupe_key: &str,
        rebon_session_id: &str,
        now_unix: i64,
    ) -> rusqlite::Result<Recorded> {
        self.record_event(
            session_id,
            "session_bound",
            payload_json,
            Some(dedupe_key),
            EventEffect::Bind(rebon_session_id),
            now_unix,
        )
    }

    /// The Rebon session a session was last bound to, if any.
    pub fn bound_rebon_session(&self, session_id: &str) -> rusqlite::Result<Option<String>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT rebon_session_id FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    fn record_event(
        &self,
        session_id: &str,
        kind: &str,
        payload_json: &str,
        dedupe_key: Option<&str>,
        effect: EventEffect<'_>,
        now_unix: i64,
    ) -> rusqlite::Result<Recorded> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(key) = dedupe_key {
            let existing: Option<i64> = transaction
                .query_row(
                    "SELECT event_id FROM session_events
                     WHERE session_id = ?1 AND dedupe_key = ?2",
                    params![session_id, key],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(event_id) = existing {
                transaction.rollback()?;
                return Ok(Recorded::Duplicate(event_id));
            }
        }
        transaction.execute(
            "INSERT INTO session_events (session_id, kind, payload_json, created_at_unix, dedupe_key)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, kind, payload_json, now_unix, dedupe_key],
        )?;
        let event_id = transaction.last_insert_rowid();
        let binding = match effect {
            EventEffect::Bind(rebon_session_id) => Some(rebon_session_id),
            EventEffect::None | EventEffect::Release(_) => None,
        };
        transaction.execute(
            "UPDATE sessions SET updated_at_unix = ?1,
                                 rebon_session_id = COALESCE(?3, rebon_session_id)
             WHERE session_id = ?2",
            params![now_unix, session_id, binding],
        )?;
        if let EventEffect::Release(request_id) = effect {
            transaction.execute(
                "DELETE FROM session_answers WHERE session_id = ?1 AND request_id = ?2",
                params![session_id, request_id],
            )?;
        }
        transaction.commit()?;
        Ok(Recorded::New(event_id))
    }

    /// Read a session's events in insertion order.
    pub fn session_events(&self, session_id: &str) -> rusqlite::Result<Vec<SessionEvent>> {
        self.recent_session_events(session_id, usize::MAX)
    }

    /// The last `limit` events of a session, oldest first.
    ///
    /// Ordering is by `event_id`, which is the insertion order — not by
    /// `created_at_unix`, whose one-second resolution would leave frames
    /// from the same second in arbitrary order, and this is the order a
    /// reattaching controller reconstructs the session from.
    pub fn recent_session_events(
        &self,
        session_id: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<SessionEvent>> {
        let connection = self.lock();
        // Take the newest `limit` rows, then put them back in order.
        let mut events = Self::events_before(&connection, session_id, i64::MAX, limit)?;
        events.reverse();
        Ok(events)
    }

    /// Up to `limit` events of a session with an id strictly below
    /// `before`, **newest first** — one step of a backwards walk.
    ///
    /// `before` is an exclusive bound, never a row that has to exist, so
    /// a walk resumes correctly across ids that retention has removed.
    /// This is the range scan `session_events_session` is for.
    pub fn session_events_before(
        &self,
        session_id: &str,
        before: i64,
        limit: usize,
    ) -> rusqlite::Result<Vec<SessionEvent>> {
        Self::events_before(&self.lock(), session_id, before, limit)
    }

    fn events_before(
        connection: &Connection,
        session_id: &str,
        before: i64,
        limit: usize,
    ) -> rusqlite::Result<Vec<SessionEvent>> {
        let mut statement = connection.prepare_cached(
            "SELECT event_id, kind, payload_json, created_at_unix FROM session_events
             WHERE session_id = ?1 AND event_id < ?2
             ORDER BY event_id DESC LIMIT ?3",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = statement.query_map(params![session_id, before, limit], |row| {
            Ok(SessionEvent {
                event_id: row.get(0)?,
                kind: row.get(1)?,
                payload_json: row.get(2)?,
                created_at_unix: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// An account's sessions, most recent activity first, optionally
    /// only those on `environment_id`, resuming strictly after `after`
    /// (`(updated_at_unix, session_id)` of the previous page's last row).
    ///
    /// Ties on the one-second activity clock are broken by session id,
    /// descending, so the order is total and a page boundary can fall
    /// between two sessions active in the same second.
    pub fn list_sessions(
        &self,
        account_id: &str,
        environment_id: Option<&str>,
        after: Option<(i64, &str)>,
        limit: usize,
    ) -> rusqlite::Result<Vec<SessionListing>> {
        // Two optional clauses, spelled out rather than `?n IS NULL OR …`
        // so the planner sees a plain range on `sessions_activity`.
        let mut sql = String::from(
            "SELECT s.session_id, s.environment_id, s.state, s.created_at_unix,
                    s.updated_at_unix,
                    last.event_id, last.created_at_unix,
                    reported.event_id, reported.payload_json
             FROM sessions s
             LEFT JOIN session_events last ON last.event_id =
                 (SELECT MAX(event_id) FROM session_events WHERE session_id = s.session_id)
             LEFT JOIN session_events reported ON reported.event_id =
                 (SELECT MAX(event_id) FROM session_events
                  WHERE session_id = s.session_id AND kind = 'session_state')
             WHERE s.account_id = :account",
        );
        if environment_id.is_some() {
            sql.push_str(" AND s.environment_id = :environment");
        }
        if after.is_some() {
            sql.push_str(" AND (s.updated_at_unix, s.session_id) < (:after_time, :after_id)");
        }
        sql.push_str(" ORDER BY s.updated_at_unix DESC, s.session_id DESC LIMIT :limit");

        let connection = self.lock();
        let mut statement = connection.prepare(&sql)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut bound: Vec<(&str, &dyn rusqlite::ToSql)> =
            vec![(":account", &account_id), (":limit", &limit)];
        if let Some(environment_id) = &environment_id {
            bound.push((":environment", environment_id));
        }
        if let Some((time, id)) = &after {
            bound.push((":after_time", time));
            bound.push((":after_id", id));
        }
        let rows = statement.query_map(bound.as_slice(), |row| {
            let last_id: Option<i64> = row.get(5)?;
            let last_at: Option<i64> = row.get(6)?;
            let reported_id: Option<i64> = row.get(7)?;
            let reported_json: Option<String> = row.get(8)?;
            Ok(SessionListing {
                session_id: row.get(0)?,
                environment_id: row.get(1)?,
                state: row.get(2)?,
                created_at_unix: row.get(3)?,
                updated_at_unix: row.get(4)?,
                last_event: last_id.zip(last_at),
                reported_state: reported_id.zip(reported_json),
            })
        })?;
        rows.collect()
    }

    /// Archive a session and finish any work still outstanding for it.
    pub fn archive_session(&self, session_id: &str, now_unix: i64) -> rusqlite::Result<()> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE sessions SET state = ?1, updated_at_unix = ?2 WHERE session_id = ?3",
            params![session_state::ARCHIVED, now_unix, session_id],
        )?;
        transaction.execute(
            "UPDATE work
             SET state = ?1, lease_deadline_unix = NULL,
                 updated_at_unix = ?2
             WHERE session_id = ?3 AND state IN ('ready', 'leased', 'acked')",
            params![work_state::DONE, now_unix, session_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Supersede every outstanding work item for a session and queue a
    /// fresh one, so a controller can recover a session whose bridge is
    /// gone without waiting for the old lease to lapse.
    ///
    /// The fresh item runs in the session's project and continues the
    /// Rebon session the worker last bound it to, or else the one its
    /// newest item named, if any. It has no prompt: the prompt already
    /// ran, and a reconnect must not run it again.
    pub fn reconnect_session(
        &self,
        environment_id: &str,
        session_id: &str,
        now_unix: i64,
    ) -> rusqlite::Result<String> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE work
             SET state = ?1, force = 1, lease_deadline_unix = NULL,
                 updated_at_unix = ?2
             WHERE session_id = ?3 AND state IN ('ready', 'leased', 'acked')",
            params![work_state::STOPPED, now_unix, session_id],
        )?;
        let work_id = generate_id("wrk");
        transaction.execute(
            "INSERT INTO work (
                 work_id, environment_id, work_type, state, session_id, prompt,
                 session_token_hmac, created_at_unix, updated_at_unix,
                 leased_at_unix, lease_deadline_unix, force,
                 project_path, resume_rebon_session_id
             ) VALUES (
                 ?1, ?2, 'session', ?3, ?4, NULL, NULL, ?5, ?5, NULL, NULL, 0,
                 (SELECT project_path FROM sessions WHERE session_id = ?4),
                 COALESCE(
                     (SELECT rebon_session_id FROM sessions WHERE session_id = ?4),
                     (SELECT resume_rebon_session_id FROM work
                      WHERE session_id = ?4 AND work_id <> ?1
                      ORDER BY rowid DESC LIMIT 1)
                 )
             )",
            params![
                work_id,
                environment_id,
                work_state::READY,
                session_id,
                now_unix
            ],
        )?;
        transaction.execute(
            "UPDATE sessions SET state = ?1, updated_at_unix = ?2 WHERE session_id = ?3",
            params![session_state::QUEUED, now_unix, session_id],
        )?;
        transaction.commit()?;
        Ok(work_id)
    }

    /// Read the audit log, oldest first. Used by tests and operators.
    pub fn audit(&self) -> rusqlite::Result<Vec<AuditRow>> {
        let connection = self.lock();
        let mut statement =
            connection.prepare("SELECT actor, action, target FROM audit ORDER BY audit_id")?;
        let rows = statement.query_map([], |row| {
            Ok(AuditRow {
                actor: row.get(0)?,
                action: row.get(1)?,
                target: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    /// State of a single work item. Used by tests.
    pub fn work_state(&self, work_id: &str) -> rusqlite::Result<Option<String>> {
        self.lock()
            .query_row(
                "SELECT state FROM work WHERE work_id = ?1",
                params![work_id],
                |row| row.get(0),
            )
            .optional()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_bridge::config::SpawnMode;

    fn config() -> BridgeConfig {
        let mut config =
            BridgeConfig::minimal("bridge-1", "client-env-1", "https://api", "wss://ingress");
        config.dir = "/srv/app".into();
        config
    }

    fn session_work(prompt: &str) -> NewWork<'_> {
        NewWork {
            work_type: WorkDataType::Session,
            session_id: None,
            prompt: Some(prompt),
            project: None,
            resume_rebon_session_id: None,
        }
    }

    /// Queue session work that must be accepted; `(work_id, session_id)`.
    fn enqueue_session(
        store: &Store,
        environment: &str,
        account: &str,
        work: &NewWork<'_>,
        now_unix: i64,
    ) -> (String, String) {
        match store
            .enqueue_work(environment, account, work, now_unix)
            .expect("enqueue")
        {
            Enqueued::Queued {
                work_id,
                session_id: Some(session_id),
            } => (work_id, session_id),
            other => panic!("expected queued session work, got {other:?}"),
        }
    }

    fn store_with_device() -> (Store, String, String) {
        let store = Store::in_memory().expect("open store");
        let (account_id, device_id) = store
            .bootstrap_account("first", &[1u8; 32], &[2u8; 32], 9_999, 0)
            .expect("bootstrap")
            .expect("account minted");
        (store, account_id, device_id)
    }

    #[test]
    fn bootstrap_mints_exactly_one_account() {
        let (store, _, _) = store_with_device();
        assert!(store.has_account().expect("has account"));
        let second = store
            .bootstrap_account("second", &[3u8; 32], &[4u8; 32], 9_999, 0)
            .expect("second bootstrap");
        assert!(second.is_none(), "bootstrap must be single-use");
    }

    #[test]
    fn registration_is_idempotent_on_the_client_environment_id() {
        let (store, account, device) = store_with_device();
        let first = store
            .register_environment(&account, &device, &config(), &[9u8; 32], 100)
            .expect("register");
        assert!(first.created);
        let second = store
            .register_environment(&account, &device, &config(), &[8u8; 32], 200)
            .expect("re-register");
        assert!(!second.created);
        assert_eq!(first.environment_id, second.environment_id);
        // The rotated secret is live and the previous one is not.
        assert!(store
            .environment_by_secret(&[8u8; 32], 300)
            .expect("lookup")
            .is_some());
        assert!(store
            .environment_by_secret(&[9u8; 32], 300)
            .expect("lookup")
            .is_none());
    }

    #[test]
    fn spawn_mode_is_stored_in_its_wire_form() {
        let (store, account, device) = store_with_device();
        let mut config = config();
        config.spawn_mode = SpawnMode::SameDir;
        store
            .register_environment(&account, &device, &config, &[9u8; 32], 100)
            .expect("register");
        let listed = store.list_environments(&account).expect("list");
        assert_eq!(listed[0].spawn_mode, "same-dir");
    }

    #[test]
    fn only_one_claimer_wins_a_ready_item() {
        let (store, account, device) = store_with_device();
        let environment = store
            .register_environment(&account, &device, &config(), &[9u8; 32], 100)
            .expect("register")
            .environment_id;
        enqueue_session(&store, &environment, &account, &session_work("hello"), 100);
        let first = store
            .claim_work(&environment, &[1u8; 32], None, 60, 100)
            .expect("claim");
        let second = store
            .claim_work(&environment, &[2u8; 32], None, 60, 100)
            .expect("claim");
        assert!(first.is_some());
        assert!(
            second.is_none(),
            "a leased item must not be handed out twice"
        );
    }

    #[test]
    fn items_queued_in_the_same_second_still_come_out_in_order() {
        let (store, account, device) = store_with_device();
        let environment = store
            .register_environment(&account, &device, &config(), &[9u8; 32], 100)
            .expect("register")
            .environment_id;
        // Identical timestamps: only the insertion order can break the
        // tie, and work ids are random, so ordering by them would be a
        // coin flip.
        let mut queued = Vec::new();
        for index in 0..4 {
            let (work_id, _) = enqueue_session(
                &store,
                &environment,
                &account,
                &session_work(&format!("prompt-{index}")),
                100,
            );
            queued.push(work_id);
        }
        let mut handed_out = Vec::new();
        for index in 0..4u8 {
            let claim = store
                .claim_work(&environment, &[index; 32], None, 3_600, 100)
                .expect("claim")
                .expect("claimed");
            handed_out.push(claim.work_id);
        }
        assert_eq!(handed_out, queued, "the queue must stay FIFO");
    }

    #[test]
    fn expired_leases_return_to_the_ready_queue() {
        let (store, account, device) = store_with_device();
        let environment = store
            .register_environment(&account, &device, &config(), &[9u8; 32], 100)
            .expect("register")
            .environment_id;
        let (work_id, _) =
            enqueue_session(&store, &environment, &account, &session_work("hello"), 100);
        store
            .claim_work(&environment, &[1u8; 32], None, 10, 100)
            .expect("claim")
            .expect("claimed");
        let woken = store.expire_leases(200).expect("expire");
        assert_eq!(woken, vec![environment.clone()]);
        assert_eq!(
            store.work_state(&work_id).expect("state").as_deref(),
            Some(work_state::READY)
        );
    }

    #[test]
    fn the_reclaim_hint_takes_an_item_before_its_lease_lapses() {
        let (store, account, device) = store_with_device();
        let environment = store
            .register_environment(&account, &device, &config(), &[9u8; 32], 100)
            .expect("register")
            .environment_id;
        enqueue_session(&store, &environment, &account, &session_work("hello"), 100);
        store
            .claim_work(&environment, &[1u8; 32], None, 3_600, 100)
            .expect("claim")
            .expect("claimed");
        // Without the hint the long lease keeps the item out of reach.
        assert!(store
            .claim_work(&environment, &[2u8; 32], None, 3_600, 130)
            .expect("claim")
            .is_none());
        // With a 10s hint the same item is reclaimable at t+130.
        assert!(store
            .claim_work(&environment, &[3u8; 32], Some(10_000), 3_600, 130)
            .expect("claim")
            .is_some());
    }

    #[test]
    fn revoking_a_device_kills_its_environment_secrets() {
        let (store, account, device) = store_with_device();
        store
            .register_environment(&account, &device, &config(), &[9u8; 32], 100)
            .expect("register");
        assert!(store
            .environment_by_secret(&[9u8; 32], 100)
            .expect("lookup")
            .expect("found")
            .alive());
        store
            .revoke_device(&account, &device, "test", 150)
            .expect("revoke");
        let resolved = store
            .environment_by_secret(&[9u8; 32], 200)
            .expect("lookup")
            .expect("still resolvable");
        assert!(
            !resolved.alive(),
            "secret must cascade dead with its device"
        );
    }

    fn store_with_session() -> (Store, String, String, String) {
        let (store, account, device) = store_with_device();
        let environment = store
            .register_environment(&account, &device, &config(), &[9u8; 32], 100)
            .expect("register")
            .environment_id;
        let (_, session) =
            enqueue_session(&store, &environment, &account, &session_work("hello"), 100);
        (store, account, environment, session)
    }

    fn ids_of(events: &[SessionEvent]) -> Vec<i64> {
        events.iter().map(|event| event.event_id).collect()
    }

    #[test]
    fn a_backwards_walk_steps_over_deleted_ids() {
        let (store, _, _, session) = store_with_session();
        let ids: Vec<i64> = (0..6)
            .map(|index| {
                store
                    .record_session_event(&session, "session_message", &format!("[{index}]"), 200)
                    .expect("record")
            })
            .collect();
        // Retention removed the middle of the history.
        store
            .lock()
            .execute(
                "DELETE FROM session_events WHERE event_id IN (?1, ?2)",
                params![ids[2], ids[3]],
            )
            .expect("delete");
        let newest = store
            .session_events_before(&session, i64::MAX, 2)
            .expect("page");
        assert_eq!(ids_of(&newest), vec![ids[5], ids[4]]);
        assert_eq!(newest[0].created_at_unix, 200);
        let older = store
            .session_events_before(&session, ids[4], 2)
            .expect("page");
        assert_eq!(ids_of(&older), vec![ids[1], ids[0]]);
        // A bound that is itself a deleted id works just as well.
        let from_a_deleted_bound = store
            .session_events_before(&session, ids[3], 10)
            .expect("page");
        assert_eq!(ids_of(&from_a_deleted_bound), vec![ids[1], ids[0]]);
    }

    #[test]
    fn the_session_list_joins_the_latest_event_and_reported_state() {
        let (store, account, environment, session) = store_with_session();
        let fresh = store.list_sessions(&account, None, None, 10).expect("list");
        assert_eq!(fresh.len(), 1);
        assert!(fresh[0].last_event.is_none() && fresh[0].reported_state.is_none());

        let state_id = store
            .record_session_event(&session, "session_state", "{}", 300)
            .expect("record");
        let last_id = store
            .record_session_event(&session, "session_message", "{}", 301)
            .expect("record");
        let listed = store
            .list_sessions(&account, Some(&environment), None, 10)
            .expect("list");
        assert_eq!(listed[0].last_event, Some((last_id, 301)));
        assert_eq!(
            listed[0].reported_state.as_ref().map(|state| state.0),
            Some(state_id)
        );
        assert_eq!(listed[0].updated_at_unix, 301);
        assert!(store
            .list_sessions(&account, Some("env_other"), None, 10)
            .expect("list")
            .is_empty());
        assert!(store
            .list_sessions(&account, None, Some((301, session.as_str())), 10)
            .expect("list")
            .is_empty());
    }

    #[test]
    fn session_work_is_placed_in_an_advertised_project() {
        let (store, account, device) = store_with_device();
        let mut two = config();
        two.projects = vec![
            ProjectInfo::from_path("/srv/one"),
            ProjectInfo::from_path("/srv/two"),
        ];
        let environment = store
            .register_environment(&account, &device, &two, &[9u8; 32], 100)
            .expect("register")
            .environment_id;

        // Two projects and none named: ambiguous.
        assert_eq!(
            store
                .enqueue_work(&environment, &account, &session_work("hi"), 100)
                .expect("enqueue"),
            Enqueued::ProjectRefused
        );
        // A project that is not advertised.
        let elsewhere = NewWork {
            project: Some("/srv/three"),
            ..session_work("hi")
        };
        assert_eq!(
            store
                .enqueue_work(&environment, &account, &elsewhere, 100)
                .expect("enqueue"),
            Enqueued::ProjectRefused
        );
        // An advertised one, with a resume target, comes back out of the
        // claim together with the prompt.
        let placed = NewWork {
            project: Some("/srv/two"),
            resume_rebon_session_id: Some("rebon-1"),
            ..session_work("hi")
        };
        let (work_id, session_id) = enqueue_session(&store, &environment, &account, &placed, 100);
        let claim = store
            .claim_work(&environment, &[1u8; 32], None, 60, 100)
            .expect("claim")
            .expect("claimed");
        assert_eq!(claim.work_id, work_id);
        assert_eq!(claim.project_path.as_deref(), Some("/srv/two"));
        assert_eq!(claim.prompt.as_deref(), Some("hi"));
        assert_eq!(claim.resume_rebon_session_id.as_deref(), Some("rebon-1"));

        // Follow-up work for the session inherits its project, and may not
        // move it.
        let follow_up = NewWork {
            session_id: Some(&session_id),
            ..session_work("again")
        };
        enqueue_session(&store, &environment, &account, &follow_up, 101);
        let claim = store
            .claim_work(&environment, &[2u8; 32], None, 60, 101)
            .expect("claim")
            .expect("claimed");
        assert_eq!(claim.project_path.as_deref(), Some("/srv/two"));
        assert_eq!(claim.resume_rebon_session_id, None);
        let moved = NewWork {
            project: Some("/srv/one"),
            ..follow_up
        };
        assert_eq!(
            store
                .enqueue_work(&environment, &account, &moved, 102)
                .expect("enqueue"),
            Enqueued::ProjectRefused
        );

        // A reconnect keeps the project and the last resume target, and
        // does not replay a prompt.
        store
            .reconnect_session(&environment, &session_id, 103)
            .expect("reconnect");
        let claim = store
            .claim_work(&environment, &[3u8; 32], None, 60, 103)
            .expect("claim")
            .expect("claimed");
        assert_eq!(claim.project_path.as_deref(), Some("/srv/two"));
        assert_eq!(claim.prompt, None);
        assert_eq!(claim.resume_rebon_session_id, None);

        // Once the project is withdrawn, the session cannot get more work.
        store
            .replace_projects(&environment, &[ProjectInfo::from_path("/srv/one")], 104)
            .expect("replace");
        assert_eq!(
            store
                .enqueue_work(&environment, &account, &follow_up, 104)
                .expect("enqueue"),
            Enqueued::ProjectRefused
        );
    }

    #[test]
    fn a_bound_session_is_what_later_work_resumes() {
        let (store, account, device) = store_with_device();
        let environment = store
            .register_environment(&account, &device, &config(), &[9u8; 32], 100)
            .expect("register")
            .environment_id;
        let (_, session_id) =
            enqueue_session(&store, &environment, &account, &session_work("hi"), 100);
        store
            .claim_work(&environment, &[1u8; 32], None, 60, 100)
            .expect("claim")
            .expect("claimed");
        assert_eq!(store.bound_rebon_session(&session_id).expect("read"), None);

        // The worker says which local session it opened.
        let bound = r#"{"type":"session_bound","rebon_session_id":"local-1"}"#;
        assert!(matches!(
            store
                .record_session_binding(&session_id, bound, "session_bound:local-1", "local-1", 101)
                .expect("bind"),
            Recorded::New(_)
        ));
        // A resend changes nothing.
        assert!(matches!(
            store
                .record_session_binding(&session_id, bound, "session_bound:local-1", "local-1", 102)
                .expect("bind again"),
            Recorded::Duplicate(_)
        ));
        assert_eq!(
            store.bound_rebon_session(&session_id).expect("read"),
            Some("local-1".into())
        );

        // A prompt queued for the session with no resume target continues it.
        let follow_up = NewWork {
            session_id: Some(&session_id),
            ..session_work("again")
        };
        enqueue_session(&store, &environment, &account, &follow_up, 103);
        let claim = store
            .claim_work(&environment, &[2u8; 32], None, 60, 103)
            .expect("claim")
            .expect("claimed");
        assert_eq!(claim.resume_rebon_session_id.as_deref(), Some("local-1"));
        assert_eq!(claim.prompt.as_deref(), Some("again"));

        // An explicit target still wins over the binding.
        let explicit = NewWork {
            session_id: Some(&session_id),
            resume_rebon_session_id: Some("local-other"),
            ..session_work("elsewhere")
        };
        enqueue_session(&store, &environment, &account, &explicit, 104);
        let claim = store
            .claim_work(&environment, &[3u8; 32], None, 60, 104)
            .expect("claim")
            .expect("claimed");
        assert_eq!(
            claim.resume_rebon_session_id.as_deref(),
            Some("local-other")
        );

        // A reconnect prefers the binding to the newest item's target.
        store
            .reconnect_session(&environment, &session_id, 105)
            .expect("reconnect");
        let claim = store
            .claim_work(&environment, &[4u8; 32], None, 60, 105)
            .expect("claim")
            .expect("claimed");
        assert_eq!(claim.prompt, None);
        assert_eq!(claim.resume_rebon_session_id.as_deref(), Some("local-1"));

        // A new binding replaces the old one.
        let rebound = r#"{"type":"session_bound","rebon_session_id":"local-2"}"#;
        store
            .record_session_binding(
                &session_id,
                rebound,
                "session_bound:local-2",
                "local-2",
                106,
            )
            .expect("rebind");
        assert_eq!(
            store.bound_rebon_session(&session_id).expect("read"),
            Some("local-2".into())
        );
        // Ordinary events never touch it.
        store
            .record_keyed_session_event(&session_id, "session_message", "{}", None, 107)
            .expect("record");
        assert_eq!(
            store.bound_rebon_session(&session_id).expect("read"),
            Some("local-2".into())
        );
    }

    #[test]
    fn a_session_on_another_environment_is_not_queued_for() {
        let (store, account, device) = store_with_device();
        let first = store
            .register_environment(&account, &device, &config(), &[9u8; 32], 100)
            .expect("register")
            .environment_id;
        let mut other = config();
        other.environment_id = "client-env-2".into();
        let second = store
            .register_environment(&account, &device, &other, &[8u8; 32], 100)
            .expect("register")
            .environment_id;
        let (_, session_id) = enqueue_session(&store, &first, &account, &session_work("hi"), 100);
        let hijack = NewWork {
            session_id: Some(&session_id),
            ..session_work("mine now")
        };
        assert_eq!(
            store
                .enqueue_work(&second, &account, &hijack, 100)
                .expect("enqueue"),
            Enqueued::ForeignSession
        );
        assert_eq!(
            store
                .enqueue_work(&first, "acc_someone_else", &hijack, 100)
                .expect("enqueue"),
            Enqueued::ForeignSession
        );
    }

    #[test]
    fn an_old_database_gains_the_new_columns() {
        let connection = Connection::open_in_memory().expect("open");
        // The work and sessions tables as the first release created them.
        connection
            .execute_batch(
                "CREATE TABLE work (work_id TEXT PRIMARY KEY NOT NULL, environment_id TEXT NOT NULL,
                     work_type TEXT NOT NULL, state TEXT NOT NULL, session_id TEXT, prompt TEXT,
                     session_token_hmac BLOB, created_at_unix INTEGER NOT NULL,
                     updated_at_unix INTEGER NOT NULL, leased_at_unix INTEGER,
                     lease_deadline_unix INTEGER, force INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE sessions (session_id TEXT PRIMARY KEY NOT NULL,
                     environment_id TEXT NOT NULL, account_id TEXT NOT NULL, state TEXT NOT NULL,
                     created_at_unix INTEGER NOT NULL, updated_at_unix INTEGER NOT NULL);",
            )
            .expect("old schema");
        let store = Store::from_connection(connection).expect("migrates");
        let connection = store.lock();
        for (table, column, _) in ADDED_COLUMNS {
            let present: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
                    params![table, column],
                    |row| row.get(0),
                )
                .expect("pragma");
            assert!(present, "{table}.{column}");
        }
        drop(connection);
        // And opening it again is a no-op.
        migrate(&store.lock()).expect("idempotent");
    }

    #[test]
    fn a_keyed_event_is_stored_once_per_session() {
        let (store, account, environment, session) = store_with_session();
        let first = store
            .record_keyed_session_event(&session, "session_message", "{}", Some("k"), 200)
            .expect("record");
        let Recorded::New(first_id) = first else {
            panic!("expected a new event, got {first:?}");
        };
        assert_eq!(
            store
                .record_keyed_session_event(&session, "session_message", "{}", Some("k"), 300)
                .expect("record"),
            Recorded::Duplicate(first_id)
        );
        // Unkeyed events are never deduplicated.
        store
            .record_keyed_session_event(&session, "session_message", "{}", None, 300)
            .expect("record");
        store
            .record_keyed_session_event(&session, "session_message", "{}", None, 300)
            .expect("record");
        assert_eq!(store.session_events(&session).expect("events").len(), 3);
        // The duplicate was not activity; the unkeyed ones were.
        let listed = store.list_sessions(&account, None, None, 10).expect("list");
        assert_eq!(listed[0].updated_at_unix, 300);

        // The same key in another session is a different event.
        let (_, other) = enqueue_session(&store, &environment, &account, &session_work("x"), 100);
        assert!(matches!(
            store
                .record_keyed_session_event(&other, "session_message", "{}", Some("k"), 200)
                .expect("record"),
            Recorded::New(_)
        ));
        // And the index refuses a second row even past the lookup.
        let direct = store.lock().execute(
            "INSERT INTO session_events (session_id, kind, payload_json, created_at_unix, dedupe_key)
             VALUES (?1, 'session_message', '{}', 0, 'k')",
            params![session],
        );
        assert!(direct.is_err());
    }

    fn answer<'a>(
        request_id: &'a str,
        device_id: &'a str,
        connection_id: u64,
        worker_connection_id: u64,
    ) -> AnswerAttempt<'a> {
        AnswerAttempt {
            request_id,
            device_id,
            connection_id,
            worker_connection_id,
        }
    }

    fn claim(store: &Store, session: &str, attempt: AnswerAttempt<'_>) -> AnswerClaim {
        store
            .record_answer(session, "permission_response", "{}", attempt, 300)
            .expect("record answer")
    }

    fn claims(store: &Store) -> i64 {
        store
            .lock()
            .query_row("SELECT COUNT(*) FROM session_answers", [], |row| row.get(0))
            .expect("count")
    }

    #[test]
    fn the_first_answer_to_a_prompt_holds_it() {
        let (store, account, device) = store_with_device();
        let environment = store
            .register_environment(&account, &device, &config(), &[9u8; 32], 100)
            .expect("register")
            .environment_id;
        let (_, session) =
            enqueue_session(&store, &environment, &account, &session_work("hello"), 100);

        // Connection 1 answers first, routed to worker socket 9.
        let AnswerClaim::Recorded(first) = claim(&store, &session, answer("perm-1", &device, 1, 9))
        else {
            panic!("the first answer is taken");
        };
        // Connection 2 — the same device, another surface — is refused,
        // told who holds it, and nothing is written.
        let refused = claim(&store, &session, answer("perm-1", &device, 2, 9));
        assert_eq!(
            refused,
            AnswerClaim::Taken(AnswerHolder {
                event_id: first,
                device_id: device.clone(),
                label: Some("first".into()),
                connection_id: 1,
            })
        );
        assert_eq!(store.session_events(&session).expect("events").len(), 1);
        // Another prompt is its own race.
        assert!(matches!(
            claim(&store, &session, answer("perm-2", &device, 2, 9)),
            AnswerClaim::Recorded(_)
        ));
        // The holder may send again; it keeps the claim.
        let AnswerClaim::Recorded(again) = claim(&store, &session, answer("perm-1", &device, 1, 9))
        else {
            panic!("the holder may retry");
        };
        assert!(matches!(
            claim(&store, &session, answer("perm-1", &device, 2, 9)),
            AnswerClaim::Taken(AnswerHolder { event_id, connection_id: 1, .. }) if event_id == again
        ));

        // The runner refuses the answer: the prompt is open again, and the
        // refusal is part of the history.
        let refusal = store
            .record_answer_refusal(&session, "{\"type\":\"control_response\"}", "perm-1", 310)
            .expect("refusal");
        assert!(refusal > again);
        let AnswerClaim::Recorded(retaken) =
            claim(&store, &session, answer("perm-1", &device, 2, 9))
        else {
            panic!("a released prompt is taken by the next answer");
        };
        assert!(matches!(
            claim(&store, &session, answer("perm-1", &device, 1, 9)),
            AnswerClaim::Taken(AnswerHolder { event_id, connection_id: 2, .. }) if event_id == retaken
        ));
        // A refusal for a prompt nobody holds is still stored.
        store
            .record_answer_refusal(&session, "{}", "perm-unheld", 310)
            .expect("refusal");
        assert_eq!(claims(&store), 2);
    }

    #[test]
    fn a_claim_routed_to_a_worker_socket_that_is_gone_does_not_hold() {
        let (store, _, _, session) = store_with_session();
        assert!(matches!(
            claim(&store, &session, answer("perm-1", "dev_a", 1, 9)),
            AnswerClaim::Recorded(_)
        ));
        // The worker reconnected (or the server restarted): the first
        // answer may never have reached anyone, so a second one goes
        // through and takes the claim over.
        let AnswerClaim::Recorded(taken) =
            claim(&store, &session, answer("perm-1", "dev_b", 2, 10))
        else {
            panic!("an answer to a new worker socket is taken");
        };
        // From then on, the new claim holds on the new socket. The device
        // row is gone for `dev_b`, so there is no label to show.
        assert_eq!(
            claim(&store, &session, answer("perm-1", "dev_a", 1, 10)),
            AnswerClaim::Taken(AnswerHolder {
                event_id: taken,
                device_id: "dev_b".into(),
                label: None,
                connection_id: 2,
            })
        );
        assert_eq!(claims(&store), 1);
    }

    #[test]
    fn connection_ids_survive_the_signed_column() {
        let (store, _, _, session) = store_with_session();
        let high = u64::MAX - 1;
        claim(&store, &session, answer("perm-1", "dev_a", high, u64::MAX));
        assert!(matches!(
            claim(&store, &session, answer("perm-1", "dev_b", 3, u64::MAX)),
            AnswerClaim::Taken(AnswerHolder { connection_id, .. }) if connection_id == high
        ));
        assert!(matches!(
            claim(&store, &session, answer("perm-1", "dev_a", high, u64::MAX)),
            AnswerClaim::Recorded(_)
        ));
    }

    #[test]
    fn device_issuance_and_revocation_are_audited() {
        let (store, account, device) = store_with_device();
        store
            .revoke_device(&account, &device, &device, 150)
            .expect("revoke");
        let actions: Vec<String> = store
            .audit()
            .expect("audit")
            .into_iter()
            .map(|row| row.action)
            .collect();
        assert_eq!(
            actions,
            vec!["account.bootstrap", "device.issue", "device.revoke"]
        );
    }
}
