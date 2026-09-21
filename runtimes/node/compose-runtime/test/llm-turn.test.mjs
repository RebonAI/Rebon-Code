// One model turn, end to end on the plugin plane.
//
// This is `crates/rebon-harness/tests/kernel_compose_deepseek_js.rs` said on
// the new transport: the REAL `@deepseek-ai/dsh-llm-deepseek` build — vendored
// verbatim, zero source modifications — loads as a composition entry, its route
// is reported by `plugin/load`, and an `llm/stream` request is answered with
// chunks on the call and a terminal at the end.
//
// Nothing here stands in for the composition: it is the real host, the real
// loader, the real Cordis realm, real `fetch`, and real SSE decoding. What is
// faked is rebon — the harness answers `seat/call` the way the kernel's
// credentials seat would — and the DeepSeek endpoint, which serves the same
// canned SSE bytes the Rust test uses.
import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { loadCompose, loadEntry, openScope, plainly, startHost } from './harness.mjs';

/// The canned turn, byte-identical to the one the Rust acceptance test serves.
const SSE = ': keep-alive comment\n\n'
  + 'data: {"choices":[{"index":0,"delta":{"reasoning_content":"想一想"}}]}\n\n'
  + 'data: {"choices":[{"index":0,"delta":{"content":"你好"}}]}\n\n'
  + 'data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\n'
  + 'data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,'
  + '"prompt_cache_hit_tokens":2,"completion_tokens_details":{"reasoning_tokens":3}}}\n\n'
  + 'data: [DONE]\n\n';

async function fakeDeepSeek() {
  const seen = [];
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (piece) => { body += piece; });
    req.on('end', () => {
      seen.push({ url: req.url, headers: req.headers, body });
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.end(SSE);
    });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return { seen, baseURL: `http://127.0.0.1:${server.address().port}`, close: () => server.close() };
}

const deepseekConfig = (baseURL) => ({
  apiKeyEnv: 'REBON_TEST_DS_KEY',
  baseURL,
  models: [{ id: 'deepseek-v4-flash', name: 'DeepSeek-V4-Flash', contextWindow: 128000 }],
});

async function composition({ env = {}, baseURL, providers = ['deepseek-official'] }) {
  const kit = await startHost({ env });
  await loadCompose(kit, { entries: [{ id: 'llm-deepseek' }] });
  const load = await loadEntry(kit, {
    id: 'llm-deepseek',
    specifier: '@deepseek-ai/dsh-llm-deepseek',
    config: deepseekConfig(baseURL),
    llmProviders: providers,
    seats: ['credentials'],
  });
  return { kit, load };
}

test('the real dsh DeepSeek plugin loads and reports its route', async () => {
  const endpoint = await fakeDeepSeek();
  const { kit, load } = await composition({ baseURL: endpoint.baseURL });
  try {
    assert.equal(load.status, 'success', JSON.stringify(load.payload));
    assert.deepEqual([...load.payload.llmProviders], ['deepseek-official']);

    // The model catalog has no field in the ready report, so it is asked for.
    await openScope(kit, 'rebon:compose');
    const report = await kit.scoped('rebon:compose', 'service/call', {
      service: 'compose', request: { kind: 'report', pluginId: 'llm-deepseek' },
    });
    assert.equal(report.terminal.status, 'success', JSON.stringify(report.terminal.payload));
    assert.deepEqual(plainly(report.terminal.payload.providers), [
      { provider: 'deepseek-official', models: [{ id: 'deepseek-v4-flash' }], defaultModel: 'deepseek-v4-flash' },
    ]);
  } finally {
    endpoint.close();
    await kit.stop();
  }
});

test('a granted turn streams the whole answer as chunks on the call', async () => {
  const endpoint = await fakeDeepSeek();
  const { kit } = await composition({
    baseURL: endpoint.baseURL,
    env: { REBON_TEST_DS_KEY: 'sk-local-fake-key' },
  });
  try {
    await openScope(kit, 'llm-deepseek');
    const turn = await kit.scoped('llm-deepseek', 'llm/stream', {
      provider: 'deepseek-official',
      request: { model: 'deepseek-v4-flash', messages: [{ role: 'user', content: [{ type: 'text', text: '你好' }] }] },
    });

    assert.deepEqual(turn.chunks.map((c) => c.type), [
      'block-start', 'reasoning-delta', 'block-start', 'text-delta', 'block-end', 'block-end', 'usage', 'finish',
    ]);
    assert.equal(turn.chunks.find((c) => c.type === 'reasoning-delta').text, '想一想');
    assert.equal(turn.chunks.find((c) => c.type === 'text-delta').text, '你好');
    assert.deepEqual(plainly(turn.chunks.at(-1).reason), { kind: 'stop' });
    assert.equal(turn.terminal.status, 'success');

    // Every chunk precedes the terminal: that is what "a stream read in order"
    // means, and the ledger depends on it.
    const order = kit.sent.filter((x) => x.call_id === turn.id).map((x) => x.message.type);
    assert.deepEqual(new Set(order.slice(0, -1)), new Set(['chunk']));

    // The credential came through the seat, not from this process's own
    // environment, and the adapter carried it to the endpoint.
    assert.deepEqual(plainly(kit.seen.seats), [
      { seat: 'credentials', method: 'resolveEnv', params: { ref: 'REBON_TEST_DS_KEY' } },
    ]);
    assert.equal(endpoint.seen.length, 1);
    assert.equal(endpoint.seen[0].headers.authorization, 'Bearer sk-local-fake-key');
    assert.equal(JSON.parse(endpoint.seen[0].body).model, 'deepseek-v4-flash');
  } finally {
    endpoint.close();
    await kit.stop();
  }
});

test('without a grant the turn ends as dsh own missing-credential refusal', async () => {
  const endpoint = await fakeDeepSeek();
  // The seat grants nothing, and this process's environment is deliberately
  // not consulted: the seat is the only way in.
  process.env.REBON_TEST_DS_KEY = 'sk-must-not-be-used';
  const { kit } = await composition({ baseURL: endpoint.baseURL });
  try {
    await openScope(kit, 'llm-deepseek');
    const turn = await kit.scoped('llm-deepseek', 'llm/stream', {
      provider: 'deepseek-official',
      request: { model: 'deepseek-v4-flash', messages: [{ role: 'user', content: [{ type: 'text', text: '你好' }] }] },
    });

    assert.deepEqual(turn.chunks, [], 'a refused turn emits nothing');
    assert.equal(turn.terminal.status, 'error');
    // dsh's own machine code survives the crossing — the terminal says what
    // went wrong, not merely that something did.
    assert.equal(turn.terminal.payload.code, 'MISSING_CREDENTIAL');
    assert.match(turn.terminal.payload.message, /no API key/);
    assert.equal(endpoint.seen.length, 0, 'nothing was sent to the endpoint');
  } finally {
    delete process.env.REBON_TEST_DS_KEY;
    endpoint.close();
    await kit.stop();
  }
});

test('an undeclared provider is refused before any of it runs', async () => {
  const endpoint = await fakeDeepSeek();
  // The manifest is the ceiling: a route rebon did not declare is refused at
  // load rather than becoming a capability nobody authorized.
  const { kit, load } = await composition({ baseURL: endpoint.baseURL, providers: [] });
  try {
    assert.equal(load.status, 'error');
    assert.equal(load.payload.code, '[UNAUTHORIZED_REGISTER]');
    assert.match(load.payload.message, /deepseek-official/);
  } finally {
    endpoint.close();
    await kit.stop();
  }
});

test('unloading the provider takes the route with it', async () => {
  const endpoint = await fakeDeepSeek();
  const { kit } = await composition({
    baseURL: endpoint.baseURL,
    env: { REBON_TEST_DS_KEY: 'sk-local-fake-key' },
  });
  try {
    await openScope(kit, 'llm-deepseek');
    const drain = await kit.control('plugin/unload', { pluginId: 'llm-deepseek' });
    assert.equal(drain.status, 'success');

    const turn = await kit.scoped('llm-deepseek', 'llm/stream', {
      provider: 'deepseek-official',
      request: { model: 'deepseek-v4-flash', messages: [] },
    });
    assert.equal(turn.terminal.payload.code, '[STALE_PROVIDER]');
  } finally {
    endpoint.close();
    await kit.stop();
  }
});
