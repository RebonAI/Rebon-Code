// JSX to `h(...)` calls, as a source-to-source transform.
//
// A Claude Code mod draws with JSX that compiles against the global `h`
// (`jsxFactory: "h"`, `jsxFragmentFactory: "Fragment"`). Node evaluates
// TypeScript by stripping its types, and strips nothing from a `.tsx` file:
// the JSX has to go first. This is that pass, and nothing more — type
// annotations survive it untouched for `stripTypeScriptTypes` to take out
// afterwards, which is why the two run in that order.
//
// It is a scanner, not a parser. It walks the source as JavaScript tokens,
// enough to know where a string, a template, a comment or a regular
// expression literal is, and treats `<` as the start of an element only where
// an expression may start — after `(`, `=`, `return`, `?`, `:` and their
// kind — and an identifier or `>` follows. Everything else is copied through.
// TSX's own rule for a generic arrow function (`<T,>(x: T) => x`) is honoured
// by bailing out of an element whose tag is followed by `,` or `extends`.
//
// What a bail-out costs is nothing: the `<` is copied as an operator and the
// rest of the source goes on, which is what the TypeScript parser would have
// done with it too. A JSX element that is malformed (an unclosed tag, a
// mismatched close) is left in the source as written, and the module then
// fails to load with Node's own syntax error naming the line.

const IDENT_START = /[A-Za-z_$]/;
const IDENT_PART = /[A-Za-z0-9_$]/;
const TAG_PART = /[A-Za-z0-9_$.:-]/;

/// Tokens after which the next `<` or `/` begins an expression rather than
/// continuing one.
const EXPRESSION_START_WORDS = new Set([
  'return', 'yield', 'await', 'typeof', 'void', 'delete', 'in', 'of', 'case',
  'else', 'do', 'throw', 'new', 'instanceof', 'export', 'default',
]);
const CONTINUATION_PUNCT = new Set([')', ']', '}']);

const ENTITIES = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: ' ' };

/// Replaces every JSX element in `source` with `h(...)` calls.
export function transformJsx(source, { factory = 'h', fragment = 'Fragment' } = {}) {
  return transformRange(source, 0, source.length, { factory, fragment });
}

function isExpressionStart(last) {
  if (last === '') return true;
  if (last.kind === 'word') return EXPRESSION_START_WORDS.has(last.text);
  if (last.kind === 'punct') return !CONTINUATION_PUNCT.has(last.text);
  // A string, number, template or element: an expression just ended.
  return false;
}

function transformRange(src, start, end, options) {
  let out = '';
  let i = start;
  let last = '';
  while (i < end) {
    const ch = src[i];
    if (ch === ' ' || ch === '\t' || ch === '\n' || ch === '\r') { out += ch; i += 1; continue; }
    if (ch === '/' && src[i + 1] === '/') {
      const stop = src.indexOf('\n', i);
      const until = stop === -1 || stop > end ? end : stop;
      out += src.slice(i, until); i = until; continue;
    }
    if (ch === '/' && src[i + 1] === '*') {
      const stop = src.indexOf('*/', i + 2);
      const until = stop === -1 || stop + 2 > end ? end : stop + 2;
      out += src.slice(i, until); i = until; continue;
    }
    if (ch === '"' || ch === "'") {
      const until = skipString(src, i, end);
      out += src.slice(i, until); i = until; last = { kind: 'value' }; continue;
    }
    if (ch === '`') {
      const { code, end: until } = copyTemplate(src, i, end, options);
      out += code; i = until; last = { kind: 'value' }; continue;
    }
    if (ch === '/' && isExpressionStart(last)) {
      const until = skipRegex(src, i, end);
      out += src.slice(i, until); i = until; last = { kind: 'value' }; continue;
    }
    if (ch === '<' && isExpressionStart(last) && (IDENT_START.test(src[i + 1] ?? '') || src[i + 1] === '>')) {
      const element = parseElement(src, i, end, options);
      if (element !== null) {
        out += element.code; i = element.end; last = { kind: 'value' }; continue;
      }
    }
    if (IDENT_START.test(ch)) {
      let j = i + 1;
      while (j < end && IDENT_PART.test(src[j])) j += 1;
      const word = src.slice(i, j);
      out += word; i = j; last = { kind: 'word', text: word }; continue;
    }
    if (/[0-9]/.test(ch)) {
      let j = i + 1;
      while (j < end && /[0-9A-Za-z_.]/.test(src[j])) j += 1;
      out += src.slice(i, j); i = j; last = { kind: 'value' }; continue;
    }
    out += ch; i += 1; last = { kind: 'punct', text: ch };
  }
  return out;
}

function skipString(src, i, end) {
  const quote = src[i];
  let j = i + 1;
  while (j < end) {
    if (src[j] === '\\') { j += 2; continue; }
    if (src[j] === quote) return j + 1;
    if (src[j] === '\n') return j;
    j += 1;
  }
  return end;
}

/// A regular expression literal, with its character classes and flags.
function skipRegex(src, i, end) {
  let j = i + 1;
  let inClass = false;
  while (j < end) {
    const ch = src[j];
    if (ch === '\\') { j += 2; continue; }
    if (ch === '\n') return j;
    if (inClass) { if (ch === ']') inClass = false; j += 1; continue; }
    if (ch === '[') { inClass = true; j += 1; continue; }
    if (ch === '/') { j += 1; while (j < end && IDENT_PART.test(src[j])) j += 1; return j; }
    j += 1;
  }
  return end;
}

/// Copies a template literal, transforming the expressions inside `${}`.
function copyTemplate(src, i, end, options) {
  let code = '`';
  let j = i + 1;
  while (j < end) {
    const ch = src[j];
    if (ch === '\\') { code += src.slice(j, j + 2); j += 2; continue; }
    if (ch === '`') return { code: code + '`', end: j + 1 };
    if (ch === '$' && src[j + 1] === '{') {
      const close = findMatching(src, j + 1, end);
      code += '${' + transformRange(src, j + 2, close, options) + '}';
      j = close + 1;
      continue;
    }
    code += ch; j += 1;
  }
  return { code, end };
}

/// The index of the `}` matching the `{` at `open`, or `end` when none.
function findMatching(src, open, end) {
  let depth = 0;
  let j = open;
  let last = '';
  while (j < end) {
    const ch = src[j];
    if (ch === '/' && src[j + 1] === '/') { const stop = src.indexOf('\n', j); j = stop === -1 ? end : stop; continue; }
    if (ch === '/' && src[j + 1] === '*') { const stop = src.indexOf('*/', j + 2); j = stop === -1 ? end : stop + 2; continue; }
    if (ch === '"' || ch === "'") { j = skipString(src, j, end); last = { kind: 'value' }; continue; }
    if (ch === '`') { j = skipTemplate(src, j, end); last = { kind: 'value' }; continue; }
    if (ch === '/' && isExpressionStart(last)) { j = skipRegex(src, j, end); last = { kind: 'value' }; continue; }
    if (ch === '<' && isExpressionStart(last) && (IDENT_START.test(src[j + 1] ?? '') || src[j + 1] === '>')) {
      const element = parseElement(src, j, end, { factory: 'h', fragment: 'Fragment' });
      if (element !== null) { j = element.end; last = { kind: 'value' }; continue; }
    }
    if (ch === '{') depth += 1;
    if (ch === '}') { depth -= 1; if (depth === 0) return j; }
    if (IDENT_START.test(ch)) {
      let k = j + 1;
      while (k < end && IDENT_PART.test(src[k])) k += 1;
      last = { kind: 'word', text: src.slice(j, k) }; j = k; continue;
    }
    if (/[0-9]/.test(ch)) { last = { kind: 'value' }; j += 1; continue; }
    if (!/\s/.test(ch)) last = { kind: 'punct', text: ch };
    j += 1;
  }
  return end;
}

function skipTemplate(src, i, end) {
  let j = i + 1;
  while (j < end) {
    const ch = src[j];
    if (ch === '\\') { j += 2; continue; }
    if (ch === '`') return j + 1;
    if (ch === '$' && src[j + 1] === '{') { j = findMatching(src, j + 1, end) + 1; continue; }
    j += 1;
  }
  return end;
}

function skipSpace(src, j, end) {
  while (j < end && /\s/.test(src[j])) j += 1;
  return j;
}

function decodeEntities(text) {
  return text.replace(/&(#x[0-9a-fA-F]+|#[0-9]+|[a-zA-Z]+);/g, (whole, body) => {
    if (body[0] === '#') {
      const code = body[1] === 'x' || body[1] === 'X' ? parseInt(body.slice(2), 16) : parseInt(body.slice(1), 10);
      return Number.isFinite(code) ? String.fromCodePoint(code) : whole;
    }
    return Object.prototype.hasOwnProperty.call(ENTITIES, body) ? ENTITIES[body] : whole;
  });
}

/// JSX text, with its lines trimmed the way the JSX grammar trims them.
function jsxText(raw) {
  const lines = raw.split('\n');
  const kept = [];
  lines.forEach((line, index) => {
    let text = line.replace(/\r$/, '');
    if (index > 0) text = text.replace(/^[ \t]+/, '');
    if (index < lines.length - 1) text = text.replace(/[ \t]+$/, '');
    if (text.length > 0) kept.push(text);
  });
  return decodeEntities(kept.join(' '));
}

/// One element starting at the `<` at `i`, or `null` where that `<` is not
/// an element after all.
function parseElement(src, i, end, options) {
  let j = i + 1;
  let tag = '';
  if (src[j] === '>') {
    j += 1;
  } else {
    while (j < end && TAG_PART.test(src[j])) j += 1;
    tag = src.slice(i + 1, j);
    if (tag === '' || tag.endsWith('.') || tag.endsWith('-')) return null;
    const after = skipSpace(src, j, end);
    if (src[after] === ',' || src.startsWith('extends', after)) return null;
  }
  const props = [];
  let selfClosing = false;
  if (tag !== '') {
    for (;;) {
      j = skipSpace(src, j, end);
      if (j >= end) return null;
      if (src[j] === '/' && src[j + 1] === '>') { selfClosing = true; j += 2; break; }
      if (src[j] === '>') { j += 1; break; }
      if (src[j] === '{') {
        const close = findMatching(src, j, end);
        if (close >= end) return null;
        const inner = src.slice(j + 1, close).trim();
        if (!inner.startsWith('...')) return null;
        props.push({ spread: transformRange(src, j + 1, close, options).trim().slice(3) });
        j = close + 1;
        continue;
      }
      if (!IDENT_START.test(src[j])) return null;
      let k = j + 1;
      while (k < end && TAG_PART.test(src[k])) k += 1;
      const name = src.slice(j, k);
      j = skipSpace(src, k, end);
      if (src[j] !== '=') { props.push({ name, value: 'true' }); continue; }
      j = skipSpace(src, j + 1, end);
      if (src[j] === '"' || src[j] === "'") {
        const close = src.indexOf(src[j], j + 1);
        if (close === -1 || close > end) return null;
        props.push({ name, value: JSON.stringify(decodeEntities(src.slice(j + 1, close))) });
        j = close + 1;
        continue;
      }
      if (src[j] === '{') {
        const close = findMatching(src, j, end);
        if (close >= end) return null;
        props.push({ name, value: '(' + transformRange(src, j + 1, close, options).trim() + ')' });
        j = close + 1;
        continue;
      }
      if (src[j] === '<') {
        const nested = parseElement(src, j, end, options);
        if (nested === null) return null;
        props.push({ name, value: nested.code });
        j = nested.end;
        continue;
      }
      return null;
    }
  }
  const children = [];
  if (!selfClosing) {
    for (;;) {
      if (j >= end) return null;
      if (src[j] === '<' && src[j + 1] === '/') {
        const close = src.indexOf('>', j + 2);
        if (close === -1 || close > end) return null;
        const closing = src.slice(j + 2, close).trim();
        if (closing !== tag) return null;
        j = close + 1;
        break;
      }
      if (src[j] === '{') {
        const close = findMatching(src, j, end);
        if (close >= end) return null;
        const inner = src.slice(j + 1, close).trim();
        if (inner !== '' && !(inner.startsWith('/*') && inner.endsWith('*/'))) {
          children.push(transformRange(src, j + 1, close, options).trim());
        }
        j = close + 1;
        continue;
      }
      if (src[j] === '<') {
        const nested = parseElement(src, j, end, options);
        if (nested === null) return null;
        children.push(nested.code);
        j = nested.end;
        continue;
      }
      let k = j;
      while (k < end && src[k] !== '<' && src[k] !== '{') k += 1;
      const text = jsxText(src.slice(j, k));
      if (text.length > 0) children.push(JSON.stringify(text));
      j = k;
    }
  }
  const type = tag === '' ? options.fragment : /^[a-z]/.test(tag) && !tag.includes('.') ? JSON.stringify(tag) : tag;
  let propsCode = 'null';
  if (props.length > 0) {
    propsCode = '{ ' + props.map((prop) => (prop.spread !== undefined ? `...(${prop.spread})` : `${JSON.stringify(prop.name)}: ${prop.value}`)).join(', ') + ' }';
  }
  const parts = [type, propsCode, ...children];
  return { code: `${options.factory}(${parts.join(', ')})`, end: j };
}
