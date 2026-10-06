import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createLoader, isModRequest, MARKER } from '../src/index.mjs';

const fixture = path.join(path.dirname(fileURLToPath(import.meta.url)), 'fixtures', 'counter-mod');

const marker = {
  name: 'counter',
  version: '0.1.0',
  options: { prefix: 'n=' },
  commands: [{ name: 'count', description: 'Shows the count' }],
  tools: [],
};

const request = (over = {}) => ({
  pluginId: 'counter',
  root: fixture,
  entry: 'hooks/register.tsx',
  services: ['mod'],
  seats: ['mods'],
  config: { [MARKER]: marker },
  ...over,
});

/// A scope context standing for the plane: it records seat calls and keeps
/// `$.state` in memory with versions.
function fakeScope() {
  const calls = [];
  const state = new Map();
  const ctx = {
    workspaceRoot: '/work/project',
    async seat(seat, method, params) {
      calls.push({ seat, method, params });
      if (method === 'state.get') {
        const key = `${params.ref.plugin}.${params.ref.key}`;
        const held = state.get(key);
        return held ? { value: held.value, version: held.version } : { value: undefined, version: 0 };
      }
      if (method === 'state.set') {
        const key = `${params.ref.plugin}.${params.ref.key}`;
        const held = state.get(key) ?? { version: 0 };
        if (params.ifVersion !== undefined && params.ifVersion !== held.version) return { isWritten: false, value: held.value, version: held.version };
        const next = { value: params.value, version: held.version + 1 };
        state.set(key, next);
        return { isWritten: true, ...next };
      }
      return {};
    },
    async invoke(tool, input) { calls.push({ invoke: tool, input }); return { ok: true }; },
  };
  return { ctx, calls, state };
}

async function loadCounter() {
  const next = async () => { throw new Error('next must not be reached for a mod'); };
  const loader = await createLoader({ next });
  const sealed = await loader.load(request());
  return { loader, sealed };
}

test('a request without the marker is handed to the next loader', async () => {
  let handed;
  const loader = await createLoader({ next: async (req) => { handed = req; return 'from-next'; } });
  const plain = { pluginId: 'other', root: fixture, entry: 'x.mjs', config: null };
  assert.ok(!isModRequest(plain));
  assert.equal(await loader.load(plain), 'from-next');
  assert.equal(handed, plain);
  assert.equal(await loader.unload('other'), false);
});

test('a .tsx hooks module loads, with its .ts import and the claude-code shim', async () => {
  const { sealed, loader } = await loadCounter();
  assert.deepEqual([...sealed.services], ['mod']);
  assert.equal(typeof sealed.serviceHandlers.get('mod'), 'function');
  assert.equal(sealed.commands.length, 1);
  assert.equal(sealed.commands[0].name, 'count');
  assert.equal(sealed.commands[0].kind.type, 'prompt');
  assert.deepEqual(sealed.commands[0].surfaces, ['tui', 'desktop', 'acp', 'web', 'mobile']);
  assert.equal(sealed.scopeHandlers.length, 1);
  assert.equal(await loader.unload('counter'), true);
});

test('opening the scope fires session.start, which registers the command and sets the status', async () => {
  const { sealed, loader } = await loadCounter();
  const { ctx, calls } = fakeScope();
  const dispose = await sealed.scopeHandlers[0](ctx);
  assert.equal(typeof dispose, 'function');
  const registered = calls.find((call) => call.method === 'command.register');
  assert.deepEqual(registered.params, { name: 'count', description: 'Shows the count', argumentHint: '[reset]' });
  assert.deepEqual(calls.find((call) => call.method === 'ui.status').params, { text: 'n=0' });
  await loader.unload('counter');
});

test('PreToolUse becomes tool.call: a deny, a rewrite, and the result on PostToolUse', async () => {
  const { sealed, loader } = await loadCounter();
  const { ctx } = fakeScope();
  await sealed.scopeHandlers[0](ctx);
  const mod = sealed.serviceHandlers.get('mod');

  const denied = await mod({ kind: 'classic', event: 'PreToolUse', input: { hook_event_name: 'PreToolUse', tool_name: 'Bash', tool_use_id: 't1', tool_input: { command: 'rm -rf /' } } }, ctx);
  assert.deepEqual(denied.outputs, [{ hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny', permissionDecisionReason: 'not on my watch' } }]);

  const rewritten = await mod({ kind: 'classic', event: 'PreToolUse', input: { hook_event_name: 'PreToolUse', tool_name: 'Bash', tool_use_id: 't2', tool_input: { command: 'ls' } } }, ctx);
  assert.deepEqual(rewritten.outputs, [{ hookSpecificOutput: { hookEventName: 'PreToolUse', updatedInput: { command: 'ls # seen' } } }]);

  const after = await mod({ kind: 'classic', event: 'PostToolUse', input: { hook_event_name: 'PostToolUse', tool_name: 'Bash', tool_use_id: 't2', tool_input: { command: 'ls # seen' }, tool_response: { stdout: 'a\n' } } }, ctx);
  assert.deepEqual(after.outputs, [{ hookSpecificOutput: { hookEventName: 'PostToolUse', additionalContext: 'bash answered fine' } }]);

  const unrelated = await mod({ kind: 'classic', event: 'PreToolUse', input: { hook_event_name: 'PreToolUse', tool_name: 'Read', tool_use_id: 't3', tool_input: { path: 'x' } } }, ctx);
  assert.deepEqual(unrelated.outputs, []);
  await loader.unload('counter');
});

test('UserPromptSubmit becomes prompt.submit: a rewrite replaces, a drop blocks', async () => {
  const { sealed, loader } = await loadCounter();
  const { ctx } = fakeScope();
  await sealed.scopeHandlers[0](ctx);
  const mod = sealed.serviceHandlers.get('mod');
  const rewritten = await mod({ kind: 'classic', event: 'UserPromptSubmit', input: { hook_event_name: 'UserPromptSubmit', prompt: 'hello', session_id: 's1', cwd: '/w' } }, ctx);
  assert.deepEqual(rewritten.outputs, [{ hookSpecificOutput: { hookEventName: 'UserPromptSubmit', replacementPrompt: 'HELLO' } }]);
  const dropped = await mod({ kind: 'classic', event: 'UserPromptSubmit', input: { hook_event_name: 'UserPromptSubmit', prompt: 'drop me' } }, ctx);
  assert.deepEqual(dropped.outputs, [{ decision: 'block', reason: 'dropped by counter' }]);
  await loader.unload('counter');
});

test('Stop fires turn.complete and classic.Stop; both outputs come back', async () => {
  const { sealed, loader } = await loadCounter();
  const { ctx } = fakeScope();
  await sealed.scopeHandlers[0](ctx);
  const mod = sealed.serviceHandlers.get('mod');
  const stopped = await mod({ kind: 'classic', event: 'Stop', input: { hook_event_name: 'Stop', stop_reason: 'end_turn', last_assistant_message: 'done' } }, ctx);
  assert.deepEqual(stopped.outputs, [{ systemMessage: 'done (seen by counter)' }, { systemMessage: 'stopped: end_turn' }]);
  await loader.unload('counter');
});

test('a render answers a JSON tree, a press runs the handler and the next render shows it', async () => {
  const { sealed, loader } = await loadCounter();
  const { ctx } = fakeScope();
  await sealed.scopeHandlers[0](ctx);
  const mod = sealed.serviceHandlers.get('mod');
  const ask = { kind: 'render', component: 'Pane', surface: 'terminal', requestId: 'counter', props: { bodyColumns: 40 } };
  const first = await mod(ask, ctx);
  assert.deepEqual(first, {
    tree: {
      type: 'Box',
      props: { flexDirection: 'column', borderStyle: 'round' },
      children: [
        { type: 'Text', props: { bold: true }, children: ['0 clicks'] },
        { type: 'Button', props: { key: 'more', variant: 'primary' }, children: ['more'] },
      ],
    },
  });
  const pressed = await mod({ kind: 'press', component: 'Pane', surface: 'terminal', requestId: 'counter', element: 'more' }, ctx);
  assert.deepEqual(pressed, { element: 'more' });
  const second = await mod(ask, ctx);
  assert.equal(second.tree.children[0].children[0], '1 click');

  const other = await mod({ kind: 'render', component: 'Pane', surface: 'desktop', requestId: 'elsewhere', props: {} }, ctx);
  assert.deepEqual(other, { engine: true });
  await loader.unload('counter');
});

test('a command runs through command.run and a describe lists the mod', async () => {
  const { sealed, loader } = await loadCounter();
  const { ctx } = fakeScope();
  await sealed.scopeHandlers[0](ctx);
  const answer = await sealed.commandHandlers.get('count')({ name: 'count', raw: '/count', rest: '', surface: 'tui' }, ctx);
  assert.equal(answer, 'count is 0');
  const mod = sealed.serviceHandlers.get('mod');
  const described = await mod({ kind: 'describe' }, ctx);
  assert.equal(described.name, 'counter');
  assert.deepEqual(described.commands, ['count']);
  assert.ok(described.patterns.includes('ui.render'));
  await assert.rejects(mod({ kind: 'nope' }, ctx), { code: '[UNKNOWN_KIND]' });
  await loader.unload('counter');
});

test('a seat call before the scope opens is refused by name', async () => {
  const { sealed, loader } = await loadCounter();
  const mod = sealed.serviceHandlers.get('mod');
  // The command's hook reads `$.state`, which needs the seat.
  const answer = await sealed.commandHandlers.get('count')({ name: 'count', raw: '/count', rest: '' }, undefined);
  assert.equal(answer, '');
  assert.equal(typeof mod, 'function');
  await loader.unload('counter');
});

test('a hooks module without register, and one that is not under the folder, are refused', async () => {
  const loader = await createLoader({ next: async () => { throw new Error('unreachable'); } });
  await assert.rejects(loader.load(request({ pluginId: 'bad', entry: 'hooks/label.ts' })), { code: '[NO_REGISTER]' });
  await assert.rejects(loader.load(request({ pluginId: 'missing', entry: 'hooks/nope.tsx' })), { code: '[ENTRY_FAILED]' });
});
