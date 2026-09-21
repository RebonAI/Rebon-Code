# rebon-rc-server

The Rebon **Remote Control** (RC) service: accounts, registered
environments, a durable work queue with leases, and the session records
a control surface reads. It is the server half of
`crates/rebon-bridge`, which has had the client types and pure logic all
along but no backend to talk to.

**This crate is the server**: the data model, the account / device /
environment routes, and the transport half of the
[session stream](#the-session-stream), plus the
[history and session list](#history-and-the-session-list) a controller
reads it back through — see
[What is not here yet](#what-is-not-here-yet). The client transports live
in `crates/rebon-bridge`: the HTTP client behind its `http` feature,
driven against this server by
[`tests/bridge_client.rs`](tests/bridge_client.rs), and the session-stream
client behind its `ws` feature, driven by
[`tests/session_stream.rs`](tests/session_stream.rs); the paged reads are
driven through the same HTTP client by
[`tests/history.rs`](tests/history.rs). What a session runner needs from
the protocol — [projects](#projects-and-work-items), work
items that say what to run and where, [frame
identity](#frame-identity), control-request verdicts and the reported
state vocabulary — is driven end to end by
[`tests/runner_protocol.rs`](tests/runner_protocol.rs).

## RC is not the relay

The repository already contains `relay-server`. It is a different thing,
and conflating the two would quietly weaken the promise relay makes
:

| | `relay-server` | `rc-server` (this crate) |
|---|---|---|
| Topology | two slots; both ends must be online at once | environment + durable queue; many controllers; offline delivery |
| Server visibility | **zero-knowledge** — ciphertext frames only | **plaintext** — the server understands the content |
| State | rooms are in-memory; only pairings persist | environments, work, sessions and events all persist |
| Single instance | enforced with an exclusive `fs2` lock on the database | **not inherited** — RC takes no such lock |
| Body limit | 1 KiB | 256 KiB (prompts are not 1 KiB) |

They are separate services, separate databases, separate Caddy sites.
Relay is untouched by this crate. The plaintext trade-off is deliberate
and is what buys content-shaped features (history on a new device,
cross-session search, notification previews); RFC-0008 §3 states the
cost, and §7–§8 the mitigations that are still owed.

## Protocol types come from `rebon-bridge`

RFC-0008 §4 makes `crates/rebon-bridge` the single definition of the
wire protocol. This crate depends on it by path and uses
`rebon_bridge::config::{BridgeConfig, RegisteredEnvironment,
WorkItem, WorkResponse, WorkData, SessionWork, EnqueueWorkRequest,
EnqueuedWork, HeartbeatOutcome, PermissionResponseEvent, …}`,
`rebon_bridge::projects::{ProjectList, EnvironmentList, …}`,
`rebon_bridge::session_stream::{SessionFrame, DeliveredFrame, …}` and
`rebon_bridge::history::{SessionEventPage, SessionPage, …}` for request
and response bodies directly — nothing is hand-copied, so a type change
breaks this build.

Rust types alone would not catch a *serde* change (a `rename`, a
case convention), so `tests/wire_shape.rs` additionally pins the JSON
keys on both sides: the structs as `rebon-bridge` serializes them, and
the same shapes as they come off a live server.

## Routes

Everything is under `/v1` except the health probe. Errors return only a
status code and a fixed opaque body (`{"error":"request rejected"}`);
every response carries `Cache-Control: no-store`.

| Method | Route | Credential | Success | `BridgeApiClient` method |
|---|---|---|---|---|
| GET | `/healthz` | none | 200 | — |
| POST | `/v1/devices` | bootstrap **or** device access | 201 | — |
| POST | `/v1/devices/token` | device refresh | 200 | — |
| GET | `/v1/devices` | device access | 200 | — |
| DELETE | `/v1/devices/{device_id}` | device access | 204 | — |
| POST | `/v1/environments` | device access | 200 | `register_bridge_environment` |
| GET | `/v1/environments` | device access | 200 | — (`HttpBridgeApiClient::list_environments`) |
| PUT | `/v1/environments/{env}/projects` | environment secret | 200 | — (`HttpBridgeApiClient::update_projects`) |
| DELETE | `/v1/environments/{env}` | environment secret | 204 | `deregister_environment` |
| GET | `/v1/environments/{env}/work` | environment secret | **200 / 204** | `poll_for_work_item` (`poll_for_work` is its envelope) |
| POST | `/v1/environments/{env}/work` | device access | 201 | — (`HttpBridgeApiClient::enqueue_work`) |
| POST | `/v1/environments/{env}/work/{id}/ack` | environment secret + session token | 204 | `acknowledge_work` |
| POST | `/v1/environments/{env}/work/{id}/heartbeat` | environment secret + session token | 200 | `heartbeat_work` |
| POST | `/v1/environments/{env}/work/{id}/stop` | environment secret **or** device access | 204 | `stop_work` |
| POST | `/v1/environments/{env}/sessions/{session}/reconnect` | device access | 202 | `reconnect_session` |
| POST | `/v1/sessions/{session}/events` | session token | 204 | `send_permission_response_event` |
| GET | `/v1/sessions/{session}/events?cursor=&limit=` | device access | 200 | — (`HttpBridgeApiClient::session_events`, [history](#history-and-the-session-list)) |
| GET | `/v1/sessions?environment=&cursor=&limit=` | device access | 200 | — (`HttpBridgeApiClient::list_sessions`, [history](#history-and-the-session-list)) |
| POST | `/v1/sessions/{session}/archive` | environment secret **or** device access | 204 | `archive_session` |
| GET | `/v1/sessions/{session}/stream` | session token **or** device access | **101** | — ([the session stream](#the-session-stream)) |

Notes on individual routes:

- **`POST /v1/environments` always answers 200**, whether a row was
  created or an existing one reused. It is an upsert and the body is
  identical either way, so a split 200/201 would give the client nothing
  to act on. It is idempotent on `BridgeConfig.environment_id`, or on
  `reuse_environment_id` when the client supplies one, and **rotates the
  environment secret on every call** — a re-register invalidates whatever
  the previous process was holding. Re-registering a deregistered
  environment revives it. It also **replaces the project list** (see
  [below](#projects-and-work-items)). `worker_type` is stored and listed
  but never interpreted; this codebase sends `rebon`
  (`WellKnownWorkerType::Rebon`).
- **`GET .../work` answers 204 on timeout**, never a 200 with an empty
  body. `timeoutMs` is clamped to 60 s (`REBON_RC_MAX_POLL_WAIT_SECONDS`);
  omitting it uses the configured default. The poll parks on a
  per-environment wakeup, so enqueued work reaches a waiting bridge
  immediately rather than after the timeout. At most one item is handed
  out per call and the claim is a single immediate transaction, so two
  pollers can never win the same item.
- **`reclaimOlderThanMs`** is `PollOptions::reclaim_older_than_ms`. An
  item is reclaimable when its lease deadline has passed *or*, when the
  hint is present, when it has been leased for longer than the client is
  willing to wait. Without the hint a live lease is respected.
- **`.../heartbeat` returns 200 even when the lease is gone**, with
  `lease_extended: false`. Losing a lease is news, not an error.
- **`state` on both the heartbeat and a leased `WorkResponse` is the
  item's own row state** (`leased`, `acked`, `stopped`, `done`, `ready`),
  never a synthetic word.
- **`GET /v1/environments` never includes a secret.** It is
  `rebon_bridge::projects::EnvironmentList`, and each environment carries
  the `projects` it currently advertises.

### Status codes

The three failure modes are kept distinct, and the distinction is part
of the contract (`tests/auth.rs`):

| Status | Meaning |
|---|---|
| **401** | No credential, a malformed one, or one that resolves to nothing in the class this route accepts. A credential minted for a *different* class never crosses over. |
| **403** | A genuine credential used outside its scope: an environment secret against a different environment, a session token against a different session. |
| **404** | A genuine, in-scope credential whose resource is gone or was never the caller's: a deregistered environment, a revoked device, an unknown id, a resource on another account — including a `session_id` in an enqueue body that belongs to another environment. |
| **409** | The caller's session token is genuine but its work item is no longer leased to it — stopped, archived, or reclaimed. A worker reads this as "stand down", not as "go fetch a fresh credential". The heartbeat is the exception: it answers 200 and reports the loss in its body. |
| **400** | A body or query that does not parse or cannot be served as written: bad JSON, an unknown field or query parameter, a `limit` of `0` or not a number, a page cursor that does not verify, a project list RC will not store, work for a project the environment does not advertise. |
| **413** | Body over `REBON_RC_MAX_BODY_BYTES`. |
| **429** | Per-IP budget exhausted on a credential-issuing route. |

Credentials are resolved *before* the resource is looked up, so an
unknown-resource 404 never tells an unauthenticated stranger which ids
exist.

## Credentials

RFC-0008 §6. RC is not an identity provider and stores no passwords.
All long-lived credentials are 32 random bytes rendered as unpadded
base64url (43 characters) and stored **only** as HMAC-SHA256 digests
keyed by `REBON_RC_TOKEN_HMAC_KEY`, compared in constant time. Each
class is hashed under its own domain separator, so a token minted for
one purpose can never authenticate another.

| Class | Issued by | Scope | Revocation |
|---|---|---|---|
| Bootstrap | configuration | mints the first account, **once** | spent by first successful use |
| Device refresh | `POST /v1/devices` | one device, long-lived | `DELETE /v1/devices/{id}` |
| Device access | `POST /v1/devices/token` | one device, `REBON_RC_ACCESS_TOKEN_TTL_SECONDS` | expiry, refresh, or revocation |
| Environment secret | `POST /v1/environments` | one environment | rotated on re-register; **cascades dead when its device is revoked** |
| Session token | issued with a leased work item | one session | dies when the item is reclaimed, stopped or finished |

The bootstrap token is valid only while no account exists, and the
check shares a transaction with the insert that creates the account —
"single use" is a property of the data, not a flag that could drift from
it. Device issuance and revocation are written to the audit table.

`POST /v1/devices` takes and answers
`rebon_bridge::devices::{IssueDeviceRequest, IssuedDevice}`, and
`HttpBridgeApiClient::issue_device` is the client call — `rebon rc
login` uses it with either the bootstrap token or an existing device's
access token.

There is no **account session** yet (the browser cookie of RFC-0008 §6),
because there is no login. Wherever the RFC says "device credential
or account session", this implementation means the device access token;
the cookie path would join at `auth::device_or_environment` without
changing any route below it.

## Projects and work items

One environment is one **machine**, not one checkout. It advertises the
projects it will run sessions in, and every piece of session work names
one of them. Types: `rebon_bridge::projects` and
`rebon_bridge::config::{SessionWork, WorkItem, EnqueueWorkRequest}`.

### Advertising projects

`BridgeConfig` gained an optional `projects` array (keys are single words,
so the shape is the same in the camelCase registration body and in the
snake_case responses):

```json
{
  "dir": "/home/me/src/app", "machineName": "workshop", "branch": "main",
  "workerType": "rebon", "environmentId": "…", "…": "…",
  "projects": [
    {"path": "/home/me/src/app", "label": "App",
     "remote": "git@example.com:me/app.git", "branch": "main"},
    {"path": "D:\\work\\docs", "label": "docs"}
  ]
}
```

- A registration **without** `projects` (or with an empty array) serves
  `dir` alone, labelled after its last path component and carrying
  `gitRepoUrl` / `branch` — every registration body from before projects
  keeps working and means what it meant. An empty `dir` too means no
  projects.
- Registration replaces the stored list in the same transaction.
  `PUT /v1/environments/{env}/projects` with `{"projects": […]}` replaces
  it later, authenticated by the environment secret (a device token is
  401, another environment's secret 403), and answers with the list as
  stored. An empty list is allowed: the machine serves nothing for now.
- A list is refused (400, nothing changes) when it has more than
  `MAX_PROJECTS` (256) entries, an empty or duplicate `path`, an empty
  `label`, or any field over 4 KiB.
- **A project's identity is its `path`, compared exactly.** RC does not
  know the machine's filesystem rules, so `/a` and `/a/` are different
  projects and a controller sends back a path it read from the list,
  byte for byte.

### Queueing work

`POST /v1/environments/{env}/work` takes `EnqueueWorkRequest` (unknown
keys are refused):

```json
{"type": "session", "prompt": "fix the flaky test",
 "project": "/home/me/src/app",
 "session_id": "sess_…",
 "resume_rebon_session_id": "0192f3c4-…"}
```

and answers 201 with `{"work_id": "wrk_…", "session_id": "sess_…"}`
(`session_id` is `null` for a healthcheck).

- Session work needs `prompt`, `resume_rebon_session_id`, or both. A
  healthcheck takes none of the session fields.
- `resume_rebon_session_id` is Rebon's own transcript id, not an RC
  session id. It must satisfy `valid_rebon_session_id` — 1–128 characters
  of `[A-Za-z0-9._-]`, not starting with `.` — because it ends up naming
  something on the machine's disk. The runner checks it again.
- The project is chosen inside the enqueue transaction:
  1. a `session_id` that exists must be on this environment and account,
     or the request is **404** (it used to be accepted, which would have
     handed this environment's worker a token for someone else's session);
  2. a session that already has a project keeps it — `project` may be
     omitted, and a different one is 400;
  3. otherwise `project` is used, or, when omitted, the environment's only
     project (none or several: 400);
  4. the result must be a project the environment advertises **now**, or
     400.
- Work already queued is not re-checked when the list changes; the
  runner is the last to decide whether it can still serve a project.

### What the poll hands out

`GET …/work` answers with `WorkItem`: the `WorkResponse` envelope, plus a
`session` object for session work. Before:

```json
{"id": "wrk_…", "type": "work", "environment_id": "env_…", "state": "leased",
 "data": {"type": "session", "id": "sess_…"},
 "secret": "…", "created_at": "…"}
```

After:

```json
{"id": "wrk_…", "type": "work", "environment_id": "env_…", "state": "leased",
 "data": {"type": "session", "id": "sess_…"},
 "session": {"project": "/home/me/src/app",
             "prompt": "fix the flaky test",
             "resume_rebon_session_id": "0192f3c4-…"},
 "secret": "…", "created_at": "…"}
```

`session` sits **next to** `data` rather than inside it: `WorkData` and
`WorkResponse` are built with struct literals by crates outside the
protocol, so growing them would break those callers, while a sibling key
is the same additive change on the wire — a decoder that only knows the
envelope ignores it. `prompt` and `resume_rebon_session_id` are omitted
when absent; `session` is omitted for a healthcheck and for session work
queued before projects existed (a runner stops such work). The prompt is
**not** in the work secret, which stays credential material only.

A reconnect (`POST …/sessions/{session}/reconnect`) queues work in the
session's project, with the newest item's resume target and **no**
prompt — the prompt already ran.

## The work secret

`WorkResponse.secret` is declared by `rebon-bridge` as "base64url-encoded
JSON that decodes into a work secret" and left to the server to define.
RC defines it as base64url (unpadded) of:

```json
{
  "session_token": "<43-character base64url>",
  "session_id": "sess_…",
  "ingress_url": "wss://rc.example.com/v1/sessions/sess_…/stream"
}
```

`session_id` is **absent** for a healthcheck item, which has no session;
`ingress_url` is then just the ingress origin. `WorkData.id` is the
session id for session work and the work id for a healthcheck.

The *shape* and its codec live in `rebon_bridge::work_secret::WorkSecret`
— RFC-0008 §4 makes `rebon-bridge` the single definition point of the
protocol, so client and server share one encoder/decoder rather than two
that could drift. What stays here is `work_secret::mint`, which is the
only thing that is genuinely server-side: the ingress origin and the
session token are RC's to choose. For session work the URL it carries is
the [session stream](#the-session-stream), and the session token beside
it is the credential that opens it.

## The session stream

`GET /v1/sessions/{session}/stream` is a WebSocket carrying JSON text
frames. It is the live side of a session; everything above is the durable
side. Implementation: [`src/routes/stream.rs`](src/routes/stream.rs) (the
route) and [`src/hub.rs`](src/hub.rs) (who is attached).

### Who may attach

The credential is `Authorization: Bearer …`, checked **before** the
upgrade, and resolved before the session is looked up — an unknown session
id tells a stranger nothing. Both roles go through the same `auth` helpers
the HTTP routes use, so the statuses are the ones in the table above.

| Credential | Role | Refusal |
|---|---|---|
| Session token for this session, item still `leased` / `acked` | **worker** | — |
| Session token for another session | — | 403 |
| Session token whose item was stopped, archived or reclaimed | — | **409** |
| Device access token on the session's account | **controller** | — |
| Device access token on another account, or an unknown session | — | 404 |
| No credential, a malformed one, or another class (environment secret, refresh token) | — | 401 |

A worker whose lease is gone never gets a socket: the session has already
been handed to somebody else. The client reports the 409 and a mid-stream
`4409` through one call, `SessionStreamError::is_lease_gone`.

### Frames

The envelope is `rebon_bridge::session_stream::SessionFrame` — defined in
`rebon-bridge`, not here. Every frame is an object tagged on
`type`:

| From | `type` | Payload |
|---|---|---|
| worker | `session_message` | `message` — opaque; the runner picks the shape. Optional `message_id` |
| worker | `session_state` | `state` — a [state word](#reported-state), optional `detail` |
| worker | `permission_request` | `request_id`, `request` — opaque |
| worker | `control_response` | `response`: `{subtype, request_id, response?, error?}` — see [control requests](#control-requests) |
| worker | `session_bound` | `rebon_session_id` — see [binding the local session](#binding-the-local-session) |
| controller | `prompt` | `text`, optional `attachments` — opaque |
| controller | `cancel` | — |
| controller | `permission_response` | `response`: `PermissionResponseBody` |
| controller | `question_response` | `request_id`, `answers`: one `{selected_options, other_text?}` per question |
| controller | `control_request` | `request_id`, `subtype`, optional `params` — opaque |
| server | `stream_error` | `code`, `message`; with `already_answered` also `request_id` and `answered_by` — see [concurrent controllers](#concurrent-controllers) |

A `type` this build does not know is **not** an error: it is relayed and
stored verbatim, routed by the sender's role, so a peer one release ahead
does not break the ingress. A *known* type from the wrong side (a worker
sending `prompt`, a controller sending `session_message` or
`stream_error`) is a protocol error.

Every frame RC delivers after persisting it also carries a top-level
`event_id` — see [frame identity](#frame-identity). `event_id` is
reserved: no frame type defines it and a peer may not send it.

`question_response` answers a `permission_request` that is a question
(`AskUserQuestion`): the runner marks those `_meta.rebonRc.kind:
"question"` with `answerWith: "question_response"` and lists the questions
in the request's `questions`. RC routes and stores it like
`permission_response` — to the worker, mirrored to the other controllers,
never keyed, and subject to the same
[first-answer rule](#concurrent-controllers) — and does not look inside.
A runner that refuses an answer replies with a `control_response` error
for the `request_id`. An RC server from before this frame relays it as an
unknown controller frame, which is the same routing.

A `PermissionResponseEvent` posted to `POST /v1/sessions/{session}/events`
serializes as exactly a `control_response` frame, so that route fans its
event out to attached controllers too: a decision looks the same on both
surfaces, live and on replay.

### Frame identity

**Server-stamped `event_id`.** A persisted frame goes out — live, in the
attach-time replay, and from the HTTP events route — with the id it was
stored under spliced in front of its other keys:

```json
{"event_id": 41, "type": "session_message", "message": {"text": "hi"}}
```

The stored payload is unchanged (history returns
`{"type": "session_message", …}` as sent); the key is added on the way out
by `stamp_event_id`, which splices text instead of re-encoding it, so every
other byte is delivered as received. `SessionStreamRx::recv_delivered`
yields `DeliveredFrame { event_id, frame }`; `recv` still yields the bare
frame. The id is the same one `GET …/events` pages by, so a controller that
reads a history page and holds a socket drops anything it has already seen.
A frame RC did not persist (`stream_error`) has no `event_id`. A peer that
sends one — even `null` — is closed with `4422`.

**Worker-chosen `message_id`.** A `session_message` may carry
`"message_id": "…"` (1–256 bytes). RC stores at most one frame per
`SessionFrame::idempotency_key` per session: `session_message:<message_id>`,
and `permission_request:<request_id>` for permission requests. A worker
that reconnects and resends what it is unsure about gets **no answer at
all** for a frame already stored: it is not stored again, not delivered
again, and the socket stays open — the history already holds it once,
which is what the worker wanted. A duplicate does not count as activity.
Frames without a key are stored every time. An empty or over-long id is a
`4422`.

### Binding the local session

An RC session is run on the machine as a Rebon session with an id of the
machine's own. A runner says which one with

```json
{"type": "session_bound", "rebon_session_id": "0192f3c4-…"}
```

as soon as it has opened or resumed it (and again after every
reconnect). RC stores the frame like any other worker frame — keyed on
`session_bound:<rebon_session_id>`, so a resend is stored once — and, in
the same transaction, records the id on the session row
(`sessions.rebon_session_id`). From then on:

- a work item that names no `resume_rebon_session_id` — a prompt queued
  for the session while no worker was attached, or an item requeued
  after its lease lapsed — is handed out with the bound id as its resume
  target (`COALESCE(work.resume_rebon_session_id,
  sessions.rebon_session_id)` at claim time);
- an explicit `resume_rebon_session_id` on an item still wins;
- `POST …/reconnect` queues its item with the bound id, falling back to
  the newest item's target only when nothing was ever bound.

Before this, a `reconnect` of a session that started fresh carried no
resume target at all, and the runner had no way to tell the second item
from a request for a new local session. The id is checked with
`valid_rebon_session_id` when the frame is parsed, so one that is empty,
hidden, or has a path separator is a `4422`; a controller that sends
`session_bound` is a `4422` like any other worker frame from the wrong
side. `rebon-bridge` builds the frame with `SessionFrame::bound`.

### Control requests

A `control_request` is relayed to the worker like any controller frame.
The worker answers from `rebon_bridge::control_request`, which no longer
answers success on its own: `set_model`, `set_max_thinking_tokens`,
`set_permission_mode` and `interrupt` are all answered from the verdict the
worker reached by trying the change — applied, with **when**, or refused —
and no verdict is an error naming the subtype. Before, `set_model` got

```json
{"type": "control_response", "response": {"subtype": "success", "request_id": "r1"}}
```

whether or not anything changed; now it is

```json
{"type": "control_response",
 "response": {"subtype": "success", "request_id": "r1",
              "response": {"applies": "next_turn"}}}
```

with `applies` one of `now` / `next_turn`, or an `error` response. A Rebon
runner applies model and effort from the next turn and has no
thinking-token setting, so `set_max_thinking_tokens` comes back as
`{"subtype": "error", "error": "set_max_thinking_tokens is not supported by
this bridge"}`. `initialize` is unchanged. RC does not interpret any of
this; it relays and stores it.

### Reported state

`session_state.state` is a word from `SessionRunState`:

| Word | Meaning |
|---|---|
| `starting` | the runner took the work and is opening or resuming the session |
| `running` | a turn is in progress |
| `idle` | waiting for the next prompt |
| `needs_input` | a turn is blocked on a controller (permission request, question) |
| `stopped` | ended normally — terminal |
| `failed` | ended with an error, described by `detail` — terminal |

A word outside the set is kept and shown verbatim (`SessionRunState::Other`),
so a newer worker does not break an older controller; an empty word is a
malformed frame (`4422`). The session list's `reported_state.state` is
this type.

### Routing

- **One worker per session.** A second worker connection supersedes the
  first, which is closed with `4426`. Refusing the newcomer instead would
  strand the session behind a connection whose process may be gone.
- **Any number of controllers.**
- A **worker frame** is persisted, then fanned out to every controller.
  It is persisted whether or not anyone is attached — there is no "nobody
  is listening, drop it" path. That is what the plaintext design buys: a
  phone attaching later finds the content already here instead of waking
  the machine.
- A **controller frame** is persisted, sent to the worker, and mirrored to
  the *other* controllers (not back to the sender). Without the mirror a
  second control surface would see a different session live than on
  replay, because the backlog holds both directions.
- A **controller frame with no worker attached** is neither queued nor
  persisted. The sender gets
  `{"type":"stream_error","code":"no_worker",…}` and the socket stays
  open. Holding a prompt for a worker that may never return would make
  "sent" a lie; closing the socket would punish a controller that is only
  reading history. The controller decides whether to retry, queue work
  through `POST …/work`, or tell the user.

### Concurrent controllers

RFC-0008 §13a: first come, first served, the way the local TUI behaves.

**Arrival order.** A controller frame takes its session's turn
(`SessionHub::in_arrival_order`, a fair async mutex per session) before
the `no_worker` check and holds it until the frame has been routed. Frames
from any number of controllers are therefore stored and handed to the
worker one at a time, in the order they reached the server, and the
worker's order is the history's order. RC does not look at whether a turn
is running: a prompt that arrives mid-turn is routed at once, and the
session host on the machine queues it behind the running turn (its
durable pending-prompt queue, FIFO). The runner delivers prompts to the
session one at a time for the same reason.

**The first answer wins.** A `permission_response` or a
`question_response` (`SessionFrame::answered_request_id`) is stored only
if no other connection's answer holds that `request_id`. The one that
wins is routed and mirrored as usual, which is how the other controllers
learn the prompt is taken. A later one from any other connection — a
different device, or a second tab on the same device — is not stored and
not routed; its sender alone gets

```json
{"type": "stream_error", "code": "already_answered",
 "message": "perm-… was already answered by phone",
 "request_id": "perm-…",
 "answered_by": {"device_id": "dev_…", "label": "phone",
                 "connection_id": "c-3f9a…", "event_id": 41}}
```

This is RC's 409. `answered_by` names the device and its label, an opaque
per-socket tag (to tell two surfaces on one device apart) and the event
id the winning answer is stored under. It never carries a credential.

The claim lives in `session_answers` (below), and is written in the same
transaction as the answer's event, so of two racing answers exactly one is
stored. Three things let a later answer through anyway:

- **The same connection** sends again. A controller is not told it lost
  to itself; the runner judges the repeat.
- **The runner refused the claim.** A worker `control_response` with
  `subtype: "error"` for that `request_id` (policy refusal, answers that do
  not fit, "no longer pending") deletes the claim in the transaction that
  stores the refusal. The next answer is taken.
- **The claim was routed to a worker socket that is no longer the
  current one.** A worker is never replayed controller frames, so an
  answer queued for a socket that dropped may never have arrived; holding
  the prompt for it would hold it forever. The new answer is routed and
  takes the claim over. If the first answer did land, the runner refuses
  the second with "that prompt is no longer pending" — the runner's own
  check of the prompt it is waiting on is what guarantees a prompt is
  answered once; the server's claim is what tells the loser who won.
  Socket ids are in memory, so after a server restart every stored claim
  is in this state.

A refusal crossing a newer claim in flight releases that claim too; the
cost is one more answer judged by the runner. A controller's
`control_request` whose `request_id` happens to equal a prompt's would do
the same, for the same cost.

Compatibility: `request_id` and `answered_by` are optional members of a
frame every client already parses, so a controller built before them sees
an ordinary `stream_error` with an unknown code; `SessionFrame` did not
gain a variant. Nothing changes for workers.

### Replay

On attach, a controller is sent the last `REBON_RC_SESSION_REPLAY_EVENTS`
persisted frames, oldest first, before any live frame. A worker is not
replayed anything: it produced that history.

The replay cannot miss or repeat a frame, by construction:

1. Every frame is **persisted before it is fanned out**, and its
   `session_events.event_id` travels with it into each socket's queue.
2. A connecting controller's slot is **registered before the backlog is
   read** (`register_then_read_backlog`). A frame published during the
   read queues for the new socket rather than being missed.
3. That leaves an overlap — a frame can be both in the backlog and in the
   queue. The socket writes the backlog first, then drains its queue,
   **dropping any queued frame whose id is at or below the last replayed
   id** (`already_replayed`).

Live frames that arrive while the backlog is being written simply wait in
the queue. A backlog large enough to overflow that queue closes the
connection like any slow consumer, which is one reason the replay length
is capped at 2,000.

### Liveness and limits

| Limit | Value | On breach |
|---|---|---|
| Frame size | `REBON_RC_MAX_BODY_BYTES` — a prompt is the same size on either surface | close `4413` |
| Outbound queue per socket | 256 frames **or** 1 MiB, whichever comes first | close `4413` (the slow consumer only — the others keep receiving) |
| Ping interval | 25 s | — |
| Pong deadline | 10 s after a ping | close `4408` |
| Idle | 75 s with nothing received | close `4408` |

The 1 Hz sweeper also closes, with `4409`, any attached worker whose work
item is no longer leased — stopped, archived or expired — and drops the
in-memory state of sessions nobody is attached to.

### Close codes

The numeric code is the contract; the reason text is fixed per code and
never describes the session. The constants live in
`rebon_bridge::session_stream::close_code`, and the client classifies
them into `rebon_bridge::stream_client::CloseReason`.

| Code | Reason | Meaning | Reconnect with the same credential? |
|---|---|---|---|
| 1000 | `normal` | An orderly close from either side. Says nothing about whether the *session* is over — its frames and its work item say that. | yes |
| 1011 | `internal_error` | A frame could not be persisted, so it was not delivered either. | yes, later |
| 4408 | `timeout` | Missed pong, or idle too long. | yes |
| 4409 | `lease_gone` | The worker's work item stopped being leased to it. | **no** — stand down |
| 4413 | `resource_limit` | A frame over the size cap, or this peer fell behind its outbound queue. | yes (without the oversized frame) |
| 4422 | `protocol_error` | A binary frame, invalid JSON, a known frame type that is malformed (including a `session_bound` whose id is not a plain session id), a frame from the wrong side, a frame carrying `event_id`, or an empty / over-long `message_id` or keyed `request_id`. | **no** — it would be sent again |
| 4426 | `superseded` | A newer worker connection took this session. | **no** — it would evict that worker |

A connection that ends with no close frame at all — a reset, a proxy that
gave up — is `CloseReason::Network` on the client, and is worth
reconnecting.

### What the history keeps

Each frame is one `session_events` row: `kind` is the frame's `type`,
`payload_json` the frame's text exactly as it arrived, `dedupe_key` its
idempotency key when it has one. `event_id` is the order, and it is the
id the socket stamps on the way out. The index on `(session_id,
event_id)` is what both the replay and the
[history route](#history-and-the-session-list) read — `WHERE session_id =
? AND event_id < ? ORDER BY event_id DESC LIMIT ?` is a range scan on it.

## History and the session list

RFC-0008 §5 ("write-through and replay"): a worker streams whether or not anyone is
attached, so a controller has to be able to read everything back — with
the machine offline, and without the stream's frame and queue limits.
The attach-time replay only fills the live window; these two routes are
how a controller pages further. Implementation:
[`src/routes/history.rs`](src/routes/history.rs), cursors in
[`src/cursor.rs`](src/cursor.rs). Bodies are
`rebon_bridge::history::{SessionEventPage, SessionPage}`, and
`HttpBridgeApiClient::{session_events, list_sessions}` are the client
calls.

Both take a **device access token**. The credential is checked first,
then the resource — a session or environment filter on another account is
**404**, identical to one that does not exist, whatever the query says —
and only then the query (**400**).

### `GET /v1/sessions/{session}/events`

```json
{
  "events": [
    {"event_id": 41, "kind": "session_message",
     "payload": {"type": "session_message", "message": {}},
     "created_at": "2026-09-16T08:00:00.000Z"}
  ],
  "next_cursor": "AQEAAAAAAAAAKf…"
}
```

- The first page (no `cursor`) is the **newest** `limit` events. Each
  following page is the `limit` events just older than the last.
- Within a page, events are **oldest first**, so a client prepends a page
  as it is. `event_id` is the session's total order; it has gaps.
- `payload` is the frame exactly as persisted (a `SessionFrame` object,
  `type` included); `kind` is its `type`. A `PermissionResponseEvent`
  posted over HTTP appears as the `control_response` it serializes to.
- `next_cursor` is present **exactly** when older events remain — the
  server reads one extra row to know — and absent on the page that
  reaches the start, so a history that divides evenly does not end in an
  empty page.
- A page also ends early once its payloads reach **4 MiB**
  (`MAX_EVENT_PAGE_BYTES`), but always carries at least one event.
- The session's environment may be deregistered; its history stays
  readable.

### `GET /v1/sessions`

```json
{
  "sessions": [
    {"session_id": "sess_…", "environment_id": "env_…", "state": "running",
     "reported_state": {"state": "idle", "event_id": 41},
     "created_at": "…", "last_activity_at": "…",
     "last_event_id": 41, "last_event_at": "…"}
  ],
  "next_cursor": "AQI…"
}
```

- Ordered by **latest activity** first: the session row's `updated_at`,
  bumped by a persisted frame, a queued prompt, a lease, a reconnect or
  an archive. The clock has one-second resolution, so the session id
  (descending) breaks ties and the order is total.
- `state` is RC's own lifecycle word (`queued`, `running`, `archived`).
  `reported_state` is the worker's newest `session_state` frame — its
  `state` in the [reported-state vocabulary](#reported-state) — absent if
  it never sent one. `last_event_id` / `last_event_at` are absent for
  a session with no events.
- `environment` filters to one environment on the caller's account —
  deregistered ones included, so a dead machine's sessions stay listed.
  Any other value is 404.
- A session that becomes active while a client pages moves to the front:
  the walk never repeats or errors on it, and the next first-page read
  shows it.

### Paging

| Parameter | Meaning |
|---|---|
| `limit` | Page size. Absent or empty: `REBON_RC_PAGE_LIMIT_DEFAULT`. Above `REBON_RC_PAGE_LIMIT_MAX`: clamped to it. `0`, a sign, or anything but digits: 400. |
| `cursor` | The previous page's `next_cursor`, verbatim. Absent or empty: the first page. |

A cursor is **opaque**: unpadded base64url of a version, a kind, the
position, and a 16-byte HMAC-SHA256 tag under the token key with its own
domain separator (`rebon-rc-cursor-v1`). The tag covers the cursor's
**scope** — the session, or the account plus the `environment` filter —
so a cursor presented to another session, another account, another
filter or the other route fails verification and is refused with 400,
as is one that was edited, truncated or made up. The position is not
secret (anyone can decode it); the tag only stops a client minting one.
Rotating `REBON_RC_TOKEN_HMAC_KEY` invalidates outstanding cursors along
with every credential.

Positions are **exclusive bounds** (`event_id < ?`, `(updated_at,
session_id) < (?, ?)`), never rows that must still exist, so a cursor
keeps working after retention deletes the row it was issued at, or any
other rows.

A client that shows history *and* holds a live socket reconciles the two
by `event_id`: the socket stamps every persisted frame with the id its
history row has ([frame identity](#frame-identity)), so the overlap
between the replay window and the newest page is dropped by id.

## Configuration

All configuration is environment variables, `REBON_RC_*`, validated
before the listener binds.

| Variable | Default | Meaning |
|---|---|---|
| `REBON_RC_TOKEN_HMAC_KEY` | **required** | base64url-nopad 32 bytes. Every credential digest derives from it: changing it logs out every device and bridge. |
| `REBON_RC_BOOTSTRAP_TOKEN` | **required until an account exists** | One-time token, same 43-character shape as any credential. |
| `REBON_RC_BIND` | `127.0.0.1:8090` | Listen address. |
| `REBON_RC_PUBLIC_URL` | `http://127.0.0.1:8090` | Public origin of the API. Must be HTTPS outside debug loopback. |
| `REBON_RC_SESSION_INGRESS_URL` | `ws://127.0.0.1:8090` | Public origin of the session WebSocket, embedded in the work secret. Must be `wss` outside debug loopback. |
| `REBON_RC_DATABASE_PATH` | `rebon-rc.sqlite3` | SQLite file. |
| `REBON_RC_LEASE_TTL_SECONDS` | `90` | How long a lease survives without a heartbeat. |
| `REBON_RC_ACCESS_TOKEN_TTL_SECONDS` | `3600` | Device access token lifetime. |
| `REBON_RC_DEFAULT_POLL_WAIT_SECONDS` | `25` | Long poll when the client sends no `timeoutMs`. |
| `REBON_RC_MAX_POLL_WAIT_SECONDS` | `60` | Hard cap; may not exceed 60. |
| `REBON_RC_MAX_BODY_BYTES` | `262144` | Request body limit, and the size cap on one session-stream frame. |
| `REBON_RC_SESSION_REPLAY_EVENTS` | `200` | Frames replayed to a controller when it attaches. 1–2000. |
| `REBON_RC_PAGE_LIMIT_DEFAULT` | `100` | Page size of the history and session-list routes when the client sends no `limit`. |
| `REBON_RC_PAGE_LIMIT_MAX` | `500` | Largest page those routes return; a bigger `limit` is clamped. 1–1000, and at least the default. |
| `REBON_RC_AUTH_RATE_BURST` | `30` | Per-IP burst on credential-issuing routes. |
| `REBON_RC_AUTH_RATE_REFILL_PER_MINUTE` | `30` | Sustained refill of that budget. |
| `REBON_RC_TRUST_FORWARDED_FOR` | `false` | Take the client IP from the rightmost `X-Forwarded-For` entry. Enable only behind a proxy that overwrites the header. |

Rate limiting covers the routes that mint or exchange credentials.
Poll, heartbeat and ack are deliberately unmetered: they are
environment-authenticated and are supposed to run hot.

## Storage

One SQLite file, WAL, `synchronous = FULL`, additive
`CREATE TABLE IF NOT EXISTS` schema applied in a single `execute_batch`
at open. Columns added since a table first shipped (`work.project_path`,
`work.resume_rebon_session_id`, `sessions.project_path`,
`sessions.rebon_session_id`, `session_events.dedupe_key`) are added to an older database with
`ALTER TABLE … ADD COLUMN` at open; all are nullable. Writes run on
`spawn_blocking`.

| Table | Holds |
|---|---|
| `accounts` | account id, plus nullable `issuer`/`subject` for OIDC |
| `devices` | credential digests, label, created / last-seen / revoked |
| `environments` | account, device, client idempotency key, secret digest, the `BridgeConfig` fields worth querying **plus the raw config JSON**, created / last-seen / deregistered |
| `environment_projects` | environment, position, path, label, remote, branch — the advertised list, keyed on `(environment_id, path)` |
| `work` | environment, type, state, session, prompt, project path, resume target, session-token digest, timestamps, lease deadline, force flag |
| `sessions` | environment, account, state, project path, the bound Rebon session, created / updated (the list's activity key, indexed with the account and id) |
| `session_events` | session, kind, verbatim payload JSON, created, dedupe key — every session-stream frame, plus the events posted over HTTP; `event_id` is the order. Indexed on `(session_id, event_id)` for paging, on `(session_id, kind, event_id)` for the list's latest `session_state`, and uniquely on `(session_id, dedupe_key)` where the key is set |
| `session_answers` | one row per prompt whose answer currently holds it, keyed on `(session_id, request_id)`: the answer's event id, device, controller socket id and the worker socket it was routed to, answered-at. Deleted when the runner refuses the answer. See [concurrent controllers](#concurrent-controllers) |
| `audit` | actor, action, target, created |

Work states are `ready → leased → acked → done`, with `stopped`
reachable from any non-terminal state and `leased | acked → ready` on
lease expiry. The queue is FIFO by `created_at_unix` then **`rowid`** —
not the work id, which is random, so items enqueued in the same second
would otherwise come out in arbitrary order.

A 1 Hz sweeper returns expired leases to `ready` (waking that
environment's pollers), clears expired access tokens, hangs up on any
attached worker whose lease is gone, and prunes the in-memory rate,
wakeup and session-stream maps.

Every session-stream frame costs one `synchronous = FULL` commit. That is
the same durability the HTTP events route already pays, and it is what
lets a controller that attaches later trust the replay; it is also the
stream's throughput ceiling.

Note that the `work` secret column stores the session token's **digest**,
not the token: the plaintext exists only in the poll response that
hands it out.

## What is not here yet

What this crate does — the server, the stream's transport, and the paged
reads of what the stream persists — is all here, so these are
deliberately absent rather than half-stubbed:

| Not yet implemented | Owed |
|---|---|
| **Session runner** | Not in this service: it lives in the main workspace as `crates/rebon-rc-runner` (`rebon rc login / serve / status`); see the remote-control design notes. The one protocol addition it needed is [`session_bound`](#binding-the-local-session). |
| **CLI wiring** | Building `BridgeConfig` from the environment, and making the TUI bridge pill and `ShowBridgeDialog` real. |
| **Web UI** | `{base_url}/code?bridge={env}`, and with it the **account session** cookie (httpOnly + SameSite + CSRF) and **OIDC** login. Until then an RC instance holds exactly one account. |
| **CI and self-hosting docs** | RC, `rebon-bridge` and `relay-server` all in the check/test matrix, plus the deployment guide. |

Also outstanding, from the RFC:

- **Encryption at rest** (§7). Not implemented. The database is
  plaintext on disk today.
- **Retention and data minimisation** (§8). No retention window, no
  per-repository metadata-only switch, no notification-preview toggle.
  Paging already tolerates rows disappearing underneath a cursor, so a
  retention sweep can delete `session_events` rows freely.
- **Refresh-token rotation.** A device that needs new long-lived
  material is revoked and re-issued.
- **Quotas and abuse controls** (§13) beyond the per-IP auth budget.

## Development

This crate is **its own workspace** (like `relay-server`) with a
committed `Cargo.lock`; it is not a member of the root workspace.

```sh
cargo check   --manifest-path services/rc-server/Cargo.toml --tests
cargo clippy  --manifest-path services/rc-server/Cargo.toml --all-targets
cargo test    --manifest-path services/rc-server/Cargo.toml
cargo fmt     --manifest-path services/rc-server/Cargo.toml -- --check
```

Tests bind a real listener and drive it with `reqwest`, in
`relay-server`'s style — no in-process service calls and no mocked
transport. `tests/bridge_client.rs`, `tests/session_stream.rs`,
`tests/history.rs` and `tests/runner_protocol.rs` go one further and drive the server through the real `rebon-bridge` clients,
which also keeps its `http` and `ws` features compiled here.

The library is also used from the main workspace:
`crates/rebon-rc-runner` takes it as a dev-dependency, and its
`tests/rc_e2e.rs` runs `app` in process to test the real runner against
it (`cargo test -p rebon-rc-runner --test rc_e2e`). The root
`Cargo.toml` lists this directory under `exclude` so that the path
dependency does not make it a member of the root workspace. As a result,
a change to `Config`, `RcState::open`, `RcState::start_cleanup` or `app`
also has to build in that test.

Two things about the test graph. The dev-dependency brings
`tokio-tungstenite` 0.24 (the client, pinned to the main workspace's
version) next to the 0.29 axum uses for the server; both are rustls-only,
and only the test binaries link both. And `session_stream.rs`'s
mid-stream replay test is probabilistic by nature — the deterministic
guard of the replay ordering is the unit test beside
`register_then_read_backlog`.
