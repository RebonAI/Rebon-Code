// `eventsource-parser/stream` for real Node — the published package's shape.
//
// The vendored build under `js/vendor/eventsource-parser/stream.js` is not the
// published module: it was rewritten so that `EventSourceParserStream` is a
// "Mini streams" transform (an object with `transformIterable`), because bare
// deno_core has no WHATWG `TransformStream` for the real one to extend. The
// dsh DeepSeek adapter consumes it as a platform stream —
// `response.body.pipeThrough(new TextDecoderStream()).pipeThrough(new
// EventSourceParserStream())` — which on real Node throws
// `ERR_INVALID_ARG_TYPE: transform.readable must be an instance of
// ReadableStream` against the rewritten class.
//
// So on Node the upstream shape comes back. Parsing itself is still the
// vendored upstream `createParser`, so framing semantics — chunk reassembly,
// CRLF/BOM, comment skipping, multi-`data:` joining — stay byte-identical to
// the published package on both runtimes; only the plumbing differs.
import { createParser } from 'eventsource-parser';

export { ParseError } from 'eventsource-parser';

export class EventSourceParserStream extends TransformStream {
  constructor({ onError, onRetry, onComment, maxBufferSize } = {}) {
    let parser;
    super({
      start(controller) {
        parser = createParser({
          onEvent: (event) => controller.enqueue(event),
          onError(error) {
            // Upstream's rule, kept: a callback observes, the string
            // `'terminate'` aborts, and a buffer overflow aborts regardless of
            // what the consumer asked for — there is no way to keep parsing
            // past it.
            if (typeof onError === 'function') onError(error);
            if (onError === 'terminate' || error.type === 'max-buffer-size-exceeded') {
              controller.error(error);
            }
          },
          onRetry,
          onComment,
          maxBufferSize,
        });
      },
      transform(chunk) {
        parser.feed(chunk);
      },
    });
  }
}
