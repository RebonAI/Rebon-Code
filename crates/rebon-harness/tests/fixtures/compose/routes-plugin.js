// Composition e2e fixture: a Cordis plugin that registers a model route on
// the kernel when applied and unregisters it on dispose — proving composed
// plugins reach the R1 seam and that teardown revokes what they added.
import { callService } from 'rebon';

export const name = 'test-routes';

export function apply(ctx, config) {
  const provider = config?.provider ?? 'compose-fake';
  // ctx.effect: the returned closure is the disposer — Cordis's RAII seam.
  ctx.effect(() => {
    callService('model-router', 'register', {
      provider,
      defaultModel: config?.defaultModel ?? 'compose-model',
    });
    return () => callService('model-router', 'unregister', { provider });
  });
}
