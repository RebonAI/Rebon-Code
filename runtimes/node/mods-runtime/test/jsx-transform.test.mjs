import test from 'node:test';
import assert from 'node:assert/strict';
import { transformJsx } from '../src/jsx-transform.mjs';

const roundTrip = (source) => transformJsx(source);

test('an element with props and children becomes an h() call', () => {
  const out = roundTrip('const a = <Box flexDirection="column" gap={1}><Text bold>Hi {name}</Text></Box>;');
  assert.equal(out, 'const a = h(Box, { "flexDirection": "column", "gap": (1) }, h(Text, { "bold": true }, "Hi ", name));');
});

test('a fragment, entities and a self-closing element', () => {
  const out = roundTrip('return (<>\n  <Text>one</Text>\n  <Text>two &amp; three</Text>\n  <Br />\n</>);');
  assert.equal(out, 'return (h(Fragment, null, h(Text, null, "one"), h(Text, null, "two & three"), h(Br, null)));');
});

test('a spread and a lowercase tag', () => {
  assert.equal(roundTrip('<Box {...style} key="k"/>'), 'h(Box, { ...(style), "key": "k" })');
  assert.equal(roundTrip('<div class="x"/>'), 'h("div", { "class": "x" })');
  assert.equal(roundTrip('<B.C d="1"/>'), 'h(B.C, { "d": "1" })');
});

test('nested braces, a map callback and a comment child', () => {
  const out = roundTrip('<List>{/* nothing */}{items.map((x) => <Row key={x.id}>{x.name}</Row>)}</List>');
  assert.equal(out, 'h(List, null, items.map((x) => h(Row, { "key": (x.id) }, x.name)))');
});

test('comparisons, regex literals and TSX generics are left alone', () => {
  const source = 'const g = <T,>(x: T) => x; const c = a < b && i<n; const r = /<x>/.test(s); const k = y > <A/> ? 1 : 2;';
  const out = roundTrip(source);
  assert.ok(out.startsWith('const g = <T,>(x: T) => x; const c = a < b && i<n; const r = /<x>/.test(s);'));
  assert.ok(out.endsWith('const k = y > h(A, null) ? 1 : 2;'));
  assert.equal(roundTrip('const f = <T extends object>(x: T) => x;'), 'const f = <T extends object>(x: T) => x;');
});

test('a template literal keeps its text and transforms its expressions', () => {
  const out = roundTrip('const t = `a ${<Text>{1}</Text>} b ${"<not jsx>"}`;');
  assert.equal(out, 'const t = `a ${h(Text, null, 1)} b ${"<not jsx>"}`;');
});

test('strings and comments holding angle brackets are copied through', () => {
  const source = '// <Box/> in a comment\nconst s = "<Box/>"; /* <A> */ const t = \'<B>\';';
  assert.equal(roundTrip(source), source);
});

test('JSX text is trimmed line by line', () => {
  const out = roundTrip('<Text>\n  first line\n  second   line\n</Text>');
  assert.equal(out, 'h(Text, null, "first line second   line")');
});

test('a malformed element is left in the source for the parser to name', () => {
  const source = 'const a = <Box><Text>x</Box>;';
  assert.equal(roundTrip(source), source);
});

test('an element value for a prop and an async hook body', () => {
  const out = roundTrip('on("ui.render", async ($, e, next) => { const { Box } = $.ui.resolve(e); return <Box icon=<I/>>{await next(e)}</Box>; })');
  assert.equal(out, 'on("ui.render", async ($, e, next) => { const { Box } = $.ui.resolve(e); return h(Box, { "icon": h(I, null) }, await next(e)); })');
});
