// The REAL dsh agent loop, driving one complete turn on the plugin plane.
//
// This is `crates/rebon-harness/tests/kernel_compose_agent_loop_js.rs` on the
// new transport. Every dsh package is vendored verbatim — session, agent,
// system-prompt, agent-loop, llm-deepseek — and the composition is six
// `plugin/load` calls rather than one nested entry list:
//
//   kickoff → dsh systemPrompt assembly (real section + the tool schemas rebon
//   handed over) → LLM request through the in-composition consumer face → the
//   real llm-deepseek adapter against a local fake SSE endpoint → tool call
//   scheduled through the seat's scheduler contract → rebon's own tool runs
//   through `tool/invoke` → result rejoins the session log → second model step
//   → `turn/end {completed}`.
//
// Every session fact is asserted from the published `loop:event` stream — the
// same event-sourced log the loop derives its own requests from.
import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { COMPOSE_ROOT, loadCompose, loadEntry, openScope, plainly, startHost } from './harness.mjs';

/// Two canned turns: the first asks for a tool, the second answers.
function fakeDeepSeek() {
  const seen = [];
  const turns = [
    ': hi\n\n'
    + 'data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function",'
    + '"function":{"name":"probe_tool","arguments":"{\\"text\\":\\"你好\\"}"}}]}}]}\n\n'
    + 'data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}\n\n'
    + 'data: [DONE]\n\n',
    'data: {"choices":[{"index":0,"delta":{"content":"办好了"}}]}\n\n'
    + 'data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\n'
    + 'data: [DONE]\n\n',
  ];
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (piece) => { body += piece; });
    req.on('end', () => {
      const index = Math.min(seen.length, turns.length - 1);
      seen.push(JSON.parse(body));
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.end(turns[index]);
    });
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({
      seen,
      baseURL: `http://127.0.0.1:${server.address().port}`,
      close: () => server.close(),
    }));
  });
}

/// rebon's own tool catalog, as it hands it to the loop at load.
const TOOL_CATALOG = [{
  name: 'probe_tool',
  description: 'A rebon core tool the loop may call.',
  inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] },
}];

const LOOP_ENTRIES = [
  { id: 'llm-deepseek' },
  {
    id: 'loop',
    isolate: { systemPrompt: 'loop' },
    group: [
      { id: 'loop-sessions' },
      { id: 'loop-agents' },
      { id: 'loop-prompt' },
      { id: 'loop-assembly' },
      { id: 'agent-loop' },
    ],
  },
];

/** Every plugin the session is opened for, in load order. */
const SESSION_PLUGINS = ['llm-deepseek', 'loop-sessions', 'loop-agents', 'loop-prompt', 'loop-assembly', 'agent-loop'];

async function bootLoop(kit, endpoint) {
  const loads = [];
  const load = async (spec) => {
    const answer = await loadEntry(kit, spec);
    assert.equal(answer.status, 'success', `${spec.id}: ${JSON.stringify(answer.payload)}`);
    loads.push(answer.payload);
    return answer.payload;
  };
  await load({
    id: 'llm-deepseek',
    specifier: '@deepseek-ai/dsh-llm-deepseek',
    config: {
      apiKeyEnv: 'REBON_TEST_LOOP_KEY',
      baseURL: endpoint.baseURL,
      models: [{ id: 'deepseek-v4-flash', name: 'F', contextWindow: 128000 }],
    },
    llmProviders: ['deepseek-official'],
    seats: ['credentials'],
  });
  await load({ id: 'loop-sessions', specifier: '@deepseek-ai/dsh-session' });
  await load({ id: 'loop-agents', specifier: '@deepseek-ai/dsh-agent' });
  await load({ id: 'loop-prompt', specifier: '@deepseek-ai/dsh-system-prompt' });
  await load({
    id: 'loop-assembly',
    root: COMPOSE_ROOT.replace(/\\/g, '/'),
    entry: 'src/loop-assembly.mjs',
    config: { kickoff: '你好', toolCatalog: TOOL_CATALOG },
    services: ['loop:control'],
    publishedTopics: ['loop:event', 'loop:agent-error', 'loop:agent-created'],
    seats: ['logger'],
  });
  await load({
    id: 'agent-loop',
    specifier: '@deepseek-ai/dsh-agent-loop',
    config: { agents: [{ id: 'probe', provider: 'deepseek-official', model: 'deepseek-v4-flash' }] },
    // The loop is what causes one of rebon's own tools to run — the model
    // picks it, the loop schedules it — so the loop is what declares which of
    // them it may reach. rebon builds that list from what it offers the model,
    // which is the same list it handed over as `toolCatalog`.
    invokableTools: ['probe_tool'],
  });
  return loads;
}

/// Waits for the loop to say the turn ended, reading only published events.
async function turnEnd(kit, timeoutMs = 20000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const end = kit.seen.events.find((e) => e.topic === 'loop:event' && e.event?.type === 'turn/end');
    if (end !== undefined) return plainly(end.event);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  const seen = kit.seen.events.filter((e) => e.topic === 'loop:event').map((e) => e.event.type);
  throw new Error(`the turn never ended; session events were: ${JSON.stringify(seen)}`);
}

test('the real agent loop runs a whole turn, tool call included', async () => {
  const endpoint = await fakeDeepSeek();
  const kit = await startHost({
    env: { REBON_TEST_LOOP_KEY: 'sk-loop-fake' },
    tools: { probe_tool: (input) => ({ ok: true, echoed: input.text }) },
  });
  try {
    await loadCompose(kit, { entries: LOOP_ENTRIES });
    await bootLoop(kit, endpoint);

    // Nothing has happened yet: the loop is loaded and no session is attached,
    // so the kickoff is still waiting.
    assert.deepEqual(endpoint.seen, [], 'no model request before a session opens');

    // rebon opens the session for every plugin in it: a plugin acting on its
    // own schedule speaks for the session it is attached to, and the adapter
    // fetching a credential mid-turn is exactly that.
    // rebon opens the session for every plugin in it: a plugin acting on its
    // own schedule speaks for the session it is attached to, and both the
    // adapter fetching a credential and the loop running a tool are that.
    for (const id of SESSION_PLUGINS) await openScope(kit, id);
    const end = await turnEnd(kit);
    assert.equal(end.type, 'turn/end');
    assert.equal(end.data?.reason?.kind, 'completed', JSON.stringify(end.data));

    const types = kit.seen.events.filter((e) => e.topic === 'loop:event').map((e) => e.event.type);
    // The agent was created before the session attached, and the held event was
    // delivered once it did.
    assert.ok(kit.seen.events.some((e) => e.topic === 'loop:agent-created'), 'the agent creation reached rebon');
    assert.ok(types.includes('turn/start'), JSON.stringify(types));

    // Two model steps, and the tool ran through rebon rather than in the
    // composition: `probe_tool` is rebon's, not a registered composition tool.
    assert.equal(endpoint.seen.length, 2, 'one request per model step');
    assert.deepEqual(plainly(kit.seen.invokes), [{ tool: 'probe_tool', input: { text: '你好' } }]);

    // The tool schema rebon handed over reached the model.
    const offered = endpoint.seen[0].tools?.map((t) => t.function?.name ?? t.name) ?? [];
    assert.ok(offered.includes('probe_tool'), JSON.stringify(endpoint.seen[0].tools));
  } finally {
    endpoint.close();
    await kit.stop();
  }
});

test('the loop control face drives the same loop from rebon', async () => {
  const endpoint = await fakeDeepSeek();
  const kit = await startHost({
    env: { REBON_TEST_LOOP_KEY: 'sk-loop-fake' },
    tools: { probe_tool: () => ({ ok: true }) },
  });
  try {
    await loadCompose(kit, { entries: LOOP_ENTRIES });
    await bootLoop(kit, endpoint);
    for (const id of SESSION_PLUGINS) await openScope(kit, id);
    await turnEnd(kit);

    const status = await kit.scoped('loop-assembly', 'service/call', {
      service: 'loop:control', request: { kind: 'status' },
    });
    assert.equal(status.terminal.status, 'success', JSON.stringify(status.terminal.payload));
    // The loop names an agent by its configured id plus the session it runs in.
    assert.match(status.terminal.payload.agentId, /^probe-session-/);

    const unknown = await kit.scoped('loop-assembly', 'service/call', {
      service: 'loop:control', request: { kind: 'nonsense' },
    });
    assert.equal(unknown.terminal.status, 'error');
    assert.match(unknown.terminal.payload.message, /unknown command kind/);
  } finally {
    endpoint.close();
    await kit.stop();
  }
});

test('a loop entry mounted outside its isolated group is refused', async () => {
  // The assembly needs the loop realm's own systemPrompt; mounted flat it would
  // find rebon's seat instead, which has no `tools()` face. The structure is
  // what keeps the two apart, so getting it wrong has to be an error.
  const kit = await startHost();
  try {
    await loadCompose(kit, { entries: [{ id: 'loop-assembly' }] });
    const load = await loadEntry(kit, {
      id: 'loop-assembly',
      root: COMPOSE_ROOT.replace(/\\/g, '/'),
      entry: 'src/loop-assembly.mjs',
      config: { toolCatalog: TOOL_CATALOG },
      services: ['loop:control'],
      publishedTopics: ['loop:event', 'loop:agent-error', 'loop:agent-created'],
      seats: ['logger'],
    });
    assert.equal(load.status, 'error');
    assert.match(load.payload.message, /systemPrompt|tools/);
  } finally {
    await kit.stop();
  }
});
