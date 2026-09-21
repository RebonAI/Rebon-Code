(() => {
  const Shared = globalThis.RebonBrowserShared;
  if (!Shared) {
    throw new Error("Rebon Browser shared helpers were not injected.");
  }
  if (globalThis.__rebonBrowserContentInstalled) {
    globalThis.__rebonBrowserEnsureOverlay?.();
    return;
  }
  globalThis.__rebonBrowserContentInstalled = true;

  const generation = createGeneration();
  const refsByElement = new WeakMap();
  const elementsByRef = new Map();
  let nextRef = 1;
  let overlayHost = null;
  let overlayShadow = null;

  function createGeneration() {
    const bytes = new Uint8Array(8);
    crypto.getRandomValues(bytes);
    return [...bytes].map(byte => byte.toString(16).padStart(2, "0")).join("");
  }

  function isVisible(element) {
    if (!(element instanceof Element)) {
      return false;
    }
    const style = getComputedStyle(element);
    if (style.display === "none" || style.visibility === "hidden" || Number(style.opacity) === 0) {
      return false;
    }
    const rect = element.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0 && rect.bottom >= 0 && rect.right >= 0
      && rect.top <= innerHeight && rect.left <= innerWidth;
  }

  function refFor(element) {
    let ref = refsByElement.get(element);
    if (!ref) {
      ref = `${generation}:${nextRef++}`;
      refsByElement.set(element, ref);
      elementsByRef.set(ref, element);
    }
    return ref;
  }

  function elementFor(ref) {
    if (!String(ref).startsWith(`${generation}:`)) {
      throw new Error("This element ref is stale because the page navigated. Observe the page again.");
    }
    const element = elementsByRef.get(ref);
    if (!element?.isConnected) {
      throw new Error("This element ref is stale because the page changed. Observe the page again.");
    }
    return element;
  }

  function collapseText(value, max = 300) {
    return Shared.collapseText(value, max);
  }

  function associatedLabel(element) {
    if (element.labels?.length) {
      return [...element.labels].map(label => label.innerText).join(" ");
    }
    const id = element.id;
    if (id && globalThis.CSS?.escape) {
      return document.querySelector(`label[for="${CSS.escape(id)}"]`)?.innerText || "";
    }
    return "";
  }

  function accessibleName(element) {
    const type = String(element.getAttribute("type") || "").toLowerCase();
    const safeValue = type === "password" ? "" : element.value;
    return collapseText(
      element.getAttribute("aria-label")
      || associatedLabel(element)
      || element.getAttribute("alt")
      || element.getAttribute("title")
      || element.innerText
      || element.getAttribute("placeholder")
      || safeValue
      || element.getAttribute("name")
      || element.tagName.toLowerCase(),
    );
  }

  function inferredRole(element) {
    const explicit = element.getAttribute("role");
    if (explicit) return explicit;
    const tag = element.tagName.toLowerCase();
    const type = String(element.getAttribute("type") || "").toLowerCase();
    if (tag === "a" && element.hasAttribute("href")) return "link";
    if (tag === "button" || type === "button" || type === "submit") return "button";
    if (tag === "textarea") return "textbox";
    if (tag === "select") return "combobox";
    if (tag === "input") {
      if (type === "checkbox") return "checkbox";
      if (type === "radio") return "radio";
      if (type === "range") return "slider";
      return "textbox";
    }
    if (element.isContentEditable) return "textbox";
    return tag;
  }

  function serializeElement(element) {
    const rect = element.getBoundingClientRect();
    const type = String(element.getAttribute("type") || "").toLowerCase();
    const result = {
      ref: refFor(element),
      role: inferredRole(element),
      name: accessibleName(element),
      tag: element.tagName.toLowerCase(),
      type: type || null,
      bounds: {
        x: Math.round(rect.x),
        y: Math.round(rect.y),
        width: Math.round(rect.width),
        height: Math.round(rect.height),
      },
      disabled: Boolean(element.disabled || element.getAttribute("aria-disabled") === "true"),
      checked: typeof element.checked === "boolean" ? element.checked : undefined,
      selected: typeof element.selected === "boolean" ? element.selected : undefined,
      expanded: element.hasAttribute("aria-expanded")
        ? element.getAttribute("aria-expanded") === "true"
        : undefined,
    };
    if ("value" in element) {
      result.value = Shared.safeInputValue(type, element.value, 200);
    }
    return result;
  }

  function frameOffset() {
    let current = window;
    let x = 0;
    let y = 0;
    try {
      while (current !== current.top) {
        const frame = current.frameElement;
        if (!frame) {
          return { x: 0, y: 0, supported: false };
        }
        const rect = frame.getBoundingClientRect();
        x += rect.left;
        y += rect.top;
        current = current.parent;
      }
      return { x, y, supported: true };
    } catch {
      return { x: 0, y: 0, supported: false };
    }
  }

  function observe(maxElements, maxTextChars) {
    const selector = [
      "a[href]",
      "button",
      "input:not([type='hidden'])",
      "select",
      "textarea",
      "[contenteditable='true']",
      "[role='button']",
      "[role='link']",
      "[role='checkbox']",
      "[role='radio']",
      "[role='tab']",
      "[role='textbox']",
      "[tabindex]:not([tabindex='-1'])",
    ].join(",");
    const elements = [];
    for (const element of document.querySelectorAll(selector)) {
      if (!isVisible(element)) continue;
      elements.push(serializeElement(element));
      if (elements.length >= maxElements) break;
    }
    const text = collapseText(document.body?.innerText || document.documentElement?.innerText || "", maxTextChars);
    const offset = frameOffset();
    return {
      title: document.title,
      url: location.href,
      document_generation: generation,
      viewport: {
        width: innerWidth,
        height: innerHeight,
        device_scale_factor: devicePixelRatio,
        scroll_x: scrollX,
        scroll_y: scrollY,
      },
      text,
      elements,
      frame_offset_x: offset.x,
      frame_offset_y: offset.y,
      coordinate_offset_supported: offset.supported,
    };
  }

  function ensureOverlay() {
    if (overlayHost?.isConnected) {
      return overlayShadow;
    }
    overlayHost = document.createElement("rebon-browser-overlay");
    overlayHost.setAttribute("aria-hidden", "true");
    overlayHost.style.cssText = "position:fixed;inset:0;z-index:2147483647;pointer-events:none;display:block;";
    overlayShadow = overlayHost.attachShadow({ mode: "closed" });
    overlayShadow.innerHTML = `
      <style>
        :host { all: initial; }
        .edge { position: fixed; pointer-events: none; z-index: 2147483646; }
        .top { top: 0; left: 0; right: 0; height: 3px; box-shadow: 0 2px 18px 7px rgba(77, 166, 255, .38); background: rgba(116, 192, 255, .72); }
        .bottom { bottom: 0; left: 0; right: 0; height: 3px; box-shadow: 0 -2px 18px 7px rgba(77, 166, 255, .38); background: rgba(116, 192, 255, .72); }
        .left { top: 0; bottom: 0; left: 0; width: 3px; box-shadow: 2px 0 18px 7px rgba(77, 166, 255, .38); background: rgba(116, 192, 255, .72); }
        .right { top: 0; bottom: 0; right: 0; width: 3px; box-shadow: -2px 0 18px 7px rgba(77, 166, 255, .38); background: rgba(116, 192, 255, .72); }
        .cursor { position: fixed; top: 0; left: 0; width: 22px; height: 30px; opacity: 0; transform: translate3d(20px, 20px, 0); transition: transform 180ms cubic-bezier(.2,.8,.2,1), opacity 120ms ease; filter: drop-shadow(0 2px 4px rgba(0,0,0,.38)); z-index: 2147483647; }
        .cursor::before { content: ""; position: absolute; inset: 0; clip-path: polygon(0 0, 0 25px, 6px 19px, 11px 29px, 16px 26px, 11px 17px, 20px 17px); background: #eef8ff; }
        .cursor::after { content: ""; position: absolute; inset: 2px; clip-path: polygon(0 0, 0 20px, 5px 15px, 10px 25px, 12px 24px, 7px 13px, 16px 13px); background: #2388e8; }
        .pulse { position: fixed; width: 12px; height: 12px; margin: -6px; border: 2px solid rgba(71, 169, 255, .92); border-radius: 50%; opacity: 0; transform: scale(.25); z-index: 2147483646; }
        .pulse.go { animation: pulse 420ms ease-out; }
        @keyframes pulse { 0% { opacity: 1; transform: scale(.25); } 100% { opacity: 0; transform: scale(3.4); } }
      </style>
      <div class="edge top"></div><div class="edge bottom"></div><div class="edge left"></div><div class="edge right"></div>
      <div class="cursor"></div><div class="pulse"></div>
    `;
    (document.documentElement || document.body).appendChild(overlayHost);
    return overlayShadow;
  }

  globalThis.__rebonBrowserEnsureOverlay = ensureOverlay;

  function moveCursor(x, y, click = false) {
    const shadow = ensureOverlay();
    const cursor = shadow.querySelector(".cursor");
    cursor.style.opacity = "1";
    cursor.style.transform = `translate3d(${Math.round(x)}px, ${Math.round(y)}px, 0)`;
    if (click) {
      const pulse = shadow.querySelector(".pulse");
      pulse.style.left = `${Math.round(x)}px`;
      pulse.style.top = `${Math.round(y)}px`;
      pulse.classList.remove("go");
      void pulse.offsetWidth;
      pulse.classList.add("go");
    }
  }

  async function resolveTarget(ref, click) {
    const element = elementFor(ref);
    element.scrollIntoView({ behavior: "auto", block: "center", inline: "center" });
    await new Promise(resolve => setTimeout(resolve, 80));
    if (!isVisible(element)) {
      throw new Error("The referenced element is no longer visible. Observe the page again.");
    }
    const rect = element.getBoundingClientRect();
    const x = rect.left + rect.width / 2;
    const y = rect.top + rect.height / 2;
    moveCursor(x, y, click);
    const offset = frameOffset();
    return {
      x,
      y,
      frame_offset_x: offset.x,
      frame_offset_y: offset.y,
      coordinate_offset_supported: offset.supported,
      role: inferredRole(element),
      name: accessibleName(element),
    };
  }

  async function handleMessage(message) {
    switch (message.type) {
      case "init_control":
        ensureOverlay();
        return { ready: true, document_generation: generation };
      case "observe":
        ensureOverlay();
        return observe(
          Math.max(1, Math.min(Number(message.maxElements || 200), 500)),
          Math.max(100, Math.min(Number(message.maxTextChars || 16_000), 50_000)),
        );
      case "resolve_target":
        return resolveTarget(message.ref, Boolean(message.click));
      case "show_cursor":
        moveCursor(Number(message.x), Number(message.y), Boolean(message.click));
        return { shown: true };
      case "scroll_ref": {
        const element = elementFor(message.ref);
        element.scrollIntoView({ behavior: "smooth", block: "center", inline: "center" });
        await new Promise(resolve => setTimeout(resolve, 180));
        return { visible: isVisible(element) };
      }
      case "has_text":
        return {
          matched: (document.body?.innerText || document.documentElement?.innerText || "")
            .includes(String(message.text || "")),
        };
      case "stop_control":
        overlayHost?.remove();
        overlayHost = null;
        overlayShadow = null;
        return { stopped: true };
      default:
        throw new Error(`Unknown Rebon content message: ${message.type}`);
    }
  }

  chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
    handleMessage(message)
      .then(sendResponse)
      .catch(error => sendResponse({ __rebon_error: error.message || String(error) }));
    return true;
  });

  ensureOverlay();
})();
