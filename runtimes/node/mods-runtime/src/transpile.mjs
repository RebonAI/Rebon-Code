// Loading a mod's files: the module hooks that make `.tsx` and the bare
// `claude-code` specifier resolve.
//
// A hooks module "is an ES module whatever its suffix", may be TypeScript,
// may hold JSX, and imports `claude-code` for its state helpers (at run time
// the type imports are empty, the value imports are not). None of that is
// something Node does by itself, so this installs synchronous module hooks
// (`module.registerHooks`) that answer for files under a registered mod root
// and hand everything else on untouched: a Cordis entry or the host's own
// files never pass through here.
//
// Order of operations on a `.tsx`: JSX to `h()` calls first
// (`jsx-transform.mjs`), then Node's own `stripTypeScriptTypes` in
// `transform` mode — the JSX is what the type stripper refuses, so it has to
// be gone before the stripper sees the file.
import { readFileSync, statSync } from 'node:fs';
import { registerHooks, stripTypeScriptTypes } from 'node:module';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { transformJsx } from './jsx-transform.mjs';

/// The one bare specifier a mod imports: the Claude Code runtime module.
export const CLAUDE_CODE_SPECIFIER = 'claude-code';

const SHIM_URL = new URL('./claude-code.mjs', import.meta.url).href;

/// The suffixes a mod's files may carry. A file named otherwise is not a
/// mod file, and an import of one is refused rather than guessed at.
export const MOD_EXTENSIONS = Object.freeze(['.ts', '.tsx', '.jsx', '.js', '.mjs', '.cjs', '.mts', '.cts']);

/// The suffixes tried, in order, for a relative import written without one
/// (`./card`), as TypeScript and Claude Code resolve it.
const IMPLIED_EXTENSIONS = Object.freeze(['.ts', '.tsx', '.mts', '.js', '.jsx', '.mjs']);

/// For a `.js`-family import with no file behind it, the TypeScript source
/// it was written against: `./util.js` naming `./util.ts`.
const SOURCE_FOR = Object.freeze({ '.js': ['.ts', '.tsx'], '.jsx': ['.tsx'], '.mjs': ['.mts'], '.cjs': ['.cts'] });

function isFile(filename) {
  try {
    return statSync(filename).isFile();
  } catch {
    return false;
  }
}

/// The mod file a relative import names: the file itself when it is one,
/// else the TypeScript resolution of an import written without a suffix
/// (`./card` → `./card.ts`, `./dir` → `./dir/index.ts`) or with the
/// emitted one (`./util.js` → `./util.ts`). `null` when none exists.
export function resolveModFile(filename) {
  const extension = path.extname(filename);
  if (MOD_EXTENSIONS.includes(extension) && isFile(filename)) return filename;
  for (const source of SOURCE_FOR[extension] ?? []) {
    const candidate = filename.slice(0, -extension.length) + source;
    if (isFile(candidate)) return candidate;
  }
  for (const implied of IMPLIED_EXTENSIONS) {
    if (isFile(filename + implied)) return filename + implied;
  }
  for (const implied of IMPLIED_EXTENSIONS) {
    const index = path.join(filename, `index${implied}`);
    if (isFile(index)) return index;
  }
  return null;
}

const roots = new Set();
let installed = false;

/// Says that files under `root` are a mod's, so the hooks answer for them.
export function registerModRoot(root) {
  const url = pathToFileURL(path.resolve(root)).href;
  roots.add(url.endsWith('/') ? url : `${url}/`);
  install();
}

export function unregisterModRoot(root) {
  const url = pathToFileURL(path.resolve(root)).href;
  roots.delete(url.endsWith('/') ? url : `${url}/`);
}

function underModRoot(url) {
  for (const root of roots) if (url.startsWith(root)) return true;
  return false;
}

/// Whether `source` holds TypeScript syntax the stripper has to remove.
function needsTypeStrip(extension) {
  return extension === '.ts' || extension === '.tsx' || extension === '.mts' || extension === '.cts';
}

/// Turns one mod file into the JavaScript module Node evaluates.
export function transpileModSource(source, filename) {
  const extension = path.extname(filename);
  let code = source;
  if (extension === '.tsx' || extension === '.jsx') code = transformJsx(code);
  if (needsTypeStrip(extension)) {
    code = stripTypeScriptTypes(code, { mode: 'transform', sourceUrl: pathToFileURL(filename).href });
  }
  return code;
}

function install() {
  if (installed) return;
  installed = true;
  // `stripTypeScriptTypes` announces itself as experimental once per process
  // on stderr, where the host expects one JSON diagnostic per line. The
  // warning is known and the API is pinned by this package's tests; keep it
  // off the host's diagnostics stream.
  const emitWarning = process.emitWarning;
  process.emitWarning = function filtered(warning, ...rest) {
    if (String(warning?.message ?? warning).includes('stripTypeScriptTypes')) return undefined;
    return emitWarning.call(process, warning, ...rest);
  };
  registerHooks({
    resolve(specifier, context, nextResolve) {
      const parent = context.parentURL ?? '';
      if (specifier === CLAUDE_CODE_SPECIFIER && underModRoot(parent)) {
        return { url: SHIM_URL, format: 'module', shortCircuit: true };
      }
      if (underModRoot(parent) && (specifier.startsWith('./') || specifier.startsWith('../'))) {
        const url = new URL(specifier, parent).href;
        if (!underModRoot(url)) {
          throw new Error(`[PATH_ESCAPES] ${specifier} resolves outside the mod's folder`);
        }
        const resolved = resolveModFile(fileURLToPath(url));
        if (resolved === null) {
          const extension = path.extname(fileURLToPath(url));
          if (MOD_EXTENSIONS.includes(extension)) return nextResolve(specifier, context); // not there: Node says so
          throw new Error(`[NOT_A_MOD_FILE] ${specifier} is not named .ts, .tsx, .jsx, .js, .mjs, .cjs, .mts or .cts, and no such file with one exists`);
        }
        const target = pathToFileURL(resolved).href;
        if (!underModRoot(target)) {
          throw new Error(`[PATH_ESCAPES] ${specifier} resolves outside the mod's folder`);
        }
        return { url: target, format: 'module', shortCircuit: true };
      }
      return nextResolve(specifier, context);
    },
    load(url, context, nextLoad) {
      if (!underModRoot(url)) return nextLoad(url, context);
      const filename = fileURLToPath(url);
      const extension = path.extname(filename);
      if (!MOD_EXTENSIONS.includes(extension)) return nextLoad(url, context);
      // Read directly rather than through `nextLoad`: the default loader
      // decides a `.ts` or `.cjs` file's format from its name, and the rule
      // here is that a mod's file is an ES module whatever it is named.
      const source = readFileSync(filename, 'utf8');
      return { format: 'module', source: transpileModSource(source, filename), shortCircuit: true };
    },
  });
}
