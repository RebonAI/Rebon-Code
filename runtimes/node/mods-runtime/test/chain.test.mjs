import test from 'node:test';
import assert from 'node:assert/strict';
import { HookChain, matcherSelects, patternSelects } from '../src/chain.mjs';

const engine = Object.freeze({});
const core = async (e) => ({ answered: 'core', input: e });

test('patterns: a name, a glob, the star and a negation', () => {
  assert.ok(patternSelects('tool.call', 'tool.call'));
  assert.ok(!patternSelects('tool.call', 'tool.check'));
  assert.ok(patternSelects('tool.*', 'tool.call'));
  assert.ok(patternSelects('*', 'prompt.submit'));
  assert.ok(!patternSelects('*', 'telemetry.log'));
  assert.ok(patternSelects('!tool.describe', 'tool.call'));
  assert.ok(!patternSelects('!tool.describe', 'tool.describe'));
  assert.ok(!patternSelects('!tool.describe', 'telemetry.mark'));
});

test('matchers: literals, RegExp, lists, objects and array values', () => {
  assert.ok(matcherSelects(undefined, { anything: 1 }));
  assert.ok(matcherSelects({ tool: 'Bash' }, { tool: 'Bash', input: {} }));
  assert.ok(!matcherSelects({ tool: 'Bash' }, { tool: 'Read' }));
  assert.ok(matcherSelects({ tool: /^mcp__/ }, { tool: 'mcp__x__y' }));
  assert.ok(matcherSelects({ tool: ['Bash', 'Read'] }, { tool: 'Read' }));
  assert.ok(matcherSelects({ props: { origin: { kind: 'user' } } }, { props: { origin: { kind: 'user' }, x: 1 } }));
  assert.ok(!matcherSelects({ props: { origin: { kind: 'user' } } }, { props: {} }));
  assert.ok(matcherSelects({ tags: 'b' }, { tags: ['a', 'b'] }));
  assert.ok(!matcherSelects({ to: 'collector' }, { to: 'anthropic' }));
});

test('hooks run in registration order and next() reaches core with rewrites', async () => {
  const chain = new HookChain();
  const seen = [];
  chain.on('x', async ($, e, next) => { seen.push('first'); return next({ ...e, n: e.n + 1 }); });
  chain.on('x', { n: 2 }, async ($, e, next) => { seen.push('second'); return next({ ...e, n: e.n * 10 }); });
  chain.on('y', async () => { seen.push('never'); });
  const { result, reachedCore, coreInput } = await chain.dispatch('x', { n: 1 }, core, { engine });
  assert.deepEqual(seen, ['first', 'second']);
  assert.ok(reachedCore);
  assert.deepEqual(coreInput, { n: 20 });
  assert.deepEqual(result, { answered: 'core', input: { n: 20 } });
});

test('a hook that returns without next answers for itself', async () => {
  const chain = new HookChain();
  let coreRan = false;
  chain.on('x', async () => ({ deny: 'no' }));
  chain.on('x', async ($, e, next) => next(e));
  const { result, reachedCore } = await chain.dispatch('x', {}, async () => { coreRan = true; return {}; }, { engine });
  assert.deepEqual(result, { deny: 'no' });
  assert.ok(!reachedCore);
  assert.ok(!coreRan);
});

test('a value returned after next replaces the answer from beneath', async () => {
  const chain = new HookChain();
  chain.on('x', async ($, e, next) => { const below = await next(e); return { ...below, wrapped: true }; });
  const { result } = await chain.dispatch('x', { n: 1 }, core, { engine });
  assert.deepEqual(result, { answered: 'core', input: { n: 1 }, wrapped: true });
});

test('e is frozen for the hook', async () => {
  const chain = new HookChain();
  chain.on('x', async ($, e, next) => { assert.ok(Object.isFrozen(e)); assert.ok(Object.isFrozen(e.inner)); return next(e); });
  await chain.dispatch('x', { inner: { a: 1 } }, core, { engine });
});

test('a failing hook is skipped and reported; its .catch may answer instead', async () => {
  const failures = [];
  const chain = new HookChain({ onFailure: (f) => failures.push(f) });
  chain.on('x', async () => { throw new Error('boom'); });
  chain.on('x', async ($, e, next) => next({ ...e, after: true }));
  const first = await chain.dispatch('x', { n: 1 }, core, { engine });
  assert.deepEqual(first.coreInput, { n: 1, after: true });
  assert.equal(failures.length, 1);
  assert.equal(failures[0].pattern, 'x');

  const caught = new HookChain({ onFailure: (f) => failures.push(f) });
  caught.on('x', async () => { throw new Error('boom'); }).catch(async ($, e, next) => ({ fallback: next.error.message }));
  const second = await caught.dispatch('x', {}, core, { engine });
  assert.deepEqual(second.result, { fallback: 'boom' });
});

test('a hook past its budget is skipped and the chain continues', async () => {
  const failures = [];
  const chain = new HookChain({ onFailure: (f) => failures.push(f) });
  chain.on('x', async ($, e, next) => {
    await new Promise((resolve) => setTimeout(resolve, 200));
    return next(e);
  });
  const { reachedCore } = await chain.dispatch('x', {}, core, { engine, budgetMs: 20 });
  assert.ok(reachedCore);
  assert.match(failures[0].error.message, /HOOK_BUDGET/);
});

test('patterns() lists each registered pattern once, in order', () => {
  const chain = new HookChain();
  chain.on('b', () => {});
  chain.on('a', { x: 1 }, () => {});
  chain.on('b', () => {});
  assert.deepEqual(chain.patterns(), ['b', 'a']);
  assert.ok(chain.listens('a'));
  assert.ok(!chain.listens('c'));
});

test('on() refuses a missing hook and a non-object matcher', () => {
  const chain = new HookChain();
  assert.throws(() => chain.on('x'), TypeError);
  assert.throws(() => chain.on('x', 'Bash', () => {}), TypeError);
});
