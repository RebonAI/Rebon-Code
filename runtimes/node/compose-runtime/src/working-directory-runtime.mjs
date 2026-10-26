// The composition's `ctx.workingDirectory` seat: a session's directory is the
// workspace rebon opened it in.
//
// DeepSeek Harness keeps a session's working directory itself
// (`@deepseek-ai/dsh-working-directory`): a projection over the session log,
// changed by a `cd` tool, announced to the model as runtime context. In
// rebon the session belongs to rebon, and so does its directory — it is the
// workspace root the scope was opened with, the same root rebon's own tools
// resolve against. So this seat answers the dsh contract every tool package
// injects (`tool-fs`, `tool-bash`, `tool-skill` reach it through
// `ctx.workingDirectory.ensure(exec.agent, signal)`) from that one fact,
// rather than mounting a second owner for it.
//
// What it does not do: change the directory. `set` refuses by name, because
// a directory this side kept would be a second answer to "where is this
// session" that rebon's own tools would not share.
import path from 'node:path';
import { Service } from 'cordis';
import { currentCall, workspaceRoot } from './bridge.mjs';
import { sessionOf } from './registry.mjs';

class WorkingDirectoryError extends Error {
  constructor(code, message) {
    super(`${code} ${message}`);
    this.code = code;
    this.name = 'WorkingDirectoryError';
  }
}

export default class RebonWorkingDirectory extends Service {
  constructor(ctx) {
    super(ctx, 'workingDirectory');
  }

  /// The fallback for code with no session: the host's own directory, which
  /// is the workspace a shared host was started in.
  get defaultDirectory() {
    return process.cwd();
  }

  /// The directory of the session this code runs for, without I/O.
  ///
  /// The call in flight says which session that is; outside one, the session
  /// the calling plugin is attached to does. `session` is accepted for the
  /// dsh signature and not consulted: rebon's session object is the scope,
  /// and the scope is what the call already names.
  get(_session) {
    const root = workspaceRoot() ?? sessionOf(this.ctx)?.workspaceRoot;
    return typeof root === 'string' && path.isAbsolute(root) ? root : this.defaultDirectory;
  }

  /// dsh's `ensure`: the session's existing directory. Rebon opened the scope
  /// on it, so there is nothing to recover.
  async ensure(agent, signal) {
    signal?.throwIfAborted?.();
    return this.get(agent?.session);
  }

  /// dsh's `set`, refused: see the module comment.
  async set(_agent, target) {
    throw new WorkingDirectoryError(
      '[UNSUPPORTED]',
      `the working directory is the workspace rebon opened this session in; it cannot be changed to ${JSON.stringify(String(target))} from a plugin`,
    );
  }

  /// Whether a call is in flight, for diagnostics.
  inCall() {
    return currentCall() !== undefined;
  }
}
