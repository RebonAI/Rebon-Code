# rebon-relay-server

Standalone Axum relay pairing one Rebon desktop with one Rebon mobile client over
protocol 2 (`src/lib.rs:41`) and the `rebon.e2ee.v2` WebSocket subprotocol
(`src/lib.rs:42`). A bootstrap room lives only in memory; once both roles connect
it becomes trusted and is persisted in SQLite, surviving restarts until revoked
(`src/lib.rs:311`, `src/lib.rs:1309`). Frames are relayed byte-for-byte and the
relay never decrypts payloads, pairing codes, or E2E keys (`src/lib.rs:1481`).

## Why this is its own workspace

`Cargo.toml:1` opens with a bare `[workspace]` table ahead of the package table
(`Cargo.toml:3`), so the crate is an independent workspace and not a member of
the monorepo root. It has no `rebon-*` dependency at all: every entry in both
dependency tables is third-party (`Cargo.toml:10`, `Cargo.toml:31`).
The root workspace lists this service in neither `members` nor `exclude`; what
it excludes explicitly is `services/rc-server` (see `../../Cargo.toml`). Run
Cargo from this directory, or pass `--manifest-path
services/relay-server/Cargo.toml` from the repository root; a root-level
`cargo test -p rebon-relay-server` does not apply.

## Dependencies and dependents

Runtime: axum 0.8 (ws, json), tokio (full, test-util), tower-http (limit, trace),
rusqlite `=0.32.1` bundled, plus base64, chrono, fs2, futures-util, hmac, http,
rand, serde, serde_json, sha2, subtle, tokio-util, tracing, tracing-subscriber,
url (`Cargo.toml:10`-`Cargo.toml:29`). Dev-only: reqwest (json), tempfile,
tokio-tungstenite 0.26 (`Cargo.toml:31`-`Cargo.toml:34`). `unsafe_code` is
forbidden (`Cargo.toml:36`). Nothing in the repository depends on this crate; its
consumers are the deployment artifacts (`Dockerfile:1`,
`deploy/docker-compose.yml:1`) and protocol-2 clients.

## Public surface

- Library: `Config` / `Config::validate` (`src/lib.rs:62`, `src/lib.rs:77`);
  `RelayState::new` / `open` / `config` / `shutdown` / `start_cleanup`
  (`src/lib.rs:377`, `src/lib.rs:388`, `src/lib.rs:441`, `src/lib.rs:445`,
  `src/lib.rs:465`); `app()` (`src/lib.rs:668`).
- Binary `rebon-relay-server` (`Cargo.toml:4`, `src/main.rs:9`), configured by
  `REBON_RELAY_BIND`, `_PUBLIC_URL`, `_PENDING_CAP`, `_TRUSTED_CAP`,
  `_SOCKETS_PER_IP`, `_TRUST_FORWARDED_FOR`, `_DATABASE_PATH`, `_TOKEN_HMAC_KEY`
  (`src/main.rs:19`-`src/main.rs:34`).
- HTTP: `GET /healthz`, `POST /v1/pairings`, `DELETE /v1/pairings/{id}`,
  `POST /v1/pairings/{id}/handoff`, `POST /v1/handoffs/claim`,
  `POST /v1/pairings/{id}/tokens/rotate`, `GET /v1/pairings/{id}/socket`
  (`src/lib.rs:670`-`src/lib.rs:679`), behind a 1 KiB body limit
  (`src/lib.rs:680`) and `Cache-Control: no-store` (`src/lib.rs:1727`).

## Invariants

- Wire: `RB` magic plus protocol byte; DATA frames declare a payload > 16 equal
  to `len - 16`; INIT frames are exactly 24 bytes with the role byte and zero
  padding (`src/lib.rs:1631`-`src/lib.rs:1653`). Max frame 65,536
  (`src/lib.rs:43`), with one byte of upgrade headroom so the relay can answer
  4413 rather than tungstenite's 1009 (`src/lib.rs:1379`).
- Close codes: 4410 revoked, 4426 superseded, 4408 timeout, 4413 resource limit,
  4422 protocol error, 1000 normal (`src/lib.rs:49`-`src/lib.rs:57`).
- Only HMAC-SHA-256 digests are stored, with domain-separated cancel and
  handoff-claim digests (`src/lib.rs:475`, `src/lib.rs:479`, `src/lib.rs:487`).
- The database is single-owner: an exclusive `<db>.lock` file is taken at open,
  so a second process fails to start (`src/lib.rs:129`, `src/lib.rs:139`).
- Cancel removes the room before the blocking revoke, then tombstones the token
  for 300 s (`src/lib.rs:831`, `src/lib.rs:887`).
- Lock order is `rooms` -> `handoff_claims`, never the reverse
  (`src/lib.rs:288`, `src/lib.rs:596`); `pending_count` is mutated only while
  `rooms` is held and is debug-asserted against the map (`src/lib.rs:280`,
  `src/lib.rs:561`).
- Two connected roles promote the room to trusted, persisting first and
  reverting the row if the room vanished during the write (`src/lib.rs:1280`,
  `src/lib.rs:1356`); `UpgradeGuard` frees the reservation and slot when the
  upgrade never yields a task (`src/lib.rs:1590`).
- `validate` rejects any public URL that is not an HTTPS origin except HTTP
  loopback in debug builds, and rejects zero caps (`src/lib.rs:76`).

## Tests

```
cd services/relay-server
cargo check --all-targets
cargo test --no-fail-fast
```

Result here (Windows, crate toolchain): `cargo check --all-targets` clean; lib
unit tests 21 passed / 0 failed / 0 ignored (0.25 s), bin unit tests 0, doc-tests
0; the only test module is in-crate (`src/lib.rs:1749`) - no `tests/` directory,
no `#[ignore]`, no platform gate, no external network (`src/lib.rs:1794`).

## Gaps

- Binary wiring (env parsing, signal handling) is untested (`src/main.rs:48`,
  `src/main.rs:81`).
- `trusted_at_unix` is written but never read back (`src/lib.rs:237`,
  `src/lib.rs:171`); rooms loaded at startup get `expires_at = now`, inert since
  trusted rooms are never expiry-reaped (`src/lib.rs:417`, `src/lib.rs:624`).
- `rustfmt`/`clippy -D warnings` were not run; the crate only declares
  `[lints.clippy] all = "warn"` (`Cargo.toml:39`).
