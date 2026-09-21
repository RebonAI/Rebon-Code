// Embedded-runtime shim of `@deepseek-ai/dsh-launch-environment`.
//
// The embedded composition deliberately exposes NO ambient environment to
// plugins: every credential goes through the fail-closed credentials seat,
// and connection facts come from plugin config. An always-empty snapshot
// makes each consumer take its documented fallback (dsh-llm-deepseek's
// baseURL falls to the public API; its ambient-key branch never wins because
// the credentials seat is mounted).

const EMPTY_SNAPSHOT = Object.freeze({
  get: (_name) => undefined,
  getFrom: (_name, _sources) => undefined,
});

/** The launcher snapshot for this run: embedded hosts provide none. */
export function launchEnvironmentOf(_ctx) {
  return EMPTY_SNAPSHOT;
}

/** Context slot name the real launcher fills; exported for API parity. */
export const DSH_LAUNCH_ENVIRONMENT_KEY = 'launchEnvironment';
