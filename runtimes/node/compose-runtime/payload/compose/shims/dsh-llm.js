// Embedded-runtime shim of `@deepseek-ai/dsh-llm` — exactly the runtime
// symbols dsh provider plugins consume, with semantics transcribed from the
// dsh sources (packages/llm/llm/src). The service half of the real package
// (LlmRuntime) is NOT here: the composition host mounts its own `ctx.llm`
// (llm-runtime.js), and this module re-exports the shared LlmAdapter base
// class (llm-adapter.js) so both import paths hand plugins the same one.
import z from 'schemastery';
import { MAX_TIMER_DELAY_MS } from '@deepseek-ai/dsh-timeout';

export { LlmAdapter } from './llm-adapter.js';

// ---- vendored helper core (loop round) ----
// The real dsh-llm helper surface the agent loop consumes — BlockAssembler,
// message factories, call-config marking, error-chain rendering — bundled
// verbatim from the dsh sources (vendor/dsh/llm-core.js) and re-exported
// here so every `@deepseek-ai/dsh-llm` import keeps resolving to this one
// module (single class/WeakSet identity for instanceof and mark checks).
export {
  BlockAssembler,
  createMessage,
  freezeMessage,
  createUserMessage,
  createAssistantMessage,
  createToolResultMessage,
  deepFreeze,
  markAgentLoopRequest,
  isAgentLoopRequest,
  callConfigEquals,
  errorChain,
} from '../../vendor/dsh/llm-core.js';

// ---- error taxonomy (dsh error.ts) ----

/** Base class for all harness errors: stable machine `code` + cause chain. */
export class HarnessError extends Error {
  constructor(message, code, options) {
    super(message, options);
    this.code = code;
    this.name = new.target.name;
  }
}

export const CONTEXT_WINDOW_EXCEEDED_CODE = 'CONTEXT_WINDOW_EXCEEDED';
export const QUOTA_EXCEEDED_CODE = 'QUOTA';
export const EMPTY_RESPONSE_CODE = 'EMPTY_RESPONSE';
export const INVALID_CREDENTIAL_CODE = 'INVALID_CREDENTIAL';

/**
 * Exhaustiveness helper (dsh never.ts, transcribed verbatim): a value that
 * escaped its closed union throws with diagnostics at runtime. Consumed by
 * the vendored dsh-tools schema layer.
 */
export function assertNever(value, context) {
  const rendered = JSON.stringify(value) ?? String(value);
  throw new Error(`unreachable variant${context ? ` in ${context}` : ''}: ${rendered}`);
}

const STRUCTURED_CONTEXT_OVERFLOW = new RegExp(
  String.raw`(?:^|[^a-z0-9])context[\s_-](?:length|window)[\s_-]`
    + String.raw`(?:exceed(?:ed|s)?|overflow(?:ed)?|limit[\s_-]exceeded)(?:$|[^a-z0-9])`,
  'i',
);

const TOO_LARGE_FOR_CONTEXT = new RegExp(
  String.raw`\b(?:request|prompt|input|messages?)\s+(?:is\s+|are\s+)?`
    + String.raw`too\s+(?:large|long)\s+for\s+(?:(?:this|the)\s+)?`
    + String.raw`(?:model(?:'s)?\s+)?context(?:\s+window)?\b`,
  'i',
);

const EXCEEDS_MODEL_CONTEXT = new RegExp(
  String.raw`\b(?:input|prompt|request|messages?)\b.{0,40}`
    + String.raw`\b(?:exceed(?:s|ed)?|overflows?|is\s+larger\s+than)\b.{0,40}`
    + String.raw`\b(?:the\s+)?(?:model(?:'s)?\s+)?context(?:\s+(?:length|window))?\b`,
  'i',
);

/** Recognize context-overflow wording (code/type/message joined into one string). */
export function isContextWindowExceededError(detail) {
  return STRUCTURED_CONTEXT_OVERFLOW.test(detail)
    || /\b(?:maximum|max)(?:\s+(?:allowed|supported))?\s+context\s+(?:length|window)\b/i.test(detail)
    || TOO_LARGE_FOR_CONTEXT.test(detail)
    || /\b(?:input|prompt|request)\s+(?:is\s+)?too\s+(?:long|large)\s+for\s+(?:this|the)\s+model\b/i.test(detail)
    || EXCEEDS_MODEL_CONTEXT.test(detail);
}

/** Recognize terminal quota/balance wording, as opposed to a transient rate limit. */
export function isQuotaExceededError(detail) {
  return /\binsufficient[\s_-]+(?:quota|balance|credits?)\b/i.test(detail)
    || /\b(?:quota|usage[\s_-]+limit)[\s_-]+(?:exceeded|exhausted|reached)\b/i.test(detail)
    || /\bexceed(?:ed|s)?[\s_-]+(?:(?:your|the)[\s_-]+)?(?:current[\s_-]+)?quota\b/i.test(detail)
    || /\b(?:balance|credits?)[\s_-]+(?:exhausted|depleted)\b/i.test(detail)
    || /\bout[\s_-]+of[\s_-]+(?:credits?|budget)\b/i.test(detail);
}

/**
 * Typed error for LLM failures: validated provider facts beside the live
 * Error (dsh index.ts, constructor contract preserved — bad arguments throw).
 */
export class LlmError extends HarnessError {
  constructor(message, code, options) {
    if (typeof message !== 'string' || message.length === 0) {
      throw new Error('LlmError message must be a non-empty string');
    }
    if (typeof code !== 'string' || code.length === 0) {
      throw new Error('LlmError code must be a non-empty string');
    }
    if (options?.status !== undefined
      && (!Number.isInteger(options.status) || options.status < 100 || options.status > 599)) {
      throw new Error('LlmError status must be an integer from 100 through 599');
    }
    if (options?.providerRetryAfterMs !== undefined
      && (!Number.isFinite(options.providerRetryAfterMs) || options.providerRetryAfterMs <= 0)) {
      throw new Error('LlmError providerRetryAfterMs must be a positive finite number');
    }
    if (options?.requestId !== undefined
      && (typeof options.requestId !== 'string' || options.requestId.length === 0)) {
      throw new Error('LlmError requestId must be a non-empty string');
    }
    super(message, code, options);
    this.name = 'LlmError';
    this.failure = Object.freeze({
      message,
      code,
      ...(options?.status === undefined ? {} : { status: options.status }),
      ...(options?.providerRetryAfterMs === undefined
        ? {}
        : { providerRetryAfterMs: options.providerRetryAfterMs }),
      ...(options?.requestId === undefined ? {} : { requestId: options.requestId }),
    });
  }
}

// ---- api keys (dsh api-key.ts + assertUsableApiKey) ----

const LEGAL_API_KEY = /^[\x21-\x7E]+$/;

/** Judge one supplied API key, trimming surrounding whitespace first. */
export function normalizeApiKey(raw) {
  const value = raw.trim();
  if (value.length === 0) return { ok: false, reason: 'empty' };
  if (!LEGAL_API_KEY.test(value)) return { ok: false, reason: 'illegalCharacters' };
  return { ok: true, value };
}

/** Accept one supplied credential or refuse it as unusable (never echoes the value). */
export function assertUsableApiKey(raw, pkg, ref) {
  const checked = normalizeApiKey(raw);
  if (checked.ok) return checked.value;
  throw new LlmError(
    checked.reason === 'empty'
      ? `${pkg}: the API key resolved from ${ref} is blank; set ${ref} to the raw key`
        + ' (the web Models page writes it) or export it in the launching environment'
      : `${pkg}: the API key resolved from ${ref} contains characters no HTTP header can carry;`
        + ` set ${ref} to the raw key alone (the web Models page writes it)`,
    INVALID_CREDENTIAL_CODE,
  );
}

// ---- attribution (dsh attribution.ts; embedded identity, fixed version) ----

export const APP_IDENTITY = {
  product: 'deepseek-harness',
  version: '0.0.0-rebon-embedded',
  url: 'https://github.com/deepseek-ai/deepseek-harness',
};

export function userAgent(identity = APP_IDENTITY) {
  return `${identity.product}/${identity.version} (+${identity.url})`;
}

export function attributionHeaders(identity = APP_IDENTITY) {
  return { 'user-agent': userAgent(identity) };
}

// ---- branded ids (dsh brand.ts: brand = identity at runtime) ----

export function MessageId(id) {
  return id;
}

export function CallId(id) {
  return id;
}

export function ProviderRequestId(id) {
  return id;
}

export function ReasoningEffortId(id) {
  return id;
}

// ---- content helpers (dsh content.ts) ----

/** True when typed model content contains an image block, walking tool results. */
export function contentHasImage(content) {
  return content.some((block) => block.type === 'image'
    || (block.type === 'tool-result' && contentHasImage(block.content)));
}

// ---- retry policy (dsh retry-policy.ts, transcribed) ----

const DEFAULT_MAX_RETRIES = 2;
const DEFAULT_INITIAL_DELAY_MS = 500;
const DEFAULT_MAX_DELAY_MS = 10_000;
const DEFAULT_JITTER_RATIO = 0.1;
const DEFAULT_RETRYABLE_CODES = Object.freeze([
  EMPTY_RESPONSE_CODE,
  'RATE_LIMIT',
  'SERVER',
  'TIMEOUT',
  'TRANSPORT',
]);

const backoffSchema = z.object({
  initialDelayMs: z.number().max(MAX_TIMER_DELAY_MS).default(DEFAULT_INITIAL_DELAY_MS),
  maxDelayMs: z.number().max(MAX_TIMER_DELAY_MS).default(DEFAULT_MAX_DELAY_MS),
  jitterRatio: z.number().min(0).max(1).default(DEFAULT_JITTER_RATIO),
});

const normalPolicySchema = z.object({
  mode: z.const('normal').required(),
  maxRetries: z.number().step(1).min(0).max(Number.MAX_SAFE_INTEGER).default(DEFAULT_MAX_RETRIES),
  retryableCodes: z.array(z.string()).default([...DEFAULT_RETRYABLE_CODES]),
  backoff: backoffSchema,
});

const alwaysPolicySchema = z.object({
  mode: z.const('always').required(),
  backoff: backoffSchema,
});

/** Cordis schema embedded by each concrete provider configuration. */
export const RetryPolicySchema = z.union([normalPolicySchema, alwaysPolicySchema]);

const NORMAL_POLICY_KEYS = new Set(['mode', 'maxRetries', 'retryableCodes', 'backoff']);
const ALWAYS_POLICY_KEYS = new Set(['mode', 'backoff']);
const BACKOFF_KEYS = new Set(['initialDelayMs', 'maxDelayMs', 'jitterRatio']);

function validateKeys(value, allowed, path) {
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) throw new Error(`${path}: unknown key "${key}"`);
  }
}

function resolveBackoff(config, path) {
  if (config !== undefined) validateKeys(config, BACKOFF_KEYS, path);
  const initialDelayMs = config?.initialDelayMs ?? DEFAULT_INITIAL_DELAY_MS;
  const maxDelayMs = config?.maxDelayMs ?? DEFAULT_MAX_DELAY_MS;
  const jitterRatio = config?.jitterRatio ?? DEFAULT_JITTER_RATIO;

  if (!Number.isFinite(initialDelayMs) || initialDelayMs <= 0 || initialDelayMs > MAX_TIMER_DELAY_MS) {
    throw new Error(`${path}.initialDelayMs must be a positive finite number no greater than ${MAX_TIMER_DELAY_MS}`);
  }
  if (!Number.isFinite(maxDelayMs) || maxDelayMs <= 0 || maxDelayMs > MAX_TIMER_DELAY_MS) {
    throw new Error(`${path}.maxDelayMs must be a positive finite number no greater than ${MAX_TIMER_DELAY_MS}`);
  }
  if (initialDelayMs > maxDelayMs) {
    throw new Error(`${path}.initialDelayMs must be less than or equal to maxDelayMs`);
  }
  if (!Number.isFinite(jitterRatio) || jitterRatio < 0 || jitterRatio > 1) {
    throw new Error(`${path}.jitterRatio must be between 0 and 1`);
  }

  return Object.freeze({ initialDelayMs, maxDelayMs, jitterRatio });
}

/** Validate, default, and detach one provider-owned retry policy. */
export function resolveRetryPolicy(config, path) {
  if (config === undefined) {
    return Object.freeze({
      mode: 'normal',
      maxRetries: DEFAULT_MAX_RETRIES,
      retryableCodes: DEFAULT_RETRYABLE_CODES,
      ...resolveBackoff(undefined, `${path}.backoff`),
    });
  }

  switch (config.mode) {
    case 'normal': {
      validateKeys(config, NORMAL_POLICY_KEYS, path);
      const maxRetries = config.maxRetries ?? DEFAULT_MAX_RETRIES;
      const retryableCodes = config.retryableCodes ?? [...DEFAULT_RETRYABLE_CODES];
      if (!Number.isSafeInteger(maxRetries) || maxRetries < 0) {
        throw new Error(`${path}.maxRetries must be a non-negative safe integer`);
      }
      if (retryableCodes.length === 0) {
        throw new Error(`${path}.retryableCodes must not be empty`);
      }
      if (retryableCodes.some((code) => typeof code !== 'string' || code.length === 0)) {
        throw new Error(`${path}.retryableCodes must contain only non-empty strings`);
      }
      if (new Set(retryableCodes).size !== retryableCodes.length) {
        throw new Error(`${path}.retryableCodes must not contain duplicates`);
      }
      return Object.freeze({
        mode: 'normal',
        maxRetries,
        retryableCodes: Object.freeze([...retryableCodes]),
        ...resolveBackoff(config.backoff, `${path}.backoff`),
      });
    }
    case 'always':
      validateKeys(config, ALWAYS_POLICY_KEYS, path);
      return Object.freeze({
        mode: 'always',
        ...resolveBackoff(config.backoff, `${path}.backoff`),
      });
    default:
      throw new Error(`${path}.mode must be "normal" or "always"`);
  }
}
