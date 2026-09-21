function num(value) {
  // `Number(null)` is 0, so an absent counter would otherwise render as a real
  // reading — a session the app has not scanned yet would claim a 0% cache rate
  // instead of admitting it does not know.
  if (value === null || value === undefined || value === '') return null;
  const n = Number(value);
  return Number.isFinite(n) ? n : null;
}

function firstNum(...values) {
  for (const value of values) {
    const n = num(value);
    if (n !== null) return n;
  }
  return null;
}

function clamp(value, min, max) {
  return Math.max(min, Math.min(max, value));
}

function trim(value) {
  return value.toFixed(value < 10 ? 1 : 0).replace(/\.0$/, '');
}

function fmt(value) {
  value = num(value);
  if (value === null) return '?';
  const sign = value < 0 ? '-' : '';
  const n = Math.abs(value);
  if (n < 1000) return sign + Math.round(n).toString();
  if (n < 1_000_000) return sign + trim(n / 1000) + 'k';
  return sign + trim(n / 1_000_000) + 'M';
}

function pct(value) {
  value = num(value);
  return value === null ? '?' : trim(value) + '%';
}

function bar(value, width) {
  value = num(value);
  if (value === null) return '▱'.repeat(width);
  const filled = Math.round(clamp(value / 100, 0, 1) * width);
  return '▰'.repeat(filled) + '▱'.repeat(width - filled);
}

function clip(text, max) {
  text = String(text || '');
  return text.length > max ? text.slice(0, Math.max(1, max - 1)) + '…' : text;
}

function terminalColumns(payload) {
  const columns = num(payload?.terminal?.columns);
  return columns !== null && columns > 0 ? columns : 120;
}

const c = {
  reset: '\x1b[0m',
  dim: '\x1b[2m',
  bold: '\x1b[1m',
  green: '\x1b[92m',
  yellow: '\x1b[93m',
  red: '\x1b[91m',
  cyan: '\x1b[96m',
  blue: '\x1b[94m',
  magenta: '\x1b[95m',
  gray: '\x1b[90m',
};

function color(text, code) {
  return code + text + c.reset;
}

function usageColor(used) {
  used = num(used);
  if (used === null) return c.gray;
  if (used >= 95) return c.red;
  if (used >= 85) return c.yellow;
  return c.green;
}

function statusMark(used, isApp, glyph) {
  used = num(used);
  // In the app the model always wears the same icon and the colour alone
  // carries the pressure; a terminal has no icon, so the glyph has to.
  if (isApp) return color(glyph('', 'bot'), usageColor(used) + c.bold);
  if (used === null) return color('◇', c.gray);
  if (used >= 95) return color('!', c.red + c.bold);
  if (used >= 85) return color('▲', c.yellow + c.bold);
  return color('●', c.green);
}

export default function render(payload) {
payload = payload || {};
// The desktop app renders `{icon:name}` from its own icon set (see
// `docs/configuration.md`); a terminal would print that marker verbatim, so the
// unicode glyph stays the default. `surface` is the app's own payload field.
const isApp = payload.surface === 'app';
// The app's header slot is a ~440px strip beside the session title, so it gets
// the short line: whatever does not fit is clipped off the end, and the
// secondary dim details are the first thing worth giving up.
const isHeader = payload.placement === 'header_right';

function glyph(unicode, name) {
  return isApp ? `{icon:${name}}` : unicode;
}
const model = (payload.model?.display_name || payload.model?.id || 'model').split("-").map(i=>i.toUpperCase()).join(" ");
const cw = payload.context_window || {};
const currentUsage = cw.current_usage && typeof cw.current_usage === 'object' ? cw.current_usage : {};
const totalUsage = payload.total_usage || cw.total_usage || {};
const lastUsage = payload.last_turn_usage || cw.last_turn_usage || currentUsage;

const windowSize = num(cw.context_window_size);
const inputTokens = firstNum(currentUsage.input_tokens, cw.total_input_tokens);
const lastOutputTokens = firstNum(lastUsage.output_tokens, currentUsage.output_tokens, cw.total_output_tokens, 0);
const totalOutputTokens = firstNum(totalUsage.output_tokens, totalUsage.total_output_tokens, lastOutputTokens);

// The desktop app reports the turn's cache split under `last_turn_usage` and
// keeps no lifetime split at all, so each lookup falls through to the turn
// before giving up. The TUI still matches on the first key and reads exactly as
// it did before.
const cacheReadHit = firstNum(totalUsage.cache_read_hit_input_tokens, totalUsage.cache_read_input_tokens, totalUsage.prompt_cache_hit_tokens, lastUsage.cache_read_hit_input_tokens, lastUsage.cache_read_input_tokens, lastUsage.prompt_cache_hit_tokens, 0);
const cacheWriteMiss = firstNum(totalUsage.cache_write_miss_input_tokens, totalUsage.cache_creation_input_tokens, totalUsage.prompt_cache_miss_tokens, lastUsage.cache_write_miss_input_tokens, lastUsage.cache_creation_input_tokens, lastUsage.prompt_cache_miss_tokens, 0);
const cacheTotal = firstNum(totalUsage.cache_total_input_tokens, lastUsage.cache_total_input_tokens, cacheReadHit + cacheWriteMiss);
const cacheRate = cacheTotal > 0 ? cacheReadHit / cacheTotal * 100 : null;

// Lifetime billing: the app hands over one combined figure, the TUI two halves.
const lifetimeIn = num(totalUsage.total_input_tokens);
const lifetimeOut = num(totalUsage.total_output_tokens);
const lifetimeTokens = firstNum(
  totalUsage.total_tokens,
  lifetimeIn !== null || lifetimeOut !== null ? (lifetimeIn || 0) + (lifetimeOut || 0) : null,
);

const used = num(cw.used_percentage) ?? (inputTokens !== null && windowSize ? inputTokens / windowSize * 100 : null);
const remaining = num(cw.remaining_percentage) ?? (used !== null ? Math.max(100 - used, 0) : null);
const remainingTokens = inputTokens !== null && windowSize !== null ? Math.max(windowSize - inputTokens, 0) : null;

const columns = terminalColumns(payload);
const compact = isHeader || columns < 100;
const modelText = compact ? clip(model, 14) : clip(model, 22);
const usedColor = usageColor(used);
const meter = color(bar(used, compact ? 8 : 12), usedColor);

// Each metric owns a hue. That hue is what separates one section from the
// next — no glyph divider needed. Bright icon + solid value = the anchor,
// dim same-hue = the secondary detail.
const cacheText = cacheTotal > 0
  ? `${color(fmt(cacheTotal), c.cyan + c.bold)} ${color('↺ ' + fmt(cacheReadHit) + ' ⊘ ' + fmt(cacheWriteMiss), c.cyan + c.dim)}`
  : color('—', c.dim);

// A section that has no number behind it is dropped, not printed with `?`: the
// desktop app's payload carries no context window size, and an empty meter next
// to `?%` reads as a broken status line rather than an unmeasured one.
// The header sits right next to the app's own model chip, so it opens on the
// numbers instead of repeating the model or a pressure mark the app cannot
// compute.
const sections = isHeader ? [] : [`${statusMark(used, isApp, glyph)} ${color(modelText, c.bold)}`];

if (used !== null) {
  sections.push(compact
    ? `${color(glyph('◷', 'clock'), c.blue + c.bold)} ${meter} ${color(pct(used), usedColor + c.bold)}`
    : `${color(glyph('◷', 'clock'), c.blue + c.bold)} ${meter} ${color(pct(used), usedColor + c.bold)} ${color(fmt(inputTokens) + '/' + fmt(windowSize), c.blue + c.dim)}`);
} else if (inputTokens) {
  // No window size to divide by — the absolute context carried is still worth
  // showing, just without a meter that would have to guess at a denominator.
  sections.push(`${color(glyph('◷', 'clock'), c.blue + c.bold)} ${color(fmt(inputTokens), c.blue + c.bold)}`);
}

if (remainingTokens !== null) {
  sections.push(compact
    ? `${color(glyph('⇣', 'chev-d'), c.green + c.bold)} ${color(fmt(remainingTokens), c.green + c.bold)}`
    : `${color(glyph('⇣', 'chev-d'), c.green + c.bold)} ${color(fmt(remainingTokens), c.green + c.bold)} ${color(pct(remaining), c.green + c.dim)}`);
}

if (totalOutputTokens) {
  sections.push(compact
    ? `${color(glyph('↗', 'send'), c.magenta + c.bold)} ${color(fmt(totalOutputTokens), c.magenta + c.bold)}`
    : `${color(glyph('↗', 'send'), c.magenta + c.bold)} ${color(fmt(totalOutputTokens), c.magenta + c.bold)} ${color('(' + fmt(lastOutputTokens) + ')', c.magenta + c.dim)}`);
}

sections.push(
  isHeader
    // Just the volume the cache saw; the hit/miss split below is the detail the
    // header has no room for and the message footnote already carries.
    ? `${color(glyph('◆', 'bolt'), c.cyan + c.bold)} ${color(cacheTotal > 0 ? fmt(cacheTotal) : '—', c.cyan + c.bold)}`
    : `${color(glyph('◆', 'bolt'), c.cyan + c.bold)} ${cacheText}`,
);

// Cache hit-rate is just one more colored block (green good / yellow low),
// flowing inline. No right-alignment, so it never depends on guessing the
// exact terminal width and never gets pushed off-screen or wrapped.
const rateHue = cacheRate === null ? c.gray : cacheRate >= 50 ? c.green : c.yellow;
sections.push(`${color(glyph('◎', 'target'), rateHue + c.bold)} ${color(pct(cacheRate), rateHue + c.bold)}`);

// Lifetime billing closes the line: it is the one figure that only grows, so it
// belongs after the per-turn blocks rather than competing with them.
if (lifetimeTokens) {
  sections.push(`${color(glyph('Σ', 'book'), c.gray + c.bold)} ${color(fmt(lifetimeTokens), c.gray)}`);
}

// Spacing (not a divider) carries the eye between the colored blocks.
const gap = compact ? '  ' : '   ';
const line = sections.join(gap);

return line;
}
