# crates/REBON.md

Continues the root `REBON.md` "Coding rules"; this file holds only the rules
that apply inside `crates/`.

Terms:
- **Kernel**: `rebon-kernel`, where the plugin model and the registry live.
- **Seat**: an interface point in the kernel that plugins register a
  contribution on (tools, commands, prompt sections, attachment producers);
  consumers obtain it through the kernel's `require`.
- **Plugin plane**: the in-process host that loads external Node plugins
  (`PluginPlane`), started by `ensure_process_composition()` and stopped by
  `shutdown_process_plugin_plane()`.

## What belongs in this directory

Directory layout and naming are governed by the root `REBON.md`, "Directory
layout and naming". That section covers kebab-case, the difference between a
directory name and a package name, and where the JS runtimes and the
standalone services go; it is not repeated here — one fact is written once,
and a copy eventually disagrees with the original.

`crates/` itself holds only the main workspace's Rust crates: an ordinary
crate as `crates/rebon-<name>/`, a Rust feature plugin as
`crates/plugins/<feature>/`.

## Layering

- Dependencies point inward only: surface → assembly (`rebon-harness`) →
  plugins (`crates/plugins/*`) → seats → kernel. Seats belong to the kernel, so
  a consumer must depend on the kernel to `require` one; never let a lower
  crate depend on a plugin.
- The assembly layer is itself split in four, and the direction is a chain, not
  four peers:

  ```
  rebon-harness ──▶ rebon-plugin-host ──▶ rebon-provider
        │                   │                   │
        │                   └───────────────────┴──▶ rebon-kernel-seats ──▶ rebon-kernel
        └──▶ rebon-provider, rebon-kernel-seats, crates/plugins/*
  ```

  - `rebon-harness`: assembly and nothing else — the `builtin_plugin_defs()`
    table (it names every plugin and the other three crates, so it can only
    live here), the boot itself (`kernel_bootstrap`), and `agent_assembly` /
    `session_assembly`. After the split it is 6.7K lines in four files.
  - `rebon-provider`: the provider catalog, the runtime cache, the
    `model-router` seat, the llm adapters plugins contribute
    (`kernel_llm_dispatch` / `plane_model_provider`), and the OAuth token
    refresher. It changes because a provider changed.
  - `rebon-plugin-host`: the Node plugin plane itself (`plugin_plane`), the
    compositions, the manifests, the payload patches, and **when a plane
    should be started at all** (`plugin_boot`, `kernel_node_host`), the three
    pieces of the vendor loop (`loop_host` / `kernel_loop_backend` /
    `kernel_loop_plane`), and `provider_registry`. It changes because the
    protocol or the process changed. `provider_registry` is here rather than in
    `rebon-provider` because an external provider is **a plugin mounted on the
    plane**: whatever can build that registry has to name the plane, and the
    plane in turn names `rebon-provider`. Settle it by dependency direction; do
    not add a callback layer.
  - `rebon-kernel-seats`: the built-in Core seats (tools / dialogs / settings /
    session scope / prompt sections / web / compose tools / `run_code` / the
    built-in slash commands `core-commands` / `tool-asks` + `tool-invoke`). It
    changes because the contract between the kernel and whoever fills a seat
    changed.
- **The process kernel is *stored* in the kernel and *booted* in the assembly
  layer.** The slot is `rebon_kernel::process`: `install_process_registry` /
  `process_registry` / `process_kernel`. `None` has exactly one meaning —
  **this process has not booted yet** — not "the kernel died" and not "ask
  again later". There is one boot:
  `rebon_harness::kernel_bootstrap::process_plugin_registry()`, because it
  needs `builtin_plugin_defs()` first, and that table names every plugin crate.
  It installs only after assembly is done, so `Some` is never a kernel still
  deciding what to load.
- **Whoever reads the slot is handed the kernel.** Read
  `rebon_kernel::process_registry()` only where you **can prove you run after
  boot**, and handle `None` with that seat's existing fail-closed meaning
  (`command_seat()` already returns an `Option`, and readers fall back to the
  built-in table). Everywhere else the caller passes it explicitly:
  `plugin_boot`'s four entry points take `&Arc<PluginRegistry>` (on the desktop
  app the plane start and the kernel boot are two tasks on one runtime and they
  race), `kernel_node_host`'s `PlaneHost` keeps the kernel inside the
  `PluginHost` it got from the factory (plugins are applied *during* boot, when
  the slot is still empty), and `EngineToolInvokeHost` takes the upstream
  `Context` it resolves tools with. No link-time hooks — no ctor, no linkme, no
  inventory.
- **The binary half only says harness.** cli / app call
  `rebon_harness::kernel_bootstrap::process_plugin_registry()` /
  `process_kernel()` (which boot), never `rebon_kernel::process_*` directly —
  those are the reads meant for crates **below** the assembly layer, and the
  `Option` they return answers "has this process booted", a question no
  front-end has a reason to ask. The invariant is written in
  `rebon-harness/src/lib.rs` next to `pub use rebon_kernel`.
- **The session chain is also a chain, not four peers**: `rebon-cli →
  rebon-session-runtime → rebon-session-host → rebon-proto`. `rebon-proto` is a
  leaf (its only rebon dependency is `rebon-types`) and holds the wire
  protocol's codec and envelopes; `rebon-session-host` is "the shared half of a
  session host" — job state, the owner / lease state machine, and the one
  client every surface uses; `rebon-session-runtime` holds the worker and the
  IPC server. The cli reaches this chain through one seam, `crate::session::…`
  (`main.rs` has `pub(crate) use rebon_session_runtime as session`).
- **Two protocols, compatible for one release.** A worker's IPC port answers
  both the old envelope and ACP + `_session/*`, and it decides **per
  connection** on the first frame (the two have different connection lifetimes
  and cannot be told apart per message); the client probes once and remembers
  by `(pid, port, token)`. The old protocol and its six wire goldens are
  deleted together in the next release, and the deletion condition together
  with the compatibility window is recorded in the commit history. While the
  window is open, every change to either side has to answer: does the other
  side still recognise this?
- Untangle a backwards dependency with a new leaf crate or by sinking the type
  into a lower crate (`rebon-session-state`, `rebon_types::SubAgentModelConfig`),
  not with a trait adapter layer.
- `rebon-cli/src/tui/` holds only code that draws the terminal interface. A
  module that uses neither `AppState` nor ratatui is `git mv`'d out whole.
  Session ownership (`HeldSessionLock`) lives in `rebon-session`.
- **One session handle, two parameter structs.** `EngineSession` is runtime
  state (13 fields). What is decided at startup and does not change during a
  turn goes into `SessionStartupParams` in `session/startup.rs`; the resolved
  model/provider goes into `rebon_harness::SessionModel` (which `HeadlessSession`
  shares); the terminal's own startup flags go into `TuiStartupParams` in
  `tui/wiring.rs`. A new field starts with "who writes it, and when": what is
  only written at construction does not go on the handle.
- Keep `EngineSession` (runtime state) separate from view state; `Deref` is only
  a transitional device. The `TuiEngineSession` shell has five view fields, and
  its `Deref` to `EngineSession` exists for the 210 `&TuiEngineSession`
  parameters; deleting it means first classifying each of those as handle /
  parameter struct / view. A `pub(crate)` kept inside `session/` is allowed only
  for tests constructing internal types, with the reason written down.
- **`SessionEngineHalf` does not mirror `SessionRuntime`.** Anything the runtime
  already owns (backend, session_agents, broker, policy, …) is read as
  `engine_half.runtime.x`; the half carries only what is its own, or a handle
  that will re-point itself between two assemblies (the registry `/skill`
  rebuilds, the task table the foreground agent swaps). Mirroring field by field
  is two truths, and the symptom of missing one is "the previous session's
  handle is still answering".
- A background worker holds no interface state: the transcript `/context` and
  `/rewind` need is computed by the pure function `replayed_messages(entries)`.
- **`rebon-session-host` is the session host's shared half, and only surfaces
  may depend on it.** It is five pieces: `state` (the on-disk shape of
  `~/.rebon/jobs/`, pinned byte for byte by `wire_contract_tests`), `store`
  (transactions that stay consistent across processes), `protocol` (the local
  control plane — there is **one**, and the only permitted increment is an
  additive `CancelCall` inside the same enum), `client` (`SessionHostClient` /
  `SessionHostConnection`, the four owner states, leases, subscriptions, the
  prompt ladder) and `legacy_foreground` (the local terminal host's file
  mailbox, kept indefinitely per RFC-0004 §16.23), plus the `supervisor` that
  has moved in. The test is "is this code the host's, or some surface's":
  `gh` fetching PR status and the updater plugin's Windows migration are a
  surface's, so they are injected hooks rather than dependencies. `cargo tree -i
  rebon-session-host` must show only `rebon-cli`, `rebon-app`,
  `rebon-mcp-channel` (the `rebon mcp serve` surface, depended on only by cli)
  and `rebon-rc-runner` (the `rebon rc` surface: it hands a local session to the
  Remote Control server, so it is a session-host client that hosts no session
  itself, depended on only by cli, with its canary in its own `layering`). Any
  lower crate depending back on it is a defect.
- **`BackgroundJobState` is five pieces by "who writes it", and a surface does
  not write across pieces.** `identity` / `process` / `workspace` / `lease` /
  `outcome` each have one writer; the top level offers read-only accessors and
  **named transactions** (`place_in`, `set_linger`, `touch`,
  `publish_local_endpoint`, `set_recorded_owner`). An operation that moves
  several fields is one transaction, not several assignments — the reason
  `RecordedOwnerSnapshot` exists is that pid / identity / port / token are one
  answer. Direct writes to owner / lease fields from the TUI, serve and app must
  stay at 0. `serde(flatten)` keeps the JSON flat; there is no on-disk
  migration.
- When dev-dependencies form a cycle, the engine's tests compile the engine a
  second time, and a trait the engine defines cannot cross the two instances.
  Split a seat's tests in half: the engine side tests the interface boundary
  with a fake rule, the plugin side tests the rule itself.
- For crates with very few dependencies (`rebon-render`, `rebon-dialog`,
  `rebon-picker`) the dependency constraint is a contract: inject a callback
  when an outside capability is needed, and an exception goes on the canary
  test's allowlist with its reason.
- **The render stack is two halves, in one direction.** `rebon-render` is the
  projection: a session message's display state, a tool call's body and diff,
  markdown layout, structured diffs. `ratatui` and `crossterm` may not appear in
  its `Cargo.toml` (a canary test pins this), because the terminal, the GPUI
  desktop app and serve all consume the same projection. The ratatui painting
  lives in `rebon-message-tui`, which depends on `rebon-render` and not the
  other way round. The test is "does this type mention `ratatui::`": if it does
  it is painting, if it does not it is projection.
- `rebon-width` and `rebon-math` are deliberately left as leaves rather than
  folded into `rebon-render`. The first is a hard constraint —
  `vendor/ratatui-0.29.0-rebon` depends on it, so folding it in would make
  ratatui depend on the whole projection crate. The second is a cost: it has no
  consumer inside `rebon-render` (only the painting side's markdown renderer
  and the desktop app call it) and it drags in resvg / usvg / tiny-skia /
  rustybuzz plus the embedded KaTeX fonts, 56 third-party crates, which would
  have `plugins/tasks` touching the render stack for one `format_duration_ms`.
- **`rebon-tui` is the repository's ratatui consumer, and only `rebon-cli` may
  depend on it.** The canary test `only_rebon_cli_may_depend_on_this_crate`
  scans every `Cargo.toml` in both workspaces to pin it. That is also the only
  test for whether a pure-logic crate may be folded in: folding it adds a
  ratatui edge to every consumer it has today. On that basis `rebon-layout` /
  `rebon-input` / `rebon-status` / `rebon-promptinput` became
  `rebon_tui::{layout,input,status,promptinput}` (they were already
  ratatui-free and their only consumer was the cli's terminal half), while
  `rebon-dialog` (five plugins + kernel-seats + ui-seat + app),
  `rebon-customselect` (depended on by dialog), `rebon-picker` (depended on by
  the onboarding plugin) and `rebon-spinner` (called directly by the app)
  stayed, for no reason other than dependency direction. RFC §12 calls those
  nine the "Widget kit", but not one of the nine depends on ratatui — they are
  the **models** in that four-layer table, and merging them only buys the
  removal of a single-consumer crate boundary.
- **Moving a module is not a home for its data shapes.** Before folding
  something into the terminal crate, put the data shapes it carries somewhere
  both sides can reach: `PromptPasteContent` (in `rebon-session-host`, formerly
  `rebon-background`, carried between detach and attach) and `effort_indicator`
  (headless paths such as `rebon exec` read `ReasoningEffort`, and a synonymous
  `EffortLevel` lives here too) both sank into `rebon-types`. The test is
  "would a headless path have to reach for ratatui to get this". A junk drawer
  like `rebon-misc` comes apart the same way: `file_mention` went to
  `rebon-types` (app and cli share it), `context_visualization` to its only
  consumer `rebon-cli/src/session/commands/`, and of `image_link` only
  `file_path_url` had a caller, which went to `rebon_tui::render::file_url`;
  the other nine modules had no consumer anywhere and were deleted.
- **The settings panel has no crate of its own.** The panel state machine and
  the projections for the three built-in tabs are all in `rebon-dialog`:
  `settings_dialog` (tab / selection / editing state), `settings_status`,
  `settings_tabs`, `settings_usage` — four dependency-free pure-logic modules,
  so the contract holds. The permission-mode vocabulary (title / short title /
  symbol / `ExternalPermissionMode` / the default-mode selector) is in
  `rebon_permissions::mode_choice`, in the same crate as `PermissionMode`
  itself; it once lived alone in `rebon-settings`, at the cost of every crate
  using `PermissionMode` depending on an interface crate. `rebon-config` only
  reads and writes the config file; no interface projections go in it.
- `plugins/agents/src/surface/` (formerly the dependency-free crate
  `rebon-agents`) is no longer bound by the zero-dependency contract now that it
  is in a plugin, and keeps two rules instead: no IO (file access through the
  injected `AgentFs`, LLM calls through `AgentGenerator`) and no ratatui (it
  produces `String`s and data structures; the terminal does the painting).
- `plugins/onboarding/src/onboarding/` (formerly the dependency-free crate
  `rebon-onboarding`, allowed only sha2 + base64) is likewise no longer bound by
  that contract, but it is OAuth's decision layer: a new dependency still needs
  its reason written down, and crypto is always `sha2` / `base64`, never
  hand-rolled. IO belongs to the same plugin's `oauth/`, config read/write to
  `store.rs`, and painting to the terminal. The import step `migrate.rs` is a
  pure library that app and cli call directly rather than through a seat — the
  import keeps working with the plugin turned off.
- `rebon-tool` owns the `CommandSandbox` trait and the `session-sandbox` seat
  (`command_sandbox.rs`, which uses only the words "command" and "process" and
  never mentions bwrap / seatbelt / sandbox-win); `plugins/sandbox` provides it
  and the harness's `resolve_command_sandbox` resolves it. A tool that cannot
  get the seat builds passthrough argv itself, which is the same as never
  having installed the feature.
- The workflow runtime (the Boa interpreter, the `agent()` bridge, the
  resume cache, the pre-run static review) belongs entirely to
  `plugins/workflow`; `rebon-tool` keeps only the `WorkflowLauncher` contract,
  the `workflow-launcher` seat and the `WorkflowNesting` nesting policy (the
  `Agent` tool and the spawner use the same depth rule). The cli resolves the
  seat once per session and mounts no launcher when it is empty. The task
  registry's resolver is in `rebon-plugin-tasks` (above `rebon-tool`, so naming
  it directly would be a cycle), and therefore passes opaquely through the seat
  as `WorkflowTaskRuntimeHandle`, retrieved by its single provider; a wrong
  shape is an `Err` at session build, not a panic halfway through.
- The sub-agent and team runtime (`EngineSubAgentSpawner`, the worker loop,
  `SessionTaskTeamManager`) belongs entirely to `plugins/agents`;
  `rebon-coordinator` has been deleted. `rebon-tool` keeps only the two
  contracts `SubAgentSpawner` / `TeamManager`, `ExternalSubAgentRunner` (the
  front-end's implementation, handed to the spawner through a seat) and the two
  seats `sub-agent-spawner` / `team-manager`; the engine and the task registry's
  resolver pass through as `SubAgentRuntimeHandle` the way workflow does. Model
  routing (`AgentModelRouter`, `resolve_sub_agent_model_with_config`) lives in
  `rebon-agent-core::model_router`: both the `agents` and `workflow` plugins ask
  it, and a plugin may not depend on a plugin.
- When the seat is empty (`plugins.agents.enabled = false`) the executor mounts
  no team manager and the front-end's own spawn path gets
  `UnavailableSubAgentSpawner` — every method refuses with the same sentence and
  names the switch, without panicking and without silently doing nothing.
- This seat's fail-closed direction is the opposite of the permission seat's:
  when `settings.json` has `sandbox.enabled` true and `plugins.sandbox.enabled
  = false`, the harness hands out `RefusingSandbox`, Bash / PowerShell all
  refuse, and `/doctor` prints one line saying which switch to change. Only
  turning both off equals the original behaviour.
- **The binary half**: a process entry point that satisfies all three of (a)
  spawned by another process or needing the main thread, (b) starts no kernel
  and reads no session, (c) cannot be expressed as a seat, belongs to the plugin
  crate that owns the feature, declared in its `Cargo.toml` as `[[bin]] name =
  "rebon-<feature>"` with its source in `src/bin/<name>.rs`, doing argument
  parsing plus one lib call and nothing else; it compiles on every platform and
  on an unsupported one prints one line to stderr at runtime and exits 2. There
  is one exception to the name and one to the exit code, both `sandbox-win`: its
  filename is frozen into the install contract (callers look for
  `sandbox-win.exe` in three places), so it carries no `rebon-` prefix and the
  exception table is in `plugin_binaries.rs`; its own failures always use 126
  plus a `sandbox-win:` marker line, because callers use those two to tell "the
  sandbox refused" apart from the child's own error. Locating a binary always
  goes through `rebon_types::sibling_binary` (same directory → `../Resources`,
  never PATH, never a cargo target). A crate under `crates/plugins/<id>` without
  a `PLUGIN` may only be this kind, and the first sentence of its crate
  description says which plugin it belongs to (`crates/plugins/{browser,
  lsp-mcp,sandbox-win}`). The manifest is derived from
  `crates/plugins/*/src/bin/*.rs`, never written by hand, and
  `crates/rebon-harness/tests/plugin_binaries.rs` pins packaging, the bundle and
  the forwarding shim to it.

## One fact, written once

- **Shell tokenizing happens only in `rebon-shell-policy::lexer`**, and no other
  crate may write its own: a second tokenizer is a second set of quoting rules,
  and the gap between the two is where a bypass hides. It holds three
  contracts, not three implementations — `simple_command_segments` is one state
  machine plus a `Grammar` (three constants: the permission rule's argv,
  "are there metacharacters", and static path resolution), while `shell_tokens`
  and `workflow_shell_tokens` are two streams of the static classifier. The
  last two are **deliberately not merged**: `workflow_shell_tokens` breaks `(`
  and `)` into their own tokens, which splits
  `[System.IO.FileInfo]::new("x").Delete()` and blinds the PowerShell deletion
  check (merging turns 4 tests red, one of them a missed detection). The
  `tokenize` in `rebon-tool/src/powershell/guard.rs` is not in this list: it only
  finds a number for `Start-Sleep`, decides no permission and knows no quoting.
  `BashShape`, `parse_bash_shape` and `parse_powershell_shape` live in this
  crate too, and `rebon-permissions` only `pub use`s them in one line.
- A tool's name, aliases, type and path-argument field name live only in
  `rebon-tools-core::BUILTIN_TOOL_FACTS`; classifying a tool as read-only /
  edit / execute queries that table through an injected callback; production
  code does not write literal name lists such as `"NotebookEdit"`. One such
  list is left, and it is the ceiling: a second one is a review finding.
- A tool that declares `file_target_field` must have a `primary_input` with the
  same content, or the interface shows one path while the permission check reads
  another.
- **Path scope checks live only in `rebon-tool/src/path_scope.rs`**: both
  `path_is_within_root` (one root) and `path_is_within_roots` (several) are
  `pub` and plugins call them directly — no writing your own `starts_with` or
  `roots.iter().any`. A second answer to "is this inside the scope" is a second
  set of answers about symlinks, `..` and Windows prefixes, and the gap between
  them is where an escape hides. `mutation_path_is_within_roots` is the same
  check plus a hard-link gate and stays inside the crate. The `is_within` in
  `plugins/sandbox` is **not** in this list: it is deliberately purely lexical
  (its callers canonicalize first, and the Linux backend re-checks with
  `realpath`), and merging it would change its meaning.
- **Refusing a session-credential command happens only in
  `path_scope::refuse_session_credential_command`**, with the predicate shared
  with the classifier (`rebon_shell_policy::is_session_credential_access_command`)
  — "does this command name a live session's credentials" should have one
  answer. Both bash and powershell ask it **before** parsing: being refused for
  naming them does not depend on the input being well-formed.
- Parsing a tool's input and converting the conclusion each happen in one place:
  `rebon_tools_core::parse_tool_input` (deserialize; malformed input is
  `InvalidInput`), `validation_outcome_from` (a parse/check failure → an invalid
  outcome, used by `validate_input`) and `require_valid_input` (an invalid
  outcome → `InvalidInput`, used by `call`). The last two are inverses and the
  round trip loses nothing. A tool's parse-and-check prelude goes into one
  `prepare_input` that hands `call` an already-parsed request — but **do not
  cache what `validate_input` produced for `call`**: `PermissionBroker::resolve`
  runs `decision.updated_input.unwrap_or(input)`, and hooks, front-end replies
  and `AskUserQuestion` answers all rewrite the input there, so the two are
  allowed to differ.
- `TaskStatus` (a background job's status) and `TaskListStatus` (a checklist
  row's status) are two different domains, both defined in `rebon_types`;
  `PermissionBehavior` is defined in `rebon_tools_core`, and hooks use their own
  `HookPermissionBehavior` with a `From` in both directions. Types that share a
  name but not a meaning get their domains separated before any merge; a
  cross-crate conversion is a `From`, not a field-by-field copy function.
- The shape that `session/request_permission` round-trips lives only in
  `rebon-proto`: `PermissionOptionKind`, `PermissionOption`,
  `RequestPermissionParams`, `RequestPermissionResult`,
  `RequestPermissionWireResult` and `PermissionOutcome` are each defined once,
  and `rebon_core::permission::PermissionOptionKind` is a `pub use` of one. The
  option-id-to-kind bucketing is also single (`rebon_core::permission::option_kind`),
  shared by the ACP path and the TUI typing channel, so a new option id cannot
  count as "allow" on one path and "deny" on the other. The JSON on the wire is
  pinned byte for byte by four goldens in `rebon-proto`. Content blocks, tool
  calls, session-updates and slash-commands are **not** wire types: the engine,
  the session store and the render stack all use them as internal values
  (`ContentBlock` appears in 105 files), so they stay in `rebon-types` and
  `rebon-proto` re-exports them.
- Known duplicates not yet merged: `PLAN_LEDGER_TOOL_NAME`; and the permission
  options' **wording** (`rebon_core::permission::option_label` asks the
  `permission-rules` seat first, while `permission_option_label` in
  `rebon-core/src/lib.rs` carries four exit-plan-mode cases) — the two behave
  differently, so decide ownership before merging. `is_likely_wide_in_terminal`,
  terminal-symbol padding and `terminal_char_width` live only in `rebon-width`.
  `truncate_to_width` is down to six copies in two families: the four that go by
  display width with a single-cell `…` have been folded into
  `rebon_width::truncate_to_ellipsis` (closed at the strictest of them —
  `terminal_char_width` measures the character and a zero width returns an empty
  string), and the same crate's `truncate_to_width` is the three-dot layout
  variant, used only by `plugins/agents`' panel; the remaining five that go by
  character count with `…` (three in `plugins/tasks`, `rebon-picker`,
  `rebon-render`) mean the same thing but have not been merged across crates —
  decide where they live first.
- The built-in command table stays as data in
  `rebon-slash-commands/src/builtin.rs`; when the seat is not loaded, `all` /
  `find` fall back to that table (at log level `debug`).
- Who owns a command follows from whether it should disappear when that thing is
  disabled, and the test is **which crate the implementation lives in**. An
  implementation in `rebon-cli` / `rebon-harness` / `rebon-core` stays in the
  built-in table; one living in a plugin crate is registered by that plugin on
  the command seat (`/profile` to `plugins/profile`, `/agents` to
  `plugins/agents`, `/update` to `plugins/updater`; `/onboarding` and `/migrate`
  to `plugins/onboarding`; `/tasks`, `/workflows` and `/teams` to
  `plugins/tasks`; `/skills` to `plugins/skill`, `/memory` to `plugins/memory`,
  `/sandbox` to `plugins/sandbox`). Commands that operate the plugin plane
  itself (`/plugin`, `/runtime`, `/kernel`) are kernel commands. Dialogs work
  the same way, registered on the `ui-registry` seat (`/memory`, `/skills`,
  `/agents`, `/tasks`, `sandbox`). `CommandHandler::Native(id)` is a declaration
  about the front-end's dispatch table and does not care who registered it — the
  equivalence test in `native_commands.rs` compares handler shape and does not
  filter by owner, so a move touches only the registrar and leaves the front-end
  dispatch arm and the command name alone.
- Moving a command carries `zh_aliases` / `aliases` / `hint` / `kind` /
  `category` / `surfaces` with it, word for word: they decide `/help` grouping,
  the Chinese aliases and which surfaces see it, and dropping one does not fail
  the build — it quietly removes a command from one surface.
  `core-commands`' `surface_bits_match_the_lists_they_replaced` reads the
  **seat** rather than the built-in table precisely so a dropped field is still
  caught after a move.
- **What `rebon-session-runtime` exposes as `pub` is its API surface** to the
  surfaces: keep only what a surface actually names, and make everything
  internal `pub(crate)`. When a test needs to construct an internal type, use
  `#[doc(hidden)] pub` behind a `test-support` feature — **a behavioural branch
  must never depend on `cfg(test)`**. Across a crate boundary a dependency is
  **not** compiled with `cfg(test)` when its user is under test, so the branch
  inside `#[cfg(test)]` silently takes the other path. That is exactly how 25 of
  `rebon-cli`'s `/new` and resume tests ended up on the wrong branch: it
  compiled, and only the assertions failed. Expect moving such a branch behind
  a feature to surface things a reader had never been shown: they were always
  there, hidden inside `cfg(test)`, not newly broken. **The `cli → rebon-session-runtime →
  rebon-session-host` layering is pinned by three canaries**: in
  `rebon-session-runtime`, `layering::the_session_runtime_cannot_reach_the_terminal`
  (may not declare `rebon-tui` / `ratatui` / `crossterm`) and
  `layering::only_rebon_cli_may_depend_on_the_session_runtime` (scans both
  workspaces' manifests; the app is a session client and may only depend on
  session-host); and in `rebon-session-host`,
  `layering::the_session_host_names_nothing_above_it` (may not declare the four
  names above it). A canary lives in the crate it protects, so whoever adds the
  edge turns it red on the spot.
- Whether `provider_runtime_cacheable` holds is decided in one function next to
  that field.

## Seats and the plugin plane

- Use `fork_scoped` when a separate namespace is needed; `fork` shares the
  service registry with its parent. A session context and the plugin plane are
  sibling forks, so visibility across subtrees goes through explicit passing,
  stated at the call site.
- Session scope has one host seat, `session/scope`: the host provides it before
  it emits `SessionOpened`, mounted on the `fork_scoped`.
- When a plugin registers, register with the host first and the routes second,
  and withdraw in the opposite order; a failed route registration withdraws the
  host registration already made. Invariants are held by ordering, not by
  polling.
- Services and commands registered at runtime can only be queried through
  `plane.registered_services` / `registered_commands`; the `provides` in a
  snapshot is a compile-time declaration and does not include runtime
  registrations.
- A duplicate command name fails the whole plugin load (built-in commands win);
  a duplicate service name logs a warning and skips that service.
- A `CommandSpec`'s handler is a synchronous function and calling a plugin needs
  an await: the proxy `block_on`s on a dedicated thread, holding only four
  handles — supervisor, scope, runtime and plugin id — and never the plane
  itself. In the TUI, command expansion runs on a blocking worker and the next
  frame collects it with `drain_command_expansion`; the expanded text is
  submitted through `submit_expanded_text` and does not go back through command
  parsing.
- `CommandHandler::Prompt` returns a `Result`: a timeout is an `Err`, and
  `[HOST_UNANSWERED]` text is never treated as an expansion result;
  asynchronous results carry a monotonically increasing id.
- `call/cancel` is only a request — the plugin still sends its final reply, and
  the side that timed out releases its own wait. Call APIs are offered with and
  without a timeout, in pairs.
- When `load_entry` asks the compose control plugin for a report: a module the
  host loaded itself (the kind exporting `activate`) is not in the Cordis realm,
  so `report_for` necessarily answers `[UNKNOWN_PLUGIN]`. That error code (the
  `Rejected` variant) means "no extra report", and registration continues
  normally; any other report error takes the same withdraw + unload path as a
  failed registration.
- What a plugin provides is whatever `PluginReadyReport.llm_providers` /
  `commands` says; one that does not meet the requirements is refused and
  unloaded at load time, not at bind time.
- On a shared plugin host each client uses its own scope (`provider:<id>#N`).
  Stream translation goes in the forwarding task, not in `poll_next`, and so
  does deciding whether a stream was interrupted.
- A failed plugin-plane start is recorded rather than cleared and retried on
  every call; the start has a timeout, `PLANE_BOOT_TIMEOUT`. Shut the plugin
  plane down before the process exits, in one place at the end of `async_main`
  for every entry point.
- When deprecating a protocol variant (for example `transport:
  {"type":"stdio"}`), keep deserializing it and refuse at `validate` /
  `materialize` with the replacement spelling.
- Disabling a plugin also hides the panels it provides — say so in the release
  notes. Process-level ownership uses `OnceLock`, not `Box::leak`.
- Tool descriptions a plugin provides are built on demand and cached by the
  generation of the process-level `AgentRegistrySelection`, rather than fixing
  one `Vec<Arc<dyn Tool>>` at startup.
- Each plugin owns exactly one section of the `settings.json` chain,
  `plugins.<id>`, and the `enabled` key belongs to the kernel. Declarations go
  through `PluginMeta::settings` (Rust; the kernel declares on the plugin's
  behalf after the whole batch is applied) or the manifest's
  `kernelPlugins.<id>.settings` (Node; the plane declares before `plugin/load`)
  — always by the host, because the id *is* the namespace and a plugin
  declaring its own could declare someone else's. Reads merge layer by layer per
  key (user → project → local) and fill in the declared defaults; writes land
  only on the user layer. Crossing a namespace, `enabled`, an undeclared key, a
  type mismatch and having no identity are all five an `Err`, not a silent
  no-op.
- The caller's identity is the host's answer: `KernelSeats::call` overwrites
  `callerPluginId` on every `seat/call`, so a Node plugin cannot forge it; an
  in-process Rust plugin fills it in itself (it could write the file directly
  anyway) and binds the id once with `PluginSettings` — that guards against a
  typo, not against malice. A seat call may only be made from a handler's `ctx`;
  `activate`'s `api` has no `seat`.
- `ConfigChanged.namespace` has a value only when the write went through a seat
  and touched exactly one namespace; `None` means "something in this file
  changed" and subscribers must re-read. Do not get the order backwards:
  `is_mine` returns true for `None`.
- `kernel_tool_asks` is the suspend-and-answer surface for a plugin's tool
  calls. The plane provides `tool-asks` on the `fork_scoped`, where the root
  context cannot see it, so the front-end's reply goes through a process slot
  (`set_process_tool_asks` / `process_tool_asks`, cleared by an identity-guarded
  effect on the plane's fork) and the subscription goes through the process
  kernel (the events are a global broadcast, and the front-end exists before the
  plane does). The front-end installs exactly one, in
  `SessionBound::build_permissions` — the channel the TUI, the background worker
  (relaying over IPC to the app) and `rebon exec` all share. It is fail-closed
  throughout: a cancel, a dropped sender, an option id the seat never provided,
  and a dashboard torn down mid-flight all count as a refusal, and an ask nobody
  saw is still backstopped by a 120-second deadline. An ask has only allow and
  deny, never `AllowAlways` — nothing is stored on this surface, and standing
  answers belong to `kernelPlugins.toolGrants`.

## Engine, attachments, permissions

- `rebon-core/src/attachments.rs` keeps only the seat and the scheduling; each
  attachment producer lives in its own plugin. When one struct is used by
  unrelated callers, split it.
- Attachment producers come in two groups, `before_session` and `after_session`,
  ordered by `Order`; before changing the order, compare byte for byte what the
  model actually receives. The binding information carries a set of
  fine-grained optional handles.
- Before deleting a trait's default implementation, prove that no implementation
  depends on the difference between the layers.
- What `PermissionMode::Plan` forbids comes from the prompt; the `Deny` in
  `mode_policy.rs` is only for display in the settings interface and takes no
  part in the execution decision.
- The "must ask the user" list in the permission decision holds only calls that
  genuinely need the user to decide. A tool that is read-only, takes no input
  and asks for a stricter mode (`EnterPlanMode`, say) does not go on the list; a
  mode that did not take effect is a defect on the write path, and papering over
  it with a forced dialog puts back the confirmation auto mode promised to
  remove. A root-cause fix and a backstop gate do not go in the same commit —
  once the cause is fixed the gate is pure cost.
- The permission-rules seat is fail-closed: an extension may only add dialogs,
  an invisible seat equals the original behaviour, and an unknown option id is
  treated as `RejectOnce`. When an attachment producer goes away it stops
  producing new content only; recording notifications such as
  `notify_plan_mode_tool` / `finish_turn` are still forwarded.
- Policy events have one entry point: `PolicySources::emit(payload)` in
  `rebon-core/src/policy_seat.rs`. Neither the engine nor any surface runs a
  hook runtime of its own any more; `rebon-hooks` is the first subscriber on
  that seat. Events are either **gating** or **notification**, and the test is
  "does the emit site have a branch a refusal could land in today", not whether
  it sounds like a gate; the classification table is in `class_of` and nowhere
  else. Gating events are fail-closed: an explicit refusal, a timeout and a
  panic are all refusals, and a refusal short-circuits immediately so later
  subscribers are not asked. Notification events have no refusal branch, and
  pretending they can refuse is a lie — there, log it, drop that subscriber's
  output for this round, and carry on with the rest.
- The seat's channel for changes is `HookEffect`; do not invent a second patch
  type. The projection function that turns an effect into each event's local
  verdict is already written in `hooks.rs` and pinned by tests, and reusing it
  is what makes "configured hook behaviour is unchanged" one piece of code
  rather than two that look alike. For the same reason, effect-level
  interception (`BlockToolCall` / `BlockStop`) does not short-circuit at the
  seat; it stays with the emit site's projection.
- Session-level subscribers go in the handle's `local` and plugins go on the
  process seat; the calling context (cwd / transcript / session id) belongs to
  the handle and subscribers read it off the request rather than keeping a
  second copy. A sub-agent uses the same handle with an agent label, so a guard
  the user configured also covers delegated tool calls.
- The handle has one constructor: `policy_sources_for_session` in
  `rebon-harness/src/session_assembly.rs`. Copying the subscriber list
  elsewhere is how one path quietly stops running something — it has happened
  once already (the copy for session rebinding of the directory lost the process
  seat). An executor serving one session uses `with_policy`; one serving several
  (`--acp` / `serve`) uses `with_policy_resolver`. They are mutually exclusive,
  because the handle carries whose session this is.
- Every session with a session id gets a temporary directory in the system
  prompt, isolated by project and session (`system_prompt::scratchpad_dir_for`),
  and that same path is set as an auto-allowed write root on the `ToolContext`
  and created ahead of time. Telling the model the path without allowing it
  means every write hits an authorization dialog.
- Bash's authorization-free deletion holds in exactly one case: the whole
  command does nothing but delete, and every target statically resolves inside a
  write root (`shell_command_only_deletes_inside_roots`). Anything chained onto
  it, a dynamic or globbed target, and a path that leaves the root through a
  link all keep the dialog. The static resolution of deletion targets exists
  once, in `rebon-tools-core`, called by both the sub-agent's broker and the
  main session's Bash.

## cli / TUI

- The event loop is stage functions plus `LoopState`; command dispatch looks up
  `native_dispatch(id)` in a table, with any semantically ordered steps written
  before the dispatch and commented; dialogs all go through `DialogStack` +
  `ViewSpec`. There is no ratatui dialog painting anywhere in
  `rebon-cli/src/tui/` — it is all in `rebon-tui::dialog_view`'s four generic
  painters (`List` / `Outline` / `Search` / `Panel`), and `ViewSpec` has no
  native escape hatch: a panel is always described declaratively, and a
  plugin's own panel takes the same path. The one exception is where it is
  stored: the `/tasks` panel still hangs off `AppState` rather than
  `DialogStack` (a dozen layout and key-dispatch sites read it directly), but it
  too only describes a `ViewSpec::Panel`; the render layer writes state back by
  explicit passing, not through a `Cell`.
- Pure parsing and pure formatting go in `session/commands/`. Before starting
  such a project, count how much code actually mixes parsing with the interface;
  do the smallest instance of a change first and measure the real cost before
  committing to the batch.
- The usage ledger `UsageLedger` hangs off `engine_half`, the accumulation logic
  exists once, and no copy is kept and written back; the snapshot it hands out
  returns owned values.
- An interface struct's fields state what "empty" means. `ui_mode` has two
  sources and is passed explicitly. Moving assembly code does not change when
  each switch takes effect.
- The global `settings.json`'s model key may not override the provider's choice;
  an invariant written in a code comment outranks a verbal instruction.
- `SessionAssembly` expresses build order as type state. When the TUI, ACP and
  headless paths share code, look at the behavioural difference, not at the
  number of duplicated lines.
- Before moving assembly code, list the objects whose drop timing is meaningful:
  locks, channel receivers, held handles, guards, `cron_scheduler`. Before
  moving a file, grep the whole repository for `include_str!`.
- Reclaiming session ownership and rebuilding the cron scheduler go in one
  function; a cross-directory resume stops before it starts; releasing the lock,
  `release_mcp` and `stop_cron_scheduler` happen together and are pinned by a
  test that checks the end state.
- An object reused across turns is lent and returned explicitly with `take_for`;
  a terminal snapshot is not discarded along with the registry.
- A prompt template's placeholders need a test that exhausts the substitutions.
- A session id is only a name: its shape is defined once in
  `rebon_types::session_id`, which offers only `new_session_id`. No time goes
  into an id and no time is read out of one. Creation time is session data:
  `rebon_session::create_session_on_disk` writes `createdAt` into the sidecar
  once when the session is created and never changes it. The read order is the
  sidecar's `createdAt`, then the transcript's first-line timestamp, then the
  file date (modification time when creation time is unavailable), so that no
  session is left without a date.

## Verification

- The plugin-plane tests need `REBON_TEST_NODE` (Node ≥ 24.19); when running the
  cli's full test suite, run it once more with that environment variable set.
  Integration binaries live where the thing they test lives:
  `kernel_code_mode_js` and `plane_fork_publishes_tool_registry` in
  `rebon-kernel-seats`, `kernel_seats_plane` in `rebon-plugin-host`, and the
  rest (those needing `builtin_plugin_defs()` or a plane start) in
  `rebon-harness`. The package names in `cargo test -p <package> --test <name>`
  in `.github/workflows/release.yml` follow.
