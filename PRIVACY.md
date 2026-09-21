# Privacy

**Rebon has no telemetry.** No analytics SDK, no crash reporter, no usage
events, no anonymous install ID, no device fingerprint, no A/B experiment
service. There is no Rebon server that collects anything about you, your
machine, your prompts, or your sessions.

The rest of this document lists **every** outbound request the binary can
make, so you can check the claim instead of taking it on faith. Each entry
names the file it lives in.

· [中文](docs/PRIVACY.zh-CN.md)

## What Rebon sends on its own

One request, ever: the update check.

`GET https://registry.npmjs.org/@rebon%2fcli` — reads the package's
`dist-tags` and compares versions locally
([`crates/plugins/updater/src/check.rs`](crates/plugins/updater/src/check.rs)).

- **No identifiers.** Bare HTTP client, no `User-Agent`, no query string, no
  headers of our own. Your current version is **not** sent — the comparison
  happens after the response arrives.
- **Lazy.** The check starts when a front end that can actually draw an
  update notice polls for it, so `rebon exec`, ACP sessions and background
  workers never issue it at all
  ([`crates/plugins/updater/src/seat.rs`](crates/plugins/updater/src/seat.rs)).
- **Skipped in dev builds** (version `0.0.1`).
- **Off switches:** `update.disabled` in `~/.rebon/config.json`;
  `plugins.updater.enabled = false` (the plugin is then never loaded, so no
  seat exists and `/update` is not on the command seat either); or
  `REBON_UPDATE_PACKAGE` to point at your own package.

What npm learns: your IP, and that someone asked for `@rebon/cli`'s public
metadata. That is the entire unsolicited footprint.

There is no startup ping, no heartbeat, no background sync, and no
"first run" registration.

## What Rebon sends when you ask it to

Everything below is triggered by you or by a tool call you approved. None of
it is reported to Rebon.

| Trigger | Goes to | Carries |
| --- | --- | --- |
| Any model turn | your configured provider base URL | your prompt, files, and tool output — this is the product |
| Sign-in / token refresh | the provider's auth host (e.g. `auth.openai.com`) | OAuth credentials only |
| `/model refresh` | `https://reboncode.ai/api/models` | nothing — a plain `GET`, no request body. Override with `REBON_MODEL_TABLE_URL` to self-host |
| `WebFetch` tool | the URL the agent fetched | a plain fetch of that URL |
| `WebSearch` tool (fallback) | `api.search.brave.com`, `html.duckduckgo.com`, or `www.bing.com` | the search query. Used only when the active provider has no server-side search |
| MCP tools | the MCP servers you configured | whatever that server's protocol carries |
| Hooks | wherever your hook points | whatever your hook sends |
| `rebon node install` | `https://nodejs.org/dist` | nothing; archives are SHA-256 pinned, and a mirror base can be passed |
| `rebon remote` | `registry.npmjs.org` for the server tarball, then your host over ssh | the session runs on the far machine you named |
| Image generation | the provider's image endpoint | your prompt |

The model table (`/model refresh`) is the only one of these that touches a
Rebon-operated host, it must be run by hand, and its only caller is the
`/model` command
([`crates/rebon-session-runtime/src/commands/model.rs`](crates/rebon-session-runtime/src/commands/model.rs)).
Rebon never refreshes it on its own; at startup it reads the local cache or
the snapshot embedded in the binary.

## Notes and edge cases

- **The one header that looks like telemetry.** Requests the embedded dsh
  runtime sends to DeepSeek carry `x-deepseek-harness-user-id`. The upstream
  package generates a random ID and persists it for telemetry correlation;
  Rebon's shim replaces that with the fixed string `rebon-embedded`, which
  identifies the harness and nothing about you
  ([`runtimes/node/compose-runtime/payload/compose/shims/dsh-anonymous-user-id.js`](runtimes/node/compose-runtime/payload/compose/shims/dsh-anonymous-user-id.js)).
  It rides on `chat/completions` calls you were making anyway; it is not a
  separate request.
- **`npm install` does no network work of its own.** The postinstall script
  only picks the right native binary that npm already fetched.
- **The web UI (`rebon serve`) is fully self-hosted.** No CDN, no external
  fonts, no third-party scripts, and the page sets
  `<meta name="referrer" content="no-referrer">`.
- **Remote control has no built-in endpoint.** `apiBaseUrl` is yours to
  configure; nothing is contacted until you do.
- **The browser extension** has its own statement — no analytics, no
  telemetry, no advertising
  ([`runtimes/node/plugins/rebon-browser/PRIVACY.md`](runtimes/node/plugins/rebon-browser/PRIVACY.md)).

## Where your data lives

All of it stays on your disk, unencrypted and readable by you:

- `~/.rebon/config.json` — providers, models, defaults
- `~/.rebon/.credentials.json` — API keys and OAuth tokens
- `~/.rebon/sessions/` — full transcripts
- `~/.rebon/skills/`, `~/.rebon/agents/`, `~/.rebon/memory/` — your assets
- `$REBON_LOG_DIR/rebon.log` — the TUI log

Nothing here is uploaded, synced, or backed up anywhere by Rebon. Deleting
the directory is a complete uninstall of your data.

## Checking for yourself

The source is here, so audit it rather than trust the list:

```bash
# Every module that can make an HTTP request
grep -rln reqwest --include=*.rs crates services

# Every hardcoded external host
grep -rhoE 'https?://[a-zA-Z0-9.-]+' --include=*.rs crates services | sort -u

# No telemetry SDKs are in the dependency tree
grep -niE 'sentry|statsig|opentelemetry|posthog|amplitude|mixpanel|datadog' Cargo.lock
```

Or run `rebon` under a network monitor with `plugins.updater.enabled = false`
and confirm the process makes no requests until you send a prompt.

If you find an outbound request this document does not describe, please open
an issue — that is a bug, not a feature.
