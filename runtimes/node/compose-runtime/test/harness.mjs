// A rebon, for tests: the real plugin host running the real compose loader,
// with the upward half of the plane answered here instead of by a kernel.
//
// Everything below the harness is production code — the host, the loader, the
// realm, the seats, the vendored dsh packages. What is faked is rebon itself:
// `seat/call`, `tool/invoke` and `event/emit` are answered by the tables a test
// hands in, and every one of them is recorded so a test can assert on who asked
// for what.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { PluginHost } from '../../plugin-host/src/host.mjs';
import { loadPlugin } from '../../plugin-host/src/loader.mjs';
import { identityOf, terminal } from '../../plugin-host/src/protocol.mjs';
import { createLoader } from '../src/loader.mjs';
import { payloadDir } from '../src/payload.mjs';

export const COMPOSE_ROOT = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
export const PAYLOAD = payloadDir();

/// The vendored payload, by the specifier a composition names it with.
///
/// A composition entry is a package: `root` is where the payload lives and
/// `entry` is the module inside it. This is the same table module resolution
/// uses, read the other way round.
export const PAYLOAD_ENTRY = {
  '@deepseek-ai/dsh-llm-deepseek': 'vendor/dsh/llm-deepseek.js',
  '@deepseek-ai/dsh-tool-todo': 'vendor/dsh/tool-todo.js',
  '@deepseek-ai/dsh-tool-web': 'vendor/dsh/tool-web.js',
  '@deepseek-ai/dsh-web-search-exa': 'vendor/dsh/web-search-exa.js',
  '@deepseek-ai/dsh-session': 'vendor/dsh/session.js',
  '@deepseek-ai/dsh-agent': 'vendor/dsh/agent.js',
  '@deepseek-ai/dsh-system-prompt': 'vendor/dsh/system-prompt.js',
  '@deepseek-ai/dsh-agent-loop': 'vendor/dsh/agent-loop.js',
};

const control = (call_id, method, payload = null) => ({
  protocol_version: 1, host_epoch: 7, plugin_id: '$rebon/platform', scope_id: '$rebon/control',
  scope_generation: 0, call_id, message: { type: 'request', method, payload },
});
const scoped = (pluginId, call_id, method, payload, generation = 1) => ({
  protocol_version: 1, host_epoch: 7, plugin_id: pluginId, scope_id: 'session.1',
  scope_generation: generation, call_id, message: { type: 'request', method, payload },
});

export const answered = (sent, call_id) => sent.filter((x) => x.message.type === 'terminal')
  .find((x) => x.call_id === call_id)?.message;
export const chunks = (sent, call_id) => sent
  .filter((x) => x.message.type === 'chunk' && x.call_id === call_id)
  .map((x) => x.message.payload);
/// Anything that crossed the wire comes back with a null prototype, which
/// strict deep-equality sees; a comparison against a literal normalises first.
export const plainly = (value) => JSON.parse(JSON.stringify(value));

/// Writes a package that re-exports one of rebon's own composition modules.
///
/// `rebon-loop-assembly` is not in the vendored payload — it is this package's
/// own module — so a composition names it the way it names any other entry: a
/// root and a module inside it.
export function localEntry(dir, specifier) {
  const target = path.join(COMPOSE_ROOT, 'src', specifier);
  fs.writeFileSync(path.join(dir, 'entry.mjs'), `export * from ${JSON.stringify(target.replace(/\\/g, '/'))};\n`);
  return { root: dir.replace(/\\/g, '/'), entry: 'entry.mjs' };
}

/**
 * Starts a host, running the real compose loader.
 *
 * `env` is what the credentials seat may hand back, `tools` what `tool/invoke`
 * answers, and `deny` a set of seats that refuse — the three shapes a test
 * needs to stand in for a kernel.
 */
export async function startHost({ env = {}, tools = {}, seats = {} } = {}) {
  const sent = [];
  const seen = { seats: [], invokes: [], events: [] };
  const writer = {
    send: async (envelope) => {
      sent.push(envelope);
      if (envelope.message.type !== 'request') return;
      const { method, payload } = envelope.message;
      let answer = { status: 'success', payload: null };
      if (method === 'seat/call') {
        seen.seats.push({ seat: payload.seat, method: payload.method, params: payload.params });
        if (payload.seat === 'credentials' && payload.method === 'resolveEnv') {
          const ref = payload.params?.ref;
          answer = { status: 'success', payload: Object.hasOwn(env, ref) ? { value: env[ref] } : {} };
        } else if (typeof seats[payload.seat] === 'function') {
          answer = { status: 'success', payload: seats[payload.seat](payload) };
        }
      } else if (method === 'tool/invoke') {
        seen.invokes.push({ tool: payload.tool, input: payload.input });
        const answerer = tools[payload.tool];
        answer = answerer === undefined
          ? { status: 'error', payload: { code: '[UNAVAILABLE_TOOL]', message: `no ${payload.tool}` } }
          : { status: 'success', payload: answerer(payload.input) };
      } else if (method === 'event/emit') {
        seen.events.push({ topic: payload.topic, event: payload.event, pluginId: envelope.plugin_id });
      }
      queueMicrotask(() => host.accept(terminal(identityOf(envelope), answer.status, answer.payload)));
    },
    flush: async () => {},
  };
  const loader = await createLoader({ next: loadPlugin });
  const host = new PluginHost(writer, loader);
  let calls = 0;
  const kit = {
    host,
    sent,
    seen,
    /// Drives one request and returns its terminal.
    async control(method, payload) {
      const id = `c${++calls}`;
      await host.accept(control(id, method, payload));
      return answered(sent, id);
    },
    async scoped(pluginId, method, payload, generation = 1) {
      const id = `s${++calls}`;
      await host.accept(scoped(pluginId, id, method, payload, generation));
      return { id, terminal: answered(sent, id), chunks: chunks(sent, id) };
    },
    async stop() {
      // Imported here rather than at the top: the realm imports Cordis by bare
      // specifier, and nothing may reach it before the control plugin has
      // installed module resolution.
      const { destroyRealm } = await import('../src/realm.mjs');
      await destroyRealm();
    },
  };
  await kit.control('platform/initialize', null);
  return kit;
}

/// Loads the composition control plugin, which creates the realm.
export async function loadCompose(kit, { entries = [], web = null, modules = {} } = {}) {
  return kit.control('plugin/load', {
    pluginId: 'rebon:compose',
    root: COMPOSE_ROOT.replace(/\\/g, '/'),
    entry: 'src/plugin.mjs',
    services: ['compose'],
    config: { payloadDir: PAYLOAD, entries, web, modules },
  });
}

/// Loads one composition entry named by its payload specifier.
export async function loadEntry(kit, { id, specifier, root, entry, config = null, ...declared }) {
  return kit.control('plugin/load', {
    pluginId: id,
    root: root ?? PAYLOAD.replace(/\\/g, '/'),
    entry: entry ?? PAYLOAD_ENTRY[specifier],
    config,
    ...declared,
  });
}

/// Opens a session for one plugin.
export async function openScope(kit, pluginId, workspaceRoot = 'C:/workspace') {
  return kit.scoped(pluginId, 'scope/open', { workspace_root: workspaceRoot });
}
