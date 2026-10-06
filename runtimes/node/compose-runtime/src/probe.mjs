// Mounts one Cordis plugin package on a realm of its own and says what it
// registered, or why it could not mount.
//
//   node --permission --allow-fs-read=... probe.mjs <package root> <entry> [config JSON]
//
// This is how rebon reads a DeepSeek Harness package published to npm, which
// carries no rebon manifest: the package is mounted against rebon's own
// seats with a sink that admits every name and records it, and what it
// recorded becomes the ceiling a person approves at install. A package that
// needs a service rebon does not offer fails here with that service's name —
// which is the compatibility answer, given before anything is installed.
//
// The caller runs it under Node's permission model (the package and this
// runtime readable, nothing writable, no processes): the package is someone
// else's code, executing for the first time.
//
// One line of JSON on stdout, whatever happens.
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
    const loaded = sink.loaded();
    return {
      ok: true,
      ...base,
      tools: loaded.tools.map((tool) => ({ name: tool.name, description: tool.description ?? '' })),
      services: loaded.services,
      llmProviders: loaded.llmProviders,
      eventTopics: loaded.eventTopics,
      commands: loaded.commands.map((command) => ({ name: command.name, description: command.description })),
      report: sink.report(),
    };
  } catch (cause) {
    return { ok: false, ...base, missing, code: cause?.code ?? '[ACTIVATE_FAILED]', error: String(cause?.message ?? cause) };
  }
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [root, entry, config] = process.argv.slice(2);
  let answer;
  try {
    answer = await probe(root, entry, config ? JSON.parse(config) : null);
  } catch (cause) {
    answer = { ok: false, code: '[PROBE_FAILED]', error: String(cause?.message ?? cause) };
  }
  process.stdout.write(`${JSON.stringify(answer)}\n`, () => process.exit(0));
}
