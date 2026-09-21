import { MAX_FRAME_BYTES, ProtocolError, validateEnvelope } from './protocol.mjs';
import { parseEnvelopeJson } from './json.mjs';

export class FramingError extends ProtocolError {}

export class NdjsonDecoder {
  #segments = []; #byteCount = 0; #failed = false; #copiedBytes = 0;
  constructor(maxFrameBytes = MAX_FRAME_BYTES) { this.maxFrameBytes = maxFrameBytes; }
  get stats() { return Object.freeze({ retained_bytes: this.#byteCount, copied_bytes: this.#copiedBytes }); }
  push(chunk) {
    if (this.#failed) throw new FramingError('poisoned', 'decoder is poisoned');
    if (!Buffer.isBuffer(chunk) && !(chunk instanceof Uint8Array)) return this.#fail('wrong_chunk', 'input chunk must be bytes');
    const input = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength);
    const frames = [];
    let start = 0;
    for (let index = 0; index < input.length; index++) {
      if (input[index] !== 0x0a) continue;
      this.#append(input.subarray(start, index));
      let bytes = this.#join();
      this.#segments = []; this.#byteCount = 0;
      if (bytes.at(-1) === 0x0d) bytes = bytes.subarray(0, -1);
      if (bytes.length > this.maxFrameBytes) return this.#fail('frame_too_large', `frame exceeds ${this.maxFrameBytes} bytes`);
      if (bytes.length === 0) return this.#fail('empty_frame', 'NDJSON frame is empty');
      if (bytes.length >= 3 && bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf) {
        return this.#fail('utf8_bom', 'UTF-8 BOM is not permitted');
      }
      try {
        const text = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes);
        frames.push(parseEnvelopeJson(text));
      } catch (error) {
        if (error instanceof ProtocolError) return this.#fail(error.code, error.message);
        return this.#fail('invalid_utf8', 'frame is not valid UTF-8');
      }
      start = index + 1;
    }
    this.#append(input.subarray(start));
    if (this.#byteCount > this.maxFrameBytes
        && !(this.#byteCount === this.maxFrameBytes + 1 && this.#lastByte() === 0x0d)) {
      return this.#fail('frame_too_large', `frame exceeds ${this.maxFrameBytes} bytes`);
    }
    return frames;
  }
  finish() {
    if (this.#failed) throw new FramingError('poisoned', 'decoder is poisoned');
    if (this.#byteCount) return this.#fail('incomplete_frame', `EOF with unterminated ${this.#byteCount}-byte frame`);
  }
  #append(bytes) {
    if (!bytes.length) return;
    const copy = Buffer.from(bytes);
    this.#segments.push(copy); this.#byteCount += copy.length; this.#copiedBytes += copy.length;
  }
  #join() {
    if (this.#segments.length === 1) return this.#segments[0];
    this.#copiedBytes += this.#byteCount;
    return Buffer.concat(this.#segments, this.#byteCount);
  }
  #lastByte() { return this.#segments.at(-1)?.at(-1); }
  #fail(code, message) { this.#failed = true; this.#segments = []; this.#byteCount = 0; throw new FramingError(code, message); }
}

export function encodeFrame(value, maxFrameBytes = MAX_FRAME_BYTES) {
  const normalized = validateEnvelope(value);
  let text;
  try { text = JSON.stringify(normalized); } catch { throw new FramingError('encode_error', 'envelope cannot be encoded'); }
  const bytes = Buffer.from(text);
  if (bytes.length > maxFrameBytes) throw new FramingError('frame_too_large', `frame exceeds ${maxFrameBytes} bytes`);
  return Buffer.concat([bytes, Buffer.from('\n')]);
}

export class SerializedWriter {
  #tail = Promise.resolve(); #error;
  constructor(stream, maxFrameBytes = MAX_FRAME_BYTES) { this.stream = stream; this.maxFrameBytes = maxFrameBytes; }
  send(value) {
    if (this.#error) return Promise.reject(this.#error);
    const bytes = encodeFrame(value, this.maxFrameBytes);
    const operation = this.#tail.then(() => {
      if (this.#error) throw this.#error;
      return this.#write(bytes);
    });
    this.#tail = operation.catch((error) => { this.#error ??= error; });
    return operation;
  }
  async flush() { await this.#tail; if (this.#error) throw this.#error; }
  #write(bytes) {
    return new Promise((resolve, reject) => {
      let callbackDone = false; let accepted; let drainSeen = false; let settled = false;
      const cleanup = () => { this.stream.removeListener('error', onError); this.stream.removeListener('drain', onDrain); };
      const fail = (cause) => {
        if (settled) return;
        settled = true; cleanup();
        const error = cause instanceof FramingError ? cause
          : new FramingError('write_error', `protocol output failed: ${cause?.code ?? 'stream error'}`);
        this.#error ??= error; reject(error);
      };
      const complete = () => {
        if (settled || accepted === undefined || !callbackDone || (!accepted && !drainSeen)) return;
        settled = true; cleanup(); resolve();
      };
      const onError = (error) => fail(error);
      const onDrain = () => { drainSeen = true; complete(); };
      this.stream.once('error', onError);
      this.stream.once('drain', onDrain);
      try {
        accepted = this.stream.write(bytes, (error) => {
          if (error) { setImmediate(() => fail(error)); return; }
          callbackDone = true; complete();
        });
        if (accepted) this.stream.removeListener('drain', onDrain);
        complete();
      } catch (error) { fail(error); }
    });
  }
}
