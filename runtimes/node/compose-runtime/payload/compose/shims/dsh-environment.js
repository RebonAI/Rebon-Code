// Embedded-runtime shim of `@deepseek-ai/dsh-environment`, which DeepSeek
// Harness packages read their API keys and base URLs through
// (`environmentOf(ctx).get('EXA_API_KEY')`). The real package is not
// published, so a package installed from npm reaches this one.
//
// The embedded composition exposes no ambient environment (see
// dsh-launch-environment.js): a variable is visible only when the person
// granted it to this plugin's container at install, which the container
// announces in `REBON_GRANTED_ENV`. On the shared host nothing is granted,
// and every consumer takes its documented fallback — its own config, or the
// credentials seat.

const granted = new Set(
  (process.env.REBON_GRANTED_ENV ?? '')
    .split(',')
    .map((name) => name.trim())
    .filter(Boolean),
);

const SNAPSHOT = Object.freeze({
  get(name) {
    if (!granted.has(name)) return undefined;
    const value = process.env[name];
    return value === undefined ? undefined : Object.freeze({ name, value, source: 'container' });
  },
  has(name) {
    return SNAPSHOT.get(name) !== undefined;
  },
});

/** The environment a plugin may read: what its container was granted. */
export function environmentOf(_ctx) {
  return SNAPSHOT;
}
