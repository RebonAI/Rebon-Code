// Mini-streams-protocol build of `eventsource-parser/stream`.
//
// The published package's EventSourceParserStream extends the WHATWG
// TransformStream, which bare deno_core does not have. Kernel JS hosts use
// the Mini streams protocol instead (see js/web.js): a transform is any
// object with `transformIterable(source) -> asyncIterable`. Parsing itself
// is the vendored upstream `createParser` (js/vendor/eventsource-parser/
// index.js), so framing semantics — chunk reassembly, CRLF/BOM, comment
// skipping, multi-`data:` joining — are byte-identical to the real package.
// Error semantics mirror upstream: a parse error reaches `onError` when one
// is supplied, and only `onError: 'terminate'` or a max-buffer-size
// overflow aborts the stream.
import { createParser } from './index.js';

export { ParseError } from './index.js';

export class EventSourceParserStream {
  constructor({ onError, onRetry, onComment, maxBufferSize } = {}) {
    this._options = { onError, onRetry, onComment, maxBufferSize };
  }

  transformIterable(source) {
    const { onError, onRetry, onComment, maxBufferSize } = this._options;
    return (async function* () {
      const queue = [];
      let terminal;
      const parser = createParser({
        onEvent: (event) => {
          queue.push(event);
        },
        onError(error) {
          if (typeof onError === 'function') onError(error);
          if (onError === 'terminate' || error.type === 'max-buffer-size-exceeded') {
            terminal = error;
          }
        },
        onRetry,
        onComment,
        maxBufferSize,
      });
      for await (const chunk of source) {
        parser.feed(chunk);
        while (queue.length > 0) yield queue.shift();
        if (terminal !== undefined) throw terminal;
      }
    })();
  }
}
