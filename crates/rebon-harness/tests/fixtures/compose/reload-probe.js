// Live-reload e2e fixture: registers a model route from its config and
// announces every apply — so a test can tell a RESTARTED entry (apply
// count grows) from an UNTOUCHED one (count stays), and see config
// changes land (the route's defaultModel moves).
import { callService, emit } from 'rebon';

export const name = 'reload-probe';

export function apply(ctx, config) {
  const provider = config?.provider ?? 'reload-probe';
  emit('reload-probe/applied', { provider });
  ctx.effect(() => {
    callService('model-router', 'register', {
      provider,
      defaultModel: config?.defaultModel ?? 'probe-model',
    });
    return () => callService('model-router', 'unregister', { provider });
  });
}
