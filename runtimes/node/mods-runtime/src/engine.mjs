// `$`, the engine interface a mod's hooks are handed.
//
// Flat, `<noun>.<method>`, frozen. The nouns here are the core ones a
// hooks module reaches for in the Claude Code declarations; what each does
// on Rebon is one of three things:
//
// * answered locally — `plugin`, `ui.resolve`, `clock`, the timer half of
//   the chain's own life;
// * answered by the `mods` seat over `seat/call` — display (`ui.status`,
//   `ui.toast`, `ui.log`, panes), state and store, files, processes, the
//   network, settings, environment, the session's facts;
// * dispatched through the chain itself — `prompt.submit`, `command.run`,
//   `tool.call` on a tool the mod registered.
//
// A seat call made before the mod's scope opened, or after it closed,
// rejects naming that: a mod that acts on its own schedule starts from
// `session.start`, which fires once the scope is open.
import { elementTable } from './render.mjs';

/// The seat the engine interface is answered by on the Rust side.
export const MODS_SEAT = 'mods';

export function buildEngine(mod) {
  const seat = (method, params = {}) => mod.seat(method, params);
  const timers = mod.timers;

  const $ = {
    plugin: Object.freeze({ name: mod.name, root: mod.root }),

    ui: Object.freeze({
      resolve: (e) => elementTable(e?.surface),
      status: (text) => { void seat('ui.status', { text: text ?? null }); },
      toast: (text, options = {}) => { void seat('ui.toast', { text, ...options }); },
      log: (text, options = {}) => { void seat('ui.log', { text: String(text), to: options.to ?? 'transcript' }); },
      notice: (toolUseId, text) => { void seat('ui.notice', { toolUseId, text: text ?? null }); },
      invalidate: (event = 'ui.render') => { void seat('ui.invalidate', { event }); },
      open: (pane) => seat('ui.open', pane),
      close: (pane) => {
        if (pane?.id !== undefined) mod.clients.forget(String(pane.id));
        return seat('ui.close', pane);
      },
      panes: () => seat('ui.panes', {}),
      focus: (args) => seat('ui.focus', args),
      scroll: (args) => seat('ui.scroll', args),
      copy: (args) => seat('ui.copy', args),
      selection: () => seat('ui.selection', {}),
      ask: (question, options) => seat('ui.ask', { question, options: Array.isArray(options) ? { choices: options } : options ?? {} }),
      blit: async () => ({ deny: 'the terminal raster is not drawn on Rebon' }),
    }),

    state: Object.freeze({
      get: (ref) => seat('state.get', { ref }),
      set: (ref, value, options = {}) => seat('state.set', { ref, value, ...options }),
    }),

    store: Object.freeze({
      get: (key) => seat('store.get', { key }),
      set: (key, value) => seat('store.set', { key, value }),
      delete: (key) => seat('store.delete', { key }),
      keys: () => seat('store.keys', {}),
    }),

    clock: Object.freeze({
      now: () => Date.now(),
      sleep: (ms, options = {}) => new Promise((resolve, reject) => {
        const timer = setTimeout(() => { timers.delete(timer); resolve(); }, ms);
        timers.add(timer);
        options.signal?.addEventListener('abort', () => { clearTimeout(timer); timers.delete(timer); reject(new Error('sleep aborted')); }, { once: true });
      }),
      after: (ms, fn) => {
        const timer = setTimeout(() => { timers.delete(timer); runTimer(mod, fn); }, ms);
        timers.add(timer);
        return Object.freeze({ cancel: () => { clearTimeout(timer); timers.delete(timer); } });
      },
      every: (ms, fn) => {
        const timer = setInterval(() => runTimer(mod, fn), Math.max(10, ms));
        timers.add(timer);
        return Object.freeze({ cancel: () => { clearInterval(timer); timers.delete(timer); } });
      },
    }),

    session: Object.freeze({
      id: () => mod.sessionFact('sessionId'),
      cwd: () => mod.sessionFact('cwd'),
      root: () => mod.sessionFact('cwd'),
      model: () => mod.sessionFact('model'),
      surface: () => mod.sessionFact('surface'),
      surfaces: () => seat('session.surfaces', {}),
      turns: () => mod.sessionFact('turns'),
      messages: (args = {}) => seat('session.messages', args),
      usage: (args = {}) => seat('session.usage', args),
      version: () => seat('session.version', {}),
      repo: () => seat('session.repo', {}),
      append: (args) => seat('session.append', args),
      send: (args) => seat('session.send', args),
      compact: (args = {}) => seat('session.compact', args),
      authorize: () => seat('session.authorize', {}),
    }),

    turn: Object.freeze({
      abort: (input = {}) => seat('turn.abort', input),
    }),

    prompt: Object.freeze({
      submit: (input) => seat('prompt.submit', typeof input === 'string' ? { text: input } : input),
      fill: (input) => seat('prompt.fill', input),
      suggest: (input) => seat('prompt.suggest', typeof input === 'string' ? { text: input } : input),
      read: () => seat('prompt.read', {}),
      compose: (args = {}) => seat('prompt.compose', args),
    }),

    tool: Object.freeze({
      list: () => seat('tool.list', {}),
      call: (input) => mod.callTool(input),
      check: (input) => seat('tool.check', input),
      register: (spec) => mod.registerTool(spec),
    }),

    command: Object.freeze({
      list: () => seat('command.list', {}),
      run: (input) => seat('command.run', typeof input === 'string' ? { command: input } : input),
      register: (spec) => mod.registerCommand(spec),
    }),

    config: Object.freeze({
      list: () => seat('config.list', {}),
      set: (input) => seat('config.set', input),
    }),

    settings: Object.freeze({
      read: (args = {}) => seat('settings.read', args),
    }),

    env: Object.freeze({
      get: (name) => seat('env.get', { name }),
      set: (name, value) => seat('env.set', { name, value: value ?? null }),
    }),

    fs: Object.freeze({
      read: (path, options = {}) => seat('fs.read', { path, ...options }),
      write: (path, text) => seat('fs.write', { path, text }),
      list: (path) => seat('fs.list', { path: path ?? null }),
      exists: (path) => seat('fs.exists', { path }),
      stat: (path, options = {}) => seat('fs.stat', { path, ...options }),
      ancestors: (request) => seat('fs.ancestors', request),
    }),

    process: Object.freeze({
      run: (argv, init = {}) => seat('process.run', { argv: [...argv], ...init }),
      spawn: async (request) => {
        // No streaming on this plane: the child runs to completion and its
        // output comes back whole, which is what `run` answers.
        const done = await seat('process.run', { argv: [...request.argv], cwd: request.cwd, env: request.env, input: request.input });
        return { ...done, isStreamed: false };
      },
    }),

    http: Object.freeze({
      fetch: (url, init = {}) => seat('http.fetch', { url: String(url), ...init }),
    }),

    model: Object.freeze({
      complete: (request, options = {}) => seat('model.complete', { ...request, ...options }),
      fork: (request) => seat('model.fork', request),
      classify: (text, labels, options = {}) => seat('model.classify', { text, labels: [...labels], ...options }),
    }),

    agent: Object.freeze({
      list: () => seat('agent.list', {}),
      spawn: (input) => seat('agent.spawn', input),
      register: (spec) => seat('agent.register', spec),
    }),

    mcp: Object.freeze({
      call: (server, tool, args = {}) => mod.callTool({ tool: `mcp__${server}__${tool}`, input: args }),
      connect: (server) => seat('mcp.connect', { server }),
    }),

    audio: Object.freeze({
      play: async () => ({ isPlayed: false, reason: 'no-audio' }),
      speak: async () => ({ isSpoken: false, reason: 'no-audio' }),
    }),

    telemetry: Object.freeze({
      log: (entry) => { void seat('ui.log', { text: JSON.stringify(entry), to: 'debug' }); },
      mark: () => {},
    }),
  };
  return Object.freeze($);
}

function runTimer(mod, fn) {
  Promise.resolve()
    .then(() => fn(mod.engine))
    .catch((error) => mod.report('timer', error));
}
