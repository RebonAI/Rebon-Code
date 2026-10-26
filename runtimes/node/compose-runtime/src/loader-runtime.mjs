// The composition is DeepSeek Harness's Loader, as far as a plugin can tell.
//
// In dsh a profile (`cordis.yml` plus its patches) is loaded by
// `cordis-plugin-loader`, and the boot packages built on it — the
// configuration editor, the profile context — are what upstream plugins
// reach when they need to know what is mounted or to change its config.
// `@deepseek-ai/dsh-settings` is the one that matters most: its settings
// forms are entry configs, read through `ctx.configEditor` and written back
// through it.
//
// In rebon the profile is `kernelPlugins` and the loader is the plugin plane:
// rebon reads the entry list, the realm mounts one entry per `plugin/load`.
// So the three services here answer the dsh vocabulary from the realm's own
// table rather than mounting dsh's loader beside it — a second loader would
// be a second answer to "what is running":
//
//   * `loader` — the mounted entries, and `await()` for the loads in flight;
//   * `configEditor` — an entry's config as two layers (nothing inherited;
//     the user's layer is its `kernelPlugins` row), and `edit`, which checks
//     the next config against the plugin's schema, persists it as the entry
//     itself through the `settings` seat, and commits its volatile fields
//     into the running plugin the way dsh's loader does;
//   * `profileContext` — the profile's home, `<config home>/dsh`, rebon's
//     counterpart of `DSH_HOME`.
//
// What is deliberately not here: the loader's create/remove/reorder API.
// Which entries exist is rebon's composition, changed by installing and
// configuring, not by a plugin.
import { Context, Service, resolveConfig } from 'cordis';
import { deepEqual, updateVolatile, volatileEntries } from 'cosmokit';
import { seatFor } from './bridge.mjs';
import { entries, settled } from './entries.mjs';

class LoaderError extends Error {
  constructor(code, message) {
    super(`${code} ${message}`);
    this.code = code;
    this.name = 'LoaderError';
  }
}

const clone = (value) => (value === undefined ? {} : structuredClone(value));

export class RebonLoader extends Service {
  constructor(ctx) {
    super(ctx, 'loader');
  }

  /// Every mounted entry, in load order.
  entries() {
    return [...entries.values()];
  }

  /// Resolves once no entry is being mounted.
  await() {
    return settled();
  }
}

export class RebonConfigEditor extends Service {
  constructor(ctx, profile) {
    super(ctx, 'configEditor');
    this.profile = profile ?? {};
  }

  /// The file a person edits by hand, when rebon named one.
  get documentPath() {
    return typeof this.profile.documentPath === 'string' ? this.profile.documentPath : undefined;
  }

  entries() {
    return [...entries.values()];
  }

  /// dsh's two layers per entry. Rebon has one: what the entry's row says.
  configuration() {
    return this.entries().map((entry) => ({
      entry,
      inherited: {},
      override: clone(entry.options.config),
    }));
  }

  /// Validates, persists and applies an entry's next config.
  ///
  /// The order is dsh's: a config the plugin's own schema refuses is
  /// refused before anything is written, and the running plugin changes
  /// only after rebon has the new config.
  ///
  /// What changes in the running plugin is what dsh's loader changes without
  /// a remount: the `.volatile()` fields, committed into the references the
  /// plugin reads them through, and announced as `loader/volatile-update` to
  /// that plugin alone. An ordinary field is a remount in dsh. Here it waits
  /// for rebon to load the entry again: what an entry registers is reported
  /// once per load, so a plugin restarted in place could not re-register.
  /// Settings forms only ever edit volatile fields.
  async edit(entry, change) {
    if (entries.get(entry?.id) !== entry || entry.fiber === undefined) {
      throw new LoaderError('[UNKNOWN_ENTRY]', 'that configuration entry is no longer mounted');
    }
    const fiber = entry.fiber;
    if (fiber.state !== 2) {
      throw new LoaderError('[ENTRY_INACTIVE]', `${entry.id} is not active`);
    }
    const next = change(clone(entry.options.config), {});
    const candidate = resolveConfig(fiber.runtime, next);
    await seatFor(fiber.ctx, 'settings', 'entryConfig', { config: next });
    entry.options.config = next;
    if (!deepEqual(fiber.config, candidate, true)) return;
    const paths = volatileEntries(fiber.config).flatMap(({ path, ref }) => {
      const source = path.reduce((value, key) => Reflect.get(value, key), candidate);
      if (deepEqual(ref.get(), source.get(), true)) return [];
      updateVolatile(ref, source);
      return [path];
    });
    if (paths.length === 0) return;
    const self = Object.create(fiber.ctx);
    self[Context.filter] = (owner) => owner.fiber?.uid === fiber.uid;
    fiber.ctx.emit(self, 'loader/volatile-update', paths);
  }
}

export class RebonProfileContext extends Service {
  constructor(ctx, profile) {
    super(ctx, 'profileContext');
    this.home = profile?.home;
    this.name = profile?.name ?? 'rebon';
    this.dir = this.home;
  }
}
