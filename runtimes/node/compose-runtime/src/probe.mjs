// Mounts Cordis plugin packages on a realm of their own and says what each
// registered, or why it could not mount.
//
//   rebon-verve --permission --allow-fs-read=... probe.mjs <package root> <entry> [config JSON]
//   rebon-verve --permission --allow-fs-read=... probe.mjs --bundle <bundle root> [configs JSON]
//
// This is how rebon reads a DeepSeek Harness package published to npm, which
// carries no rebon manifest: the package is mounted against rebon's own
// seats with a sink that admits every name and records it, and what it
// recorded becomes the ceiling a person approves at install. A package that
// needs a service rebon does not offer fails here with that service's name —
// which is the compatibility answer, given before anything is installed.
//
// A *bundle* is several packages installed into one `node_modules` and run in
// one realm, because that is how dsh packages reach each other: a seam
// implementation (`dsh-fs-local`) provides the service a consumer
// (`dsh-tool-fs`) injects. Its members are the rows of the bundle's dsh
// patch (`package.json` `dsh.bundle.patch`, the format dsh's own profile
// bundles use), or, without one, its dependencies that are Cordis plugins.
// They are mounted in the order their injections allow, and each member's
// report says which realm services it provided, so rebon can keep that order
// when it loads them.
//
// The caller runs it under the permission model (the packages and this
// runtime readable, nothing writable, no processes): the code is someone
// else's, executing for the first time.
//
// One line of JSON on stdout, whatever happens.
import fs from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { installModuleResolution } from './resolve.mjs';

function injects(plugin) {
  const inject = plugin?.inject;
  if (Array.isArray(inject)) return { required: inject, optional: [] };
  if (inject && typeof inject === 'object') {
    return { required: inject.required ?? [], optional: inject.optional ?? [] };
  }
  return { required: [], optional: [] };
}

/// What one mounted sink reports, in the shape the manifest is written from.
function registered(sink) {
  const loaded = sink.loaded();
  return {
    tools: loaded.tools.map((tool) => ({ name: tool.name, description: tool.description ?? '' })),
    services: loaded.services,
    llmProviders: loaded.llmProviders,
    eventTopics: loaded.eventTopics,
    commands: loaded.commands.map((command) => ({ name: command.name, description: command.description })),
    report: sink.report(),
  };
}

async function probe(root, entry, config) {
  installModuleResolution();
  const { cordisPluginOf } = await import('./loader.mjs');
  const { createRealm, mountEntry, builtinSeats } = await import('./realm.mjs');
  let module;
  try {
    module = await import(pathToFileURL(path.resolve(root, entry)).href);
  } catch (cause) {
    return { ok: false, code: '[ENTRY_FAILED]', error: String(cause?.message ?? cause) };
  }
  const plugin = cordisPluginOf(module);
  if (plugin === undefined) {
    return { ok: false, code: '[NOT_CORDIS]', error: `${entry} exports no Cordis plugin (no apply)` };
  }
  const wanted = injects(plugin);
  await createRealm({});
  const seats = builtinSeats();
  const missing = wanted.required.filter((service) => !seats.includes(service));
  const base = { name: plugin.name ?? null, inject: wanted, seats };
  try {
    const sink = await mountEntry({ pluginId: 'probe', config }, plugin, { probe: true });
    return { ok: true, ...base, ...registered(sink) };
  } catch (cause) {
    return { ok: false, ...base, missing, code: cause?.code ?? '[ACTIVATE_FAILED]', error: String(cause?.message ?? cause) };
  }
}

// ---- bundles ------------------------------------------------------------

const readJson = (file) => JSON.parse(fs.readFileSync(file, 'utf8'));

/// The module a package runs: `main`, else the `.` export, else `index.js` —
/// the rule rebon's installer uses (`dsh_npm::entry_of`).
function entryOf(manifest) {
  if (typeof manifest.main === 'string') return manifest.main.replace(/^\.\//, '');
  const dot = manifest.exports?.['.'];
  const fromExports = typeof dot === 'string' ? dot : (dot?.default ?? dot?.import);
  return typeof fromExports === 'string' ? fromExports.replace(/^\.\//, '') : 'index.js';
}

/// Whether a parsed config holds a dsh `!!js` expression anywhere.
function holdsExpression(value) {
  if (value === null || typeof value !== 'object') return false;
  if (Object.hasOwn(value, '__jsExpr')) return true;
  return Object.values(value).some(holdsExpression);
}

/// The rows a bundle's dsh patch inserts, with later `{id, config}` rows
/// replacing a row's config the way dsh applies a patch layer.
function patchRows(root, manifest) {
  const patch = manifest.dsh?.bundle?.patch;
  if (typeof patch !== 'string') return undefined;
  // dsh writes its patches in YAML and parses them with the `yaml` package,
  // which every bundle that carries one installs; `!!js` values are dsh
  // Loader expressions and are kept as markers, not evaluated.
  const yaml = createRequire(path.join(root, 'package.json'))('yaml');
  const text = fs.readFileSync(path.resolve(root, patch), 'utf8');
  const doc = yaml.parse(text, {
    customTags: [{ tag: 'tag:yaml.org,2002:js', resolve: (source) => ({ __jsExpr: source }) }],
  });
  const rows = [];
  for (const op of Array.isArray(doc) ? doc : []) {
    if (Array.isArray(op?.insert)) {
      for (const row of op.insert) {
        if (row && typeof row.name === 'string') rows.push({ id: row.id ?? row.name, name: row.name, config: row.config });
      }
    } else if (op && typeof op.id === 'string' && Object.hasOwn(op, 'config')) {
      const row = rows.find((candidate) => candidate.id === op.id);
      if (row) row.config = op.config;
    } else if (op && typeof op.remove === 'string') {
      const index = rows.findIndex((candidate) => candidate.id === op.remove);
      if (index >= 0) rows.splice(index, 1);
    }
  }
  return rows;
}

/// The members of a bundle: its patch rows, or its dependencies.
function bundleMembers(root) {
  const manifest = readJson(path.join(root, 'package.json'));
  const rows = patchRows(root, manifest);
  if (rows !== undefined) return rows;
  return Object.keys(manifest.dependencies ?? {}).map((name) => ({ id: name.split('/').pop(), name }));
}

async function probeBundle(root, overrides = {}) {
  installModuleResolution();
  const { cordisPluginOf } = await import('./loader.mjs');
  const { createRealm, mountEntry, builtinSeats } = await import('./realm.mjs');
  const live = await createRealm({});
  const skipped = [];
  const candidates = [];
  for (const row of bundleMembers(root)) {
    const dir = path.join(root, 'node_modules', ...row.name.split('/'));
    let manifest;
    try {
      manifest = readJson(path.join(dir, 'package.json'));
    } catch {
      skipped.push({ id: row.id, name: row.name, code: '[NOT_INSTALLED]', error: `${row.name} is not installed in the bundle` });
      continue;
    }
    const config = Object.hasOwn(overrides, row.id) ? overrides[row.id] : row.config;
    if (holdsExpression(config)) {
      skipped.push({ id: row.id, name: row.name, code: '[CONFIG_EXPRESSION]', error: `${row.id}'s config is a dsh Loader expression, which rebon does not evaluate` });
      continue;
    }
    const entry = path.posix.join('node_modules', row.name, entryOf(manifest));
    let module;
    try {
      module = await import(pathToFileURL(path.join(root, entry)).href);
    } catch (cause) {
      skipped.push({ id: row.id, name: row.name, code: '[ENTRY_FAILED]', error: String(cause?.message ?? cause) });
      continue;
    }
    const plugin = cordisPluginOf(module);
    // A dependency that is a library, not a plugin, is simply not a member.
    if (plugin === undefined) continue;
    candidates.push({ ...row, entry, config, plugin, inject: injects(plugin) });
  }

  // Every name any member injects, so what each one provided can be read off
  // the realm before and after it mounts.
  const watched = [...new Set(candidates.flatMap((member) => [...member.inject.required, ...member.inject.optional]))];
  const present = () => new Set(watched.filter((name) => live.ctx.get(name) !== undefined));
  const entries = [];
  const failed = new Set();
  let progress = true;
  while (progress) {
    progress = false;
    for (const member of candidates) {
      if (failed.has(member) || entries.some((done) => done.id === member.id)) continue;
      if (member.inject.required.some((name) => live.ctx.get(name) === undefined)) continue;
      const before = present();
      try {
        const sink = await mountEntry({ pluginId: member.id, name: member.name, config: member.config ?? undefined }, member.plugin, { probe: true });
        const after = present();
        entries.push({
          id: member.id,
          name: member.name,
          entry: member.entry,
          config: member.config ?? null,
          inject: member.inject,
          provides: [...after].filter((name) => !before.has(name)),
          ...registered(sink),
        });
        progress = true;
      } catch (cause) {
        failed.add(member);
        skipped.push({ id: member.id, name: member.name, code: cause?.code ?? '[ACTIVATE_FAILED]', error: String(cause?.message ?? cause) });
      }
    }
  }
  const seats = builtinSeats();
  for (const member of candidates) {
    if (failed.has(member) || entries.some((done) => done.id === member.id)) continue;
    const missing = member.inject.required.filter((name) => live.ctx.get(name) === undefined);
    skipped.push({
      id: member.id,
      name: member.name,
      code: '[MISSING_INJECT]',
      missing,
      error: `${member.id} requires ${missing.map((name) => JSON.stringify(name)).join(', ')}, which neither rebon nor another member of the bundle provides`,
    });
  }
  return { ok: entries.length > 0, bundle: true, seats, entries, skipped };
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const args = process.argv.slice(2);
  let answer;
  try {
    if (args[0] === '--bundle') {
      answer = await probeBundle(args[1], args[2] ? JSON.parse(args[2]) : {});
    } else {
      const [root, entry, config] = args;
      answer = await probe(root, entry, config ? JSON.parse(config) : null);
    }
  } catch (cause) {
    answer = { ok: false, code: '[PROBE_FAILED]', error: String(cause?.message ?? cause) };
  }
  process.stdout.write(`${JSON.stringify(answer)}\n`, () => process.exit(0));
}
