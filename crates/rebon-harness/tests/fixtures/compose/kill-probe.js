// F1 kill-path fixture: registers a model route (with a well-behaved
// disposer) and a raw kernel event subscription it never drops. A hard kill
// skips both JS teardown paths — the bridge effect ledger and the
// composition-fork disposal must reclaim everything.
import { callService, subscribe } from 'rebon';

export const name = 'kill-probe';

export function apply(ctx, config) {
  const provider = config?.provider ?? 'kill-probe';
  ctx.effect(() => {
    callService('model-router', 'register', {
      provider,
      defaultModel: config?.defaultModel ?? 'kp-model',
    });
    return () => callService('model-router', 'unregister', { provider });
  });
  // Deliberately leaked: no cordis effect, no unsubscribe.
  subscribe('kill-probe/evt');
}
