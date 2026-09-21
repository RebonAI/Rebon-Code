// A composition entry acting on its own schedule, rather than answering a call.
//
// `bridge.mjs` decides who is calling by looking at the async store first and
// falling back to `sessionOf(via)` — the session the entry is attached to. Code
// on a timer, an agent loop driving its own turn, an adapter fetching a
// credential: none of those is inside a call rebon made, so all of them depend
// on the fallback, and the fallback depends on a scope being open.
//
// These two pin that, because the difference between them is invisible from
// rebon's side. A plugin whose call fails this way throws inside its own
// isolate; an unhandled rejection there does not stop the host, so nothing
// reaches the worker's log and the symptom is silence. That is what these
// tests exist to make loud.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { loadCompose, loadEntry, openScope, plainly, startHost } from './harness.mjs';

function cordisPackage(body) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'rebon-own-schedule-'));
  fs.writeFileSync(path.join(root, 'entry.mjs'), body);
  return { root: root.replace(/\\/g, '/'), entry: 'entry.mjs' };
}

// The invoke happens on a timer, outside any inbound call. The registered tool
// only reports what happened afterwards, which does not change how the invoke
// itself was identified.
const PROBE = `
import { defineTool } from '@deepseek-ai/dsh-tools';
import { invokeTool, logger } from 'rebon';
export const name = 'probe';
export const inject = ['tools'];
export function apply(ctx, config) {
  let result = { state: 'pending' };
  setTimeout(async () => {
    try {
      const answer = await invokeTool('read_file', { path: 'x' }, { via: ctx });
      result = { state: 'ok', answer: JSON.stringify(answer) };
    } catch (e) {
      result = { state: 'threw', error: String(e?.message ?? e) };
    }
    // Said on the plugin's own schedule: without the logger seat this would go
    // to stderr and be read only if the host died.
    logger.info('probe finished: ' + result.state, { via: ctx });
  }, Number(config?.delayMs ?? 60));
  ctx.tools.register(defineTool({
    name: 'probe_report',
    description: 'Reports what the timer-driven invoke did.',
    parameters: {},
    output: {
      schema: { type: 'object', additionalProperties: true, properties: {} },
      render: (_a, value) => [{ type: 'text', text: JSON.stringify(value) }],
    },
    execute: async () => result,
  }));
}
`;

async function reportFrom(kit) {
  await new Promise((resolve) => setTimeout(resolve, 400));
  const call = await kit.scoped('probe', 'tool/call', { tool: 'probe_report', input: {} });
  const text = plainly(call.terminal.payload)?.content?.[0]?.text;
  return { text, value: JSON.parse(text ?? '{}') };
}

async function loadProbe(kit) {
  await loadCompose(kit, { entries: [{ id: 'probe' }] });
  await loadEntry(kit, {
    id: 'probe',
    ...cordisPackage(PROBE),
    tools: ['probe_report'],
    invokableTools: ['read_file'],
    // Logging is a seat call like any other, so it is declared like any other.
    // Without this the host refuses it before it leaves the process, and the
    // line falls to stderr — the failure this test is about — so it is worth
    // stating rather than inheriting.
    seats: ['logger'],
    config: { delayMs: 60 },
  });
}

test('an entry with an open scope invokes a tool on its own schedule', async () => {
  const kit = await startHost({ tools: { read_file: () => ({ text: 'file contents' }) } });
  try {
    await loadProbe(kit);
    await openScope(kit, 'probe');
    const { text, value } = await reportFrom(kit);
    assert.equal(value.state, 'ok', `expected the invoke to reach the tool; got ${text}`);
  } finally {
    await kit.stop();
  }
});

test('what the entry says on its own schedule reaches the logger seat', async () => {
  // The other half of the same problem. Without the seat this line would go to
  // the host's stderr, which rebon keeps as a short tail and prints only when
  // the host dies — so a plugin could describe exactly what it was doing and
  // nobody would ever read it.
  const kit = await startHost({ tools: { read_file: () => ({ text: 'file contents' }) } });
  try {
    await loadProbe(kit);
    await openScope(kit, 'probe');
    await reportFrom(kit);

    const logs = kit.seen.seats.filter((call) => call.seat === 'logger');
    assert.ok(logs.length >= 1, 'the timer-driven line reached the logger seat');
    const said = logs.map((call) => plainly(call.params)?.message).join(' | ');
    assert.match(said, /probe finished: ok/);
    assert.equal(logs[0].method, 'info', 'the level the plugin asked for is the level sent');
  } finally {
    await kit.stop();
  }
});

test('a plugin logging in a loop is capped, and the drop is counted out loud', async () => {
  // A plugin that says the same thing four hundred times must not fill the
  // worker's log. The cap is a bucket per level; what makes it safe rather than
  // merely quiet is that the drop is reported, so a reader sees a gap instead
  // of an unexplained absence.
  const kit = await startHost({ tools: { read_file: () => ({ text: 'x' }) } });
  try {
    await loadCompose(kit, { entries: [{ id: 'shouty' }] });
    await loadEntry(kit, {
      id: 'shouty',
      ...cordisPackage(`
import { logger } from 'rebon';
export const name = 'shouty';
export function apply(ctx, config) {
  setTimeout(() => {
    for (let i = 0; i < 400; i += 1) logger.info('line ' + i, { via: ctx });
  }, 20);
}
`),
      seats: ['logger'],
    });
    await openScope(kit, 'shouty');
    await new Promise((resolve) => setTimeout(resolve, 300));

    const logs = kit.seen.seats.filter((call) => call.seat === 'logger');
    const messages = logs.map((call) => plainly(call.params)?.message ?? '');
    const carried = messages.filter((m) => m.startsWith('line ')).length;
    const notices = messages.filter((m) => m.includes('dropped'));

    assert.ok(carried < 400, `the cap held: ${carried} of 400 got through`);
    assert.ok(carried > 0, 'the cap is a cap, not a mute button');
    assert.ok(notices.length >= 1, 'the drop is reported rather than silent');
    assert.match(notices[0], /\d+ info line\(s\) dropped/);
  } finally {
    await kit.stop();
  }
});

test('a line with no session and no call still goes somewhere rather than throwing', async () => {
  // `via` on an entry that is attached to nothing resolves to no session, and
  // logging must not fail the plugin over that: it falls back to stderr, which
  // is a worse channel but is not an exception in the middle of someone's
  // teardown path.
  const kit = await startHost({ tools: { read_file: () => ({ text: 'file contents' }) } });
  try {
    await loadProbe(kit);
    // no `scope/open`
    const { value } = await reportFrom(kit);
    assert.equal(value.state, 'threw', 'the invoke still refuses without a session');
    const logs = kit.seen.seats.filter((call) => call.seat === 'logger');
    assert.equal(logs.length, 0, 'with nothing to reach, the line took the stderr path');
  } finally {
    await kit.stop();
  }
});

test('without a scope the same entry is told it is attached to nothing', async () => {
  const kit = await startHost({ tools: { read_file: () => ({ text: 'file contents' }) } });
  try {
    await loadProbe(kit);
    // No `scope/open`. This is also what `apply` itself sees: a plugin's body
    // runs during `plugin/load`, before any scope exists, so an invoke made
    // there fails this way by design rather than by accident.
    const { text, value } = await reportFrom(kit);
    assert.equal(value.state, 'threw', `expected a refusal; got ${text}`);
    assert.match(value.error, /NO_CALL_CONTEXT/);
    assert.match(value.error, /not attached to a session/);
  } finally {
    await kit.stop();
  }
});
