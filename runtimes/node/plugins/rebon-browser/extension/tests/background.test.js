const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const shared = require("../shared.js");
const source = fs.readFileSync(path.resolve(__dirname, "..", "background.js"), "utf8");

function event() {
  const listeners = [];
  return {
    addListener(listener) {
      listeners.push(listener);
    },
    dispatch(...args) {
      for (const listener of listeners) listener(...args);
    },
  };
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, reject, resolve };
}

function createHarness(options = {}) {
  const sockets = [];
  const debuggerDetach = event();
  const attachGate = options.attachGate;
  const detachGate = options.detachGate;
  const calls = { attach: 0, detach: 0, messages: [] };

  class MockWebSocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;

    constructor(url, protocol) {
      this.url = url;
      this.protocol = protocol;
      this.readyState = MockWebSocket.CONNECTING;
      this.listeners = new Map();
      this.sent = [];
      sockets.push(this);
    }

    addEventListener(name, listener) {
      const listeners = this.listeners.get(name) || [];
      listeners.push(listener);
      this.listeners.set(name, listeners);
    }

    emit(name, payload = {}) {
      for (const listener of this.listeners.get(name) || []) listener(payload);
    }

    send(message) {
      this.sent.push(message);
    }

    close() {
      this.readyState = MockWebSocket.CLOSED;
      this.emit("close");
    }
  }

  const events = {
    alarm: event(),
    debuggerDetach,
    installed: event(),
    message: event(),
    startup: event(),
    tabActivated: event(),
    tabGroupRemoved: event(),
    tabRemoved: event(),
    tabUpdated: event(),
  };

  const storageGet = options.storageGet || (async () => ({}));
  const chrome = {
    alarms: {
      create() {},
      onAlarm: events.alarm,
    },
    debugger: {
      async attach() {
        calls.attach += 1;
        if (options.attachError) throw new Error(options.attachError);
        if (attachGate) await attachGate.promise;
      },
      async detach({ tabId }) {
        calls.detach += 1;
        if (detachGate && tabId === options.detachGateTab) await detachGate.promise;
        debuggerDetach.dispatch({ tabId }, "canceled_by_user");
      },
      async sendCommand() {
        return {};
      },
      onDetach: debuggerDetach,
    },
    permissions: {
      async contains() {
        return true;
      },
    },
    runtime: {
      getManifest() {
        return { version: "0.1.0" };
      },
      async getPlatformInfo() {
        return { os: "win" };
      },
      onInstalled: events.installed,
      onMessage: events.message,
      onStartup: events.startup,
    },
    scripting: {
      async executeScript() {},
    },
    storage: {
      local: {
        get: storageGet,
        async remove() {},
        async set() {},
      },
    },
    tabGroups: {
      async update() {},
      onRemoved: events.tabGroupRemoved,
    },
    tabs: {
      async get(tabId) {
        return { id: tabId, groupId: 1, status: "complete", url: "https://example.com" };
      },
      async query() {
        return options.queryTabs || [];
      },
      async sendMessage(tabId, message, options) {
        calls.messages.push({ tabId, message, options });
        return {};
      },
      onActivated: events.tabActivated,
      onRemoved: events.tabRemoved,
      onUpdated: events.tabUpdated,
    },
    webNavigation: {
      async getAllFrames() {
        return [{ frameId: 0 }];
      },
    },
    windows: {
      async update() {},
    },
  };

  const hooks = {};
  const context = vm.createContext({
    __REBON_BROWSER_TEST__: hooks,
    chrome,
    clearInterval() {},
    clearTimeout() {},
    console,
    globalThis: null,
    importScripts() {},
    RebonBrowserShared: shared,
    setInterval() {
      return 1;
    },
    setTimeout() {
      return 1;
    },
    WebSocket: MockWebSocket,
  });
  context.globalThis = context;
  vm.runInContext(source, context, { filename: "background.js" });
  return { calls, events, hooks, MockWebSocket, sockets };
}

async function flushMicrotasks() {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

test("transport connection setup is single-flight", async () => {
  const tokenGate = deferred();
  const harness = createHarness({
    storageGet(key) {
      if (key === "rebonBrowserControlState") return Promise.resolve({});
      return tokenGate.promise;
    },
  });
  await flushMicrotasks();

  const first = harness.hooks.connectTransport();
  const second = harness.hooks.connectTransport();
  assert.equal(harness.sockets.length, 0);
  tokenGate.resolve({});
  await Promise.all([first, second]);
  assert.equal(harness.sockets.length, 1);
});

test("a stale socket close cannot clear a newer connection", async () => {
  const harness = createHarness();
  await harness.hooks.connectTransport();
  const first = harness.sockets[0];
  first.readyState = harness.MockWebSocket.CLOSED;

  await harness.hooks.connectTransport();
  const second = harness.sockets[1];
  assert.equal(harness.hooks.getSocket(), second);
  first.emit("close");
  assert.equal(harness.hooks.getSocket(), second);
});

test("debugger attach is single-flight and expected detach does not stop control", async () => {
  const attachGate = deferred();
  const harness = createHarness({ attachGate });
  await harness.hooks.connectTransport();
  harness.hooks.state.controlledTabs.add(7);
  harness.hooks.state.controlling = true;

  const first = harness.hooks.attachTab(7);
  const second = harness.hooks.attachTab(7);
  assert.equal(harness.calls.attach, 1);
  assert.equal(harness.hooks.getAttachPromiseCount(), 1);
  attachGate.resolve();
  await Promise.all([first, second]);

  await harness.hooks.detachTab(7);
  assert.equal(harness.calls.detach, 1);
  assert.equal(harness.hooks.state.controlling, true);
  assert.equal(harness.hooks.expectedDetachTabs.has(7), false);
});

test("authenticated transport loss detaches tabs and removes overlays", async () => {
  const harness = createHarness();
  await harness.hooks.connectTransport();
  const socket = harness.sockets[0];
  socket.readyState = harness.MockWebSocket.OPEN;
  socket.emit("open");
  socket.emit("message", {
    data: JSON.stringify({ type: "hello_ok", authenticated: true, pairing_required: false }),
  });
  await flushMicrotasks();

  harness.hooks.state.controlledTabs.add(11);
  harness.hooks.state.attachedTabs.add(11);
  harness.hooks.state.controlling = true;
  socket.readyState = harness.MockWebSocket.CLOSED;
  socket.emit("close");
  await new Promise(resolve => setImmediate(resolve));
  await new Promise(resolve => setImmediate(resolve));

  assert.equal(harness.hooks.state.controlling, false);
  assert.equal(harness.calls.detach, 1);
  assert.ok(harness.calls.messages.some(call => call.message.type === "stop_control"));
});

test("releasing a tab waits for attach ownership and cleans up only that tab", async () => {
  const attachGate = deferred();
  const harness = createHarness({ attachGate });
  await harness.hooks.connectTransport();
  harness.hooks.state.controlledTabs.add(13);
  harness.hooks.state.controlling = true;

  const attaching = harness.hooks.attachTab(13);
  const releasing = harness.hooks.releaseTab(13, "Tab left the Rebon group.");
  attachGate.resolve();
  await Promise.all([attaching, releasing]);

  assert.equal(harness.hooks.state.controlledTabs.has(13), false);
  assert.equal(harness.hooks.state.attachedTabs.has(13), false);
  assert.equal(harness.calls.attach, 1);
  assert.equal(harness.calls.detach, 1);
  assert.ok(harness.calls.messages.some(call => call.message.type === "stop_control"));
});

test("moving the active tab out of the Rebon group selects a remaining controlled tab", async () => {
  const harness = createHarness();
  await harness.hooks.connectTransport();
  harness.hooks.state.groupId = 1;
  harness.hooks.state.controlledTabs.add(10);
  harness.hooks.state.controlledTabs.add(11);
  harness.hooks.state.attachedTabs.add(10);
  harness.hooks.state.activeTabId = 10;
  harness.hooks.state.controlling = true;

  harness.events.tabUpdated.dispatch(10, {}, { id: 10, groupId: 2 });
  await new Promise(resolve => setImmediate(resolve));
  await new Promise(resolve => setImmediate(resolve));

  assert.deepEqual([...harness.hooks.state.controlledTabs], [11]);
  assert.equal(harness.hooks.state.activeTabId, 11);
  assert.equal(harness.hooks.state.groupId, 1);
  assert.equal(harness.hooks.state.controlling, true);
  assert.equal(harness.calls.detach, 1);
  assert.ok(harness.calls.messages.some(call => call.message.type === "stop_control"));
});

test("refresh never restores a tab released while cleanup is pending", async () => {
  const detachGate = deferred();
  const harness = createHarness({
    detachGate,
    detachGateTab: 10,
    queryTabs: [{ id: 11, groupId: 1 }],
  });
  await harness.hooks.connectTransport();
  harness.hooks.state.groupId = 1;
  harness.hooks.state.controlledTabs.add(10);
  harness.hooks.state.controlledTabs.add(11);
  harness.hooks.state.attachedTabs.add(10);
  harness.hooks.state.activeTabId = 10;
  harness.hooks.state.controlling = true;

  const refreshing = harness.hooks.refreshControlledTabs();
  await flushMicrotasks();
  harness.events.tabUpdated.dispatch(11, {}, { id: 11, groupId: 2 });
  await new Promise(resolve => setImmediate(resolve));
  await new Promise(resolve => setImmediate(resolve));

  assert.deepEqual([...harness.hooks.state.controlledTabs], []);
  assert.equal(harness.hooks.state.activeTabId, null);
  assert.equal(harness.hooks.state.groupId, null);
  assert.equal(harness.hooks.state.controlling, false);

  detachGate.resolve();
  await refreshing;

  assert.deepEqual([...harness.hooks.state.controlledTabs], []);
  assert.equal(harness.hooks.state.activeTabId, null);
  assert.equal(harness.hooks.state.groupId, null);
  assert.equal(harness.hooks.state.controlling, false);
  assert.equal(harness.calls.detach, 1);
  assert.deepEqual(
    harness.calls.messages.map(call => call.tabId).sort((a, b) => a - b),
    [10, 11],
  );
});

test("attach failure never detaches a debugger the extension does not own", async () => {
  const harness = createHarness({ attachError: "Another debugger is already attached" });
  await harness.hooks.connectTransport();
  harness.hooks.state.controlledTabs.add(9);
  harness.hooks.state.controlling = true;

  await assert.rejects(harness.hooks.attachTab(9), /already controlled by another debugger/);
  assert.equal(harness.calls.detach, 0);
  assert.equal(harness.hooks.state.attachedTabs.has(9), false);
});
