import { ProtocolError, RawJsonNumber, validateEnvelope } from './protocol.mjs';

class RawNumber {
  constructor(raw) { this.raw = raw; Object.freeze(this); }
}

class JsonParser {
  constructor(text) { this.text = text; this.index = 0; }
  parse() {
    this.#space();
    const value = this.#value('envelope');
    this.#space();
    if (this.index !== this.text.length) this.#error('trailing_json', 'trailing data after JSON value');
    return value;
  }
  #value(role = 'value') {
    const c = this.text[this.index];
    if (c === '{') return this.#object(role);
    if (c === '[') return this.#array();
    if (c === '"') return this.#string();
    if (c === 't') return this.#literal('true', true);
    if (c === 'f') return this.#literal('false', false);
    if (c === 'n') return this.#literal('null', null);
    if (c === '-' || (c >= '0' && c <= '9')) return this.#number();
    this.#error('malformed_json', 'expected a JSON value');
  }
  #object(role) {
    this.index++;
    const object = Object.create(null);
    const keys = new Set();
    this.#space();
    if (this.text[this.index] === '}') { this.index++; return object; }
    for (;;) {
      if (this.text[this.index] !== '"') this.#error('malformed_json', 'object key must be a string');
      const key = this.#string();
      this.#space();
      if (this.text[this.index++] !== ':') this.#error('malformed_json', 'expected colon after object key');
      this.#space();
      const childRole = role === 'envelope' && key === 'message' ? 'message' : 'value';
      const value = this.#value(childRole);
      if ((role === 'envelope' || role === 'message') && keys.has(key)) {
        this.#error('duplicate_field', `duplicate field in ${role}`);
      }
      keys.add(key);
      object[key] = value; // serde_json::Value uses the last value for opaque object duplicates.
      this.#space();
      const delimiter = this.text[this.index++];
      if (delimiter === '}') return object;
      if (delimiter !== ',') this.#error('malformed_json', 'expected comma or object end');
      this.#space();
    }
  }
  #array() {
    this.index++;
    const array = [];
    this.#space();
    if (this.text[this.index] === ']') { this.index++; return array; }
    for (;;) {
      array.push(this.#value());
      this.#space();
      const delimiter = this.text[this.index++];
      if (delimiter === ']') return array;
      if (delimiter !== ',') this.#error('malformed_json', 'expected comma or array end');
      this.#space();
    }
  }
  #string() {
    this.index++;
    let result = '';
    for (;;) {
      if (this.index >= this.text.length) this.#error('malformed_json', 'unterminated JSON string');
      const c = this.text[this.index++];
      if (c === '"') return result;
      if (c === '\\') {
        if (this.index >= this.text.length) this.#error('malformed_json', 'unterminated JSON escape');
        const escape = this.text[this.index++];
        const simple = { '"': '"', '\\': '\\', '/': '/', b: '\b', f: '\f', n: '\n', r: '\r', t: '\t' }[escape];
        if (simple !== undefined) { result += simple; continue; }
        if (escape !== 'u') this.#error('malformed_json', 'invalid JSON escape');
        const first = this.#hexUnit();
        if (first >= 0xd800 && first <= 0xdbff) {
          if (this.text.slice(this.index, this.index + 2) !== '\\u') this.#error('invalid_surrogate', 'lone high surrogate in JSON string');
          this.index += 2;
          const second = this.#hexUnit();
          if (second < 0xdc00 || second > 0xdfff) this.#error('invalid_surrogate', 'invalid surrogate pair in JSON string');
          result += String.fromCodePoint(0x10000 + ((first - 0xd800) << 10) + second - 0xdc00);
        } else if (first >= 0xdc00 && first <= 0xdfff) {
          this.#error('invalid_surrogate', 'lone low surrogate in JSON string');
        } else result += String.fromCharCode(first);
        continue;
      }
      const unit = c.charCodeAt(0);
      if (unit < 0x20) this.#error('malformed_json', 'unescaped control character in JSON string');
      // The text is UTF-16, so a raw character outside the BMP (an emoji, as
      // serde_json writes it unescaped) is two units here: a high surrogate
      // followed by a low one. Only a half without its partner is invalid.
      if (unit >= 0xd800 && unit <= 0xdbff) {
        const next = this.text.charCodeAt(this.index);
        if (next >= 0xdc00 && next <= 0xdfff) {
          result += c + this.text[this.index++];
          continue;
        }
        this.#error('invalid_surrogate', 'lone surrogate in JSON string');
      }
      if (unit >= 0xdc00 && unit <= 0xdfff) this.#error('invalid_surrogate', 'lone surrogate in JSON string');
      result += c;
    }
  }
  #hexUnit() {
    const token = this.text.slice(this.index, this.index + 4);
    if (!/^[0-9a-fA-F]{4}$/.test(token)) this.#error('malformed_json', 'invalid unicode escape');
    this.index += 4;
    return Number.parseInt(token, 16);
  }
  #number() {
    const start = this.index;
    if (this.text[this.index] === '-') this.index++;
    if (this.text[this.index] === '0') this.index++;
    else {
      if (this.text[this.index] < '1' || this.text[this.index] > '9') this.#error('malformed_json', 'invalid JSON number');
      while (this.text[this.index] >= '0' && this.text[this.index] <= '9') this.index++;
    }
    if (this.text[this.index] === '.') {
      this.index++;
      const fraction = this.index;
      while (this.text[this.index] >= '0' && this.text[this.index] <= '9') this.index++;
      if (fraction === this.index) this.#error('malformed_json', 'fraction requires digits');
    }
    if (this.text[this.index] === 'e' || this.text[this.index] === 'E') {
      this.index++;
      if (this.text[this.index] === '+' || this.text[this.index] === '-') this.index++;
      const exponent = this.index;
      while (this.text[this.index] >= '0' && this.text[this.index] <= '9') this.index++;
      if (exponent === this.index) this.#error('malformed_json', 'exponent requires digits');
    }
    return new RawNumber(this.text.slice(start, this.index));
  }
  #literal(token, value) {
    if (this.text.slice(this.index, this.index + token.length) !== token) this.#error('malformed_json', 'invalid JSON literal');
    this.index += token.length;
    return value;
  }
  #space() { while (' \t\r\n'.includes(this.text[this.index] ?? '\0')) this.index++; }
  #error(code, message) { throw new ProtocolError(code, message); }
}

function exponentValue(raw) {
  const marker = Math.max(raw.indexOf('e'), raw.indexOf('E'));
  if (marker < 0) return 0;
  let value = raw.slice(marker + 1);
  let sign = 1;
  if (value[0] === '+' || value[0] === '-') { if (value[0] === '-') sign = -1; value = value.slice(1); }
  value = value.replace(/^0+/, '') || '0';
  if (value.length > 9) return sign * Infinity;
  return sign * Number(value);
}

export function parseSafeIntegerToken(raw, field) {
  const negative = raw[0] === '-';
  const unsigned = negative ? raw.slice(1) : raw;
  const marker = Math.max(unsigned.indexOf('e'), unsigned.indexOf('E'));
  const mantissa = marker < 0 ? unsigned : unsigned.slice(0, marker);
  const dot = mantissa.indexOf('.');
  const fractionLength = dot < 0 ? 0 : mantissa.length - dot - 1;
  let digits = (dot < 0 ? mantissa : mantissa.slice(0, dot) + mantissa.slice(dot + 1)).replace(/^0+/, '');
  if (!digits) return 0;
  if (negative) throw new ProtocolError('unsafe_integer', `${field} must be a safe non-negative integer`);
  const scale = exponentValue(unsigned) - fractionLength;
  if (!Number.isFinite(scale)) throw new ProtocolError('unsafe_integer', `${field} must be a safe non-negative integer`);
  if (scale < 0) {
    const remove = -scale;
    if (remove > digits.length || !digits.endsWith('0'.repeat(remove))) throw new ProtocolError('unsafe_integer', `${field} must be an exact integer`);
    digits = digits.slice(0, digits.length - remove).replace(/^0+/, '') || '0';
  } else {
    if (digits.length + scale > 16) throw new ProtocolError('unsafe_integer', `${field} must be a safe non-negative integer`);
    digits += '0'.repeat(scale);
  }
  if (digits.length > 16 || (digits.length === 16 && digits > '9007199254740991')) {
    throw new ProtocolError('unsafe_integer', `${field} must be a safe non-negative integer`);
  }
  return Number(digits);
}

function materialize(value) {
  if (value instanceof RawNumber) {
    const number = Number(value.raw);
    if (/^-?(?:0|[1-9][0-9]*)$/.test(value.raw) && Number.isSafeInteger(number)) return Object.is(number, -0) ? 0 : number;
    return new RawJsonNumber(value.raw);
  }
  if (Array.isArray(value)) return value.map(materialize);
  if (value && typeof value === 'object') for (const key of Object.keys(value)) value[key] = materialize(value[key]);
  return value;
}

export function parseEnvelopeJson(text) {
  const value = new JsonParser(text).parse();
  if (!value || value instanceof RawNumber || Array.isArray(value) || typeof value !== 'object') throw new ProtocolError('wrong_shape', 'envelope must be an object');
  if (!(value.protocol_version instanceof RawNumber) || value.protocol_version.raw !== '1') {
    throw new ProtocolError('bad_version', 'protocol_version must be integer token 1');
  }
  value.protocol_version = 1;
  for (const field of ['host_epoch', 'scope_generation']) {
    if (!(value[field] instanceof RawNumber)) throw new ProtocolError('unsafe_integer', `${field} must be a JSON number`);
    value[field] = parseSafeIntegerToken(value[field].raw, field);
  }
  value.message = materialize(value.message);
  return validateEnvelope(value);
}
