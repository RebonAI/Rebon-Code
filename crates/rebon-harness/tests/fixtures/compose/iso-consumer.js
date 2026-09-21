// Isolate probe consumer: reports which `probe` implementation its
// scope resolves — the isolation acceptance signal.
import { emit } from 'rebon';

export const name = 'iso-consumer';
export const inject = ['probe'];

export function apply(ctx, config) {
  emit('iso-probe/seen', { tag: config?.tag ?? 'untagged', value: ctx.probe.value() });
}
