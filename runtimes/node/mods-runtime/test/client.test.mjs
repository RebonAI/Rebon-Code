import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createLoader, MARKER } from '../src/index.mjs';
import { moduleFunction, regionCells } from '../src/client.mjs';

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), 'fixtures', 'client-mod');

function scope() {
  const calls = [];
  const ctx = {
    workspaceRoot: '/work',
    async seat(seat, method, params) { calls.push({ method, params }); return {}; },
    async invoke() { return {}; },
  };
  return { ctx, calls };
}

async function load() {
  const loader = await createLoader({ next: async () => { throw new Error('not a mod?'); } });
  const sealed = await loader.load({
    pluginId: 'clientmod',
    root,
    entry: 'hooks/register.tsx',
    services: ['mod'],
    seats: ['mods'],
    config: { [MARKER]: { name: 'clientmod', version: '0.1.0', options: {}, commands: [], tools: [] } },
  });
  const { ctx, calls } = scope();
  await sealed.scopeHandlers[0](ctx);
  const service = sealed.serviceHandlers.get('mod');
  const fixture = await import(pathToFileURL(path.join(root, 'hooks', 'register.tsx')).href);
  fixture.setLabel('start');
  fixture.heard.length = 0;
  const render = () => service({ kind: 'render', component: 'Pane', surface: 'terminal', requestId: 'game', props: {}, viewport: { columns: 40, rows: 12 } }, ctx);
  return { loader, service, ctx, calls, fixture, render };
}

/// The text a drawn tree holds, depth first.
function textOf(node) {
  if (typeof node === 'string') return node;
  return (node.children ?? []).map(textOf).join('|');
}

function clientOf(tree) {
  if (typeof tree === 'string') return undefined;
  if (tree.type === 'Client') return tree;
  for (const child of tree.children ?? []) {
    const found = clientOf(child);
    if (found) return found;
  }
  return undefined;
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 5));

test('regionCells reads a count, a percentage, or all the room', () => {
  assert.equal(regionCells(10, 40), 10);
  assert.equal(regionCells(99, 40), 40, 'never past the room');
  assert.equal(regionCells('50%', 12), 6);
  assert.equal(regionCells(undefined, 7), 7);
  assert.equal(regionCells('wide', 7), 7);
  assert.equal(regionCells(-3, 7), 0);
});

test('moduleFunction takes the default, else the one PascalCase export', () => {
  const one = () => 1;
  assert.equal(moduleFunction({ default: one }), one);
  assert.equal(moduleFunction({ Game: one, helper: () => 2 }), one);
  assert.equal(moduleFunction({ A: one, B: one }), undefined, 'two candidates is no answer');
  assert.equal(moduleFunction({}), undefined);
});

test('a Client is drawn by its surface module, its region and props handed in', async () => {
  const { loader, render } = await load();
  try {
    const first = await render();
    const client = clientOf(first.tree);
    assert.deepEqual(client.props, { key: 'game', module: './game.tsx', width: 10, height: '50%' });
    assert.equal(textOf(client), 'loading', 'the first call sets its state up');
    await settle();
    const second = await render();
    assert.equal(textOf(clientOf(second.tree)), 'start 10x6 presses=0');
  } finally {
    await loader.unload('clientmod');
  }
});

test('setState asks for a redraw, once per turn of the loop', async () => {
  const { loader, render, calls } = await load();
  try {
    await render();
    await settle();
    const invalidates = calls.filter((call) => call.method === 'ui.invalidate');
    assert.ok(invalidates.length >= 1, 'the first setState asked for a redraw');
  } finally {
    await loader.unload('clientmod');
  }
});

test('a key reaches the instance, its state survives the redraw, and a post raises ui.message', async () => {
  const { loader, render, service, ctx, fixture } = await load();
  try {
    await render();
    await settle();
    const heard = await service({ kind: 'clientKey', requestId: 'game', element: 'game', key: { key: 'up' } }, ctx);
    assert.deepEqual(heard, { heard: true });
    assert.equal(textOf(clientOf((await render()).tree)), 'start 10x6 presses=1');
    await settle();
    assert.equal(fixture.heard.length, 1);
    assert.equal(fixture.heard[0].element, 'game');
    assert.equal(fixture.heard[0].module, './game.tsx');
    assert.deepEqual(fixture.heard[0].data, { key: 'up' });
  } finally {
    await loader.unload('clientmod');
  }
});

test('a ui.message hook answering { props } hands the instance its next props until the hook draws new ones', async () => {
  const { loader, render, service, ctx, fixture } = await load();
  try {
    await render();
    await settle();
    await service({ kind: 'clientKey', requestId: 'game', element: 'game', key: { key: 'x' } }, ctx);
    await settle();
    assert.match(textOf(clientOf((await render()).tree)), /^from-post /);
    fixture.setLabel('redrawn');
    assert.match(textOf(clientOf((await render()).tree)), /^redrawn /, 'new props from the hook win');
  } finally {
    await loader.unload('clientmod');
  }
});

test('a key for an instance that is not there is not heard', async () => {
  const { loader, service, ctx } = await load();
  try {
    assert.deepEqual(await service({ kind: 'clientKey', requestId: 'game', element: 'ghost', key: { key: 'a' } }, ctx), { heard: false });
  } finally {
    await loader.unload('clientmod');
  }
});

test('a drawing without the Client, or forget, unmounts it and stops its timers', async () => {
  const { loader, render, service, ctx, fixture } = await load();
  try {
    await render();
    const described = async () => (await service({ kind: 'describe' }, ctx)).clients;
    assert.equal(await described(), 1);
    fixture.setLabel('none');
    await render();
    assert.equal(await described(), 0);
    fixture.setLabel('start');
    await render();
    assert.equal(await described(), 1);
    await service({ kind: 'forget', requestId: 'game' }, ctx);
    assert.equal(await described(), 0);
  } finally {
    await loader.unload('clientmod');
  }
});

test('a module that throws stops, and says so where it was drawn', async () => {
  const { loader, render, fixture } = await load();
  try {
    fixture.setLabel('broken');
    assert.equal(textOf(clientOf((await render()).tree)), './broken.tsx stopped: kaboom');
  } finally {
    await loader.unload('clientmod');
  }
});

test('setState on every render with nothing between is a render loop, and the instance stops', async () => {
  const { loader, render, fixture } = await load();
  try {
    fixture.setLabel('loop');
    let text = '';
    for (let n = 0; n < 4; n += 1) text = textOf(clientOf((await render()).tree));
    assert.match(text, /stopped: setState on 3 renders in a row/);
  } finally {
    await loader.unload('clientmod');
  }
});

test('ui.focus: a hook may keep the ring, move it to another element, or let it land', async () => {
  const { loader, service, ctx } = await load();
  try {
    const focus = (element) => service({ kind: 'focus', component: 'Pane', requestId: 'game', element, origin: { kind: 'person' } }, ctx);
    assert.deepEqual(await focus('locked'), { deny: 'locked stays put' });
    assert.deepEqual(await focus('alias'), { element: 'real' });
    assert.deepEqual(await focus('ok'), { element: 'ok' });
    assert.deepEqual(await focus(undefined), { element: null }, 'off the elements');
  } finally {
    await loader.unload('clientmod');
  }
});
