// Real dsh tool plugins, served back through `tool/call`.
//
// This is `crates/rebon-harness/tests/kernel_compose_dsh_tools_js.rs` on the
// new transport. The plugins are vendored verbatim: `tool-todo` registers
// through `ctx.tools.register(defineTool({...}))`, `tool-web` reaches
// `ctx.web`, and neither knows the plugin plane exists.
//
// The two things worth watching:
//
//   * a tool is *offered*, so what `plugin/load` reports is the definition —
//     the description and input schema a model reads — not a name;
//   * a plugin whose tool reaches `ctx.web` causes rebon's own WebSearch to
//     run, attributed to that plugin. That is why it declares `WebSearch` in
//     `invokableTools`, and why refusing to declare it refuses the call.
import test from 'node:test';
import assert from 'node:assert/strict';
import { loadCompose, loadEntry, openScope, plainly, startHost } from './harness.mjs';

const todoArgs = {
  todos: [
    { content: '写协议', status: 'in_progress' },
    { content: '写测试', status: 'pending' },
  ],
};

test('the real tool-todo plugin registers a tool rebon can offer and call', async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'tool-todo' }] });
    const load = await loadEntry(kit, {
      id: 'tool-todo',
      specifier: '@deepseek-ai/dsh-tool-todo',
      config: { allowParallelInProgress: false },
      tools: ['todo_write'],
      publishedTopics: ['compose:session/append'],
    });
    assert.equal(load.status, 'success', JSON.stringify(load.payload));

    const [tool] = plainly(load.payload.tools);
    assert.equal(tool.name, 'todo_write');
    assert.match(tool.description, /task list/);
    assert.equal(tool.inputSchema.properties.todos.type, 'array');

    await openScope(kit, 'tool-todo');
    const call = await kit.scoped('tool-todo', 'tool/call', { tool: 'todo_write', input: todoArgs });
    assert.equal(call.terminal.status, 'success', JSON.stringify(call.terminal.payload));
    assert.equal(call.terminal.payload.isError, false);

    // The tool body wrote to the session; on the plane that is a published
    // event, attributed to the plugin whose tool wrote it.
    const appended = kit.seen.events.filter((e) => e.topic === 'compose:session/append');
    assert.equal(appended.length, 1);
    assert.equal(appended[0].pluginId, 'tool-todo');
  } finally {
    await kit.stop();
  }
});

test("the vendored validator rejects bad arguments as this call's failure", async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'tool-todo' }] });
    await loadEntry(kit, {
      id: 'tool-todo',
      specifier: '@deepseek-ai/dsh-tool-todo',
      config: { allowParallelInProgress: false },
      tools: ['todo_write'],
      publishedTopics: ['compose:session/append'],
    });
    await openScope(kit, 'tool-todo');

    const call = await kit.scoped('tool-todo', 'tool/call', { tool: 'todo_write', input: { todos: 'not a list' } });
    // The plugin's own exception fails that call, not the host — every other
    // plugin in the composition is still running.
    assert.equal(call.terminal.status, 'error');
    const list = await kit.scoped('rebon:compose', 'service/call', { service: 'compose', request: { kind: 'list' } });
    assert.deepEqual(plainly(list.terminal.payload.plugins), ['tool-todo']);
  } finally {
    await kit.stop();
  }
});

test('the real tool-web plugin reaches rebon own WebSearch through the web seat', async () => {
  const kit = await startHost({
    tools: {
      WebSearch: (input) => ({
        query: input.query,
        answer: '摘要',
        results: [{ url: 'https://example.invalid/a', title: 'A', snippet: '…' }],
      }),
    },
  });
  try {
    await loadCompose(kit, { entries: [{ id: 'tool-web' }] });
    const load = await loadEntry(kit, {
      id: 'tool-web',
      specifier: '@deepseek-ai/dsh-tool-web',
      config: {},
      tools: ['web_search', 'web_fetch'],
      invokableTools: ['WebSearch', 'WebFetch'],
    });
    assert.equal(load.status, 'success', JSON.stringify(load.payload));
    assert.deepEqual(plainly(load.payload.tools).map((t) => t.name).sort(), ['web_fetch', 'web_search']);

    await openScope(kit, 'tool-web');
    const call = await kit.scoped('tool-web', 'tool/call', { tool: 'web_search', input: { query: 'rebon' } });
    assert.equal(call.terminal.status, 'success', JSON.stringify(call.terminal.payload));

    // The seat's deployment default is rebon's own tool, and the invocation is
    // attributed to the plugin that caused it.
    assert.deepEqual(plainly(kit.seen.invokes), [{ tool: 'WebSearch', input: { query: 'rebon' } }]);
  } finally {
    await kit.stop();
  }
});

test('a plugin that did not declare WebSearch cannot reach it through the web seat', async () => {
  const kit = await startHost({ tools: { WebSearch: () => ({ results: [] }) } });
  try {
    await loadCompose(kit, { entries: [{ id: 'tool-web' }] });
    await loadEntry(kit, {
      id: 'tool-web',
      specifier: '@deepseek-ai/dsh-tool-web',
      config: {},
      tools: ['web_search', 'web_fetch'],
    });
    await openScope(kit, 'tool-web');

    const call = await kit.scoped('tool-web', 'tool/call', { tool: 'web_search', input: { query: 'rebon' } });
    assert.equal(call.terminal.status, 'error');
    assert.deepEqual(kit.seen.invokes, [], 'refused on the plugin own side');
  } finally {
    await kit.stop();
  }
});

test('a plugin web provider is reported and served under its own name', async () => {
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'exa' }] });
    const load = await loadEntry(kit, {
      id: 'exa',
      specifier: '@deepseek-ai/dsh-web-search-exa',
      config: { apiKey: 'exa-test-key' },
      services: ['web:search:exa'],
    });
    assert.equal(load.status, 'success', JSON.stringify(load.payload));
    assert.deepEqual([...load.payload.services], ['web:search:exa']);

    await openScope(kit, 'rebon:compose');
    const report = await kit.scoped('rebon:compose', 'service/call', {
      service: 'compose', request: { kind: 'report', pluginId: 'exa' },
    });
    assert.deepEqual(plainly(report.terminal.payload.webProviders), [
      { kind: 'search', id: 'exa', available: true },
    ]);
  } finally {
    await kit.stop();
  }
});
