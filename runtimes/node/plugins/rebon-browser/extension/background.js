importScripts("shared.js");

const STATE_KEY = "rebonBrowserControlState";
const TOKEN_KEY = "rebonBrowserPairingToken";
const PAGE_ORIGINS = ["http://*/*", "https://*/*"];
const DEBUGGER_VERSION = "1.3";
const BRIDGE_PORT_KEY = "rebonBrowserBridgePort";
const BRIDGE_PORT_DEFAULT = 17373;
const WEBSOCKET_PROTOCOL = "rebon-browser-v1";
const PROTOCOL_VERSION = 1;
const RECONNECT_ALARM = "rebon-browser-reconnect";

const state = {
  groupId: null,
  controlledTabs: new Set(),
  attachedTabs: new Set(),
  activeTabId: null,
  controlling: false,
  transport: {
    connected: false,
    authenticated: false,
    pairingRequired: false,
    error: null,
  },
};

let socket = null;
let connectPromise = null;
let reconnectTimer = null;
let reconnectDelay = 250;
let heartbeatTimer = null;
const attachPromises = new Map();
const expectedDetachTabs = new Set();

const stateReady = restoreState();
stateReady.then(connectTransport).catch(() => {});
chrome.alarms.create(RECONNECT_ALARM, { periodInMinutes: 1 });

function updateTransport(patch) {
  const wasAuthenticated = state.transport.authenticated;
  state.transport = { ...state.transport, ...patch };
  if (wasAuthenticated && !state.transport.authenticated && state.controlling) {
    stopControl("The local Rebon browser connection closed.").catch(() => {});
  }
}

function connectTransport() {
  if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) {
    return Promise.resolve();
  }
  if (connectPromise) {
    return connectPromise;
  }
  connectPromise = establishTransport().finally(() => {
    connectPromise = null;
  });
  return connectPromise;
}

// A host started as `rebon browser-mcp --port <n>` listens off the
// default port; an MV3 worker has no argv, so the override rides in
// storage: `chrome.storage.local.set({rebonBrowserBridgePort: n})`
// from the extension's service-worker console. The manifest CSP
// allows any 127.0.0.1 port — the loopback restriction is what
// matters, not the number.
async function bridgeUrl() {
  try {
    const stored = await chrome.storage.local.get(BRIDGE_PORT_KEY);
    const port = Number(stored[BRIDGE_PORT_KEY]);
    if (Number.isInteger(port) && port > 0 && port < 65536) {
      return `ws://127.0.0.1:${port}`;
    }
  } catch {
    // Fall through to the default.
  }
  return `ws://127.0.0.1:${BRIDGE_PORT_DEFAULT}`;
}

async function establishTransport() {
  clearTimeout(reconnectTimer);
  const stored = await chrome.storage.local.get(TOKEN_KEY);
  const token = stored[TOKEN_KEY] || null;
  if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) {
    return;
  }

  let candidate;
  try {
    candidate = new WebSocket(await bridgeUrl(), WEBSOCKET_PROTOCOL);
    socket = candidate;
  } catch (error) {
    scheduleReconnect(error.message || String(error));
    return;
  }

  candidate.addEventListener("open", () => {
    if (socket !== candidate) {
      candidate.close();
      return;
    }
    reconnectDelay = 250;
    updateTransport({ connected: true, authenticated: false, error: null });
    candidate.send(JSON.stringify({
      type: "hello",
      protocol: PROTOCOL_VERSION,
      version: chrome.runtime.getManifest().version,
      token,
    }));
    clearInterval(heartbeatTimer);
    heartbeatTimer = setInterval(() => {
      if (socket === candidate && candidate.readyState === WebSocket.OPEN) {
        candidate.send(JSON.stringify({ type: "heartbeat" }));
      }
    }, 20_000);
  });

  candidate.addEventListener("message", event => {
    if (socket !== candidate) {
      return;
    }
    handleHostMessage(event.data, candidate).catch(error => {
      if (socket === candidate) {
        updateTransport({ error: error.message || String(error) });
      }
    });
  });

  candidate.addEventListener("error", () => {
    if (socket === candidate) {
      updateTransport({ error: "Cannot reach the local Rebon browser plugin." });
    }
  });

  candidate.addEventListener("close", () => {
    if (socket !== candidate) {
      return;
    }
    clearInterval(heartbeatTimer);
    heartbeatTimer = null;
    socket = null;
    updateTransport({
      connected: false,
      authenticated: false,
      pairingRequired: false,
    });
    scheduleReconnect();
  });
}

function scheduleReconnect(error = null) {
  if (error) {
    updateTransport({ error });
  }
  clearTimeout(reconnectTimer);
  reconnectTimer = setTimeout(() => connectTransport().catch(() => {}), reconnectDelay);
  reconnectDelay = Math.min(reconnectDelay * 2, 5_000);
}

async function handleHostMessage(raw, transportSocket) {
  const message = JSON.parse(raw);
  switch (message.type) {
    case "hello_ok":
      updateTransport({
        connected: true,
        authenticated: Boolean(message.authenticated),
        pairingRequired: Boolean(message.pairing_required),
        error: null,
      });
      break;
    case "pair_ok":
      await chrome.storage.local.set({ [TOKEN_KEY]: message.token });
      updateTransport({
        connected: true,
        authenticated: true,
        pairingRequired: false,
        error: null,
      });
      break;
    case "pair_error":
      updateTransport({ error: message.message || "Invalid browser pairing code." });
      break;
    case "pairing_forgotten":
      await chrome.storage.local.remove(TOKEN_KEY);
      updateTransport({ authenticated: false, pairingRequired: true, error: null });
      break;
    case "busy":
    case "protocol_error":
      updateTransport({ error: message.message || "The Rebon browser bridge rejected this extension." });
      break;
    case "request":
      await handleTransportRequest(message, transportSocket);
      break;
    case "heartbeat_ack":
      break;
    default:
      throw new Error(`Unknown Rebon browser message: ${message.type}`);
  }
}

async function handleTransportRequest(message, transportSocket) {
  let response;
  try {
    const result = await handleHostRequest(message.method, message.params || {});
    response = { type: "response", id: message.id, ok: true, result };
  } catch (error) {
    response = {
      type: "response",
      id: message.id,
      ok: false,
      error: error.message || String(error),
    };
  }
  if (socket === transportSocket && transportSocket.readyState === WebSocket.OPEN) {
    transportSocket.send(JSON.stringify(response));
  }
}

function pairWithCode(code) {
  if (socket?.readyState !== WebSocket.OPEN) {
    throw new Error("The local Rebon browser plugin is not connected.");
  }
  socket.send(JSON.stringify({ type: "pair", code: String(code || "") }));
}

async function forgetPairing() {
  await chrome.storage.local.remove(TOKEN_KEY);
  if (socket?.readyState === WebSocket.OPEN && state.transport.authenticated) {
    socket.send(JSON.stringify({ type: "forget_pairing" }));
  }
  updateTransport({ authenticated: false, pairingRequired: true, error: null });
}

async function restoreState() {
  const stored = await chrome.storage.local.get(STATE_KEY);
  const saved = stored[STATE_KEY];
  if (!saved) {
    return;
  }
  state.groupId = Number.isInteger(saved.groupId) ? saved.groupId : null;
  state.controlledTabs = new Set(Array.isArray(saved.controlledTabs) ? saved.controlledTabs : []);
  state.activeTabId = Number.isInteger(saved.activeTabId) ? saved.activeTabId : null;
  state.controlling = false;
  await refreshControlledTabs();
}

async function saveState() {
  await chrome.storage.local.set({
    [STATE_KEY]: {
      groupId: state.groupId,
      controlledTabs: [...state.controlledTabs],
      activeTabId: state.activeTabId,
      controlling: state.controlling,
    },
  });
}

async function hasPageAccess() {
  return chrome.permissions.contains({ origins: PAGE_ORIGINS });
}

async function requirePageAccess() {
  if (!(await hasPageAccess())) {
    throw new Error("Page access is disabled. Open the Rebon Browser extension and click Enable page access.");
  }
}

async function refreshControlledTabs() {
  const groupId = state.groupId;
  if (groupId === null) {
    state.controlledTabs.clear();
    state.activeTabId = null;
    return [];
  }
  let groupedTabs;
  try {
    groupedTabs = await chrome.tabs.query({ groupId });
  } catch {
    groupedTabs = [];
  }
  if (state.groupId !== groupId) {
    return [];
  }
  const reconciliation = RebonBrowserShared.reconcileControlledTabs(
    [...state.controlledTabs],
    groupedTabs.map(tab => tab.id),
    state.activeTabId,
  );
  for (const tabId of reconciliation.removed) {
    if (state.controlledTabs.has(tabId)) {
      await releaseTab(tabId, "Tab left the Rebon group.");
    }
  }
  if (!state.controlledTabs.has(state.activeTabId)) {
    state.activeTabId = [...state.controlledTabs][0] ?? null;
  }
  if (state.controlledTabs.size === 0) {
    state.groupId = null;
    state.activeTabId = null;
    state.controlling = false;
  }
  return groupedTabs.filter(tab => state.controlledTabs.has(tab.id));
}

function assertAllowedUrl(raw) {
  return RebonBrowserShared.validateBrowserUrl(raw);
}

function isInjectableUrl(url) {
  return typeof url === "string" && (url.startsWith("http://") || url.startsWith("https://"));
}

async function attachTab(tabId) {
  if (state.attachedTabs.has(tabId)) {
    return;
  }
  const existing = attachPromises.get(tabId);
  if (existing) {
    return existing;
  }

  expectedDetachTabs.delete(tabId);
  const operation = (async () => {
    try {
      await chrome.debugger.attach({ tabId }, DEBUGGER_VERSION);
    } catch (error) {
      const message = String(error.message || error);
      if (message.includes("Another debugger is already attached")) {
        throw new Error(`Tab ${tabId} is already controlled by another debugger.`);
      }
      throw new Error(`Could not attach Rebon to tab ${tabId}: ${message}`);
    }
    state.attachedTabs.add(tabId);
    await chrome.debugger.sendCommand({ tabId }, "Page.enable").catch(() => {});
  })();
  attachPromises.set(tabId, operation);
  try {
    await operation;
  } finally {
    if (attachPromises.get(tabId) === operation) {
      attachPromises.delete(tabId);
    }
  }
}

async function detachTab(tabId) {
  const attaching = attachPromises.get(tabId);
  if (attaching) {
    await attaching.catch(() => {});
  }
  if (!state.attachedTabs.delete(tabId)) {
    return;
  }
  expectedDetachTabs.add(tabId);
  try {
    await chrome.debugger.detach({ tabId });
  } catch {
    expectedDetachTabs.delete(tabId);
  }
}

async function injectContent(tabId) {
  const tab = await chrome.tabs.get(tabId);
  if (!isInjectableUrl(tab.url)) {
    return false;
  }
  await chrome.scripting.executeScript({
    target: { tabId, allFrames: true },
    files: ["shared.js", "content.js"],
  });
  const frames = await getFrames(tabId);
  await Promise.all(frames.map(frame => sendContent(tabId, frame.frameId, {
    type: "init_control",
  }).catch(() => null)));
  return true;
}

async function getFrames(tabId) {
  try {
    return await chrome.webNavigation.getAllFrames({ tabId }) || [{ frameId: 0 }];
  } catch {
    return [{ frameId: 0 }];
  }
}

async function sendContent(tabId, frameId, message) {
  const response = await chrome.tabs.sendMessage(tabId, message, { frameId });
  if (response?.__rebon_error) {
    throw new Error(response.__rebon_error);
  }
  return response;
}

async function createControlledTab(url = "about:blank") {
  const safeUrl = assertAllowedUrl(url);
  const tab = await chrome.tabs.create({ url: safeUrl, active: true });
  if (!Number.isInteger(tab.id)) {
    throw new Error("Chrome did not return an id for the new Rebon tab.");
  }
  if (state.groupId === null) {
    state.groupId = await chrome.tabs.group({ tabIds: [tab.id] });
    await chrome.tabGroups.update(state.groupId, {
      title: "Rebon",
      color: "blue",
      collapsed: false,
    });
  } else {
    await chrome.tabs.group({ tabIds: [tab.id], groupId: state.groupId });
  }
  state.controlledTabs.add(tab.id);
  state.activeTabId = tab.id;
  state.controlling = true;
  await attachTab(tab.id);
  if (tab.status === "complete") {
    await injectContent(tab.id).catch(() => false);
  }
  await saveState();
  return chrome.tabs.get(tab.id);
}

async function startControl(params) {
  await requirePageAccess();
  await refreshControlledTabs();
  state.controlling = true;

  let tab;
  if (params.url || state.controlledTabs.size === 0) {
    tab = await createControlledTab(params.url || "about:blank");
  } else {
    for (const tabId of state.controlledTabs) {
      await attachTab(tabId);
      await injectContent(tabId).catch(() => false);
    }
    const tabId = state.activeTabId ?? [...state.controlledTabs][0];
    tab = await chrome.tabs.get(tabId);
    await chrome.tabs.update(tabId, { active: true });
    await chrome.windows.update(tab.windowId, { focused: true });
  }
  await saveState();
  return {
    started: true,
    group_id: state.groupId,
    active_tab: serializeTab(tab),
    notice: "Chrome/Edge owns the debugging notice shown while Rebon is attached.",
  };
}

async function listTabs() {
  const tabs = await refreshControlledTabs();
  return tabs.map(serializeTab);
}

function serializeTab(tab) {
  return {
    id: tab.id,
    title: tab.title || "",
    url: tab.url || "",
    active: Boolean(tab.active),
    status: tab.status || "unknown",
  };
}

async function handleTabs(params) {
  await requirePageAccess();
  switch (params.action) {
    case "list":
      return { tabs: await listTabs(), active_tab_id: state.activeTabId };
    case "new": {
      const tab = await createControlledTab(params.url || "about:blank");
      return { created: serializeTab(tab) };
    }
    case "activate": {
      const tabId = requireControlledTabId(params.tab_id);
      const tab = await chrome.tabs.update(tabId, { active: true });
      await chrome.windows.update(tab.windowId, { focused: true });
      state.activeTabId = tabId;
      await saveState();
      return { activated: serializeTab(tab) };
    }
    case "close": {
      const tabId = requireControlledTabId(params.tab_id);
      await releaseTab(tabId, "The Rebon tab was closed.");
      await chrome.tabs.remove(tabId);
      if (state.activeTabId === tabId) {
        state.activeTabId = [...state.controlledTabs][0] ?? null;
      }
      await saveState();
      return { closed_tab_id: tabId };
    }
    default:
      throw new Error(`Unsupported tabs action: ${params.action}`);
  }
}

function requireControlledTabId(requested) {
  const tabId = Number.isInteger(requested) ? requested : state.activeTabId;
  if (!Number.isInteger(tabId) || !state.controlledTabs.has(tabId)) {
    throw new Error("The requested tab is not controlled by the Rebon tab group.");
  }
  return tabId;
}

async function navigate(params) {
  await requirePageAccess();
  const tabId = requireControlledTabId(params.tab_id);
  const url = assertAllowedUrl(params.url);
  await chrome.tabs.update(tabId, { url, active: true });
  state.activeTabId = tabId;
  await saveState();
  return { tab_id: tabId, url };
}

async function observe(params) {
  await requirePageAccess();
  const tabId = requireControlledTabId(params.tab_id);
  await attachTab(tabId);
  await injectContent(tabId);
  const tab = await chrome.tabs.get(tabId);
  const frames = await getFrames(tabId);
  const frameResults = [];

  for (const frame of frames) {
    try {
      const snapshot = await sendContent(tabId, frame.frameId, {
        type: "observe",
        maxElements: params.max_elements || 200,
        maxTextChars: params.max_text_chars || 16_000,
      });
      if (!snapshot) {
        continue;
      }
      snapshot.frame_id = frame.frameId;
      snapshot.elements = (snapshot.elements || []).map(element => ({
        ...element,
        ref: `${frame.frameId}|${element.ref}`,
        frame_id: frame.frameId,
      }));
      frameResults.push(snapshot);
    } catch {
      // Chrome blocks browser-internal and some fenced frames. Other frames remain observable.
    }
  }
  if (frameResults.length === 0) {
    throw new Error("This page cannot be observed. Browser-internal pages and Web Store pages are not controllable.");
  }

  const top = frameResults.find(frame => frame.frame_id === 0) || frameResults[0];
  const result = {
    tab_id: tabId,
    title: tab.title || top.title || "",
    url: tab.url || top.url || "",
    viewport: top.viewport,
    document_generation: top.document_generation,
    text: frameResults
      .map(frame => frame.text ? `[frame ${frame.frame_id}] ${frame.text}` : "")
      .filter(Boolean)
      .join("\n\n"),
    elements: frameResults.flatMap(frame => frame.elements || []),
    frames: frameResults.map(frame => ({
      frame_id: frame.frame_id,
      url: frame.url,
      document_generation: frame.document_generation,
      coordinate_offset_supported: frame.coordinate_offset_supported,
    })),
  };
  if (params.include_screenshot) {
    result.screenshot = await captureScreenshot(tabId);
  }
  return result;
}

async function captureScreenshot(tabId) {
  await attachTab(tabId);
  const result = await chrome.debugger.sendCommand({ tabId }, "Page.captureScreenshot", {
    format: "png",
    fromSurface: true,
    captureBeyondViewport: false,
  });
  return {
    data: result.data,
    mime_type: "image/png",
    tab_id: tabId,
  };
}

function parsePublicRef(ref) {
  return RebonBrowserShared.parsePublicRef(ref);
}

async function resolveTarget(tabId, params, click = false) {
  if (params.ref) {
    const { frameId, innerRef } = parsePublicRef(params.ref);
    const target = await sendContent(tabId, frameId, {
      type: "resolve_target",
      ref: innerRef,
      click,
    });
    if (!target.coordinate_offset_supported && frameId !== 0) {
      throw new Error("This element is inside a cross-origin frame whose viewport coordinates cannot be mapped safely.");
    }
    return {
      x: target.x + (target.frame_offset_x || 0),
      y: target.y + (target.frame_offset_y || 0),
      frameId,
      ref: params.ref,
    };
  }
  const x = Number(params.x);
  const y = Number(params.y);
  await sendContent(tabId, 0, { type: "show_cursor", x, y, click }).catch(() => {});
  return { x, y, frameId: 0, ref: null };
}

function modifierBits(modifiers = []) {
  return RebonBrowserShared.modifierBits(modifiers);
}

async function click(params) {
  await requirePageAccess();
  const tabId = requireControlledTabId(params.tab_id);
  await attachTab(tabId);
  await injectContent(tabId);
  const target = await resolveTarget(tabId, params, true);
  const button = params.button || "left";
  const clickCount = params.click_count || 1;
  const modifiers = modifierBits(params.modifiers);
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchMouseEvent", {
    type: "mouseMoved",
    x: target.x,
    y: target.y,
    modifiers,
  });
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchMouseEvent", {
    type: "mousePressed",
    x: target.x,
    y: target.y,
    button,
    clickCount,
    modifiers,
  });
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchMouseEvent", {
    type: "mouseReleased",
    x: target.x,
    y: target.y,
    button,
    clickCount,
    modifiers,
  });
  return { clicked: target, button, click_count: clickCount, tab_id: tabId };
}

async function hover(params) {
  await requirePageAccess();
  const tabId = requireControlledTabId(params.tab_id);
  await attachTab(tabId);
  await injectContent(tabId);
  const target = await resolveTarget(tabId, params, false);
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchMouseEvent", {
    type: "mouseMoved",
    x: target.x,
    y: target.y,
    modifiers: modifierBits(params.modifiers),
  });
  return { hovered: target, tab_id: tabId };
}

const KEY_CODES = {
  Enter: { code: "Enter", windowsVirtualKeyCode: 13 },
  Escape: { code: "Escape", windowsVirtualKeyCode: 27 },
  Tab: { code: "Tab", windowsVirtualKeyCode: 9 },
  Backspace: { code: "Backspace", windowsVirtualKeyCode: 8 },
  Delete: { code: "Delete", windowsVirtualKeyCode: 46 },
  ArrowLeft: { code: "ArrowLeft", windowsVirtualKeyCode: 37 },
  ArrowUp: { code: "ArrowUp", windowsVirtualKeyCode: 38 },
  ArrowRight: { code: "ArrowRight", windowsVirtualKeyCode: 39 },
  ArrowDown: { code: "ArrowDown", windowsVirtualKeyCode: 40 },
  Home: { code: "Home", windowsVirtualKeyCode: 36 },
  End: { code: "End", windowsVirtualKeyCode: 35 },
  PageUp: { code: "PageUp", windowsVirtualKeyCode: 33 },
  PageDown: { code: "PageDown", windowsVirtualKeyCode: 34 },
  Space: { code: "Space", windowsVirtualKeyCode: 32, text: " " },
};

async function dispatchKey(tabId, key, modifiers = []) {
  const known = KEY_CODES[key] || {};
  const printable = key.length === 1;
  const params = {
    key: key === "Space" ? " " : key,
    code: known.code || (printable ? `Key${key.toUpperCase()}` : key),
    windowsVirtualKeyCode: known.windowsVirtualKeyCode || (printable ? key.toUpperCase().charCodeAt(0) : 0),
    nativeVirtualKeyCode: known.windowsVirtualKeyCode || 0,
    modifiers: modifierBits(modifiers),
    text: printable && modifiers.length === 0 ? key : known.text || undefined,
    unmodifiedText: printable ? key : known.text || undefined,
  };
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchKeyEvent", {
    ...params,
    type: "keyDown",
  });
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchKeyEvent", {
    ...params,
    type: "keyUp",
    text: undefined,
  });
}

async function typeText(params) {
  await requirePageAccess();
  const tabId = requireControlledTabId(params.tab_id);
  await attachTab(tabId);
  await injectContent(tabId);
  const target = await resolveTarget(tabId, { ref: params.ref }, false);
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchMouseEvent", {
    type: "mousePressed",
    x: target.x,
    y: target.y,
    button: "left",
    clickCount: 1,
  });
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchMouseEvent", {
    type: "mouseReleased",
    x: target.x,
    y: target.y,
    button: "left",
    clickCount: 1,
  });
  if (params.clear) {
    const platform = await chrome.runtime.getPlatformInfo();
    await dispatchKey(tabId, "a", [platform.os === "mac" ? "Meta" : "Control"]);
    await dispatchKey(tabId, "Backspace");
  }
  if (params.text) {
    await chrome.debugger.sendCommand({ tabId }, "Input.insertText", { text: params.text });
  }
  if (params.submit) {
    await dispatchKey(tabId, "Enter");
  }
  return {
    typed: true,
    characters: String(params.text || "").length,
    submitted: Boolean(params.submit),
    tab_id: tabId,
  };
}

async function keyPress(params) {
  await requirePageAccess();
  const tabId = requireControlledTabId(params.tab_id);
  await attachTab(tabId);
  await dispatchKey(tabId, params.key, params.modifiers || []);
  return { key: params.key, tab_id: tabId };
}

async function scroll(params) {
  await requirePageAccess();
  const tabId = requireControlledTabId(params.tab_id);
  await injectContent(tabId);
  if (params.ref) {
    const { frameId, innerRef } = parsePublicRef(params.ref);
    const result = await sendContent(tabId, frameId, {
      type: "scroll_ref",
      ref: innerRef,
    });
    return { scrolled_to: params.ref, tab_id: tabId, result };
  }
  await attachTab(tabId);
  const tab = await chrome.tabs.get(tabId);
  const viewport = await chrome.debugger.sendCommand({ tabId }, "Runtime.evaluate", {
    expression: "({x: innerWidth / 2, y: innerHeight / 2})",
    returnByValue: true,
  }).catch(() => null);
  const center = viewport?.result?.value || { x: 400, y: 300 };
  await chrome.debugger.sendCommand({ tabId }, "Input.dispatchMouseEvent", {
    type: "mouseWheel",
    x: center.x,
    y: center.y,
    deltaX: Number(params.delta_x || 0),
    deltaY: Number(params.delta_y || 0),
  });
  return {
    tab_id: tab.id,
    delta_x: Number(params.delta_x || 0),
    delta_y: Number(params.delta_y || 0),
  };
}

async function waitFor(params) {
  const tabId = requireControlledTabId(params.tab_id);
  const timeoutMs = Math.min(Number(params.timeout_ms || 10_000), 60_000);
  if (params.condition === "delay") {
    const delay = Math.min(Number(params.value || 0), timeoutMs);
    await new Promise(resolve => setTimeout(resolve, delay));
    return { waited_ms: delay, tab_id: tabId };
  }
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    const tab = await chrome.tabs.get(tabId);
    if (params.condition === "load" && tab.status === "complete") {
      return { condition: "load", matched: true, tab_id: tabId };
    }
    if (params.condition === "url" && String(tab.url || "").includes(String(params.value))) {
      return { condition: "url", matched: true, url: tab.url, tab_id: tabId };
    }
    if (params.condition === "text") {
      await injectContent(tabId).catch(() => false);
      const frames = await getFrames(tabId);
      for (const frame of frames) {
        try {
          const found = await sendContent(tabId, frame.frameId, {
            type: "has_text",
            text: String(params.value),
          });
          if (found?.matched) {
            return { condition: "text", matched: true, frame_id: frame.frameId, tab_id: tabId };
          }
        } catch {}
      }
    }
    await new Promise(resolve => setTimeout(resolve, 200));
  }
  throw new Error(`Timed out waiting for ${params.condition}.`);
}

async function releaseTab(tabId, reason) {
  state.controlledTabs.delete(tabId);
  if (state.activeTabId === tabId) {
    state.activeTabId = [...state.controlledTabs][0] ?? null;
  }
  if (state.controlledTabs.size === 0) {
    state.groupId = null;
    state.controlling = false;
  }
  await detachTab(tabId);
  const frames = await getFrames(tabId);
  await Promise.all(frames.map(frame => sendContent(tabId, frame.frameId, {
    type: "stop_control",
    reason,
  }).catch(() => null)));
}

async function stopControl(reason = "Stopped by Rebon.") {
  await stateReady;
  const tabIds = [...state.controlledTabs];
  state.controlling = false;
  for (const tabId of tabIds) {
    await detachTab(tabId);
    const frames = await getFrames(tabId);
    await Promise.all(frames.map(frame => sendContent(tabId, frame.frameId, {
      type: "stop_control",
      reason,
    }).catch(() => null)));
  }
  state.controlling = false;
  await saveState();
  return { stopped: true, preserved_tab_ids: tabIds };
}

async function browserStatus() {
  await refreshControlledTabs();
  return {
    connected: state.transport.connected,
    authenticated: state.transport.authenticated,
    pairing_required: state.transport.pairingRequired,
    transport_error: state.transport.error,
    page_access_granted: await hasPageAccess(),
    controlling: state.controlling,
    group_id: state.groupId,
    active_tab_id: state.activeTabId,
    tabs: await listTabs(),
  };
}

async function handleHostRequest(method, params) {
  await stateReady;
  switch (method) {
    case "status": return browserStatus();
    case "start": return startControl(params);
    case "tabs": return handleTabs(params);
    case "navigate": return navigate(params);
    case "observe": return observe(params);
    case "screenshot": {
      const tabId = requireControlledTabId(params.tab_id);
      return captureScreenshot(tabId);
    }
    case "click": return click(params);
    case "hover": return hover(params);
    case "type": return typeText(params);
    case "key": return keyPress(params);
    case "scroll": return scroll(params);
    case "wait": return waitFor(params);
    case "stop": return stopControl();
    default: throw new Error(`Unknown Rebon browser method: ${method}`);
  }
}

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.target !== "background") {
    return false;
  }
  if (message.type === "popup_status") {
    connectTransport()
      .then(browserStatus)
      .then(result => sendResponse({ ok: true, result }))
      .catch(error => sendResponse({ ok: false, error: error.message || String(error) }));
    return true;
  }
  if (message.type === "popup_pair") {
    try {
      pairWithCode(message.code);
      sendResponse({ ok: true });
    } catch (error) {
      sendResponse({ ok: false, error: error.message || String(error) });
    }
    return false;
  }
  if (message.type === "popup_forget_pairing") {
    forgetPairing()
      .then(() => sendResponse({ ok: true }))
      .catch(error => sendResponse({ ok: false, error: error.message || String(error) }));
    return true;
  }
  if (message.type === "popup_stop") {
    stopControl("Stopped from the Rebon Browser popup.")
      .then(result => sendResponse({ ok: true, result }))
      .catch(error => sendResponse({ ok: false, error: error.message || String(error) }));
    return true;
  }
  return false;
});

chrome.tabs.onRemoved.addListener(tabId => {
  if (!state.controlledTabs.has(tabId)) {
    return;
  }
  state.controlledTabs.delete(tabId);
  state.attachedTabs.delete(tabId);
  attachPromises.delete(tabId);
  expectedDetachTabs.delete(tabId);
  if (state.activeTabId === tabId) {
    state.activeTabId = [...state.controlledTabs][0] ?? null;
  }
  if (state.controlledTabs.size === 0) {
    state.groupId = null;
    state.controlling = false;
  }
  saveState().catch(() => {});
});

chrome.tabs.onActivated.addListener(({ tabId }) => {
  if (state.controlledTabs.has(tabId)) {
    state.activeTabId = tabId;
    saveState().catch(() => {});
  }
});

chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (!state.controlledTabs.has(tabId)) {
    return;
  }
  if (Number.isInteger(tab.groupId) && tab.groupId !== state.groupId) {
    releaseTab(tabId, "Tab left the Rebon group.").then(saveState).catch(() => {});
    return;
  }
  if (changeInfo.status === "complete" && state.controlling) {
    attachTab(tabId)
      .then(() => injectContent(tabId))
      .catch(() => {});
  }
});

chrome.tabGroups.onRemoved.addListener(group => {
  if (group.id !== state.groupId) {
    return;
  }
  stopControl("The Rebon tab group was closed.").finally(() => {
    state.groupId = null;
    state.controlledTabs.clear();
    state.activeTabId = null;
    saveState().catch(() => {});
  });
});

chrome.debugger.onDetach.addListener((source, reason) => {
  const tabId = source.tabId;
  state.attachedTabs.delete(tabId);
  if (expectedDetachTabs.delete(tabId) || reason === "target_closed") {
    return;
  }
  if (!state.controlledTabs.has(tabId) || !state.controlling) {
    return;
  }
  stopControl("Browser debugging permission was revoked.").catch(() => {});
});

chrome.alarms.onAlarm.addListener(alarm => {
  if (alarm.name === RECONNECT_ALARM) {
    connectTransport().catch(() => {});
  }
});
chrome.runtime.onStartup.addListener(() => connectTransport().catch(() => {}));
chrome.runtime.onInstalled.addListener(() => connectTransport().catch(() => {}));

if (globalThis.__REBON_BROWSER_TEST__) {
  Object.assign(globalThis.__REBON_BROWSER_TEST__, {
    attachTab,
    connectTransport,
    detachTab,
    refreshControlledTabs,
    releaseTab,
    state,
    stopControl,
    getSocket: () => socket,
    getAttachPromiseCount: () => attachPromises.size,
    expectedDetachTabs,
  });
}
