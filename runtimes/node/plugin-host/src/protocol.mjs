export const PROTOCOL_VERSION = 1;
export const MAX_FRAME_BYTES = 8 * 1024 * 1024;
export const MAX_SAFE_WIRE_INTEGER = Number.MAX_SAFE_INTEGER;
export const PLATFORM_PLUGIN_ID = '$rebon/platform';
export const PLATFORM_CONTROL_SCOPE_ID = '$rebon/control';
export const PLATFORM_CONTROL_GENERATION = 0;
export const CALL_CANCEL_METHOD = 'call/cancel';

export class ProtocolError extends Error {
  constructor(code, message) { super(message); this.name = 'ProtocolError'; this.code = code; }
}

export class RawJsonNumber {
  constructor(raw) { this.raw = raw; Object.freeze(this); }
  valueOf() { return Number(this.raw); }
  toJSON() { throw new ProtocolError('raw_number', 'raw JSON number cannot be emitted without an explicit schema conversion'); }
}

const own = (value, key) => Object.prototype.hasOwnProperty.call(value, key);
export const plain = (value) => value !== null && typeof value === 'object' && !Array.isArray(value)
  && (Object.getPrototypeOf(value) === Object.prototype || Object.getPrototypeOf(value) === null);

function exact(value, keys, where) {
  if (!plain(value)) throw new ProtocolError('wrong_shape', `${where} must be a plain object`);
  const actual = Object.keys(value).sort();
  const expected = [...keys].sort();
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
    throw new ProtocolError('unknown_field', `${where} must have exactly: ${expected.join(', ')}`);
  }
  for (const key of expected) if (!own(value, key)) throw new ProtocolError('wrong_shape', `${where}.${key} is required`);
}

function wireInteger(value, field) {
  if (!Number.isSafeInteger(value) || value < 0) throw new ProtocolError('unsafe_integer', `${field} must be a safe non-negative integer`);
  return Object.is(value, -0) ? 0 : value;
}
function string(value, field) {
  if (typeof value !== 'string') throw new ProtocolError('wrong_shape', `${field} must be a string`);
  return value;
}
function cloneJson(value, seen = new Set()) {
  if (value instanceof RawJsonNumber) return value;
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value;
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) throw new ProtocolError('invalid_payload', 'payload numbers must be finite JSON numbers');
    return Object.is(value, -0) ? 0 : value;
  }
  if (typeof value !== 'object') throw new ProtocolError('invalid_payload', 'payload must be JSON-compatible');
  if (seen.has(value)) throw new ProtocolError('invalid_payload', 'payload must not be cyclic');
  seen.add(value);
  let copy;
  if (Array.isArray(value)) copy = value.map((item) => cloneJson(item, seen));
  else {
    if (!plain(value)) throw new ProtocolError('invalid_payload', 'payload objects must have a plain prototype');
    copy = Object.create(null);
    for (const key of Object.keys(value)) copy[key] = cloneJson(value[key], seen);
  }
  seen.delete(value);
  return Object.freeze(copy);
}

/// Every name a message object may carry, whatever its variant.
const MESSAGE_FIELDS = ['type', 'method', 'status', 'payload'];

/// Checks a message object in the order the other language does.
///
/// The order is the contract, not an implementation detail. A name nobody knows
/// is refused before the variant is even decided; then the fields every variant
/// needs; only then a name that is real but belongs to a different variant.
/// Collapsing these into one set comparison would report "you left out payload"
/// as "unknown field", which sends a sender to the wrong place — and would say
/// something different from the Rust side for the same bytes.
function messageShape(message, fields) {
  if (!plain(message)) throw new ProtocolError('wrong_shape', 'message must be a plain object');
  for (const key of Object.keys(message)) {
    if (!MESSAGE_FIELDS.includes(key)) throw new ProtocolError('unknown_field', `message has unknown field ${JSON.stringify(key)}`);
  }
  for (const key of ['type', 'payload']) {
    if (!own(message, key)) throw new ProtocolError('wrong_shape', `message.${key} is required`);
  }
  for (const key of Object.keys(message)) {
    if (!fields.includes(key)) throw new ProtocolError('unknown_field', `message must have exactly: ${[...fields].sort().join(', ')}`);
  }
  for (const key of fields) {
    if (!own(message, key)) throw new ProtocolError('wrong_shape', `message.${key} is required`);
  }
}

export function validateEnvelope(input) {
  exact(input, ['protocol_version','host_epoch','plugin_id','scope_id','scope_generation','call_id','message'], 'envelope');
  if (input.protocol_version !== PROTOCOL_VERSION) throw new ProtocolError('bad_version', `protocol_version must be ${PROTOCOL_VERSION}`);
  const type = plain(input.message) ? input.message.type : undefined;
  // A chunk carries no method: it belongs to the call whose identity it rides,
  // and that call's method already said what it means.
  const fields = type === 'terminal' ? ['type','status','payload']
    : type === 'chunk' ? ['type','payload']
    : ['type','method','payload'];
  if (!['request','terminal','notification','chunk'].includes(type)) throw new ProtocolError('wrong_shape', 'message.type is invalid');
  messageShape(input.message, fields);
  const message = type === 'terminal'
    ? Object.freeze({ type, status: terminalStatus(input.message.status), payload: cloneJson(input.message.payload) })
    : type === 'chunk'
      ? Object.freeze({ type, payload: cloneJson(input.message.payload) })
      : Object.freeze({ type, method: string(input.message.method, 'message.method'), payload: cloneJson(input.message.payload) });
  return Object.freeze({
    protocol_version: PROTOCOL_VERSION,
    host_epoch: wireInteger(input.host_epoch, 'host_epoch'),
    plugin_id: string(input.plugin_id, 'plugin_id'),
    scope_id: string(input.scope_id, 'scope_id'),
    scope_generation: wireInteger(input.scope_generation, 'scope_generation'),
    call_id: string(input.call_id, 'call_id'),
    message,
  });
}
function terminalStatus(status) {
  if (!['success','error','cancelled'].includes(status)) throw new ProtocolError('bad_status', 'terminal status is invalid');
  return status;
}
export function identityOf(envelope) {
  return Object.freeze({ host_epoch: envelope.host_epoch, plugin_id: envelope.plugin_id, scope_id: envelope.scope_id,
    scope_generation: envelope.scope_generation, call_id: envelope.call_id });
}
export function envelope(identity, message) { return validateEnvelope({ protocol_version: PROTOCOL_VERSION, ...identity, message }); }
export function terminal(identity, status, payload) { return envelope(identity, { type: 'terminal', status, payload }); }
export function request(identity, method, payload) { return envelope(identity, { type: 'request', method, payload }); }
export function notification(identity, method, payload) { return envelope(identity, { type: 'notification', method, payload }); }
export function chunk(identity, payload) { return envelope(identity, { type: 'chunk', payload }); }
export function cancel(target) { return notification(target, CALL_CANCEL_METHOD, null); }
export function isPlatformControl(value) { return value.plugin_id === PLATFORM_PLUGIN_ID && value.scope_id === PLATFORM_CONTROL_SCOPE_ID && value.scope_generation === 0; }
