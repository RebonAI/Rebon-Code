// Two scripted "tabs" against `rebon serve`, to watch the mux.
//
// `rebon serve` speaks ACP over a WebSocket at /ws. Node has had a global
// `WebSocket` since 22, so this needs no dependency and no hand-rolled
// framing — an earlier attempt to write the frames by hand reached the
// upgrade and got no further, which is eighty lines of RFC 6455 this file
// does not contain.
//
// What it checks is mux semantics, not rendering:
//
//   * two connections are two clients to the socket layer but one lease on
//     the session, which is what `rebon serve: page connected` counts;
//   * what one tab does reaches the other;
//   * closing the last one releases what the mux held.
//
// Usage:
//   node scripts/probe-serve-tabs.mjs --port 7391 --token <token>
//   node scripts/probe-serve-tabs.mjs --port 7391 --token <token> --seconds 20

const args = new Map();
for (let i = 2; i < process.argv.length; i += 2) {
  args.set(process.argv[i].replace(/^--/, ''), process.argv[i + 1]);
}
const PORT = Number(args.get('port') ?? 7391);
const TOKEN = args.get('token') ?? '';
const SECONDS = Number(args.get('seconds') ?? 15);
const URL = `ws://127.0.0.1:${PORT}/ws?token=${encodeURIComponent(TOKEN)}`;

/** One tab. Records everything it is sent, so the two can be compared. */
class Tab {
  constructor(label) {
    this.label = label;
    this.seen = [];
    this.nextId = 1;
    this.ws = new WebSocket(URL);
    this.ready = new Promise((resolve, reject) => {
      this.ws.addEventListener('open', () => resolve(this));
      this.ws.addEventListener('error', (e) => reject(new Error(`${label}: ${e.message ?? e}`)));
    });
    this.ws.addEventListener('message', (event) => {
      const text = typeof event.data === 'string' ? event.data : String(event.data);
      for (const line of text.split('\n')) {
        if (!line.trim()) continue;
        try {
          const msg = JSON.parse(line);
          this.seen.push(msg);
          const what = msg.method ?? `response#${msg.id}`;
          console.log(`  ${this.label} <- ${what}  ${line.slice(0, 160)}`);
        } catch {
          console.log(`  ${this.label} <- (not JSON) ${line.slice(0, 120)}`);
        }
      }
    });
    this.ws.addEventListener('close', (e) => {
      console.log(`  ${this.label} closed (code ${e.code})`);
    });
  }

  call(method, params) {
    const msg = { jsonrpc: '2.0', id: this.nextId++, method };
    if (params !== undefined) msg.params = params;
    this.ws.send(JSON.stringify(msg));
    console.log(`  ${this.label} -> ${method}`);
    return msg.id;
  }

  close() {
    try { this.ws.close(); } catch { /* already gone */ }
  }
}

const wait = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  console.log(`=== connecting two tabs to ${URL.replace(TOKEN, '<token>')} ===`);
  const a = await new Tab('A').ready;
  const b = await new Tab('B').ready;
  console.log('both tabs are open');

  console.log('=== each initializes ===');
  a.call('initialize', { protocolVersion: 1, clientCapabilities: {} });
  b.call('initialize', { protocolVersion: 1, clientCapabilities: {} });
  await wait(2000);

  console.log('=== A opens a session; B loads the same one ===');
  a.call('session/new', { cwd: args.get('cwd') ?? process.cwd(), mcpServers: [] });
  await wait(4000);
  const created = a.seen.find((m) => m.result && m.result.sessionId);
  const sessionId = created?.result?.sessionId;
  console.log('session id:', sessionId ?? '(none — session/new did not answer with one)');
  if (sessionId) {
    b.call('session/load', { sessionId, cwd: args.get('cwd') ?? process.cwd() });
    await wait(3000);

    // One tab changes something; the other should be told, because the mux
    // broadcasts to every connection rather than to the one that asked.
    console.log('=== A sets a config option; watching whether B hears it ===');
    a.call('session/set_config_option', { sessionId, configId: 'permission-mode', value: 'plan' });
    await wait(3000);
  }

  console.log(`=== watching for ${SECONDS}s ===`);
  await wait(SECONDS * 1000);

  console.log('=== closing A only ===');
  a.close();
  await wait(2000);
  console.log('=== closing B ===');
  b.close();
  await wait(1000);

  console.log(`\nA saw ${a.seen.length} message(s), B saw ${b.seen.length}`);
  const methods = (t) => t.seen.map((m) => m.method ?? `response#${m.id}`);
  console.log('A:', JSON.stringify(methods(a)));
  console.log('B:', JSON.stringify(methods(b)));
}

main().then(() => process.exit(0), (err) => {
  console.error('failed:', err.message);
  process.exit(1);
});
