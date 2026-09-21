// Consumer-direction probe: a composed plugin CONSUMES ctx.web (dsh
// API) against whatever provider arbitration selects. Runs its search at
// apply time and reports the outcome as a kernel event, exercising
// resolveProvider + the provider's real transport + the seat's capSources
// rule (maxResults: 1 against a two-result endpoint ⇒ truncated).
import { emit } from 'rebon';

export const name = 'web-consumer';
export const inject = ['web'];

export function apply(ctx) {
  void (async () => {
    try {
      const result = await ctx.web.search({ query: 'consumer probe', maxResults: 1 });
      emit('web-probe/result', {
        sources: result.sources,
        truncated: result.truncated === true,
      });
    } catch (e) {
      emit('web-probe/result', { error: String(e?.message ?? e), code: e?.code });
    }
  })();
}
