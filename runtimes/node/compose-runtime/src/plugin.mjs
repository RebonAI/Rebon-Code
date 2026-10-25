// The composition control plugin.
//
// rebon loads this one first, and everything else the composition contains is
// loaded after it, as an ordinary `plugin/load` each. What this one does is
// make that possible: install module resolution for the vendored payload,
// create the shared Cordis realm the entries mount into, and answer for the
// facts the protocol has no field for.
//
// **Why the structure lives in this plugin's config.** Groups and `isolate` are
// composition structure, not plugin facts: a group is a container that is not a
// module, and an isolated realm is a relationship between entries. Neither can
// be said in one entry's own load request, so rebon says it once, here, naming
// only ids and placement. Each entry's `plugin/load` then carries its module
// and its own configuration.
//
// Module resolution has to be installed before Cordis is imported, and static
// imports are linked before a module body runs — which is why everything below
// is imported dynamically. Only Node's own builtins, which need no resolution,
// could be static, and none are needed.
export async function activate(api, config = {}) {
  const { installModuleResolution } = await import('./resolve.mjs');
  installModuleResolution({
    ...(config.payloadDir ? { dir: config.payloadDir } : {}),
    extraModules: config.modules ?? {},
  });

  const { withCall } = await import('./bridge.mjs');
  const { builtinSeats, createRealm, mounted, realm, reportFor } = await import('./realm.mjs');
  const { remote, REMOTE_EVENTS } = await import('./remote.mjs');
  await createRealm({
    web: config.web ?? null,
    entries: config.entries ?? [],
    profile: config.profile ?? null,
  });

  // Registered before any entry is loaded, so an entry claiming the same
  // service name hits the registrar's refusal rather than replacing this.
  //
  // What it answers is the part of a load report the protocol cannot carry: a
  // route's model catalog, a prompt section, a web provider. Those belong to
  // one plugin kind, and the ready report is a shape every plugin shares — so
  // they are asked for rather than bolted on.
  api.service('compose', async (request, ctx) => withCall(ctx, async () => {
    const kind = request?.kind;
    if (kind === 'list') {
      return { plugins: mounted(), builtin: builtinSeats() };
    }
    if (kind === 'remote') {
      return remote(String(request?.endpoint ?? ''), request?.args ?? {});
    }
    if (kind === 'report') {
      const pluginId = String(request?.pluginId ?? '');
      const report = reportFor(pluginId);
      if (report === undefined) {
        const error = new Error(`no composition entry ${JSON.stringify(pluginId)} is mounted`);
        error.code = '[UNKNOWN_PLUGIN]';
        throw error;
      }
      return report;
    }
    const error = new Error(`compose does not answer kind ${JSON.stringify(kind ?? null)}`);
    error.code = '[UNKNOWN_CONTROL]';
    throw error;
  }));

  // The Host events a dsh client hears, put on rebon's event plane. They are
  // the composition's own facts — a settings form changing under a page —
  // and nothing is asking when they happen, so they go out through the
  // session the control plugin is attached to rather than an inbound call.
  const sessions = new Set();
  api.scope((scope) => {
    sessions.add(scope);
    return () => sessions.delete(scope);
  });
  for (const event of REMOTE_EVENTS) {
    realm().ctx.on(event, (...args) => {
      for (const scope of sessions) {
        void scope.publish(REMOTE_EVENT_TOPIC, { event, args: JSON.parse(JSON.stringify(args)) }).catch(() => {});
        break;
      }
    });
  }
}

/** The topic forwarded Host events are published on. */
export const REMOTE_EVENT_TOPIC = 'compose:remote-event';

export default activate;
