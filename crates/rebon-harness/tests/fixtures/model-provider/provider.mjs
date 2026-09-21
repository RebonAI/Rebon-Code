// A package model provider on the plugin plane, as small as the contract allows.
//
// Stands in for a real one (`runtimes/node/plugins/deepseek-responses`) in the e2e test: it
// speaks the same `rebon.modelProvider` v1 payload vocabulary — a
// `ModelProviderTurnV1` in, `StreamEventV1` chunks out — without needing an
// upstream API to talk to.
//
// It also records what it was told *around* the turns, because that is the
// half a stream cannot show: the three `llm/control` signals, and whether a
// turn rebon stopped reading actually saw its abort. Both come back in the
// text of a later turn, which is the only channel a test has that does not
// need a second protocol.

const PROVIDER_ID = "fixture-provider";

/** What the ready report tells rebon before anything routes here. */
const ADAPTER_INFO = {
  protocolVersion: 1,
  capabilities: {
    // Deliberately not the set the manifest declares: the test asserts the
    // union, which is only visible when each side contributes one.
    reasoningText: true,
    anchoredMinimal: true,
  },
  defaultModel: "fixture-pro",
  displayName: "Fixture Provider",
};

/** What happened outside the turns, reported inside a later one.
 *
 * Kept module-global, which is the honest shape for a *stateless* provider. A
 * stateful one would key everything here by `ctx.scopeId` — each client of a
 * loaded adapter opens its own scope, and that is what stops one session's
 * `endTurn` from clearing another's. The test asserts the scope arrives; what
 * a provider does with it is the provider's business.
 */
const seen = { signals: [], aborted: false, apiKeys: [], scopes: [] };

// The enum tags are snake_case and the payload structs inside them are
// camelCase: `StreamEventV1` renames its variants, `UsageV1` and
// `MessageDeltaFieldsV1` rename their own fields. Getting that wrong is a
// deserialise error at the first chunk, which is what this shape is copied
// from `deepseek_plugin_frame_shapes_deserialize` to avoid.
function textTurn(text) {
  return [
    {
      type: "message_start",
      message_id: "fixture-1",
      model: "fixture-pro",
      usage: { inputTokens: 7 },
    },
    { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
    { type: "content_block_delta", index: 0, delta: { type: "text_delta", text } },
    { type: "content_block_stop", index: 0 },
    {
      type: "message_delta",
      delta: { stopReason: "end_turn", usage: { inputTokens: 7, outputTokens: 3 } },
    },
    { type: "message_stop" },
  ];
}

async function stream(turn, ctx) {
  const request = turn?.request;
  if (!request) throw new TypeError("no request in the turn");
  if (turn.protocolVersion !== 1) {
    throw new TypeError(`unexpected protocol version ${turn.protocolVersion}`);
  }
  seen.apiKeys.push(turn.connection?.apiKey ?? null);
  if (!seen.scopes.includes(ctx.scopeId)) seen.scopes.push(ctx.scopeId);

  // A turn rebon is expected to walk away from: it runs until the host raises
  // the signal, and remembers that it did.
  if (request.model === "fixture-slow") {
    for (let i = 0; i < 400 && !ctx.signal.aborted; i += 1) {
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
    seen.aborted = ctx.signal.aborted;
    return null;
  }

  const text = request.model === "fixture-report" ? JSON.stringify(seen) : "你好";
  for (const event of textTurn(text)) await ctx.emit(event);
  return null;
}

// The conversation-level signals ride on the handler itself: registration
// closes when `activate` returns, so there is no later moment to add one.
stream.control = (signal, ctx) => {
  seen.signals.push(signal);
  if (!seen.scopes.includes(ctx.scopeId)) seen.scopes.push(ctx.scopeId);
};

export function activate(api) {
  api.llm(PROVIDER_ID, stream, ADAPTER_INFO);
}
