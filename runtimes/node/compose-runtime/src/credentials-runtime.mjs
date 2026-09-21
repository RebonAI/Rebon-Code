// The composition's `ctx.credentials` seat, on the plugin plane.
//
// dsh configuration carries credential *references* (environment-variable
// names); the provider owns the values. This resolves a reference through
// rebon's `credentials` seat, whose `resolveEnv` runs the fail-closed
// authorize waterfall before touching the environment — so an unauthorized or
// unset reference is simply "absent", and the consumer's own missing-credential
// diagnosis fires (dsh-llm's `MISSING_CREDENTIAL`) instead of this seat
// inventing one.
//
// Ported from `js/compose/credentials-runtime.js`. The one change is that the
// seat call is now a request across a process boundary, which means it is
// awaited — and that it can only be made from inside a call rebon is waiting
// on, or by the plugin being attached to a session. Both hold where a
// credential is actually wanted: inside a model turn rebon asked for, or inside
// one an agent loop is running in a session of its own. `bridge.mjs` names the
// refusal if anything ever tries from a disposer or a module top level.
import { Service } from 'cordis';
import { seat } from './bridge.mjs';

export default class RebonCredentials extends Service {
  constructor(ctx) {
    super(ctx, 'credentials');
  }

  /** dsh CredentialProvider.resolve: `{value, source}` or undefined. */
  async resolve(ref) {
    try {
      // `via` is this seat's caller — the plugin whose code wants the
      // credential. Inside a call that changes nothing; outside one it is what
      // lets an agent loop's own model turn resolve a key at all, attributed to
      // the plugin and session it belongs to.
      const out = await seat('credentials', 'resolveEnv', { ref: String(ref) }, { via: this.ctx });
      const value = out?.value;
      if (typeof value === 'string' && value.length > 0) {
        return { value, source: 'env' };
      }
    } catch {
      // A refusal from the seat (no authorizer, unset variable, bad ref) and
      // an absent value are the same fact to a consumer: not configured here.
    }
    return undefined;
  }

  /** dsh CredentialProvider.describe: facts only, never the value. */
  async describe(ref) {
    const hit = await this.resolve(ref);
    return { configured: hit !== undefined, ...(hit ? { source: hit.source } : {}), writable: false };
  }
}
