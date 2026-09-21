// Embedded-runtime shim of `@deepseek-ai/dsh-settings`.
//
// The composition host has no user-settings document and no hot reload: a
// plugin's configuration is its composition entry, fixed for the process
// lifetime. `installSettingsSection` therefore resolves statically — the
// source is the entry config, `onChange` never fires — which matches dsh's
// own behavior in a composition that mounts no settings service (its real
// implementation rides `ctx.inject(['settings'], …)` and simply never runs).

const NAMESPACE_PATTERN = /^[a-z][a-z0-9-]*$/;

/** Brand a raw string as a SettingsNamespace (lowercase kebab-case). */
export function settingsNamespace(value) {
  if (!NAMESPACE_PATTERN.test(value)) {
    throw new TypeError(`settings namespace "${value}" must match ${String(NAMESPACE_PATTERN)}`);
  }
  return value;
}

/** Structural JSON equality (dsh deepEqualJson semantics). */
export function deepEqualJson(a, b) {
  if (a === b) return true;
  if (typeof a !== typeof b) return false;
  if (a === null || b === null || typeof a !== 'object') return false;
  const aIsArray = Array.isArray(a);
  if (aIsArray !== Array.isArray(b)) return false;
  if (aIsArray) {
    if (a.length !== b.length) return false;
    return a.every((item, i) => deepEqualJson(item, b[i]));
  }
  const aKeys = Object.keys(a);
  const bKeys = Object.keys(b);
  if (aKeys.length !== bKeys.length) return false;
  return aKeys.every((key) => Object.hasOwn(b, key) && deepEqualJson(a[key], b[key]));
}

/**
 * Static section install: the entry config is the whole source, once.
 * `onChange` is deliberately never invoked — there is nothing to change.
 */
export function installSettingsSection(_ctx, _ns, _schema, entry, hooks) {
  hooks.setSource(() => entry);
}
