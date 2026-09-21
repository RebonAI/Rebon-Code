use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    net::{IpAddr, SocketAddr},
    path::{Path as StdPath, PathBuf},
    sync::{
        atomic::{AtomicU16, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use axum::body::Bytes;
use axum::{
    extract::{
        rejection::JsonRejection,
        ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade},
        ConnectInfo, OriginalUri, Path, State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{SecondsFormat, Utc};
use fs2::FileExt;
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};
use tracing::{error as log_error, info};
use url::Url;

const PROTOCOL: u8 = 2;
const SUBPROTOCOL: &str = "rebon.e2ee.v2";
const MAX_FRAME: usize = 65_536;
const MAX_QUEUE_BYTES: usize = 262_144;
const TRAFFIC_BYTES_PER_SECOND: f64 = 1_048_576.0;
const TRAFFIC_FRAMES_PER_SECOND: f64 = 64.0;
const TRAFFIC_BYTE_BURST: f64 = 2_097_152.0;
const TRAFFIC_FRAME_BURST: f64 = 128.0;
/// Close code sent to both sockets when a pairing is authoritatively revoked
/// via the cancel endpoint. Distinct from 4408 (liveness/expiry timeout) so a
/// client can tell a permanent revocation (drop the stored credentials) apart
/// from a transient timeout (reconnect).
const CLOSE_REVOKED: u16 = 4410;
/// Close code sent to a role's previous socket when a new authenticated
/// connection for the same role takes over the slot. Clients treat it as a
/// transient supersession rather than a hard failure.
const CLOSE_SUPERSEDED: u16 = 4426;

type TokenDigest = [u8; 32];

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub public_url: String,
    pub pending_cap: usize,
    pub trusted_cap: usize,
    pub sockets_per_ip: usize,
    /// When true the client IP for per-source limits is taken from the
    /// rightmost `X-Forwarded-For` entry (the value appended by the trusted
    /// reverse proxy) instead of the TCP peer. Enable only behind a proxy that
    /// overwrites/appends this header; leaving it false keys limits on the TCP
    /// peer, which collapses to the proxy address when one is present.
    pub trust_forwarded_for: bool,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        let url = Url::parse(&self.public_url).map_err(|e| format!("invalid public URL: {e}"))?;
        if url.cannot_be_a_base()
            || url.host_str().is_none()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(
                "public URL must be an origin without credentials, path, query, or fragment".into(),
            );
        }
        let secure = url.scheme() == "https";
        let debug_loopback = cfg!(debug_assertions)
            && url.scheme() == "http"
            && url.host_str().is_some_and(|host| {
                host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
            });
        if !secure && !debug_loopback {
            return Err("public URL must use HTTPS (debug builds permit HTTP loopback)".into());
        }
        if self.pending_cap == 0 || self.trusted_cap == 0 || self.sockets_per_ip == 0 {
            return Err("capacity limits must be nonzero".into());
        }
        Ok(())
    }
}

struct PairingStore {
    connection: Mutex<Connection>,
    _instance_lock: Option<File>,
}

struct TrustedRecord {
    pairing_id: String,
    desktop_digest: TokenDigest,
    mobile_digest: TokenDigest,
}

fn database_lock_path(path: &StdPath) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    PathBuf::from(value)
}

fn sqlite_io_error(error: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

impl PairingStore {
    fn open(path: impl AsRef<StdPath>) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        let lock_path = database_lock_path(path);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(sqlite_io_error)?;
        lock.try_lock_exclusive().map_err(sqlite_io_error)?;
        Self::from_connection(Connection::open(path)?, Some(lock))
    }

    fn in_memory() -> rusqlite::Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, None)
    }

    fn from_connection(
        connection: Connection,
        instance_lock: Option<File>,
    ) -> rusqlite::Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS trusted_pairings (
                 pairing_id TEXT PRIMARY KEY NOT NULL,
                 desktop_token_hmac BLOB NOT NULL CHECK(length(desktop_token_hmac) = 32),
                 mobile_token_hmac BLOB NOT NULL CHECK(length(mobile_token_hmac) = 32),
                 trusted_at_unix INTEGER NOT NULL
             );",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
            _instance_lock: instance_lock,
        })
    }

    fn load_trusted(&self) -> rusqlite::Result<Vec<TrustedRecord>> {
        let connection = self.connection.lock().expect("database lock");
        let mut statement = connection.prepare(
            "SELECT pairing_id, desktop_token_hmac, mobile_token_hmac
             FROM trusted_pairings",
        )?;
        let rows = statement.query_map([], |row| {
            let pairing_id = row.get::<_, String>(0)?;
            let desktop = row.get::<_, Vec<u8>>(1)?;
            let mobile = row.get::<_, Vec<u8>>(2)?;
            Ok((pairing_id, desktop, mobile))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (pairing_id, desktop, mobile) = row?;
            let (Ok(desktop_digest), Ok(mobile_digest)) = (desktop.try_into(), mobile.try_into())
            else {
                return Err(rusqlite::Error::InvalidQuery);
            };
            if decode_id(&pairing_id).is_none() {
                return Err(rusqlite::Error::InvalidQuery);
            }
            records.push(TrustedRecord {
                pairing_id,
                desktop_digest,
                mobile_digest,
            });
        }
        Ok(records)
    }

    fn trust(
        &self,
        pairing_id: &str,
        desktop_digest: &TokenDigest,
        mobile_digest: &TokenDigest,
        trusted_cap: usize,
    ) -> rusqlite::Result<bool> {
        let mut connection = self.connection.lock().expect("database lock");
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Idempotent: if the record already exists (e.g. a second connection for
        // the same pairing raced the first through the trust transition), report
        // success without re-inserting or re-checking capacity.
        let already: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM trusted_pairings WHERE pairing_id = ?1)",
            [pairing_id],
            |row| row.get(0),
        )?;
        if already {
            transaction.commit()?;
            return Ok(true);
        }
        let trusted_count: i64 =
            transaction.query_row("SELECT COUNT(*) FROM trusted_pairings", [], |row| {
                row.get(0)
            })?;
        if trusted_count >= i64::try_from(trusted_cap).unwrap_or(i64::MAX) {
            transaction.rollback()?;
            return Ok(false);
        }
        transaction.execute(
            "INSERT INTO trusted_pairings (
                 pairing_id, desktop_token_hmac, mobile_token_hmac, trusted_at_unix
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                pairing_id,
                desktop_digest.as_slice(),
                mobile_digest.as_slice(),
                Utc::now().timestamp()
            ],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    fn rotate(
        &self,
        pairing_id: &str,
        role: Role,
        current_digest: &TokenDigest,
        new_digest: &TokenDigest,
    ) -> rusqlite::Result<bool> {
        let column = match role {
            Role::Desktop => "desktop_token_hmac",
            Role::Mobile => "mobile_token_hmac",
        };
        let sql = format!(
            "UPDATE trusted_pairings SET {column} = ?1
             WHERE pairing_id = ?2 AND {column} = ?3"
        );
        let changed = self.connection.lock().expect("database lock").execute(
            &sql,
            params![new_digest.as_slice(), pairing_id, current_digest.as_slice()],
        )?;
        Ok(changed == 1)
    }

    fn revoke(&self, pairing_id: &str) -> rusqlite::Result<()> {
        self.connection.lock().expect("database lock").execute(
            "DELETE FROM trusted_pairings WHERE pairing_id = ?1",
            [pairing_id],
        )?;
        Ok(())
    }
}

pub struct RelayState {
    config: Config,
    hmac_key: [u8; 32],
    store: PairingStore,
    rooms: Mutex<HashMap<String, Arc<Room>>>,
    /// Count of rooms currently in `Lifecycle::Pending`. Maintained at every
    /// pending create / transition / removal while the `rooms` lock is held so
    /// `create_pairing` can enforce `pending_cap` in O(1) instead of scanning.
    pending_count: AtomicUsize,
    tombstones: Mutex<HashMap<TokenDigest, Instant>>,
    /// Manual pairing-code handoff index: HMAC(claim) → pairing id. Entries are
    /// memory-only, at most one per pending room, validated against the room's
    /// current handoff on claim, and swept when their room disappears. Lock
    /// order: `rooms` may be held while taking this lock, never the reverse.
    handoff_claims: Mutex<HashMap<TokenDigest, String>>,
    rate: Mutex<HashMap<IpAddr, RateBucket>>,
    traffic: Mutex<HashMap<IpAddr, TrafficBucket>>,
    sockets: Mutex<HashMap<IpAddr, usize>>,
}

struct RateBucket {
    tokens: f64,
    updated: Instant,
}

struct TrafficBucket {
    bytes: f64,
    frames: f64,
    updated: Instant,
}

struct Room {
    correlation: String,
    inner: Mutex<RoomInner>,
}

struct RoomInner {
    desktop_digest: TokenDigest,
    mobile_digest: TokenDigest,
    expires_at: Instant,
    lifecycle: Lifecycle,
    desktop: Option<Slot>,
    mobile: Option<Slot>,
    /// Encrypted manual pairing-code handoff box, present only while Pending.
    /// The relay never holds the pairing code or any key material: the box is
    /// opaque ciphertext and `claim_digest` is an HMAC of a value the mobile
    /// client can only derive from the code itself.
    handoff: Option<Handoff>,
}

struct Handoff {
    claim_digest: TokenDigest,
    box_b64: String,
}

#[derive(Clone, Copy)]
enum Lifecycle {
    Pending,
    Trusted,
}

struct Slot {
    id: u64,
    tx: mpsc::Sender<Outbound>,
    queued_bytes: Arc<AtomicUsize>,
    cancel: CancellationToken,
    close_code: Arc<AtomicU16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Desktop,
    Mobile,
}

impl Role {
    fn other(self) -> Self {
        match self {
            Self::Desktop => Self::Mobile,
            Self::Mobile => Self::Desktop,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Mobile => "mobile",
        }
    }
}

enum Outbound {
    Binary(Bytes),
}
impl Outbound {
    fn len(&self) -> usize {
        match self {
            Self::Binary(v) => v.len(),
        }
    }
}

impl RelayState {
    pub fn new(config: Config) -> Self {
        let mut hmac_key = [0; 32];
        OsRng.fill_bytes(&mut hmac_key);
        Self::from_store(
            config,
            hmac_key,
            PairingStore::in_memory().expect("initialize in-memory pairing database"),
        )
        .expect("load empty in-memory pairing database")
    }

    pub fn open(
        config: Config,
        database_path: impl AsRef<StdPath>,
        hmac_key: [u8; 32],
    ) -> rusqlite::Result<Self> {
        Self::from_store(config, hmac_key, PairingStore::open(database_path)?)
    }

    fn from_store(
        config: Config,
        hmac_key: [u8; 32],
        store: PairingStore,
    ) -> rusqlite::Result<Self> {
        let mut rooms = HashMap::new();
        let trusted = store.load_trusted()?;
        if trusted.len() > config.trusted_cap {
            return Err(rusqlite::Error::InvalidQuery);
        }
        for record in trusted {
            let pairing_bytes =
                decode_id(&record.pairing_id).ok_or(rusqlite::Error::InvalidQuery)?;
            let correlation = hex8(&hmac_digest(&hmac_key, &pairing_bytes));
            rooms.insert(
                record.pairing_id,
                Arc::new(Room {
                    correlation,
                    inner: Mutex::new(RoomInner {
                        desktop_digest: record.desktop_digest,
                        mobile_digest: record.mobile_digest,
                        expires_at: Instant::now(),
                        lifecycle: Lifecycle::Trusted,
                        desktop: None,
                        mobile: None,
                        handoff: None,
                    }),
                }),
            );
        }
        Ok(Self {
            config,
            hmac_key,
            store,
            rooms: Mutex::new(rooms),
            // Loaded rooms are all Trusted; no pending rooms exist at startup.
            pending_count: AtomicUsize::new(0),
            tombstones: Mutex::new(HashMap::new()),
            handoff_claims: Mutex::new(HashMap::new()),
            rate: Mutex::new(HashMap::new()),
            traffic: Mutex::new(HashMap::new()),
            sockets: Mutex::new(HashMap::new()),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn shutdown(&self) {
        let rooms: Vec<(String, Arc<Room>)> = self
            .rooms
            .lock()
            .expect("rooms lock")
            .iter()
            .map(|(id, room)| (id.clone(), room.clone()))
            .collect();
        for (id, room) in rooms {
            self.remove_room(&id, &room, 1000, "shutdown");
        }
        self.tombstones.lock().expect("tombstones lock").clear();
        self.handoff_claims
            .lock()
            .expect("handoff claims lock")
            .clear();
        self.rate.lock().expect("rate lock").clear();
        self.traffic.lock().expect("traffic lock").clear();
    }

    pub fn start_cleanup(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                self.cleanup();
            }
        });
    }

    fn digest(&self, value: &[u8]) -> TokenDigest {
        hmac_digest(&self.hmac_key, value)
    }

    fn cancellation_digest(&self, pairing_id: &[u8; 16], token: &[u8]) -> TokenDigest {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.hmac_key).expect("HMAC accepts any key");
        mac.update(b"rebon-cancellation-v1\0");
        mac.update(pairing_id);
        mac.update(token);
        mac.finalize().into_bytes().into()
    }

    fn handoff_claim_digest(&self, claim: &[u8]) -> TokenDigest {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.hmac_key).expect("HMAC accepts any key");
        mac.update(b"rebon-handoff-claim-v1\0");
        mac.update(claim);
        mac.finalize().into_bytes().into()
    }

    fn allow_creation(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut rate = self.rate.lock().expect("rate lock");
        rate.retain(|_, bucket| now.duration_since(bucket.updated) < Duration::from_secs(60));
        let maximum_sources = self.config.pending_cap.saturating_mul(2).max(1_024);
        if !rate.contains_key(&ip) && rate.len() >= maximum_sources {
            return false;
        }
        let bucket = rate.entry(ip).or_insert(RateBucket {
            tokens: 3.0,
            updated: now,
        });
        bucket.tokens =
            (3.0_f64).min(bucket.tokens + now.duration_since(bucket.updated).as_secs_f64() / 6.0);
        bucket.updated = now;
        if bucket.tokens < 1.0 {
            false
        } else {
            bucket.tokens -= 1.0;
            true
        }
    }

    fn allow_traffic(&self, ip: IpAddr, bytes: usize) -> bool {
        let now = Instant::now();
        let mut traffic = self.traffic.lock().expect("traffic lock");
        let bucket = traffic.entry(ip).or_insert(TrafficBucket {
            bytes: TRAFFIC_BYTE_BURST,
            frames: TRAFFIC_FRAME_BURST,
            updated: now,
        });
        let elapsed = now.duration_since(bucket.updated).as_secs_f64();
        bucket.bytes = TRAFFIC_BYTE_BURST.min(bucket.bytes + elapsed * TRAFFIC_BYTES_PER_SECOND);
        bucket.frames =
            TRAFFIC_FRAME_BURST.min(bucket.frames + elapsed * TRAFFIC_FRAMES_PER_SECOND);
        bucket.updated = now;
        if bucket.bytes < bytes as f64 || bucket.frames < 1.0 {
            return false;
        }
        bucket.bytes -= bytes as f64;
        bucket.frames -= 1.0;
        true
    }

    fn reserve_socket(&self, ip: IpAddr) -> bool {
        let mut sockets = self.sockets.lock().expect("socket lock");
        let count = sockets.entry(ip).or_default();
        if *count >= self.config.sockets_per_ip {
            false
        } else {
            *count += 1;
            true
        }
    }

    fn release_socket(&self, ip: IpAddr) {
        let mut sockets = self.sockets.lock().expect("socket lock");
        if let Some(count) = sockets.get_mut(&ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                sockets.remove(&ip);
            }
        }
    }

    fn cleanup(&self) {
        let now = Instant::now();
        #[cfg(debug_assertions)]
        {
            // The pending counter is only mutated while the rooms lock is held,
            // so holding it here yields a consistent snapshot to validate against.
            let rooms = self.rooms.lock().expect("rooms lock");
            let actual = rooms
                .values()
                .filter(|room| {
                    matches!(
                        room.inner.lock().expect("room lock").lifecycle,
                        Lifecycle::Pending
                    )
                })
                .count();
            debug_assert_eq!(
                actual,
                self.pending_count.load(Ordering::Relaxed),
                "pending counter drifted from room map"
            );
        }
        let rooms: Vec<(String, Arc<Room>)> = self
            .rooms
            .lock()
            .expect("rooms lock")
            .iter()
            .map(|(id, room)| (id.clone(), room.clone()))
            .collect();
        for (id, room) in rooms {
            self.remove_room_if_expired(&id, &room, now);
        }
        self.tombstones
            .lock()
            .expect("tombstones lock")
            .retain(|_, until| *until > now);
        {
            // Sweep handoff claim entries whose room is gone. Holding the rooms
            // lock while retaining keeps a concurrent create+handoff from being
            // dropped against a stale room snapshot (lock order: rooms → claims).
            let rooms = self.rooms.lock().expect("rooms lock");
            self.handoff_claims
                .lock()
                .expect("handoff claims lock")
                .retain(|_, pairing_id| rooms.contains_key(pairing_id));
        }
        self.rate
            .lock()
            .expect("rate lock")
            .retain(|_, bucket| now.duration_since(bucket.updated) < Duration::from_secs(60));
        self.traffic
            .lock()
            .expect("traffic lock")
            .retain(|_, bucket| now.duration_since(bucket.updated) < Duration::from_secs(60));
    }

    fn remove_room_if_expired(&self, id: &str, expected: &Arc<Room>, now: Instant) {
        let mut rooms = self.rooms.lock().expect("rooms lock");
        let Some(registered) = rooms.get(id).cloned() else {
            return;
        };
        if !Arc::ptr_eq(&registered, expected) {
            return;
        }
        let mut inner = registered.inner.lock().expect("room lock");
        let expired = match inner.lifecycle {
            Lifecycle::Pending => now >= inner.expires_at,
            Lifecycle::Trusted => false,
        };
        if !expired {
            return;
        }
        rooms.remove(id);
        // Only a Pending room can be expired here, so the counter always drops.
        self.pending_count.fetch_sub(1, Ordering::Relaxed);
        cancel_slot(inner.desktop.take(), 4408);
        cancel_slot(inner.mobile.take(), 4408);
        drop(inner);
        drop(rooms);
        info!(pairing = %registered.correlation, state_transition = "timeout");
    }

    fn remove_room(&self, id: &str, expected: &Arc<Room>, code: u16, transition: &'static str) {
        let removed = {
            let mut rooms = self.rooms.lock().expect("rooms lock");
            match rooms.get(id) {
                Some(room) if Arc::ptr_eq(room, expected) => {
                    // Adjust the pending counter under the rooms lock so it stays
                    // consistent with the map for concurrent observers.
                    if matches!(
                        room.inner.lock().expect("room lock").lifecycle,
                        Lifecycle::Pending
                    ) {
                        self.pending_count.fetch_sub(1, Ordering::Relaxed);
                    }
                    rooms.remove(id)
                }
                _ => None,
            }
        };
        if let Some(room) = removed {
            let mut inner = room.inner.lock().expect("room lock");
            cancel_slot(inner.desktop.take(), code);
            cancel_slot(inner.mobile.take(), code);
            info!(pairing = %room.correlation, state_transition = transition);
        }
    }
}

pub fn app(state: Arc<RelayState>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/pairings", post(create_pairing))
        .route("/v1/pairings/{pairing_id}", delete(cancel_pairing))
        .route("/v1/pairings/{pairing_id}/handoff", post(store_handoff))
        .route("/v1/handoffs/claim", post(claim_handoff))
        .route(
            "/v1/pairings/{pairing_id}/tokens/rotate",
            post(rotate_token),
        )
        .route("/v1/pairings/{pairing_id}/socket", get(connect_socket))
        .layer(RequestBodyLimitLayer::new(1024))
        .layer(TraceLayer::new_for_http().make_span_with(
            |request: &axum::http::Request<axum::body::Body>| {
                tracing::info_span!(
                    "http_request",
                    method = %request.method(),
                    path = request.uri().path()
                )
            },
        ))
        .layer(middleware::map_response(normalize_response))
        .with_state(state)
}

async fn normalize_response(response: Response) -> Response {
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE {
        error(StatusCode::BAD_REQUEST)
    } else {
        no_store(response)
    }
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    protocol: u8,
}
async fn health() -> impl IntoResponse {
    no_store(
        (
            StatusCode::OK,
            Json(Health {
                status: "ok",
                protocol: PROTOCOL,
            }),
        )
            .into_response(),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    protocol: u8,
    expires_in_seconds: u64,
}

#[derive(Serialize)]
struct CreateResponse {
    protocol: u8,
    pairing_id: String,
    desktop_token: String,
    mobile_token: String,
    expires_at: String,
}

async fn create_pairing(
    State(state): State<Arc<RelayState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    request: Result<Json<CreateRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = request else {
        return error(StatusCode::BAD_REQUEST);
    };
    if request.protocol != PROTOCOL || !(30..=300).contains(&request.expires_in_seconds) {
        return error(StatusCode::BAD_REQUEST);
    }
    let ip = client_ip(&state.config, &headers, peer);
    if !state.allow_creation(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS);
    }
    let mut rooms = state.rooms.lock().expect("rooms lock");
    if state.pending_count.load(Ordering::Relaxed) >= state.config.pending_cap {
        return error(StatusCode::SERVICE_UNAVAILABLE);
    }

    let (pairing_id, pairing_bytes) = loop {
        let mut bytes = [0; 16];
        OsRng.fill_bytes(&mut bytes);
        let id = URL_SAFE_NO_PAD.encode(bytes);
        if !rooms.contains_key(&id) {
            break (id, bytes);
        }
    };
    let mut desktop = [0; 32];
    OsRng.fill_bytes(&mut desktop);
    let mut mobile = [0; 32];
    OsRng.fill_bytes(&mut mobile);
    let desktop_digest = state.digest(&desktop);
    let mobile_digest = state.digest(&mobile);
    let desktop_token = URL_SAFE_NO_PAD.encode(desktop);
    let mobile_token = URL_SAFE_NO_PAD.encode(mobile);
    desktop.fill(0);
    mobile.fill(0);
    let duration = Duration::from_secs(request.expires_in_seconds);
    let expires_at_utc = Utc::now() + chrono::Duration::seconds(request.expires_in_seconds as i64);
    let correlation = hex8(&state.digest(&pairing_bytes));
    rooms.insert(
        pairing_id.clone(),
        Arc::new(Room {
            correlation: correlation.clone(),
            inner: Mutex::new(RoomInner {
                desktop_digest,
                mobile_digest,
                expires_at: Instant::now() + duration,
                lifecycle: Lifecycle::Pending,
                desktop: None,
                mobile: None,
                handoff: None,
            }),
        }),
    );
    state.pending_count.fetch_add(1, Ordering::Relaxed);
    drop(rooms);
    info!(pairing = %correlation, state_transition = "created");
    no_store(
        (
            StatusCode::CREATED,
            Json(CreateResponse {
                protocol: PROTOCOL,
                pairing_id,
                desktop_token,
                mobile_token,
                expires_at: expires_at_utc.to_rfc3339_opts(SecondsFormat::Secs, true),
            }),
        )
            .into_response(),
    )
}

async fn cancel_pairing(
    State(state): State<Arc<RelayState>>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if uri.query().is_some() {
        return error(StatusCode::BAD_REQUEST);
    }
    let Some(pairing_bytes) = decode_id(&id) else {
        return error(StatusCode::UNAUTHORIZED);
    };
    let Some(token) = bearer(&headers) else {
        return error(StatusCode::UNAUTHORIZED);
    };
    let mut token_bytes = URL_SAFE_NO_PAD.decode(token).expect("bearer was validated");
    let digest = state.digest(&token_bytes);
    let tombstone_digest = state.cancellation_digest(&pairing_bytes, &token_bytes);
    token_bytes.fill(0);

    // Phase 1: authenticate and remove the room from the map (the source of
    // truth) before the blocking revoke, so a concurrent trust transition cannot
    // resurrect a record we are about to delete.
    let (room, was_pending) = {
        let mut rooms = state.rooms.lock().expect("rooms lock");
        let Some(room) = rooms.get(&id).cloned() else {
            drop(rooms);
            let valid = state
                .tombstones
                .lock()
                .expect("tombstones lock")
                .get(&tombstone_digest)
                .is_some_and(|until| *until > Instant::now());
            return if valid {
                no_store(StatusCode::NO_CONTENT.into_response())
            } else {
                error(StatusCode::UNAUTHORIZED)
            };
        };
        let (valid, was_pending) = {
            let inner = room.inner.lock().expect("room lock");
            let valid: bool =
                (digest.ct_eq(&inner.desktop_digest) | digest.ct_eq(&inner.mobile_digest)).into();
            (valid, matches!(inner.lifecycle, Lifecycle::Pending))
        };
        if !valid {
            return error(StatusCode::UNAUTHORIZED);
        }
        if was_pending {
            state.pending_count.fetch_sub(1, Ordering::Relaxed);
        }
        rooms.remove(&id).expect("room remained registered");
        (room, was_pending)
    };

    // Phase 2: persist the revoke off the async worker without holding a lock.
    let revoke_state = state.clone();
    let revoke_id = id.clone();
    let revoke_result = tokio::task::spawn_blocking(move || revoke_state.store.revoke(&revoke_id))
        .await
        .expect("revoke store task");
    if let Err(database_error) = revoke_result {
        log_error!(pairing = %room.correlation, error = %database_error, "failed to revoke trusted pairing");
        // The persisted record still exists; restore the in-memory room (its
        // sockets were left intact) so memory and the database do not diverge.
        let mut rooms = state.rooms.lock().expect("rooms lock");
        if !rooms.contains_key(&id) {
            if was_pending {
                state.pending_count.fetch_add(1, Ordering::Relaxed);
            }
            rooms.insert(id.clone(), room.clone());
        }
        return error(StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Phase 3: finalize — tombstone the token and close any live sockets.
    state
        .tombstones
        .lock()
        .expect("tombstones lock")
        .insert(tombstone_digest, Instant::now() + Duration::from_secs(300));
    {
        let mut inner = room.inner.lock().expect("room lock");
        cancel_slot(inner.desktop.take(), CLOSE_REVOKED);
        cancel_slot(inner.mobile.take(), CLOSE_REVOKED);
    }
    info!(pairing = %room.correlation, state_transition = "cancelled");
    no_store(StatusCode::NO_CONTENT.into_response())
}

/// Upper bound on the decoded handoff box: a 24-byte XChaCha20 nonce plus the
/// encrypted QR payload (canonically well under 600 bytes) plus the 16-byte
/// tag. Keeps the base64 form inside the global 1 KiB request body limit.
const MAX_HANDOFF_BOX_BYTES: usize = 640;
/// Lower bound: nonce + tag + at least one ciphertext byte.
const MIN_HANDOFF_BOX_BYTES: usize = 41;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffRequest {
    protocol: u8,
    claim: String,
    #[serde(rename = "box")]
    box_b64: String,
}

async fn store_handoff(
    State(state): State<Arc<RelayState>>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Result<Json<HandoffRequest>, JsonRejection>,
) -> Response {
    if uri.query().is_some() {
        return error(StatusCode::BAD_REQUEST);
    }
    let Ok(Json(request)) = request else {
        return error(StatusCode::BAD_REQUEST);
    };
    if request.protocol != PROTOCOL {
        return error(StatusCode::BAD_REQUEST);
    }
    let Some(claim_bytes) = decode_canonical(&request.claim, 32, 32) else {
        return error(StatusCode::BAD_REQUEST);
    };
    if decode_canonical(
        &request.box_b64,
        MIN_HANDOFF_BOX_BYTES,
        MAX_HANDOFF_BOX_BYTES,
    )
    .is_none()
    {
        return error(StatusCode::BAD_REQUEST);
    }
    if decode_id(&id).is_none() {
        return error(StatusCode::UNAUTHORIZED);
    }
    let Some(token) = bearer(&headers) else {
        return error(StatusCode::UNAUTHORIZED);
    };
    let mut token_bytes = URL_SAFE_NO_PAD.decode(token).expect("bearer was validated");
    let digest = state.digest(&token_bytes);
    token_bytes.fill(0);
    let claim_digest = state.handoff_claim_digest(&claim_bytes);
    let Some(room) = state.rooms.lock().expect("rooms lock").get(&id).cloned() else {
        return error(StatusCode::UNAUTHORIZED);
    };
    // Only the desktop role publishes a handoff, and only while the room is
    // still Pending; re-publishing replaces the previous box atomically.
    let previous = {
        let mut inner = room.inner.lock().expect("room lock");
        if !bool::from(digest.ct_eq(&inner.desktop_digest)) {
            return error(StatusCode::UNAUTHORIZED);
        }
        match inner.lifecycle {
            Lifecycle::Trusted => return error(StatusCode::CONFLICT),
            Lifecycle::Pending if Instant::now() >= inner.expires_at => {
                return error(StatusCode::UNAUTHORIZED);
            }
            Lifecycle::Pending => {}
        }
        inner.handoff.replace(Handoff {
            claim_digest,
            box_b64: request.box_b64.clone(),
        })
    };
    {
        let mut claims = state.handoff_claims.lock().expect("handoff claims lock");
        if let Some(previous) = previous {
            claims.remove(&previous.claim_digest);
        }
        claims.insert(claim_digest, id.clone());
    }
    info!(pairing = %room.correlation, state_transition = "handoff_stored");
    no_store(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimRequest {
    protocol: u8,
    claim: String,
}

#[derive(Serialize)]
struct ClaimResponse {
    protocol: u8,
    #[serde(rename = "box")]
    box_b64: String,
}

async fn claim_handoff(
    State(state): State<Arc<RelayState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    request: Result<Json<ClaimRequest>, JsonRejection>,
) -> Response {
    if uri.query().is_some() {
        return error(StatusCode::BAD_REQUEST);
    }
    let Ok(Json(request)) = request else {
        return error(StatusCode::BAD_REQUEST);
    };
    if request.protocol != PROTOCOL {
        return error(StatusCode::BAD_REQUEST);
    }
    let Some(claim_bytes) = decode_canonical(&request.claim, 32, 32) else {
        return error(StatusCode::BAD_REQUEST);
    };
    // Unauthenticated by design (possession of the claim is the credential);
    // shares the pairing-creation budget so codes cannot be brute-forced online.
    let ip = client_ip(&state.config, &headers, peer);
    if !state.allow_creation(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS);
    }
    let claim_digest = state.handoff_claim_digest(&claim_bytes);
    let Some(pairing_id) = state
        .handoff_claims
        .lock()
        .expect("handoff claims lock")
        .get(&claim_digest)
        .cloned()
    else {
        return error(StatusCode::NOT_FOUND);
    };
    let room = state
        .rooms
        .lock()
        .expect("rooms lock")
        .get(&pairing_id)
        .cloned();
    let handoff = room.as_ref().and_then(|room| {
        let mut inner = room.inner.lock().expect("room lock");
        let claimable = matches!(inner.lifecycle, Lifecycle::Pending)
            && Instant::now() < inner.expires_at
            && inner
                .handoff
                .as_ref()
                .is_some_and(|handoff| bool::from(handoff.claim_digest.ct_eq(&claim_digest)));
        if claimable {
            inner.handoff.take()
        } else {
            None
        }
    });
    // Single-use: the index entry is dropped whether the claim succeeded or the
    // room turned out to be gone/stale — a claim can never be replayed.
    state
        .handoff_claims
        .lock()
        .expect("handoff claims lock")
        .remove(&claim_digest);
    match (room, handoff) {
        (Some(room), Some(handoff)) => {
            info!(pairing = %room.correlation, state_transition = "handoff_claimed");
            no_store(
                (
                    StatusCode::OK,
                    Json(ClaimResponse {
                        protocol: PROTOCOL,
                        box_b64: handoff.box_b64,
                    }),
                )
                    .into_response(),
            )
        }
        _ => error(StatusCode::NOT_FOUND),
    }
}

#[derive(Serialize)]
struct RotateResponse {
    protocol: u8,
    role: &'static str,
    token: String,
}

async fn rotate_token(
    State(state): State<Arc<RelayState>>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if uri.query().is_some() {
        return error(StatusCode::BAD_REQUEST);
    }
    if decode_id(&id).is_none() {
        return error(StatusCode::UNAUTHORIZED);
    }
    let Some(token) = bearer(&headers) else {
        return error(StatusCode::UNAUTHORIZED);
    };
    let mut token_bytes = URL_SAFE_NO_PAD.decode(token).expect("bearer was validated");
    let current_digest = state.digest(&token_bytes);
    token_bytes.fill(0);

    // Phase 1: authenticate the role and mint the replacement under the lock.
    let (room, role, new_digest, new_token) = {
        let rooms = state.rooms.lock().expect("rooms lock");
        let Some(room) = rooms.get(&id).cloned() else {
            return error(StatusCode::UNAUTHORIZED);
        };
        let role = {
            let inner = room.inner.lock().expect("room lock");
            let role = if bool::from(current_digest.ct_eq(&inner.desktop_digest)) {
                Role::Desktop
            } else if bool::from(current_digest.ct_eq(&inner.mobile_digest)) {
                Role::Mobile
            } else {
                return error(StatusCode::UNAUTHORIZED);
            };
            if !matches!(inner.lifecycle, Lifecycle::Trusted) {
                return error(StatusCode::CONFLICT);
            }
            role
        };
        let mut new_token_bytes = [0; 32];
        OsRng.fill_bytes(&mut new_token_bytes);
        let new_digest = state.digest(&new_token_bytes);
        let new_token = URL_SAFE_NO_PAD.encode(new_token_bytes);
        new_token_bytes.fill(0);
        (room, role, new_digest, new_token)
    };

    // Phase 2: persist the rotation off the async worker, without holding a
    // lock. The store UPDATE is conditional on `current_digest`, so concurrent
    // rotations serialize and only one succeeds from a given starting digest.
    let rotate_state = state.clone();
    let rotate_id = id.clone();
    let rotate_result = tokio::task::spawn_blocking(move || {
        rotate_state
            .store
            .rotate(&rotate_id, role, &current_digest, &new_digest)
    })
    .await
    .expect("rotate store task");
    match rotate_result {
        Ok(true) => {}
        Ok(false) => {
            log_error!(pairing = %room.correlation, "trusted pairing database was out of sync");
            return error(StatusCode::INTERNAL_SERVER_ERROR);
        }
        Err(database_error) => {
            log_error!(pairing = %room.correlation, error = %database_error, "failed to rotate pairing token");
            return error(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    // Phase 3: commit the in-memory digest. Because only this rotation could
    // have moved the row off `current_digest`, adopt the replacement when the
    // room is still present and unchanged; if the room was revoked meanwhile it
    // was dropped together with its record and there is nothing to update.
    {
        let rooms = state.rooms.lock().expect("rooms lock");
        if rooms.get(&id).is_some_and(|r| Arc::ptr_eq(r, &room)) {
            let mut inner = room.inner.lock().expect("room lock");
            let slot_digest = match role {
                Role::Desktop => inner.desktop_digest,
                Role::Mobile => inner.mobile_digest,
            };
            if bool::from(slot_digest.ct_eq(&current_digest)) {
                match role {
                    Role::Desktop => inner.desktop_digest = new_digest,
                    Role::Mobile => inner.mobile_digest = new_digest,
                }
            }
        }
    }

    info!(pairing = %room.correlation, role = role.name(), state_transition = "token_rotated");
    no_store(
        (
            StatusCode::OK,
            Json(RotateResponse {
                protocol: PROTOCOL,
                role: role.name(),
                token: new_token,
            }),
        )
            .into_response(),
    )
}

async fn connect_socket(
    State(state): State<Arc<RelayState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if uri.query().is_some() {
        return error(StatusCode::BAD_REQUEST);
    }
    if decode_id(&id).is_none() {
        return error(StatusCode::UNAUTHORIZED);
    }
    if !has_subprotocol(&headers) {
        let mut response = error(StatusCode::UPGRADE_REQUIRED);
        response.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(SUBPROTOCOL),
        );
        return response;
    }
    let Some(token) = bearer(&headers) else {
        return error(StatusCode::UNAUTHORIZED);
    };
    let mut token_bytes = URL_SAFE_NO_PAD.decode(token).expect("bearer was validated");
    let digest = state.digest(&token_bytes);
    token_bytes.fill(0);
    let Some(room) = state.rooms.lock().expect("rooms lock").get(&id).cloned() else {
        return error(StatusCode::UNAUTHORIZED);
    };
    let ip = client_ip(&state.config, &headers, peer);
    let (tx, rx) = mpsc::channel(8);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let close_code = Arc::new(AtomicU16::new(4408));
    let slot_id = OsRng.next_u64();

    // Phase 1: authenticate, reserve the per-IP socket, and install the slot
    // (taking over any prior holder of this role) under the room lock. Nothing
    // fallible runs after the slot is installed, so the upgrade guard below owns
    // exactly the reservation and slot registered here.
    let (role, became_trusted, superseded, desktop_digest, mobile_digest) = {
        let rooms = state.rooms.lock().expect("rooms lock");
        if !rooms
            .get(&id)
            .is_some_and(|registered| Arc::ptr_eq(registered, &room))
        {
            return error(StatusCode::UNAUTHORIZED);
        }
        let mut inner = room.inner.lock().expect("room lock");
        let now = Instant::now();
        let role = if bool::from(digest.ct_eq(&inner.desktop_digest)) {
            Role::Desktop
        } else if bool::from(digest.ct_eq(&inner.mobile_digest)) {
            Role::Mobile
        } else {
            return error(StatusCode::UNAUTHORIZED);
        };
        let valid_time = match inner.lifecycle {
            Lifecycle::Pending => now < inner.expires_at,
            Lifecycle::Trusted => true,
        };
        if !valid_time {
            drop(inner);
            drop(rooms);
            state.remove_room(&id, &room, 4408, "expired");
            return error(StatusCode::UNAUTHORIZED);
        }
        if !state.reserve_socket(ip) {
            return error(StatusCode::TOO_MANY_REQUESTS);
        }
        // Takeover: a valid token for an already-occupied role supersedes the
        // previous connection rather than being rejected. The displaced slot is
        // closed with CLOSE_SUPERSEDED after the locks are released. The new slot
        // carries a fresh id, so the old socket task's disconnect cleanup (which
        // compares slot ids) will not tear down this registration.
        let superseded = slot_mut(&mut inner, role).take();
        *slot_mut(&mut inner, role) = Some(Slot {
            id: slot_id,
            tx,
            queued_bytes: queued_bytes.clone(),
            cancel: cancel.clone(),
            close_code: close_code.clone(),
        });
        let became_trusted = matches!(inner.lifecycle, Lifecycle::Pending)
            && inner.desktop.is_some()
            && inner.mobile.is_some();
        let desktop_digest = inner.desktop_digest;
        let mobile_digest = inner.mobile_digest;
        (
            role,
            became_trusted,
            superseded,
            desktop_digest,
            mobile_digest,
        )
    };

    // Close the superseded connection now that the room locks are released.
    cancel_slot(superseded, CLOSE_SUPERSEDED);

    // Owns the reservation and slot on every early return below; the socket task
    // disarms it once it takes over their lifetime.
    let guard = UpgradeGuard {
        state: state.clone(),
        room: room.clone(),
        ip,
        role,
        slot_id,
        armed: true,
    };
    let correlation = room.correlation.clone();

    if became_trusted {
        // Phase 2: persist the trust transition without holding the room locks so
        // the synchronous FULL-durability SQLite write cannot stall other
        // connections waiting on the global rooms lock.
        let store_state = state.clone();
        let trust_id = id.clone();
        let trusted_cap = state.config.trusted_cap;
        let trust_result = tokio::task::spawn_blocking(move || {
            store_state
                .store
                .trust(&trust_id, &desktop_digest, &mobile_digest, trusted_cap)
        })
        .await
        .expect("trust store task");

        match trust_result {
            Ok(true) => {
                // Phase 3: commit the in-memory transition, re-validating that
                // the room and our slot survived the write window.
                let vanished = {
                    let rooms = state.rooms.lock().expect("rooms lock");
                    if rooms.get(&id).is_some_and(|r| Arc::ptr_eq(r, &room)) {
                        let mut inner = room.inner.lock().expect("room lock");
                        if matches!(inner.lifecycle, Lifecycle::Pending)
                            && slot_ref(&inner, role).is_some_and(|slot| slot.id == slot_id)
                        {
                            inner.lifecycle = Lifecycle::Trusted;
                            // A trusted room is no longer claimable by pairing
                            // code; drop the handoff box and its claim entry
                            // (lock order: rooms → claims).
                            let handoff = inner.handoff.take();
                            state.pending_count.fetch_sub(1, Ordering::Relaxed);
                            drop(inner);
                            if let Some(handoff) = handoff {
                                state
                                    .handoff_claims
                                    .lock()
                                    .expect("handoff claims lock")
                                    .remove(&handoff.claim_digest);
                            }
                            info!(pairing = %correlation, state_transition = "trusted");
                        }
                        false
                    } else {
                        true
                    }
                };
                if vanished {
                    // The room was cancelled or expired during the write; undo the
                    // persisted trust so it cannot resurface after a restart.
                    let revert_state = state.clone();
                    let revert_id = id.clone();
                    let _ =
                        tokio::task::spawn_blocking(move || revert_state.store.revoke(&revert_id))
                            .await;
                    return error(StatusCode::UNAUTHORIZED);
                }
            }
            Ok(false) => {
                return error(StatusCode::SERVICE_UNAVAILABLE);
            }
            Err(database_error) => {
                log_error!(pairing = %correlation, error = %database_error, "failed to persist trusted pairing");
                return error(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    info!(pairing = %correlation, role = role.name(), state_transition = "connected");
    ws.protocols([SUBPROTOCOL])
        // Permit one byte beyond the application limit so it can be rejected
        // with the protocol-defined 4413 close code rather than tungstenite's 1009.
        .max_message_size(MAX_FRAME + 1)
        .max_frame_size(MAX_FRAME + 1)
        .on_upgrade(move |socket| {
            // The socket task now owns the reservation and slot cleanup.
            let mut guard = guard;
            guard.disarm();
            socket_task(
                socket,
                state,
                room,
                id,
                role,
                slot_id,
                ip,
                rx,
                queued_bytes,
                cancel,
                close_code,
            )
        })
}

#[allow(clippy::too_many_arguments)]
async fn socket_task(
    mut socket: WebSocket,
    state: Arc<RelayState>,
    room: Arc<Room>,
    id: String,
    role: Role,
    slot_id: u64,
    ip: IpAddr,
    mut rx: mpsc::Receiver<Outbound>,
    queued: Arc<AtomicUsize>,
    cancel: CancellationToken,
    close_code: Arc<AtomicU16>,
) {
    let mut liveness = tokio::time::interval(Duration::from_secs(1));
    liveness.tick().await;
    let mut next_ping = Instant::now() + Duration::from_secs(25);
    let mut last_receive = Instant::now();
    let mut awaiting_pong: Option<Instant> = None;
    let mut local_bytes = TRAFFIC_BYTE_BURST;
    let mut local_frames = TRAFFIC_FRAME_BURST;
    let mut local_updated = Instant::now();
    let close = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break close_code.load(Ordering::Relaxed),
            _ = liveness.tick() => {
                let now = Instant::now();
                if now.duration_since(last_receive) >= Duration::from_secs(75)
                    || awaiting_pong.is_some_and(|sent| now.duration_since(sent) >= Duration::from_secs(10))
                {
                    break 4408;
                }
                if now >= next_ping {
                    if socket.send(Message::Ping(Vec::new().into())).await.is_err() { break 1000; }
                    awaiting_pong = Some(now);
                    next_ping = now + Duration::from_secs(25);
                }
            }
            outbound = rx.recv() => {
                let Some(outbound) = outbound else { break 1000; };
                queued.fetch_sub(outbound.len(), Ordering::AcqRel);
                let Outbound::Binary(value) = outbound;
                if socket.send(Message::Binary(value)).await.is_err() { break 1000; }
            }
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break 1000; };
                last_receive = Instant::now();
                match message {
                    Message::Pong(_) => awaiting_pong = None,
                    Message::Ping(v) => {
                        // Meter pings like data frames so a ping flood is bounded
                        // by the same per-frame/byte budget as relayed traffic.
                        if !meter_traffic(
                            &mut local_bytes,
                            &mut local_frames,
                            &mut local_updated,
                            &state,
                            ip,
                            v.len(),
                        ) {
                            break 4413;
                        }
                        if socket.send(Message::Pong(v)).await.is_err() { break 1000; }
                    }
                    Message::Binary(bytes) => {
                        if bytes.len() > MAX_FRAME { break 4413; }
                        if !valid_binary(&bytes, role) { break 4422; }
                        if !meter_traffic(
                            &mut local_bytes,
                            &mut local_frames,
                            &mut local_updated,
                            &state,
                            ip,
                            bytes.len(),
                        ) {
                            break 4413;
                        }
                        let peer = { let inner = room.inner.lock().expect("room lock"); slot_ref(&inner, role.other()).map(clone_slot_sender) };
                        if let Some(peer) = peer { let _ = enqueue(&peer, Outbound::Binary(bytes)); }
                    }
                    Message::Text(_) => break 4422,
                    Message::Close(_) => break 1000,
                }
            }
        }
    };
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: close,
            reason: Utf8Bytes::from_static(generic_reason(close)),
        })))
        .await;
    state.release_socket(ip);
    let mut inner = room.inner.lock().expect("room lock");
    if slot_ref(&inner, role).is_some_and(|slot| slot.id == slot_id) {
        *slot_mut(&mut inner, role) = None;
    }
    drop(inner);
    info!(pairing = %room.correlation, role = role.name(), state_transition = "disconnected", close_code = close);
    let _ = id;
}

/// Local per-socket token bucket shared with the global per-IP `allow_traffic`
/// budget. Returns false when either the frame or byte allowance is exhausted.
fn meter_traffic(
    local_bytes: &mut f64,
    local_frames: &mut f64,
    local_updated: &mut Instant,
    state: &RelayState,
    ip: IpAddr,
    len: usize,
) -> bool {
    let now = Instant::now();
    let elapsed = now.duration_since(*local_updated).as_secs_f64();
    *local_bytes = TRAFFIC_BYTE_BURST.min(*local_bytes + elapsed * TRAFFIC_BYTES_PER_SECOND);
    *local_frames = TRAFFIC_FRAME_BURST.min(*local_frames + elapsed * TRAFFIC_FRAMES_PER_SECOND);
    *local_updated = now;
    if *local_bytes < len as f64 || *local_frames < 1.0 || !state.allow_traffic(ip, len) {
        return false;
    }
    *local_bytes -= len as f64;
    *local_frames -= 1.0;
    true
}

struct SlotSender {
    tx: mpsc::Sender<Outbound>,
    queued: Arc<AtomicUsize>,
    cancel: CancellationToken,
    close_code: Arc<AtomicU16>,
}
fn clone_slot_sender(slot: &Slot) -> SlotSender {
    SlotSender {
        tx: slot.tx.clone(),
        queued: slot.queued_bytes.clone(),
        cancel: slot.cancel.clone(),
        close_code: slot.close_code.clone(),
    }
}
fn enqueue(slot: &SlotSender, outbound: Outbound) -> bool {
    let len = outbound.len();
    let mut current = slot.queued.load(Ordering::Acquire);
    loop {
        if current.saturating_add(len) > MAX_QUEUE_BYTES {
            slot.close_code.store(4413, Ordering::Relaxed);
            slot.cancel.cancel();
            return false;
        }
        match slot.queued.compare_exchange_weak(
            current,
            current + len,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
    if slot.tx.try_send(outbound).is_err() {
        slot.queued.fetch_sub(len, Ordering::AcqRel);
        slot.close_code.store(4413, Ordering::Relaxed);
        slot.cancel.cancel();
        false
    } else {
        true
    }
}
fn slot_ref(inner: &RoomInner, role: Role) -> Option<&Slot> {
    match role {
        Role::Desktop => inner.desktop.as_ref(),
        Role::Mobile => inner.mobile.as_ref(),
    }
}
fn slot_mut(inner: &mut RoomInner, role: Role) -> &mut Option<Slot> {
    match role {
        Role::Desktop => &mut inner.desktop,
        Role::Mobile => &mut inner.mobile,
    }
}
fn cancel_slot(slot: Option<Slot>, code: u16) {
    if let Some(slot) = slot {
        slot.close_code.store(code, Ordering::Relaxed);
        slot.cancel.cancel();
    }
}

/// Releases the per-IP socket reservation and role slot committed in
/// `connect_socket` before the WebSocket upgrade. If the upgrade never completes
/// its callback (and thus `socket_task`) never runs, so this guard's `Drop` is
/// the only thing that frees them. Once `socket_task` takes ownership it disarms
/// the guard, and `socket_task`'s own cleanup performs the release exactly once.
struct UpgradeGuard {
    state: Arc<RelayState>,
    room: Arc<Room>,
    ip: IpAddr,
    role: Role,
    slot_id: u64,
    armed: bool,
}

impl UpgradeGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for UpgradeGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.state.release_socket(self.ip);
        // Clear the slot only if it is still the one we registered; a takeover
        // may have replaced it with a newer connection that must be preserved.
        let mut inner = self.room.inner.lock().expect("room lock");
        if slot_ref(&inner, self.role).is_some_and(|slot| slot.id == self.slot_id) {
            *slot_mut(&mut inner, self.role) = None;
        }
    }
}

fn hmac_digest(key: &[u8; 32], value: &[u8]) -> TokenDigest {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key");
    mac.update(value);
    mac.finalize().into_bytes().into()
}

fn valid_binary(frame: &[u8], role: Role) -> bool {
    if frame.len() > MAX_FRAME || frame.len() < 4 || &frame[..2] != b"RB" || frame[2] != PROTOCOL {
        return false;
    }
    match frame[3] {
        0 => {
            if frame.len() < 33 {
                return false;
            }
            let declared =
                u32::from_be_bytes(frame[12..16].try_into().expect("fixed slice")) as usize;
            declared > 16 && declared == frame.len() - 16
        }
        1 => {
            let expected_role = match role {
                Role::Desktop => 0,
                Role::Mobile => 1,
            };
            frame.len() == 24 && frame[4] == expected_role && frame[5..8] == [0, 0, 0]
        }
        _ => false,
    }
}
/// Decode canonical (round-tripping) unpadded base64url with a length bound.
fn decode_canonical(value: &str, minimum: usize, maximum: usize) -> Option<Vec<u8>> {
    let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
    if bytes.len() < minimum || bytes.len() > maximum || URL_SAFE_NO_PAD.encode(&bytes) != value {
        return None;
    }
    Some(bytes)
}
fn decode_id(id: &str) -> Option<[u8; 16]> {
    if id.len() != 22 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(id).ok()?;
    if URL_SAFE_NO_PAD.encode(&bytes) != id {
        return None;
    }
    bytes.try_into().ok()
}
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let token = value.strip_prefix("Bearer ")?;
    let decoded = URL_SAFE_NO_PAD.decode(token).ok()?;
    if token.len() == 43 && decoded.len() == 32 && URL_SAFE_NO_PAD.encode(decoded) == token {
        Some(token)
    } else {
        None
    }
}
/// Resolves the client IP used for per-source limits. Behind a trusted proxy
/// (`trust_forwarded_for`), the TCP peer is the proxy, so the real client is
/// taken from the rightmost `X-Forwarded-For` entry — the one the immediate
/// proxy appends. Falls back to the TCP peer when disabled, absent, or malformed.
fn client_ip(config: &Config, headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    if config.trust_forwarded_for {
        if let Some(forwarded) = headers
            .get(header::HeaderName::from_static("x-forwarded-for"))
            .and_then(|value| value.to_str().ok())
        {
            if let Some(entry) = forwarded.rsplit(',').next() {
                if let Ok(ip) = entry.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }
    }
    peer.ip()
}
fn has_subprotocol(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim() == SUBPROTOCOL)
}
fn generic_reason(code: u16) -> &'static str {
    match code {
        1000 => "normal",
        4408 => "expired",
        CLOSE_REVOKED => "revoked",
        4413 => "resource_limit",
        4422 => "protocol_error",
        CLOSE_SUPERSEDED => "superseded",
        _ => "normal",
    }
}
fn hex8(bytes: &[u8]) -> String {
    bytes[..8].iter().map(|b| format!("{b:02x}")).collect()
}
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
fn error(status: StatusCode) -> Response {
    #[derive(Serialize)]
    struct ErrorBody {
        error: &'static str,
    }
    no_store(
        (
            status,
            Json(ErrorBody {
                error: "request rejected",
            }),
        )
            .into_response(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use tempfile::TempDir;
    use tokio::{net::TcpListener, task::JoinHandle};
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{client::IntoClientRequest, Message as ClientMessage},
        MaybeTlsStream, WebSocketStream,
    };

    struct Server {
        base: String,
        state: Arc<RelayState>,
        task: JoinHandle<()>,
    }

    impl Server {
        async fn stop(mut self) {
            self.state.shutdown();
            self.task.abort();
            let _ = (&mut self.task).await;
            for _ in 0..100 {
                if Arc::strong_count(&self.state) == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(Arc::strong_count(&self.state), 1, "server state leaked");
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn server() -> Server {
        server_with_limits(100, 10).await
    }

    async fn server_with_limits(pending_cap: usize, sockets_per_ip: usize) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let state = Arc::new(RelayState::new(Config {
            bind: address,
            public_url: format!("http://{address}"),
            pending_cap,
            trusted_cap: 100,
            sockets_per_ip,
            trust_forwarded_for: false,
        }));
        state.clone().start_cleanup();
        serve_state(listener, state).await
    }

    async fn persistent_server(database_path: &StdPath, hmac_key: [u8; 32]) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let state = Arc::new(
            RelayState::open(
                Config {
                    bind: address,
                    public_url: format!("http://{address}"),
                    pending_cap: 100,
                    trusted_cap: 100,
                    sockets_per_ip: 10,
                    trust_forwarded_for: false,
                },
                database_path,
                hmac_key,
            )
            .expect("open persistent relay state"),
        );
        serve_state(listener, state).await
    }

    async fn serve_state(listener: TcpListener, state: Arc<RelayState>) -> Server {
        let address = listener.local_addr().expect("address");
        let serve_state = state.clone();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app(serve_state).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .expect("server");
        });
        Server {
            base: format!("http://{address}"),
            state,
            task,
        }
    }

    fn temporary_database_dir() -> TempDir {
        tempfile::Builder::new()
            .prefix("rebon-relay-test-")
            .tempdir()
            .expect("temp dir")
    }

    async fn allocate(server: &Server) -> Value {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/pairings", server.base))
            .json(&json!({"protocol": PROTOCOL, "expires_in_seconds": 120}))
            .send()
            .await
            .expect("allocate response");
        assert_eq!(response.status(), StatusCode::CREATED);
        response.json().await.expect("allocation JSON")
    }

    type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    async fn connect(
        server: &Server,
        pairing_id: &str,
        token: &str,
    ) -> Result<ClientSocket, tokio_tungstenite::tungstenite::Error> {
        let websocket_url = format!(
            "ws://{}/v1/pairings/{pairing_id}/socket",
            server.base.trim_start_matches("http://")
        );
        let mut request = websocket_url.into_client_request().expect("request");
        request.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).expect("authorization"),
        );
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(SUBPROTOCOL),
        );
        connect_async(request).await.map(|(socket, response)| {
            assert_eq!(
                response.headers().get(header::SEC_WEBSOCKET_PROTOCOL),
                Some(&HeaderValue::from_static(SUBPROTOCOL))
            );
            socket
        })
    }

    async fn connect_without_subprotocol(
        server: &Server,
        pairing_id: &str,
        token: &str,
    ) -> Result<ClientSocket, tokio_tungstenite::tungstenite::Error> {
        let websocket_url = format!(
            "ws://{}/v1/pairings/{pairing_id}/socket",
            server.base.trim_start_matches("http://")
        );
        let mut request = websocket_url.into_client_request().expect("request");
        request.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).expect("authorization"),
        );
        connect_async(request).await.map(|(socket, _)| socket)
    }

    fn http_status(error: tokio_tungstenite::tungstenite::Error) -> StatusCode {
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => response.status(),
            other => panic!("expected HTTP rejection, got {other}"),
        }
    }

    fn valid_frame(payload_len: usize) -> Vec<u8> {
        let mut frame = vec![0; 16 + payload_len];
        frame[..2].copy_from_slice(b"RB");
        frame[2] = PROTOCOL;
        frame[12..16].copy_from_slice(&(payload_len as u32).to_be_bytes());
        frame
    }

    #[tokio::test]
    async fn health_is_exact_and_stable() {
        let server = server().await;
        let client = reqwest::Client::new();
        let before = client
            .get(format!("{}/healthz", server.base))
            .send()
            .await
            .expect("health");
        assert_eq!(before.status(), StatusCode::OK);
        assert_eq!(before.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            before.text().await.expect("body"),
            r#"{"status":"ok","protocol":2}"#
        );
        let _ = allocate(&server).await;
        let after = client
            .get(format!("{}/healthz", server.base))
            .send()
            .await
            .expect("health")
            .text()
            .await
            .expect("body");
        assert_eq!(after, r#"{"status":"ok","protocol":2}"#);
    }

    fn encode32(bytes: [u8; 32]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    async fn store_handoff_request(
        server: &Server,
        pairing_id: &str,
        token: &str,
        claim: &str,
        box_b64: &str,
    ) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/v1/pairings/{pairing_id}/handoff", server.base))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .json(&json!({"protocol": PROTOCOL, "claim": claim, "box": box_b64}))
            .send()
            .await
            .expect("handoff response")
    }

    async fn claim_handoff_request(server: &Server, claim: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/v1/handoffs/claim", server.base))
            .json(&json!({"protocol": PROTOCOL, "claim": claim}))
            .send()
            .await
            .expect("claim response")
    }

    #[tokio::test]
    async fn handoff_requires_desktop_token_on_a_pending_room() {
        let server = server().await;
        let allocation = allocate(&server).await;
        let pairing_id = allocation["pairing_id"].as_str().expect("pairing id");
        let desktop = allocation["desktop_token"].as_str().expect("desktop token");
        let mobile = allocation["mobile_token"].as_str().expect("mobile token");
        let claim = encode32([7; 32]);
        let box_b64 = URL_SAFE_NO_PAD.encode(vec![9u8; 64]);

        // Schema violations are rejected before authentication.
        let short_claim = URL_SAFE_NO_PAD.encode([1u8; 16]);
        let response = store_handoff_request(&server, pairing_id, desktop, &short_claim, &box_b64)
            .await
            .status();
        assert_eq!(response, StatusCode::BAD_REQUEST);
        let oversize_box = URL_SAFE_NO_PAD.encode(vec![1u8; MAX_HANDOFF_BOX_BYTES + 1]);
        let response = store_handoff_request(&server, pairing_id, desktop, &claim, &oversize_box)
            .await
            .status();
        assert_eq!(response, StatusCode::BAD_REQUEST);
        let undersize_box = URL_SAFE_NO_PAD.encode(vec![1u8; MIN_HANDOFF_BOX_BYTES - 1]);
        let response = store_handoff_request(&server, pairing_id, desktop, &claim, &undersize_box)
            .await
            .status();
        assert_eq!(response, StatusCode::BAD_REQUEST);

        // Only the desktop role may publish a handoff.
        let response = store_handoff_request(&server, pairing_id, mobile, &claim, &box_b64)
            .await
            .status();
        assert_eq!(response, StatusCode::UNAUTHORIZED);

        // Unknown room.
        let ghost = URL_SAFE_NO_PAD.encode([3u8; 16]);
        let response = store_handoff_request(&server, &ghost, desktop, &claim, &box_b64)
            .await
            .status();
        assert_eq!(response, StatusCode::UNAUTHORIZED);

        let response = store_handoff_request(&server, pairing_id, desktop, &claim, &box_b64)
            .await
            .status();
        assert_eq!(response, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn handoff_claim_is_single_use_and_rate_limited() {
        let server = server().await;
        let allocation = allocate(&server).await; // burns rate token 1 of 3
        let pairing_id = allocation["pairing_id"].as_str().expect("pairing id");
        let desktop = allocation["desktop_token"].as_str().expect("desktop token");
        let claim = encode32([11; 32]);
        let box_b64 = URL_SAFE_NO_PAD.encode(vec![5u8; 96]);
        let response = store_handoff_request(&server, pairing_id, desktop, &claim, &box_b64)
            .await
            .status();
        assert_eq!(response, StatusCode::NO_CONTENT);

        // Valid claim returns the box verbatim, exactly once. (token 2 of 3)
        let response = claim_handoff_request(&server, &claim).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.expect("claim JSON");
        assert_eq!(body["protocol"], PROTOCOL);
        assert_eq!(body["box"].as_str(), Some(box_b64.as_str()));

        // Replay is gone. (token 3 of 3)
        let response = claim_handoff_request(&server, &claim).await.status();
        assert_eq!(response, StatusCode::NOT_FOUND);

        // Budget exhausted: brute-force attempts hit the shared creation limiter.
        let response = claim_handoff_request(&server, &encode32([12; 32]))
            .await
            .status();
        assert_eq!(response, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn handoff_replacement_invalidates_the_previous_claim() {
        let server = server().await;
        let allocation = allocate(&server).await;
        let pairing_id = allocation["pairing_id"].as_str().expect("pairing id");
        let desktop = allocation["desktop_token"].as_str().expect("desktop token");
        let first_claim = encode32([21; 32]);
        let second_claim = encode32([22; 32]);
        let first_box = URL_SAFE_NO_PAD.encode(vec![1u8; 48]);
        let second_box = URL_SAFE_NO_PAD.encode(vec![2u8; 48]);
        for (claim, box_b64) in [(&first_claim, &first_box), (&second_claim, &second_box)] {
            let response = store_handoff_request(&server, pairing_id, desktop, claim, box_b64)
                .await
                .status();
            assert_eq!(response, StatusCode::NO_CONTENT);
        }
        let response = claim_handoff_request(&server, &first_claim).await.status();
        assert_eq!(response, StatusCode::NOT_FOUND);
        let response = claim_handoff_request(&server, &second_claim).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.expect("claim JSON");
        assert_eq!(body["box"].as_str(), Some(second_box.as_str()));
    }

    #[tokio::test]
    async fn handoff_dies_with_trust() {
        let server = server().await;

        // Trusted room: storing is 409 and any staged claim is purged.
        let allocation = allocate(&server).await;
        let pairing_id = allocation["pairing_id"].as_str().expect("pairing id");
        let desktop = allocation["desktop_token"].as_str().expect("desktop token");
        let mobile = allocation["mobile_token"].as_str().expect("mobile token");
        let claim = encode32([31; 32]);
        let box_b64 = URL_SAFE_NO_PAD.encode(vec![6u8; 48]);
        let response = store_handoff_request(&server, pairing_id, desktop, &claim, &box_b64)
            .await
            .status();
        assert_eq!(response, StatusCode::NO_CONTENT);
        let desktop_socket = connect(&server, pairing_id, desktop)
            .await
            .expect("desktop");
        let mobile_socket = connect(&server, pairing_id, mobile).await.expect("mobile");
        let response = claim_handoff_request(&server, &claim).await.status();
        assert_eq!(response, StatusCode::NOT_FOUND);
        let response = store_handoff_request(&server, pairing_id, desktop, &claim, &box_b64)
            .await
            .status();
        assert_eq!(response, StatusCode::CONFLICT);
        drop(desktop_socket);
        drop(mobile_socket);
        assert!(server
            .state
            .handoff_claims
            .lock()
            .expect("handoff claims lock")
            .is_empty());
    }

    #[tokio::test]
    async fn handoff_dies_with_cancellation() {
        let server = server().await;
        let box_b64 = URL_SAFE_NO_PAD.encode(vec![6u8; 48]);

        // Cancelled room: the claim dies with the room.
        let allocation = allocate(&server).await;
        let pairing_id = allocation["pairing_id"].as_str().expect("pairing id");
        let desktop = allocation["desktop_token"].as_str().expect("desktop token");
        let claim = encode32([32; 32]);
        let response = store_handoff_request(&server, pairing_id, desktop, &claim, &box_b64)
            .await
            .status();
        assert_eq!(response, StatusCode::NO_CONTENT);
        let response = reqwest::Client::new()
            .delete(format!("{}/v1/pairings/{pairing_id}", server.base))
            .header(header::AUTHORIZATION, format!("Bearer {desktop}"))
            .send()
            .await
            .expect("cancel response")
            .status();
        assert_eq!(response, StatusCode::NO_CONTENT);
        let response = claim_handoff_request(&server, &claim).await.status();
        assert_eq!(response, StatusCode::NOT_FOUND);
        assert!(server
            .state
            .handoff_claims
            .lock()
            .expect("handoff claims lock")
            .is_empty());
    }

    #[tokio::test]
    async fn creation_schema_validation_limits_rate_and_capacity() {
        let server = server().await;
        let client = reqwest::Client::new();
        let pairing = allocate(&server).await;
        let object = pairing.as_object().expect("allocation object");
        assert_eq!(object.len(), 5);
        assert_eq!(pairing["protocol"], 2);
        let id = pairing["pairing_id"].as_str().expect("pairing id");
        let desktop = pairing["desktop_token"].as_str().expect("desktop token");
        let mobile = pairing["mobile_token"].as_str().expect("mobile token");
        assert!(decode_id(id).is_some());
        assert!(bearer_value_is_canonical(desktop));
        assert!(bearer_value_is_canonical(mobile));
        assert_ne!(desktop, mobile);
        assert!(pairing["expires_at"]
            .as_str()
            .is_some_and(|value| value.ends_with('Z')));

        for body in [
            r#"{"protocol":2,"expires_in_seconds":120,"unknown":true}"#,
            r#"{"protocol":2,"protocol":2,"expires_in_seconds":120}"#,
            r#"{"protocol":1,"expires_in_seconds":120}"#,
            r#"{"protocol":2,"expires_in_seconds":29}"#,
            r#"{"protocol":2,"expires_in_seconds":301}"#,
            r#"{"protocol":2,"expires_in_seconds":30.5}"#,
            r#"{"protocol":2,"expires_in_seconds":120} trailing"#,
            r#"{"protocol":2,"expires_in_seconds":120"#,
        ] {
            let response = client
                .post(format!("{}/v1/pairings", server.base))
                .header(header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .expect("invalid create response");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "body: {body}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
        let oversized = client
            .post(format!("{}/v1/pairings", server.base))
            .header(header::CONTENT_TYPE, "application/json")
            .body(" ".repeat(1_025))
            .send()
            .await
            .expect("oversized response");
        assert_eq!(oversized.status(), StatusCode::BAD_REQUEST);
        assert_eq!(oversized.headers()[header::CACHE_CONTROL], "no-store");

        let rate_server = server_with_limits(100, 10).await;
        for _ in 0..3 {
            assert_eq!(
                client
                    .post(format!("{}/v1/pairings", rate_server.base))
                    .json(&json!({"protocol": PROTOCOL, "expires_in_seconds": 120}))
                    .send()
                    .await
                    .expect("rate create")
                    .status(),
                StatusCode::CREATED
            );
        }
        assert_eq!(
            client
                .post(format!("{}/v1/pairings", rate_server.base))
                .json(&json!({"protocol": PROTOCOL, "expires_in_seconds": 120}))
                .send()
                .await
                .expect("rate rejection")
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );

        let capacity_server = server_with_limits(1, 10).await;
        let _ = allocate(&capacity_server).await;
        assert_eq!(
            client
                .post(format!("{}/v1/pairings", capacity_server.base))
                .json(&json!({"protocol": PROTOCOL, "expires_in_seconds": 120}))
                .send()
                .await
                .expect("capacity rejection")
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    fn bearer_value_is_canonical(value: &str) -> bool {
        value.len() == 43
            && URL_SAFE_NO_PAD
                .decode(value)
                .ok()
                .is_some_and(|bytes| bytes.len() == 32 && URL_SAFE_NO_PAD.encode(bytes) == value)
    }

    #[tokio::test]
    async fn relays_binary_byte_for_byte_between_two_roles() {
        let server = server().await;
        let pairing = allocate(&server).await;
        let id = pairing["pairing_id"].as_str().expect("id");
        let mut desktop = connect(
            &server,
            id,
            pairing["desktop_token"].as_str().expect("desktop"),
        )
        .await
        .expect("desktop socket");
        let mut mobile = connect(
            &server,
            id,
            pairing["mobile_token"].as_str().expect("mobile"),
        )
        .await
        .expect("mobile socket");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), desktop.next())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), mobile.next())
                .await
                .is_err()
        );

        let frame = valid_frame(17);
        desktop
            .send(ClientMessage::Binary(frame.clone().into()))
            .await
            .expect("send");
        let received = tokio::time::timeout(Duration::from_secs(2), mobile.next())
            .await
            .expect("relay timeout")
            .expect("message")
            .expect("websocket message");
        assert_eq!(received.into_data(), frame);

        let reverse = valid_frame(23);
        mobile
            .send(ClientMessage::Binary(reverse.clone().into()))
            .await
            .expect("reverse send");
        let received = tokio::time::timeout(Duration::from_secs(2), desktop.next())
            .await
            .expect("reverse relay timeout")
            .expect("message")
            .expect("websocket message");
        assert_eq!(received.into_data(), reverse);
    }

    #[tokio::test]
    async fn unauthorized_rejected_and_duplicate_role_supersedes() {
        let server = server().await;
        let pairing = allocate(&server).await;
        let id = pairing["pairing_id"].as_str().expect("id");
        let wrong = URL_SAFE_NO_PAD.encode([7_u8; 32]);
        let unauthorized = connect(&server, id, &wrong)
            .await
            .expect_err("must reject token");
        assert!(
            matches!(unauthorized, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == StatusCode::UNAUTHORIZED)
        );

        let token = pairing["desktop_token"].as_str().expect("desktop");
        let mut first = connect(&server, id, token).await.expect("first desktop");
        // A second valid connection for the same role takes over the slot.
        let mut second = connect(&server, id, token).await.expect("takeover desktop");
        // The superseded connection is closed with CLOSE_SUPERSEDED (4426).
        let close = tokio::time::timeout(Duration::from_secs(2), first.next())
            .await
            .expect("supersede close timeout")
            .expect("supersede close")
            .expect("supersede close frame");
        match close {
            ClientMessage::Close(Some(frame)) => {
                assert_eq!(u16::from(frame.code), CLOSE_SUPERSEDED);
                assert_eq!(frame.reason, "superseded");
            }
            other => panic!("expected supersede close, got {other:?}"),
        }
        // The replacement connection remains usable and owns the only slot.
        second
            .send(ClientMessage::Ping(Vec::new().into()))
            .await
            .expect("new connection works");
        let desktop_present = server
            .state
            .rooms
            .lock()
            .expect("rooms")
            .get(id)
            .map(|room| room.inner.lock().expect("room").desktop.is_some());
        assert_eq!(desktop_present, Some(true));
        // The superseded socket released its per-IP reservation, leaving one.
        for _ in 0..50 {
            let count = server
                .state
                .sockets
                .lock()
                .expect("sockets")
                .values()
                .sum::<usize>();
            if count == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            server
                .state
                .sockets
                .lock()
                .expect("sockets")
                .values()
                .sum::<usize>(),
            1
        );
    }

    #[tokio::test]
    async fn upgrade_guard_releases_reservation_and_slot_on_drop() {
        let server = server().await;
        let pairing = allocate(&server).await;
        let id = pairing["pairing_id"].as_str().expect("id").to_owned();
        let room = server
            .state
            .rooms
            .lock()
            .expect("rooms")
            .get(&id)
            .cloned()
            .expect("room");
        let ip: IpAddr = "203.0.113.7".parse().expect("test IP");

        // Reproduce connect_socket's pre-upgrade commit: reserve + install a slot.
        let install_slot = |slot_id: u64| {
            let (tx, rx) = mpsc::channel(8);
            let mut inner = room.inner.lock().expect("room");
            inner.desktop = Some(Slot {
                id: slot_id,
                tx,
                queued_bytes: Arc::new(AtomicUsize::new(0)),
                cancel: CancellationToken::new(),
                close_code: Arc::new(AtomicU16::new(4408)),
            });
            rx
        };

        assert!(server.state.reserve_socket(ip));
        let _rx = install_slot(0xABCD);
        assert_eq!(
            *server
                .state
                .sockets
                .lock()
                .expect("sockets")
                .get(&ip)
                .expect("reserved"),
            1
        );

        // Dropping an armed guard releases both the reservation and the slot.
        let guard = UpgradeGuard {
            state: server.state.clone(),
            room: room.clone(),
            ip,
            role: Role::Desktop,
            slot_id: 0xABCD,
            armed: true,
        };
        drop(guard);
        assert!(server
            .state
            .sockets
            .lock()
            .expect("sockets")
            .get(&ip)
            .is_none());
        assert!(room.inner.lock().expect("room").desktop.is_none());

        // A disarmed guard releases nothing (socket_task owns cleanup instead).
        assert!(server.state.reserve_socket(ip));
        let _rx2 = install_slot(0x1234);
        let mut disarmed = UpgradeGuard {
            state: server.state.clone(),
            room: room.clone(),
            ip,
            role: Role::Desktop,
            slot_id: 0x1234,
            armed: true,
        };
        disarmed.disarm();
        drop(disarmed);
        assert_eq!(
            *server
                .state
                .sockets
                .lock()
                .expect("sockets")
                .get(&ip)
                .expect("still reserved"),
            1
        );
        assert!(room.inner.lock().expect("room").desktop.is_some());

        // Leave the reservation/slot released so the background reaper stays quiet.
        server.state.release_socket(ip);
        room.inner.lock().expect("room").desktop = None;
    }

    #[tokio::test]
    async fn pending_counter_tracks_lifecycle_transitions() {
        let server = server().await;
        assert_eq!(server.state.pending_count.load(Ordering::Relaxed), 0);

        let a = allocate(&server).await;
        assert_eq!(server.state.pending_count.load(Ordering::Relaxed), 1);
        let b = allocate(&server).await;
        assert_eq!(server.state.pending_count.load(Ordering::Relaxed), 2);

        // Both roles connecting promotes `a` to trusted, dropping the counter.
        let id_a = a["pairing_id"].as_str().expect("id");
        let _desktop = connect(&server, id_a, a["desktop_token"].as_str().expect("desktop"))
            .await
            .expect("desktop");
        let _mobile = connect(&server, id_a, a["mobile_token"].as_str().expect("mobile"))
            .await
            .expect("mobile");
        assert_eq!(server.state.pending_count.load(Ordering::Relaxed), 1);

        // Cancelling the still-pending `b` drops the counter to zero.
        let id_b = b["pairing_id"].as_str().expect("id");
        let cancelled = reqwest::Client::new()
            .delete(format!("{}/v1/pairings/{id_b}", server.base))
            .bearer_auth(b["desktop_token"].as_str().expect("desktop"))
            .send()
            .await
            .expect("cancel pending");
        assert_eq!(cancelled.status(), StatusCode::NO_CONTENT);
        assert_eq!(server.state.pending_count.load(Ordering::Relaxed), 0);

        // Expiring a fresh pending room drops the counter through the reaper path.
        let c = allocate(&server).await;
        assert_eq!(server.state.pending_count.load(Ordering::Relaxed), 1);
        let id_c = c["pairing_id"].as_str().expect("id").to_owned();
        server
            .state
            .rooms
            .lock()
            .expect("rooms")
            .get(&id_c)
            .expect("room c")
            .inner
            .lock()
            .expect("room")
            .expires_at = Instant::now();
        server.state.cleanup();
        assert_eq!(server.state.pending_count.load(Ordering::Relaxed), 0);
        assert!(!server
            .state
            .rooms
            .lock()
            .expect("rooms")
            .contains_key(&id_c));
    }

    #[tokio::test]
    async fn websocket_preupgrade_rejections_are_generic_and_no_store() {
        let server = server().await;
        let pairing = allocate(&server).await;
        let id = pairing["pairing_id"].as_str().expect("id");
        let desktop = pairing["desktop_token"].as_str().expect("desktop");

        let missing_protocol = connect_without_subprotocol(&server, id, desktop)
            .await
            .expect_err("missing subprotocol");
        match missing_protocol {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                assert_eq!(
                    response.headers()[header::SEC_WEBSOCKET_PROTOCOL],
                    SUBPROTOCOL
                );
            }
            other => panic!("expected HTTP rejection, got {other}"),
        }

        let socket_base = format!(
            "ws://{}/v1/pairings/{id}/socket",
            server.base.trim_start_matches("http://")
        );
        for (url, authorization, expected) in [
            (socket_base.clone(), None, StatusCode::UNAUTHORIZED),
            (
                socket_base.clone(),
                Some("Bearer malformed".to_owned()),
                StatusCode::UNAUTHORIZED,
            ),
            (
                format!("{socket_base}?token=forbidden"),
                Some(format!("Bearer {desktop}")),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let mut request = url.into_client_request().expect("request");
            request.headers_mut().insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_static(SUBPROTOCOL),
            );
            if let Some(value) = authorization {
                request.headers_mut().insert(
                    header::AUTHORIZATION,
                    HeaderValue::from_str(&value).expect("authorization"),
                );
            }
            let error = connect_async(request).await.expect_err("must reject");
            match error {
                tokio_tungstenite::tungstenite::Error::Http(response) => {
                    assert_eq!(response.status(), expected);
                    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                    if expected == StatusCode::UNAUTHORIZED {
                        assert_eq!(
                            response.body().as_ref().map(Vec::as_slice),
                            Some(br#"{"error":"request rejected"}"#.as_slice())
                        );
                    }
                }
                other => panic!("expected HTTP rejection, got {other}"),
            }
        }

        let unknown = URL_SAFE_NO_PAD.encode([9_u8; 16]);
        assert_eq!(
            http_status(
                connect(&server, &unknown, desktop)
                    .await
                    .expect_err("unknown room")
            ),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn oversized_frame_closes_connection() {
        let server = server().await;
        let pairing = allocate(&server).await;
        let id = pairing["pairing_id"].as_str().expect("id");
        let mut desktop = connect(
            &server,
            id,
            pairing["desktop_token"].as_str().expect("desktop"),
        )
        .await
        .expect("desktop socket");
        desktop
            .send(ClientMessage::Binary(vec![0_u8; MAX_FRAME + 1].into()))
            .await
            .expect("send oversized");
        let result = tokio::time::timeout(Duration::from_secs(2), desktop.next())
            .await
            .expect("close timeout")
            .expect("close message")
            .expect("close frame");
        match result {
            ClientMessage::Close(Some(frame)) => {
                assert_eq!(u16::from(frame.code), 4413);
                assert_eq!(frame.reason, "resource_limit");
            }
            other => panic!("expected resource-limit close, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn absent_peer_frames_drop_and_protocol_violations_close() {
        let server = server().await;
        let pairing = allocate(&server).await;
        let id = pairing["pairing_id"].as_str().expect("id");
        let mut desktop = connect(
            &server,
            id,
            pairing["desktop_token"].as_str().expect("desktop"),
        )
        .await
        .expect("desktop socket");
        desktop
            .send(ClientMessage::Binary(valid_frame(17).into()))
            .await
            .expect("absent-peer send");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut mobile = connect(
            &server,
            id,
            pairing["mobile_token"].as_str().expect("mobile"),
        )
        .await
        .expect("mobile socket");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), mobile.next())
                .await
                .is_err(),
            "frame sent while absent must not be delivered"
        );

        desktop
            .send(ClientMessage::Text("forbidden".into()))
            .await
            .expect("text send");
        let close = desktop
            .next()
            .await
            .expect("text close")
            .expect("text close frame");
        match close {
            ClientMessage::Close(Some(frame)) => {
                assert_eq!(u16::from(frame.code), 4422);
                assert_eq!(frame.reason, "protocol_error");
            }
            other => panic!("expected protocol close, got {other:?}"),
        }

        mobile
            .send(ClientMessage::Binary(vec![0_u8; 33].into()))
            .await
            .expect("malformed send");
        let close = mobile
            .next()
            .await
            .expect("malformed close")
            .expect("malformed close frame");
        match close {
            ClientMessage::Close(Some(frame)) => assert_eq!(u16::from(frame.code), 4422),
            other => panic!("expected malformed-frame close, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn expired_pairing_cannot_upgrade() {
        let server = server().await;
        let pairing = allocate(&server).await;
        let id = pairing["pairing_id"].as_str().expect("id");
        let room = server
            .state
            .rooms
            .lock()
            .expect("rooms")
            .get(id)
            .cloned()
            .expect("room");
        room.inner.lock().expect("room").expires_at = Instant::now();
        let result = connect(
            &server,
            id,
            pairing["desktop_token"].as_str().expect("desktop"),
        )
        .await
        .expect_err("expired pairing");
        assert!(
            matches!(result, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == StatusCode::UNAUTHORIZED)
        );
    }

    #[tokio::test]
    async fn trusted_pairing_survives_restart_and_revoke_does_not() {
        let database_dir = temporary_database_dir();
        let database_path = database_dir.path().join("relay.sqlite3");
        let hmac_key = [0x5a; 32];

        let server = persistent_server(&database_path, hmac_key).await;
        let pending = allocate(&server).await;
        server.stop().await;

        let server = persistent_server(&database_path, hmac_key).await;
        assert_eq!(
            http_status(
                connect(
                    &server,
                    pending["pairing_id"].as_str().expect("pending id"),
                    pending["desktop_token"]
                        .as_str()
                        .expect("pending desktop token"),
                )
                .await
                .expect_err("pending pairing must not survive restart"),
            ),
            StatusCode::UNAUTHORIZED
        );
        server.stop().await;

        let server = persistent_server(&database_path, hmac_key).await;
        let pairing = allocate(&server).await;
        let id = pairing["pairing_id"].as_str().expect("id");
        let desktop = connect(
            &server,
            id,
            pairing["desktop_token"].as_str().expect("desktop"),
        )
        .await
        .expect("desktop bootstrap");
        let mobile = connect(
            &server,
            id,
            pairing["mobile_token"].as_str().expect("mobile"),
        )
        .await
        .expect("mobile bootstrap");
        assert!(matches!(
            server
                .state
                .rooms
                .lock()
                .expect("rooms")
                .get(id)
                .expect("trusted room")
                .inner
                .lock()
                .expect("room")
                .lifecycle,
            Lifecycle::Trusted
        ));
        drop(desktop);
        drop(mobile);
        let trusted = pairing;
        server.stop().await;

        let stored_desktop: Vec<u8> = Connection::open(&database_path)
            .expect("inspect pairing database")
            .query_row(
                "SELECT desktop_token_hmac FROM trusted_pairings WHERE pairing_id = ?1",
                [trusted["pairing_id"].as_str().expect("id")],
                |row| row.get(0),
            )
            .expect("stored desktop HMAC");
        let mut desktop_token_bytes = URL_SAFE_NO_PAD
            .decode(trusted["desktop_token"].as_str().expect("desktop"))
            .expect("decode desktop token");
        assert_ne!(stored_desktop, desktop_token_bytes);
        assert_eq!(stored_desktop, hmac_digest(&hmac_key, &desktop_token_bytes));
        desktop_token_bytes.fill(0);

        {
            let server = persistent_server(&database_path, hmac_key).await;
            let id = trusted["pairing_id"].as_str().expect("id");
            let desktop_token = trusted["desktop_token"].as_str().expect("desktop");
            let mobile_token = trusted["mobile_token"].as_str().expect("mobile");
            let mut desktop = connect(&server, id, desktop_token)
                .await
                .expect("restored desktop");
            let mut mobile = connect(&server, id, mobile_token)
                .await
                .expect("restored mobile");
            let frame = valid_frame(19);
            desktop
                .send(ClientMessage::Binary(frame.clone().into()))
                .await
                .expect("send after restart");
            assert_eq!(
                mobile
                    .next()
                    .await
                    .expect("restored relay message")
                    .expect("restored relay frame")
                    .into_data(),
                frame
            );

            let revoked = reqwest::Client::new()
                .delete(format!("{}/v1/pairings/{id}", server.base))
                .bearer_auth(desktop_token)
                .send()
                .await
                .expect("revoke restored pairing");
            assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
            server.stop().await;
        }

        {
            let server = persistent_server(&database_path, hmac_key).await;
            assert_eq!(
                http_status(
                    connect(
                        &server,
                        trusted["pairing_id"].as_str().expect("id"),
                        trusted["desktop_token"].as_str().expect("desktop"),
                    )
                    .await
                    .expect_err("revoked pairing must not survive restart"),
                ),
                StatusCode::UNAUTHORIZED
            );
            server.stop().await;
        }
    }

    #[tokio::test]
    async fn trusted_role_token_rotation_is_atomic_and_backward_compatible() {
        let server = server().await;
        let client = reqwest::Client::new();
        let pending = allocate(&server).await;
        let pending_rotation = client
            .post(format!(
                "{}/v1/pairings/{}/tokens/rotate",
                server.base,
                pending["pairing_id"].as_str().expect("pending id")
            ))
            .bearer_auth(pending["desktop_token"].as_str().expect("pending desktop"))
            .send()
            .await
            .expect("pending rotation response");
        assert_eq!(pending_rotation.status(), StatusCode::CONFLICT);

        let pairing = allocate(&server).await;
        let id = pairing["pairing_id"].as_str().expect("id");
        let old_desktop = pairing["desktop_token"].as_str().expect("desktop");
        let old_mobile = pairing["mobile_token"].as_str().expect("mobile");
        let desktop = connect(&server, id, old_desktop)
            .await
            .expect("desktop bootstrap");
        let mobile = connect(&server, id, old_mobile)
            .await
            .expect("mobile bootstrap");

        let rotated = client
            .post(format!("{}/v1/pairings/{id}/tokens/rotate", server.base))
            .bearer_auth(old_desktop)
            .send()
            .await
            .expect("rotation response");
        assert_eq!(rotated.status(), StatusCode::OK);
        assert_eq!(rotated.headers()[header::CACHE_CONTROL], "no-store");
        let rotated: Value = rotated.json().await.expect("rotation JSON");
        assert_eq!(rotated["protocol"], 2);
        assert_eq!(rotated["role"], "desktop");
        let new_desktop = rotated["token"].as_str().expect("new desktop token");
        assert!(bearer_value_is_canonical(new_desktop));
        assert_ne!(new_desktop, old_desktop);

        drop(desktop);
        drop(mobile);
        for _ in 0..50 {
            let roles_absent = server
                .state
                .rooms
                .lock()
                .expect("rooms")
                .get(id)
                .is_some_and(|room| {
                    let inner = room.inner.lock().expect("room");
                    inner.desktop.is_none() && inner.mobile.is_none()
                });
            if roles_absent {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            http_status(
                connect(&server, id, old_desktop)
                    .await
                    .expect_err("old token must be invalid"),
            ),
            StatusCode::UNAUTHORIZED
        );

        let rotated_mobile = client
            .post(format!("{}/v1/pairings/{id}/tokens/rotate", server.base))
            .bearer_auth(old_mobile)
            .send()
            .await
            .expect("mobile rotation response");
        assert_eq!(rotated_mobile.status(), StatusCode::OK);
        let rotated_mobile: Value = rotated_mobile.json().await.expect("mobile rotation JSON");
        assert_eq!(rotated_mobile["role"], "mobile");
        let new_mobile = rotated_mobile["token"].as_str().expect("new mobile token");
        assert!(bearer_value_is_canonical(new_mobile));
        assert_eq!(
            http_status(
                connect(&server, id, old_mobile)
                    .await
                    .expect_err("old mobile token must be invalid"),
            ),
            StatusCode::UNAUTHORIZED
        );
        let replacement_mobile = connect(&server, id, new_mobile)
            .await
            .expect("replacement mobile token connects");
        let replacement = connect(&server, id, new_desktop)
            .await
            .expect("replacement token connects");
        drop(replacement_mobile);
        drop(replacement);

        assert_eq!(
            client
                .delete(format!("{}/v1/pairings/{id}", server.base))
                .bearer_auth(old_desktop)
                .send()
                .await
                .expect("old token revoke rejection")
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .delete(format!("{}/v1/pairings/{id}", server.base))
                .bearer_auth(new_desktop)
                .send()
                .await
                .expect("new token revoke")
                .status(),
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn cancellation_accepts_either_role_is_idempotent_and_closes_peers() {
        let server = server().await;
        let pairing = allocate(&server).await;
        let id = pairing["pairing_id"].as_str().expect("id");
        let desktop_token = pairing["desktop_token"].as_str().expect("desktop");
        let mobile_token = pairing["mobile_token"].as_str().expect("mobile");
        let mut desktop = connect(&server, id, desktop_token)
            .await
            .expect("desktop socket");
        let mut mobile = connect(&server, id, mobile_token)
            .await
            .expect("mobile socket");
        let client = reqwest::Client::new();
        let endpoint = format!("{}/v1/pairings/{id}", server.base);

        for _ in 0..2 {
            let cancelled = client
                .delete(&endpoint)
                .bearer_auth(mobile_token)
                .send()
                .await
                .expect("mobile cancellation");
            assert_eq!(cancelled.status(), StatusCode::NO_CONTENT);
            assert_eq!(cancelled.headers()[header::CACHE_CONTROL], "no-store");
        }
        // An authoritative cancel closes both peers with 4410 "revoked" (not the
        // 4408 "expired" used for liveness/pending-expiry timeouts).
        for socket in [&mut desktop, &mut mobile] {
            let close = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("cancellation close timeout")
                .expect("cancellation close")
                .expect("cancellation close frame");
            match close {
                ClientMessage::Close(Some(frame)) => {
                    assert_eq!(u16::from(frame.code), CLOSE_REVOKED);
                    assert_eq!(frame.reason, "revoked");
                }
                other => panic!("expected cancellation close, got {other:?}"),
            }
        }
        assert!(!server.state.rooms.lock().expect("rooms").contains_key(id));
    }

    #[tokio::test]
    async fn cleanup_prunes_expiry_rate_entries_and_shutdown_state() {
        let server = server().await;
        let pairing = allocate(&server).await;
        let id = pairing["pairing_id"].as_str().expect("id").to_owned();
        let room = server
            .state
            .rooms
            .lock()
            .expect("rooms")
            .get(&id)
            .cloned()
            .expect("room");
        room.inner.lock().expect("room").expires_at = Instant::now();
        let stale_ip: IpAddr = "192.0.2.1".parse().expect("test IP");
        server.state.rate.lock().expect("rate").insert(
            stale_ip,
            RateBucket {
                tokens: 0.0,
                updated: Instant::now() - Duration::from_secs(61),
            },
        );
        server.state.cleanup();
        assert!(!server.state.rooms.lock().expect("rooms").contains_key(&id));
        assert!(!server
            .state
            .rate
            .lock()
            .expect("rate")
            .contains_key(&stale_ip));

        let pairing = allocate(&server).await;
        assert!(server
            .state
            .rooms
            .lock()
            .expect("rooms")
            .contains_key(pairing["pairing_id"].as_str().expect("id")));
        server.state.shutdown();
        assert!(server.state.rooms.lock().expect("rooms").is_empty());
        assert!(server
            .state
            .tombstones
            .lock()
            .expect("tombstones")
            .is_empty());
        assert!(server.state.rate.lock().expect("rate").is_empty());
    }

    #[test]
    fn persistent_store_is_single_owner_and_enforces_trusted_capacity() {
        let database_dir = temporary_database_dir();
        let database_path = database_dir.path().join("relay.sqlite3");
        let store = PairingStore::open(&database_path).expect("first store owner");
        assert!(
            PairingStore::open(&database_path).is_err(),
            "a second relay must not share an authorization database"
        );

        let first_id = URL_SAFE_NO_PAD.encode([1u8; 16]);
        let second_id = URL_SAFE_NO_PAD.encode([2u8; 16]);
        assert!(store
            .trust(&first_id, &[3; 32], &[4; 32], 1)
            .expect("trust first"));
        assert!(!store
            .trust(&second_id, &[5; 32], &[6; 32], 1)
            .expect("reject over capacity"));
        assert_eq!(store.load_trusted().expect("load trusted").len(), 1);

        drop(store);
        PairingStore::open(&database_path).expect("lock released after owner drop");
    }

    #[test]
    fn framing_boundaries_and_protocol_errors() {
        assert!(valid_binary(&valid_frame(17), Role::Desktop));
        assert!(valid_binary(&valid_frame(MAX_FRAME - 16), Role::Mobile));
        assert!(!valid_binary(&valid_frame(16), Role::Desktop));
        assert!(!valid_binary(&valid_frame(MAX_FRAME - 15), Role::Desktop));
        let mut unknown_kind = valid_frame(17);
        unknown_kind[3] = 2;
        assert!(!valid_binary(&unknown_kind, Role::Desktop));
        let mut bad_length = valid_frame(17);
        bad_length[15] = 18;
        assert!(!valid_binary(&bad_length, Role::Desktop));

        let mut desktop_init = vec![0u8; 24];
        desktop_init[..2].copy_from_slice(b"RB");
        desktop_init[2] = PROTOCOL;
        desktop_init[3] = 1;
        assert!(valid_binary(&desktop_init, Role::Desktop));
        assert!(!valid_binary(&desktop_init, Role::Mobile));
        desktop_init[5] = 1;
        assert!(!valid_binary(&desktop_init, Role::Desktop));
    }
}
