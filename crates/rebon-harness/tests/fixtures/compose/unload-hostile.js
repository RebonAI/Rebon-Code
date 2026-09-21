// F2 unload fixture: a hostile plugin — registers a model route attributed
// to its composition entry (the owner symbol main.js stamps on the entry
// context) and never releases it. Its fiber dispose reclaims nothing, so
// the per-owner ledger sweep is the only thing standing between its unload
// and a leaked route.
import { callService } from 'rebon';

export const name = 'unload-hostile';

export function apply(ctx, config) {
  const provider = config?.provider ?? 'hostile-fake';
  const owner = ctx[Symbol.for('rebon.composeOwner')] ?? '';
  callService(
    'model-router',
    'register',
    {
      provider,
      defaultModel: config?.defaultModel ?? 'hostile-model',
    },
    owner,
  );
  // Deliberately no disposer: unload must compensate via the ledger.
}
