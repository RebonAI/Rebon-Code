# DeepSeek Harness client plugins in Rebon

A DeepSeek Harness (dsh) package can have two halves. The **host half** is a
Cordis plugin, and Rebon already loads it unchanged on the plugin plane
(Verve plus native Cordis). The **client half** is a React 18 bundle,
`lib/client.js`, that dsh runs in a browser page next to its host. It
contributes UI to named slots and talks to the host through `ctx.remote`.

This document covers four things:

- the client contract as upstream dsh defines it (source version
  0.2.1-rc.3);
- where each part of it lands in Rebon;
- the host side of that contract, which Rebon now answers;
- what is still needed to run client halves inside the desktop app.

References are to the upstream repository (`deepseek-ai/deepseek-harness`,
`packages/...`) unless they name a Rebon path.

## 1. How client halves are found and loaded upstream

### Declaration

A package declares its client half in `package.json`:

```json
"exports": { ".": "./lib/index.js", "./client": "./lib/client.js" },
"dsh": { "client": { "platform": "web", "inject": ["@deepseek-ai/dsh-client-ui-slots"],
                     "immediately": false, "external": [] } }
```

The fields are typed in `util/package-manifest/src/types.ts`
(`DshClientManifest`):

- `platform` is always `web`, the only value anything reads.
- `inject` names packages. It only orders when their bundles arrive in the
  page. A name that is not installed is skipped.
- `immediately` prefetches the bundle in the first stage.
- `external` lists the module requests the bundle needs beyond the
  platform's own set.

The real dependencies are the client module's own Cordis `inject`. For
example, `dsh-client-ui-settings-web-search` injects `slots`, `locale`,
`remote`, `remote.credentials` and `configForms`.

The host half is the package root. It is often an empty `apply`, and it
exists so that the profile's Loader mounts the package: **a client half is
served only while its host half is mounted and active.**

### Discovery and serving

`ClientModuleRegistry` (`client/modules`, `ctx.clientModules`, which injects
`loader`) walks the Loader's live entries and keeps one row per package that
has `dsh.client`. From those rows it builds a `WebBootGraph`:

```ts
WebBootEntry { id /* package name */, url, rev, inject?, immediately?, external? }
WebBootGraph { rev, release?, entries, batches }
```

`rev` changes with the bundle's mtime, ctime and size. The page receives the
graph as `window.__DSH_BOOT__`, through the `webserver/index-inject` event.

Bundles are served from `GET /plugins/??<pkg>/client.js,...&rev=`. When an
entry is enabled or disabled, the change is pushed to the page over the SSE
channel `/plugins/events`, as `{type:'graph'}` and `{type:'rebuilt'}` frames.

### Module loader

`window.__ModuleLoader__.load({ id, factory: (require) => exports })`
registers a factory. Running a bundle does nothing more than that; the
factory itself runs once, the first time something requires it.

`require` resolves in this order:

1. the platform seed table;
2. memoised records;
3. graph rows;
4. registered factories.

Factories add their CSS with `document.createElement('style')`, and the
system claims those tags as `style[data-plugin=<id>]`.

The **platform modules**, bundled into the shell and handed to every factory,
are `react`, `react/jsx-runtime`, `react-dom`, `react-dom/client`,
`@deepseek-ai/cordis`, `zod`, `@tanstack/react-virtual`,
`dsh-client-store`, `dsh-client-ui-slots`, `dsh-client-ui-primitives` and
`dsh-client-ui-dockkit`.

### Boot

The shell is `dsh-client-web`, published built as `dsh-web-frontend`. It
starts like this:

1. `__ModuleLoader__.create({ boot, staticModules, loadBundle?, resolveAsset? })`.
2. `new Context()`.
3. Mount `cordis-plugin-loader`, which is vendored in the shell.
4. `loader.create({ name: id })` for each graph row. Client entries get no
   config.
5. `loader.await()`. An entry still pending at this point is fatal.
6. `uiRenderer.mount(container)`.

## 2. Slots, and where they land in Rebon

Slots belong to `ui-renderer` (`ctx.slots`); `ui-slots` holds only the
types. A contribution is written like this:

```ts
ctx.slots.inject(key, () => ctx.slots.register({ name, id?, order?, label?, priority?, select?, inject? }, Component))
```

Each key has a cardinality: `single`, `list`, `keyed` or `chain`. It also
has a scope: `root`, `session-maybe` or `session`. Components never receive
`ctx`. They get the owner's props, the standard hooks (`useSessions`,
`useSession`, `usePanelInfo`, …), `useStore` and `t`. The full per-key
contract is generated in
`extensions/cordis-client-runner/src/client/slot-catalog.ts`.

The table lists the keys a Rebon host has to own.

| Upstream key(s) | Upstream owner | What it is | Rebon surface |
|---|---|---|---|
| `plugins.item` (`{view:'summary'\|'page', form?}`, `{id, order, label}`) | ui-plugin-manager | an official plugin's card and its settings page | Settings → Plugins: the card body and detail page of the package's row |
| `plugins.row.config`, `plugins.bundle.config`, `plugins.detail.{actions,badge,section}` | ui-plugin-manager | extra per-row, per-bundle and per-detail UI | Settings → Plugins detail page |
| `settings.section`, `settings.general.item`, `settings.models.provider-card`, `settings.models.footer` | ui-settings-* | settings pages and cards | Settings window: a page per section; provider cards under Models |
| `sidebar.panellist`, plus the keyed `main` panel for that id | ui-sidebar, ui-layout | a global panel: an icon, and the main view it opens | a left-sidebar entry and a workspace pane (cmux pane) |
| `sidebar.brand.mark`, `sidebar.brand.name`, `conversation.hero.brand.mark` | ui-sidebar, ui-conversation | brand marks | **not mapped**. Rebon keeps its own brand, so a brand plugin loads and contributes nothing visible |
| `sidebar.session.row.{leading,trailing,hover}`, `sidebar.workspaces.session.menu.item` | ui-workspace | adornments and actions on a session row | Rebon's session list rows and their context menu |
| `rightbar.session`, `sidebar.right.pane.tab*` | ui-sidebar-right | a session's right-hand panes | a session side pane next to the transcript |
| `conversation.chat.node` (keyed `tool-call`, `assistant-step`, …), `tool.call.toolview` (keyed by tool name) | ui-chat, ui-tool | how one transcript node or tool call renders | Rebon's transcript: a plugin tool's call card |
| `conversation.input.{left,right,dock,attachments,model,…}`, `conversation.composer.*` | ui-conversation | composer furniture | the composer bar |
| `conversation.header.*`, `conversation.session.header.*` | ui-conversation | the session header | the conversation title bar |
| `shell.overlay`, `shell.bottom`, `shell.leading`, `shell.quota-notice` | ui-layout, ui-chat | shell-level chrome | overlays and toasts; the bottom status area |
| `conversation.approval.detail`, `conversation.plan-review.actions` | ui-approval, user-questions | permission and plan UI | **not mapped**. Rebon owns permission prompts |

Rebon owns the slots and supplies them to client halves: their owner is
Rebon's UI, not dsh's shell. A key with no Rebon surface is accepted and
renders nowhere. That is the same thing dsh does when a key's owner is not
mounted.

## 3. What a client `ctx` offers

These are the services a client module can inject, and who provides each
one upstream:

- `remote` and `remote.<ns>`: the api-gateway client half (described in §4).
- `slots` and `uiRenderer`: ui-renderer.
- `configForms` and `settingsSchema`: ui-settings. `configForms.get(entryId)`
  returns a form with:
  - `getSnapshot() → {status, value, base, user, revision, writable}`;
  - `subscribe`;
  - `mutate(ops, rev?)`;
  - `set(field, value)` and `unset(field)`.

  `configForms.whileServed(namespaces, register)` registers only while those
  namespaces are served. Under it is a **settings mirror**:
  - one `remote.settings.describe()`;
  - re-read on the event `settings/document-updated` and on `connection/reset`;
  - writes go through `settings.mutate(ns, ops, expectedRevision)`, one at a
    time per namespace;
  - a refusal reloads.
- `locale`: client/locale. It offers `bind(ns)`, `register(ns, {zh, en})`,
  `getSnapshot`, `subscribe` and `setLocale`. The chosen locale persists in
  the settings namespace `locale`.
- Stores: `dsh-client-store` is a platform module offering
  `createSnapshotStore`, `defineStore` and `shallowEqual`.
- Session and workspace views: `sessions`, `workspaces`, `jobs`,
  `conversation`, `layout`, `uiSession`, `sidebarRight`, `theme`,
  `shortcuts`, `fileUpload` and others.

## 4. Remotes: how a client reaches its host

Host services are `TypertRemoteService`s with `@Remote` methods. Their client
stubs are generated per package (`lib/typert.remote-client.js`). The client
half of `dsh-api-remotes` mounts 25 namespaces statically, so every method
exists on the client whether or not any host answers it.

**Unary calls.** A call is `rpc.call('/api', '<ns>/<method>', { args })`.
`args` carries the named wire fields, with undefined fields left out. The
answer is always one of:

- `{ ok: true, value }`
- `{ ok: false, error: { code, message, details } }`

It never rejects for a carrier fault. Error codes come in two groups:

- the gateway's: `gateway/method-unavailable`, `gateway/bad-request`,
  `gateway/internal`, `gateway/cancelled`, …
- the domains': `settings/conflict` (details `{ns, expected, actual}`),
  `settings/rejected`, `credential/rejected`.

**Streams and events.** Upstream, both travel over the WebSocket
`/api/remote.mux`, as `open`/`item`/`end`/`cancel` frames.

- Host events reach the client on the logical stream `$events`: first
  `ready`, then one `{type:'emit', event, args}` per event, for events on an
  allowlist (`api/remotes/src/remote-events.ts`).
- Waterfalls (`approval/request`, `user-questions/request`) are answered
  through `$events/result`.

**The carrier seam.** `window.__DSH_TRANSPORT__ = { rpc: { call, open? },
loadBundle?, resolveAsset?, ownsHost? }` (`client/connection`) replaces HTTP
entirely. dsh's own test carrier, `RemoteMock`, works this way. This seam is
how Rebon carries remotes: no web server and no WebSocket, only the plane.

### What the sample client halves call

| Package | Host calls | Events |
|---|---|---|
| `dsh-client-ui-settings-web-search` | `settings.describe`, `settings.mutate`, `credentials.describe`, `credentials.set` | `settings/document-updated`, `credentials/reference-updated` |
| `dsh-client-ui-plugin-manager` | `pluginManager.*`, `pluginInventory.list`, `pluginRegistryProbe.fastest` | `plugin-manager/*` |
| `dsh-client-ui-brand-official` | none | none |

## 5. The host side in Rebon

The plane is the host, so the plane answers the contract. It has two
halves.

- **Rust**, `PluginPlane::call_remote(endpoint, args) -> Value`
  (`crates/rebon-plugin-host/src/plugin_plane/remote.rs`). It takes dsh's
  endpoint and named wire arguments and answers in dsh's wire shape, never
  with an `Err`. Facts that live in the plane are answered here. Facts that
  live in a realm are asked of the composition's `compose` service, as
  `{ kind: 'remote', endpoint, args }`. Each host holds its own realm, so:
  - a settings write goes to the host serving that namespace's entry;
  - a listing is gathered from every host, shared and contained.
- **JS**, `runtimes/node/compose-runtime/src/remote.mjs`, for what a realm
  holds.

| Endpoint | Answered by | What it does |
|---|---|---|
| `settings/describe` | realm, gathered | Upstream `@deepseek-ai/dsh-settings` (`SettingsForms`) over Rebon's configuration editor. Reads are redacted (`role('secret')` fields never ride an answer) and come in dsh's `SettingsNamespaceView` shape. `gateway/method-unavailable` if no settings service is mounted. |
| `settings/update`, `/replace`, `/mutate` | realm of the entry `ns` | The same service's writes, with dsh's revision fence. A stale revision is `settings/conflict`; any other refusal is `settings/rejected`. |
| `credentials/describe` | plane | `{configured, source?, writable:false}` per reference. Rebon reads only whether the variable is set and never its value. At most 64 references, each an environment-variable name. |
| `credentials/set`, `/unset` | plane | `credential/rejected`. Rebon's credentials are set in Rebon, not from a page. |
| `pluginInventory/list` | realm, gathered | The mounted entries, with their package names and fiber phases. `managementAvailable: false`. |
| `pluginManager/listPlugins` | plane | The loaded entries as dsh `PluginInfo`, `readOnlyReason: "management-required"`. The mutating methods are `gateway/method-unavailable`, because Rebon configures its composition in its own plugin settings. |
| `rebon/clientModules` | realm, gathered | Rebon's counterpart of `__DSH_BOOT__`: each mounted entry whose package has a web client half, as `{entryId, id, file, rev, inject, immediately, external}`. `file` is the bundle on disk, for the client realm's `loadBundle`. |
| anything else | – | `gateway/method-unavailable`, as dsh's gateway answers an endpoint no live service claims. |

**Events.** The composition's control plugin forwards the realm's
`settings/document-updated`, `credentials/*-updated`, `llm/adapters-updated`
and `commands/change` onto Rebon's event plane, on the topic
`compose:remote-event` (`plugin_plane::REMOTE_EVENT_TOPIC`)，内核订阅仍可读取
`{event, args}`。client carrier 现在可直接调用
`PluginPlane::open_remote("$events", {})`，取得 `RemoteEventStream`，无需自行拼装帧：

- 首项固定为 `{type:'ready', clientId, host:{home}}`；`home` 与共享 realm 的
  `profileContext.home` 一致，每次连接的 `clientId` 不同。
- 后续 `recv().await` 返回 `{type:'emit', event, args}`。事件与 unary 调用共用现有
  plugin-plane NDJSON 协议，不启动 HTTP/WebSocket 服务。
- 仅已认证的 composition control plugin 能向该流发布；普通插件获准发布同名
  topic 也不能冒充 Host。每个 plane 独立广播，不会因 scope 同名而串流。
- 丢弃流或调用 `close()` 即取消订阅；plane `shutdown()` 会结束流，关闭后不能
  新建订阅。慢客户端超过 256 项缓存时得到一次 `gateway/internal` 错误后结束，
  必须重连并重读 settings 等快照，不能静默漏掉 invalidation。
- 未提供的 stream（包括权限 waterfall）明确返回 `gateway/method-unavailable`。

`__DSH_TRANSPORT__.rpc.call('/api', endpoint, {args})` 的 Host 端映射是
`call_remote(endpoint, args)`；`rpc.open('/api', '$events', {args:{}})` 映射到
`open_remote`。流中 `Ok(frame)` 是直接交给上游的 item，`Err(envelope)` 使用
与 unary 相同的 `{ok:false,error}` 结构；carrier 应以该 `error` 结束流。

**Settings are entry configs.** This is dsh's model, kept as it is. A
settings namespace is a composition entry's id. Its form is the entry's
`.volatile()` config fields.

The configuration editor's `edit` works in this order:

1. It checks the next config against the plugin's own schema.
2. It persists the config as that entry, through the `settings` seat's new
   `entryConfig` method. That method replaces only the caller's own
   `kernelPlugins.plugins` row in `config.json`
   (`rebon_config::set_kernel_plugin_entry_config_in`).
3. It commits the volatile fields into the running plugin's references,
   without a remount, and announces `loader/volatile-update` to that plugin
   alone, as dsh's loader does.

A change to an ordinary field waits for the entry to be loaded again.

**The boot services behind it.** dsh's settings package injects
`configEditor` and `profileContext`, and reads `ctx.root.loader`. In Rebon
the composition is the loader, so the realm answers all three from its own
entry table (`compose-runtime/src/loader-runtime.mjs`). It does not mount a
second loader.

**Tested without a DOM.**

- `compose-runtime/test/dsh-host.test.mjs` drives the realm half with small
  local packages.
- `crates/rebon-session-runtime/tests/dsh_upstream.rs` 安装真实、未改写的 npm
  包并启动 Verve + native Cordis plane。它调用 `settings/describe`、
  `settings/mutate`（持久化且实时改变后续 search endpoint）、
  `pluginManager/listPlugins`、`pluginInventory/list`、`credentials/describe`，
  并通过 `open_remote("$events", {})` 检查 ready、settings 更新和关闭，而不只观察
  内核 topic。
- `plugin_plane/remote.rs` 的单元测试覆盖 plane 自有 namespace，以及多 client
  广播、跨 plane 隔离、来源认证、取消、参数拒绝、慢消费者丢失和关闭。

**真实 npm 验收的依赖边界。** DSH 集成依赖（depends on）
[Quorafind/verve#6](https://github.com/Quorafind/verve/pull/6) 的模块解析、
原生扩展拒绝与 resource timing 修复；该 PR 尚未 merge。生产 workspace 的
Verve 与 7 个 Boa patch 仍使用原有 git `main`，正式 `Cargo.lock` 保持原有来源，
不包含这次 test-only 替换，也不提交机器专属路径。

现有 `release.yml` 的独立、必跑 macOS job `dsh-integration` checkout 当前
RebonCode 与 Verve 精确 commit `a9fc5f6e15bec3d11fe7f3db5c034ae5ac3c41aa`。
它仅在 runner 临时 Cargo config 中将 Verve 与 7 个 Boa 指向同一副本，使用独立
构建目录，构建 `rebon-verve` 后依次执行 Host lib、真实 npm `dsh_upstream`
ignored、DSH base profile ignored 与 contained-plugin 边界测试。临时 patch 图允许
更新该 job 的临时 lockfile，不使用 `--locked` 硬卡正式 lock；正常 `verify` 保留
Host lib 测试，但不执行这两条依赖 #6 的 npm 集成调用。发布权限与触发保持不变。

上述 job 不执行 React/DOM 客户端验收，验证结果仅适用于它 checkout 的精确
依赖图；Host 单元测试通过不等于生产 git `main` 已具备 #6 的修复，也不能
代替真实 npm 或桌面 app plane 验收。

## 6. Which process runs what

**Host halves run where the composition runs, and in Rebon that is more
than one process:**

- **The desktop app process.** It boots its own process plane at start-up
  (`app/src/plugin_plane.rs`, `plugin_boot::ensure_process_composition`) for
  its `/` menu and its settings page. The entries of this plane are the
  composition as configured, with no session open.
- **Every session worker.** Sessions run in `rebon` worker processes, and
  each worker boots the same composition for the turns it runs. Tools, model
  routes and prompt sections are served from there.

**The client realm belongs in the desktop app process.** It should be one
Verve isolate with `verve-dom` installed, per window. It sits next to the
GPUI drawer that paints its DOM, and next to the app's own plane, so most
remotes stay inside the app process:

- **Composition-level remotes go to the app's own plane**: settings,
  credentials, plugin inventory and manager, client modules. They describe
  the configured composition, which is identical in every process. A
  settings write lands in `config.json` and is live in the app's realm at
  once.
- **Session-scoped remotes go to that session's worker**, over the existing
  session IPC: anything taking a session or agent (`session.*`,
  tool-call views that read session state). This is a new IPC request kind
  carrying `{endpoint, args}` to the worker's `call_remote`.
- **Settings writes reach workers.** A worker's realm is not live-updated by
  a write made in the app. Today it picks the write up on its next
  composition reload (the existing IPC `reload` op, or a new session). The
  next step is to have the app send the same `entryConfig` edit to running
  workers, so a volatile change applies there live too.

## 7. Recommendation

**Run the upstream client framework packages that are vocabulary unchanged,
and implement the slot host natively, the way su-editor implements
Obsidian's app.**

Running dsh's whole web shell (`dsh-client-web`, `ui-layout`, `ui-sidebar`,
…) on the DOM would put a second application inside Rebon. It would bring
its own sidebar, conversation view and settings window, all duplicating
Rebon's. It would also demand browser-grade DOM and CSS fidelity for all of
it. Rule 4 applies: one mechanism per concern.

What a client plugin actually needs is narrower. It needs:

- its `require` to resolve the platform modules;
- `ctx.slots` to accept its contributions;
- `ctx.remote` to reach its host;
- a few small services (`configForms`, `locale`, the store).

The split:

- **Unchanged, as platform modules in the client realm:** `react`,
  `react-dom`, `react/jsx-runtime`, `dsh-client-store`,
  `dsh-client-ui-primitives` (with shiki, clsx and anser),
  `dsh-client-ui-dockkit`, `zod`, `@tanstack/react-virtual`. Also unchanged
  are the generated remote clients of `dsh-api-remotes` and
  `dsh-client-modules`'s `__ModuleLoader__`. These are libraries, and
  re-implementing them would only be a worse copy.
- **Native, owned by Rebon:**
  - `ctx.slots`, a Rebon slot host. It is the replacement for `ui-renderer`
    and accepts the `inject`/`register` API. Each registration becomes a
    React root mounted into a `verve-dom` element that the GPUI drawer paints
    in the Rebon surface from §2.
  - The `connection` carrier: `__DSH_TRANSPORT__.rpc` mapped to
    `call_remote` as in §6, and `$events` fed from `compose:remote-event`.
  - `uiSession` and `sessions`, which are Rebon's sessions.
- **Unchanged where they only consume remotes:** `ui-settings`'s
  `configForms` and `client/locale`. If either reaches for a shell service
  Rebon does not provide, it gets a thin native face instead.

The boot is the one in §1, without `uiRenderer.mount`:

1. The app asks `rebon/clientModules` for the graph.
2. It reads each `file` for `loadBundle`.
3. It runs `cordis-plugin-loader`'s entries in the client realm.
4. Rebon's slot host draws whatever was registered.

## 8. What the DOM and drawer work needs from this side

- **The graph and the bundles.** `PluginPlane::call_remote("rebon/clientModules", {})`
  lists them. Bundles are files on this machine (`entries[].file`), and
  `rev` changes when a bundle does.
- **The carrier.** `call_remote(endpoint, args)` 提供 unary Host seam；
  `open_remote("$events", {})` 提供 ready／emit 流，生命周期与错误语义见 §5。
  DOM 侧只需将二者接到 `__DSH_TRANSPORT__`，不再自行订阅内核 topic 或构造 ready。
  当前不提供的 namespace／stream 都会按 wire contract 明确拒绝。
- **DOM capabilities the framework uses:**
  - `document.head`;
  - `<style>` elements with `data-*` attributes;
  - `querySelectorAll`;
  - a container element per slot;
  - what React 18's `createRoot` and `useSyncExternalStore` need (the
    microtask scheduler, `MessageChannel`, event delegation at the root).
- **Not yet here:**
  - a session-scoped `call_remote` over session IPC (§6);
  - waterfall remotes (`approval/request`, `user-questions/request`), which
    Rebon answers with its own prompts;
  - the native slot host itself.
