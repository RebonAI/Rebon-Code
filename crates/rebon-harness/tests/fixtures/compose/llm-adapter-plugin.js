// Composition e2e fixture: a dsh-shaped llm adapter plugin. Registers on
// `ctx.llm` exactly like dsh's llm-deepseek does (registerAdapter with a
// provider route list; only `stream()` is required). The stream honours
// `options.signal` so host-driven aborts are observable.
import { emit } from 'rebon';

export const name = 'fake-llm-adapter';
export const inject = ['llm'];

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

class FakeAdapter {
  listModels(_provider) {
    return Promise.resolve([
      { id: 'fake-large', contextWindow: 64000 },
      { id: 'slow-model' },
    ]);
  }

  async *stream(options) {
    if (options.model === 'slow-model') {
      yield { type: 'block-start', index: 0, blockType: 'text' };
      yield { type: 'text-delta', index: 0, text: '开始' };
      for (let i = 0; i < 400 && !options.signal?.aborted; i++) {
        await sleep(25);
      }
      const aborted = options.signal?.aborted === true;
      emit('llm-compose-test:ended', { aborted });
      yield { type: 'finish', reason: aborted ? 'aborted' : 'stop' };
      return;
    }
    yield { type: 'block-start', index: 0, blockType: 'reasoning' };
    yield { type: 'reasoning-delta', index: 0, text: '组合思考' };
    yield { type: 'block-end', index: 0 };
    yield { type: 'block-start', index: 1, blockType: 'text' };
    yield { type: 'text-delta', index: 1, text: '来自组合 adapter' };
    yield { type: 'block-end', index: 1 };
    yield { type: 'usage', usage: { inputTokens: 5, outputTokens: 7 } };
    yield { type: 'finish', reason: 'stop' };
  }
}

export function apply(ctx) {
  ctx.llm.registerAdapter(['compose-llm'], new FakeAdapter());
}
