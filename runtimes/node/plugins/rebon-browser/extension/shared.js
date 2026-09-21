(function exposeRebonBrowserShared(root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) {
    module.exports = api;
  }
  root.RebonBrowserShared = api;
})(typeof globalThis === "object" ? globalThis : this, function createRebonBrowserShared() {
  function validateBrowserUrl(raw) {
    if (raw === "about:blank") {
      return raw;
    }
    let parsed;
    try {
      parsed = new URL(raw);
    } catch {
      throw new Error(`Invalid browser URL: ${raw}`);
    }
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
      throw new Error(`Only http and https pages can be controlled, not ${parsed.protocol}`);
    }
    if (!parsed.hostname) {
      throw new Error("Browser URLs must include a host.");
    }
    const host = parsed.hostname.toLowerCase().replace(/\.+$/, "");
    const path = parsed.pathname.toLowerCase();
    const isChromeStore = host === "chromewebstore.google.com"
      || (host === "chrome.google.com" && path.startsWith("/webstore"));
    const isEdgeStore = (host === "microsoftedge.microsoft.com" && path.startsWith("/addons"))
      || host === "addons.microsoft.com";
    if (isChromeStore || isEdgeStore) {
      throw new Error("Browser extension store pages cannot be controlled.");
    }
    return parsed.href;
  }

  function parsePublicRef(ref) {
    const value = String(ref);
    const separator = value.indexOf("|");
    if (separator <= 0) {
      throw new Error("Invalid browser element ref. Observe the page again.");
    }
    const frameId = Number(value.slice(0, separator));
    const innerRef = value.slice(separator + 1);
    if (!Number.isInteger(frameId) || frameId < 0 || !innerRef) {
      throw new Error("Invalid browser element ref. Observe the page again.");
    }
    return { frameId, innerRef };
  }

  function modifierBits(modifiers = []) {
    let value = 0;
    if (modifiers.includes("Alt")) value |= 1;
    if (modifiers.includes("Control")) value |= 2;
    if (modifiers.includes("Meta")) value |= 4;
    if (modifiers.includes("Shift")) value |= 8;
    return value;
  }

  function collapseText(value, max = 300) {
    const text = String(value || "").replace(/\s+/g, " ").trim();
    return text.length > max ? `${text.slice(0, max)}…` : text;
  }

  function safeInputValue(type, value, max = 200) {
    return String(type || "").toLowerCase() === "password"
      ? "[redacted]"
      : collapseText(value, max);
  }

  function reconcileControlledTabs(controlledTabs, groupedTabs, activeTabId) {
    const grouped = new Set(groupedTabs);
    const kept = controlledTabs.filter(tabId => grouped.has(tabId));
    const removed = controlledTabs.filter(tabId => !grouped.has(tabId));
    return {
      kept,
      removed,
      activeTabId: kept.includes(activeTabId) ? activeTabId : kept[0] ?? null,
    };
  }

  function normalizePairingCode(value) {
    return String(value || "").replace(/\D/g, "").slice(0, 6);
  }

  return {
    collapseText,
    modifierBits,
    normalizePairingCode,
    parsePublicRef,
    reconcileControlledTabs,
    safeInputValue,
    validateBrowserUrl,
  };
});
