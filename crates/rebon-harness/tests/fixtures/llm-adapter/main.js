// R2 e2e fixture: a fake dsh-style llm adapter. Streams StreamChunk JSON
// back through the llm host ops; `slow-model` requests keep streaming until
// cancelled so the abort path is observable.
const core = globalThis.Deno.core;
const aborts = new Map();

function emit(id, obj) {
  core.ops.op_llm_emit(id, JSON.stringify(obj));
}

async function handle(id, request) {
  const chunk = (c) => emit(id, { chunk: c });
  if (request.model === 'slow-model') {
    let aborted = false;
    aborts.set(id, () => { aborted = true; });
    chunk({ type: 'block-start', index: 0, blockType: 'text' });
    chunk({ type: 'text-delta', index: 0, text: '开始' });
    for (let i = 0; i < 400 && !aborted; i++) {
      await new Promise((r) => setTimeout(r, 25));
    }
    chunk({ type: 'finish', reason: aborted ? 'aborted' : 'stop' });
    emit(id, { done: true });
    aborts.delete(id);
    // Side channel so the Rust test can assert the abort reached JS.
    core.ops.op_rebon_emit_event('llm-test:ended', JSON.stringify({ aborted }));
    return;
  }
  chunk({ type: 'block-start', index: 0, blockType: 'reasoning' });
  chunk({ type: 'reasoning-delta', index: 0, text: '思考中' });
  chunk({ type: 'block-end', index: 0 });
  chunk({ type: 'block-start', index: 1, blockType: 'text' });
  chunk({ type: 'text-delta', index: 1, text: '你好' });
  chunk({ type: 'text-delta', index: 1, text: '！' });
  chunk({ type: 'block-end', index: 1 });
  chunk({ type: 'usage', usage: { inputTokens: 12, outputTokens: 34, reasoningTokens: 3 } });
  chunk({ type: 'finish', reason: 'stop' });
  emit(id, { done: true });
}

while (true) {
  const instr = JSON.parse(await core.ops.op_llm_next());
  if (instr.closed) break;
  if (instr.kind === 'cancel') {
    aborts.get(instr.id)?.();
    continue;
  }
  handle(instr.id, instr.request);
}
