// Embedded-runtime shim of `@deepseek-ai/dsh-web` — the runtime symbols web
// provider plugins consume. The service half (WebRuntime, `ctx.web`) is NOT
// here: the composition host mounts its own seat (web-runtime.js); this
// module carries the shared error class so a provider's `throw new
// WebError(...)` and the runtime's own errors are one identity. Types are
// erased at transpile time.
import { HarnessError } from './dsh-llm.js';

/** dsh types.ts: typed web error with a machine-routable open-string code. */
export class WebError extends HarnessError {}
